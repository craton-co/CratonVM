// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP1.12 — `java.lang.ProcessBuilder` / `Runtime.exec` real subprocess
//! spawning for real-JDK mode.
//!
//! # Overview
//!
//! HotSpot's `java.lang.ProcessBuilder.start()` and the (deprecated but
//! still widely used) `Runtime.exec(String[])` ultimately delegate to the
//! platform-specific JVM native methods:
//!
//! * Windows: `java.lang.ProcessImpl.create(…)` returns a native handle.
//! * Linux/macOS: `java.lang.ProcessImpl.forkAndExec(…)` (JDK internal name
//!   of the old `UNIXProcess` native) returns a pid.
//!
//! In both cases the returned handle is stored in the `Process` instance
//! and the JDK bytecode wraps the child's three standard file descriptors
//! (stdin, stdout, stderr) in `FileInputStream`/`FileOutputStream`
//! instances whose `fd.fd` int slot holds the OS-level file descriptor.
//!
//! This module bridges to `std::process::Command::spawn()` (which is
//! cross-platform and handles all the low-level `CreateProcessW` /
//! `fork+exec` quirks for us). The child's `ChildStdin`, `ChildStdout`,
//! and `ChildStderr` handles are then registered in the `FdTable` so the
//! existing `FileInputStream`/`FileOutputStream` natives in `lib.rs` can
//! read/write them through the normal `fd_table().read_bytes` / `write_bytes`
//! code paths — no special casing anywhere else in the VM.
//!
//! # Layout
//!
//! The VM uses synthetic fields on the `java.lang.Process` object to
//! carry the live state:
//!
//! | field | meaning                                         |
//! | ----- | ----------------------------------------------- |
//! |   0   | exit code (Int; `i32::MIN` == not exited yet)   |
//! |   1   | stdin fd_id (Int; -1 if inherited/closed)       |
//! |   2   | stdout fd_id (Int; -1 if inherited/closed)      |
//! |   3   | stderr fd_id (Int; -1 if inherited/closed)      |
//! |   4   | pid (Long)                                      |
//! |   5   | process-table handle (Long; key into `PROCESS_TABLE`) |
//!
//! We retain the `Child` object in `PROCESS_TABLE` because
//! `Child::wait()` requires `&mut self` and the fd_table-stored pipes
//! alone can't be used to observe exit status.
//!
//! # Platform notes
//!
//! * Windows: `std::process::Command` on Windows goes through
//!   `CreateProcessW`, honoring `CREATE_SUSPENDED` flag if requested. We
//!   don't currently support any special flags beyond the defaults.
//! * Linux/macOS: `Command::spawn` uses `posix_spawn` where available
//!   (glibc ≥ 2.24) and falls back to `fork+execvp` otherwise. Both
//!   paths honor our working-directory and environment setting.
//!
//! Only the Windows arm is runtime-verified from this dev host; the
//! Linux arm is covered by `#[cfg(target_os = "linux")]` code-only
//! (cargo check) plus the integration test `wp1_12_process.rs`.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use crate::io_flags;
use cratonvm_native_api::fd_table::FdId;
use cratonvm_native_api::{Capability, CapabilityCheck, NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError, VmError};
use cratonvm_types::{ObjectRef, Value};

// ---------------------------------------------------------------------------
// Java-visible spawn policy gate
// ---------------------------------------------------------------------------

/// A pre-spawn policy check, called with `command[0]` before any fork/exec.
///
/// Returning `Err` cancels the spawn and propagates the error verbatim to the
/// Java caller, so a `SecurityException` stays a `SecurityException`.
pub type SpawnPolicyHook = fn(&mut dyn NativeContext, &str) -> Result<(), MethodCallFailed>;

/// Installed once at native-registration time by `native-builtins`, which owns
/// `SecurityManager` and therefore cannot be called from here directly (it
/// depends on this crate, not the reverse).
///
/// A `fn` pointer, not per-VM state: the hook is the same code in every VM in
/// the process, and it resolves the *calling* VM's SecurityManager through the
/// `NativeContext` it is handed. Nothing about a particular VM is latched.
static SPAWN_POLICY_HOOK: OnceLock<SpawnPolicyHook> = OnceLock::new();

/// Install the pre-spawn policy gate. Idempotent: the first hook wins, so
/// calling this from more than one registration path is safe.
pub fn set_spawn_policy_hook(hook: SpawnPolicyHook) {
    let _ = SPAWN_POLICY_HOOK.set(hook);
}

/// Run the installed policy gate, if any. With no hook installed (a
/// native-io-only build, or a unit test) this is a no-op, which matches the
/// JDK: spawning is unrestricted until a SecurityManager is installed.
fn run_spawn_policy(ctx: &mut dyn NativeContext, program: &str) -> Result<(), MethodCallFailed> {
    match SPAWN_POLICY_HOOK.get() {
        Some(hook) => hook(ctx, program),
        None => Ok(()),
    }
}

// ---------------------------------------------------------------------------
// Process-table (holds live std::process::Child handles)
// ---------------------------------------------------------------------------

/// Opaque handle into [`PROCESS_TABLE`].  We return this to Java-land
/// as a 64-bit integer so a single `Process` instance can be looked up
/// later without storing a raw pointer in bytecode land.
///
/// Starts at 1 (0 = "no live process", reserved sentinel).
static NEXT_HANDLE: AtomicI64 = AtomicI64::new(1);

/// The subprocess table: `ProcessHandle` → `std::process::Child`.
///
/// Entries live for the VM's lifetime, and the `Child` is shared rather than
/// owned by whoever looks it up. Both properties are load-bearing, and both
/// replaced an arrangement that was fine until the real JDK's `ProcessImpl`
/// started driving this table.
///
/// `ProcessImpl`'s constructor ends in `ProcessHandleImpl.completion(pid, true)`,
/// which puts a reaper thread into `Child::wait()` for the whole life of every
/// child, starting the instant it is spawned. The previous design took the
/// `Child` *out* of the table to wait on it, so from a subsequent
/// `Process.destroy()`'s point of view every child was permanently missing:
/// `destroy` became a no-op, and `new ProcessBuilder("sleep","30").start()`
/// followed by `destroy()` then `waitFor()` blocked for the full thirty
/// seconds and reported 0 instead of 143. Measured, on the first build where
/// the real `ProcessImpl` ran.
///
/// So the `Child` stays, behind its own mutex:
///
/// * a waiter clones the `Arc`, releases the table lock, and blocks on the
///   child's own mutex — the table stays usable while a child runs;
/// * `Child::wait` caches its status internally, so two waiters on one child
///   both get the real code rather than the second seeing "already reaped";
/// * a killer takes the child's mutex with `try_lock`. Failing that lock is
///   not a problem to route around — it is *information*: someone is inside
///   `Child::wait()`, therefore the child has not been reaped, therefore its
///   pid is still its own and signalling it directly is safe. See
///   [`destroy_handle`].
fn process_table() -> &'static Mutex<HashMap<i64, Arc<Mutex<Child>>>> {
    static T: OnceLock<Mutex<HashMap<i64, Arc<Mutex<Child>>>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Look up the shared child for `handle` without holding the table lock.
fn child_for(handle: i64) -> Option<Arc<Mutex<Child>>> {
    process_table().lock().get(&handle).cloned()
}

/// Extra per-process state that can't live inside `std::process::Child`:
/// the terminal exit code (so `waitFor` is idempotent) and the
/// pid captured at spawn time (so `pid()` works after the `Child` has
/// been reaped).
fn exit_cache() -> &'static Mutex<HashMap<i64, ExitCache>> {
    static T: OnceLock<Mutex<HashMap<i64, ExitCache>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

fn pipe_cache() -> &'static Mutex<HashMap<i64, PipeFds>> {
    static T: OnceLock<Mutex<HashMap<i64, PipeFds>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}


#[derive(Clone, Copy, Debug)]
struct ExitCache {
    pid: i64,
    exit_code: Option<i32>,
}

#[derive(Clone, Copy, Debug)]
struct PipeFds {
    stdin_fd: i32,
    stdout_fd: i32,
    stderr_fd: i32,
}

/// Number of instance fields `java.lang.Process` itself declares, reserved as
/// the LEADING slots of the synthetic Process so that real `java.lang.Process`
/// bytecode reaching one of these receivers reads the field it means to.
///
/// `java.lang.Process` is not field-less. Since JDK 17 it declares, in this
/// order, `outputWriter`, `outputCharset`, `inputReader`, `inputCharset`,
/// `errorReader`, `errorCharset` — the caches behind the final concrete
/// `inputReader()` / `errorReader()` / `outputWriter()` methods. An instance
/// field resolves to an ABSOLUTE slot (superclass field count + declaration
/// index) and `java.lang.Process` extends `Object`, so those six are slots
/// 0..=5 of whatever receiver that bytecode runs against.
///
/// The synthetic Process used to keep its own state at 0..=5, so
/// `p.inputReader()` read the stdout pipe fd as `inputReader` — a non-null
/// int — took the "reader already created" branch, and then NPE'd on the
/// still-null `inputCharset`:
///
/// ```text
/// java.lang.NullPointerException: Cannot invoke "java.nio.charset.Charset.equals(Object)"
///         because "this.inputCharset" is null
///         at java.lang.Process.inputReader(Process.java:338)
/// ```
///
/// That aliasing is why `java/lang/Process` is recorded as a *supertype* of
/// `cratonvm/synthetic/Process` in `class_manager::jdk_interfaces` rather than
/// as its superclass. Reserving the slots here removes the aliasing instead,
/// so the three final reader/writer methods run their real bytecode (which
/// then calls the native `getInputStream`/`getErrorStream`/`getOutputStream`)
/// rather than corrupting it. Cost: six reference slots per Process object.
///
/// Reserved slots stay null — only the JDK's own bytecode writes them.
const JAVA_PROCESS_FIELD_COUNT: usize = 6;

/// Field layout on the synthetic `java/lang/Process` object, offset past the
/// reserved slots above.
/// Must match the initialization done by the bytecode / native below.
const PROC_FIELD_EXIT: usize = JAVA_PROCESS_FIELD_COUNT;
const PROC_FIELD_STDIN_FD: usize = JAVA_PROCESS_FIELD_COUNT + 1;
const PROC_FIELD_STDOUT_FD: usize = JAVA_PROCESS_FIELD_COUNT + 2;
const PROC_FIELD_STDERR_FD: usize = JAVA_PROCESS_FIELD_COUNT + 3;
const PROC_FIELD_PID: usize = JAVA_PROCESS_FIELD_COUNT + 4;
const PROC_FIELD_HANDLE: usize = JAVA_PROCESS_FIELD_COUNT + 5;

/// Sentinel "not yet exited" value stored in the exit-code field.
const EXIT_NOT_YET: i32 = i32::MIN;

/// Total number of fields on the synthetic Process.
const PROC_FIELD_COUNT: usize = JAVA_PROCESS_FIELD_COUNT + 6;

/// Class name the synthetic Process is allocated under.
///
/// Virtual dispatch on these objects resolves by the RECEIVER's class-chain
/// names (the call-site `java/lang/Process` entry is only reached when the
/// resolved method's declaring class matches), so the object must carry a
/// name the registrations below are keyed on. Allocating with
/// `ClassId::new(0)` decayed the receiver to the anonymous fallback class
/// `cratonvm/synthetic/AnonymousObject$6`, on which EVERY `Process` virtual
/// (`waitFor`, `isAlive`, `getInputStream`, ...) raised NoSuchMethodError —
/// first seen as picocli's terminal-width probe failing during
/// `junit-platform-console --help` (gaps/gap-anonymous-object-getinputstream.md).
const SYNTHETIC_PROCESS_CLASS: &str = "cratonvm/synthetic/Process";
const SYNTHETIC_PROCESS_INPUT_STREAM: &str = "cratonvm/synthetic/ProcessPipeInputStream";
const SYNTHETIC_PROCESS_OUTPUT_STREAM: &str = "cratonvm/synthetic/ProcessPipeOutputStream";

/// Class the `Process.onExit()` reaper Runnable is allocated under.
///
/// `onExit()` on a still-running child must hand back an *incomplete* future
/// and complete it later, so somebody has to block on the child off the
/// calling thread. Rather than attach a foreign thread (the AIO dispatcher
/// route in `async_socket.rs`), this is a plain daemon `java.lang.Thread`
/// whose Runnable is a synthetic object carrying a native `run()` — the same
/// shape as `cratonvm/xnio/AcceptPump`. That keeps the waiter a genuine VM
/// thread with a live `NativeContext`, so it can call `CompletableFuture
/// .complete` directly, and it mirrors real JDK's own per-process reaper
/// thread (`ProcessHandleImpl.processReaperExecutor`, also daemon).
const SYNTHETIC_PROCESS_EXIT_WAITER: &str = "cratonvm/synthetic/ProcessExitWaiter";
/// The `Process` the future completes with.
const EXIT_WAITER_FIELD_PROCESS: usize = 0;
/// The `CompletableFuture` to complete once the child exits.
const EXIT_WAITER_FIELD_FUTURE: usize = 1;
/// `process_table` handle to block on (see `wait_for_handle`).
const EXIT_WAITER_FIELD_HANDLE: usize = 2;
const EXIT_WAITER_FIELD_COUNT: usize = 3;

fn pb_debug_enabled() -> bool {
    io_flags().dbg_pb
}

#[derive(Clone, Debug)]
enum StdioRedirect {
    Pipe,
    Inherit,
    Null,
    ReadFile(String),
    WriteFile { path: String, append: bool },
    /// A descriptor the caller has already opened, named by its `FdTable` id.
    ///
    /// The real JDK's `ProcessImpl` opens file redirects itself and passes the
    /// resulting descriptor down to `forkAndExec` — it never tells the native
    /// the path. `Inherit` is deliberately NOT folded in here even though ids
    /// 0/1/2 are the VM's own standard streams: inheriting is `Stdio::inherit`,
    /// which hands the child this process's real OS descriptors, whereas this
    /// variant duplicates a table entry.
    ExistingFd(i32),
}

#[derive(Clone, Debug)]
struct ProcessRedirects {
    stdin: StdioRedirect,
    stdout: StdioRedirect,
    stderr: StdioRedirect,
}

impl Default for ProcessRedirects {
    fn default() -> Self {
        Self {
            stdin: StdioRedirect::Pipe,
            stdout: StdioRedirect::Pipe,
            stderr: StdioRedirect::Pipe,
        }
    }
}

fn redirect_io_error(op: &str, path: &str, err: impl std::fmt::Display) -> RuntimeError {
    RuntimeError::IOException {
        message: format!("ProcessBuilder.{op} failed for {path:?}: {err}"),
    }
}

fn validate_redirect_path(path: &str, op: &str) -> Result<String, RuntimeError> {
    crate::validate_path(path).map_err(|_| RuntimeError::IOException {
        message: format!("ProcessBuilder.{op}: redirect path rejected by sandbox: {path}"),
    })
}

fn open_redirect_input(path: &str) -> Result<File, RuntimeError> {
    let validated = validate_redirect_path(path, "redirectInput")?;
    File::open(&validated).map_err(|e| redirect_io_error("redirectInput", path, e))
}

fn open_redirect_output(path: &str, append: bool) -> Result<File, RuntimeError> {
    let validated = validate_redirect_path(path, "redirectOutput")?;
    OpenOptions::new()
        .create(true)
        .write(true)
        .append(append)
        .truncate(!append)
        .open(&validated)
        .map_err(|e| redirect_io_error("redirectOutput", path, e))
}

/// Duplicate a file-backed `FdTable` entry for handing to a child.
///
/// `try_clone` is a `dup`, so the child gets an independent descriptor on the
/// same open file: closing the JDK's own `FileInputStream`/`FileOutputStream`
/// afterwards — which `ProcessImpl.start` does in a `finally` — cannot pull the
/// file out from under the child.
fn stdio_from_existing_fd(
    fd_table: &cratonvm_native_api::fd_table::FileDescriptorTable,
    id: i32,
    op: &str,
) -> Result<Stdio, RuntimeError> {
    if id < 0 {
        return Err(RuntimeError::IOException {
            message: format!("ProcessBuilder.{op}: negative descriptor {id}"),
        });
    }
    let file = fd_table
        .clone_file(id as FdId)
        .map_err(|e| RuntimeError::IOException {
            message: format!(
                "ProcessBuilder.{op}: descriptor {id} is not backed by a file: {e}"
            ),
        })?;
    Ok(Stdio::from(file))
}

fn stdin_stdio(
    fd_table: &cratonvm_native_api::fd_table::FileDescriptorTable,
    spec: &StdioRedirect,
) -> Result<(Stdio, bool), RuntimeError> {
    match spec {
        StdioRedirect::Pipe => Ok((Stdio::piped(), true)),
        StdioRedirect::Inherit => Ok((Stdio::inherit(), false)),
        StdioRedirect::Null => Ok((Stdio::null(), false)),
        StdioRedirect::ReadFile(path) => Ok((Stdio::from(open_redirect_input(path)?), false)),
        StdioRedirect::ExistingFd(id) => Ok((
            stdio_from_existing_fd(fd_table, *id, "redirectInput")?,
            false,
        )),
        StdioRedirect::WriteFile { path, .. } => Err(RuntimeError::IOException {
            message: format!(
                "ProcessBuilder.redirectInput cannot read from output redirect: {path}"
            ),
        }),
    }
}

fn output_stdio(
    fd_table: &cratonvm_native_api::fd_table::FileDescriptorTable,
    spec: &StdioRedirect,
    op: &str,
) -> Result<(Stdio, bool), RuntimeError> {
    match spec {
        StdioRedirect::Pipe => Ok((Stdio::piped(), true)),
        StdioRedirect::Inherit => Ok((Stdio::inherit(), false)),
        StdioRedirect::Null => Ok((Stdio::null(), false)),
        StdioRedirect::WriteFile { path, append } => {
            Ok((Stdio::from(open_redirect_output(path, *append)?), false))
        }
        StdioRedirect::ExistingFd(id) => Ok((stdio_from_existing_fd(fd_table, *id, op)?, false)),
        StdioRedirect::ReadFile(path) => Err(RuntimeError::IOException {
            message: format!("ProcessBuilder.{op} cannot write to input redirect: {path}"),
        }),
    }
}

fn configure_stdio(
    fd_table: &cratonvm_native_api::fd_table::FileDescriptorTable,
    command: &mut Command,
    redirects: &ProcessRedirects,
    redirect_error_stream: bool,
) -> Result<(bool, bool, bool, Option<std::io::PipeReader>), RuntimeError> {
    let (stdin, stdin_piped) = stdin_stdio(fd_table, &redirects.stdin)?;
    command.stdin(stdin);

    if redirect_error_stream {
        match &redirects.stdout {
            StdioRedirect::Pipe => {
                let (reader, writer) = std::io::pipe().map_err(|e| RuntimeError::IOException {
                    message: format!("ProcessBuilder.redirectErrorStream pipe failed: {e}"),
                })?;
                let writer2 = writer.try_clone().map_err(|e| RuntimeError::IOException {
                    message: format!("ProcessBuilder.redirectErrorStream pipe clone failed: {e}"),
                })?;
                command.stdout(Stdio::from(writer));
                command.stderr(Stdio::from(writer2));
                Ok((stdin_piped, false, false, Some(reader)))
            }
            StdioRedirect::Inherit => {
                command.stdout(Stdio::inherit());
                command.stderr(Stdio::inherit());
                Ok((stdin_piped, false, false, None))
            }
            StdioRedirect::Null => {
                command.stdout(Stdio::null());
                command.stderr(Stdio::null());
                Ok((stdin_piped, false, false, None))
            }
            StdioRedirect::WriteFile { path, append } => {
                let file = open_redirect_output(path, *append)?;
                let file2 = file
                    .try_clone()
                    .map_err(|e| redirect_io_error("redirectError", path, e))?;
                command.stdout(Stdio::from(file));
                command.stderr(Stdio::from(file2));
                Ok((stdin_piped, false, false, None))
            }
            StdioRedirect::ExistingFd(id) => {
                let out = stdio_from_existing_fd(fd_table, *id, "redirectOutput")?;
                let err = stdio_from_existing_fd(fd_table, *id, "redirectError")?;
                command.stdout(out);
                command.stderr(err);
                Ok((stdin_piped, false, false, None))
            }
            StdioRedirect::ReadFile(path) => Err(RuntimeError::IOException {
                message: format!(
                    "ProcessBuilder.redirectOutput cannot write to input redirect: {path}"
                ),
            }),
        }
    } else {
        let (stdout, stdout_piped) = output_stdio(fd_table, &redirects.stdout, "redirectOutput")?;
        let (stderr, stderr_piped) = output_stdio(fd_table, &redirects.stderr, "redirectError")?;
        command.stdout(stdout);
        command.stderr(stderr);
        Ok((stdin_piped, stdout_piped, stderr_piped, None))
    }
}

