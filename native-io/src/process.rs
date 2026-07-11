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
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use cratonvm_native_api::fd_table::FdId;
use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError, VmError};
use cratonvm_types::{ObjectRef, Value};

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
/// Entries stay until `waitFor` / `destroy` observes termination and
/// removes them.  We don't reap zombies eagerly — the OS keeps the
/// child's exit status in the process entry until either `wait()` is
/// called (POSIX) or the `HANDLE` is closed (Windows).  Both happen
/// naturally when the `Child` is dropped from the table.
fn process_table() -> &'static Mutex<HashMap<i64, Child>> {
    static T: OnceLock<Mutex<HashMap<i64, Child>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
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

/// Field layout on the synthetic `java/lang/Process` object.
/// Must match the initialization done by the bytecode / native below.
const PROC_FIELD_EXIT: usize = 0;
const PROC_FIELD_STDIN_FD: usize = 1;
const PROC_FIELD_STDOUT_FD: usize = 2;
const PROC_FIELD_STDERR_FD: usize = 3;
const PROC_FIELD_PID: usize = 4;
const PROC_FIELD_HANDLE: usize = 5;

/// Sentinel "not yet exited" value stored in field 0.
const EXIT_NOT_YET: i32 = i32::MIN;

/// Total number of fields on the synthetic Process.
const PROC_FIELD_COUNT: usize = 6;

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
/// `junit-platform-console --help` (docs/gaps/gap-anonymous-object-getinputstream.md).
const SYNTHETIC_PROCESS_CLASS: &str = "cratonvm/synthetic/Process";
const SYNTHETIC_PROCESS_INPUT_STREAM: &str = "cratonvm/synthetic/ProcessPipeInputStream";
const SYNTHETIC_PROCESS_OUTPUT_STREAM: &str = "cratonvm/synthetic/ProcessPipeOutputStream";

fn pb_debug_enabled() -> bool {
    std::env::var_os("CRATONVM_DBG_PB").is_some()
}