// ---------------------------------------------------------------------------
// Spawn + teardown primitives
// ---------------------------------------------------------------------------

/// SECURITY (V1, HIGH): vet the *executable* of a subprocess spawn against
/// the CWD-confinement policy before launching it.
///
/// `validate_path` confines guest file *reads/writes* to the sandbox root,
/// but a spawned child inherits the JVM's full ambient authority — once it
/// runs it can open files anywhere the host process can, completely bypassing
/// every per-syscall path check the guest is subject to. Spawning an arbitrary
/// host program (`new ProcessBuilder("/bin/sh").start()`,
/// `Runtime.exec("C:\\Windows\\System32\\cmd.exe ...")`) is therefore a far
/// larger escape than the file reads the confinement profile is designed to
/// block, and the work-dir validation alone left it wide open.
///
/// Policy:
///   * Confinement OFF (the JDK-faithful single-tenant default): unchanged —
///     `Ok` for any program, matching `validate_path`'s default of letting a
///     `java -jar app.jar` launch reach the host freely.
///   * Confinement ON (`CRATONVM_CONFINE_IO` / `set_path_confine_to_cwd(true)`):
///     fail closed. The program must be an *explicit* path (contain a path
///     separator) — a bare command name such as `sh` or `cmd` would
///     `PATH`-resolve to an arbitrary host binary and is rejected — AND that
///     path must resolve, via `validate_path`, to a location inside the
///     sandbox root. Anything else is rejected with a `SecurityException`,
///     which the caller maps to the `IOException` that `ProcessBuilder.start`
///     / `UNIXProcess.forkAndExec` raise for an unusable command.
fn validate_spawn_program(program: &str) -> Result<(), RuntimeError> {
    // Null bytes are rejected unconditionally (truncate the host C-string at
    // the boundary); `validate_path` already does this, mirror it here so a
    // bare-name reject path can't slip a NUL through under confinement-off.
    if program.contains('\0') {
        return Err(RuntimeError::SecurityException {
            message: format!(
                "ProcessBuilder.start: program contains null byte: {}",
                program.replace('\0', "\\0")
            ),
        });
    }

    // Default (unconfined) profile: JDK-faithful, spawn freely.
    if !crate::is_path_confine_to_cwd() {
        return Ok(());
    }

    // --- Confined profile: fail closed --------------------------------------
    //
    // Reject bare command names. `Command::new("sh")` resolves `sh` through
    // the host `PATH` to an arbitrary system binary that has nothing to do
    // with the sandbox, so under confinement only an explicit path that we
    // can range-check is allowed.
    let has_separator = program.contains('/') || program.contains('\\');
    if !has_separator {
        return Err(RuntimeError::SecurityException {
            message: format!(
                "ProcessBuilder.start: bare program name rejected under CWD confinement \
                 (PATH-resolved host binary escapes sandbox): {program}"
            ),
        });
    }

    // Explicit path: it must resolve inside the sandbox root, exactly like
    // any file the confined guest is allowed to touch.
    match crate::validate_path(program) {
        Ok(_) => Ok(()),
        Err(_) => Err(RuntimeError::SecurityException {
            message: format!(
                "ProcessBuilder.start: program rejected by sandbox (escapes confinement root): {program}"
            ),
        }),
    }
}

/// Spawn a command and populate a synthetic `java/lang/Process` object.
///
/// This is the workhorse called by every `Runtime.exec` overload and
/// by `ProcessBuilder.start()`.  It:
///
/// 1. Builds a [`Command`] with the given program + args.
/// 2. Applies `work_dir` and `env_vars` when present.
/// 3. Pipes the child's three standard fds so the JDK wrappers can read
///    them through the `fd_table()` API.
/// 4. Registers the live `Child` in [`PROCESS_TABLE`] keyed by a fresh
///    handle.
/// 5. Allocates a `java/lang/Process` synthetic and stores the fds/pid
///    in its fields.
pub fn spawn_and_wrap(
    ctx: &mut dyn NativeContext,
    program: &str,
    args: &[String],
    work_dir: Option<&str>,
    env_vars: Option<&[(String, String)]>,
    clear_env: bool,
    redirect_error_stream: bool,
) -> MethodCallResult {
    let redirects = ProcessRedirects::default();
    spawn_and_wrap_with_redirects(
        ctx,
        program,
        args,
        work_dir,
        env_vars,
        clear_env,
        redirect_error_stream,
        &redirects,
    )
}

/// One spawned child, as the process tables now know it.
///
/// Returned by [`spawn_child`] so that the two callers can diverge on what they
/// build from it: the VM's own `ProcessBuilder.start` shadow fabricates a
/// synthetic `Process` around it, while the real JDK's `forkAndExec` writes the
/// pipe ids back into the caller's `int[]` and lets `ProcessImpl` build its own
/// streams from them.
struct SpawnedChild {
    handle: i64,
    pid: i64,
    fds: PipeFds,
}

fn spawn_and_wrap_with_redirects(
    ctx: &mut dyn NativeContext,
    program: &str,
    args: &[String],
    work_dir: Option<&str>,
    env_vars: Option<&[(String, String)]>,
    clear_env: bool,
    redirect_error_stream: bool,
    redirects: &ProcessRedirects,
) -> MethodCallResult {
    let spawned = spawn_child(
        ctx,
        program,
        args,
        work_dir,
        env_vars,
        clear_env,
        redirect_error_stream,
        redirects,
    )?;

    // `ensure_synthetic_class` gives `cratonvm/synthetic/Process` a real
    // `java.lang.Process` superclass, but it resolves it with
    // `get_loaded_class_id` — which answers only for a class that is ALREADY
    // loaded. Load it here so the fabrication cannot silently fall back to
    // `java/lang/Object` and reintroduce the supertype inconsistency
    // (`isAssignableFrom` true while the `getSuperclass()` chain omits it).
    //
    // In practice the caller's own bytecode has already resolved
    // `java.lang.Process` — it is `start()`'s return type — so this is
    // ordinarily a no-op lookup. It is not free to rely on that: this native is
    // also reached from paths that never named the type, and a mode without a
    // real `java.lang.Process` at all must still get the old behaviour rather
    // than an error, which is why the result is deliberately discarded.
    let _ = ctx.load_class("java/lang/Process");
    // Allocate the synthetic Process under its own named class (see
    // SYNTHETIC_PROCESS_CLASS) and populate its 6 own fields. The 6 slots
    // ahead of them belong to java.lang.Process's own reader/writer caches and
    // are deliberately left null — see JAVA_PROCESS_FIELD_COUNT.
    let proc_class = ctx.ensure_synthetic_class(SYNTHETIC_PROCESS_CLASS, PROC_FIELD_COUNT);
    let proc_ref = ctx.alloc_object(proc_class, PROC_FIELD_COUNT);
    ctx.set_field(proc_ref, PROC_FIELD_EXIT, Value::Int(EXIT_NOT_YET));
    ctx.set_field(proc_ref, PROC_FIELD_STDIN_FD, Value::Int(spawned.fds.stdin_fd));
    ctx.set_field(
        proc_ref,
        PROC_FIELD_STDOUT_FD,
        Value::Int(spawned.fds.stdout_fd),
    );
    ctx.set_field(
        proc_ref,
        PROC_FIELD_STDERR_FD,
        Value::Int(spawned.fds.stderr_fd),
    );
    ctx.set_field(proc_ref, PROC_FIELD_PID, Value::Long(spawned.pid));
    ctx.set_field(proc_ref, PROC_FIELD_HANDLE, Value::Long(spawned.handle));

    Ok(Some(Value::Object(Some(proc_ref))))
}

/// Spawn a child and register it in the process tables, without building any
/// Java-visible object around it.
#[allow(clippy::too_many_arguments)]
fn spawn_child(
    ctx: &mut dyn NativeContext,
    program: &str,
    args: &[String],
    work_dir: Option<&str>,
    env_vars: Option<&[(String, String)]>,
    clear_env: bool,
    redirect_error_stream: bool,
    redirects: &ProcessRedirects,
) -> Result<SpawnedChild, MethodCallFailed> {
    if program.is_empty() {
        return Err(RuntimeError::IllegalArgumentException {
            message: "ProcessBuilder: empty program".to_string(),
        }
        .into());
    }

    // SECURITY: the Java-visible spawn gate (`SecurityManager.checkExec`),
    // ahead of everything else so a denial cannot be observed as an
    // `IOException` and so no fork/exec syscall is issued.
    //
    // It lives HERE, in the one function every spawn route funnels through
    // (`ProcessBuilder.start`, `Runtime.exec`, `ProcessImpl.create`,
    // `forkAndExec`), because it previously lived in only some of them:
    // `native-builtins` gated `Runtime.exec` and its own now-shadowed
    // `ProcessBuilder.start`, while THIS crate's `ProcessBuilder.start` -- the
    // registration that actually wins at runtime -- had no gate at all. A
    // deny-all `checkExec` policy therefore refused `Runtime.exec` and let
    // `new ProcessBuilder(...).start()` fork the same child unchallenged
    // (probes/ExecPolicyProbe.java, PB_CHILD_RAN=true).
    //
    // Called with `program` verbatim, before the Windows quote-strip below, so
    // the SecurityManager sees exactly the `command[0]` the caller wrote --
    // which is what HotSpot's `ProcessBuilder.start` passes to `checkExec`.
    run_spawn_policy(ctx, program)?;

    // Windows launchers wrap a space-containing program path in double quotes
    // (e.g. WildFly's `StandaloneCommandBuilder` →
    // `"C:\Program Files\…\bin\java"`). The OS `CreateProcess` takes the program
    // and arguments separately, so a surrounding-quoted program is treated as a
    // literal filename → `ERROR_INVALID_NAME` (os error 123). A `"` is illegal in
    // a Windows filename, so a matched surrounding pair is unambiguously quoting,
    // not part of the name — strip it, mirroring the JDK's `ProcessImpl`.
    let program = program
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .filter(|s| !s.is_empty())
        .unwrap_or(program);

    // AUDIT ROW P3 / work-list item 10. `validate_spawn_program` returns `Ok`
    // for **any** program when CWD confinement is off, which is the default —
    // so without this line the capability layer never learns a spawn was
    // attempted at all, and `capability_audit(vm)` would silently omit
    // `process-spawn` from a run that spawned freely.
    //
    // It sits *before* the confinement validator deliberately: the capability
    // decision is about authority, the validator is about sandbox geometry, and
    // a refusal here must not be reported as the `IOException` the validator's
    // failure is translated into below. Permissive by default, so with no
    // policy installed this is a lookup and `Ok(())`.
    ctx.check_capability_or_throw(Capability::process_spawn(program))?;

    // SECURITY (V1, HIGH): vet the executable against the CWD-confinement
    // policy before spawning. Under confinement this rejects bare PATH-resolved
    // host binaries and any explicit path that escapes the sandbox root; with
    // confinement off (the default) it is a no-op. The validator's
    // `SecurityException` is mapped to `IOException` so the failure looks like
    // any other unusable-command spawn error to `ProcessBuilder.start` /
    // `UNIXProcess.forkAndExec`.
    if let Err(security_err) = validate_spawn_program(program) {
        let detail = match security_err {
            RuntimeError::SecurityException { message } => message,
            other => format!("{other:?}"),
        };
        return Err(RuntimeError::IOException { message: detail }.into());
    }

    if pb_debug_enabled() {
        eprintln!(
            "[PB-SPAWN] program={:?} args={:?} work_dir={:?} clear_env={} envc={} redirect_error_stream={}",
            program,
            args,
            work_dir,
            clear_env,
            env_vars.map(|v| v.len()).unwrap_or(0),
            redirect_error_stream
        );
    }

    let mut command = Command::new(program);
    command.args(args);
    let (stdin_piped, stdout_piped, stderr_piped, merged_reader) =
        configure_stdio(ctx.fd_table(), &mut command, redirects, redirect_error_stream)
            .map_err(cratonvm_types::error::MethodCallFailed::from)?;

    if clear_env {
        command.env_clear();
    }
    if let Some(vars) = env_vars {
        for (k, v) in vars {
            command.env(k, v);
        }
    }
    if let Some(dir) = work_dir {
        if !dir.is_empty() {
            // SECURITY (HIGH): the child process inherits ambient
            // authority from us, so once it `chdir`s into `dir` it can
            // open files relative to that location — completely
            // bypassing every per-syscall path check we do for the
            // guest JVM itself. Under `set_path_confine_to_cwd(true)`
            // we must therefore reject a `work_dir` that escapes the
            // sandbox before spawning. The validator's
            // `SecurityException` is translated to `IOException` to
            // match what `ProcessBuilder.start` / `UNIXProcess.forkAndExec`
            // would throw for any other unusable working directory.
            match crate::validate_path(dir) {
                Ok(validated) => {
                    command.current_dir(validated);
                }
                Err(_) => {
                    return Err(RuntimeError::IOException {
                        message: format!(
                            "ProcessBuilder.start: working directory rejected by sandbox: {dir}"
                        ),
                    }
                    .into());
                }
            }
        }
    }

    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => {
            return Err(RuntimeError::IOException {
                message: format!(
                    "ProcessBuilder.start failed: program={:?}, args={:?}: {}",
                    program, args, e
                ),
            }
            .into());
        }
    };

    // Pull only the pipe handles Java requested. Inherited, file, and discard
    // redirects do not have guest-visible process streams, matching HotSpot's
    // ProcessPipeInputStream/NullInputStream behavior closely enough for the
    // WildFly launchers.
    let stdin_fd = if stdin_piped {
        child
            .stdin
            .take()
            .map(|s| ctx.fd_table().insert_child_stdin(s))
            .map(|fd| fd as i32)
            .unwrap_or(-1)
    } else {
        -1
    };
    // When redirectErrorStream merged the pipes, child.stdout/stderr are None
    // (we gave the child a custom pipe); expose the merged reader as the stdout
    // fd and leave stderr empty (-1).
    let (stdout_fd, stderr_fd) = if let Some(reader) = merged_reader {
        (ctx.fd_table().insert_child_merged(reader) as i32, -1)
    } else {
        let so = if stdout_piped {
            child
                .stdout
                .take()
                .map(|s| ctx.fd_table().insert_child_stdout(s))
                .map(|fd| fd as i32)
                .unwrap_or(-1)
        } else {
            -1
        };
        let se = if stderr_piped {
            child
                .stderr
                .take()
                .map(|s| ctx.fd_table().insert_child_stderr(s))
                .map(|fd| fd as i32)
                .unwrap_or(-1)
        } else {
            -1
        };
        (so, se)
    };

    let pid = child.id() as i64;
    let handle = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
    if pb_debug_enabled() {
        eprintln!(
            "[PB-SPAWNED] handle={} pid={} stdin_fd={} stdout_fd={} stderr_fd={}",
            handle, pid, stdin_fd, stdout_fd, stderr_fd
        );
    }
    process_table()
            .lock()
            .insert(handle, Arc::new(Mutex::new(child)));
    exit_cache().lock().insert(
        handle,
        ExitCache {
            pid,
            exit_code: None,
        },
    );
    pipe_cache().lock().insert(
        handle,
        PipeFds {
            stdin_fd,
            stdout_fd,
            stderr_fd,
        },
    );
    Ok(SpawnedChild {
        handle,
        pid,
        fds: PipeFds {
            stdin_fd,
            stdout_fd,
            stderr_fd,
        },
    })
}

/// Wait for the child identified by `handle` to exit.  Blocks the
/// calling thread.  Returns the exit code.  Idempotent — subsequent
/// calls return the cached exit code.
///
/// Once the child has exited, the underlying `Child` is removed from
/// the process table so the OS can reap the zombie.
pub fn wait_for_handle(handle: i64) -> i32 {
    // Fast path: already cached.
    if let Some(cache) = exit_cache().lock().get(&handle) {
        if let Some(code) = cache.exit_code {
            return code;
        }
    }
    // Slow path: block on the OS. The child stays in the table — see the note
    // there — so `destroy_handle` can still reach it while we are blocked here.
    let Some(child) = child_for(handle) else {
        // Handle unknown (forged, or from a VM whose tables have been reset).
        return -1;
    };
    let status = {
        let mut child = child.lock();
        match child.wait() {
            Ok(s) => s,
            Err(_e) => return -1,
        }
    };
    // On Unix, `ExitStatus::code()` returns None if terminated by
    // signal; map that to 128 + signum pattern as HotSpot does.
    #[cfg(unix)]
    let code = {
        use std::os::unix::process::ExitStatusExt;
        match status.code() {
            Some(c) => c,
            None => 128 + status.signal().unwrap_or(0),
        }
    };
    #[cfg(not(unix))]
    let code = status.code().unwrap_or(-1);

    if let Some(cache) = exit_cache().lock().get_mut(&handle) {
        cache.exit_code = Some(code);
    }
    code
}

/// Non-blocking exit-status probe.  Returns `Some(code)` if the child
/// has already exited (either observed earlier or reaped now); returns
/// `None` if the child is still running.
///
/// This powers `Process.isAlive()` and `ProcessHandleImpl.isAlive0()`.
pub fn try_exit_handle(handle: i64) -> Option<i32> {
    // Cached?
    if let Some(cache) = exit_cache().lock().get(&handle) {
        if let Some(code) = cache.exit_code {
            if pb_debug_enabled() {
                eprintln!("[PB-TRY-CACHED] handle={handle} code={code}");
            }
            return Some(code);
        }
    }
    // `try_wait` returns Ok(None) while still running, Ok(Some(status))
    // after exit.
    let child = child_for(handle)?;
    // A failed `try_lock` means a waiter is inside `Child::wait()`, i.e. the
    // child is running. Report that rather than blocking — this native backs
    // `isAlive()`, which must not stall behind a `waitFor()` on another thread.
    let mut entry = child.try_lock()?;
    match entry.try_wait() {
        Ok(Some(status)) => {
            #[cfg(unix)]
            let code = {
                use std::os::unix::process::ExitStatusExt;
                match status.code() {
                    Some(c) => c,
                    None => 128 + status.signal().unwrap_or(0),
                }
            };
            #[cfg(not(unix))]
            let code = status.code().unwrap_or(-1);
            if let Some(cache) = exit_cache().lock().get_mut(&handle) {
                cache.exit_code = Some(code);
            }
            if pb_debug_enabled() {
                eprintln!("[PB-TRY-EXIT] handle={handle} code={code} status={status:?}");
            }
            Some(code)
        }
        _ => None,
    }
}

/// Send a SIGTERM (or Windows equivalent via `TerminateProcess`) to the
/// subprocess identified by `handle`.  Best-effort: returns `true` if
/// the kill signal was accepted, `false` otherwise (usually because the
/// child has already exited).
pub fn destroy_handle(handle: i64, force: bool) -> bool {
    let Some(child) = child_for(handle) else {
        return false;
    };
    if let Some(mut child) = child.try_lock() {
        return child.kill().is_ok();
    }
    // The child's mutex is held, which can only be a thread inside
    // `Child::wait()` — and with the real JDK's `ProcessImpl` that is the
    // normal state, not a race: its constructor puts a reaper thread there for
    // every child it spawns. Blocking for the lock would mean waiting for the
    // very exit we are trying to cause.
    //
    // Signalling by pid is safe *because* the lock is held: an unreturned
    // `wait()` means the child has not been reaped, so the pid is still the
    // child's and cannot have been recycled onto some unrelated process.
    signal_pid(pid_for_handle(handle), force)
}

/// Send a termination signal straight to a pid.
///
/// Only ever called with a pid that is provably un-reaped — see
/// [`destroy_handle`], which is the only caller. `SIGKILL` for a forcible
/// destroy, `SIGTERM` otherwise, matching what `Child::kill` and HotSpot's
/// `ProcessHandleImpl.destroy0` send.
#[cfg(unix)]
fn signal_pid(pid: i64, force: bool) -> bool {
    if pid <= 0 {
        return false;
    }
    let sig = if force { libc::SIGKILL } else { libc::SIGTERM };
    // SAFETY: `kill` takes two integers and touches no memory the caller owns.
    unsafe { libc::kill(pid as libc::pid_t, sig) == 0 }
}

#[cfg(not(unix))]
fn signal_pid(_pid: i64, _force: bool) -> bool {
    // No portable equivalent without a Win32 OpenProcess/TerminateProcess pair,
    // and the case that needs it — the real `ProcessImpl` reaper holding the
    // child — is Linux-only, since `forkAndExec` is.
    false
}

/// Look up the captured pid for a live-or-recently-exited handle.
pub fn pid_for_handle(handle: i64) -> i64 {
    exit_cache()
        .lock()
        .get(&handle)
        .map(|c| c.pid)
        .unwrap_or(-1)
}

// ---------------------------------------------------------------------------
// Helper utilities for bytecode-land interaction
// ---------------------------------------------------------------------------

/// Read a Java `String[]` into `Vec<String>`.
fn read_string_array(ctx: &mut dyn NativeContext, arr: ObjectRef) -> Vec<String> {
    let len = ctx.array_length(arr);
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        if let Value::Object(Some(s)) = ctx.get_array_element(arr, i) {
            out.push(ctx.read_string(s).unwrap_or_default());
        } else {
            out.push(String::new());
        }
    }
    out
}

/// Tokenize a single command-line string into argv, respecting double
/// quotes so that quoted programs and paths containing spaces (such as
/// `C:\Program Files\...`) stay intact.
///
/// Splitting rules (a pragmatic subset of the Win32 `CommandLineToArgvW`
/// behavior, sufficient for the strings the JDK's `ProcessImpl` builds):
///   * Unquoted runs of whitespace separate tokens.
///   * A `"` toggles "inside-quote" state; whitespace inside quotes is
///     literal and does not split.
///   * A `""` while already inside a quoted section emits a literal `"`
///     (the standard escaping for an embedded double quote).
/// The surrounding quote characters themselves are not kept in the token.
fn tokenize_command_line(cmd_line: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut has_token = false;
    let mut chars = cmd_line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                if in_quotes && chars.peek() == Some(&'"') {
                    // `""` inside a quoted section -> literal quote.
                    chars.next();
                    cur.push('"');
                } else {
                    in_quotes = !in_quotes;
                }
                has_token = true;
            }
            c if c.is_whitespace() && !in_quotes => {
                if has_token {
                    out.push(std::mem::take(&mut cur));
                    has_token = false;
                }
            }
            c => {
                cur.push(c);
                has_token = true;
            }
        }
    }
    if has_token {
        out.push(cur);
    }
    out
}

fn file_path_of(ctx: &mut dyn NativeContext, file_obj: ObjectRef) -> Option<String> {
    match ctx.get_field_by_name(file_obj, "path") {
        Value::Object(Some(s)) => ctx.read_string(s),
        _ => match ctx.get_field(file_obj, 0) {
            Value::Object(Some(s)) => ctx.read_string(s),
            _ => None,
        },
    }
}

fn object_to_string(ctx: &mut dyn NativeContext, obj: ObjectRef) -> Option<String> {
    if let Some(s) = ctx.read_string(obj) {
        return Some(s);
    }
    match ctx.invoke_virtual(obj, "toString", "()Ljava/lang/String;", &[]) {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s),
        _ => None,
    }
}

fn enum_ordinal(ctx: &mut dyn NativeContext, enum_obj: ObjectRef) -> Option<i32> {
    if let Value::Int(v) = ctx.get_field_by_name(enum_obj, "ordinal") {
        return Some(v);
    }
    match ctx.invoke_virtual(enum_obj, "ordinal", "()I", &[]) {
        Ok(Some(Value::Int(v))) => Some(v),
        _ => None,
    }
}

fn read_process_redirect(ctx: &mut dyn NativeContext, redirect_obj: ObjectRef) -> StdioRedirect {
    let type_obj = match ctx.invoke_virtual(
        redirect_obj,
        "type",
        "()Ljava/lang/ProcessBuilder$Redirect$Type;",
        &[],
    ) {
        Ok(Some(Value::Object(Some(o)))) => o,
        _ => return StdioRedirect::Pipe,
    };
    let ordinal = enum_ordinal(ctx, type_obj).unwrap_or(0);
    match ordinal {
        // Redirect.Type.PIPE
        0 => StdioRedirect::Pipe,
        // Redirect.Type.INHERIT
        1 => StdioRedirect::Inherit,
        // Redirect.Type.READ
        2 => match ctx.invoke_virtual(redirect_obj, "file", "()Ljava/io/File;", &[]) {
            Ok(Some(Value::Object(Some(file)))) => file_path_of(ctx, file)
                .map(StdioRedirect::ReadFile)
                .unwrap_or(StdioRedirect::Pipe),
            _ => StdioRedirect::Pipe,
        },
        // Redirect.Type.WRITE / APPEND. DISCARD is represented by WRITE to the
        // JDK's null file, so this path also handles Redirect.DISCARD.
        3 | 4 => match ctx.invoke_virtual(redirect_obj, "file", "()Ljava/io/File;", &[]) {
            Ok(Some(Value::Object(Some(file)))) => {
                let append = ordinal == 4
                    || matches!(
                        ctx.invoke_virtual(redirect_obj, "append", "()Z", &[]),
                        Ok(Some(Value::Int(v))) if v != 0
                    );
                file_path_of(ctx, file)
                    .map(|path| StdioRedirect::WriteFile { path, append })
                    .unwrap_or(StdioRedirect::Null)
            }
            _ => StdioRedirect::Null,
        },
        _ => StdioRedirect::Pipe,
    }
}

fn read_process_redirects(ctx: &mut dyn NativeContext, builder: ObjectRef) -> ProcessRedirects {
    let mut redirects = ProcessRedirects::default();
    let arr = match ctx.get_field_by_name(builder, "redirects") {
        Value::Object(Some(arr)) => arr,
        _ => return redirects,
    };
    if ctx.heap_kind_of(arr) != cratonvm_types::ObjectKind::Array {
        return redirects;
    }
    let len = ctx.array_length(arr);
    if len > 0 {
        if let Value::Object(Some(r)) = ctx.get_array_element(arr, 0) {
            redirects.stdin = read_process_redirect(ctx, r);
        }
    }
    if len > 1 {
        if let Value::Object(Some(r)) = ctx.get_array_element(arr, 1) {
            redirects.stdout = read_process_redirect(ctx, r);
        }
    }
    if len > 2 {
        if let Value::Object(Some(r)) = ctx.get_array_element(arr, 2) {
            redirects.stderr = read_process_redirect(ctx, r);
        }
    }
    redirects
}

/// Slot the synthetic `ProcessBuilder` layout keeps its environment map in
/// (`phases_late`'s `PB_FIELD_ENVIRONMENT`), consulted only when the receiver
/// has no field NAMED `environment`.
///
/// On a real JDK 25 `java.lang.ProcessBuilder` the two coincide -- `command`,
/// `directory`, `environment` are declared in that order -- which is the only
/// reason the by-name read alone ever worked for a `ProcessBuilder` whose
/// `environment()` native writes the indexed slot. That is a coincidence, not
/// a contract, and it does not hold for a fabricated ProcessBuilder with no
/// named fields at all.
const PB_FIELD_ENVIRONMENT: usize = 2;

fn read_process_environment(
    ctx: &mut dyn NativeContext,
    builder: ObjectRef,
) -> Option<Vec<(String, String)>> {
    let env_obj = match ctx.get_field_by_name(builder, "environment") {
        Value::Object(Some(o)) => o,
        _ => match ctx.get_field(builder, PB_FIELD_ENVIRONMENT) {
            Value::Object(Some(o)) => o,
            _ => return None,
        },
    };
    let entry_set = match ctx.invoke_virtual(env_obj, "entrySet", "()Ljava/util/Set;", &[]) {
        Ok(Some(Value::Object(Some(o)))) => o,
        _ => return None,
    };
    let iter = match ctx.invoke_virtual(entry_set, "iterator", "()Ljava/util/Iterator;", &[]) {
        Ok(Some(Value::Object(Some(o)))) => o,
        _ => return None,
    };
    let mut out = Vec::new();
    for _ in 0..100_000 {
        let has_next = matches!(
            ctx.invoke_virtual(iter, "hasNext", "()Z", &[]),
            Ok(Some(Value::Int(v))) if v != 0
        );
        if !has_next {
            break;
        }
        let entry = match ctx.invoke_virtual(iter, "next", "()Ljava/lang/Object;", &[]) {
            Ok(Some(Value::Object(Some(o)))) => o,
            _ => break,
        };
        let key = match ctx.invoke_virtual(entry, "getKey", "()Ljava/lang/Object;", &[]) {
            Ok(Some(Value::Object(Some(o)))) => object_to_string(ctx, o),
            _ => None,
        };
        let value = match ctx.invoke_virtual(entry, "getValue", "()Ljava/lang/Object;", &[]) {
            Ok(Some(Value::Object(Some(o)))) => object_to_string(ctx, o),
            _ => None,
        };
        if let (Some(k), Some(v)) = (key, value) {
            out.push((k, v));
        }
    }
    Some(out)
}

/// Return the process-table handle stored in a `java/lang/Process` synthetic,
/// or 0 if the field is missing (unknown / reaped).
fn handle_of(ctx: &mut dyn NativeContext, proc_ref: ObjectRef) -> i64 {
    match ctx.get_field(proc_ref, PROC_FIELD_HANDLE) {
        Value::Long(h) => h,
        _ => 0,
    }
}

/// Is `this` one of the VM's own `Process` objects — the only receiver whose
/// `PROC_FIELD_*` slots mean anything?
///
/// # Why every concrete `java.lang.Process` native has to ask
///
/// These natives are registered under **both** `cratonvm/synthetic/Process` and
/// `java/lang/Process`. The second registration is what makes the VM's own
/// process reachable through a `java.lang.Process`-typed reference — which is
/// how every caller holds one (`Process p = pb.start()`) — and removing it once
/// raised `NoSuchMethodError` on exactly that path.
///
/// It also puts these natives in front of every **application** subclass of
/// `Process`. For the ABSTRACT methods that is harmless: a concrete subclass
/// must override them, so dispatch finds the override and never walks up here.
/// For the CONCRETE ones — `isAlive`, `pid`, `toHandle`, `destroyForcibly`,
/// `waitFor(long, TimeUnit)` — a subclass normally does *not* override, so
/// dispatch reaches this code with a receiver whose layout is nothing like
/// `PROC_FIELD_COUNT` fields of subprocess bookkeeping. `handle_of` then reads
/// slot `PROC_FIELD_HANDLE` off a stranger, gets `0`, and every one of them took
/// that as "a stub Process" and answered from field bytes that belong to someone
/// else. Measured against HotSpot 25 before this guard existed:
/// `isAlive()` **false** for a live process, `pid()` **0** where the spec
/// requires `UnsupportedOperationException`, `toHandle()` handing back a handle,
/// and `waitFor(0, NANOSECONDS)` **true** for a process that had not exited.
///
/// Contract §1.4 says a `Bridge` loses to real bytecode. For a foreign receiver
/// that is exactly what has to happen, so each caller below delegates to what
/// `java.lang.Process`'s own bytecode does instead of guessing.
///
/// `handle == 0` is deliberately NOT the test. It cannot distinguish "my object,
/// not spawned or already reaped" from "not my object at all", and those need
/// opposite answers — the first is a stub Process the VM owns, the second is
/// someone else's.
///
/// See `docs/known-issues/jdk-only/process-natives-answer-for-user-subclasses.md`
/// and `probes/UserProcessInterceptProbe.java`.
fn is_vm_process(ctx: &mut dyn NativeContext, this: ObjectRef) -> bool {
    let cid = ctx.class_id_of_object(this);
    ctx.class_name_of_id(cid).as_deref() == Some(SYNTHETIC_PROCESS_CLASS)
}

/// `java.lang.Process.exitValue()` on `this`, as the JDK's own concrete methods
/// call it: `Ok(Some(code))` when the process has exited, `Ok(None)` when
/// `exitValue` threw (which is how a `Process` reports "still running", via
/// `IllegalThreadStateException`).
///
/// The thrown exception is consumed rather than propagated because every caller
/// here is a method the JDK specifies as *not* throwing it — `isAlive` and
/// `waitFor(long, TimeUnit)` both turn it into a boolean.
fn foreign_exit_value(ctx: &mut dyn NativeContext, this: ObjectRef) -> Option<i32> {
    match ctx.invoke_virtual(this, "exitValue", "()I", &[]) {
        Ok(Some(Value::Int(code))) => Some(code),
        _ => None,
    }
}

fn time_unit_to_millis(ctx: &mut dyn NativeContext, value: i64, unit: Option<ObjectRef>) -> i64 {
    if value <= 0 {
        return 0;
    }

    if let Some(unit) = unit {
        if let Ok(Some(Value::Long(ms))) =
            ctx.invoke_virtual(unit, "toMillis", "(J)J", &[Value::Long(value)])
        {
            return ms.max(0);
        }

        let ordinal = match ctx.get_field_by_name(unit, "ordinal") {
            Value::Int(o) => o,
            _ => ctx.get_field(unit, 0).as_int().unwrap_or(2),
        };

        fn sat_mul(value: i64, factor: i64) -> i64 {
            value.checked_mul(factor).unwrap_or(i64::MAX)
        }

        return match ordinal {
            0 => value / 1_000_000,
            1 => value / 1_000,
            2 => value,
            3 => sat_mul(value, 1_000),
            4 => sat_mul(value, 60_000),
            5 => sat_mul(value, 3_600_000),
            6 => sat_mul(value, 86_400_000),
            _ => value,
        }
        .max(0);
    }

    value
}

// ---------------------------------------------------------------------------
// Native method implementations
// ---------------------------------------------------------------------------

/// `java.lang.ProcessImpl.create(cmdarray, envblock, dir, stdHandles, redirectErrorStream) -> long`
///
/// Windows-oriented JDK native that the bytecode invokes from the
/// ProcessImpl constructor.  We treat the cmdarray + env + dir pieces
/// just like `ProcessBuilder.start` and return our process-table
/// handle as the "native process handle" long (the JDK stores it in the
/// private `ProcessImpl.handle` field).
///
/// Signature: `(Ljava/lang/String;[Ljava/lang/String;Ljava/lang/String;[JZ)J`
/// — we accept whatever the caller sends and do a best-effort match.
fn native_process_impl_create(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // args[0] = cmd string (pipe-joined on Windows, or a single arg),
    // args[1] = envblock (String[] of KEY=VALUE),
    // args[2] = working dir (String, nullable),
    // args[3] = stdHandles (long[], can be null),
    // args[4] = redirectErrorStream (boolean).
    let cmd_line = match args.first() {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => return Ok(Some(Value::Long(0))),
    };
    // The JDK passes a single pre-built command string here; on Windows
    // its arguments are double-quoted whenever they contain spaces (e.g.
    // `"C:\Program Files\Java\bin\java.exe" -cp "a b"`). Naive whitespace
    // splitting would tear quoted programs/paths apart, so tokenize with
    // double-quote handling instead.
    let parts: Vec<String> = tokenize_command_line(&cmd_line);
    if parts.is_empty() {
        return Ok(Some(Value::Long(0)));
    }
    let program = &parts[0];
    let rest = &parts[1..];

    let env_vars: Option<Vec<(String, String)>> = match args.get(1) {
        Some(Value::Object(Some(arr))) => {
            let raw = read_string_array(ctx, *arr);
            Some(
                raw.iter()
                    .filter_map(|s| {
                        s.split_once('=')
                            .map(|(k, v)| (k.to_string(), v.to_string()))
                    })
                    .collect(),
            )
        }
        _ => None,
    };
    let work_dir = match args.get(2) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s),
        _ => None,
    };

    // args[4] = redirectErrorStream (boolean, passed as Int by the JDK ProcessImpl).
    let redirect_err = matches!(args.get(4), Some(Value::Int(v)) if *v != 0);
    let result = spawn_and_wrap(
        ctx,
        program,
        rest,
        work_dir.as_deref(),
        env_vars.as_deref(),
        env_vars.is_some(),
        redirect_err,
    )?;
    // The native returns the native handle (our internal handle id) so
    // the JDK can later dispatch into native_wait_for0 etc.
    match result {
        Some(Value::Object(Some(proc_ref))) => {
            let handle = handle_of(ctx, proc_ref);
            Ok(Some(Value::Long(handle)))
        }
        _ => Ok(Some(Value::Long(0))),
    }
}

/// `java.lang.ProcessImpl.forkAndExec(int mode, byte[] helperpath, byte[] prog,
/// byte[] argBlock, int argc, byte[] envBlock, int envc, byte[] dir,
/// int[] fds, boolean redirectErrorStream) -> int` (the child's pid)
///
/// The spawn entry point of the REAL JDK's Linux `ProcessImpl`, and therefore
/// of every `ProcessBuilder.start()` / `Runtime.exec` on this platform once the
/// VM stops shadowing `start()`. Three things hang off one call:
///
/// * the returned **pid** becomes `ProcessImpl.pid`, and
///   `ProcessHandleImpl.completion(pid, true)` — started by the constructor
///   immediately after — blocks the reaper thread in `waitForProcessExit0` with
///   it. That is the whole reason [`handle_for_pid`] exists.
/// * the **`int[] fds`** is the only channel by which the child's pipes reach
///   `initStreams`.
/// * a thrown `IOException` is how an unspawnable command is reported;
///   `spawn_child` already raises one.
///
/// # The `fds` in/out contract
///
/// From the JDK's own javadoc on this method: "On input, a value of -1 means to
/// create a pipe to connect child and parent processes. On output, a value
/// which is not -1 is the parent pipe fd corresponding to the pipe which has
/// been created. An element of this array is -1 on input if and only if it is
/// *not* -1 on output."
///
/// Writing the array back is not bookkeeping — it is load-bearing. A native
/// that spawns the child correctly and leaves the array alone hands the caller
/// a live process whose `getInputStream`/`getOutputStream`/`getErrorStream` are
/// all `ProcessBuilder.Null*Stream`, because `initStreams` reads -1 for every
/// slot and takes the null branch. That failure looks like a hung or mute
/// child, not like a missing write-back.
///
/// A non-(-1) input is a descriptor the caller already owns, and the values are
/// **`FdTable` ids, not OS descriptors** — the JDK obtains them with
/// `fdAccess.get(fis.getFD())`, and in this VM a `FileInputStream`'s `fd.fd`
/// holds an `FdTable` id (see `fos_get_fd`). Ids 0, 1 and 2 are permanently the
/// VM's own stdin/stdout/stderr (`FileDescriptorTable::new` seeds them and the
/// counter starts at 3), and those are precisely the values `Redirect.INHERIT`
/// writes into slots 0, 1 and 2 — so "inherit" and "this descriptor" name the
/// same three streams and cannot be confused for one another.
#[cfg(target_os = "linux")]
fn native_process_impl_fork_and_exec(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // `forkAndExec` is an INSTANCE method (`private native int forkAndExec`),
    // so args[0] is the `ProcessImpl` under construction and every declared
    // parameter sits one slot to the right of its declaration index. The dead
    // `java.lang.UNIXProcess` registration this replaces decoded from index 0,
    // i.e. off by one throughout; the class has not existed since JDK 9, so
    // nothing ever called it and the error had no way to surface.
    const A_PROG: usize = 3;
    const A_ARGBLOCK: usize = 4;
    const A_ARGC: usize = 5;
    const A_ENVBLOCK: usize = 6;
    const A_ENVC: usize = 7;
    const A_DIR: usize = 8;
    const A_FDS: usize = 9;
    const A_REDIRECT_ERR: usize = 10;

    let prog_bytes = match args.get(A_PROG) {
        Some(Value::Object(Some(arr))) => read_byte_array(ctx, *arr),
        _ => {
            return Err(RuntimeError::IOException {
                message: "ProcessImpl.forkAndExec: null program".to_string(),
            }
            .into())
        }
    };
    let program = decode_c_string(&prog_bytes);

    let argc = int_arg(args, A_ARGC).max(0) as usize;
    let arg_list = match args.get(A_ARGBLOCK) {
        Some(Value::Object(Some(arr))) => {
            let block = read_byte_array(ctx, *arr);
            split_nul_block(&block, argc)
        }
        _ => Vec::new(),
    };

    let envc = int_arg(args, A_ENVC).max(0) as usize;
    let env_vars: Option<Vec<(String, String)>> = match args.get(A_ENVBLOCK) {
        Some(Value::Object(Some(arr))) => {
            let block = read_byte_array(ctx, *arr);
            Some(
                split_nul_block(&block, envc)
                    .into_iter()
                    .filter_map(|entry| {
                        entry
                            .split_once('=')
                            .map(|(k, v)| (k.to_string(), v.to_string()))
                    })
                    .collect(),
            )
        }
        // A null envBlock means "inherit this process's environment", which is
        // `clear_env = false` and no explicit vars.
        _ => None,
    };

    let work_dir = match args.get(A_DIR) {
        Some(Value::Object(Some(arr))) => {
            let bytes = read_byte_array(ctx, *arr);
            Some(decode_c_string(&bytes))
        }
        _ => None,
    };

    let redirect_err = int_arg(args, A_REDIRECT_ERR) != 0;

    // --- Decode the requested stdio shape from `fds` -----------------------
    let fds_array = match args.get(A_FDS) {
        Some(Value::Object(Some(arr))) => Some(*arr),
        _ => None,
    };
    let mut requested = [-1i32; 3];
    if let Some(arr) = fds_array {
        let len = ctx.array_length(arr).min(3);
        for (slot, want) in requested.iter_mut().enumerate().take(len) {
            if let Value::Int(v) = ctx.get_array_element(arr, slot) {
                *want = v;
            }
        }
    }
    let redirects = ProcessRedirects {
        stdin: redirect_from_requested_fd(0, requested[0]),
        stdout: redirect_from_requested_fd(1, requested[1]),
        stderr: redirect_from_requested_fd(2, requested[2]),
    };

    let spawned = spawn_child(
        ctx,
        &program,
        &arg_list,
        work_dir.as_deref(),
        env_vars.as_deref(),
        env_vars.is_some(),
        redirect_err,
        &redirects,
    )?;

    // --- Write the parent-side pipe ids back -------------------------------
    //
    // `spawn_child` reports -1 for any slot it did not pipe, which is exactly
    // the value `initStreams` reads as "no pipe, use the null stream". So the
    // write-back is unconditional per slot: a slot the caller supplied a
    // descriptor for was not piped, gets -1, and satisfies the "-1 on output
    // iff not -1 on input" half of the contract for free.
    //
    // One documented departure: under `redirectErrorStream` the child's stderr
    // is merged into the stdout pipe here rather than dup2'd in the child, so
    // slot 2 is -1 on input AND on output. That breaks the letter of the "iff",
    // and the observable consequence is the intended one — `getErrorStream()`
    // returns `NullInputStream`, which is what a merged stream means. HotSpot
    // reaches the same observable state by creating a stderr pipe nothing ever
    // writes to.
    if let Some(arr) = fds_array {
        let parent_side = [
            spawned.fds.stdin_fd,
            spawned.fds.stdout_fd,
            spawned.fds.stderr_fd,
        ];
        let len = ctx.array_length(arr).min(3);
        for (slot, value) in parent_side.iter().enumerate().take(len) {
            ctx.set_array_element(arr, slot, Value::Int(*value));
        }
    }

    if pb_debug_enabled() {
        eprintln!(
            "[PB-FORKEXEC] pid={} requested={:?} returned={:?}",
            spawned.pid,
            requested,
            [
                spawned.fds.stdin_fd,
                spawned.fds.stdout_fd,
                spawned.fds.stderr_fd
            ]
        );
    }

    Ok(Some(Value::Int(spawned.pid as i32)))
}