#[derive(Clone, Debug)]
enum StdioRedirect {
    Pipe,
    Inherit,
    Null,
    ReadFile(String),
    WriteFile { path: String, append: bool },
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

fn stdin_stdio(spec: &StdioRedirect) -> Result<(Stdio, bool), RuntimeError> {
    match spec {
        StdioRedirect::Pipe => Ok((Stdio::piped(), true)),
        StdioRedirect::Inherit => Ok((Stdio::inherit(), false)),
        StdioRedirect::Null => Ok((Stdio::null(), false)),
        StdioRedirect::ReadFile(path) => Ok((Stdio::from(open_redirect_input(path)?), false)),
        StdioRedirect::WriteFile { path, .. } => Err(RuntimeError::IOException {
            message: format!(
                "ProcessBuilder.redirectInput cannot read from output redirect: {path}"
            ),
        }),
    }
}

fn output_stdio(spec: &StdioRedirect, op: &str) -> Result<(Stdio, bool), RuntimeError> {
    match spec {
        StdioRedirect::Pipe => Ok((Stdio::piped(), true)),
        StdioRedirect::Inherit => Ok((Stdio::inherit(), false)),
        StdioRedirect::Null => Ok((Stdio::null(), false)),
        StdioRedirect::WriteFile { path, append } => {
            Ok((Stdio::from(open_redirect_output(path, *append)?), false))
        }
        StdioRedirect::ReadFile(path) => Err(RuntimeError::IOException {
            message: format!("ProcessBuilder.{op} cannot write to input redirect: {path}"),
        }),
    }
}

fn configure_stdio(
    command: &mut Command,
    redirects: &ProcessRedirects,
    redirect_error_stream: bool,
) -> Result<(bool, bool, bool, Option<std::io::PipeReader>), RuntimeError> {
    let (stdin, stdin_piped) = stdin_stdio(&redirects.stdin)?;
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
            StdioRedirect::ReadFile(path) => Err(RuntimeError::IOException {
                message: format!(
                    "ProcessBuilder.redirectOutput cannot write to input redirect: {path}"
                ),
            }),
        }
    } else {
        let (stdout, stdout_piped) = output_stdio(&redirects.stdout, "redirectOutput")?;
        let (stderr, stderr_piped) = output_stdio(&redirects.stderr, "redirectError")?;
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
/// larger escape than the file reads the certified profile is designed to
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
    if program.is_empty() {
        return Err(RuntimeError::IllegalArgumentException {
            message: "ProcessBuilder: empty program".to_string(),
        }
        .into());
    }

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
        configure_stdio(&mut command, redirects, redirect_error_stream)
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
    process_table().lock().insert(handle, child);
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

    // Allocate the synthetic Process under its own named class (see
    // SYNTHETIC_PROCESS_CLASS) and populate its 6 fields.
    let proc_class = ctx.ensure_synthetic_class(SYNTHETIC_PROCESS_CLASS, PROC_FIELD_COUNT);
    let proc_ref = ctx.alloc_object(proc_class, PROC_FIELD_COUNT);
    ctx.set_field(proc_ref, PROC_FIELD_EXIT, Value::Int(EXIT_NOT_YET));
    ctx.set_field(proc_ref, PROC_FIELD_STDIN_FD, Value::Int(stdin_fd));
    ctx.set_field(proc_ref, PROC_FIELD_STDOUT_FD, Value::Int(stdout_fd));
    ctx.set_field(proc_ref, PROC_FIELD_STDERR_FD, Value::Int(stderr_fd));
    ctx.set_field(proc_ref, PROC_FIELD_PID, Value::Long(pid));
    ctx.set_field(proc_ref, PROC_FIELD_HANDLE, Value::Long(handle));

    Ok(Some(Value::Object(Some(proc_ref))))
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
    // Slow path: block on the OS.
    let child_opt = {
        let mut table = process_table().lock();
        table.remove(&handle)
    };
    let Some(mut child) = child_opt else {
        // Handle unknown (double-wait without prior removal, or forged)
        return -1;
    };
    let status = match child.wait() {
        Ok(s) => s,
        Err(_e) => return -1,
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
    let mut table = process_table().lock();
    let entry = table.get_mut(&handle)?;
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
            // Remove from the live table so its resources can be reclaimed.
            table.remove(&handle);
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
pub fn destroy_handle(handle: i64, _force: bool) -> bool {
    let mut table = process_table().lock();
    match table.get_mut(&handle) {
        Some(child) => child.kill().is_ok(),
        None => false,
    }
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

fn read_process_environment(
    ctx: &mut dyn NativeContext,
    builder: ObjectRef,
) -> Option<Vec<(String, String)>> {
    let env_obj = match ctx.get_field_by_name(builder, "environment") {
        Value::Object(Some(o)) => o,
        _ => return None,
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

/// `java.lang.UNIXProcess.forkAndExec(mode, helperpath, prog, argBlock, argc, envBlock, envc, dir, std_fds, redirectErrorStream) -> int`
///
/// Linux/macOS equivalent of `ProcessImpl.create`.  Returns a pid.
/// Same behavior as the Windows variant — we route through `spawn_and_wrap`.
#[cfg(target_os = "linux")]
fn native_unix_fork_and_exec(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // UNIXProcess encodes the argv as a null-separated byte block.
    // args[2] = prog (byte[]), args[3] = argBlock (byte[]),
    // args[5] = envBlock (byte[] — can be null).
    //
    // Since we route through `std::process::Command` which uses real
    // argv arrays, we decode the byte-block form back to strings.
    fn decode_byte_array(ctx: &mut dyn NativeContext, arr: ObjectRef) -> Vec<u8> {
        // AUDIT 2026-05-24: bulk read via NativeContext intrinsic
        // instead of per-element `get_array_element`. Single memcpy
        // from the heap byte[] payload.
        let len = ctx.array_length(arr);
        let mut out = vec![0u8; len];
        let n = ctx.read_byte_array_into(arr, 0, &mut out);
        out.truncate(n);
        out
    }
    let prog_bytes = match args.get(2) {
        Some(Value::Object(Some(arr))) => decode_byte_array(ctx, *arr),
        _ => return Ok(Some(Value::Int(-1))),
    };
    let arg_block_bytes = match args.get(3) {
        Some(Value::Object(Some(arr))) => decode_byte_array(ctx, *arr),
        _ => Vec::new(),
    };
    let env_block_bytes = match args.get(5) {
        Some(Value::Object(Some(arr))) => Some(decode_byte_array(ctx, *arr)),
        _ => None,
    };

    // Strip trailing null byte from prog name if present.
    let program = {
        let trimmed: &[u8] = prog_bytes.strip_suffix(&[0u8]).unwrap_or(&prog_bytes[..]);
        String::from_utf8_lossy(trimmed).into_owned()
    };
    // argBlock is NUL-separated.
    let args_vec: Vec<String> = arg_block_bytes
        .split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect();
    let env_vars: Option<Vec<(String, String)>> = env_block_bytes.map(|b| {
        b.split(|&c| c == 0)
            .filter(|s| !s.is_empty())
            .filter_map(|s| {
                let text = String::from_utf8_lossy(s);
                text.split_once('=')
                    .map(|(k, v)| (k.to_string(), v.to_string()))
            })
            .collect()
    });
    let work_dir = match args.get(7) {
        Some(Value::Object(Some(arr))) => {
            let bytes = decode_byte_array(ctx, *arr);
            let trimmed: &[u8] = bytes.strip_suffix(&[0u8]).unwrap_or(&bytes[..]);
            Some(String::from_utf8_lossy(trimmed).into_owned())
        }
        _ => None,
    };

    // forkAndExec's last arg is redirectErrorStream (boolean).
    let redirect_err = matches!(args.get(9), Some(Value::Int(v)) if *v != 0);
    let result = spawn_and_wrap(
        ctx,
        &program,
        &args_vec,
        work_dir.as_deref(),
        env_vars.as_deref(),
        env_vars.is_some(),
        redirect_err,
    )?;
    match result {
        Some(Value::Object(Some(proc_ref))) => {
            let pid = match ctx.get_field(proc_ref, PROC_FIELD_PID) {
                Value::Long(p) => p as i32,
                _ => -1,
            };
            Ok(Some(Value::Int(pid)))
        }
        _ => Ok(Some(Value::Int(-1))),
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
/// Returns the start-time of the process (or 0 if dead) in HotSpot's
/// spec; our simplified implementation returns 1 if alive, 0 if dead.
/// JDK code checks `> 0`.
fn native_proc_handle_is_alive0(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let handle = match args.first() {
        Some(Value::Long(h)) => *h,
        _ => return Ok(Some(Value::Long(0))),
    };
    // Handle 0 = current JVM process — always alive.
    if handle == 0 {
        return Ok(Some(Value::Long(1)));
    }
    match try_exit_handle(handle) {
        Some(_) => Ok(Some(Value::Long(0))), // exited
        None => Ok(Some(Value::Long(1))),    // still running
    }
}

/// `java.lang.ProcessHandleImpl.waitForProcessExit0(long, boolean) -> int`
fn native_proc_handle_wait_for_process_exit0(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let handle = match args.first() {
        Some(Value::Long(h)) => *h,
        _ => return Ok(Some(Value::Int(-1))),
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
    let handle = match args.first() {
        Some(Value::Long(h)) => *h,
        _ => return Ok(Some(Value::Int(0))),
    };
    let force = matches!(args.get(1), Some(Value::Int(1)));
    let ok = destroy_handle(handle, force);
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
/// (docs/known-issues/wildfly-process-tohandle-missing.md).
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
/// natives (`isAlive0`/`destroy0`/`waitForProcessExit0`), which key off the
/// VM's *internal* subprocess-table handle, not the real OS pid we store
/// here — so they'll take the same "not found in table" fallback path
/// `ProcessHandle.current()` already exercises, rather than accurately
/// tracking this specific child. Fixing that needs the table to also be
/// queryable by real pid, which is out of scope for the missing-native fix.
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
    let pid = match ctx.get_field(this, PROC_FIELD_PID) {
        Value::Long(p) => p,
        _ => -1,
    };
    match ctx.new_object_initialized(
        "java/lang/ProcessHandleImpl",
        "(JJ)V",
        &[Value::Long(pid), Value::Long(0)],
    ) {
        Ok(Some(v)) => Ok(Some(v)),
        _ => {
            // Pure-synthetic fallback: no real ProcessHandleImpl class was
            // loadable at all, so fall back to the 1-field synthetic
            // `java/lang/ProcessHandle` layout `ProcessHandle.current()`
            // already uses in phases_late.rs (field 0 = pid).
            let handle = alloc_process_handle(ctx, Value::Long(pid));
            Ok(Some(Value::Object(Some(handle))))
        }
    }
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

fn pipe_io_err(err: impl std::fmt::Display) -> MethodCallFailed {
    RuntimeError::IOException {
        message: err.to_string(),
    }
    .into()
}

fn pipe_array_bounds(off: i32, len: i32, arr_len: usize) -> Result<(), MethodCallFailed> {
    if off < 0 || len < 0 {
        return Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::ArrayIndexOutOfBoundsException {
                index: if off < 0 { off } else { len },
            },
        )));
    }
    match (off as usize).checked_add(len as usize) {
        Some(end) if end <= arr_len => Ok(()),
        _ => Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::ArrayIndexOutOfBoundsException {
                index: off.saturating_add(len),
            },
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

fn captured_string_stream(ctx: &mut dyn NativeContext, text: &str) -> Value {
    let cid = match ctx.ensure_class_initialized("java/io/ByteArrayInputStream") {
        Ok(cid) => cid,
        Err(_) => ctx.ensure_synthetic_class("java/io/ByteArrayInputStream", 4),
    };
    let stream = ctx.alloc_object(cid, 4usize.max(ctx.class_num_total_fields(cid)));
    let pin = ctx.pin_native_root(stream);
    let bytes = text.as_bytes();
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, bytes.len());
    let stream = ctx.read_native_pin(pin, stream);
    ctx.unpin_native_roots(pin);
    for (i, &byte) in bytes.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(byte as i8 as i32));
    }
    ctx.set_field(stream, 0, Value::Object(Some(arr)));
    ctx.set_field(stream, 1, Value::Int(0));
    ctx.set_field(stream, 2, Value::Int(0));
    ctx.set_field(stream, 3, Value::Int(bytes.len() as i32));
    Value::Object(Some(stream))
}

fn legacy_captured_stream(ctx: &mut dyn NativeContext, args: &[Value], field: usize) -> Option<Value> {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return None,
    };
    match ctx.get_field(this, field) {
        Value::Object(Some(s)) => ctx.read_string(s).map(|text| captured_string_stream(ctx, &text)),
        _ => None,
    }
}

/// `java.lang.Process.getInputStream()Ljava/io/InputStream;` — the child's
/// stdout pipe.
fn native_process_get_input_stream(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if let Some(stream) = legacy_captured_stream(ctx, args, PROC_FIELD_STDIN_FD) {
        return Ok(Some(stream));
    }
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
    if let Some(stream) = legacy_captured_stream(ctx, args, PROC_FIELD_STDOUT_FD) {
        return Ok(Some(stream));
    }
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
// Registration
// ---------------------------------------------------------------------------

/// Register every WP1.12-owned subprocess native.  Called from
/// `register_io_natives` at VM boot.
pub fn register_process_natives(registry: &mut NativeMethodRegistry) {
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::Bridge);
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

    #[cfg(target_os = "linux")]
    registry.register(
        "java/lang/UNIXProcess",
        "forkAndExec",
        "(I[B[B[BI[BI[BZ)I",
        native_unix_fork_and_exec,
    );

    // ProcessHandleImpl family — ProcessHandle.current() / Process.pid()
    // ultimately reach these.
    // OpenJDK <clinit> calls initNative(); without it, UnsatisfiedLinkError leaves
    // internal stubs null and Spring Boot / logging fails on ProcessHandle.current().
    registry.register(
        "java/lang/ProcessHandleImpl",
        "initNative",
        "()V",
        |_ctx, _args| Ok(None),
    );
    registry.register(
        "java/lang/ProcessHandleImpl",
        "getCurrentPid0",
        "()J",
        native_proc_handle_current_pid0,
    );
    registry.register(
        "java/lang/ProcessHandleImpl",
        "isAlive0",
        "(J)J",
        native_proc_handle_is_alive0,
    );
    registry.register(
        "java/lang/ProcessHandleImpl",
        "waitForProcessExit0",
        "(JZ)I",
        native_proc_handle_wait_for_process_exit0,
    );
    registry.register(
        "java/lang/ProcessHandleImpl",
        "destroyProcess0",
        "(JZ)Z",
        native_proc_handle_destroy_process0,
    );
    // Real JDK 25 signature: destroy0(pid, startTime, forcibly) -> boolean.
    registry.register(
        "java/lang/ProcessHandleImpl",
        "destroy0",
        "(JJZ)Z",
        |_ctx, args| {
            let handle = match args.first() {
                Some(Value::Long(h)) => *h,
                _ => return Ok(Some(Value::Int(0))),
            };
            let force = matches!(args.get(2), Some(Value::Int(1)));
            let ok = destroy_handle(handle, force);
            Ok(Some(Value::Int(if ok { 1 } else { 0 })))
        },
    );
    // parent0(pid, startTime) -> long. We don't track parent relationships;
    // returning -1 is the documented "unknown" value.
    registry.register(
        "java/lang/ProcessHandleImpl",
        "parent0",
        "(JJ)J",
        |_ctx, _args| Ok(Some(Value::Long(-1))),
    );
    // getProcessPids0(pid, pids[], ppids[], starttimes[]) -> int (count).
    // We don't enumerate child processes; return 0 (no children found).
    registry.register(
        "java/lang/ProcessHandleImpl",
        "getProcessPids0",
        "(J[J[J[J)I",
        |_ctx, _args| Ok(Some(Value::Int(0))),
    );

    // ProcessHandleImpl$Info: initIDs() is a JNI fieldID cache init — no-op for us.
    // info0(pid) fills in command/user/arguments/startTime/totalTime fields. Without
    // OS-level introspection we leave the fields at their constructor defaults (null/-1),
    // which the JDK code path handles gracefully (info() returns a partially-empty Info).
    registry.register(
        "java/lang/ProcessHandleImpl$Info",
        "initIDs",
        "()V",
        |_ctx, _args| Ok(None),
    );
    registry.register(
        "java/lang/ProcessHandleImpl$Info",
        "info0",
        "(J)V",
        |_ctx, _args| Ok(None),
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
    }

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

    // ProcessBuilder.start — route through the real spawn path.  This
    // overrides the synthetic stub from phases_late.
    registry.register(
        "java/lang/ProcessBuilder",
        "start",
        "()Ljava/lang/Process;",
        native_process_builder_start,
    );
    registry.set_category(__prev_cat);
}

/// `java.lang.ProcessBuilder.start()Ljava/lang/Process;`
///
/// Reads the command + directory + env map fields that the
/// ProcessBuilder synthetic lays out in phases_late, then spawns the
/// child via `spawn_and_wrap`.
fn native_process_builder_start(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
    use super::*;
    use crate::test_support::MockNativeContext;

    fn install_child_for_test(child: Child) -> (i64, i64) {
        let handle = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
        let pid = child.id() as i64;
        process_table().lock().insert(handle, child);
        exit_cache().lock().insert(
            handle,
            ExitCache {
                pid,
                exit_code: None,
            },
        );
        (handle, pid)
    }

    fn mock_process(ctx: &mut MockNativeContext, handle: i64, pid: i64) -> ObjectRef {
        let proc_ref = ctx.alloc_object(PROC_FIELD_COUNT);
        ctx.set_field(proc_ref, PROC_FIELD_EXIT, Value::Int(EXIT_NOT_YET));
        ctx.set_field(proc_ref, PROC_FIELD_PID, Value::Long(pid));
        ctx.set_field(proc_ref, PROC_FIELD_HANDLE, Value::Long(handle));
        proc_ref
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
        process_table().lock().insert(handle, child);
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
        process_table().lock().insert(handle, child);
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

        let (handle, _pid) = install_child_for_test(child);
        let mut ctx = MockNativeContext::new();
        let result = native_proc_handle_wait_for_process_exit0(
            &mut ctx,
            &[Value::Long(handle), Value::Int(0)],
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
        process_table().lock().insert(handle, child);
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