/// Map one `fds[slot]` input value to the redirect it asks for.
///
/// See the contract note on [`native_process_impl_fork_and_exec`]: -1 asks for
/// a pipe, the slot's own index names the VM's corresponding standard stream
/// (which is what `Redirect.INHERIT` encodes), and anything else is an
/// `FdTable` id the caller opened.
#[cfg(target_os = "linux")]
fn redirect_from_requested_fd(slot: usize, requested: i32) -> StdioRedirect {
    if requested == -1 {
        StdioRedirect::Pipe
    } else if requested == slot as i32 {
        StdioRedirect::Inherit
    } else {
        StdioRedirect::ExistingFd(requested)
    }
}

/// Read a Java `byte[]` in one bulk copy.
///
/// AUDIT 2026-05-24: bulk read via the `NativeContext` intrinsic rather than
/// per-element `get_array_element` — a single memcpy from the heap payload.
fn read_byte_array(ctx: &mut dyn NativeContext, arr: ObjectRef) -> Vec<u8> {
    let len = ctx.array_length(arr);
    let mut out = vec![0u8; len];
    let n = ctx.read_byte_array_into(arr, 0, &mut out);
    out.truncate(n);
    out
}

/// Decode one NUL-terminated string from a JDK `toCString` byte array.
fn decode_c_string(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

/// Split a JDK arg/env block: exactly `count` NUL-terminated strings, laid end
/// to end (`ProcessImpl.start` builds them with `arraycopy` and relies on the
/// array being zero-filled, so there is a NUL after each one including the
/// last).
///
/// `count` comes from the `argc`/`envc` parameter and is not inferred. The
/// previous decoder split on NUL and dropped every empty piece, which also
/// drops a legitimately empty argument — `new ProcessBuilder("printf", "[%s]",
/// "")` would have lost its last argument and printed one field instead of two.
fn split_nul_block(bytes: &[u8], count: usize) -> Vec<String> {
    bytes
        .split(|&b| b == 0)
        .take(count)
        .map(|piece| String::from_utf8_lossy(piece).into_owned())
        .collect()
}

/// Read an `int`/`boolean` argument, defaulting to 0 when absent or not an int.
fn int_arg(args: &[Value], index: usize) -> i32 {
    match args.get(index) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    }
}

/// Resolve a JDK-visible **pid** to our internal process-table **handle**.
///
/// The four `ProcessHandleImpl` natives are handed a pid by the JDK's own
/// bytecode -- `isAlive0(pid)`, `waitForProcessExit0(pid, ..)`,
/// `destroyProcess0(pid, ..)`, `destroy0(pid, startTime, ..)` -- while
/// `try_exit_handle` / `wait_for_handle` / `destroy_handle` are keyed by
/// `NEXT_HANDLE`, a counter that starts at 1. All four used to pass the pid
/// straight through as if it were a handle. Those are two unrelated number
/// spaces, so every one of them addressed a child that did not exist (or, for
/// a pid that happened to be a small integer, the WRONG child): `isAlive0`
/// answered "still running" for every pid forever, `waitForProcessExit0`
/// returned -1 without waiting, and `destroyProcess0` killed nothing and said
/// so.
///
/// The entry survives the child's death -- `wait_for_handle` records the exit
/// code in `exit_cache` rather than removing the row -- so a reaped child
/// still resolves, which is exactly the case `isAlive0` has to get right.
fn handle_for_pid(pid: i64) -> Option<i64> {
    exit_cache()
        .lock()
        .iter()
        .find(|(_, cache)| cache.pid == pid)
        .map(|(handle, _)| *handle)
}

/// Is a pid we did not spawn still alive?
///
/// `/proc/<pid>` is present for a zombie too, which is the answer we want: a
/// process that has exited but not been reaped is still a process. Only
/// meaningful on Linux; elsewhere there is no portable probe, and answering a
/// confident `false` would make `ProcessHandle.of(pid).isAlive()` claim a
/// running process had exited, so the optimistic answer stands.
fn foreign_pid_is_alive(pid: i64) -> bool {
    #[cfg(target_os = "linux")]
    {
        std::path::Path::new(&format!("/proc/{pid}")).exists()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        true
    }
}

/// `java.lang.ProcessHandleImpl.getCurrentPid0() -> long`
fn native_proc_handle_current_pid0(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Long(std::process::id() as i64)))
}

/// `java.lang.ProcessHandleImpl.isAlive0(long) -> long`
///
/// The `long` returned is a start-time, not a boolean: `ProcessHandleImpl`
/// reads it as `STARTTIME_PROCESS_UNKNOWN` (-1) for "no such process",
/// `STARTTIME_ANY` (0) for "exists, start time unavailable", and any positive
/// value as the start time itself. We have no cheap start time, so alive is
/// reported as the constant 1 — `isAlive()` compares it against the value it
/// cached at construction, and a constant compares equal to itself.
///
/// The distinction between 0 and -1 is load-bearing. Returning 0 for a process
/// that does not exist reads as "exists, unknown start time"; the reaper's
/// `NOT_A_CHILD` fallback loop spins `while (startTime >= 0)` and would never
/// leave it.
fn native_proc_handle_is_alive0(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let pid = match args.first() {
        Some(Value::Long(p)) => *p,
        _ => return Ok(Some(Value::Long(-1))),
    };
    // The return value is NOT a boolean. `ProcessHandleImpl.isAlive()` reads it
    // as the process's START TIME and decides liveness from the sign:
    //
    //     long startTime = isAlive0(pid);
    //     return startTime >= 0
    //         && (startTime == this.startTime || startTime == 0
    //             || this.startTime == 0);
    //
    // so ANY value >= 0 means alive, and -1 is the only way to say "not
    // alive". This native used to return 1 for running and **0 for exited**,
    // and 0 is `>= 0` -- with `this.startTime == 0` on every handle
    // `build_process_handle` makes, the third disjunct then made
    // `ProcessHandle.isAlive()` answer `true` for a child that had already
    // been waited for, while `Process.isAlive()` on the same child correctly
    // answered `false` (probes/ProcHandleProbe.java, CHILD_ALIVE_AFTER_WAIT).
    //
    // 0 is the JDK's own "alive, start time unknown" value, which is honest:
    // we do not track a start time, and every comparison against it is
    // short-circuited by the `startTime == 0` disjunct anyway.
    if pid == std::process::id() as i64 {
        return Ok(Some(Value::Long(0)));
    }
    match handle_for_pid(pid) {
        // One of our own children: the process table knows for certain.
        Some(handle) => match try_exit_handle(handle) {
            Some(_) => Ok(Some(Value::Long(-1))), // exited
            None => Ok(Some(Value::Long(0))),     // still running
        },
        // Someone else's process: ask the OS.
        None => Ok(Some(Value::Long(if foreign_pid_is_alive(pid) {
            0
        } else {
            -1
        }))),
    }
}

/// `java.lang.ProcessHandleImpl.waitForProcessExit0(long, boolean) -> int`
///
/// This is the native the real JDK's per-process reaper thread blocks in —
/// `ProcessImpl`'s constructor ends in `ProcessHandleImpl.completion(pid, true)`,
/// and everything the JDK later reports about the child (`waitFor`,
/// `exitValue`, `isAlive`, `onExit`) is settled by what this returns.
fn native_proc_handle_wait_for_process_exit0(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Keyed by pid, like the rest of this bridge -- see `handle_for_pid`.
    let pid = match args.first() {
        Some(Value::Long(p)) => *p,
        _ => return Ok(Some(Value::Int(-1))),
    };
    // A pid we did not spawn cannot be waited for: `wait(2)` only works on
    // one's own children. -1 is what the JDK's own native reports there.
    let Some(handle) = handle_for_pid(pid) else {
        return Ok(Some(Value::Int(-1)));
    };
    ctx.begin_blocking_region();
    let code = wait_for_handle(handle);
    ctx.end_blocking_region();
    Ok(Some(Value::Int(code)))
}

/// `java.lang.ProcessHandleImpl.destroyProcess0(long, boolean) -> boolean`
fn native_proc_handle_destroy_process0(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Keyed by pid, like the rest of this bridge -- see `handle_for_pid`.
    let pid = match args.first() {
        Some(Value::Long(p)) => *p,
        _ => return Ok(Some(Value::Int(0))),
    };
    let force = matches!(args.get(1), Some(Value::Int(1)));
    // Refusing to signal a process we did not spawn is deliberate: this bridge
    // has no ownership check of its own, and `false` is a legal answer
    // ("could not be destroyed"), unlike killing the wrong pid.
    let ok = match handle_for_pid(pid) {
        Some(handle) => destroy_handle(handle, force),
        None => false,
    };
    Ok(Some(Value::Int(if ok { 1 } else { 0 })))
}

/// `java.lang.Process.waitFor()I` on our synthetic Process object.
fn native_process_wait_for(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let handle = handle_of(ctx, this);
    if handle == 0 {
        // Legacy / stub Process (from phases_late ProcessBuilder.start) -
        // fall back to reading the cached exit code in field 0.
        return Ok(Some(ctx.get_field(this, PROC_FIELD_EXIT)));
    }
    let mut held = [Value::Object(Some(this))];
    ctx.begin_blocking_region();
    let code = wait_for_handle(handle);
    ctx.end_blocking_region_refs(&mut held);
    if let Value::Object(Some(updated)) = held[0] {
        this = updated;
    }
    ctx.set_field(this, PROC_FIELD_EXIT, Value::Int(code));
    Ok(Some(Value::Int(code)))
}

/// `java.lang.Process.waitFor(long, TimeUnit)Z`.
///
/// WildFly uses this overload to detect launch failures without blocking
/// forever on the server process. A missing registration raised
/// `NoSuchMethodError`; delegating to the blocking `waitFor()` would be just as
/// bad for long-lived servers, so poll the process table until the timeout
/// expires.
fn native_process_wait_for_timeout(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let timeout = match args.get(1) {
        Some(Value::Long(v)) => *v,
        Some(Value::Int(v)) => *v as i64,
        _ => 0,
    };
    let unit = match args.get(2) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    };

    if !is_vm_process(ctx, this) {
        // `Process.waitFor(long, TimeUnit)` is specified as a poll of
        // `exitValue()`: exited -> true, still `IllegalThreadStateException` at
        // the deadline -> false. A zero/negative timeout is a single test, which
        // is the case that made the old code answer `true` for a process that
        // had not exited.
        if foreign_exit_value(ctx, this).is_some() {
            return Ok(Some(Value::Int(1)));
        }
        let millis = time_unit_to_millis(ctx, timeout, unit);
        if millis <= 0 {
            return Ok(Some(Value::Int(0)));
        }
        let Some(deadline) = Instant::now().checked_add(Duration::from_millis(millis as u64))
        else {
            return Ok(Some(Value::Int(0)));
        };
        loop {
            let now = Instant::now();
            if now >= deadline {
                return Ok(Some(Value::Int(0)));
            }
            // Only the SLEEP goes inside the blocked region. A collection that
            // starts while this thread naps must not have to wait for the
            // timeout to expire — the thread is in `NativeRunning`, which the
            // STW census waits for, so an unblocked 30-second nap is a
            // 30-second GC pause. `this` is re-read afterwards because a moving
            // collection during the block relocates it.
            //
            // `foreign_exit_value` stays OUTSIDE: it runs arbitrary application
            // bytecode, which can allocate, take monitors and re-enter the VM,
            // none of which is legal while the thread is counted as blocked.
            let remaining = deadline.saturating_duration_since(now);
            let mut held = [Value::Object(Some(this))];
            ctx.begin_blocking_region();
            std::thread::sleep(remaining.min(Duration::from_millis(10)));
            ctx.end_blocking_region_refs(&mut held);
            if let Value::Object(Some(updated)) = held[0] {
                this = updated;
            }
            if foreign_exit_value(ctx, this).is_some() {
                return Ok(Some(Value::Int(1)));
            }
        }
    }
    let handle = handle_of(ctx, this);
    if handle == 0 {
        let exited = !matches!(
            ctx.get_field(this, PROC_FIELD_EXIT),
            Value::Int(EXIT_NOT_YET)
        );
        return Ok(Some(Value::Int(if exited { 1 } else { 0 })));
    }

    if let Some(code) = try_exit_handle(handle) {
        ctx.set_field(this, PROC_FIELD_EXIT, Value::Int(code));
        return Ok(Some(Value::Int(1)));
    }
    if timeout <= 0 {
        return Ok(Some(Value::Int(0)));
    }

    let mut timeout_ms = time_unit_to_millis(ctx, timeout, unit);
    if timeout_ms == 0 {
        timeout_ms = 1;
    }
    let timeout_ms = timeout_ms as u64;
    let Some(deadline) = Instant::now().checked_add(Duration::from_millis(timeout_ms)) else {
        let mut held = [Value::Object(Some(this))];
        ctx.begin_blocking_region();
        let code = wait_for_handle(handle);
        ctx.end_blocking_region_refs(&mut held);
        if let Value::Object(Some(updated)) = held[0] {
            this = updated;
        }
        ctx.set_field(this, PROC_FIELD_EXIT, Value::Int(code));
        return Ok(Some(Value::Int(1)));
    };

    loop {
        if let Some(code) = try_exit_handle(handle) {
            ctx.set_field(this, PROC_FIELD_EXIT, Value::Int(code));
            return Ok(Some(Value::Int(1)));
        }

        let now = Instant::now();
        if now >= deadline {
            return Ok(Some(Value::Int(0)));
        }

        let remaining = deadline.saturating_duration_since(now);
        let mut held = [Value::Object(Some(this))];
        ctx.begin_blocking_region();
        std::thread::sleep(remaining.min(Duration::from_millis(10)));
        ctx.end_blocking_region_refs(&mut held);
        if let Value::Object(Some(updated)) = held[0] {
            this = updated;
        }
    }
}

/// Allocate a real, initially-incomplete `CompletableFuture`.
///
/// `new_object` rather than the `completedFuture` factory is deliberate: the
/// no-arg constructor adds nothing beyond the null/zero fields allocation
/// already installs, whereas a pre-completed future is exactly the lie this
/// method exists to avoid. Same helper shape as `async_socket`'s
/// `aio_pending_future`, which completes its futures the same way.
fn pending_future(ctx: &mut dyn NativeContext) -> Result<ObjectRef, MethodCallFailed> {
    match ctx.new_object("java/util/concurrent/CompletableFuture")? {
        Some(Value::Object(Some(future))) => Ok(future),
        _ => Err(RuntimeError::IOException {
            message: "Process.onExit: could not allocate CompletableFuture".to_string(),
        }
        .into()),
    }
}

/// Complete `future` with `process`, first publishing `code` into the
/// process's cached exit-status field.
///
/// `complete` is dispatched virtually on purpose: the VM's synthetic
/// `CompletableFuture` model overrides it, and the real-JDK native behind it
/// is real-aware (it drives the real `postComplete` waiter release), so this
/// one call is correct under both JDK modes.
fn complete_exit_future(
    ctx: &mut dyn NativeContext,
    process: ObjectRef,
    future: ObjectRef,
    code: i32,
) -> MethodCallResult {
    ctx.set_field(process, PROC_FIELD_EXIT, Value::Int(code));
    ctx.invoke_virtual(
        future,
        "complete",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(process))],
    )
}

/// `java.lang.Process.onExit()Ljava/util/concurrent/CompletableFuture;`
///
/// HotSpot returns a `CompletableFuture<Process>` that completes with *this*
/// process once the child exits. It was the one `Process` virtual missing from
/// the dual registration below, so in compatible mode — where
/// `ProcessBuilder.start()` is shadowed to produce a
/// `cratonvm/synthetic/Process`, whose class chain never reaches
/// `java/lang/Process` — it raised `NoSuchMethodError` with no bytecode left
/// to fall back to.
///
/// A child that has ALREADY exited is completed inline: that is a fact, not a
/// constant standing in for one. A child that is still running gets a genuinely
/// incomplete future plus a daemon reaper thread (see
/// `SYNTHETIC_PROCESS_EXIT_WAITER`); `isDone()` stays false until the child
/// really exits, which is what distinguishes this from the pre-completed stub
/// `ProcessHandle.onExit()` used to be.
///
/// Two `onExit()` calls on the same live child produce two futures and two
/// reaper threads, where real JDK shares one completion per pid
/// (`ProcessHandleImpl.completions`). Both futures still complete with the same
/// process and the same exit code, so the difference is not observable through
/// the `Process` API; `wait_for_handle` serialises on the process table and
/// caches the code, so the second waiter returns the cached value.
fn native_process_on_exit(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some("Process.onExit on a null receiver".to_string()),
            }
            .into())
        }
    };
    let handle = handle_of(ctx, this);

    // The future allocation can move `this`, and every later step reads it.
    let this_pin = ctx.pin_native_root(this);
    let future = match pending_future(ctx) {
        Ok(future) => future,
        Err(error) => {
            ctx.unpin_native_roots(this_pin);
            return Err(error);
        }
    };
    let this = ctx.read_native_pin(this_pin, this);
    let future_pin = ctx.pin_native_root(future);

    // Legacy / stub Process (handle 0) has no live child to wait on — its exit
    // code is whatever is already cached in field 0, exactly as `waitFor`
    // reads it, so the future is complete on arrival.
    //
    // Otherwise probe without blocking: `try_exit_handle` returning `Some`
    // means the child has genuinely terminated (this is the `p.waitFor();
    // p.onExit()` ordering, where the child was already reaped).
    let settled = if handle == 0 {
        match ctx.get_field(this, PROC_FIELD_EXIT) {
            Value::Int(code) => Some(code),
            _ => Some(-1),
        }
    } else {
        try_exit_handle(handle)
    };
    if let Some(code) = settled {
        let this = ctx.read_native_pin(this_pin, this);
        let future = ctx.read_native_pin(future_pin, future);
        let result = complete_exit_future(ctx, this, future, code);
        let future = ctx.read_native_pin(future_pin, future);
        ctx.unpin_native_roots(this_pin);
        result?;
        return Ok(Some(Value::Object(Some(future))));
    }

    // Still running: park the wait on a daemon thread so the caller gets an
    // incomplete future back immediately.
    let this = ctx.read_native_pin(this_pin, this);
    let future = ctx.read_native_pin(future_pin, future);
    let spawned = spawn_exit_waiter(ctx, this, future, handle);
    let future = ctx.read_native_pin(future_pin, future);
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);

    if !spawned {
        // No reaper thread — completing the future is now this thread's job.
        // Blocking here is worse than HotSpot's asynchrony, but it is the only
        // remaining way to keep the future's answer true.
        let mut held = [Value::Object(Some(this)), Value::Object(Some(future))];
        ctx.begin_blocking_region();
        let code = wait_for_handle(handle);
        ctx.end_blocking_region_refs(&mut held);
        let (Value::Object(Some(this)), Value::Object(Some(future))) = (held[0], held[1]) else {
            return Ok(Some(Value::Object(Some(future))));
        };
        let future_pin = ctx.pin_native_root(future);
        let result = complete_exit_future(ctx, this, future, code);
        let future = ctx.read_native_pin(future_pin, future);
        ctx.unpin_native_roots(future_pin);
        result?;
        return Ok(Some(Value::Object(Some(future))));
    }

    Ok(Some(Value::Object(Some(future))))
}

/// Start the daemon reaper thread for `onExit()`. Returns false if the thread
/// could not be created, leaving the caller to complete the future itself.
fn spawn_exit_waiter(
    ctx: &mut dyn NativeContext,
    process: ObjectRef,
    future: ObjectRef,
    handle: i64,
) -> bool {
    let process_pin = ctx.pin_native_root(process);
    let future_pin = ctx.pin_native_root(future);

    let waiter_class =
        ctx.ensure_synthetic_class(SYNTHETIC_PROCESS_EXIT_WAITER, EXIT_WAITER_FIELD_COUNT);
    let waiter = ctx.alloc_object(waiter_class, EXIT_WAITER_FIELD_COUNT);
    let waiter_pin = ctx.pin_native_root(waiter);
    ctx.set_field(
        waiter,
        EXIT_WAITER_FIELD_PROCESS,
        Value::Object(Some(ctx.read_native_pin(process_pin, process))),
    );
    ctx.set_field(
        waiter,
        EXIT_WAITER_FIELD_FUTURE,
        Value::Object(Some(ctx.read_native_pin(future_pin, future))),
    );
    ctx.set_field(waiter, EXIT_WAITER_FIELD_HANDLE, Value::Long(handle));

    // `create_string` allocates — refresh the waiter through its pin after it.
    let name = ctx.create_string(&format!("process-reaper-{handle}"));
    let name_pin = ctx.pin_native_root(name);
    let waiter = ctx.read_native_pin(waiter_pin, waiter);
    let name = ctx.read_native_pin(name_pin, name);
    let thread = ctx.new_object_initialized(
        "java/lang/Thread",
        "(Ljava/lang/Runnable;Ljava/lang/String;)V",
        &[Value::Object(Some(waiter)), Value::Object(Some(name))],
    );
    ctx.unpin_native_roots(process_pin);

    let thread = match thread {
        Ok(Some(Value::Object(Some(thread)))) => thread,
        _ => return false,
    };
    // Daemon, like real JDK's process reapers: a pending onExit() must not
    // hold the VM open past the last application thread.
    let thread_pin = ctx.pin_native_root(thread);
    let _ = ctx.invoke_virtual(thread, "setDaemon", "(Z)V", &[Value::Int(1)]);
    let thread = ctx.read_native_pin(thread_pin, thread);
    ctx.unpin_native_roots(thread_pin);
    ctx.invoke_virtual(thread, "start", "()V", &[]).is_ok()
}

/// `run()` of the `onExit()` reaper thread — blocks on the child, then
/// completes the future with the `Process`.
fn native_process_exit_waiter_run(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let handle = match ctx.get_field(this, EXIT_WAITER_FIELD_HANDLE) {
        Value::Long(h) => h,
        _ => return Ok(None),
    };

    // Read both refs out BEFORE the blocking region and carry them through it:
    // `wait_for_handle` can block for the child's whole lifetime, across any
    // number of moving collections, so a raw local read afterwards would be a
    // stale address (the native stale-local family).
    let mut held = [
        ctx.get_field(this, EXIT_WAITER_FIELD_PROCESS),
        ctx.get_field(this, EXIT_WAITER_FIELD_FUTURE),
    ];
    ctx.begin_blocking_region();
    let code = wait_for_handle(handle);
    ctx.end_blocking_region_refs(&mut held);

    let (Value::Object(Some(process)), Value::Object(Some(future))) = (held[0], held[1]) else {
        return Ok(None);
    };
    complete_exit_future(ctx, process, future, code)?;
    Ok(None)
}

/// `java.lang.Process.exitValue()I`
///
/// Throws `IllegalThreadStateException` if the process is still running
/// (matches HotSpot behavior).
fn native_process_exit_value(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let handle = handle_of(ctx, this);
    if handle == 0 {
        return Ok(Some(ctx.get_field(this, PROC_FIELD_EXIT)));
    }
    match try_exit_handle(handle) {
        Some(code) => {
            ctx.set_field(this, PROC_FIELD_EXIT, Value::Int(code));
            Ok(Some(Value::Int(code)))
        }
        None => Err(RuntimeError::IllegalThreadStateException {
            message: "process has not exited".to_string(),
        }
        .into()),
    }
}

/// `java.lang.Process.isAlive()Z`
fn native_process_is_alive(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    if !is_vm_process(ctx, this) {
        // `Process.isAlive()` is `try { exitValue(); return false; }
        // catch (IllegalThreadStateException) { return true; }`.
        let alive = foreign_exit_value(ctx, this).is_none();
        return Ok(Some(Value::Int(if alive { 1 } else { 0 })));
    }
    let handle = handle_of(ctx, this);
    if handle == 0 {
        return Ok(Some(Value::Int(0)));
    }
    let alive = try_exit_handle(handle).is_none();
    Ok(Some(Value::Int(if alive { 1 } else { 0 })))
}

/// `java.lang.Process.destroy()V`
fn native_process_destroy(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let handle = handle_of(ctx, this);
    if handle != 0 {
        destroy_handle(handle, false);
    }
    Ok(None)
}

/// `java.lang.Process.destroyForcibly()Ljava/lang/Process;`
fn native_process_destroy_forcibly(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(args.first().copied()),
    };
    if !is_vm_process(ctx, this) {
        // `Process.destroyForcibly()` is `destroy(); return this;` — and
        // `destroy()` is abstract, so this reaches the subclass's override.
        ctx.invoke_virtual(this, "destroy", "()V", &[])?;
        return Ok(Some(Value::Object(Some(this))));
    }
    let handle = handle_of(ctx, this);
    if handle != 0 {
        destroy_handle(handle, true);
    }
    Ok(Some(Value::Object(Some(this))))
}

/// `java.lang.Process.toHandle()Ljava/lang/ProcessHandle;` (JDK 9+).
///
/// Was entirely unregistered here, so any CratonVM-backed `Process` (real
/// or synthetic-JDK mode alike, since `spawn_and_wrap` always allocates the
/// object under `SYNTHETIC_PROCESS_CLASS`/`java/lang/Process` regardless of
/// mode) threw `NoSuchMethodError` on `.toHandle()`
/// (fixed-suite-bugs/wildfly/wildfly-process-tohandle-missing-FIXED.md).
///
/// Builds a REAL `java.lang.ProcessHandleImpl(pid, startTime)` rather than
/// a bare 1-field synthetic `java/lang/ProcessHandle` — an interface, whose
/// abstract `pid()` has no Code attribute, so dispatching a virtual call on
/// an object allocated directly under the interface's own (real, loadable)
/// class id throws AbstractMethodError instead of falling back to the
/// native registry (confirmed empirically: `ensure_class_initialized`
/// succeeds for `java/lang/ProcessHandle` even without `--java-home`, so the
/// `Err(_)` synthetic-fallback branch this VM uses elsewhere never triggers
/// here). `ProcessHandleImpl` is a concrete class with real method bodies,
/// so `.pid()` runs actual bytecode and returns the pid we pass in — which
/// matches `Process.pid()` per the JDK's documented `pid() ==
/// toHandle().pid()` contract.
///
/// Known gap (not fixed here): `isAlive()`/`destroy()`/`waitFor()` on the
/// returned handle route through `ProcessHandleImpl`'s already-registered
/// natives (`isAlive0`/`destroy0`/`waitForProcessExit0`). Those used to key
/// off the VM's *internal* subprocess-table handle rather than the real OS pid
/// stored here, so they tracked no child at all; they now resolve the pid
/// through `handle_for_pid`, which is the "queryable by real pid" the previous
/// note deferred.
fn native_process_to_handle(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some("Process.toHandle: null this".to_string()),
            }
            .into())
        }
    };
    if !is_vm_process(ctx, this) {
        // `Process.toHandle()`'s implementation on the abstract class is
        // `throw new UnsupportedOperationException(...)`. A subclass that wants
        // a handle overrides it, in which case dispatch never arrives here.
        return Err(RuntimeError::UnsupportedOperationException {
            message: "Process.toHandle()".to_string(),
        }
        .into());
    }
    let pid = match ctx.get_field(this, PROC_FIELD_PID) {
        Value::Long(p) => p,
        _ => -1,
    };
    Ok(Some(Value::Object(Some(build_process_handle(ctx, pid)))))
}

/// Build a 1-field `java/lang/ProcessHandle` (field 0 = pid) — the exact
/// layout `ProcessHandle.current()` in `phases_late.rs` already
/// establishes, so the `pid()`/`isAlive()` natives registered there
/// dispatch correctly against it regardless of real-JDK vs synthetic mode.
/// Mirrors `native-builtins`'s `alloc_concurrent_synthetic` (not reusable
/// here directly: `native-io` sits below `native-builtins` in the crate
/// dependency graph).
fn alloc_process_handle(ctx: &mut dyn NativeContext, pid: Value) -> ObjectRef {
    let obj = match ctx.ensure_class_initialized("java/lang/ProcessHandle") {
        Ok(cid) => {
            let real = ctx.class_num_total_fields(cid);
            let n = 1usize.max(real);
            ctx.alloc_object(cid, n)
        }
        Err(_) => {
            let cid = ctx.ensure_synthetic_class("java/lang/ProcessHandle", 1);
            ctx.alloc_object(cid, 1)
        }
    };
    ctx.set_field(obj, 0, pid);
    obj
}

/// Build a ProcessHandle for a bare pid, preferring a real
/// java.lang.ProcessHandleImpl(pid, startTime) and falling back to the
/// 1-field synthetic java/lang/ProcessHandle layout when no real class is
/// loadable. Shared by Process.toHandle() and Process.descendants().
fn build_process_handle(ctx: &mut dyn NativeContext, pid: i64) -> ObjectRef {
    match ctx.new_object_initialized(
        "java/lang/ProcessHandleImpl",
        "(JJ)V",
        &[Value::Long(pid), Value::Long(0)],
    ) {
        Ok(Some(Value::Object(Some(handle)))) => handle,
        _ => alloc_process_handle(ctx, Value::Long(pid)),
    }
}

/// Direct child pids of `pid`, sourced from /proc/<pid>/task/*/children —
/// every thread's children file is read since a child can be reparented to
/// any thread of a multi-threaded parent. Empty on non-Linux targets (no
/// portable equivalent; matches this module's existing Linux-only process
/// introspection, see native_unix_fork_and_exec).
#[cfg(target_os = "linux")]
fn direct_child_pids(pid: i64) -> Vec<i64> {
    let mut children = Vec::new();
    let task_dir = format!("/proc/{pid}/task");
    let Ok(entries) = std::fs::read_dir(&task_dir) else {
        return children;
    };
    for entry in entries.flatten() {
        let children_path = entry.path().join("children");
        if let Ok(content) = std::fs::read_to_string(&children_path) {
            for tok in content.split_whitespace() {
                if let Ok(cpid) = tok.parse::<i64>() {
                    children.push(cpid);
                }
            }
        }
    }
    children
}

#[cfg(not(target_os = "linux"))]
fn direct_child_pids(_pid: i64) -> Vec<i64> {
    Vec::new()
}

/// Every live descendant (children, grandchildren, ...) of `pid`, in
/// breadth-first discovery order — the same "descendants" contract as
/// java.lang.Process.descendants()/ProcessHandle.descendants().
fn collect_descendant_pids(pid: i64) -> Vec<i64> {
    let mut result = Vec::new();
    let mut seen = std::collections::HashSet::new();
    seen.insert(pid);
    let mut queue = std::collections::VecDeque::new();
    queue.push_back(pid);
    while let Some(cur) = queue.pop_front() {
        for child in direct_child_pids(cur) {
            if seen.insert(child) {
                result.push(child);
                queue.push_back(child);
            }
        }
    }
    result
}

/// java.lang.Process.descendants() -> Stream<ProcessHandle> (JDK 9+).
///
/// Was entirely unregistered, so keycloak-test-framework's
/// ProcessUtils.getKeycloakPid() (which calls
/// keycloakProcess.descendants().toList() to tell apart the kc.sh wrapper
/// script's pid from the exec'd java process's pid) threw NoSuchMethodError
/// before a single test could start its managed Keycloak server — see
/// fixed-suite-bugs/pom-xml-declaration-char-corruption-breaks-quarkus-maven-bootstrap-FIXED.md
/// (this was the next missing-native gap surfaced once that doc's actual
/// bug, and the ProcessBuilder LinkedList-command bug above, were fixed).
///
/// Builds a real ArrayList<ProcessHandle> and returns list.stream(),
/// mirroring native_jarfile_stream's ArrayList-then-.stream() pattern
/// (zip_real_jar.rs) rather than hand-rolling a Stream implementation.
fn native_process_descendants(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some("Process.descendants: null this".to_string()),
            }
            .into())
        }
    };
    let pid = match ctx.get_field(this, PROC_FIELD_PID) {
        Value::Long(p) => p,
        _ => -1,
    };
    let descendant_pids = collect_descendant_pids(pid);

    let al_class = "java/util/ArrayList";
    let al_cid = ctx.ensure_class_initialized(al_class).map_err(|_| {
        MethodCallFailed::InternalError(VmError::Internal {
            message: format!("{al_class}: not loaded"),
        })
    })?;
    let list = ctx.alloc_object(al_cid, ctx.class_num_total_fields(al_cid).max(4));
    ctx.invoke(al_class, "<init>", "()V", &[Value::Object(Some(list))])?;
    for cpid in descendant_pids {
        let handle = build_process_handle(ctx, cpid);
        ctx.invoke(
            al_class,
            "add",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(list)), Value::Object(Some(handle))],
        )?;
    }
    ctx.invoke(
        al_class,
        "stream",
        "()Ljava/util/stream/Stream;",
        &[Value::Object(Some(list))],
    )
}

fn pipe_io_err(err: impl std::fmt::Display) -> MethodCallFailed {
    RuntimeError::IOException {
        message: err.to_string(),
    }
    .into()
}

fn pipe_array_bounds(off: i32, len: i32, arr_len: usize) -> Result<(), MethodCallFailed> {
    if off < 0 || len < 0 {
        return Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::aioobe_index_only(if off < 0 { off } else { len }),
        )));
    }
    match (off as usize).checked_add(len as usize) {
        Some(end) if end <= arr_len => Ok(()),
        _ => Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::aioobe_index_only(off.saturating_add(len)),
        ))),
    }
}

fn alloc_pipe_stream(ctx: &mut dyn NativeContext, class_name: &str, fd_id: i32) -> Value {
    let class_id = ctx.ensure_synthetic_class(class_name, 1);
    let stream = ctx.alloc_object(class_id, 1);
    ctx.set_field(stream, 0, Value::Int(fd_id));
    Value::Object(Some(stream))
}

fn pipe_fd(ctx: &dyn NativeContext, this: ObjectRef) -> Option<FdId> {
    match ctx.get_field(this, 0) {
        Value::Int(v) if v >= 0 => Some(v as FdId),
        _ => None,
    }
}

fn native_pipe_output_write_byte(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let b = match args.get(1) {
        Some(Value::Int(v)) => *v as u8,
        _ => 0,
    };
    if let Some(fd) = pipe_fd(ctx, this) {
        ctx.fd_table().write_byte(fd, b).map_err(pipe_io_err)?;
    }
    Ok(None)
}

fn native_pipe_output_write_bytes(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let arr = match args.get(1) {
        Some(Value::Object(Some(a))) => *a,
        _ => return Ok(None),
    };
    let off = args.get(2).and_then(Value::as_int).unwrap_or(0);
    let len = args.get(3).and_then(Value::as_int).unwrap_or(0);
    pipe_array_bounds(off, len, ctx.array_length(arr))?;
    let off = off as usize;
    let len = len as usize;
    if let Some(fd) = pipe_fd(ctx, this) {
        let mut buf = vec![0u8; len];
        ctx.read_byte_array_into(arr, off, &mut buf);
        ctx.fd_table().write_bytes(fd, &buf).map_err(pipe_io_err)?;
    }
    Ok(None)
}

fn native_pipe_output_write_array(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let arr = match args.get(1) {
        Some(Value::Object(Some(a))) => *a,
        _ => return Ok(None),
    };
    let len = ctx.array_length(arr) as i32;
    native_pipe_output_write_bytes(
        ctx,
        &[
            Value::Object(Some(this)),
            Value::Object(Some(arr)),
            Value::Int(0),
            Value::Int(len),
        ],
    )
}

fn native_pipe_output_flush(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    if let Some(fd) = pipe_fd(ctx, this) {
        ctx.fd_table().flush(fd).map_err(pipe_io_err)?;
    }
    Ok(None)
}

fn native_pipe_output_close(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    if let Some(fd) = pipe_fd(ctx, this) {
        let _ = ctx.fd_table().flush(fd);
        let _ = ctx.fd_table().close(fd);
        ctx.set_field(this, 0, Value::Int(-1));
    }
    Ok(None)
}

fn native_pipe_input_read(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let Some(fd) = pipe_fd(ctx, this) else {
        return Ok(Some(Value::Int(-1)));
    };
    ctx.begin_blocking_region();
    let result = ctx.fd_table().read_byte(fd);
    ctx.end_blocking_region();
    let result = result.map_err(pipe_io_err)?;
    Ok(Some(Value::Int(result)))
}

fn native_pipe_input_read_bytes(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let arr = match args.get(1) {
        Some(Value::Object(Some(a))) => *a,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let off = args.get(2).and_then(Value::as_int).unwrap_or(0);
    let len = args.get(3).and_then(Value::as_int).unwrap_or(0);
    pipe_array_bounds(off, len, ctx.array_length(arr))?;
    if len == 0 {
        return Ok(Some(Value::Int(0)));
    }
    let Some(fd) = pipe_fd(ctx, this) else {
        return Ok(Some(Value::Int(-1)));
    };
    let mut buf = vec![0u8; len as usize];
    let mut held = args.to_vec();
    ctx.begin_blocking_region();
    let n = ctx.fd_table().read_bytes(fd, &mut buf);
    ctx.end_blocking_region_refs(&mut held);
    let n = n.map_err(pipe_io_err)?;
    if n == 0 {
        return Ok(Some(Value::Int(-1)));
    }
    let arr = match held.get(1) {
        Some(Value::Object(Some(a))) => *a,
        _ => return Ok(Some(Value::Int(-1))),
    };
    ctx.write_byte_array_from(arr, off as usize, &buf[..n]);
    Ok(Some(Value::Int(n as i32)))
}

fn native_pipe_input_read_array(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let arr = match args.get(1) {
        Some(Value::Object(Some(a))) => *a,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let len = ctx.array_length(arr) as i32;
    native_pipe_input_read_bytes(
        ctx,
        &[
            Value::Object(Some(this)),
            Value::Object(Some(arr)),
            Value::Int(0),
            Value::Int(len),
        ],
    )
}

fn native_pipe_input_available(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let n = pipe_fd(ctx, this)
        .and_then(|fd| ctx.fd_table().available(fd).ok())
        .unwrap_or(0);
    Ok(Some(Value::Int(n as i32)))
}

fn native_pipe_input_close(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    if let Some(fd) = pipe_fd(ctx, this) {
        let _ = ctx.fd_table().close(fd);
        ctx.set_field(this, 0, Value::Int(-1));
    }
    Ok(None)
}

/// `java.lang.Process.pid()J`
fn native_process_pid(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Long(-1))),
    };
    if !is_vm_process(ctx, this) {
        // `Process.pid()` is `return toHandle().pid();`, so the
        // `UnsupportedOperationException` from the default `toHandle()`
        // propagates — which is the specified answer for a `Process` with no
        // pid support, and is what distinguishes it from "the pid is 0".
        let handle = ctx.invoke_virtual(this, "toHandle", "()Ljava/lang/ProcessHandle;", &[])?;
        let Some(Value::Object(Some(handle))) = handle else {
            return Err(RuntimeError::UnsupportedOperationException {
                message: "Process.pid()".to_string(),
            }
            .into());
        };
        return Ok(ctx.invoke_virtual(handle, "pid", "()J", &[])?);
    }
    let handle = handle_of(ctx, this);
    if handle == 0 {
        // Stub Process — fall back to whatever's in field 4.
        return Ok(Some(ctx.get_field(this, PROC_FIELD_PID)));
    }
    Ok(Some(Value::Long(pid_for_handle(handle))))
}

/// Wrap an `FdTable` id in a real JDK stream object (`stream_class` must
/// declare a public `(Ljava/io/FileDescriptor;)V` constructor, i.e.
/// `java/io/FileInputStream` or `java/io/FileOutputStream`).
///
/// The descriptor carries the id in both its `fd` (int) and `handle` (long)
/// fields — the same dual-write contract as `fis_set_fd`/`fos_get_fd` in
/// `lib.rs`, so every existing read/write/available/close native resolves it.
/// An absent pipe (`fd_id == -1`) yields a descriptor neither lookup accepts,
/// which the stream natives surface as EOF / dropped writes — matching the
/// "inherited or closed" semantics the spawn path encodes as -1.
fn wrap_fd_in_stream(
    ctx: &mut dyn NativeContext,
    fd_id: i32,
    stream_class: &str,
) -> MethodCallResult {
    let fd_obj = match ctx.new_object_initialized("java/io/FileDescriptor", "()V", &[])? {
        Some(Value::Object(Some(o))) => o,
        _ => {
            return Err(RuntimeError::IOException {
                message: "Process stream: FileDescriptor construction failed".to_string(),
            }
            .into())
        }
    };
    ctx.set_field_by_name(fd_obj, "fd", Value::Int(fd_id));
    ctx.set_field_by_name(fd_obj, "handle", Value::Long(fd_id as i64));
    let stream = match ctx.new_object_initialized(
        stream_class,
        "(Ljava/io/FileDescriptor;)V",
        &[Value::Object(Some(fd_obj))],
    )? {
        Some(Value::Object(Some(o))) => o,
        _ => {
            return Err(RuntimeError::IOException {
                message: format!("Process stream: {stream_class} construction failed"),
            }
            .into())
        }
    };
    // Some real-JDK stream constructors touch the descriptor during
    // initialization. Re-seed the descriptor after construction so the
    // fd-table id remains recoverable when the stream is later written/read.
    ctx.set_field_by_name(fd_obj, "fd", Value::Int(fd_id));
    ctx.set_field_by_name(fd_obj, "handle", Value::Long(fd_id as i64));

    // Some early WildFly process-controller paths reach this wrapper before
    // the real stream constructor has reliably populated FileInputStream.fd /
    // FileOutputStream.fd. Seed it explicitly so the fd-based stream natives
    // recover the child pipe instead of falling through to legacy slot probes.
    if let Some(fd_slot) = ctx.resolve_field_index(stream_class, "fd") {
        ctx.set_field(stream, fd_slot, Value::Object(Some(fd_obj)));
    }
    ctx.set_field_by_name(stream, "fd", Value::Object(Some(fd_obj)));
    if !matches!(ctx.get_field_by_name(stream, "fd"), Value::Object(Some(_))) {
        ctx.set_field(stream, 0, Value::Int(fd_id));
    }
    Ok(Some(Value::Object(Some(stream))))
}

/// Shared body of the three `java.lang.Process` stream getters: read the
/// pipe's fd id from the synthetic field and wrap it in a real stream.
///
/// A non-synthetic receiver (field holds a reference or nothing) degrades to
/// fd -1 — the same foreign-receiver tolerance `handle_of` gives `waitFor`.
fn process_stream(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    fd_field: usize,
    stream_class: &str,
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some("Process stream getter: null this".to_string()),
            }
            .into())
        }
    };
    let mut fd_id = match ctx.get_field(this, fd_field) {
        Value::Int(v) => v,
        _ => -1,
    };
    if fd_id < 0 {
        let handle = handle_of(ctx, this);
        if handle != 0 {
            if let Some(pipes) = pipe_cache().lock().get(&handle).copied() {
                fd_id = match fd_field {
                    PROC_FIELD_STDIN_FD => pipes.stdin_fd,
                    PROC_FIELD_STDOUT_FD => pipes.stdout_fd,
                    PROC_FIELD_STDERR_FD => pipes.stderr_fd,
                    _ => -1,
                };
            }
        }
    }
    Ok(Some(alloc_pipe_stream(ctx, stream_class, fd_id)))
}

// `captured_string_stream` / `legacy_captured_stream` lived here.
//
// They served ONE producer: a `ProcessBuilder.start` in `native-builtins` that
// ran the child to completion and stored its whole stdout and stderr as two
// Java Strings on the Process, which `getInputStream`/`getErrorStream` then
// re-wrapped in a `ByteArrayInputStream`. That producer is gone -- every spawn
// route now returns a Process carrying live pipe fds -- and the probe was left
// reading slot 7 of every Process it was handed, hoping to find a String. In
// the surviving layout that slot is `PROC_FIELD_STDIN_FD`, an `Int`, so it
// could only ever fire on a foreign receiver that happened to hold a String
// there. Deleted rather than kept as a fallback: there is nothing left for it
// to fall back to.

/// `java.lang.Process.getInputStream()Ljava/io/InputStream;` — the child's
/// stdout pipe.
fn native_process_get_input_stream(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    process_stream(
        ctx,
        args,
        PROC_FIELD_STDOUT_FD,
        SYNTHETIC_PROCESS_INPUT_STREAM,
    )
}

/// `java.lang.Process.getErrorStream()Ljava/io/InputStream;` — the child's
/// stderr pipe.
fn native_process_get_error_stream(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    process_stream(
        ctx,
        args,
        PROC_FIELD_STDERR_FD,
        SYNTHETIC_PROCESS_INPUT_STREAM,
    )
}

/// `java.lang.Process.getOutputStream()Ljava/io/OutputStream;` — the child's
/// stdin pipe.
fn native_process_get_output_stream(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    process_stream(
        ctx,
        args,
        PROC_FIELD_STDIN_FD,
        SYNTHETIC_PROCESS_OUTPUT_STREAM,
    )
}

// ---------------------------------------------------------------------------
// ProcessHandleImpl OS introspection
//
// Linux exposes everything these three natives need through `/proc`. Other
// hosts have no portable equivalent, so they answer the JDK's documented
// "unknown" values (-1 / no entries / untouched fields) rather than
// fabricating data.
// ---------------------------------------------------------------------------

/// Parent pid of `pid`, or `-1` when unknown. `/proc/<pid>/status` is used
/// rather than `/proc/<pid>/stat` because the latter embeds the (unescaped,
/// possibly space- and paren-containing) executable name in field 2.
#[cfg(target_os = "linux")]
fn os_parent_pid(pid: i64) -> i64 {
    if pid <= 0 {
        return -1;
    }
    let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status")) else {
        return -1;
    };
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("PPid:") {
            if let Ok(v) = rest.trim().parse::<i64>() {
                return if v > 0 { v } else { -1 };
            }
        }
    }
    -1
}

#[cfg(not(target_os = "linux"))]
fn os_parent_pid(_pid: i64) -> i64 {
    -1
}

/// `(pid, ppid)` for every visible process when `of_pid == 0`, or for the
/// direct children of `of_pid` otherwise — the two modes the JDK's
/// `getProcessPids0` contract defines.
#[cfg(target_os = "linux")]
fn os_list_processes(of_pid: i64) -> Vec<(i64, i64)> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return out;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Ok(pid) = name.parse::<i64>() else {
            continue;
        };
        let ppid = os_parent_pid(pid);
        if of_pid == 0 || ppid == of_pid {
            out.push((pid, ppid.max(0)));
        }
    }
    out
}

#[cfg(not(target_os = "linux"))]
fn os_list_processes(_of_pid: i64) -> Vec<(i64, i64)> {
    Vec::new()
}

/// `(command, arguments)` from `/proc/<pid>/cmdline` (NUL-separated).
#[cfg(target_os = "linux")]
fn os_process_cmdline(pid: i64) -> Option<(String, Vec<String>)> {
    if pid <= 0 {
        return None;
    }
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let mut parts: Vec<String> = raw
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect();
    if parts.is_empty() {
        return None;
    }
    let command = parts.remove(0);
    Some((command, parts))
}

#[cfg(not(target_os = "linux"))]
fn os_process_cmdline(_pid: i64) -> Option<(String, Vec<String>)> {
    None
}

/// `java.lang.ProcessHandleImpl.parent0(long pid, long startTime) -> long`
fn native_proc_handle_parent0(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let pid = match args.first() {
        Some(Value::Long(p)) => *p,
        _ => return Ok(Some(Value::Long(-1))),
    };
    // pid 0 is this VM's own `ProcessHandle.current()` sentinel in some call
    // paths; resolve it to the real process id first.
    let pid = if pid == 0 {
        std::process::id() as i64
    } else {
        pid
    };
    Ok(Some(Value::Long(os_parent_pid(pid))))
}

/// `java.lang.ProcessHandleImpl.getProcessPids0(long, long[], long[], long[]) -> int`
///
/// Returns the number of processes found. The JDK caller grows its arrays and
/// retries whenever the count exceeds their length, so filling only as far as
/// each array reaches (and still reporting the true total) is the contract.
fn native_proc_handle_get_process_pids0(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let of_pid = match args.first() {
        Some(Value::Long(p)) => *p,
        _ => return Ok(Some(Value::Int(0))),
    };
    let found = os_list_processes(of_pid);
    let arr_of = |idx: usize| -> Option<ObjectRef> {
        match args.get(idx) {
            Some(Value::Object(Some(a))) => Some(*a),
            _ => None,
        }
    };
    let pids = arr_of(1);
    let ppids = arr_of(2);
    let starts = arr_of(3);
    // `set_array_element` cannot allocate, so none of these refs can move
    // underneath the loop.
    for (i, (pid, ppid)) in found.iter().enumerate() {
        if let Some(a) = pids {
            if i < ctx.array_length(a) {
                ctx.set_array_element(a, i, Value::Long(*pid));
            }
        }
        if let Some(a) = ppids {
            if i < ctx.array_length(a) {
                ctx.set_array_element(a, i, Value::Long(*ppid));
            }
        }
        if let Some(a) = starts {
            if i < ctx.array_length(a) {
                // 0 = "start time unknown", the JDK's own sentinel.
                ctx.set_array_element(a, i, Value::Long(0));
            }
        }
    }
    Ok(Some(Value::Int(found.len() as i32)))
}

/// `java.lang.ProcessHandleImpl$Info.info0(long pid)V` — an INSTANCE method,
/// so `args[0]` is the `Info` receiver whose fields are filled in and
/// `args[1]` is the pid.
fn native_proc_handle_info0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let pid = match args.get(1) {
        Some(Value::Long(p)) => *p,
        _ => return Ok(None),
    };
    let pid = if pid == 0 {
        std::process::id() as i64
    } else {
        pid
    };
    // Nothing to report on a host without `/proc`: leave the fields at their
    // constructor defaults, which `Info` renders as `Optional.empty()`.
    let Some((command, arguments)) = os_process_cmdline(pid) else {
        return Ok(None);
    };
    let command_line = if arguments.is_empty() {
        command.clone()
    } else {
        format!("{command} {}", arguments.join(" "))
    };
    // Pin `this` across every allocation below — each `create_string` /
    // `new_array` can trigger a moving young GC that would relocate it.
    let this_pin = ctx.pin_native_root(this);
    let cmd_str = ctx.create_string(&command);
    let this_cur = ctx.read_native_pin(this_pin, this);
    ctx.set_field_by_name(this_cur, "command", Value::Object(Some(cmd_str)));
    let line_str = ctx.create_string(&command_line);
    let this_cur = ctx.read_native_pin(this_pin, this);
    ctx.set_field_by_name(this_cur, "commandLine", Value::Object(Some(line_str)));
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, arguments.len());
    let arr_pin = ctx.pin_native_root(arr);
    for (i, a) in arguments.iter().enumerate() {
        let s = ctx.create_string(a);
        let arr_cur = ctx.read_native_pin(arr_pin, arr);
        ctx.set_array_element(arr_cur, i, Value::Object(Some(s)));
    }
    let this_cur = ctx.read_native_pin(this_pin, this);
    let arr_cur = ctx.read_native_pin(arr_pin, arr);
    ctx.set_field_by_name(this_cur, "arguments", Value::Object(Some(arr_cur)));
    ctx.unpin_native_roots(this_pin);
    Ok(None)
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

// JDK-ONLY-CLASSIFY: bridge — for 9 of the 51 registrations, and the census
// (schema 3, JDK 25, 2026-08-05) says exactly which. Process control is one of
// the categories jdk-only-native-review.md §5 names explicitly, and the nine
// `java.lang.ProcessHandleImpl` / `ProcessHandleImpl$Info` entries below are
// ACC_NATIVE on the image: `initNative`, `getCurrentPid0`, `isAlive0`,
// `waitForProcessExit0`, `destroy0`, `parent0`, `getProcessPids0`,
// `Info.initIDs`, `Info.info0`. A subprocess cannot be created or reaped from
// bytecode and the live `std::process::Child` lives in this crate's table, so
// those must survive `--jdk-only`; they state their kind at their own call
// sites (L5, 2026-08-05).
//
// The other 42 do NOT resolve to an ACC_NATIVE method and stay on the ambient
// category, because they are three quite different things:
//
//   * 25 registrations on `cratonvm/synthetic/Process*` classes — a class the
//     image does not contain and the VM mints. `Bridge` is what keeps them
//     alive under `--jdk-only`, which is the same unresolved shape as the
//     `Function$Identity` successor defect in the record below, not a bridge.
//   * 13 on `java.lang.Process` itself, which declares them abstract (6) or
//     with concrete bytecode (7) — shadows, adjudicated by contract §1.4, not
//     §1.5.
//   * `ProcessImpl.create` (the Windows spawn entry point; the Linux image
//     declares `forkAndExec` instead), `UNIXProcess.forkAndExec` (class gone
//     since JDK 9), `ProcessHandleImpl.destroyProcess0` (superseded by
//     `destroy0(JJZ)Z`) and `ProcessBuilder.start` — dead or shadowing.
//
// Details and the per-row table:
// docs/known-issues/jdk-only/l5-native-io-bridge-residuals.md
/// Register every WP1.12-owned subprocess native.  Called from
/// `register_io_natives` at VM boot.
pub fn register_process_natives(registry: &mut NativeMethodRegistry) {
    use cratonvm_native_api::NativeKind;
    let __prev_cat = registry.current_category();
    registry.set_category(NativeKind::Bridge);
    // Platform-specific spawn natives that the JDK invokes from inside
    // ProcessImpl / UNIXProcess.  Only the Windows one is registered on
    // non-Linux targets, and vice-versa, so we don't override each
    // other's natives on the wrong platform.
    registry.register(
        "java/lang/ProcessImpl",
        "create",
        "(Ljava/lang/String;[Ljava/lang/String;Ljava/lang/String;[JZ)J",
        native_process_impl_create,
    );

    // `java.lang.UNIXProcess` was the pre-JDK-9 name of this class and is not
    // on any supported image, so the registration that used to sit here could
    // never resolve. `ProcessImpl.forkAndExec` is the live one, and it is a
    // genuine §1.5 bridge: `ACC_NATIVE` on the image, and a subprocess cannot
    // be created from bytecode.
    #[cfg(target_os = "linux")]
    registry.register_with_kind(
        "java/lang/ProcessImpl",
        "forkAndExec",
        "(I[B[B[BI[BI[B[IZ)I",
        native_process_impl_fork_and_exec,
        NativeKind::Bridge,
    );

    // ProcessHandleImpl family — ProcessHandle.current() / Process.pid()
    // ultimately reach these.
    // OpenJDK <clinit> calls initNative(); without it, UnsatisfiedLinkError leaves
    // internal stubs null and Spring Boot / logging fails on ProcessHandle.current().
    //
    // KEEP: `initNative()` exists purely to cache JNI jfieldIDs and start
    // HotSpot's process-reaper thread. CratonVM resolves fields by name and
    // reaps through `PROCESS_TABLE`, so there is genuinely nothing to do —
    // the same reasoning as the `initIDs` no-ops elsewhere in this crate.
    registry.register_with_kind(
        "java/lang/ProcessHandleImpl",
        "initNative",
        "()V",
        |_ctx, _args| Ok(None),
        NativeKind::Bridge,
    );
    registry.register_with_kind(
        "java/lang/ProcessHandleImpl",
        "getCurrentPid0",
        "()J",
        native_proc_handle_current_pid0,
        NativeKind::Bridge,
    );
    registry.register_with_kind(
        "java/lang/ProcessHandleImpl",
        "isAlive0",
        "(J)J",
        native_proc_handle_is_alive0,
        NativeKind::Bridge,
    );
    registry.register_with_kind(
        "java/lang/ProcessHandleImpl",
        "waitForProcessExit0",
        "(JZ)I",
        native_proc_handle_wait_for_process_exit0,
        NativeKind::Bridge,
    );
    registry.register(
        "java/lang/ProcessHandleImpl",
        "destroyProcess0",
        "(JZ)Z",
        native_proc_handle_destroy_process0,
    );
    // Real JDK 25 signature: destroy0(pid, startTime, forcibly) -> boolean.
    registry.register_with_kind(
        "java/lang/ProcessHandleImpl",
        "destroy0",
        "(JJZ)Z",
        |_ctx, args| {
            // args = (pid, startTime, force). Keyed by pid -- see
            // `handle_for_pid`; `startTime` is the JDK's staleness check
            // against a recycled pid, which we cannot answer (we record no
            // start time) and do not need to: the process table maps a pid to
            // OUR child, and a recycled pid is not in it.
            let pid = match args.first() {
                Some(Value::Long(p)) => *p,
                _ => return Ok(Some(Value::Int(0))),
            };
            let force = matches!(args.get(2), Some(Value::Int(1)));
            let ok = match handle_for_pid(pid) {
                Some(handle) => destroy_handle(handle, force),
                None => false,
            };
            Ok(Some(Value::Int(if ok { 1 } else { 0 })))
        },
        NativeKind::Bridge,
    );
    // parent0(pid, startTime) -> long. Was a hardcoded -1 ("unknown") for
    // every process, so `ProcessHandle.parent()` was permanently empty — a
    // caller walking up the process tree (or checking whether it was launched
    // by a known supervisor) silently got nothing. The `long` here really IS
    // an OS pid: `Process.pid()` resolves the table handle through
    // `pid_for_handle` before the JDK bytecode reaches this native.
    registry.register_with_kind(
        "java/lang/ProcessHandleImpl",
        "parent0",
        "(JJ)J",
        native_proc_handle_parent0,
        NativeKind::Bridge,
    );
    // getProcessPids0(pid, pids[], ppids[], starttimes[]) -> int (count).
    // Was a hardcoded 0, i.e. "this process has no children and the machine is
    // running no processes" — indistinguishable from a real empty answer, so
    // `ProcessHandle.children()`/`allProcesses()` quietly returned nothing.
    registry.register_with_kind(
        "java/lang/ProcessHandleImpl",
        "getProcessPids0",
        "(J[J[J[J)I",
        native_proc_handle_get_process_pids0,
        NativeKind::Bridge,
    );

    // KEEP: `ProcessHandleImpl$Info.initIDs()` is a JNI jfieldID cache init;
    // CratonVM resolves fields by name, so an empty body is the spec-correct
    // implementation (same as every other `initIDs` in this crate).
    registry.register_with_kind(
        "java/lang/ProcessHandleImpl$Info",
        "initIDs",
        "()V",
        |_ctx, _args| Ok(None),
        NativeKind::Bridge,
    );
    // info0(pid) fills command/commandLine/arguments/startTime/totalTime/user
    // on the receiver. It used to leave every field at its constructor default,
    // so `ProcessHandle.info()` reported an entirely empty record for a process
    // that plainly has a command line. Populate what the OS will tell us.
    registry.register_with_kind(
        "java/lang/ProcessHandleImpl$Info",
        "info0",
        "(J)V",
        native_proc_handle_info0,
        NativeKind::Bridge,
    );

    // Process methods on our synthetic Process — override the stubs
    // from phases_late::register_phase57_process with real fd-aware
    // implementations.
    //
    // Registered under BOTH `java/lang/Process` (call-site keyed lookups,
    // and parity with the phase57 stubs) and SYNTHETIC_PROCESS_CLASS:
    // receiver-driven dispatch probes the registry by the receiver's
    // class-chain names, and the synthetic stub's chain never reaches
    // `java/lang/Process` (superclass: None), so without the second
    // registration every one of these raised NoSuchMethodError on the
    // objects `spawn_and_wrap` actually creates.
    for proc_cls in ["java/lang/Process", SYNTHETIC_PROCESS_CLASS] {
        // `cratonvm/synthetic/Process` is a class no image contains and this VM
        // mints, so by the contract's own definition every registration on it
        // is a `SyntheticStub`, not a `Bridge` — §1.5 defines a bridge as what
        // an `ACC_NATIVE` method on the image binds to, and there is no image
        // method here to bind to. `Bridge` was what kept them alive under
        // `--jdk-only`, which is the outcome §5 forbids: strict mode must not
        // reach a fabricated class at all. Restated as stubs, strict mode drops
        // them at registration and `java.lang.ProcessImpl` answers instead.
        //
        // The `java/lang/Process` half of this loop keeps the ambient `Bridge`:
        // those 13 rows are a different question (§1.4 shadows of real
        // bytecode, or abstract declarations every real subclass overrides) and
        // are adjudicated in their own record.
        let __loop_cat = registry.current_category();
        if proc_cls == SYNTHETIC_PROCESS_CLASS {
            registry.set_category(NativeKind::SyntheticStub);
        }
        registry.register(proc_cls, "waitFor", "()I", native_process_wait_for);
        registry.register(
            proc_cls,
            "waitFor",
            "(JLjava/util/concurrent/TimeUnit;)Z",
            native_process_wait_for_timeout,
        );
        registry.register(proc_cls, "exitValue", "()I", native_process_exit_value);
        registry.register(proc_cls, "isAlive", "()Z", native_process_is_alive);
        registry.register(proc_cls, "destroy", "()V", native_process_destroy);
        registry.register(
            proc_cls,
            "destroyForcibly",
            "()Ljava/lang/Process;",
            native_process_destroy_forcibly,
        );
        registry.register(proc_cls, "pid", "()J", native_process_pid);
        registry.register(
            proc_cls,
            "toHandle",
            "()Ljava/lang/ProcessHandle;",
            native_process_to_handle,
        );

        // Stream getters. The spawn path stores the child's pipe fd ids in
        // fields 1-3 precisely so streams can be served through the
        // fd_table; until these were registered the synthetic Process had
        // NO getInputStream/getErrorStream/getOutputStream anywhere (the
        // phase57 stubs are synthetic-jdk-only), so the first caller —
        // picocli's terminal-width probe reading `mode con` output during
        // junit-console --help — hit NoSuchMethodError.
        registry.register(
            proc_cls,
            "getInputStream",
            "()Ljava/io/InputStream;",
            native_process_get_input_stream,
        );
        registry.register(
            proc_cls,
            "getErrorStream",
            "()Ljava/io/InputStream;",
            native_process_get_error_stream,
        );
        registry.register(
            proc_cls,
            "getOutputStream",
            "()Ljava/io/OutputStream;",
            native_process_get_output_stream,
        );
        registry.register(
            proc_cls,
            "descendants",
            "()Ljava/util/stream/Stream;",
            native_process_descendants,
        );
        // onExit() was the one Process virtual left out of this loop, so it
        // raised NoSuchMethodError on every process the shadowed
        // `ProcessBuilder.start()` returns. See `native_process_on_exit`.
        registry.register(
            proc_cls,
            "onExit",
            "()Ljava/util/concurrent/CompletableFuture;",
            native_process_on_exit,
        );
        registry.set_category(__loop_cat);
    }

    // The remaining fabricated-receiver rows, for the same reason as the loop
    // above: `ProcessExitWaiter`, `ProcessPipeInputStream` and
    // `ProcessPipeOutputStream` are all classes this VM mints.
    let __synthetic_cat = registry.current_category();
    registry.set_category(NativeKind::SyntheticStub);
    // Runnable body of the reaper thread `onExit()` starts for a live child.
    registry.register(
        SYNTHETIC_PROCESS_EXIT_WAITER,
        "run",
        "()V",
        native_process_exit_waiter_run,
    );

    registry.register(
        SYNTHETIC_PROCESS_OUTPUT_STREAM,
        "write",
        "(I)V",
        native_pipe_output_write_byte,
    );
    registry.register(
        SYNTHETIC_PROCESS_OUTPUT_STREAM,
        "write",
        "([BII)V",
        native_pipe_output_write_bytes,
    );
    registry.register(
        SYNTHETIC_PROCESS_OUTPUT_STREAM,
        "write",
        "([B)V",
        native_pipe_output_write_array,
    );
    registry.register(
        SYNTHETIC_PROCESS_OUTPUT_STREAM,
        "flush",
        "()V",
        native_pipe_output_flush,
    );
    registry.register(
        SYNTHETIC_PROCESS_OUTPUT_STREAM,
        "close",
        "()V",
        native_pipe_output_close,
    );

    registry.register(
        SYNTHETIC_PROCESS_INPUT_STREAM,
        "read",
        "()I",
        native_pipe_input_read,
    );
    registry.register(
        SYNTHETIC_PROCESS_INPUT_STREAM,
        "read",
        "([BII)I",
        native_pipe_input_read_bytes,
    );
    registry.register(
        SYNTHETIC_PROCESS_INPUT_STREAM,
        "read",
        "([B)I",
        native_pipe_input_read_array,
    );
    registry.register(
        SYNTHETIC_PROCESS_INPUT_STREAM,
        "available",
        "()I",
        native_pipe_input_available,
    );
    registry.register(
        SYNTHETIC_PROCESS_INPUT_STREAM,
        "close",
        "()V",
        native_pipe_input_close,
    );
    // readAllBytes() is registered generically for java/io/InputStream
    // (native-io/src/lib.rs), but the synthetic Process pipe stream's class
    // chain never reaches java/io/InputStream (same receiver-driven-dispatch
    // gap the SYNTHETIC_PROCESS_CLASS dual-registration above works around
    // for java/lang/Process) -- register it directly here too. Surfaced by
    // keycloak-test-framework's DistributionKeycloakServer.getErrorOutput(),
    // which calls keycloakProcess.getErrorStream().readAllBytes().
    registry.register(
        SYNTHETIC_PROCESS_INPUT_STREAM,
        "readAllBytes",
        "()[B",
        crate::native_is_read_all_bytes,
    );
    registry.set_category(__synthetic_cat);

    // ProcessBuilder.start — route through the real spawn path. This is the
    // registration that wins the slot (the census names this line), so it is
    // the one that decides what `start()` returns in every mode.
    //
    // `SyntheticStub`, stated, and the tag is the whole point. `start()` is
    // ordinary bytecode on the image — `acc_native: false, has_code: true` in
    // the adjudication — so by §1.4 the real method outranks any bridge, and
    // calling this a bridge is what let a fabricated `cratonvm/synthetic/Process`
    // escape into `--jdk-only`. Restated, strict mode drops it, the JDK's own
    // `start()` runs, and it builds a real `java.lang.ProcessImpl` through
    // `ProcessImpl.forkAndExec` (this crate, above).
    //
    // Default `--real-jdk` (compatible) mode is deliberately unchanged: the
    // registration survives there and still shadows `start()`. Compatible mode
    // keeps the VM's own process object, strict mode gets the JDK's — which is
    // exactly the difference the two modes are for.
    registry.register_with_kind(
        "java/lang/ProcessBuilder",
        "start",
        "()Ljava/lang/Process;",
        native_process_builder_start,
        NativeKind::SyntheticStub,
    );
    registry.set_category(__prev_cat);
}

/// `java.lang.ProcessBuilder.start()Ljava/lang/Process;`
///
/// Reads the command + directory + env map fields that the ProcessBuilder
/// synthetic lays out in phases_late, then spawns the child via
/// `spawn_and_wrap`.
///
/// `pub` because it is THE implementation: `native-builtins` registers this
/// same function pointer from its own two `ProcessBuilder.start` sites rather
/// than keeping reimplementations behind it. Both of those allocated the
/// returned Process under the name `java/lang/Process`, which in real-JDK mode
/// is a real six-field class -- so the extra slots they asked for did not
/// exist and every write to them was silently dropped. They were dead (this
/// registration runs later and wins), but "dead" was the only thing keeping
/// them harmless, and a registration-order change would have reintroduced the
/// empty-CGI-body bug the same shape caused on the `Runtime.exec` route. See
/// docs/internal/runtime-exec-returned-a-process-with-no-streams-FIXED-20260806.md.
pub fn native_process_builder_start(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if pb_debug_enabled() {
        eprintln!("[PB-START-IO] ProcessBuilder.start via native-io");
    }
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some("ProcessBuilder.start: null this".to_string()),
            }
            .into())
        }
    };

    // --- Field 0: command (List<String> or String[]) ---
    let cmd_val = ctx.get_field(this, 0);
    let mut cmd_strings: Vec<String> = Vec::new();

    if let Value::Object(Some(cmd_obj)) = cmd_val {
        use cratonvm_types::ObjectKind;
        if ctx.heap_kind_of(cmd_obj) == ObjectKind::Array {
            // Genuine String[] — `array_length` is legal only on an array.
            let len = ctx.array_length(cmd_obj);
            for i in 0..len {
                if let Value::Object(Some(s)) = ctx.get_array_element(cmd_obj, i) {
                    cmd_strings.push(ctx.read_string(s).unwrap_or_default());
                }
            }
        } else {
            // A List (typically an ArrayList). Prefer the real-JDK
            // `size`/`elementData` fields read BY NAME — a real ArrayList
            // carries `AbstractList.modCount` ahead of `elementData`/`size`,
            // so the old hard-coded `size = slot1` guess was wrong and fell
            // through to `array_length(list)`, which is illegal on a non-array
            // and tripped `[ARRAY-LEN-GUARD]` during picocli's
            // `getTerminalWidth()` ProcessBuilder probe (junit-console --help).
            // Fall back to the synthetic ArrayList layout (data=slot0, size=slot1)
            // only when the named fields are absent.
            let size_by_name = match ctx.get_field_by_name(cmd_obj, "size") {
                Value::Int(n) => Some(n),
                _ => None,
            };
            let data_by_name = match ctx.get_field_by_name(cmd_obj, "elementData") {
                Value::Object(Some(a)) => Some(a),
                _ => None,
            };
            let (size, data) = match (size_by_name, data_by_name) {
                (Some(sz), Some(arr)) => (sz, Some(arr)),
                _ => {
                    let sz = ctx.get_field(cmd_obj, 1).as_int().unwrap_or(0);
                    let arr = match ctx.get_field(cmd_obj, 0) {
                        Value::Object(Some(a)) => Some(a),
                        _ => None,
                    };
                    (sz, arr)
                }
            };
            if let Some(data_arr) = data {
                // Only read the backing store as an array once confirmed.
                if ctx.heap_kind_of(data_arr) == ObjectKind::Array {
                    let cap = ctx.array_length(data_arr);
                    let n = (size.max(0) as usize).min(cap);
                    for i in 0..n {
                        if let Value::Object(Some(s)) = ctx.get_array_element(data_arr, i) {
                            cmd_strings.push(ctx.read_string(s).unwrap_or_default());
                        }
                    }
                }
            }
            // Generic fallback via the List public API (size()/get(int)) for
            // any List<String> implementation that isn't ArrayList-shaped --
            // e.g. keycloak-test-framework's DistributionKeycloakServer
            // .startKeycloak() builds its command with new LinkedList<>(),
            // which has neither an elementData field nor a plain backing
            // array at slot 0 (its real layout is first/last Node links),
            // so the ArrayList-shaped read above silently found nothing and
            // this native threw "ProcessBuilder: no command specified" even
            // though the command list was genuinely populated. One virtual
            // dispatch per element, so only used when the fast path above
            // came up empty.
            if cmd_strings.is_empty() {
                if let Ok(Some(Value::Int(n))) = ctx.invoke_virtual(cmd_obj, "size", "()I", &[]) {
                    for i in 0..n {
                        if let Ok(Some(Value::Object(Some(s)))) = ctx.invoke_virtual(
                            cmd_obj,
                            "get",
                            "(I)Ljava/lang/Object;",
                            &[Value::Int(i)],
                        ) {
                            cmd_strings.push(ctx.read_string(s).unwrap_or_default());
                        }
                    }
                }
            }
        }
    }

    if cmd_strings.is_empty() {
        return Err(RuntimeError::IllegalStateException {
            message: "ProcessBuilder: no command specified".to_string(),
        }
        .into());
    }
    if pb_debug_enabled() {
        eprintln!("[PB-CMD] {:?}", cmd_strings);
    }

    // --- Field 1: directory (File) ---
    // B5: read the File's path string BY NAME (`path`) — a real-JDK
    // `java.io.File` does not place its `String path` field at slot 0, so the
    // old fixed `get_field(file_obj, 0)` silently dropped
    // `ProcessBuilder.directory(dir)` (spawning in CWD instead). Fall back to
    // slot 0 only for the synthetic File layout where the field is unnamed.
    let work_dir: Option<String> = match ctx.get_field(this, 1) {
        Value::Object(Some(file_obj)) => match ctx.get_field_by_name(file_obj, "path") {
            Value::Object(Some(s)) => ctx.read_string(s),
            _ => match ctx.get_field(file_obj, 0) {
                Value::Object(Some(s)) => ctx.read_string(s),
                _ => None,
            },
        },
        _ => None,
    };

    // --- Spawn ---
    let env_vars = read_process_environment(ctx, this);
    let redirects = read_process_redirects(ctx, this);
    let program = cmd_strings[0].clone();
    let rest: Vec<String> = cmd_strings.into_iter().skip(1).collect();
    // Honor ProcessBuilder.redirectErrorStream(true) (`2>&1`).
    let redirect_err = matches!(
        ctx.get_field_by_name(this, "redirectErrorStream"),
        Value::Int(v) if v != 0
    );
    spawn_and_wrap_with_redirects(
        ctx,
        &program,
        &rest,
        work_dir.as_deref(),
        env_vars.as_deref(),
        env_vars.is_some(),
        redirect_err,
        &redirects,
    )
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use cratonvm_native_api::{NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess, NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess};
    use super::*;
    use crate::test_support::MockNativeContext;

    /// Serializes the tests that call `spawn_and_wrap`.
    ///
    /// `set_spawn_policy_hook` installs a PROCESS-GLOBAL hook, and
    /// `spawn_policy_hook_is_consulted_and_can_refuse_the_fork` spends part of
    /// its run with that hook set to refuse everything. Any other test
    /// spawning through `spawn_and_wrap` at that moment is refused too, which
    /// showed up as a flaky `SecurityException` from an unrelated test.
    static SPAWN_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn install_child_for_test(child: Child) -> (i64, i64) {
        let handle = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
        let pid = child.id() as i64;
        process_table()
            .lock()
            .insert(handle, Arc::new(Mutex::new(child)));
        exit_cache().lock().insert(
            handle,
            ExitCache {
                pid,
                exit_code: None,
            },
        );
        (handle, pid)
    }

    /// The two ids are different numbers, and a `ProcessHandleImpl` native
    /// handed the pid must still find the child.
    #[test]
    #[cfg(unix)]
    fn a_real_pid_resolves_to_its_table_handle() {
        let child = Command::new("/bin/sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn /bin/sleep");
        let (handle, pid) = install_child_for_test(child);
        assert_ne!(
            handle, pid,
            "handle is a counter and pid is the OS's; a test where they \
             coincide would pass for the wrong reason"
        );

        assert_eq!(handle_for_pid(pid), Some(handle), "by pid");

        // `isAlive0` answers a START TIME, not a boolean: >= 0 is alive and -1
        // is the only way to say otherwise. 0 is the JDK's "alive, start time
        // unknown".
        let alive =
            native_proc_handle_is_alive0(&mut MockNativeContext::new(), &[Value::Long(pid)])
                .unwrap();
        assert_eq!(alive, Some(Value::Long(0)), "alive, start time unknown");

        destroy_handle(handle, true);
        let code = wait_for_handle(handle);
        assert!(code != 0, "killed child should not report success: {code}");

        let dead =
            native_proc_handle_is_alive0(&mut MockNativeContext::new(), &[Value::Long(pid)])
                .unwrap();
        assert_eq!(dead, Some(Value::Long(-1)), "exited, by pid");
    }

    /// `argc`/`envc` are passed for a reason: an empty argument is an
    /// argument.
    ///
    /// The block is `count` NUL-terminated strings laid end to end, so the
    /// split yields a trailing empty piece that is NOT an argument, and an
    /// empty argument in the middle that IS one. Dropping every empty piece —
    /// what the decoder did before — gets both wrong in the same direction.
    #[test]
    fn a_nul_block_keeps_empty_arguments_and_drops_the_trailer() {
        // `sh -c 'printf [%s] "$1" "$2"' sh "" z` — argv[2] is deliberately "".
        let block = b"sh\0\0z\0";
        assert_eq!(
            split_nul_block(block, 3),
            vec!["sh".to_string(), String::new(), "z".to_string()],
        );
        // Asking for fewer than the block holds takes a prefix, never a scan
        // for a terminator that is not there.
        assert_eq!(split_nul_block(block, 1), vec!["sh".to_string()]);
        assert!(split_nul_block(b"", 0).is_empty());
    }

    /// `toCString` output is NUL-terminated; the terminator is not part of the
    /// string, and neither is anything the JDK left in the tail.
    #[test]
    fn a_c_string_stops_at_its_terminator() {
        assert_eq!(decode_c_string(b"/bin/sh\0"), "/bin/sh");
        assert_eq!(decode_c_string(b"/bin/sh\0junk"), "/bin/sh");
        assert_eq!(decode_c_string(b"/bin/sh"), "/bin/sh", "no terminator");
        assert_eq!(decode_c_string(b"\0"), "");
    }

    /// The three `fds[i]` input cases, and why the middle one is unambiguous.
    ///
    /// `Redirect.INHERIT` puts the slot's own index in the slot, and fd-table
    /// ids 0/1/2 are permanently the VM's own standard streams with the
    /// counter starting at 3 — so `fds[1] == 1` can only ever mean "stdout",
    /// whichever way you read it.
    #[test]
    #[cfg(target_os = "linux")]
    fn a_requested_fd_maps_to_pipe_inherit_or_an_existing_descriptor() {
        assert!(matches!(
            redirect_from_requested_fd(0, -1),
            StdioRedirect::Pipe
        ));
        assert!(matches!(
            redirect_from_requested_fd(1, 1),
            StdioRedirect::Inherit
        ));
        assert!(matches!(
            redirect_from_requested_fd(2, 2),
            StdioRedirect::Inherit
        ));
        // A file redirect: the JDK opened it and handed us its table id.
        assert!(matches!(
            redirect_from_requested_fd(1, 7),
            StdioRedirect::ExistingFd(7)
        ));
        // The slot index is compared against ITS OWN slot, so id 0 in the
        // stdout slot is a descriptor, not an inherit.
        assert!(matches!(
            redirect_from_requested_fd(1, 0),
            StdioRedirect::ExistingFd(0)
        ));
    }

    /// `destroy` must still reach a child that a `waitFor` is blocked on.
    ///
    /// The real JDK's `ProcessImpl` puts a reaper thread into
    /// `waitForProcessExit0` for every child it spawns, so this is the normal
    /// state rather than a race. Before the table held the child behind its own
    /// mutex, the waiter took it out and `destroy` silently did nothing.
    #[test]
    #[cfg(unix)]
    fn destroy_reaches_a_child_a_waiter_is_blocked_on() {
        let child = Command::new("/bin/sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn /bin/sleep");
        let (handle, _pid) = install_child_for_test(child);

        let waiter = std::thread::spawn(move || wait_for_handle(handle));
        // Give the waiter time to actually be inside `Child::wait()`. If it is
        // not yet, `destroy_handle` takes the fast path and the test still
        // asserts the thing that matters — that the child dies.
        std::thread::sleep(Duration::from_millis(200));

        assert!(destroy_handle(handle, true), "destroy must report success");
        let code = waiter.join().expect("waiter thread");
        assert_eq!(code, 128 + 9, "SIGKILL is reported as 128+signum");
    }

    /// A pid we never spawned resolves to nothing, and the bridge says so
    /// rather than reaching into the table with it.
    #[test]
    fn an_unspawned_pid_resolves_to_no_handle() {
        let foreign = 0x7fff_0000i64; // far above any pid this host will mint
        assert_eq!(handle_for_pid(foreign), None);
        let mut ctx = MockNativeContext::new();
        let rc =
            native_proc_handle_wait_for_process_exit0(&mut ctx, &[Value::Long(foreign)]).unwrap();
        assert_eq!(rc, Some(Value::Int(-1)), "cannot wait on a non-child");
        assert_eq!(
            native_proc_handle_destroy_process0(&mut ctx, &[Value::Long(foreign), Value::Int(1)])
                .unwrap(),
            Some(Value::Int(0)),
            "and must not signal it"
        );
    }

    /// The VM's own pid must read as alive: `ProcessHandleImpl.<clinit>` seeds
    /// `current` with `isAlive0(getCurrentPid0())`, and -1 there would make the
    /// VM report its own process as nonexistent.
    #[test]
    fn the_current_process_is_alive_by_its_real_pid() {
        let mut ctx = MockNativeContext::new();
        let self_pid = std::process::id() as i64;
        assert_eq!(
            native_proc_handle_is_alive0(&mut ctx, &[Value::Long(self_pid)]).unwrap(),
            Some(Value::Long(0)),
            "alive, start time unknown"
        );
    }

    /// A receiver shaped AND named like one of the VM's own process objects.
    ///
    /// The class name is load-bearing, not decoration: every concrete
    /// `java.lang.Process` native now asks `is_vm_process` before trusting the
    /// `PROC_FIELD_*` slots, so an unnamed mock takes the foreign-receiver path
    /// and a test written for the VM path would quietly measure the other one.
    fn mock_process(ctx: &mut MockNativeContext, handle: i64, pid: i64) -> ObjectRef {
        let proc_ref = ctx.alloc_object_with_class(PROC_FIELD_COUNT, SYNTHETIC_PROCESS_CLASS);
        ctx.set_field(proc_ref, PROC_FIELD_EXIT, Value::Int(EXIT_NOT_YET));
        ctx.set_field(proc_ref, PROC_FIELD_PID, Value::Long(pid));
        ctx.set_field(proc_ref, PROC_FIELD_HANDLE, Value::Long(handle));
        proc_ref
    }

    /// A spawn must hand back a LIVE, READABLE stdout pipe.
    ///
    /// This is the property `Runtime.exec` did not have: it ran the child to
    /// completion with `Command::output()` and stored the bytes as a Java
    /// String on an object nothing downstream could read, so
    /// `Process.getInputStream()` fell through to fd -1 and every read
    /// returned EOF. Tomcat's `CGIServlet` copies exactly that stream into
    /// the HTTP response, which is why `TestSecurity2019.testCVE_2019_0232`
    /// saw `200 OK` with an empty body.
    ///
    /// Asserted through the `FdTable`, the same route the pipe-stream natives
    /// take, rather than through the Java stream objects: the point is that
    /// the fd recorded on the Process is the child's real stdout.
    #[test]
    #[cfg(not(target_os = "windows"))]
    fn spawn_and_wrap_exposes_a_live_stdout_pipe() {
        let _guard = SPAWN_TEST_LOCK.lock();
        let mut ctx = MockNativeContext::new();
        let result = spawn_and_wrap(
            &mut ctx,
            "/bin/echo",
            &["hello-from-the-child".to_string()],
            None,
            None,
            false,
            false,
        )
        .expect("spawn /bin/echo")
        .expect("spawn returns a Process");
        let proc_ref = match result {
            Value::Object(Some(o)) => o,
            other => panic!("expected a Process object, got {other:?}"),
        };

        let handle = match ctx.get_field(proc_ref, PROC_FIELD_HANDLE) {
            Value::Long(h) => h,
            other => panic!("PROC_FIELD_HANDLE must be a Long, got {other:?}"),
        };
        assert_ne!(handle, 0, "a spawned Process must carry a live handle");

        let stdout_fd = match ctx.get_field(proc_ref, PROC_FIELD_STDOUT_FD) {
            Value::Int(fd) => fd,
            other => panic!("PROC_FIELD_STDOUT_FD must be an Int, got {other:?}"),
        };
        assert!(
            stdout_fd >= 0,
            "a piped stdout must have a real fd-table id, got {stdout_fd}"
        );

        let mut buf = [0u8; 64];
        let n = ctx
            .fd_table()
            .read_bytes(stdout_fd as FdId, &mut buf)
            .expect("read the child's stdout");
        let text = String::from_utf8_lossy(&buf[..n]).into_owned();
        assert!(
            text.contains("hello-from-the-child"),
            "the child's stdout must be readable through the recorded fd, got {text:?}"
        );

        assert_eq!(wait_for_handle(handle), 0);
    }

    /// The spawn policy gate runs BEFORE the fork, on every spawn route.
    ///
    /// `SecurityManager.checkExec` used to be applied by `Runtime.exec` and by
    /// `native-builtins`' own (shadowed) `ProcessBuilder.start`, but NOT by the
    /// `ProcessBuilder.start` that actually wins at runtime, so a deny-all
    /// policy refused one entry point and let the other fork the same child.
    /// The gate now lives in `spawn_and_wrap`, which all of them funnel
    /// through.
    ///
    /// Both arms are in one test on purpose: the hook is a process-global
    /// `OnceLock`, so a second test installing a different one would silently
    /// keep the first.
    #[test]
    #[cfg(not(target_os = "windows"))]
    fn spawn_policy_hook_is_consulted_and_can_refuse_the_fork() {
        let _guard = SPAWN_TEST_LOCK.lock();
        use std::sync::atomic::AtomicBool;

        static DENY: AtomicBool = AtomicBool::new(true);
        static SAW: Mutex<Option<String>> = Mutex::new(None);

        fn hook(_ctx: &mut dyn NativeContext, program: &str) -> Result<(), MethodCallFailed> {
            *SAW.lock() = Some(program.to_string());
            if DENY.load(Ordering::Relaxed) {
                Err(RuntimeError::SecurityException {
                    message: format!("test policy denies {program}"),
                }
                .into())
            } else {
                Ok(())
            }
        }
        set_spawn_policy_hook(hook);

        // --- deny arm: the error propagates verbatim and nothing is spawned.
        let mut ctx = MockNativeContext::new();
        let marker = std::env::temp_dir().join(format!(
            "cratonvm-spawn-policy-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&marker);
        let denied = spawn_and_wrap(
            &mut ctx,
            "/usr/bin/touch",
            &[marker.to_string_lossy().into_owned()],
            None,
            None,
            false,
            false,
        );
        match denied {
            Err(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::SecurityException { .. },
            ))) => {}
            other => panic!("a refused spawn must surface the SecurityException, got {other:?}"),
        }
        assert_eq!(
            SAW.lock().as_deref(),
            Some("/usr/bin/touch"),
            "the gate must see command[0]"
        );
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            !marker.exists(),
            "a refused spawn must not fork: {} exists",
            marker.display()
        );

        // --- allow arm: the same call goes through once the gate says yes.
        DENY.store(false, Ordering::Relaxed);
        let allowed = spawn_and_wrap(
            &mut ctx,
            "/bin/echo",
            &["ok".to_string()],
            None,
            None,
            false,
            false,
        )
        .expect("an allowed spawn must proceed")
        .expect("spawn returns a Process");
        if let Value::Object(Some(proc_ref)) = allowed {
            if let Value::Long(handle) = ctx.get_field(proc_ref, PROC_FIELD_HANDLE) {
                assert_eq!(wait_for_handle(handle), 0);
            }
        }
        let _ = std::fs::remove_file(&marker);
    }

    /// `isAlive0` is keyed by PID and answers with a START TIME.
    ///
    /// Both halves were wrong at once. It took `args[0]` -- the JDK's pid --
    /// and looked it up as a `NEXT_HANDLE` id, two unrelated number spaces, so
    /// it found nothing and reported "still running" for every pid forever.
    /// And it returned 1/0 as though `ProcessHandleImpl.isAlive()` read a
    /// boolean; that method reads the value as a start time and treats ANY
    /// value >= 0 as alive, so the 0 it returned for an exited process meant
    /// alive as well. The two errors could not cancel out: the answer was
    /// "alive" either way.
    ///
    /// -1 is the only encoding of "not alive"; 0 is "alive, start time
    /// unknown".
    #[test]
    #[cfg(not(target_os = "windows"))]
    fn is_alive0_is_keyed_by_pid_and_reports_a_start_time() {
        let child = Command::new("/bin/sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sleep");
        let (handle, pid) = install_child_for_test(child);
        assert_ne!(handle, pid, "the test is meaningless if the two coincide");
        let mut ctx = MockNativeContext::new();

        let alive = native_proc_handle_is_alive0(&mut ctx, &[Value::Long(pid)])
            .unwrap()
            .unwrap();
        assert_eq!(
            alive,
            Value::Long(0),
            "a running child must report a start time >= 0"
        );

        // A pid that is neither ours nor anyone's must be -1. (Deliberately
        // NOT asserted for the internal handle id: on Linux a small integer is
        // a perfectly plausible live pid -- `/proc/1` is init -- so that
        // assertion would be testing the host, not this code.)
        let nobody = native_proc_handle_is_alive0(&mut ctx, &[Value::Long(pid + 4_000_000)])
            .unwrap()
            .unwrap();
        assert_eq!(nobody, Value::Long(-1), "an unused pid is not alive");

        assert!(destroy_handle(handle, true));
        let _ = wait_for_handle(handle);

        let dead = native_proc_handle_is_alive0(&mut ctx, &[Value::Long(pid)])
            .unwrap()
            .unwrap();
        assert_eq!(
            dead,
            Value::Long(-1),
            "a reaped child must report -1, the only value ProcessHandleImpl \
             .isAlive() reads as not-alive"
        );
    }

    /// `destroyProcess0` and `waitForProcessExit0` are keyed by pid too.
    #[test]
    #[cfg(not(target_os = "windows"))]
    fn destroy_and_wait_natives_resolve_the_pid_not_the_handle() {
        let child = Command::new("/bin/sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sleep");
        let (handle, pid) = install_child_for_test(child);
        let mut ctx = MockNativeContext::new();

        // A pid we never spawned cannot be waited for.
        let unknown = native_proc_handle_wait_for_process_exit0(
            &mut ctx,
            &[Value::Long(pid + 4_000_000), Value::Int(0)],
        )
        .unwrap()
        .unwrap();
        assert_eq!(unknown, Value::Int(-1));

        let killed =
            native_proc_handle_destroy_process0(&mut ctx, &[Value::Long(pid), Value::Int(1)])
                .unwrap()
                .unwrap();
        assert_eq!(killed, Value::Int(1), "destroyProcess0 must find the child by pid");

        let code = native_proc_handle_wait_for_process_exit0(
            &mut ctx,
            &[Value::Long(pid), Value::Int(0)],
        )
        .unwrap()
        .unwrap();
        assert!(
            matches!(code, Value::Int(c) if c != -1),
            "waitForProcessExit0 must find the child by pid, got {code:?}"
        );

        let _ = wait_for_handle(handle);
    }

    /// End-to-end spawn + waitFor round-trip — no NativeContext needed.
    #[test]
    fn spawn_and_wait_returns_exit_code() {
        // Use the platform-appropriate "echo" command; `cmd /c echo` on
        // Windows, `/bin/echo` on Linux.  We want a known-good exit.
        #[cfg(target_os = "windows")]
        let mut child = Command::new("cmd")
            .args(["/c", "echo", "hello"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn cmd /c echo");
        #[cfg(not(target_os = "windows"))]
        let mut child = Command::new("/bin/echo")
            .arg("hello")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn /bin/echo");

        let handle = 42424242i64;
        let pid = child.id() as i64;
        process_table()
            .lock()
            .insert(handle, Arc::new(Mutex::new(child)));
        exit_cache().lock().insert(
            handle,
            ExitCache {
                pid,
                exit_code: None,
            },
        );

        let code = wait_for_handle(handle);
        assert_eq!(code, 0, "echo should exit 0");
        // Second call must be idempotent and return the cached code.
        let code_again = wait_for_handle(handle);
        assert_eq!(code_again, 0);
    }

    /// `destroy_handle` signals the kill to the OS.
    #[test]
    fn destroy_handle_kills_live_child() {
        // Spawn a command that sleeps; we'll kill it.
        #[cfg(target_os = "windows")]
        let child = Command::new("cmd")
            .args(["/c", "ping", "-n", "30", "127.0.0.1"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        #[cfg(not(target_os = "windows"))]
        let child = Command::new("/bin/sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();

        let Ok(child) = child else {
            // If we can't spawn (e.g. PATH quirks on CI), skip this test.
            eprintln!("skip destroy_handle_kills_live_child — spawn failed");
            return;
        };
        let handle = 42424243i64;
        let pid = child.id() as i64;
        process_table()
            .lock()
            .insert(handle, Arc::new(Mutex::new(child)));
        exit_cache().lock().insert(
            handle,
            ExitCache {
                pid,
                exit_code: None,
            },
        );

        let killed = destroy_handle(handle, true);
        assert!(killed, "destroy_handle should succeed for live child");

        // After kill, waitFor returns some exit code (non-zero on
        // Unix; Windows returns 1).
        let _code = wait_for_handle(handle);
    }

    #[test]
    fn process_wait_for_enters_gc_blocked_region() {
        #[cfg(target_os = "windows")]
        let child = Command::new("cmd")
            .args(["/c", "echo", "x"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn");
        #[cfg(not(target_os = "windows"))]
        let child = Command::new("/bin/echo")
            .arg("x")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn");

        let (handle, pid) = install_child_for_test(child);
        let mut ctx = MockNativeContext::new();
        let proc_ref = mock_process(&mut ctx, handle, pid);

        let result = native_process_wait_for(&mut ctx, &[Value::Object(Some(proc_ref))])
            .unwrap()
            .unwrap();
        assert_eq!(result, Value::Int(0));
        assert_eq!(ctx.get_field(proc_ref, PROC_FIELD_EXIT), Value::Int(0));
        assert_eq!(ctx.blocking_region_counts(), (1, 1));
    }

    #[test]
    fn process_handle_wait_for_exit_enters_gc_blocked_region() {
        #[cfg(target_os = "windows")]
        let child = Command::new("cmd")
            .args(["/c", "echo", "x"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn");
        #[cfg(not(target_os = "windows"))]
        let child = Command::new("/bin/echo")
            .arg("x")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn");

        // The JDK's `ProcessHandleImpl.waitForProcessExit0` is handed a PID.
        // This test used to pass the internal handle, which is what the native
        // used to (wrongly) expect -- so it asserted the bug rather than the
        // contract. See `handle_for_pid`.
        let (_handle, pid) = install_child_for_test(child);
        let mut ctx = MockNativeContext::new();
        let result = native_proc_handle_wait_for_process_exit0(
            &mut ctx,
            &[Value::Long(pid), Value::Int(0)],
        )
        .unwrap()
        .unwrap();
        assert_eq!(result, Value::Int(0));
        assert_eq!(ctx.blocking_region_counts(), (1, 1));
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn process_wait_for_timeout_enters_gc_blocked_region_between_polls() {
        let child = Command::new("/bin/sleep")
            .arg("1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sleep");

        let (handle, pid) = install_child_for_test(child);
        let mut ctx = MockNativeContext::new();
        let proc_ref = mock_process(&mut ctx, handle, pid);

        let result = native_process_wait_for_timeout(
            &mut ctx,
            &[
                Value::Object(Some(proc_ref)),
                Value::Long(20),
                Value::Object(None),
            ],
        )
        .unwrap()
        .unwrap();
        assert_eq!(result, Value::Int(0));
        let (begin, end) = ctx.blocking_region_counts();
        assert!(begin >= 1, "timed wait should enter a blocked region");
        assert_eq!(begin, end, "blocked-region enter/leave must balance");

        let _ = destroy_handle(handle, true);
        let _ = wait_for_handle(handle);
    }

    /// The foreign-receiver timed wait must ALSO nap inside a blocked region.
    ///
    /// `waitFor(long, TimeUnit)` on an application subclass polls the
    /// subclass's own `exitValue()`, so it can wait for the full timeout with no
    /// subprocess handle in sight. The thread is in `NativeRunning` throughout,
    /// which the STW census waits for, so a nap outside a blocked region is a
    /// GC pause of the caller's choosing. The first cut of the receiver guard
    /// had exactly that bug.
    #[test]
    fn foreign_receiver_timed_wait_also_enters_a_blocked_region() {
        let mut ctx = MockNativeContext::new();
        // No class name => not one of the VM's process objects, which is the
        // whole point: this is the application-subclass path.
        let foreign = ctx.alloc_object(2);
        assert!(
            !is_vm_process(&mut ctx, foreign),
            "an unnamed mock object must not be mistaken for a VM Process"
        );

        let result = native_process_wait_for_timeout(
            &mut ctx,
            &[
                Value::Object(Some(foreign)),
                Value::Long(20),
                Value::Object(None),
            ],
        )
        .unwrap()
        .unwrap();

        // The mock's `invoke_virtual` has no `exitValue` to run, so the poll
        // never sees an exit and the wait times out — which is the specified
        // answer for a process that has not exited, and the answer the
        // pre-guard code got wrong by reading slot bytes off a stranger.
        assert_eq!(result, Value::Int(0), "a never-exiting process waits out its timeout");
        let (begin, end) = ctx.blocking_region_counts();
        assert!(
            begin >= 1,
            "the foreign-receiver poll must nap inside a blocked region, not spin outside one"
        );
        assert_eq!(begin, end, "blocked-region enter/leave must balance");
    }

    /// `pid_for_handle` returns the captured pid even after reap.
    #[test]
    fn pid_for_handle_is_stable_after_wait() {
        #[cfg(target_os = "windows")]
        let child = Command::new("cmd")
            .args(["/c", "echo", "x"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn");
        #[cfg(not(target_os = "windows"))]
        let child = Command::new("/bin/echo")
            .arg("x")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn");

        let handle = 42424244i64;
        let pid = child.id() as i64;
        process_table()
            .lock()
            .insert(handle, Arc::new(Mutex::new(child)));
        exit_cache().lock().insert(
            handle,
            ExitCache {
                pid,
                exit_code: None,
            },
        );
        assert_eq!(pid_for_handle(handle), pid);
        let _ = wait_for_handle(handle);
        // pid should still be retrievable post-wait.
        assert_eq!(pid_for_handle(handle), pid);
    }

    /// V1: under CWD confinement the spawn gate rejects a bare PATH-resolved
    /// program name and an explicit path that escapes the sandbox, while the
    /// default (unconfined) profile accepts anything. NUL is always rejected.
    #[test]
    fn validate_spawn_program_confinement_gate() {
        // Default profile: anything spawns (JDK-faithful single-tenant).
        crate::set_path_confine_to_cwd(false);
        assert!(validate_spawn_program("sh").is_ok());
        assert!(validate_spawn_program("/bin/sh").is_ok());
        assert!(validate_spawn_program(r"C:\Windows\System32\cmd.exe").is_ok());
        // NUL is rejected even with confinement off (host C-string truncation).
        assert!(validate_spawn_program("sh\0-c").is_err());

        // Confined profile: fail closed.
        crate::set_path_confine_to_cwd(true);
        // Bare command name -> PATH-resolved host binary -> rejected.
        assert!(
            validate_spawn_program("sh").is_err(),
            "bare program name must be rejected under confinement"
        );
        assert!(
            validate_spawn_program("cmd").is_err(),
            "bare program name must be rejected under confinement"
        );
        // An explicit absolute path outside the sandbox is rejected by the
        // containment check inside `validate_path`.
        #[cfg(unix)]
        assert!(
            validate_spawn_program("/bin/sh").is_err(),
            "explicit out-of-sandbox path must be rejected under confinement"
        );
        #[cfg(windows)]
        assert!(
            validate_spawn_program(r"C:\Windows\System32\cmd.exe").is_err(),
            "explicit out-of-sandbox path must be rejected under confinement"
        );

        // Restore the global default so we don't leak state to other tests.
        crate::set_path_confine_to_cwd(false);
    }

    #[test]
    fn tokenize_keeps_quoted_paths_intact() {
        // A quoted program path with spaces must stay a single argv[0].
        let argv = tokenize_command_line(r#""C:\Program Files\Java\bin\java.exe" -version"#);
        assert_eq!(
            argv,
            vec![
                r"C:\Program Files\Java\bin\java.exe".to_string(),
                "-version".to_string()
            ]
        );
    }

    #[test]
    fn tokenize_plain_and_quoted_args() {
        assert_eq!(
            tokenize_command_line("prog -cp \"a b\" Main"),
            vec![
                "prog".to_string(),
                "-cp".to_string(),
                "a b".to_string(),
                "Main".to_string()
            ]
        );
        // Embedded "" inside a quoted section -> literal quote.
        assert_eq!(
            tokenize_command_line(r#"prog "say ""hi""""#),
            vec!["prog".to_string(), r#"say "hi""#.to_string()]
        );
        // Empty / whitespace-only input yields no tokens.
        assert!(tokenize_command_line("   ").is_empty());
        // An explicit empty quoted argument is preserved.
        assert_eq!(
            tokenize_command_line(r#"prog """#),
            vec!["prog".to_string(), String::new()]
        );
    }
}
