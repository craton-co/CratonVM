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

/// One VM's process tables (interpreter round i5 wave 101, lane L1; page
/// `interpreter-i101-L1-proposal-per-vm-process-tables-and-a-settled-suspension-word-DONE-20261021.md`,
/// part 1). Until wave 101 every field below was a process-wide static, so
/// two VMs in one process (`cratonvm-embed`, `libcratonvm`, the test
/// harnesses) shared them: one VM's `ProcessHandleImpl.waitForProcessExit0` /
/// `isAlive0` / `destroy0` found the other VM's children by pid
/// ([`handle_for_pid`]) and answered for them as its own children, and nothing
/// released a VM's rows when it ended. Now each VM owns one set, held in its
/// native registry's slot (`NativeMethodRegistry::process_tables_slot`,
/// reached through [`tables`]) and dropped with the VM.
///
/// The OS-level reaping stays sound per VM: nothing here installs a `SIGCHLD`
/// handler or calls `waitpid(-1)`. Each child is reaped by `waitpid(pid)`
/// inside its own `std::process::Child` (`wait` / `try_wait`), which only the
/// owning VM's table holds, so a VM never reaps another VM's child; a pid
/// another VM spawned is "not a child" here (`NOT_A_CHILD`, then the JDK's
/// `isAlive0` poll), as a separate HotSpot process would see it. An exit
/// reaper thread ([`ensure_process_exit_reaper`]) holds an `Arc` of its VM's
/// tables until its child exits, so a VM that ends with a child still running
/// keeps that one row set alive until the exit, and no longer.
pub struct ProcessTables {
    /// Opaque handle into [`Self::children`]. We return this to Java-land
    /// as a 64-bit integer so a single `Process` instance can be looked up
    /// later without storing a raw pointer in bytecode land.
    ///
    /// Starts at 1 (0 = "no live process", reserved sentinel).
    next_handle: AtomicI64,
    /// The subprocess table (see [`child_for`]).
    children: Mutex<HashMap<i64, Arc<Mutex<Child>>>>,
    /// Extra per-process state that can't live inside `std::process::Child`:
    /// the terminal exit code (so `waitFor` is idempotent) and the pid
    /// captured at spawn time (so `pid()` works after the `Child` has been
    /// reaped).
    exits: Mutex<HashMap<i64, ExitCache>>,
    /// The parent-side pipe ids of each child.
    pipes: Mutex<HashMap<i64, PipeFds>>,
    /// The exit signal (see [`notify_process_exit`]).
    exit_signal: (Mutex<u64>, parking_lot::Condvar),
    /// The handles that have an exit reaper ([`ensure_process_exit_reaper`]).
    reapers: Mutex<std::collections::HashSet<i64>>,
    /// `CRATONVM_PROCESS_WAIT_DIRECT_WAKE` for this VM, read once
    /// (interpreter round i5 wave 102, lane L1; default OFF until the A/B
    /// medians of `L1W102ProcessExitWakeBench`): see [`direct_wake`].
    direct_wake: bool,
    /// `CRATONVM_PROCESS_PIDFD_WAIT` for this VM, read once (interpreter round
    /// i5 wave 102, lane L1; part 2 of
    /// `interpreter-i101-L1-proposal-per-vm-process-tables-and-a-settled-suspension-word-DONE-20261021.md`;
    /// default ON since the round-i5 switch triage, `=0` off): Linux waits on
    /// a `pidfd_open` descriptor (see `LinuxPidfdWait`). Read on every
    /// target, used on Linux only.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pidfd_wait: bool,
    /// `CRATONVM_PROCESS_PIDFD_REAPER` for this VM, read once (interpreter
    /// round i7 wave 136, lane L1b; item 1 of
    /// `docs/feature-designs/jdk-only-mode.md`;
    /// performance, default OFF): [`wait_for_handle`] polls a `pidfd_open`
    /// descriptor with no mutex held, then reaps once (`try_wait`), instead
    /// of holding the child's mutex in `Child::wait` for the child's whole
    /// life ([`wait_for_handle_pidfd`]). Read on every target, used on Linux
    /// only.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pidfd_reaper: bool,
    /// `CRATONVM_PROCESS_WAIT_INTERRUPT_EVENT` for this VM, read once
    /// (interpreter round i6 wave 115, lane L1; page
    /// `interpreter-i114-L1-proposal-windows-process-wait-wakes-on-interrupt-DONE-20261019.md`;
    /// default ON since the round-i6 wave-115 triage, `=0` off; performance):
    /// see [`exit_wait_slice`]. Read on every
    /// target, used on Windows only.
    #[cfg_attr(not(windows), allow(dead_code))]
    interrupt_event: bool,
    /// `CRATONVM_PROCESS_WAIT_LAZY_NATIVE` for this VM, read once
    /// (interpreter round i7 wave 139, lane L1b; item 2 of
    /// `docs/feature-designs/jdk-only-mode.md`;
    /// performance, default OFF): `waitForProcessExit0` for a VM-spawned
    /// child waits in the lazy in-native state
    /// (`NativeContext::run_in_native_wait_lazy`: no root deposit on entry)
    /// instead of an `InNative` blocking region, when the context can.
    pub(crate) wait_lazy_native: bool,
}

impl ProcessTables {
    /// An empty table set.
    pub fn new() -> Self {
        Self {
            next_handle: AtomicI64::new(1),
            children: Mutex::new(HashMap::new()),
            exits: Mutex::new(HashMap::new()),
            pipes: Mutex::new(HashMap::new()),
            exit_signal: (Mutex::new(0), parking_lot::Condvar::new()),
            reapers: Mutex::new(std::collections::HashSet::new()),
            direct_wake: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_PROCESS_WAIT_DIRECT_WAKE",
            ),
            pidfd_wait: cratonvm_types::flags::runtime_flag_default_on("CRATONVM_PROCESS_PIDFD_WAIT"),
            pidfd_reaper: cratonvm_types::flags::runtime_flag_on("CRATONVM_PROCESS_PIDFD_REAPER"),
            interrupt_event: cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_PROCESS_WAIT_INTERRUPT_EVENT",
            ),
            wait_lazy_native: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_PROCESS_WAIT_LAZY_NATIVE",
            ),
        }
    }
}

/// `CRATONVM_PROCESS_WAIT_DIRECT_WAKE` (interpreter round i5 wave 102, lane
/// L1, performance, default OFF; page
/// `interpreter-i97-L1-process-exit-wake-latency-DONE-20261224.md`, "Wave 102"): what a waiter
/// for a VM-spawned child does between the OS wake and its return, cut to the
/// minimum.
///
/// * The OS wait object answers the code (`ExitWait::wait` returns it): the
///   waiter breaks out with it instead of looping once more through
///   [`try_exit_handle`] (a second exit-cache lock, and on Windows a lost
///   `try_lock` race against the other waiter woken by the same exit).
/// * The `--compatible` timed `waitFor(long, TimeUnit)` stand-in waits all
///   its slices inside ONE blocking region, as the `--jdk-only` native and the
///   untimed stand-in already do, instead of a leave + enter (a root deposit)
///   per 10 ms slice: the exit can no longer land while the waiter is
///   between two slices' transitions.
/// * The Windows `getExitCodeProcess` / `isProcessAlive` natives the JDK
///   body calls right after the wait answer a cached exit from ONE exit-cache
///   lookup (they took two: `known_handle`, then `try_exit_handle`).
///
/// No Java-visible answer changes: the same code, the same interrupt
/// semantics (the status is read between slices, inside the region, as
/// before), the same deadlines.
fn direct_wake(t: &ProcessTables) -> bool {
    t.direct_wake
}

impl Default for ProcessTables {
    fn default() -> Self {
        Self::new()
    }
}

/// The tables held in a VM's slot, made on first use.
fn tables_in(
    slot: &std::sync::OnceLock<Box<dyn std::any::Any + Send + Sync>>,
) -> Arc<ProcessTables> {
    let held = slot.get_or_init(|| Box::new(Arc::new(ProcessTables::new())));
    match held.downcast_ref::<Arc<ProcessTables>>() {
        Some(tables) => Arc::clone(tables),
        // Another crate put something else in the slot: never by design.
        // A detached set keeps this VM's children apart from the others'.
        None => detached_tables(),
    }
}

/// The calling VM's process tables. A context with no slot (a mock context
/// in unit tests) shares one detached set.
pub(crate) fn tables(ctx: &dyn NativeContext) -> Arc<ProcessTables> {
    match ctx.process_tables_slot() {
        Some(slot) => tables_in(slot),
        None => detached_tables(),
    }
}

/// The table set of contexts without a VM slot (mock contexts).
fn detached_tables() -> Arc<ProcessTables> {
    Arc::clone(detached_tables_ref())
}

/// [`detached_tables`] by reference (the unit tests' table set too).
fn detached_tables_ref() -> &'static Arc<ProcessTables> {
    static DETACHED: OnceLock<Arc<ProcessTables>> = OnceLock::new();
    DETACHED.get_or_init(|| Arc::new(ProcessTables::new()))
}

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
///
/// Look up the shared child for `handle` without holding the table lock.
fn child_for(t: &Arc<ProcessTables>, handle: i64) -> Option<Arc<Mutex<Child>>> {
    t.children.lock().get(&handle).cloned()
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
    /// The `FileDescriptorTable::generation` of each pipe at spawn (stdin,
    /// stdout, stderr; 0 for a slot with no pipe). JIT r23 wave 4, lane L6:
    /// child pipes take OS keys (`CRATONVM_FD_TABLE_OS_KEYS`), which the OS
    /// reuses once a pipe is closed, so [`process_stream`]'s fallback to this
    /// row hands out an id only while it still names the pipe it recorded.
    gens: [u64; 3],
}

impl PipeFds {
    /// `fd` (one of this row's ids) if the table still holds the entry that
    /// was spawned under it, else -1 (closed, or a later owner of the value).
    fn live_fd(&self, ctx: &dyn NativeContext, fd: i32, slot: usize) -> i32 {
        if fd < 0 {
            return -1;
        }
        let gen = self.gens.get(slot).copied().unwrap_or(0);
        match ctx.fd_table().generation(fd as FdId) {
            Some(live) if live == gen => fd,
            _ => -1,
        }
    }
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
/// The process's one `AbstractQueuedSynchronizer$ConditionObject`, the park
/// blocker its `waitFor` stand-ins publish (a reference slot, null until the
/// first `waitFor` that publishes one fills it; see
/// [`process_wait_condition_of`]). Interpreter round i2 wave 69, lane L1
/// (`CRATONVM_PROCESS_ONE_WAIT_CONDITION`). The one allocation site,
/// `spawn_and_wrap`, sizes the object from [`PROC_FIELD_COUNT`], and every
/// fabrication of `SYNTHETIC_PROCESS_CLASS` passes that same constant, so the
/// class keeps one layout.
const PROC_FIELD_CONDITION: usize = JAVA_PROCESS_FIELD_COUNT + 6;

/// Sentinel "not yet exited" value stored in the exit-code field.
const EXIT_NOT_YET: i32 = i32::MIN;

/// `ProcessHandleImpl.NOT_A_CHILD` — what `waitForProcessExit0` answers for a
/// pid that is not a child of this process.
///
/// `@Native`-annotated on the JDK side, which makes it part of the native
/// contract rather than an implementation detail: `ProcessHandleImpl.completion`
/// compares against it by name and branches. Never answered on Windows, as
/// HotSpot's Windows native never answers it (wave 100, lane L1).
#[cfg_attr(windows, allow(dead_code))]
const PROCESS_NOT_A_CHILD: i32 = -2;

/// The stream objects `getOutputStream` / `getInputStream` / `getErrorStream`
/// handed out (reference slots, null until the first call). JIT r23 wave 4,
/// lane L6: the JDK's `ProcessImpl` builds each pipe stream ONCE and every
/// getter returns that object, so a `close()` on it closes the pipe for every
/// holder. The stand-in allocated a fresh stream per call, each holding the
/// raw pipe id: closing one left its siblings holding an id the OS may hand
/// to an unrelated file or pipe under `CRATONVM_FD_TABLE_OS_KEYS`. Now one per
/// pipe, as the JDK (and `p.getInputStream() == p.getInputStream()`, as on
/// HotSpot).
const PROC_FIELD_STDIN_STREAM: usize = JAVA_PROCESS_FIELD_COUNT + 7;
const PROC_FIELD_STDOUT_STREAM: usize = JAVA_PROCESS_FIELD_COUNT + 8;
const PROC_FIELD_STDERR_STREAM: usize = JAVA_PROCESS_FIELD_COUNT + 9;

/// Total number of fields on the synthetic Process.
const PROC_FIELD_COUNT: usize = JAVA_PROCESS_FIELD_COUNT + 10;

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
/// `junit-platform-console --help` (gap-anonymous-object-getinputstream.md).
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
    WriteFile {
        path: String,
        append: bool,
    },
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
            message: format!("ProcessBuilder.{op}: descriptor {id} is not backed by a file: {e}"),
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
        false,
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
    //
    // Fallible since 2026-08-10 (JDK-only wave 2, step 3): `cratonvm/synthetic/Process`
    // is a compatibility stand-in for `java.lang.ProcessImpl` — the class this
    // VM hands back from `ProcessBuilder.start()` in compatible mode — so a
    // strict run refuses it and `start()` raises `NoClassDefFoundError` naming
    // it, instead of returning a process object no `java.lang.Process`
    // bytecode can read.
    let proc_class = crate::refused_class(ctx, SYNTHETIC_PROCESS_CLASS, PROC_FIELD_COUNT)?;
    let proc_ref = ctx.alloc_object(proc_class, PROC_FIELD_COUNT);
    ctx.set_field(proc_ref, PROC_FIELD_EXIT, Value::Int(EXIT_NOT_YET));
    ctx.set_field(
        proc_ref,
        PROC_FIELD_STDIN_FD,
        Value::Int(spawned.fds.stdin_fd),
    );
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

/// The OS half of a spawn failure, as the JDK's own spawn native words it
/// (interpreter round i5 wave 101, lane L1): Windows `ProcessImpl_md.c`
/// `win32Error` -- `CreateProcess error=<code>, <FormatMessage text>` with the
/// text's final period and line end dropped (`getErrorString`) -- and the UNIX
/// `ProcessImpl_md.c` `throwIOExceptionImpl` -- since wave 102
/// `Exec failed, error: <errno> (<strerror>) ` (trailing space). The
/// JDK's `ProcessBuilder.start` prefixes `Cannot run program "<prog>"` (and
/// ` (in directory "<dir>")`) itself. A program Rust's Windows `Command`
/// could not resolve on the `PATH` comes back as a custom `NotFound` error
/// with no OS code ("program not found"); `CreateProcess` answers
/// `ERROR_FILE_NOT_FOUND` (2) for it.
fn spawn_failure_detail(err: &std::io::Error) -> String {
    jdk_os_error_detail("CreateProcess", err)
}

/// [`spawn_failure_detail`] for the Win32 call `function` (Windows
/// `win32Error(env, L"<function>")`; the UNIX form names no function).
fn jdk_os_error_detail(function: &str, err: &std::io::Error) -> String {
    let code = err
        .raw_os_error()
        .or_else(|| (err.kind() == std::io::ErrorKind::NotFound).then_some(2));
    let Some(code) = code else {
        return err.to_string();
    };
    #[cfg(windows)]
    {
        // Wave 101 hand-back (Russian-locale host): the system's own text in
        // its default language, as `win32Error`'s `os_error_message`
        // (`FormatMessageW`, language 0) words it; with none, HotSpot's
        // `"%s failed, error=%d"` form.
        match win32_error_text(code) {
            Some(text) => format!("{function} error={code}, {text}"),
            None => format!("{function} failed, error={code}"),
        }
    }
    #[cfg(not(windows))]
    {
        let _ = function;
        let os = std::io::Error::from_raw_os_error(code).to_string();
        let suffix = format!(" (os error {code})");
        let text = os.strip_suffix(suffix.as_str()).unwrap_or(os.as_str()).trim_end();
        // Interpreter round i5 wave 102 hand-back (Linux, OpenJDK 25.0.4,
        // `L1W101ProcessSpawnFailureTexts`): JDK 25 launches through
        // jspawnhelper, and `ProcessImpl_md.c` reports the child's failure
        // through `throwIOExceptionImpl(env, errnum, "Exec failed", NULL)`
        // with `IOE_FORMAT` `"%s, error: %d (%s) %s"` -- the missing internal
        // detail leaves a TRAILING SPACE. Was the pre-JDK-24 `"error=%d, %s"`.
        format!("Exec failed, error: {code} ({text}) ")
    }
}

/// The Win32 system message for `code` in the default language
/// (`FormatMessageW(FORMAT_MESSAGE_FROM_SYSTEM | FORMAT_MESSAGE_IGNORE_INSERTS,
/// .., language 0)`), with the final line end and period dropped, as the
/// JDK's `getErrorString` / `getLastErrorString` (`jni_util_md.c`) return it
/// (interpreter round i5 wave 101 hand-back: Rust's `io::Error` text and the
/// hard-coded English `error=87` texts did not follow a non-English host).
/// `None` when the system has no message for the code.
#[cfg(windows)]
fn win32_error_text(code: i32) -> Option<String> {
    // IDENTICAL to `net_iocp_win.rs`' declaration (`clashing_extern_declarations`).
    #[link(name = "Kernel32")]
    extern "system" {
        fn FormatMessageW(
            flags: u32,
            source: *const core::ffi::c_void,
            message_id: u32,
            language_id: u32,
            buffer: *mut u16,
            size: u32,
            arguments: *const core::ffi::c_void,
        ) -> u32;
    }
    const FORMAT_MESSAGE_FROM_SYSTEM: u32 = 0x1000;
    const FORMAT_MESSAGE_IGNORE_INSERTS: u32 = 0x0200;
    let mut buf = [0u16; 1024];
    // SAFETY: `buf` is a live, writable array of `buf.len()` UTF-16 units and
    // the flags name no insert arguments.
    let len = unsafe {
        FormatMessageW(
            FORMAT_MESSAGE_FROM_SYSTEM | FORMAT_MESSAGE_IGNORE_INSERTS,
            core::ptr::null(),
            code as u32,
            0,
            buf.as_mut_ptr(),
            buf.len() as u32,
            core::ptr::null(),
        )
    } as usize;
    if len == 0 {
        return None;
    }
    let text = String::from_utf16_lossy(&buf[..len.min(buf.len())]);
    // `getErrorString`: drop the final LF, CR and '.'.
    let text = text.strip_suffix('\n').unwrap_or(&text);
    let text = text.strip_suffix('\r').unwrap_or(text);
    let text = text.strip_suffix('.').unwrap_or(text);
    Some(text.to_string())
}

/// The OS half of `ProcessImpl.create`'s refusal of a command line it cannot
/// split (`ERROR_INVALID_PARAMETER`, 87), in the system's language.
#[cfg(windows)]
fn create_process_error_87() -> String {
    jdk_os_error_detail("CreateProcess", &std::io::Error::from_raw_os_error(87))
}

/// `ProcessBuilder.start`'s wrapping of a spawn failure's OS half, for the
/// VM's own `ProcessBuilder.start` stand-in, which the JDK's Java does not
/// wrap (interpreter round i5 wave 101, lane L1).
fn cannot_run_program(program: &str, work_dir: Option<&str>, detail: &str) -> String {
    match work_dir {
        Some(dir) => format!("Cannot run program \"{program}\" (in directory \"{dir}\"): {detail}"),
        None => format!("Cannot run program \"{program}\": {detail}"),
    }
}

/// Spawn a child and register it in the process tables, without building any
/// Java-visible object around it.
///
/// `jdk_wraps`: the caller is a real JDK spawn native (`ProcessImpl.create`,
/// `forkAndExec`), whose `IOException` the JDK's `ProcessBuilder.start`
/// wraps into `Cannot run program ...` itself, so a spawn failure answers
/// only the OS half ([`spawn_failure_detail`]); before wave 101 it answered
/// `ProcessBuilder.start failed: program=..., args=[...]: <Rust error>`, and
/// the empty-program arm a whole `Cannot run program "": ...` the JDK then
/// wrapped a second time. `false` (the VM's own stand-in) wraps here.
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
    jdk_wraps: bool,
) -> Result<SpawnedChild, MethodCallFailed> {
    // Interpreter round i5 wave 101, lane L1: this VM's process tables.
    let t = &tables(ctx);
    if program.is_empty() {
        // HotSpot does NOT pre-validate the program name here. `ProcessBuilder.start`
        // hands `command[0]` straight to `ProcessImpl.create`, the OS refuses it, and
        // the failure surfaces as an `IOException` naming the empty program:
        //
        //   HotSpot 25 (Windows): java.io.IOException: Cannot run program "":
        //                         CreateProcess error=87, <localized>
        //   HotSpot 25 (Unix):    java.io.IOException: Cannot run program "":
        //                         Exec failed, error: 2 (No such file or directory) 
        //
        // We refused with `IllegalArgumentException`, which is not an `IOException`,
        // so a caller writing the JDK's own `catch (IOException)` around a spawn --
        // the only checked exception `start()` declares -- did not catch it and the
        // error escaped as an unchecked throw. Measured on this host:
        //   HotSpot  : java.io.IOException: Cannot run program ""...
        //   CratonVM : java.lang.IllegalArgumentException: ProcessBuilder: empty program
        //
        // Both modes were wrong; this is a HotSpot-parity fix, not a mode-keyed one.
        // regression-suite/src/RJdkProcess.java asserts the TYPE only, because the
        // OS half of the message is localized and the errno differs per platform.
        // The OS half is platform-specific: HotSpot reports whatever the spawn
        // syscall returned. Emitting the Windows text on Unix would be a fresh
        // divergence in place of the old one, so it is selected per platform.
        // Wave 101 hand-back: the system's text, in its language.
        let os_code = if cfg!(windows) { 87 } else { 2 };
        let os_detail =
            jdk_os_error_detail("CreateProcess", &std::io::Error::from_raw_os_error(os_code));
        let message = if jdk_wraps {
            os_detail
        } else {
            cannot_run_program("", work_dir, &os_detail)
        };
        return Err(RuntimeError::IOException { message }.into());
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
    // Interpreter round i4 wave 98, lane L1 (hand-back on w104,
    // `interpreter-i98-L1-processhandle-destroy-of-an-exited-child-answers-true-FIXED-20261130.md`):
    // the JDK's Windows `ProcessImpl_md.c` creates every child with
    // `CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT`, so a console child gets
    // a console of its own (a `conhost.exe` child, which
    // `ProcessHandle.children()` lists: `L1W98ProcessHandleAnswers` measured
    // `children=1` on HotSpot for `ping`) instead of sharing the VM's. Rust
    // adds `CREATE_UNICODE_ENVIRONMENT` itself.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let (stdin_piped, stdout_piped, stderr_piped, merged_reader) = configure_stdio(
        ctx.fd_table(),
        &mut command,
        redirects,
        redirect_error_stream,
    )
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
            if pb_debug_enabled() {
                eprintln!("[PB-SPAWN-FAILED] program={program:?} args={args:?}: {e}");
            }
            let detail = spawn_failure_detail(&e);
            let message = if jdk_wraps {
                detail
            } else {
                cannot_run_program(program, work_dir, &detail)
            };
            return Err(RuntimeError::IOException { message }.into());
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
    let handle = t.next_handle.fetch_add(1, Ordering::Relaxed);
    if pb_debug_enabled() {
        eprintln!(
            "[PB-SPAWNED] handle={} pid={} stdin_fd={} stdout_fd={} stderr_fd={}",
            handle, pid, stdin_fd, stdout_fd, stderr_fd
        );
    }
    t.children.lock().insert(handle, Arc::new(Mutex::new(child)));
    t.exits.lock().insert(
        handle,
        ExitCache {
            pid,
            exit_code: None,
        },
    );
    let gen_of = |fd: i32| {
        if fd < 0 {
            0
        } else {
            ctx.fd_table().generation(fd as FdId).unwrap_or(0)
        }
    };
    let gens = [gen_of(stdin_fd), gen_of(stdout_fd), gen_of(stderr_fd)];
    t.pipes.lock().insert(
        handle,
        PipeFds {
            stdin_fd,
            stdout_fd,
            stderr_fd,
            gens,
        },
    );
    Ok(SpawnedChild {
        handle,
        pid,
        fds: PipeFds {
            stdin_fd,
            stdout_fd,
            stderr_fd,
            gens,
        },
    })
}

/// Wait for the child identified by `handle` to exit.  Blocks the
/// calling thread.  Returns the exit code.  Idempotent — subsequent
/// calls return the cached exit code.
///
/// Once the child has exited, the underlying `Child` is removed from
/// the process table so the OS can reap the zombie.
pub fn wait_for_handle(t: &Arc<ProcessTables>, handle: i64) -> i32 {
    // Fast path: already cached.
    if let Some(cache) = t.exits.lock().get(&handle) {
        if let Some(code) = cache.exit_code {
            return code;
        }
    }
    // Slow path: block on the OS. The child stays in the table — see the note
    // there — so `destroy_handle` can still reach it while we are blocked here.
    // Interpreter round i7 wave 136, lane L1b (`CRATONVM_PROCESS_PIDFD_REAPER`,
    // performance, default off): on Linux, wait on the child's pidfd with no
    // mutex held, so `isAlive()` / `exitValue()` / `destroy()` meanwhile
    // reach the child (`try_lock`) instead of meeting this waiter's lock.
    // `None` (no pidfd, or a descriptor that stopped answering) falls through
    // to `Child::wait` below.
    #[cfg(target_os = "linux")]
    {
        if t.pidfd_reaper {
            if let Some(code) = wait_for_handle_pidfd(t, handle) {
                notify_process_exit(t);
                return code;
            }
        }
    }
    let Some(child) = child_for(t, handle) else {
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

    if let Some(cache) = t.exits.lock().get_mut(&handle) {
        cache.exit_code = Some(code);
    }
    // Interpreter round i2 wave 70, lane L1: wake the timed `waitFor`
    // stand-ins (see [`process_exit_signal`]). Nobody waits on it unless
    // `CRATONVM_PROCESS_EXIT_SIGNAL` is on, so this is a no-op otherwise.
    notify_process_exit(t);
    code
}

/// The exit signal of VM-spawned children (interpreter round i2 wave 70, lane
/// L1, `CRATONVM_PROCESS_EXIT_SIGNAL`, default on since the wave-70 triage;
/// `interpreter-i69-L1-proposal-signal-the-process-condition-at-exit-CLOSED-20261224.md`, step 2
/// in Rust): a generation counter bumped, and a condition variable notified,
/// whenever [`wait_for_handle`] publishes an exit code. It lives beside the
/// process table it describes (the children are the OS process's, not one
/// VM's); it carries no compatibility state.
///
/// Interpreter round i5 wave 101, lane L1: one per VM
/// ([`ProcessTables::exit_signal`]), beside that VM's children.
///
/// Publish one exit to every thread in [`wait_process_exit_signal`].
fn notify_process_exit(t: &Arc<ProcessTables>) {
    let (generation, signal) = &t.exit_signal;
    *generation.lock() += 1;
    signal.notify_all();
}

/// The exit signal's generation: read it BEFORE testing for the exit, and
/// hand it to [`wait_process_exit_signal`], so an exit published between the
/// test and the wait is not lost.
fn process_exit_generation(t: &Arc<ProcessTables>) -> u64 {
    *t.exit_signal.0.lock()
}

/// Block for at most `slice`, or until an exit newer than `seen` is
/// published (any child's: the caller re-tests its own and loops).
fn wait_process_exit_signal(t: &Arc<ProcessTables>, seen: u64, slice: Duration) {
    let (generation, signal) = &t.exit_signal;
    let mut g = generation.lock();
    if *g != seen {
        return;
    }
    let _ = signal.wait_for(&mut g, slice);
}

/// Make sure one OS thread blocks in [`wait_for_handle`] for `handle`, so its
/// exit is published (and [`notify_process_exit`] fires) the moment the child
/// exits, as UNIX `ProcessImpl`'s reaper does with its `signalAll` on
/// HotSpot. One per handle; a plain OS thread (it runs no Java and touches no
/// heap: `wait_for_handle` is pure Rust over the process table). While it is
/// inside `Child::wait`, `try_exit_handle` answers "running" until the code
/// is cached and `destroy_handle` signals by pid -- the same state the JDK's
/// own reaper puts a child in (see `destroy_handle`). `false` if the thread
/// could not be created (the caller keeps polling).
fn ensure_process_exit_reaper(t: &Arc<ProcessTables>, handle: i64) -> bool {
    if !t.reapers.lock().insert(handle) {
        return true;
    }
    // Wave 101: the reaper holds its VM's tables until its child exits.
    let reaper_tables = Arc::clone(t);
    let spawned = std::thread::Builder::new()
        .name(format!("cratonvm-process-exit-{handle}"))
        .spawn(move || {
            let _ = wait_for_handle(&reaper_tables, handle);
        })
        .is_ok();
    if !spawned {
        t.reapers.lock().remove(&handle);
    }
    spawned
}

/// `CRATONVM_PROCESS_EXIT_SIGNAL` (interpreter round i2 wave 70, lane L1;
/// default ON since the wave-70 triage, `=0` off): a VM-spawned child's `waitFor(long, TimeUnit)` stand-in
/// starts an exit reaper and waits on the exit signal instead of sleeping
/// 10 ms slices, so it returns when the child exits (HotSpot: at the reaper's
/// `signalAll`) rather than up to 10 ms later. The 10 ms slice stays as the
/// fallback bound, so a lost signal costs what the poll cost.
fn process_exit_signal_on() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_PROCESS_EXIT_SIGNAL")
}

/// How a waiter for a VM-spawned child blocks for one slice (interpreter round
/// i4 wave 97, lane L1, `interpreter-i97-L1-process-exit-wake-latency-DONE-20261224.md`).
///
/// * `Handle` (Windows, `CRATONVM_PROCESS_EXIT_SIGNAL` on): the waiter blocks
///   in `WaitForSingleObject` on the child's own process handle, as the JDK's
///   `waitForInterruptibly` / `waitForTimeoutInterruptibly` natives do. The
///   kernel wakes it at the exit -- one hop. The reaper route (`Signal`) was
///   two: the reaper's `Child::wait` returned, then its condvar notify woke the
///   waiter, which measured 60-130 us median after the exit
///   (`L1W96ProcessWaitWakes`, HotSpot 0 us).
/// * `Signal` (UNIX, or Windows when the handle cannot be opened): the exit
///   reaper + exit signal ([`ensure_process_exit_reaper`],
///   [`wait_process_exit_signal`]); UNIX `ProcessImpl`'s own shape (a reaper
///   thread `signalAll`s the waiters).
/// * `Sleep` (`CRATONVM_PROCESS_EXIT_SIGNAL=0`, or no reaper thread): plain
///   sleeps; the caller re-tests the exit between slices.
///
/// * `Pidfd` (Linux 5.3+, `CRATONVM_PROCESS_PIDFD_WAIT`, default on since
///   the round-i5 triage; interpreter round i5 wave 102, lane L1): the waiter `poll`s a
///   `pidfd_open` descriptor of the child, which the kernel makes readable at
///   the exit -- one hop, no reaper thread ([`LinuxPidfdWait`]). Falls back
///   to `Signal` where `pidfd_open` fails (`ENOSYS` before 5.3, seccomp).
///
/// Whatever the arm, the caller loops: read [`ExitWait::generation`], test the
/// exit and the interrupt status, then [`ExitWait::wait`] for at most one
/// slice (10 ms bounds how late an interrupt is seen).
enum ExitWait {
    #[cfg(windows)]
    Handle(WinProcessWait),
    #[cfg(target_os = "linux")]
    Pidfd(LinuxPidfdWait),
    /// The exit signal of these tables (wave 101: the waiter's VM's).
    Signal(Arc<ProcessTables>),
    Sleep,
}

impl ExitWait {
    /// The wait for `handle`'s exit; `signal_on` is the caller's reading of
    /// `CRATONVM_PROCESS_EXIT_SIGNAL` (the untimed UNIX-shaped stand-in always
    /// waits on the exit signal).
    fn for_handle(t: &Arc<ProcessTables>, handle: i64, signal_on: bool) -> ExitWait {
        if !signal_on {
            return ExitWait::Sleep;
        }
        #[cfg(windows)]
        {
            if let Some(wait) = WinProcessWait::open(t, handle) {
                return ExitWait::Handle(wait);
            }
        }
        // Wave 102 (lane L1, `CRATONVM_PROCESS_PIDFD_WAIT`): the child's own
        // pidfd instead of a reaper thread and its condvar.
        #[cfg(target_os = "linux")]
        {
            if t.pidfd_wait {
                if let Some(wait) = LinuxPidfdWait::open(t, handle) {
                    return ExitWait::Pidfd(wait);
                }
            }
        }
        if ensure_process_exit_reaper(t, handle) {
            ExitWait::Signal(Arc::clone(t))
        } else {
            ExitWait::Sleep
        }
    }

    /// Read BEFORE testing for the exit (see [`process_exit_generation`]).
    fn generation(&self) -> u64 {
        match self {
            ExitWait::Signal(t) => process_exit_generation(t),
            _ => 0,
        }
    }

    /// Block for at most `slice`, or until the child (`Handle`, `Pidfd`) or
    /// any child (`Signal`) exits. Pure OS waits: legal inside a blocking
    /// region. Answers the child's exit code when the wait object itself
    /// observed (and published) the exit -- `Handle` and `Pidfd` only; the
    /// caller may break out with it under [`direct_wake`] (wave 102), and
    /// otherwise re-tests as before.
    fn wait(&self, handle: i64, seen: u64, slice: Duration) -> Option<i32> {
        #[cfg(not(any(windows, target_os = "linux")))]
        let _ = handle;
        match self {
            #[cfg(windows)]
            ExitWait::Handle(wait) => wait.wait(handle, slice),
            #[cfg(target_os = "linux")]
            ExitWait::Pidfd(wait) => wait.wait(handle, slice),
            ExitWait::Signal(t) => {
                wait_process_exit_signal(t, seen, slice);
                None
            }
            ExitWait::Sleep => {
                std::thread::sleep(slice);
                None
            }
        }
    }
}

/// One slice of a waiter for a VM-spawned child: [`ExitWait::wait`], and
/// on Windows with `CRATONVM_PROCESS_WAIT_INTERRUPT_EVENT` (interpreter round
/// i6 wave 115, lane L1, performance, default OFF; page
/// `interpreter-i114-L1-proposal-windows-process-wait-wakes-on-interrupt-DONE-20261019.md`) the
/// `Handle` arm also waits on the thread's interrupt-wake event
/// ([`NativeContext::enter_interrupt_wake_event`]), so `Thread.interrupt`
/// ends the slice at once, as the JDK's Windows natives'
/// `WaitForMultipleObjects({process, JVM_GetThreadInterruptEvent()})` does,
/// instead of up to 10 ms later. The slice length is unchanged (it still
/// bounds how late an interrupt is seen where the event is not reached, e.g.
/// a mounted virtual thread). The interrupt status is re-tested after the
/// announce: raised in between, the slice returns `None` at once and the
/// caller's own test sees it. Same answers as [`ExitWait::wait`]; legal
/// inside a blocking region (one short lock on each side of the wait).
fn exit_wait_slice(
    ctx: &mut dyn NativeContext,
    t: &ProcessTables,
    exit_wait: &ExitWait,
    handle: i64,
    seen: u64,
    slice: Duration,
) -> Option<i32> {
    #[cfg(windows)]
    {
        if let ExitWait::Handle(wait) = exit_wait {
            if t.interrupt_event {
                if let Some(event) = ctx.enter_interrupt_wake_event() {
                    if ctx.is_interrupted(false) {
                        ctx.leave_interrupt_wake_event(false);
                        return None;
                    }
                    let (code, consumed) = wait.wait_or_event(handle, slice, event);
                    ctx.leave_interrupt_wake_event(consumed);
                    return code;
                }
            }
        }
    }
    #[cfg(not(windows))]
    let _ = (&ctx, t);
    exit_wait.wait(handle, seen, slice)
}

/// Cache `code` as `handle`'s exit code unless one is cached already, notify
/// the exit signal if this published it, and answer the cached code (`None`
/// for a handle with no exit-cache row). Interpreter round i5 wave 102, lane
/// L1: the Linux pidfd wait's publish ([`LinuxPidfdWait`]); the Windows
/// handle wait keeps its own copy (`WinProcessWait::publish_exit`).
#[cfg(target_os = "linux")]
fn publish_exit_code(t: &Arc<ProcessTables>, handle: i64, code: i32) -> Option<i32> {
    let (answer, published) = {
        let mut cache = t.exits.lock();
        match cache.get_mut(&handle) {
            Some(row) => match row.exit_code {
                Some(cached) => (Some(cached), false),
                None => {
                    row.exit_code = Some(code);
                    (Some(code), true)
                }
            },
            None => (None, false),
        }
    };
    if published {
        notify_process_exit(t);
    }
    answer
}

/// `pidfd_open(2)`'s syscall number: 434 in the unified table every Linux
/// architecture has used since 5.1 (spelled out rather than taken from
/// `libc::SYS_pidfd_open`, which not every target's `libc` declares).
#[cfg(target_os = "linux")]
const SYS_PIDFD_OPEN: libc::c_long = 434;

/// A pidfd for `pid` (Linux 5.3+), or `None` (`ENOSYS`, `ESRCH`, a seccomp
/// filter, a pid out of range). Interpreter round i5 wave 102, lane L1.
#[cfg(target_os = "linux")]
fn pidfd_open(pid: i64) -> Option<libc::c_int> {
    if pid <= 0 || pid > libc::pid_t::MAX as i64 {
        return None;
    }
    let flags: libc::c_long = 0;
    // SAFETY: a raw syscall with two scalar arguments; a negative answer is
    // the failure (errno set) and is never used as a descriptor.
    let r = unsafe { libc::syscall(SYS_PIDFD_OPEN, pid as libc::c_long, flags) };
    if r < 0 || r > libc::c_int::MAX as libc::c_long {
        None
    } else {
        Some(r as libc::c_int)
    }
}

/// What one `poll` of a pidfd saw.
#[cfg(target_os = "linux")]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PidfdPoll {
    /// Readable: the process has exited.
    Exited,
    /// The slice passed (or a signal interrupted the poll).
    Running,
    /// `poll` failed for good (the descriptor is unusable).
    Failed,
}

/// `poll` the pidfd `fd` for at most `slice` (rounded UP to whole
/// milliseconds, `poll`'s unit).
#[cfg(target_os = "linux")]
fn pidfd_poll(fd: libc::c_int, slice: Duration) -> PidfdPoll {
    let ms = slice.as_nanos().div_ceil(1_000_000).min(i32::MAX as u128) as libc::c_int;
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one live `pollfd`, count 1.
    let r = unsafe { libc::poll(&mut pfd, 1, ms) };
    if r > 0 {
        if pfd.revents & libc::POLLIN != 0 {
            PidfdPoll::Exited
        } else {
            // POLLERR / POLLNVAL / POLLHUP without POLLIN.
            PidfdPoll::Failed
        }
    } else if r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
        PidfdPoll::Running
    } else {
        PidfdPoll::Failed
    }
}

/// A pidfd on a VM-spawned Linux child, for [`ExitWait::Pidfd`] (interpreter
/// round i5 wave 102, lane L1, `CRATONVM_PROCESS_PIDFD_WAIT`, default off).
///
/// Reaping stays where it was: the waiter reaps through [`try_exit_handle`]
/// (`Child::try_wait`) once the descriptor is readable, so no zombie is left
/// behind. When another thread holds the child's mutex inside `Child::wait`
/// (it will reap), the code is read with `waitid(P_PID, WEXITED | WNOWAIT)`,
/// which leaves the child waitable for that thread, and published.
///
/// Pid reuse: the child cannot have been recycled while it is unreaped, and
/// a child reaped before the open has its code in the exit cache, which the
/// caller's re-test reads every slice whatever this descriptor does. A
/// descriptor that keeps reporting an exit nobody can answer (a recycled pid
/// whose own process ended) is retired after [`PIDFD_MISS_LIMIT`] misses;
/// the waits then sleep their slices, as the `Sleep` arm does.
#[cfg(target_os = "linux")]
struct LinuxPidfdWait {
    fd: libc::c_int,
    pid: i64,
    _child: Arc<Mutex<Child>>,
    tables: Arc<ProcessTables>,
    misses: std::sync::atomic::AtomicU32,
}

/// See [`LinuxPidfdWait`].
#[cfg(target_os = "linux")]
const PIDFD_MISS_LIMIT: u32 = 1000;

#[cfg(target_os = "linux")]
impl LinuxPidfdWait {
    fn open(t: &Arc<ProcessTables>, handle: i64) -> Option<LinuxPidfdWait> {
        let child = child_for(t, handle)?;
        let pid = pid_for_handle(t, handle);
        let fd = pidfd_open(pid)?;
        Some(LinuxPidfdWait {
            fd,
            pid,
            _child: child,
            tables: Arc::clone(t),
            misses: std::sync::atomic::AtomicU32::new(0),
        })
    }

    /// Wait for at most `slice`; the exit code once the child has exited and
    /// it is known (reaped here, or read without reaping and published).
    fn wait(&self, handle: i64, slice: Duration) -> Option<i32> {
        use std::sync::atomic::Ordering::Relaxed;
        if self.misses.load(Relaxed) >= PIDFD_MISS_LIMIT {
            std::thread::sleep(slice);
            return None;
        }
        match pidfd_poll(self.fd, slice) {
            PidfdPoll::Running => return None,
            PidfdPoll::Failed => {
                self.misses.store(PIDFD_MISS_LIMIT, Relaxed);
                return None;
            }
            PidfdPoll::Exited => {}
        }
        if pb_debug_enabled() {
            eprintln!(
                "[PB-WAKE] handle={handle} pidfd t_us={}",
                pb_trace_epoch().elapsed().as_micros()
            );
        }
        if let Some(code) = try_exit_handle(&self.tables, handle) {
            return Some(code);
        }
        if let Some(code) = self.exited_code(handle) {
            return Some(code);
        }
        // Readable, but the code is in nobody's hands yet: a `Child::wait`
        // holder reaped it and is about to publish (the exit signal), or the
        // pid is not our child's any more. Let the holder run.
        self.misses.fetch_add(1, Relaxed);
        std::thread::yield_now();
        None
    }

    /// The child's status read without reaping it (`WNOWAIT`), published.
    fn exited_code(&self, handle: i64) -> Option<i32> {
        const CLD_EXITED: libc::c_int = 1;
        const CLD_KILLED: libc::c_int = 2;
        const CLD_DUMPED: libc::c_int = 3;
        // SAFETY: an all-zero `siginfo_t` is a valid out-parameter.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: `info` is a live local; scalars otherwise.
        let r = unsafe {
            libc::waitid(
                libc::P_PID,
                self.pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOWAIT | libc::WNOHANG,
            )
        };
        // SAFETY: `waitid` filled `info` (or left it zeroed: `si_pid` 0).
        if r != 0 || unsafe { info.si_pid() } == 0 {
            return None;
        }
        // SAFETY: as above; `si_status` is the exit status or the signal.
        let status = unsafe { info.si_status() };
        let code = match info.si_code {
            CLD_EXITED => status,
            // `wait_for_handle`'s rule for a signal death: 128 + signal.
            CLD_KILLED | CLD_DUMPED => 128 + status,
            _ => return None,
        };
        publish_exit_code(&self.tables, handle, code)
    }
}

#[cfg(target_os = "linux")]
impl LinuxPidfdWait {
    /// Has this descriptor stopped answering ([`PIDFD_MISS_LIMIT`] misses, or
    /// a failed `poll`)? Its waits only sleep then (interpreter round i7 wave
    /// 136, lane L1b: [`wait_for_handle_pidfd`] falls back to `Child::wait`).
    fn retired(&self) -> bool {
        self.misses.load(std::sync::atomic::Ordering::Relaxed) >= PIDFD_MISS_LIMIT
    }
}

/// How long one `poll` of [`wait_for_handle_pidfd`] lasts. The poll wakes at
/// the exit whatever this is; the slice only bounds how often the loop
/// re-reads the exit cache (an exit another waiter published).
#[cfg(target_os = "linux")]
const PIDFD_REAPER_SLICE: Duration = Duration::from_millis(500);

/// [`wait_for_handle`]'s Linux pidfd wait (interpreter round i7 wave 136,
/// lane L1b, `CRATONVM_PROCESS_PIDFD_REAPER`, performance, default off; item 1
/// of `docs/feature-designs/jdk-only-mode.md`):
/// `poll` the child's pidfd with no lock held until it is readable, then reap
/// it once through [`LinuxPidfdWait::wait`] (`try_exit_handle`'s `try_wait`,
/// or the `WNOWAIT` read when another thread holds the child in
/// `Child::wait`). The child's mutex is never held across the wait, so
/// `isAlive()` meanwhile reaches `try_wait` instead of a failed `try_lock`
/// ("running" answered from the lock, not the OS), and `destroy()` finds the
/// child. `None` when no pidfd can be opened (`ENOSYS`, seccomp, an unknown
/// handle) or the descriptor retires: the caller then takes `Child::wait`.
/// The exit code is cached in the table by the reap (or the publish); the
/// caller notifies the exit signal. Pure OS waits: legal inside a blocking
/// region.
#[cfg(target_os = "linux")]
fn wait_for_handle_pidfd(t: &Arc<ProcessTables>, handle: i64) -> Option<i32> {
    let wait = LinuxPidfdWait::open(t, handle)?;
    loop {
        if let Some(code) = t.exits.lock().get(&handle).and_then(|c| c.exit_code) {
            return Some(code);
        }
        if let Some(code) = wait.wait(handle, PIDFD_REAPER_SLICE) {
            return Some(code);
        }
        if wait.retired() {
            return None;
        }
    }
}

#[cfg(target_os = "linux")]
impl Drop for LinuxPidfdWait {
    fn drop(&mut self) {
        // SAFETY: `self.fd` is a descriptor owned by `self`, closed once.
        unsafe {
            libc::close(self.fd);
        }
    }
}

/// A synchronisable handle on a VM-spawned Windows child, for [`ExitWait`].
///
/// Opened by pid with `SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION`
/// rather than borrowed from the `Child` (whose mutex a `Child::wait` reaper
/// may hold). The pid cannot have been recycled: `_child` keeps the table's
/// `Child` -- and so its own open process handle -- alive for as long as this
/// value lives, and Windows does not reuse a pid while a handle is open.
#[cfg(windows)]
struct WinProcessWait {
    raw: *mut std::ffi::c_void,
    _child: Arc<Mutex<Child>>,
    /// The tables the child is in (wave 101), for the exit it publishes.
    tables: Arc<ProcessTables>,
}

#[cfg(windows)]
mod win_process_wait_ffi {
    use std::ffi::c_void;
    // `OpenProcess`, `GetExitCodeProcess` and `CloseHandle` are IDENTICAL to
    // the other declarations in this file (`clashing_extern_declarations`).
    pub type Handle = *mut c_void;
    pub type Bool = i32;
    pub const SYNCHRONIZE: u32 = 0x0010_0000;
    pub const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    pub const WAIT_OBJECT_0: u32 = 0;

    #[link(name = "Kernel32")]
    extern "system" {
        pub fn OpenProcess(desired_access: u32, inherit_handle: Bool, process_id: u32) -> Handle;
        pub fn GetExitCodeProcess(h_process: Handle, lp_exit_code: *mut u32) -> Bool;
        pub fn WaitForSingleObject(h_handle: Handle, dw_milliseconds: u32) -> u32;
        pub fn CloseHandle(h_object: Handle) -> Bool;
        // Interpreter round i6 wave 115, lane L1
        // (`CRATONVM_PROCESS_WAIT_INTERRUPT_EVENT`): the process handle and
        // the thread's interrupt-wake event in one wait.
        pub fn WaitForMultipleObjects(
            n_count: u32,
            lp_handles: *const Handle,
            b_wait_all: Bool,
            dw_milliseconds: u32,
        ) -> u32;
    }
}

#[cfg(windows)]
impl WinProcessWait {
    fn open(t: &Arc<ProcessTables>, handle: i64) -> Option<WinProcessWait> {
        use win_process_wait_ffi::*;
        let child = child_for(t, handle)?;
        let pid = pid_for_handle(t, handle);
        if pid <= 0 || pid > u32::MAX as i64 {
            return None;
        }
        // SAFETY: scalars in, a handle out; null is the documented failure
        // and is never used.
        let raw = unsafe {
            OpenProcess(
                SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
                0,
                pid as u32,
            )
        };
        if raw.is_null() {
            return None;
        }
        Some(WinProcessWait {
            raw,
            _child: child,
            tables: Arc::clone(t),
        })
    }

    /// Wait on the process handle for at most `slice` (rounded UP to whole
    /// milliseconds, the unit of `WaitForSingleObject` and of the JDK's own
    /// `waitForTimeoutInterruptibly`). When the child has exited and
    /// [`try_exit_handle`] cannot reap it (a `Child::wait` reaper holds its
    /// mutex and has not published yet), publish the exit code read from the
    /// handle, so the caller's re-test answers at once instead of spinning
    /// until the reaper is scheduled. Answers the published (cached) code
    /// when the handle was signalled (wave 102: see [`direct_wake`]).
    fn wait(&self, handle: i64, slice: Duration) -> Option<i32> {
        use win_process_wait_ffi::*;
        let nanos = slice.as_nanos();
        let ms = nanos.div_ceil(1_000_000).min(u32::MAX as u128 - 1) as u32;
        // SAFETY: `self.raw` is an open handle owned by `self`.
        let r = unsafe { WaitForSingleObject(self.raw, ms) };
        if r == WAIT_OBJECT_0 {
            if pb_debug_enabled() {
                eprintln!(
                    "[PB-WAKE] handle={handle} t_us={}",
                    pb_trace_epoch().elapsed().as_micros()
                );
            }
            // Publish straight from the handle (wave 97 follow-up): the
            // caller's re-test then answers from the cache whoever holds the
            // child's mutex, instead of losing a `try_lock` race against a
            // waiter woken by the same exit and looping once more.
            return self.publish_exit(handle);
        }
        None
    }

    /// [`Self::wait`] that also ends when `event` (the waiter's
    /// interrupt-wake event, `NativeContext::enter_interrupt_wake_event`) is
    /// signalled: `WaitForMultipleObjects({process, event})`, the JDK's
    /// `waitForInterruptibly` / `waitForTimeoutInterruptibly` shape with
    /// `JVM_GetThreadInterruptEvent()` (interpreter round i6 wave 115, lane
    /// L1, `CRATONVM_PROCESS_WAIT_INTERRUPT_EVENT`). Answers the published
    /// exit code as [`Self::wait`] does, and whether the wait returned ON the
    /// event (consuming its auto-reset signal). The process handle is index
    /// 0, so when both are signalled the exit wins and the event's signal is
    /// left for the caller's leave to reset.
    fn wait_or_event(&self, handle: i64, slice: Duration, event: isize) -> (Option<i32>, bool) {
        use win_process_wait_ffi::*;
        let nanos = slice.as_nanos();
        let ms = nanos.div_ceil(1_000_000).min(u32::MAX as u128 - 1) as u32;
        let handles: [Handle; 2] = [self.raw, event as Handle];
        // SAFETY: `self.raw` is an open handle owned by `self`; `event` is the
        // calling thread's live wake event (announced, so it outlives the
        // wait); the array has the two elements the count says.
        let r = unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, ms) };
        if r == WAIT_OBJECT_0 {
            if pb_debug_enabled() {
                eprintln!(
                    "[PB-WAKE] handle={handle} t_us={}",
                    pb_trace_epoch().elapsed().as_micros()
                );
            }
            return (self.publish_exit(handle), false);
        }
        (None, r == WAIT_OBJECT_0 + 1)
    }

    /// The child's exit code if it has exited (a zero-timeout wait on the
    /// handle), published to the exit cache.
    fn exited_code(&self, handle: i64) -> Option<i32> {
        use win_process_wait_ffi::*;
        // SAFETY: `self.raw` is an open handle owned by `self`.
        if unsafe { WaitForSingleObject(self.raw, 0) } != WAIT_OBJECT_0 {
            return None;
        }
        self.publish_exit(handle)
    }

    /// Cache the exit code of the (exited) child unless one is cached
    /// already, notify the exit signal if this published it, and return the
    /// cached code.
    fn publish_exit(&self, handle: i64) -> Option<i32> {
        use win_process_wait_ffi::*;
        let mut code: u32 = 0;
        // SAFETY: `self.raw` is an open handle owned by `self`; `code` is a
        // live local.
        if unsafe { GetExitCodeProcess(self.raw, &mut code) } == 0 {
            return None;
        }
        let (answer, published) = {
            let mut cache = self.tables.exits.lock();
            match cache.get_mut(&handle) {
                Some(row) => match row.exit_code {
                    Some(cached) => (Some(cached), false),
                    None => {
                        // `ExitStatus::code()` on Windows is the DWORD as
                        // `i32`, so this is the value `Child::wait` caches too.
                        row.exit_code = Some(code as i32);
                        (Some(code as i32), true)
                    }
                },
                None => (None, false),
            }
        };
        if published {
            notify_process_exit(&self.tables);
        }
        answer
    }
}

/// The epoch of the `CRATONVM_DBG_PB` wake traces (`[PB-WAKE]`,
/// `[PB-LEAVE]`), so the microsecond stamps of different threads compare.
fn pb_trace_epoch() -> Instant {
    static E: OnceLock<Instant> = OnceLock::new();
    *E.get_or_init(Instant::now)
}

/// `CRATONVM_DBG_PB`: a waiter's blocking region has ended (pairs with the
/// `[PB-WAKE]` of the same handle; the gap is the region leave).
fn pb_trace_leave(handle: i64, site: &str) {
    if pb_debug_enabled() {
        eprintln!(
            "[PB-LEAVE] handle={handle} site={site} t_us={}",
            pb_trace_epoch().elapsed().as_micros()
        );
    }
}

/// `CRATONVM_DBG_PB`: a thread blocked in [`wait_for_handle`] (the `onExit()`
/// reapers) has returned from its OS wait (interpreter round i5 wave 106,
/// lane L1; pairs with the [`pb_trace_leave`] of the same site).
fn pb_trace_wake_at(handle: i64, site: &str) {
    if pb_debug_enabled() {
        eprintln!(
            "[PB-WAKE] handle={handle} site={site} t_us={}",
            pb_trace_epoch().elapsed().as_micros()
        );
    }
}

#[cfg(windows)]
impl Drop for WinProcessWait {
    fn drop(&mut self) {
        // SAFETY: `self.raw` is an open handle owned by `self`, closed once.
        unsafe {
            win_process_wait_ffi::CloseHandle(self.raw);
        }
    }
}

/// Non-blocking exit-status probe.  Returns `Some(code)` if the child
/// has already exited (either observed earlier or reaped now); returns
/// `None` if the child is still running.
///
/// This powers `Process.isAlive()` and `ProcessHandleImpl.isAlive0()`.
pub fn try_exit_handle(t: &Arc<ProcessTables>, handle: i64) -> Option<i32> {
    // Cached?
    if let Some(cache) = t.exits.lock().get(&handle) {
        if let Some(code) = cache.exit_code {
            if pb_debug_enabled() {
                eprintln!("[PB-TRY-CACHED] handle={handle} code={code}");
            }
            return Some(code);
        }
    }
    // `try_wait` returns Ok(None) while still running, Ok(Some(status))
    // after exit.
    let child = child_for(t, handle)?;
    // A failed `try_lock` means a waiter is inside `Child::wait()`, i.e. the
    // child is running. Report that rather than blocking — this native backs
    // `isAlive()`, which must not stall behind a `waitFor()` on another thread.
    //
    // Interpreter round i4 wave 97, lane L1: on Windows the holder is no
    // longer only a `Child::wait` reaper -- a waiter woken by the same exit
    // may be inside `try_wait` -- and either way "running" can be stale by
    // the reaper's scheduling delay. Ask the OS through a handle of our own
    // (`WinProcessWait`): a zero-timeout wait, and the code from
    // `GetExitCodeProcess`, published to the cache.
    let Some(mut entry) = child.try_lock() else {
        #[cfg(windows)]
        {
            return WinProcessWait::open(t, handle)?.exited_code(handle);
        }
        #[cfg(not(windows))]
        {
            return None;
        }
    };
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
            if let Some(cache) = t.exits.lock().get_mut(&handle) {
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
pub fn destroy_handle(t: &Arc<ProcessTables>, handle: i64, force: bool) -> bool {
    let Some(child) = child_for(t, handle) else {
        return false;
    };
    // Interpreter round i4 wave 98, lane L1
    // (`interpreter-i98-L1-processhandle-destroy-of-an-exited-child-answers-true-FIXED-20261130.md`):
    // an exited child cannot be destroyed. HotSpot's `destroy0` answers
    // `false` for it (Windows: `TerminateProcess` fails with
    // ERROR_ACCESS_DENIED on an exited process; UNIX: `kill` of a reaped pid
    // fails), but Rust's `Child::kill` returns `Ok(())` for a child that has
    // already exited, so `ProcessHandle.destroy()` after `waitFor()` read
    // `true`.
    if try_exit_handle(t, handle).is_some() {
        return false;
    }
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
    signal_pid(pid_for_handle(t, handle), force)
}

/// Is `pid` still the process the caller's `startTime` came from?
///
/// The guard against acting on a recycled pid. `STARTTIME_ANY` (0) means the
/// caller never learned one and is trusted — the JDK's native makes the same
/// allowance, and without it every handle minted before start times were
/// reported would stop working. An unreadable `/proc` entry is treated as a
/// match for the same reason: unknown is not disagreement.
fn start_time_matches(pid: i64, start_time: i64) -> bool {
    if start_time == PROCESS_STARTTIME_ANY {
        return true;
    }
    match os_process_start_time(pid) {
        Some(actual) => actual == start_time,
        None => true,
    }
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

/// Send a termination signal straight to a pid — Win32.
///
/// The flat `false` this replaces gave as its reason that "the case that needs
/// it — the real `ProcessImpl` reaper holding the child — is Linux-only, since
/// `forkAndExec` is". That stopped being true the moment `--jdk-only` started
/// running the real Windows `ProcessImpl`: its constructor ends in
/// `ProcessHandleImpl.completion(pid, true)`, which parks a reaper thread inside
/// `Child::wait()` for EVERY child, so [`destroy_handle`]'s `try_lock` always
/// fails on this platform and the pid route is the only one left. `destroy()` /
/// `destroyForcibly()` therefore terminated nothing at all, and
/// `RJdkProcess.java:206` (`destroyForcibly did not terminate the child`) is
/// what measures it.
///
/// `OpenProcess(PROCESS_TERMINATE)` + `TerminateProcess` is what HotSpot's own
/// `ProcessImpl_md.c` and `ProcessHandleImpl_md.c` do. `force` is not a
/// distinction Windows offers — there is no catchable termination — so both
/// spellings terminate, which is precisely what
/// `ProcessHandle.supportsNormalTermination()` already reports (`false` off
/// POSIX) rather than a shortfall this hides.
///
/// Exit code 1 is the value HotSpot passes, so a killed child reads back the
/// same status it would there.
#[cfg(windows)]
fn signal_pid(pid: i64, _force: bool) -> bool {
    use std::ffi::c_void;
    // Signatures are IDENTICAL to `win_liveness_and_start_time`'s and
    // `pipe.rs`'s declarations so the `clashing_extern_declarations` deny-lint
    // does not fire. (This used to name `foreign_pid_is_alive`, whose Windows
    // arm was folded into `win_liveness_and_start_time` and no longer exists —
    // a dangling name in a lint rationale is how the next reader concludes the
    // rationale is obsolete.)
    type Handle = *mut c_void;
    type Bool = i32;
    const PROCESS_TERMINATE: u32 = 0x0001;

    #[link(name = "Kernel32")]
    extern "system" {
        fn OpenProcess(desired_access: u32, inherit_handle: Bool, process_id: u32) -> Handle;
        fn TerminateProcess(h_process: Handle, u_exit_code: u32) -> Bool;
        fn CloseHandle(h_object: Handle) -> Bool;
    }

    // A Windows process id is a DWORD; anything outside that range names no
    // process. pid 0 is the System Idle Process and must never be signalled.
    if pid <= 0 || pid > u32::MAX as i64 {
        return false;
    }
    // SAFETY: `OpenProcess` takes only scalars and `TerminateProcess` only a
    // handle plus a scalar; neither touches caller memory. The handle is closed
    // on every path out, and a null handle is the documented failure return and
    // is never passed on.
    unsafe {
        let h = OpenProcess(PROCESS_TERMINATE, 0, pid as u32);
        if h.is_null() {
            return false;
        }
        let ok = TerminateProcess(h, 1) != 0;
        CloseHandle(h);
        ok
    }
}

#[cfg(not(any(unix, windows)))]
fn signal_pid(_pid: i64, _force: bool) -> bool {
    // No termination primitive is wired up for this target. `false` is a legal
    // answer ("could not be destroyed") and is the honest one here.
    false
}

/// Look up the captured pid for a live-or-recently-exited handle.
pub fn pid_for_handle(t: &Arc<ProcessTables>, handle: i64) -> i64 {
    t.exits
        .lock()
        .get(&handle)
        .map(|c| c.pid)
        .unwrap_or(-1)
}

// ---------------------------------------------------------------------------
// Helper utilities for bytecode-land interaction
// ---------------------------------------------------------------------------

/// Read a Java `String[]` into `Vec<String>`.
///
/// Currently uncalled: its one caller was `ProcessImpl.create`'s envblock
/// decoder, which read a `String[]` the JDK 25 image does not pass — see the
/// descriptor note on [`native_process_impl_create`]. Kept because it is the
/// generic reader every future `String[]` argument on this surface wants, and
/// deleting it would only invite a hand-rolled copy.
#[allow(dead_code)]
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
///
/// `allow(dead_code)`: its only non-test caller is the Windows-only
/// `ProcessImpl.create`, and the unit tests below exercise it on every platform.
#[allow(dead_code)]
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
    // GC: `type()` runs Java, and every branch below reads `redirect_obj`
    // again — `file()`, and for WRITE/APPEND `append()` as well.
    let pin = ctx.pin_native_root(redirect_obj);
    let type_obj = match ctx.invoke_virtual(
        redirect_obj,
        "type",
        "()Ljava/lang/ProcessBuilder$Redirect$Type;",
        &[],
    ) {
        Ok(Some(Value::Object(Some(o)))) => o,
        _ => {
            ctx.unpin_native_roots(pin);
            return StdioRedirect::Pipe;
        }
    };
    let ordinal = enum_ordinal(ctx, type_obj).unwrap_or(0);
    let redirect_obj = ctx.read_native_pin(pin, redirect_obj);
    let out = match ordinal {
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
                // `file` must survive `append()`, which runs Java too.
                let file_pin = ctx.pin_native_root(file);
                // gc-common w19-g: the receiver is re-read after `file()`
                // (Java); `append()` was dispatched on the pre-call address.
                let redirect_now = ctx.read_native_pin(pin, redirect_obj);
                let append = ordinal == 4
                    || matches!(
                        ctx.invoke_virtual(redirect_now, "append", "()Z", &[]),
                        Ok(Some(Value::Int(v))) if v != 0
                    );
                let file = ctx.read_native_pin(file_pin, file);
                file_path_of(ctx, file)
                    .map(|path| StdioRedirect::WriteFile { path, append })
                    .unwrap_or(StdioRedirect::Null)
            }
            _ => StdioRedirect::Null,
        },
        _ => StdioRedirect::Pipe,
    };
    ctx.unpin_native_roots(pin);
    out
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
    // GC: `read_process_redirect` runs the Redirect's Java (`type()`, `file()`,
    // `append()`), and the array is indexed again for each of the three slots.
    let arr_pin = ctx.pin_native_root(arr);
    let mut arr = arr;
    if len > 0 {
        if let Value::Object(Some(r)) = ctx.get_array_element(arr, 0) {
            redirects.stdin = read_process_redirect(ctx, r);
        }
    }
    if len > 1 {
        arr = ctx.read_native_pin(arr_pin, arr);
        if let Value::Object(Some(r)) = ctx.get_array_element(arr, 1) {
            redirects.stdout = read_process_redirect(ctx, r);
        }
    }
    if len > 2 {
        arr = ctx.read_native_pin(arr_pin, arr);
        if let Value::Object(Some(r)) = ctx.get_array_element(arr, 2) {
            redirects.stderr = read_process_redirect(ctx, r);
        }
    }
    ctx.unpin_native_roots(arr_pin);
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
    // GC: every `invoke_virtual` in this loop runs the map's Java, so the
    // iterator does not survive its own `hasNext`/`next` pair and an entry does
    // not survive `getKey()` before `getValue()` reads it again. The pin is
    // released on every exit — `read_process_environment` runs once per
    // `ProcessBuilder.start()`, but the loop below is bounded at 100_000.
    let iter_pin = ctx.pin_native_root(iter);
    let mut iter = iter;
    let mut out = Vec::new();
    for _ in 0..100_000 {
        iter = ctx.read_native_pin(iter_pin, iter);
        let has_next = matches!(
            ctx.invoke_virtual(iter, "hasNext", "()Z", &[]),
            Ok(Some(Value::Int(v))) if v != 0
        );
        if !has_next {
            break;
        }
        iter = ctx.read_native_pin(iter_pin, iter);
        let entry = match ctx.invoke_virtual(iter, "next", "()Ljava/lang/Object;", &[]) {
            Ok(Some(Value::Object(Some(o)))) => o,
            _ => break,
        };
        let entry_pin = ctx.pin_native_root(entry);
        let key = match ctx.invoke_virtual(entry, "getKey", "()Ljava/lang/Object;", &[]) {
            Ok(Some(Value::Object(Some(o)))) => object_to_string(ctx, o),
            _ => None,
        };
        let entry = ctx.read_native_pin(entry_pin, entry);
        ctx.unpin_native_roots(entry_pin);
        let value = match ctx.invoke_virtual(entry, "getValue", "()Ljava/lang/Object;", &[]) {
            Ok(Some(Value::Object(Some(o)))) => object_to_string(ctx, o),
            _ => None,
        };
        iter = ctx.read_native_pin(iter_pin, iter);
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
/// # The census, taken rather than asserted (2026-08-12, W7-46)
///
/// The paragraph above said "every concrete `java.lang.Process` native now
/// asks" and then named five. That was a list, not a census, and it was one
/// short — the guard's own comment outlived the sweep that wrote it. Re-taken
/// against the image with `javap -p java.lang.Process` and
/// `javap -p java.lang.ProcessImpl` (JDK 25.0.3, Windows), because what matters
/// is not "is this method concrete" but "is it concrete AND unoverridden by the
/// class strict mode actually instantiates":
///
/// | registered on `java/lang/Process` | on the image | `ProcessImpl` overrides it? | needs the guard |
/// | --- | --- | --- | --- |
/// | `waitFor()I`, `exitValue`, `destroy`, `getInputStream`, `getErrorStream`, `getOutputStream` | abstract | yes (must) | no — dispatch never walks up |
/// | `isAlive`, `pid`, `toHandle`, `destroyForcibly`, `waitFor(JLjava/util/concurrent/TimeUnit;)Z` | concrete | yes | **yes**, and all five have it |
/// | `onExit` | concrete | **yes** | no — same reason as the abstract rows |
/// | `descendants` | concrete | **NO** | **yes**, and it did not have one until W7-46 |
///
/// `descendants()` is the whole point of the table: it is the only triple this
/// VM registers on `java/lang/Process` that is both reachable from a real
/// `java.lang.ProcessImpl` receiver and unguarded, and what it answered there
/// was an empty stream. `info()` and `children()` are equally unoverridden but
/// this VM does not register them, so they reach the JDK's own bytecode.
///
/// See `process-natives-answer-for-user-subclasses-FIXED-20260806.md`
/// and `probes/UserProcessInterceptProbe.java`.
fn is_vm_process(ctx: &mut dyn NativeContext, this: ObjectRef) -> bool {
    let cid = ctx.class_id_of_object(this);
    ctx.class_name_arc_of_id(cid).as_deref() == Some(SYNTHETIC_PROCESS_CLASS)
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
        // gc-common w19-g: the fallback ordinal is read BEFORE `toMillis`
        // (Java, a GC point); it was read after it, off the pre-call `unit`.
        let ordinal = match ctx.get_field_by_name(unit, "ordinal") {
            Value::Int(o) => o,
            _ => ctx.get_field(unit, 0).as_int().unwrap_or(2),
        };
        if let Ok(Some(Value::Long(ms))) =
            ctx.invoke_virtual(unit, "toMillis", "(J)J", &[Value::Long(value)])
        {
            return ms.max(0);
        }

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

/// `java.lang.ProcessImpl.create(String cmdstr, String envblock, String dir,
/// long[] stdHandles, boolean redirectErrorStream) -> long`
///
/// The spawn entry point of the REAL JDK's **Windows** `ProcessImpl`, and
/// therefore of every `ProcessBuilder.start()` / `Runtime.exec` on this platform
/// once the VM stops shadowing `start()` — the exact counterpart of
/// [`native_process_impl_fork_and_exec`] on Linux.
///
/// # The descriptor
///
/// This was registered as
/// `(Ljava/lang/String;[Ljava/lang/String;Ljava/lang/String;[JZ)J` — a
/// `String[]` envblock. JDK 25's image declares
///
/// ```text
/// private static synchronized native long create(
///     java.lang.String, java.lang.String, java.lang.String, long[], boolean);
///   descriptor: (Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;[JZ)J
/// ```
///
/// so the registration could never bind, and the `--jdk-only` census recorded
/// `ProcessImpl.create` as "dead or shadowing" for exactly that reason. `arg1`
/// is a single **environment block** String, not an array: NUL-separated
/// `KEY=VALUE` entries, `null` when the child inherits this process's
/// environment (`ProcessEnvironment.toEnvironmentBlock(null)` is `null`).
///
/// Entries whose key begins with `=` are Windows' per-drive current-directory
/// pseudo-variables (`=C:=C:\some\dir`). They are dropped rather than forwarded:
/// they are not environment variables any program reads by name, and
/// `Command::env` has no way to express one. The observable cost is that a child
/// does not inherit a per-drive CWD it was never given explicitly.
///
/// # The `stdHandles` in/out contract
///
/// Identical in shape to the Linux `int[] fds` contract documented on
/// [`native_process_impl_fork_and_exec`], and load-bearing for the same reason.
/// HotSpot's `processCreate` sets `handles[i] = -1` when the caller supplied its
/// own handle and to the parent-side pipe end when it created one, and the
/// constructor then reads each slot back: `-1` builds a
/// `ProcessBuilder.Null{Input,Output}Stream`, anything else is wrapped in a
/// `FileDescriptor` via `fdAccess.setHandle`. Ignoring the array — which this
/// native did, always piping all three streams and never writing back — hands
/// the caller a live child whose streams are attached to nothing it asked for.
///
/// The values are **`FdTable` ids, not OS handles**: the JDK obtains them with
/// `fdAccess.getHandle(fd)`, which reads `FileDescriptor.handle`, and in this VM
/// that slot holds an `FdTable` id (`FileDescriptor.getHandle(int)` answers
/// `0`/`1`/`2` for the standard streams, which is what `Redirect.INHERIT`
/// encodes, and `fis_get_fd`/`fos_get_fd` resolve the rest).
#[cfg(windows)]
fn native_process_impl_create(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // args[0] = cmdstr (the pre-built, quoted Windows command line),
    // args[1] = envblock (String of NUL-separated KEY=VALUE, nullable),
    // args[2] = working dir (String, nullable),
    // args[3] = stdHandles (long[], in/out),
    // args[4] = redirectErrorStream (boolean).
    // A command line this native cannot turn into a program is an `IOException`,
    // NOT a handle of 0.
    //
    // Both arms below used to `return Ok(Some(Value::Long(0)))`, and the JDK
    // caller has no null-handle check to catch it: `ProcessImpl.<init>` stores
    // whatever `create` returned into its `handle` field and carries straight on
    // to `getProcessId0(handle)`. So `ProcessBuilder.start()` returned a live
    // `java.lang.ProcessImpl` naming no process — `pid()` 0, `isAlive()` false,
    // `waitFor()` -1 — where HotSpot 25 raises
    // `IOException: Cannot run program "": CreateProcess error=87`.
    //
    // This is the same defect species W6-10 finding 4 removed from the
    // enumeration primitives' error paths, entering here through the ARGUMENT
    // path of code W3-6 added; the species does not stay fixed by fixing one
    // path. The real `create` is declared `throws IOException` on the image
    // (`javap -p -s java.lang.ProcessImpl`) and reports `CreateProcess error=`
    // plus the Win32 code, so an `IOException` is the answer the caller's own
    // `catch` is already written for — `ProcessBuilder.start()` wraps it into
    // `Cannot run program ...` and rethrows.
    let cmd_line = match args.first() {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => {
            // Interpreter round i5 wave 101, lane L1: the OS half only;
            // `ProcessBuilder.start` adds `Cannot run program ...`.
            return Err(RuntimeError::IOException {
                message: create_process_error_87(),
            }
            .into())
        }
    };
    // The JDK passes a single pre-built command string here; on Windows
    // its arguments are double-quoted whenever they contain spaces (e.g.
    // `"C:\Program Files\Java\bin\java.exe" -cp "a b"`). Naive whitespace
    // splitting would tear quoted programs/paths apart, so tokenize with
    // double-quote handling instead.
    let parts: Vec<String> = tokenize_command_line(&cmd_line);
    if parts.is_empty() {
        // The REACHABLE arm: `new ProcessBuilder("").start()` reaches
        // `ProcessImpl.createCommandLine` with an empty program name, so
        // `cmdstr` is empty and this tokenizes to nothing.
        // Wave 101 (lane L1): the OS half only, as above; the JDK's
        // `ProcessBuilder.start` wrapped this message a second time.
        return Err(RuntimeError::IOException {
            message: create_process_error_87(),
        }
        .into());
    }
    let program = &parts[0];
    let rest = &parts[1..];

    let env_vars: Option<Vec<(String, String)>> = match args.get(1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).map(|block| parse_env_block(&block)),
        _ => None,
    };
    let work_dir = match args.get(2) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s),
        _ => None,
    };

    // args[4] = redirectErrorStream (boolean, passed as Int by the JDK ProcessImpl).
    let redirect_err = matches!(args.get(4), Some(Value::Int(v)) if *v != 0);

    // --- Decode the requested stdio shape from `stdHandles` ----------------
    let handles_array = match args.get(3) {
        Some(Value::Object(Some(arr))) => Some(*arr),
        _ => None,
    };
    let mut requested = [-1i64; 3];
    if let Some(arr) = handles_array {
        let len = ctx.array_length(arr).min(3);
        for (slot, want) in requested.iter_mut().enumerate().take(len) {
            if let Value::Long(v) = ctx.get_array_element(arr, slot) {
                *want = v;
            }
        }
    }
    // Interpreter round i7 (orchestrator, merged tree): since JIT r23 wave 2
    // (`CRATONVM_FD_FIELDS_PER_PLATFORM`, default ON) `FileDescriptor.in/out/err`
    // carry the process's real standard `HANDLE`, as HotSpot's, so
    // `Redirect.INHERIT` arrives as that handle rather than the slot index and
    // was decoded as a table id (`inheritIO().start()` threw "descriptor 580 is
    // not backed by a file", `L1bW139ProcessWaitUnderGc`). The stream's own
    // handle in its own slot is an inherit again.
    windows_standard_handles_inherit(&mut requested);
    let redirects = ProcessRedirects {
        stdin: redirect_from_requested_fd(0, requested[0]),
        stdout: redirect_from_requested_fd(1, requested[1]),
        stderr: redirect_from_requested_fd(2, requested[2]),
    };

    let spawned = spawn_child(
        ctx,
        program,
        rest,
        work_dir.as_deref(),
        env_vars.as_deref(),
        env_vars.is_some(),
        redirect_err,
        &redirects,
        true,
    )?;

    // --- Write the parent-side pipe ids back -------------------------------
    //
    // `spawn_child` reports -1 for any slot it did not pipe, which is exactly
    // the `-1` the constructor reads as "no pipe, use the null stream" — so a
    // slot the caller supplied a handle for satisfies the "-1 on output iff not
    // -1 on input" half of the contract for free. The `redirectErrorStream`
    // departure documented on the Linux arm applies here identically: slot 2 is
    // -1 on both sides and `getErrorStream()` is a `NullInputStream`, which is
    // what a merged stream means.
    if let Some(arr) = handles_array {
        let parent_side = [
            spawned.fds.stdin_fd,
            spawned.fds.stdout_fd,
            spawned.fds.stderr_fd,
        ];
        let len = ctx.array_length(arr).min(3);
        for (slot, value) in parent_side.iter().enumerate().take(len) {
            ctx.set_array_element(arr, slot, Value::Long(*value as i64));
        }
    }

    if pb_debug_enabled() {
        eprintln!(
            "[PB-CREATE] pid={} handle={} requested={:?} returned={:?}",
            spawned.pid,
            spawned.handle,
            requested,
            [
                spawned.fds.stdin_fd,
                spawned.fds.stdout_fd,
                spawned.fds.stderr_fd
            ]
        );
    }

    // The native returns the "native process handle" the JDK stores in the
    // private `ProcessImpl.handle` field — our process-table key. Every other
    // `ProcessImpl` native is handed that same value back.
    Ok(Some(Value::Long(spawned.handle)))
}

/// Split a Windows `ProcessEnvironment.toEnvironmentBlock` string.
///
/// NUL-separated `KEY=VALUE` entries (the block is also NUL-terminated, so the
/// final split piece is empty). See the note on
/// [`native_process_impl_create`] for why `=`-prefixed keys are dropped.
#[cfg(windows)]
fn parse_env_block(block: &str) -> Vec<(String, String)> {
    block
        .split('\0')
        .filter(|entry| !entry.is_empty() && !entry.starts_with('='))
        .filter_map(|entry| {
            entry
                .split_once('=')
                .map(|(k, v)| (k.to_string(), v.to_string()))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// The rest of the Windows `java.lang.ProcessImpl` native surface
//
// Everything below is reached only once `--jdk-only` stops shadowing
// `ProcessBuilder.start()` and the image's own `ProcessImpl` runs. Each is
// `ACC_NATIVE` on the JDK 25 Windows image — verified with
// `javap -p -s java.lang.ProcessImpl` — so each is a §1.5 `Bridge`, and there is
// no bytecode behind any of them to fall back to: an unregistered one is an
// `UnsatisfiedLinkError`, which is how `ProcessImpl.<clinit>` failed before this
// (`getStillActive`, from `STILL_ACTIVE = getStillActive()`).
//
// The `long handle` every one of them takes is whatever [`native_process_impl_create`]
// returned, i.e. this VM's process-table key — NOT an OS handle. That is the
// same substitution the Linux arm makes with the pid, and it is why none of
// these need a Win32 call: the table already knows the child.
// ---------------------------------------------------------------------------

/// Win32's `STILL_ACTIVE` (`0x103`) — the value `GetExitCodeProcess` reports for
/// a process that has not exited, and the sentinel
/// `ProcessImpl.exitValue`/`waitFor` compare against.
///
/// The literal matters even though we own both sides of the comparison: the JDK
/// caches it in the `static final int STILL_ACTIVE` field at class-init time and
/// compares raw exit codes against it, so a child that genuinely exits with 259
/// would be read as running. HotSpot has that exact ambiguity and resolves it
/// the same way we do — `exitValue()` re-checks `isProcessAlive(handle)` before
/// throwing, and asks again if the process is really gone.
#[cfg(windows)]
const WINDOWS_STILL_ACTIVE: i32 = 259;

/// Read a `long` argument, defaulting to 0 when absent or not a long.
#[cfg(windows)]
fn long_arg(args: &[Value], index: usize) -> i64 {
    match args.get(index) {
        Some(Value::Long(v)) => *v,
        Some(Value::Int(v)) => *v as i64,
        _ => 0,
    }
}

/// Is `handle` a row this VM's process table owns?
///
/// The guard that keeps every native below from fabricating an answer for a
/// handle it has never seen. `try_exit_handle` alone cannot be that guard: it
/// answers `None` both for "still running" and for "no such child", and taking
/// the second as the first makes `isProcessAlive` report a permanently live
/// process and `waitFor` never return.
///
/// `exit_cache` rather than `process_table` because its rows outlive the child —
/// a reaped child must still resolve, which is exactly the case `exitValue()`
/// asks about.
#[cfg(windows)]
fn known_handle(t: &Arc<ProcessTables>, handle: i64) -> bool {
    t.exits.lock().contains_key(&handle)
}

/// `handle`'s exit-cache row in one lookup (interpreter round i5 wave 102,
/// lane L1, [`direct_wake`]): `None` for a handle this VM does not own
/// ([`known_handle`] false), `Some(code)` for an owned one, `code` its cached
/// exit code if any.
#[cfg(windows)]
fn cached_exit_row(t: &Arc<ProcessTables>, handle: i64) -> Option<Option<i32>> {
    t.exits.lock().get(&handle).map(|row| row.exit_code)
}

/// `java.lang.ProcessImpl.getStillActive()I`
#[cfg(windows)]
fn native_process_impl_get_still_active(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Int(WINDOWS_STILL_ACTIVE)))
}

/// `java.lang.ProcessImpl.getExitCodeProcess(J)I`
///
/// The whole of `exitValue()` and half of `waitFor()`:
///
/// ```text
/// int code = getExitCodeProcess(handle);
/// if (code == STILL_ACTIVE) {
///     if (isProcessAlive(handle)) throw new IllegalThreadStateException(...);
///     return getExitCodeProcess(handle);
/// }
/// return code;
/// ```
///
/// A handle this VM does not own answers -1 rather than `STILL_ACTIVE`: the
/// latter would make `waitFor()` throw `IllegalThreadStateException` from a
/// method the JDK specifies as never throwing it, and `isAlive()` claim a
/// process that does not exist is running.
#[cfg(windows)]
fn native_process_impl_get_exit_code(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Interpreter round i5 wave 101, lane L1: this VM's process tables.
    let t = &tables(ctx);
    let handle = long_arg(args, 0);
    // Wave 102 (lane L1, `CRATONVM_PROCESS_WAIT_DIRECT_WAKE`): the row and its
    // cached code in one lookup (the JDK body calls this right after the wait).
    if direct_wake(t) {
        match cached_exit_row(t, handle) {
            None => return Ok(Some(Value::Int(-1))),
            Some(Some(code)) => return Ok(Some(Value::Int(code))),
            Some(None) => {}
        }
    } else if !known_handle(t, handle) {
        return Ok(Some(Value::Int(-1)));
    }
    Ok(Some(Value::Int(match try_exit_handle(t, handle) {
        Some(code) => code,
        None => WINDOWS_STILL_ACTIVE,
    })))
}

/// `java.lang.ProcessImpl.isProcessAlive(J)Z`
#[cfg(windows)]
fn native_process_impl_is_process_alive(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Interpreter round i5 wave 101, lane L1: this VM's process tables.
    let t = &tables(ctx);
    let handle = long_arg(args, 0);
    // `try_exit_handle` reports `None` while a reaper thread holds the child's
    // mutex inside `Child::wait()`, which is the real `ProcessImpl`'s normal
    // state for every child it spawns — and "someone is blocked waiting for this
    // child to exit" means it has not exited. That is the answer we want.
    // Wave 102 (lane L1, `CRATONVM_PROCESS_WAIT_DIRECT_WAKE`): an unowned
    // handle or a cached exit answered from one lookup.
    let alive = if direct_wake(t) {
        match cached_exit_row(t, handle) {
            None | Some(Some(_)) => false,
            Some(None) => try_exit_handle(t, handle).is_none(),
        }
    } else {
        known_handle(t, handle) && try_exit_handle(t, handle).is_none()
    };
    Ok(Some(Value::Int(i32::from(alive))))
}

/// `java.lang.ProcessImpl.terminateProcess(J)V`
///
/// Both `destroy()` and `destroyForcibly()` land here — Windows has no
/// catchable termination, so `ProcessImpl.destroyForcibly()` is literally
/// `destroy(); return this;`. Hence the unconditional `force`.
#[cfg(windows)]
fn native_process_impl_terminate(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Interpreter round i5 wave 101, lane L1: this VM's process tables.
    let t = &tables(ctx);
    let handle = long_arg(args, 0);
    if known_handle(t, handle) {
        destroy_handle(t, handle, true);
    }
    Ok(None)
}

/// `java.lang.ProcessImpl.waitForInterruptibly(J)V`
///
/// `ProcessImpl.waitFor()` is `waitForInterruptibly(handle)` followed by an
/// interrupt check and `exitValue()`, so this must not return while the child
/// runs unless the waiter is interrupted — returning early otherwise makes
/// `exitValue()` throw `IllegalThreadStateException` out of `waitFor()`.
#[cfg(windows)]
fn native_process_impl_wait_for_interruptibly(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Interpreter round i5 wave 101, lane L1: this VM's process tables.
    let t = &tables(ctx);
    let handle = long_arg(args, 0);
    if !known_handle(t, handle) {
        return Ok(None);
    }
    // Interpreter round i4 wave 96, lane L1
    // (`interpreter-i96-L1-windows-processimpl-wait-ignores-interrupt-FIXED-20261128.md`): the
    // JDK's native is `WaitForMultipleObjects({process, interruptEvent})`, so
    // `Thread.interrupt` wakes it and the Java body throws. `wait_for_handle`
    // (`Child::wait`) is woken by nothing but the exit.
    windows_wait_child_interruptibly(ctx, handle, None);
    Ok(None)
}

/// The body of the Windows `ProcessImpl.waitFor*Interruptibly` natives
/// (interpreter round i4 wave 96, lane L1): return once the child has exited,
/// the waiter's interrupt status is set (the JDK's native waits on the
/// thread's interrupt event too; the Java caller then tests
/// `Thread.interrupted()` and throws), or `deadline` passes. One blocking
/// region, `InNative` (HotSpot: RUNNABLE in the native).
///
/// The wait is [`ExitWait`]: since wave 97 the child's own process handle
/// (`WaitForSingleObject`), so a waiter wakes when the child exits, not at
/// the next 10 ms poll (`L1W70ProcessExitSignal`: HotSpot wakes in
/// microseconds, the poll answered 1-4 ms late) nor one reaper hop later (the
/// wave-96 exit-signal route, 60-130 us). The 10 ms slice bounds how late an
/// interrupt is seen (an atomic read between slices).
/// `CRATONVM_PROCESS_EXIT_SIGNAL=0`: 10 ms sleeps.
#[cfg(windows)]
fn windows_wait_child_interruptibly(
    ctx: &mut dyn NativeContext,
    handle: i64,
    deadline: Option<Instant>,
) {
    // Interpreter round i5 wave 101, lane L1: this VM's process tables.
    let t = &tables(ctx);
    if try_exit_handle(t, handle).is_some() || ctx.is_interrupted(false) {
        return;
    }
    // Interpreter round i4 wave 97, lane L1: the wait is the child's process
    // handle itself (`ExitWait::Handle`), as the JDK native's
    // `WaitForMultipleObjects` is, not the reaper's condvar.
    let exit_wait = ExitWait::for_handle(t, handle, process_exit_signal_on());
    ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::InNative);
    loop {
        // Read before the test (see [`process_exit_generation`]).
        let seen = exit_wait.generation();
        if try_exit_handle(t, handle).is_some() {
            break;
        }
        // An atomic read only: legal inside the region.
        if ctx.is_interrupted(false) {
            break;
        }
        let slice = match deadline {
            Some(deadline) => {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    break;
                }
                left.min(Duration::from_millis(10))
            }
            None => Duration::from_millis(10),
        };
        // Wave 102 (lane L1, `CRATONVM_PROCESS_WAIT_DIRECT_WAKE`): the
        // signalled handle published the code; leave without the re-test.
        // Wave 115 (lane L1, `CRATONVM_PROCESS_WAIT_INTERRUPT_EVENT`): the
        // slice also ends on the thread's interrupt event.
        if exit_wait_slice(ctx, t, &exit_wait, handle, seen, slice).is_some() && direct_wake(t)
        {
            break;
        }
    }
    ctx.end_blocking_region();
    pb_trace_leave(handle, "ProcessImpl.waitFor*Interruptibly");
}

/// `java.lang.ProcessImpl.waitForTimeoutInterruptibly(JJ)V`
///
/// Bounded wait, `void`: the caller re-reads `isProcessAlive(handle)` after every
/// call and loops until its own deadline, so this only has to not overshoot
/// `timeout_ms` and to return promptly once the child is gone. The JDK passes
/// `Integer.MAX_VALUE` for a timeout whose millisecond conversion overflows, so
/// the clamp reproduces its own ceiling rather than inventing one.
#[cfg(windows)]
fn native_process_impl_wait_for_timeout(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Interpreter round i5 wave 101, lane L1: this VM's process tables.
    let t = &tables(ctx);
    let handle = long_arg(args, 0);
    if !known_handle(t, handle) {
        return Ok(None);
    }
    let timeout_ms = long_arg(args, 1).clamp(0, i32::MAX as i64) as u64;
    // Interpreter round i4 wave 96, lane L1: interruptible, and woken by the
    // exit signal rather than the next 10 ms sleep (see
    // [`windows_wait_child_interruptibly`]).
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    windows_wait_child_interruptibly(ctx, handle, Some(deadline));
    Ok(None)
}

/// `java.lang.ProcessImpl.getProcessId0(J)I`
///
/// Called once, from the constructor, and everything the JDK later says about
/// the child's identity hangs off it:
/// `processHandle = ProcessHandleImpl.getInternal(getProcessId0(handle))`. So
/// this must be the real OS pid, not the table key — `Process.pid()`,
/// `toHandle()`, `parent()`, `children()` and the reaper's
/// `completion(pid, true)` all read it.
///
/// `0` for an unknown handle is HotSpot's own failure value (`GetProcessId`
/// returns 0 on error), not a placeholder.
#[cfg(windows)]
fn native_process_impl_get_process_id0(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Interpreter round i5 wave 101, lane L1: this VM's process tables.
    let t = &tables(ctx);
    let pid = pid_for_handle(t, long_arg(args, 0));
    Ok(Some(Value::Int(if pid > 0 { pid as i32 } else { 0 })))
}

/// `java.lang.ProcessImpl.closeHandle(J)Z`
///
/// Invoked from the `Cleaner` the constructor registers, once the `ProcessImpl`
/// becomes unreachable, to release the Win32 process handle.
///
/// There is nothing to release here and the `true` is not a fabricated success:
/// the value handed to us is a process-table key, and the OS handle it stands
/// for is owned by the `std::process::Child` in `PROCESS_TABLE`, whose `Drop`
/// closes it. Dropping the row on this call would be actively wrong — the tables
/// are keyed by pid for `ProcessHandleImpl`'s natives as well, and those outlive
/// the `ProcessImpl` object by design.
#[cfg(windows)]
fn native_process_impl_close_handle(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Int(1)))
}

/// `java.lang.ProcessImpl.openForAtomicAppend(Ljava/lang/String;)J`
///
/// `ProcessImpl.newFileOutputStream(f, append=true)` — i.e. the
/// `Redirect.appendTo(file)` path — opens the file here and wraps the result
/// with `fdAccess.setHandle(fd, handle)`. So the `long` must be a value
/// `FileDescriptor.handle` can carry, which in this VM is an `FdTable` id: the
/// same convention `create`'s `stdHandles` uses, and the same one
/// `fos_get_fd`/`fis_get_fd` resolve.
///
/// The path goes through the sandbox validator and the write capability before
/// the file is opened, exactly as the redirect route in `spawn_child` does —
/// this is a second door onto the same operation and must not be a cheaper one.
/// Declared `throws IOException`, and a refusal or an open failure is reported
/// as one.
#[cfg(windows)]
fn native_process_impl_open_for_atomic_append(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let path = match args.first() {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => {
            return Err(RuntimeError::IOException {
                message: "ProcessImpl.openForAtomicAppend: null path".to_string(),
            }
            .into())
        }
    };
    let validated =
        validate_redirect_path(&path, "redirectOutput").map_err(MethodCallFailed::from)?;
    ctx.check_capability_or_throw(Capability::file_write(&validated))?;
    let fd =
        ctx.fd_table()
            .open_write(&validated, true)
            // Interpreter round i5 wave 101, lane L1 (corrected by the w101
            // hand-back, measured on a Russian-locale host): the JDK native
            // throws `JNU_ThrowIOExceptionWithLastError(env, "CreateFileW")`,
            // whose message is the bare system text (`getLastErrorString`,
            // no `CreateFile error=N, ` prefix), or "CreateFileW" when the
            // system has none; `ProcessBuilder.start` wraps it. It was this
            // function's name, the path and a Rust error.
            .map_err(|e| RuntimeError::IOException {
                message: e
                    .raw_os_error()
                    .and_then(win32_error_text)
                    .unwrap_or_else(|| "CreateFileW".to_string()),
            })?;
    Ok(Some(Value::Long(fd as i64)))
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
        stdin: redirect_from_requested_fd(0, requested[0] as i64),
        stdout: redirect_from_requested_fd(1, requested[1] as i64),
        stderr: redirect_from_requested_fd(2, requested[2] as i64),
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
        true,
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

/// Map one `fds[slot]` / `stdHandles[slot]` input value to the redirect it asks
/// for.
///
/// See the contract notes on [`native_process_impl_fork_and_exec`] (Linux
/// `int[]`) and [`native_process_impl_create`] (Windows `long[]`): -1 asks for a
/// pipe, the slot's own index names the VM's corresponding standard stream
/// (which is what `Redirect.INHERIT` encodes, on both platforms — Windows
/// arrives here through `FileDescriptor.getHandle(int)`, which answers 0/1/2 for
/// the standard streams), and anything else is an `FdTable` id the caller
/// opened. Widened to `i64` so both arms decode through one function and cannot
/// drift apart.
/// Windows `stdHandles`: a slot holding its own standard stream's real
/// `HANDLE` (what `FileDescriptor.getHandle(0..=2)` answers under
/// `CRATONVM_FD_FIELDS_PER_PLATFORM`) is rewritten to the slot index, the
/// `Redirect.INHERIT` spelling [`redirect_from_requested_fd`] decodes. With the
/// switch off the stream's handle IS the index, so nothing changes; a missing
/// stream (`0` / `-1` under a harness) is never matched.
#[cfg(windows)]
fn windows_standard_handles_inherit(requested: &mut [i64; 3]) {
    let per_platform = crate::nio_native::fd_fields_per_platform_enabled();
    for (slot, want) in requested.iter_mut().enumerate() {
        let own = crate::nio_native::standard_stream_handle(slot as i32, per_platform);
        if own > 2 && *want == own {
            *want = slot as i64;
        }
    }
}

#[cfg(any(target_os = "linux", windows))]
fn redirect_from_requested_fd(slot: usize, requested: i64) -> StdioRedirect {
    if requested == -1 {
        StdioRedirect::Pipe
    } else if requested == slot as i64 || is_real_standard_handle(slot, requested) {
        StdioRedirect::Inherit
    } else {
        StdioRedirect::ExistingFd(requested as i32)
    }
}

/// Windows, `FileDescriptor.getHandle(int)` answers the process's real
/// standard `HANDLE` (as HotSpot does; `CRATONVM_FD_FIELDS_PER_PLATFORM`), so
/// `Redirect.INHERIT` reaches `ProcessImpl.create` as that handle value, not as
/// the slot index. It names the same stream: inherit.
#[cfg(any(target_os = "linux", windows))]
fn is_real_standard_handle(slot: usize, requested: i64) -> bool {
    cfg!(windows)
        && requested > 2
        && requested == cratonvm_native_api::os_encoding::standard_stream_os_handle(slot as i32)
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
fn handle_for_pid(t: &Arc<ProcessTables>, pid: i64) -> Option<i64> {
    t.exits
        .lock()
        .iter()
        .find(|(_, cache)| cache.pid == pid)
        .map(|(handle, _)| *handle)
}

/// Milliseconds since the epoch at which `pid` started, or `None`.
///
/// This is the value the JDK calls a process's *start time*, and it is what
/// makes a pid safe to act on: a pid alone is ambiguous, because the OS recycles
/// them, and `(pid, startTime)` is not. `ProcessHandleImpl` threads it through
/// `isAlive0` -> the handle's `startTime` field -> `destroy0`, which refuses to
/// signal when the two disagree. Reporting the JDK's "unknown" sentinel 0
/// everywhere, as this bridge used to, collapses that pair back to a bare pid
/// and leaves `destroy0` with nothing to check.
///
/// `/proc/<pid>/stat` field 22 is the start time in clock ticks since boot.
/// Field 2 is the executable name in parentheses and may itself contain spaces
/// AND parentheses, so the fields are counted from the LAST `)` rather than by
/// splitting the whole line — the classic way to misparse this file.
///
/// Converted to the epoch, because that is the unit the rest of the JDK's
/// process surface speaks: `ProcessHandle.Info.startInstant()` is
/// `Instant.ofEpochMilli` of the same quantity. Ticks would compare equal to
/// themselves and satisfy `destroy0` just as well, but would be wrong the moment
/// anything displayed one.
/// `(creation time in epoch millis, kernel+user CPU in nanoseconds)` for `pid`,
/// or `None` when `/proc/<pid>/stat` cannot be read or parsed — the Linux
/// counterpart of [`win_process_times`], and deliberately the same shape.
///
/// **One read answers both**, for the reason [`win_process_times`]' contract
/// states: fields 14 (`utime`), 15 (`stime`) and 22 (`starttime`) are three
/// columns of ONE line, so a single `read_to_string` cannot attribute them to
/// two different processes the way two reads across a pid recycle could. Before
/// this, `Info.totalTime` was simply never sourced on Linux and kept its `-1`
/// constructor default, so `totalCpuDuration()` reported `Optional.empty()` on a
/// platform whose kernel had the number in the file the start time was already
/// being parsed out of (W5-2 residual 2).
///
/// The field indices all share one off-by-three: `after_comm` begins at field 3
/// (`state`), so field N is index N-3 — 14 -> 11, 15 -> 12, 22 -> 19.
///
/// `utime`/`stime` are the process's OWN CPU; `cutime`/`cstime` (fields 16/17)
/// are its reaped children's and are NOT added. HotSpot's
/// `ProcessHandleImpl_unix.c` sums exactly the first pair, and
/// `Info.totalCpuDuration()` is documented as the cputime "of the process".
#[cfg(target_os = "linux")]
fn linux_proc_stat_times(pid: i64) -> Option<(i64, i64)> {
    if pid <= 0 {
        return None;
    }
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    linux_stat_line_times(&stat)
}

/// The PARSE half of [`linux_proc_stat_times`], split out from the READ so that
/// one line of `/proc/<pid>/stat` can answer more than one question.
///
/// [`linux_liveness_and_start_time`] needs exactly that: the fact that the file
/// opened at all is `isAlive0`'s liveness answer, and field 22 of the same
/// buffer is its start time. Keeping the split at the function boundary rather
/// than copying the parser is deliberate — the field indices below share one
/// off-by-three and the `rfind(')')` rule exists because field 2 may contain
/// spaces AND parentheses, so a second copy is a second chance to get both
/// wrong, and the two copies would then disagree about a start time that
/// `ProcessHandleImpl$Info.info(pid, startTime)` compares with a bare `!=`.
#[cfg(target_os = "linux")]
fn linux_stat_line_times(stat: &str) -> Option<(i64, i64)> {
    let after_comm = &stat[stat.rfind(')')? + 1..];
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    let hz = clock_ticks_per_second();
    if hz <= 0 {
        return None;
    }
    let ticks: i64 = fields.get(19)?.parse().ok()?;
    let start_ms = boot_time_millis()? + ticks.checked_mul(1000)? / hz;
    // A tick is 1/hz of a second, so nanoseconds is `ticks * 1e9 / hz`. The
    // multiply is done first and saturating: on a machine that has been up for
    // years an i64 of nanoseconds still has ~292 years of headroom, but a
    // `checked_mul` that returned `None` here would drop the START time too, and
    // the start time is the one this record cannot do without.
    let utime: i64 = fields.get(11).and_then(|f| f.parse().ok()).unwrap_or(0);
    let stime: i64 = fields.get(12).and_then(|f| f.parse().ok()).unwrap_or(0);
    let cpu_nanos = utime.saturating_add(stime).saturating_mul(1_000_000_000) / hz;
    Some((start_ms, cpu_nanos))
}

#[cfg(target_os = "linux")]
fn os_process_start_time(pid: i64) -> Option<i64> {
    linux_proc_stat_times(pid).map(|(start_ms, _)| start_ms)
}

/// `GetProcessTimes` — `(creation time in epoch millis, kernel+user CPU in
/// nanoseconds)` for `pid`, or `None` when the process cannot be opened.
///
/// The Win32 counterpart of `/proc/<pid>/stat` fields 14/15/22, and the same
/// call HotSpot's `ProcessHandleImpl_md.c` makes on this platform. One
/// `OpenProcess` answers both quantities, so they can never disagree about
/// which process they describe.
///
/// A `FILETIME` counts 100-nanosecond intervals since 1601-01-01 UTC;
/// `FILETIME_UNIX_EPOCH` is that origin expressed against 1970-01-01 (134774
/// days x 86400 s x 10^7). `dwSize`-style layout traps do not apply here — the
/// struct is two `DWORD`s — but the two halves must be recombined explicitly
/// rather than aliased as a `u64`, because `FILETIME` is only 4-byte aligned.
#[cfg(windows)]
fn win_process_times(pid: i64) -> Option<(i64, i64)> {
    use std::ffi::c_void;
    type Handle = *mut c_void;
    type Bool = i32;
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const FILETIME_UNIX_EPOCH: u64 = 116_444_736_000_000_000;

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct FileTime {
        low: u32,
        high: u32,
    }
    impl FileTime {
        fn as_u64(self) -> u64 {
            (u64::from(self.high) << 32) | u64::from(self.low)
        }
    }

    #[link(name = "Kernel32")]
    extern "system" {
        fn OpenProcess(desired_access: u32, inherit_handle: Bool, process_id: u32) -> Handle;
        fn GetProcessTimes(
            h_process: Handle,
            lp_creation_time: *mut FileTime,
            lp_exit_time: *mut FileTime,
            lp_kernel_time: *mut FileTime,
            lp_user_time: *mut FileTime,
        ) -> Bool;
        fn CloseHandle(h_object: Handle) -> Bool;
    }

    if pid <= 0 || pid > u32::MAX as i64 {
        return None;
    }
    // SAFETY: the four out-parameters are live, fully-initialised locals of
    // exactly the documented layout; the handle is closed on every path out and
    // the null failure return is refused before use.
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid as u32);
        if h.is_null() {
            return None;
        }
        let mut creation = FileTime::default();
        let mut exit = FileTime::default();
        let mut kernel = FileTime::default();
        let mut user = FileTime::default();
        let ok = GetProcessTimes(h, &mut creation, &mut exit, &mut kernel, &mut user) != 0;
        CloseHandle(h);
        if !ok {
            return None;
        }
        let created = creation.as_u64().checked_sub(FILETIME_UNIX_EPOCH)?;
        let start_ms = i64::try_from(created / 10_000).ok()?;
        let cpu_100ns = kernel.as_u64().saturating_add(user.as_u64());
        let cpu_nanos = i64::try_from(cpu_100ns.saturating_mul(100)).unwrap_or(i64::MAX);
        Some((start_ms, cpu_nanos))
    }
}

#[cfg(windows)]
fn os_process_start_time(pid: i64) -> Option<i64> {
    win_process_times(pid).map(|(start_ms, _)| start_ms)
}

#[cfg(not(any(target_os = "linux", windows)))]
fn os_process_start_time(_pid: i64) -> Option<i64> {
    None
}

/// The start time to stamp into an `Info` record, together with that process's
/// cumulative kernel+user CPU time in nanoseconds (`Info.totalTime`, which
/// `Info.totalCpuDuration()` reads back through `Duration.ofNanos`) — from ONE
/// OS probe.
///
/// Windows and Linux both report a CPU time, and on both it comes out of the
/// SAME probe the start time already needs — `GetProcessTimes` there,
/// `/proc/<pid>/stat` here. Every other target has no probe wired up and answers
/// `None`, so `Info.totalTime` keeps its `-1` constructor default, which is the
/// JDK's own "unknown" sentinel: `totalCpuDuration()`'s guard is `totalTime !=
/// -1` (verified with `javap -c java.lang.ProcessHandleImpl$Info`), so it
/// renders as `Optional.empty()` — the spec-mandated absence, not a fabricated
/// zero. A written `0` would render as `Optional[PT0S]`, i.e. the positive claim
/// that the process has used no CPU.
///
/// **Why the two are fetched together.** `info0` used to ask
/// `start_time_or_any(pid)` and then a separate `os_process_cpu_nanos(pid)`,
/// and on Windows both bottom out in [`win_process_times`] — so a single
/// `ProcessHandle.info()` opened the same process TWICE, ran `GetProcessTimes`
/// twice, and read a different half of the same struct each time. That is a
/// wasted `OpenProcess`/`CloseHandle` pair per call, and worse, it re-opened
/// the defect [`win_process_times`]'s own contract exists to close: with two
/// opens the start time and the CPU total no longer provably describe the same
/// process instance, because a pid can be recycled between them.
///
/// **The start time is the same number `start_time_or_any` answers, by
/// construction.** The Windows and Linux arms here are each
/// `os_process_start_time`'s own body plus `start_time_or_any`'s
/// `STARTTIME_ANY` fallback, inlined; the catch-all arm calls
/// `start_time_or_any` outright. That identity is load-bearing:
/// `ProcessHandleImpl$Info.info(pid, startTime)` wipes `command`, `arguments`,
/// `startTime`, `totalTime` and `user` off the record unless `startTime ==
/// info.startTime` — a bare `!=` with no `STARTTIME_ANY` wildcarding — and
/// `isAlive0` and `current_process_start_time` both source their number from
/// `start_time_or_any`. All three must stay one function's answer.
#[cfg(windows)]
fn start_time_and_cpu(pid: i64) -> (i64, Option<i64>) {
    match win_process_times(pid) {
        Some((start_ms, cpu_nanos)) => (start_ms, Some(cpu_nanos)),
        None => (PROCESS_STARTTIME_ANY, None),
    }
}

/// The Linux arm, and the same identity argument as the Windows one above: this
/// is `os_process_start_time`'s body (i.e. `linux_proc_stat_times`) plus
/// `start_time_or_any`'s `STARTTIME_ANY` fallback, inlined, so the number
/// written to `Info.startTime` is still exactly the number `isAlive0` returns
/// and `current_process_start_time` stamps on `ProcessHandle.current()`.
///
/// A `/proc/<pid>/stat` that cannot be read degrades BOTH halves together: the
/// start time to `STARTTIME_ANY` (the process exists, its start time is not
/// available) and the CPU total to `None` (`Optional.empty()`). Reporting a
/// start time from one read and a CPU total from another would reintroduce
/// exactly the two-probe attribution hole [`win_process_times`] exists to close.
#[cfg(target_os = "linux")]
fn start_time_and_cpu(pid: i64) -> (i64, Option<i64>) {
    match linux_proc_stat_times(pid) {
        Some((start_ms, cpu_nanos)) => (start_ms, Some(cpu_nanos)),
        None => (PROCESS_STARTTIME_ANY, None),
    }
}

/// No platform outside Windows and Linux has a CPU-time probe wired up here, so
/// `Info.totalTime` keeps its `-1` constructor default. The start time still
/// comes from `start_time_or_any`, the one source `isAlive0`, `info0` and
/// `current_process_start_time` all share.
#[cfg(not(any(target_os = "linux", windows)))]
fn start_time_and_cpu(pid: i64) -> (i64, Option<i64>) {
    (start_time_or_any(pid), None)
}

/// The full path of the executable image backing `pid`.
///
/// Windows has no `/proc/<pid>/cmdline` equivalent that a bystander process can
/// read cheaply, which is why real HotSpot 25 on Windows reports
/// `Info.command()` PRESENT while `commandLine()` and `arguments()` are both
/// EMPTY (measured against this host's JDK 25.0.3, see the lane record). So
/// only the image name is sourced here, from `QueryFullProcessImageNameW` —
/// the same call `ProcessHandleImpl_md.c` makes.
#[cfg(windows)]
fn os_process_image_name(pid: i64) -> Option<String> {
    use std::ffi::c_void;
    type Handle = *mut c_void;
    type Bool = i32;
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    // `MAX_PATH` is not the ceiling for a full image path; the API takes the
    // buffer size in CHARACTERS and reports back how many it wrote.
    const BUF_CHARS: u32 = 32_768;

    #[link(name = "Kernel32")]
    extern "system" {
        fn OpenProcess(desired_access: u32, inherit_handle: Bool, process_id: u32) -> Handle;
        fn QueryFullProcessImageNameW(
            h_process: Handle,
            dw_flags: u32,
            lp_exe_name: *mut u16,
            lpdw_size: *mut u32,
        ) -> Bool;
        fn CloseHandle(h_object: Handle) -> Bool;
    }

    if pid <= 0 || pid > u32::MAX as i64 {
        return None;
    }
    let mut buf: Vec<u16> = vec![0; BUF_CHARS as usize];
    // SAFETY: `buf` is a live allocation of exactly `BUF_CHARS` u16s and `size`
    // is initialised to that same count, which is the contract the API
    // documents; it writes at most `size` characters and updates `size` to the
    // count actually written. The handle is closed on every path out.
    let written = unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid as u32);
        if h.is_null() {
            return None;
        }
        let mut size: u32 = BUF_CHARS;
        let ok = QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut size) != 0;
        CloseHandle(h);
        if !ok {
            return None;
        }
        size as usize
    };
    if written == 0 || written > buf.len() {
        return None;
    }
    let name = String::from_utf16_lossy(&buf[..written]);
    if name.trim().is_empty() {
        None
    } else {
        Some(name)
    }
}

#[cfg(not(windows))]
fn os_process_image_name(_pid: i64) -> Option<String> {
    None
}

/// The account that owns `pid`, in the platform's own spelling, or `None`.
///
/// `Info.user` was left `null` on every platform, so `ProcessHandle.Info.user()`
/// — whose body is `Optional.ofNullable(user)`, verified with `javap -c
/// java.lang.ProcessHandleImpl$Info` — reported `Optional.empty()` for every
/// process on a platform where HotSpot reports a name (W5-2 residual 1;
/// `Optional[CARBON\Victor]` is what real HotSpot 25 answered on this host).
///
/// **`None` here is a spec-mandated absence, not a fallback.**
/// `ProcessHandle.Info`'s own javadoc: *"The attributes of a process vary by
/// operating system and are not available in all implementations. Information
/// about processes is limited by the operating system privileges of the process
/// making the request."* Every failure path below is one of those two sentences
/// — a target with no probe wired up, or a process this VM may not open — and
/// each answers `Optional.empty()` rather than substituting THIS process's user,
/// which is the fabricated success the equivalent `parent()` stub still commits
/// (see the record).
///
/// Windows spells it `DOMAIN\name`, which is what `LookupAccountSidW` produces
/// from the token's SID and what HotSpot's `ProcessHandleImpl_md.c` reports; it
/// is deliberately not normalised.
#[cfg(windows)]
fn os_process_user(pid: i64) -> Option<String> {
    use std::ffi::c_void;
    type Handle = *mut c_void;
    type Bool = i32;
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const TOKEN_QUERY: u32 = 0x0008;
    /// `TOKEN_INFORMATION_CLASS::TokenUser`.
    const TOKEN_USER_CLASS: i32 = 1;
    /// Both are the documented maxima for an account and a domain name. The API
    /// takes them in CHARACTERS and reports back how many it wrote.
    const NAME_CHARS: u32 = 256;
    const DOMAIN_CHARS: u32 = 256;

    #[link(name = "Kernel32")]
    extern "system" {
        fn OpenProcess(desired_access: u32, inherit_handle: Bool, process_id: u32) -> Handle;
        fn CloseHandle(h_object: Handle) -> Bool;
    }
    #[link(name = "Advapi32")]
    extern "system" {
        fn OpenProcessToken(
            h_process: Handle,
            desired_access: u32,
            token_handle: *mut Handle,
        ) -> Bool;
        fn GetTokenInformation(
            token_handle: Handle,
            token_information_class: i32,
            token_information: *mut c_void,
            token_information_length: u32,
            return_length: *mut u32,
        ) -> Bool;
        fn LookupAccountSidW(
            lp_system_name: *const u16,
            sid: *mut c_void,
            name: *mut u16,
            cch_name: *mut u32,
            referenced_domain_name: *mut u16,
            cch_referenced_domain_name: *mut u32,
            pe_use: *mut i32,
        ) -> Bool;
    }

    if pid <= 0 || pid > u32::MAX as i64 {
        return None;
    }
    // SAFETY: every out-parameter is a live, fully-initialised local of the
    // documented layout; both handles are closed on every path out of the block;
    // the two null failure returns are refused before use. The token buffer is a
    // `Vec<u64>` rather than a `Vec<u8>` on purpose — `TOKEN_USER` begins with a
    // `SID_AND_ATTRIBUTES` whose first member is a `PSID`, and reading a pointer
    // out of a 1-aligned byte buffer is an unaligned load; a `u64` element type
    // gives the allocation 8-byte alignment by construction.
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid as u32);
        if h.is_null() {
            return None;
        }
        let mut token: Handle = std::ptr::null_mut();
        let opened = OpenProcessToken(h, TOKEN_QUERY, &mut token) != 0;
        CloseHandle(h);
        if !opened || token.is_null() {
            return None;
        }
        // Size probe first: `TOKEN_USER` is variable-length because the SID it
        // carries is, so there is no fixed struct to hand in. The probe call is
        // EXPECTED to fail (`ERROR_INSUFFICIENT_BUFFER`); only `needed` matters.
        let mut needed: u32 = 0;
        GetTokenInformation(
            token,
            TOKEN_USER_CLASS,
            std::ptr::null_mut(),
            0,
            &mut needed,
        );
        if needed == 0 {
            CloseHandle(token);
            return None;
        }
        let mut buf: Vec<u64> = vec![0; (needed as usize).div_ceil(8)];
        let mut written = needed;
        let got = GetTokenInformation(
            token,
            TOKEN_USER_CLASS,
            buf.as_mut_ptr().cast::<c_void>(),
            needed,
            &mut written,
        ) != 0;
        CloseHandle(token);
        if !got {
            return None;
        }
        // The SID points INTO `buf`, which outlives the lookup below.
        let sid = *buf.as_ptr().cast::<*mut c_void>();
        if sid.is_null() {
            return None;
        }
        let mut name = vec![0u16; NAME_CHARS as usize];
        let mut domain = vec![0u16; DOMAIN_CHARS as usize];
        let mut cch_name = NAME_CHARS;
        let mut cch_domain = DOMAIN_CHARS;
        let mut sid_type: i32 = 0;
        let resolved = LookupAccountSidW(
            std::ptr::null(),
            sid,
            name.as_mut_ptr(),
            &mut cch_name,
            domain.as_mut_ptr(),
            &mut cch_domain,
            &mut sid_type,
        ) != 0;
        // A SID that resolves to no account (a deleted user, an unreachable
        // domain controller) is the javadoc's "not available", so it stays
        // absent. Rendering the raw SID string instead would be a value HotSpot
        // never produces.
        if !resolved {
            return None;
        }
        let cch_name = (cch_name as usize).min(name.len());
        let cch_domain = (cch_domain as usize).min(domain.len());
        let account = String::from_utf16_lossy(&name[..cch_name]);
        if account.trim().is_empty() {
            return None;
        }
        let domain = String::from_utf16_lossy(&domain[..cch_domain]);
        if domain.trim().is_empty() {
            Some(account)
        } else {
            Some(format!("{domain}\\{account}"))
        }
    }
}

/// `/proc/<pid>/status`' `Uid:` line carries four columns — real, effective,
/// saved-set and filesystem uid — and the REAL uid (the first) is the one
/// HotSpot's `ProcessHandleImpl_unix.c` reports, so it is the one taken here.
///
/// Resolved through the passwd database rather than `/etc/passwd` directly:
/// `getpwuid_r` goes through NSS, so an LDAP/SSSD account resolves to the same
/// name HotSpot prints, and a container with no passwd entry at all answers
/// `None` — the javadoc's "not available", not a fabricated numeric stand-in.
/// A buffer too small for the entry (`ERANGE`) is treated the same way; it is an
/// absence, and inventing a name is the failure this campaign is hunting.
#[cfg(target_os = "linux")]
fn os_process_user(pid: i64) -> Option<String> {
    if pid <= 0 {
        return None;
    }
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let uid: u32 = status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    // SAFETY: `libc::passwd` is a plain C struct of pointers and integers, for
    // which an all-zero bit pattern is a valid (null/0) value. It is only read
    // after `getpwuid_r` below reports success, which is what fills it in.
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buf = vec![0 as libc::c_char; 4096];
    let mut found: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: `pwd`, `buf` and `found` are live locals; `getpwuid_r` writes only
    // through them and only up to `buf.len()` bytes, and `pwd.pw_name` points
    // into `buf`, which outlives the read below. It is the reentrant form on
    // purpose — `getpwuid` returns a pointer into a static that another thread's
    // call would overwrite under us.
    let rc = unsafe {
        libc::getpwuid_r(
            uid as libc::uid_t,
            &mut pwd,
            buf.as_mut_ptr(),
            buf.len(),
            &mut found,
        )
    };
    if rc != 0 || found.is_null() || pwd.pw_name.is_null() {
        return None;
    }
    // SAFETY: `pw_name` is a NUL-terminated C string inside `buf`, which is
    // still in scope.
    let name = unsafe { std::ffi::CStr::from_ptr(pwd.pw_name) }
        .to_string_lossy()
        .into_owned();
    if name.trim().is_empty() {
        None
    } else {
        Some(name)
    }
}

/// No account probe is wired up for this target, so `Info.user` stays `null` and
/// `user()` answers `Optional.empty()` — which the `ProcessHandle.Info` javadoc
/// specifies for exactly this case: *"The attributes of a process vary by
/// operating system and are not available in all implementations."*
#[cfg(not(any(target_os = "linux", windows)))]
fn os_process_user(_pid: i64) -> Option<String> {
    None
}

/// `sysconf(_SC_CLK_TCK)` — the unit of `/proc/<pid>/stat`'s tick fields.
#[cfg(target_os = "linux")]
fn clock_ticks_per_second() -> i64 {
    static HZ: OnceLock<i64> = OnceLock::new();
    // SAFETY: `sysconf` reads a process-wide constant and touches no caller
    // memory.
    *HZ.get_or_init(|| unsafe { libc::sysconf(libc::_SC_CLK_TCK) })
}

/// Wall-clock time of the last boot, in milliseconds since the epoch.
///
/// `/proc/stat`'s `btime` line, in seconds. Cached: it does not change while
/// this process runs, and every `isAlive0` would otherwise re-read a file whose
/// first lines are the whole machine's CPU accounting.
#[cfg(target_os = "linux")]
fn boot_time_millis() -> Option<i64> {
    static BOOT: OnceLock<Option<i64>> = OnceLock::new();
    *BOOT.get_or_init(|| {
        let stat = std::fs::read_to_string("/proc/stat").ok()?;
        stat.lines()
            .find_map(|line| line.strip_prefix("btime "))
            .and_then(|secs| secs.trim().parse::<i64>().ok())
            .and_then(|secs| secs.checked_mul(1000))
    })
}

/// The start time to report for a process known to exist.
///
/// Falls back to `STARTTIME_ANY` rather than to "no such process": a
/// `/proc/<pid>/stat` this VM cannot read (permissions, a non-Linux host, a
/// process that exited between the liveness check and this read) means the start
/// time is unknown, not that the process is absent. 0 is the JDK's own word for
/// that, and every comparison against it is short-circuited.
fn start_time_or_any(pid: i64) -> i64 {
    os_process_start_time(pid).unwrap_or(PROCESS_STARTTIME_ANY)
}

/// The start time to stamp into THE `ProcessHandle.current()` handle.
///
/// Exported for `native-builtins`' `p60_process_handle_current`, which mints
/// that object. It must be **this** function rather than a second
/// implementation, because `ProcessHandleImpl$Info.info(pid, startTime)` — real
/// JDK bytecode, not something this VM can influence — WIPES `command`,
/// `arguments`, `startTime`, `totalTime` and `user` off the freshly filled
/// record whenever the handle's `startTime` differs by so much as one from the
/// value `info0` wrote:
///
/// ```text
/// info.info0(pid);
/// if (startTime != info.startTime) { info.command = null; ... }
/// ```
///
/// `info0` writes `start_time_or_any(pid)`, `isAlive0` returns
/// `start_time_or_any(pid)`, and HotSpot's own `<clinit>` seeds `current` with
/// `new ProcessHandleImpl(pid, isAlive0(pid))` — so all three must be the one
/// number. The handle used to be built with a hardcoded `0` (`STARTTIME_ANY`),
/// which `equals()`/`isAlive()` treat as a wildcard but `Info.info` does not,
/// and that single mismatch is what silently emptied
/// `ProcessHandle.current().info()`.
pub fn current_process_start_time() -> i64 {
    start_time_or_any(std::process::id() as i64)
}

/// `ProcessHandleImpl.STARTTIME_ANY` — "the process exists; its start time is
/// not available". Distinct from `STARTTIME_PROCESS_UNKNOWN` (-1), which is the
/// only value `isAlive()` reads as not-alive.
const PROCESS_STARTTIME_ANY: i64 = 0;

// There is no `#[cfg(target_os = "linux")] fn foreign_pid_is_alive` any more
// either, and for the same reason the Windows one went (below). It was
// `std::path::Path::new(&format!("/proc/{pid}")).exists()`, and its only caller
// went straight on to `start_time_or_any(pid)` -> `/proc/<pid>/stat`: two
// filesystem probes of two DIFFERENT files for one question, in the one native
// whose return value exists so that `ProcessHandleImpl.isAlive()` and `destroy0`
// can detect a RECYCLED pid. Cheaper than Windows' two `OpenProcess` calls, and
// the attribution hole is identical — see [`linux_liveness_and_start_time`],
// which is now the single door. What the boolean knew is preserved there: a
// zombie is still a process (`/proc/<pid>/stat` exists until it is reaped, and
// reporting it alive is what the JDK's reaper needs), and a non-positive pid
// names none.

// There is no `#[cfg(windows)] fn foreign_pid_is_alive` any more, deliberately.
// It existed to answer liveness ALONE, and its one caller
// (`native_proc_handle_is_alive0`) always went on to ask
// `start_time_or_any(pid)` in the same breath — a second `OpenProcess` for the
// other half of one question. Keeping the boolean wrapper beside
// `win_liveness_and_start_time` would leave a second door onto the same probe
// through which that pairing could grow back, and it would have no caller, so
// it is folded in rather than retained. What it knew is preserved below: the
// `ERROR_ACCESS_DENIED` edge, the 259 ambiguity, and the DWORD-range refusal
// that keeps `ProcessHandle.of(Long.MAX_VALUE)` syscall-free
// (`RJdkProcess` asserts that lookup is empty).

/// Liveness AND start time for a foreign pid, from **one** `OpenProcess`.
///
/// # Why one handle rather than two calls
///
/// `ProcessHandleImpl.isAlive0(pid)` needs both facts at once — its return value
/// is `-1` for "not alive" and the process's start time otherwise — and it used
/// to get them from two separate probes: `foreign_pid_is_alive(pid)`
/// (`OpenProcess` + `GetExitCodeProcess` + `CloseHandle`) followed by
/// `start_time_or_any(pid)` -> [`win_process_times`] (`OpenProcess` +
/// `GetProcessTimes` + `CloseHandle`). That is the shape W6-10 finding 2
/// removed from `info0` — "a single call opened the same process TWICE and read
/// a different half of the same answer each time" — surviving on the
/// neighbouring native, and it is not only a wasted syscall pair:
///
/// * **Attribution.** [`win_process_times`]' own contract is "one `OpenProcess`
///   answers both quantities, so they can never disagree about which process
///   they describe". With two opens a pid recycled in between yields "alive"
///   about one process and a start time about another — and that start time is
///   exactly the number `ProcessHandleImpl.isAlive()` compares against
///   `this.startTime`, and the number `destroy0`'s recycled-pid guard trusts.
///   A probe whose whole job is to detect a recycled pid must not itself
///   straddle one.
/// * **Cost.** `ProcessHandle.of(pid)` calls `isAlive0` once per lookup, and
///   `RJdkProcess`' tree polling reaches it through every handle it materialises.
///   Two opens become one.
///
/// The Win32 handle from `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)` is
/// valid for `GetExitCodeProcess` and `GetProcessTimes` alike, so no additional
/// access right is needed and the desired-access mask is unchanged.
///
/// # The answers, and where each came from before
///
/// Every arm below is one of the two old functions' arms, unchanged — this is a
/// merge, not a re-decision, and the pair `(alive, start)` it returns is
/// value-identical to what the two calls produced whenever no recycle happened:
///
/// * pid outside the `DWORD` range — `(false, None)`. Refused without a
///   syscall, which is what keeps `ProcessHandle.of(Long.MAX_VALUE)` cheap
///   (`RJdkProcess` asserts it is empty).
/// * `OpenProcess` returns null, `ERROR_ACCESS_DENIED` included —
///   `(false, None)`. Until wave 98 an access-denied open answered
///   `(true, None)` ("it exists, we lack rights"); HotSpot's `isAlive0`
///   returns -1 for every failed open, and `ProcessHandle.of(4)` is empty
///   there (measured), so that arm is gone.
/// * `GetExitCodeProcess` fails, or reports anything but `STILL_ACTIVE` —
///   `(false, None)`. The 259 ambiguity is unchanged and is the one HotSpot
///   inherits too.
/// * alive, and `GetProcessTimes` fails — `(true, None)`. Same degradation to
///   `STARTTIME_ANY` the separate `win_process_times` produced.
#[cfg(windows)]
fn win_liveness_and_start_time(pid: i64) -> (bool, Option<i64>) {
    use std::ffi::c_void;
    // Signatures are IDENTICAL to `pipe.rs`'s `CloseHandle` declaration so the
    // `clashing_extern_declarations` deny-lint does not fire.
    type Handle = *mut c_void;
    type Bool = i32;
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const STILL_ACTIVE: u32 = 259;
    const FILETIME_UNIX_EPOCH: u64 = 116_444_736_000_000_000;

    /// Same layout and the same recombination rule as [`win_process_times`]'
    /// local copy: a `FILETIME` is only 4-byte aligned, so the two halves are
    /// shifted together explicitly rather than aliased as a `u64`.
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct FileTime {
        low: u32,
        high: u32,
    }
    impl FileTime {
        fn as_u64(self) -> u64 {
            (u64::from(self.high) << 32) | u64::from(self.low)
        }
    }

    #[link(name = "Kernel32")]
    extern "system" {
        fn OpenProcess(desired_access: u32, inherit_handle: Bool, process_id: u32) -> Handle;
        fn GetExitCodeProcess(h_process: Handle, lp_exit_code: *mut u32) -> Bool;
        fn GetProcessTimes(
            h_process: Handle,
            lp_creation_time: *mut FileTime,
            lp_exit_time: *mut FileTime,
            lp_kernel_time: *mut FileTime,
            lp_user_time: *mut FileTime,
        ) -> Bool;
        fn CloseHandle(h_object: Handle) -> Bool;
    }

    // A Windows process id is a DWORD. A value outside that range cannot name a
    // process, and answering that without a syscall keeps the `Long.MAX_VALUE`
    // probe cheap.
    if pid <= 0 || pid > u32::MAX as i64 {
        return (false, None);
    }
    // SAFETY: `OpenProcess` takes only scalars; `GetExitCodeProcess` writes one
    // `u32` and `GetProcessTimes` four `FILETIME`s through pointers to live,
    // fully-initialised locals of exactly the documented layout; the handle is
    // closed on every path out. A null handle is the documented failure return
    // and is never passed on.
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid as u32);
        if h.is_null() {
            // Interpreter round i4 wave 98, lane L1 (hand-back on w104,
            // `interpreter-i98-L1-processhandle-destroy-of-an-exited-child-answers-true-FIXED-20261130.md`):
            // HotSpot's `ProcessHandleImpl_win.c` `isAlive0` answers -1 for
            // ANY failed `OpenProcess`, access denied included, so
            // `ProcessHandle.of(4)` (the System process) is empty there
            // (MEASURED, `L1W98ProcessHandleAnswers`). The old "denied means
            // alive" arm made it present.
            return (false, None);
        }
        let mut code: u32 = 0;
        let queried = GetExitCodeProcess(h, &mut code) != 0;
        let alive = queried && code == STILL_ACTIVE;
        let mut start = None;
        if alive {
            let mut creation = FileTime::default();
            let mut exit = FileTime::default();
            let mut kernel = FileTime::default();
            let mut user = FileTime::default();
            if GetProcessTimes(h, &mut creation, &mut exit, &mut kernel, &mut user) != 0 {
                start = creation
                    .as_u64()
                    .checked_sub(FILETIME_UNIX_EPOCH)
                    .and_then(|created| i64::try_from(created / 10_000).ok());
            }
        }
        CloseHandle(h);
        (alive, start)
    }
}

/// Liveness AND start time for a foreign pid, from **one** `/proc/<pid>/stat`
/// read — the Linux counterpart of [`win_liveness_and_start_time`], and the
/// same argument.
///
/// # Why one read rather than two probes
///
/// `isAlive0(pid)` used to ask `foreign_pid_is_alive(pid)` — an `exists()` on
/// `/proc/<pid>` — and then `start_time_or_any(pid)`, which reads
/// `/proc/<pid>/stat`. Two probes of two different paths for one question, and
/// the objection is not the cost (two file reads, not two handle opens) but the
/// ATTRIBUTION: a pid recycled between them yields "alive" about one process and
/// a start time about another, and that start time is precisely the number
/// `ProcessHandleImpl.isAlive()` compares against `this.startTime` and
/// `destroy0`'s recycled-pid guard trusts. A probe whose whole job is to detect
/// a recycled pid must not itself straddle one. One `read_to_string` cannot:
/// the existence of the line and field 22 of that same line describe one
/// process instance by construction.
///
/// It is also what the oracle does. `ProcessHandleImpl_unix.c`'s `isAlive0` is
/// `os_getParentPidAndTimings(env, pid, &totalTime, &startTime)` — a single
/// `/proc/<pid>/stat` open — returning `-1` when it fails, so this is HotSpot's
/// shape rather than a local invention.
///
/// # The answers, and where each came from before
///
/// Every arm is one of the two old probes' arms, unchanged; this is a merge,
/// not a re-decision:
///
/// * non-positive pid — `(false, None)`. Both old probes refused it.
/// * the file opened — `(true, ...)`. `/proc/<pid>/stat` exists for a ZOMBIE
///   too, which is the answer the old `exists()` on `/proc/<pid>` gave and the
///   one the JDK's reaper needs: a process that has exited but not been reaped
///   is still a process.
/// * the file opened but the line will not parse (a `/proc/stat` with no
///   `btime`, an `_SC_CLK_TCK` of 0, a truncated line) — `(true, None)`, which
///   the caller degrades to `STARTTIME_ANY` (0) exactly as the old
///   `start_time_or_any` did. This is why the READ and the PARSE are separate
///   functions: collapsing them would report a live process as DEAD on a
///   machine-wide condition that says nothing about that process.
/// * `ENOENT` — `(false, None)`. On this platform that IS the liveness answer;
///   `/proc/<pid>/stat` is mode 0444, so a live process cannot be missing it.
/// * any other error — `(true, None)`. Refusing to say "dead" on an error we
///   cannot attribute is the same asymmetry the Windows arm makes for
///   `ERROR_ACCESS_DENIED`: claiming a running process had exited is the
///   fabrication in the direction that breaks callers.
#[cfg(target_os = "linux")]
fn linux_liveness_and_start_time(pid: i64) -> (bool, Option<i64>) {
    if pid <= 0 {
        return (false, None);
    }
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => (
            true,
            linux_stat_line_times(&stat).map(|(start_ms, _)| start_ms),
        ),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => (false, None),
        Err(_) => (true, None),
    }
}

/// `isAlive0`'s answer for a pid this VM did not spawn: `-1` when it is not
/// alive, its start time (or `STARTTIME_ANY`) when it is.
///
/// Split per platform so that each can answer from the SINGLE probe that
/// carries both halves — [`win_liveness_and_start_time`] on Windows,
/// [`linux_liveness_and_start_time`] on Linux. The two bodies are deliberately
/// identical below the probe name; the pair `(alive, start)` is the whole
/// interface, and a platform that grows a real probe joins by supplying one
/// rather than by re-deriving the mapping onto `-1` / `STARTTIME_ANY`.
///
/// Only the platform-of-last-resort arm still takes two steps, because there
/// `os_process_start_time` has no probe at all and answers `None` outright —
/// there is no second read to straddle a recycle with.
#[cfg(windows)]
fn foreign_start_time_or_dead(pid: i64) -> i64 {
    match win_liveness_and_start_time(pid) {
        (true, start) => start.unwrap_or(PROCESS_STARTTIME_ANY),
        (false, _) => -1,
    }
}

#[cfg(target_os = "linux")]
fn foreign_start_time_or_dead(pid: i64) -> i64 {
    match linux_liveness_and_start_time(pid) {
        (true, start) => start.unwrap_or(PROCESS_STARTTIME_ANY),
        (false, _) => -1,
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
fn foreign_start_time_or_dead(pid: i64) -> i64 {
    if foreign_pid_is_alive(pid) {
        start_time_or_any(pid)
    } else {
        -1
    }
}

/// Is a pid we did not spawn still alive? — platforms with no probe wired up.
///
/// There is no portable liveness primitive here, and answering a confident
/// `false` would make `ProcessHandle.of(pid).isAlive()` claim a running process
/// had exited, so the optimistic answer stands for any pid that could name a
/// process at all. Values outside the `pid_t` range cannot, and are refused.
#[cfg(not(any(target_os = "linux", windows)))]
fn foreign_pid_is_alive(pid: i64) -> bool {
    pid > 0 && pid <= i32::MAX as i64
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
/// value as the start time itself. A live process is reported as
/// `start_time_or_any(pid)` — the real OS start time where the platform has a
/// probe, `STARTTIME_ANY` (0) where it does not.
///
/// (This paragraph used to say "we have no cheap start time, so alive is
/// reported as the constant 1". That stopped being true when `os_process_start_time`
/// gained its Windows and Linux arms, and the body below has not returned a
/// constant since. Left corrected rather than deleted: a stale comment that
/// describes a defect the code no longer has is how the next reader concludes
/// the fix never landed.)
///
/// The distinction between 0 and -1 is load-bearing. Returning 0 for a process
/// that does not exist reads as "exists, unknown start time"; the reaper's
/// `NOT_A_CHILD` fallback loop spins `while (startTime >= 0)` and would never
/// leave it.
fn native_proc_handle_is_alive0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Interpreter round i5 wave 101, lane L1: this VM's process tables.
    let t = &tables(ctx);
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
        return Ok(Some(Value::Long(start_time_or_any(pid))));
    }
    match handle_for_pid(t, pid) {
        // One of our own children: the process table knows for certain.
        Some(handle) => match try_exit_handle(t, handle) {
            Some(_) => Ok(Some(Value::Long(-1))), // exited
            None => Ok(Some(Value::Long(start_time_or_any(pid)))),
        },
        // Someone else's process: ask the OS — for BOTH facts at once. This was
        // `foreign_pid_is_alive(pid)` followed by `start_time_or_any(pid)`: two
        // separate `OpenProcess`/`CloseHandle` pairs on Windows, and two reads
        // of two different `/proc` paths on Linux, for one question — straddling
        // a pid-recycle window in the one native whose return value exists to
        // detect recycled pids. Both arms now answer from one probe; see
        // `foreign_start_time_or_dead`.
        None => Ok(Some(Value::Long(foreign_start_time_or_dead(pid)))),
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
    // Interpreter round i5 wave 101, lane L1: this VM's process tables.
    let t = &tables(ctx);
    // Keyed by pid, like the rest of this bridge -- see `handle_for_pid`.
    let pid = match args.first() {
        Some(Value::Long(p)) => *p,
        _ => return Ok(Some(Value::Int(-1))),
    };
    // A pid we did not spawn cannot be waited for: `wait(2)` only works on
    // one's own children. That is not an error, and it is not an exit status —
    // it is a specific answer the JDK asks for by name:
    //
    //     int exitValue = waitForProcessExit0(pid, shouldReap);
    //     if (exitValue == NOT_A_CHILD) {
    //         // pid not alive or not a child of this process
    //         // If it is alive wait for it to terminate
    //         long startTime = isAlive0(pid);
    //         while (startTime >= 0) { Thread.sleep(..); startTime = isAlive0(pid); }
    //         exitValue = 0;
    //     }
    //     newCompletion.complete(exitValue);
    //
    // Anything else is taken at face value, so the -1 that used to stand here
    // completed `ProcessHandle.of(pid).onExit()` IMMEDIATELY, with a fabricated
    // exit status of -1, for a process that was still running. HotSpot's own
    // native returns `NOT_A_CHILD` on `ECHILD` for exactly this case.
    //
    // Measured with `probes/ForeignHandleProbe.java`, whose subject is a
    // grandchild (`sh` forks it and exits, so it is reparented and `waitpid`
    // gives ECHILD). Bounding the wait at 400ms against a process that lives 30s:
    //
    //   HotSpot 25   onExitWaitsWhileAlive=still-waiting
    //   before       onExitWaitsWhileAlive=completed-while-alive
    //   after        onExitWaitsWhileAlive=still-waiting
    //
    // The fallback then depends on `isAlive0` answering for a pid we did not
    // spawn, which `foreign_start_time_or_dead` does; without that this would trade a
    // wrong answer for a hang, so the probe waits on the future after the
    // process really dies rather than only checking that it does not complete.
    //
    // Windows is different (interpreter round i5 wave 100, lane L1; page
    // `interpreter-i100-L1-windows-foreign-process-onexit-polls-FIXED-20261015.md`): HotSpot's
    // `ProcessHandleImpl_win.c` never answers `NOT_A_CHILD`. It opens ANY pid
    // (`SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION`), answers -1 when it
    // cannot (no such process, or access denied), and otherwise waits on the
    // process handle and answers its exit code. So
    // `ProcessHandle.of(pid).onExit()` of another process completes at the
    // exit, not at the JDK's 300 ms+ `isAlive0` poll that `NOT_A_CHILD` sends
    // it to (and an unopenable pid completes at once, as on HotSpot).
    let Some(handle) = handle_for_pid(t, pid) else {
        #[cfg(windows)]
        return Ok(Some(Value::Int(windows_wait_foreign_process_exit(ctx, pid))));
        // Interpreter round i5 wave 102 hand-back (w102c, Linux): a pid this VM
        // did not spawn answers `NOT_A_CHILD` at once whatever
        // `CRATONVM_PROCESS_PIDFD_WAIT` says, as HotSpot's `waitpid` ECHILD does;
        // a pidfd wait here completed `onExit()` at the exit where HotSpot's
        // JDK completes it at its `isAlive0` poll (`L1W100ForeignProcessOnExit`
        // `onExit-prompt=false`). The pidfd serves this VM's own children only.
        #[cfg(not(windows))]
        return Ok(Some(Value::Int(PROCESS_NOT_A_CHILD)));
    };
    // Interpreter round i7 wave 139, lane L1b (`CRATONVM_PROCESS_WAIT_LAZY_NATIVE`,
    // default OFF): the wait in the lazy in-native state, no root deposit; the
    // closure reads the process tables and the OS only. Declined: the
    // ordinary `InNative` region below.
    if t.wait_lazy_native {
        let mut code = None;
        let waited = ctx.run_in_native_wait_lazy(&mut || {
            code = Some(wait_for_handle(t, handle));
            pb_trace_wake_at(handle, "waitForProcessExit0");
        });
        if let (true, Some(code)) = (waited, code) {
            pb_trace_leave(handle, "ProcessHandleImpl.waitForProcessExit0");
            return Ok(Some(Value::Int(code)));
        }
    }
    ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::InNative);
    let code = wait_for_handle(t, handle);
    // Interpreter round i5 wave 106, lane L1 (i97 attribution): the
    // `onExit()` reaper's wake and leave, same epoch as the waiters' stamps.
    pb_trace_wake_at(handle, "waitForProcessExit0");
    ctx.end_blocking_region();
    pb_trace_leave(handle, "ProcessHandleImpl.waitForProcessExit0");
    Ok(Some(Value::Int(code)))
}

/// `waitForProcessExit0` for a pid this VM did not spawn, on Windows, as
/// HotSpot 25's `ProcessHandleImpl_win.c` answers it (interpreter round i5
/// wave 100, lane L1): -1 when the process cannot be opened (gone, or access
/// denied); otherwise the exit code, after waiting on the process handle.
/// The wait is one `InNative` blocking region (HotSpot: RUNNABLE in the
/// native), in 1 s slices; HotSpot's own wait also wakes on the thread's
/// interrupt event and simply waits again, so an interrupt does not end it
/// here either. One deliberate difference: a process whose exit code IS
/// `STILL_ACTIVE` (259) is answered once its handle is signalled, where
/// HotSpot's `do { } while (exitValue == STILL_ACTIVE)` would spin on it. A
/// failed `GetExitCodeProcess` (HotSpot: `RuntimeException`) answers -1.
#[cfg(windows)]
fn windows_wait_foreign_process_exit(ctx: &mut dyn NativeContext, pid: i64) -> i32 {
    use win_process_wait_ffi::*;
    if pid <= 0 || pid > u32::MAX as i64 {
        return -1;
    }
    // SAFETY: scalars in, a handle out; null is the documented failure and is
    // never used.
    let raw = unsafe { OpenProcess(SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION, 0, pid as u32) };
    if raw.is_null() {
        return -1;
    }
    let mut in_region = false;
    let answer = loop {
        let mut code: u32 = 0;
        // SAFETY: `raw` is the open handle above; `code` is a live local.
        if unsafe { GetExitCodeProcess(raw, &mut code) } == 0 {
            break -1;
        }
        if code as i32 != WINDOWS_STILL_ACTIVE {
            break code as i32;
        }
        if !in_region {
            ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::InNative);
            in_region = true;
        }
        // SAFETY: `raw` is the open handle above.
        if unsafe { WaitForSingleObject(raw, 1000) } == WAIT_OBJECT_0 {
            let mut exited: u32 = 0;
            // SAFETY: as above.
            break if unsafe { GetExitCodeProcess(raw, &mut exited) } == 0 {
                -1
            } else {
                exited as i32
            };
        }
    };
    if in_region {
        ctx.end_blocking_region();
    }
    // SAFETY: `raw` was opened above and is closed exactly once.
    unsafe { CloseHandle(raw) };
    answer
}

/// `java.lang.ProcessHandleImpl.destroyProcess0(long, boolean) -> boolean`
fn native_proc_handle_destroy_process0(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Interpreter round i5 wave 101, lane L1: this VM's process tables.
    let t = &tables(ctx);
    // Keyed by pid, like the rest of this bridge -- see `handle_for_pid`.
    let pid = match args.first() {
        Some(Value::Long(p)) => *p,
        _ => return Ok(Some(Value::Int(0))),
    };
    let force = matches!(args.get(1), Some(Value::Int(1)));
    // Refusing to signal a process we did not spawn is deliberate: this bridge
    // has no ownership check of its own, and `false` is a legal answer
    // ("could not be destroyed"), unlike killing the wrong pid.
    let ok = match handle_for_pid(t, pid) {
        Some(handle) => destroy_handle(t, handle, force),
        None => false,
    };
    Ok(Some(Value::Int(if ok { 1 } else { 0 })))
}

/// `java.lang.Process.waitFor()I` on our synthetic Process object.
fn native_process_wait_for(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Interpreter round i5 wave 101, lane L1: this VM's process tables.
    let t = &tables(ctx);
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
    // Interpreter round i2 wave 68, lane L1 (`CRATONVM_STAND_IN_WAIT_OBJECTS`):
    // the condition the JDK body parks on, made while still running (it runs
    // Java, so `this` is re-read from a pin); see [`process_wait_condition`].
    // Wave 69 (lane L1): the process's one condition, see
    // [`process_wait_condition_of`].
    let condition = process_wait_condition_of(ctx, &mut this);
    let mut held = [Value::Object(Some(this))];
    // Interpreter round i2 wave 59, lane L6: what the JDK's own `ProcessImpl.waitFor`
    // reports -- on Windows it blocks in the native `waitForInterruptibly`
    // (HotSpot MEASURED: RUNNABLE, `L6W59BlockedNativeStates`), elsewhere in
    // `Object.wait` (WAITING). Interpreter round i2 wave 66, lane L1 said
    // `ObjectWait`, reading the UNIX body as `synchronized ... wait()`; the
    // host run corrected it (JDK 25.0.4: `Process.waitFor: WAITING
    // jmx-same=true lock=java.util.concurrent.locks.AbstractQueuedSynchronizer$ConditionObject`):
    // JDK 25's UNIX `ProcessImpl.waitFor` awaits a `ReentrantLock` condition,
    // so the thread PARKS. Interpreter round i2 wave 67, lane L1
    // (`CRATONVM_CLASSIFIED_STAND_IN_WAITS`, default on): `Parked` (WAITING
    // as before; with `CRATONVM_PUBLISHED_ALL_WAIT_KINDS` JVMTI `PARKED`);
    // `=0` the wave-66 `ObjectWait`. Wave 68 (lane L1): the region publishes
    // the condition as the thread's park blocker, which JMX names
    // (`i66-L1-process-waitfor-names-no-lock-20261030.md`).
    let kind = if cfg!(windows) {
        cratonvm_native_api::BlockKind::InNative
    } else if classified_stand_in_waits() {
        cratonvm_native_api::BlockKind::Parked
    } else {
        cratonvm_native_api::BlockKind::ObjectWait
    };
    // Interpreter round i3 wave 80, lane L6
    // (`i70-L1-synthetic-process-waitfor-ignores-interrupt-20261103.md`): the
    // JDK body is interruptible (UNIX `condition.await()`, Windows
    // `waitForInterruptibly` + `Thread.interrupted()`), so the wait is the
    // exit signal in slices with the interrupt status read between them, not
    // `Child::wait` (which nothing but the child's exit wakes).
    if let Some(code) = try_exit_handle(t, handle) {
        // An exited child answers at once on UNIX (`while (!hasExited)` never
        // awaits); Windows' body still tests `Thread.interrupted()` after its
        // (immediate) native wait.
        if cfg!(windows) && ctx.is_interrupted(false) {
            return Err(process_wait_interrupted(ctx));
        }
        ctx.set_field(this, PROC_FIELD_EXIT, Value::Int(code));
        return Ok(Some(Value::Int(code)));
    }
    if ctx.is_interrupted(false) {
        // `ConditionObject.await` throws on entry when already interrupted.
        return Err(process_wait_interrupted(ctx));
    }
    // Interpreter round i4 wave 97, lane L1: on Windows the child's process
    // handle (the JDK's `waitForInterruptibly`), elsewhere the exit signal;
    // see [`ExitWait`].
    let exit_wait = ExitWait::for_handle(t, handle, true);
    match condition {
        Some(condition) => ctx.begin_blocking_region_as_on(condition, kind),
        None => ctx.begin_blocking_region_as(kind),
    }
    let code = loop {
        // Read before the test (see [`process_exit_generation`]).
        let seen = exit_wait.generation();
        if let Some(code) = try_exit_handle(t, handle) {
            break Some(code);
        }
        // An atomic read only: legal inside the region.
        if ctx.is_interrupted(false) {
            break None;
        }
        // Wave 102 (lane L1, `CRATONVM_PROCESS_WAIT_DIRECT_WAKE`): the wait
        // object's own answer, without the re-test.
        if let Some(code) =
            exit_wait_slice(ctx, t, &exit_wait, handle, seen, Duration::from_millis(10))
        {
            if direct_wake(t) {
                break Some(code);
            }
        }
    };
    ctx.end_blocking_region_refs(&mut held);
    pb_trace_leave(handle, "Process.waitFor stand-in");
    if let Value::Object(Some(updated)) = held[0] {
        this = updated;
    }
    let Some(code) = code else {
        return Err(process_wait_interrupted(ctx));
    };
    ctx.set_field(this, PROC_FIELD_EXIT, Value::Int(code));
    Ok(Some(Value::Int(code)))
}

/// The `InterruptedException` a VM-spawned child's `waitFor` stand-ins raise
/// when the waiter is interrupted (interpreter round i3 wave 80, lane L6): the
/// JDK bodies throw `new InterruptedException()` (no message) -- UNIX from
/// `AbstractQueuedSynchronizer.ConditionObject.await` / `awaitNanos`, Windows
/// from `ProcessImpl.waitFor` itself -- having consumed the interrupt status,
/// so this clears it first. A debugger's pending `ThreadReference.Stop`
/// replaces it, as for `Thread.sleep`. Called outside any blocking region
/// (clearing the status writes the `Thread` mirror's field).
fn process_wait_interrupted(ctx: &mut dyn NativeContext) -> MethodCallFailed {
    let _ = ctx.is_interrupted(true);
    if let Some(stop) = ctx.take_debugger_stop() {
        return MethodCallFailed::ExceptionThrown(stop);
    }
    match ctx.new_object_initialized("java/lang/InterruptedException", "()V", &[]) {
        Ok(Some(Value::Object(Some(exc)))) => MethodCallFailed::ExceptionThrown(exc),
        Err(failed @ MethodCallFailed::ExceptionThrown(_)) => failed,
        _ => MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::InterruptedException)),
    }
}

/// The helpful NPE message of `unit.toNanos(timeout)` in the JDK's
/// `waitFor(long, TimeUnit)` bodies with a null `unit` (java.base carries its
/// `LocalVariableTable`, so HotSpot names the parameter; the same text as
/// `CountDownLatch.await(1, null)`, `L1W71CountDownLatchBodies`).
const WAIT_FOR_NULL_UNIT_NPE: &str =
    "Cannot invoke \"java.util.concurrent.TimeUnit.toNanos(long)\" because \"unit\" is null";

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
    // Interpreter round i5 wave 101, lane L1: this VM's process tables.
    let t = &tables(ctx);
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
    // Interpreter round i5 wave 102, lane L1: every JDK body this stands for
    // (Windows and UNIX `ProcessImpl.waitFor(long, TimeUnit)`, the default
    // `Process.waitFor(long, TimeUnit)` a subclass inherits) starts with
    // `unit.toNanos(timeout); // throw NPE before other conditions`. A null
    // unit was read as milliseconds: an exited child answered `true`, a zero
    // timeout `false`, and a running child waited.
    if unit.is_none() {
        return Err(RuntimeError::NullPointerException {
            message: Some(WAIT_FOR_NULL_UNIT_NPE.to_string()),
        }
        .into());
    }

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
        // `end_blocking_region_refs` refreshes `this` across the SLEEP, and
        // nothing refreshes it across `foreign_exit_value` -- which is the
        // comment below's own reason for keeping that call outside the blocked
        // region: it runs arbitrary application bytecode. That bytecode can
        // collect, and the NEXT turn then hands the pre-GC address to
        // `begin_blocking_region`. Pin for the poll and re-read at the top.
        let this_pin = ctx.pin_native_root(this);
        loop {
            this = ctx.read_native_pin(this_pin, this);
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
            // Interpreter round i2 wave 67, lane L1
            // (`CRATONVM_CLASSIFIED_STAND_IN_WAITS`): `Process.waitFor(long,
            // TimeUnit)`'s default body (the one a `Process` subclass
            // inherits) loops on `Thread.sleep`, so the nap is a sleep:
            // TIMED_WAITING, JVMTI `SLEEPING`. `=0`: the plain WAITING region.
            ctx.begin_blocking_region_as(stand_in_wait_kind(
                cratonvm_native_api::BlockKind::Sleeping,
            ));
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

    if let Some(code) = try_exit_handle(t, handle) {
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
    // Interpreter round i2 wave 68, lane L1 (`CRATONVM_STAND_IN_WAIT_OBJECTS`):
    // the condition the JDK's `awaitNanos` parks on, made once for the whole
    // wait while still running (see [`process_wait_condition`]); carried
    // across each slice's region in `held`, so a collection forwards it.
    // Wave 69 (lane L1): the process's one condition, see
    // [`process_wait_condition_of`].
    let mut condition = process_wait_condition_of(ctx, &mut this);
    // Interpreter round i2 wave 67, lane L1: a deadline past `Instant`'s range
    // is still the JDK's timed wait (see [`process_timed_wait_kind`]).
    // Interpreter round i3 wave 80, lane L6: it waits in the same slices as a
    // finite one (`None` = no deadline), so it sees an interrupt too.
    let deadline = Instant::now().checked_add(Duration::from_millis(timeout_ms));

    // Interpreter round i2 wave 70, lane L1 (`CRATONVM_PROCESS_EXIT_SIGNAL`,
    // default off): wake on the child's exit instead of the next 10 ms slice.
    // Interpreter round i4 wave 97, lane L1: on Windows the wait is the
    // child's process handle, one wake hop as on HotSpot (see [`ExitWait`]).
    let exit_wait = ExitWait::for_handle(t, handle, process_exit_signal_on());
    // Interpreter round i5 wave 102, lane L1 (`CRATONVM_PROCESS_WAIT_DIRECT_WAKE`,
    // default off): every slice inside ONE blocking region, as the untimed
    // stand-in and the `--jdk-only` Windows native wait, instead of a leave +
    // enter (a root deposit) per 10 ms slice, and out with the wait object's
    // own answer. The tests run in the same order as the loop below (exit,
    // interrupt -- an atomic read, legal inside the region -- then the
    // deadline); the exception is made after the region ends.
    if direct_wake(t) {
        enum Outcome {
            Exited(i32),
            Interrupted,
            TimedOut,
        }
        let mut held = [Value::Object(Some(this)), Value::Object(condition)];
        match condition {
            Some(condition) => {
                ctx.begin_blocking_region_as_on(condition, process_timed_wait_kind())
            }
            None => ctx.begin_blocking_region_as(process_timed_wait_kind()),
        }
        let outcome = loop {
            let seen = exit_wait.generation();
            if let Some(code) = try_exit_handle(t, handle) {
                break Outcome::Exited(code);
            }
            if ctx.is_interrupted(false) {
                break Outcome::Interrupted;
            }
            let now = Instant::now();
            let remaining = match deadline {
                Some(deadline) if now >= deadline => break Outcome::TimedOut,
                Some(deadline) => deadline.saturating_duration_since(now),
                None => Duration::from_millis(10),
            };
            let slice = remaining.min(Duration::from_millis(10));
            if let Some(code) = exit_wait_slice(ctx, t, &exit_wait, handle, seen, slice) {
                break Outcome::Exited(code);
            }
        };
        ctx.end_blocking_region_refs(&mut held);
        pb_trace_leave(handle, "Process.waitFor(long, TimeUnit) stand-in (one region)");
        if let Value::Object(Some(updated)) = held[0] {
            this = updated;
        }
        return match outcome {
            Outcome::Exited(code) => {
                ctx.set_field(this, PROC_FIELD_EXIT, Value::Int(code));
                Ok(Some(Value::Int(1)))
            }
            Outcome::Interrupted => Err(process_wait_interrupted(ctx)),
            Outcome::TimedOut => Ok(Some(Value::Int(0))),
        };
    }
    loop {
        // Read before the test, so an exit published after it still wakes
        // the wait below (see [`process_exit_generation`]).
        let seen = exit_wait.generation();
        if let Some(code) = try_exit_handle(t, handle) {
            ctx.set_field(this, PROC_FIELD_EXIT, Value::Int(code));
            return Ok(Some(Value::Int(1)));
        }
        // Interpreter round i3 wave 80, lane L6
        // (`i70-L1-synthetic-process-waitfor-ignores-interrupt-20261103.md`):
        // the JDK body is interruptible (UNIX `condition.awaitNanos`, which
        // throws on entry and on an interrupt while parked; Windows
        // `waitForTimeoutInterruptibly` + `Thread.interrupted()`).
        if ctx.is_interrupted(false) {
            return Err(process_wait_interrupted(ctx));
        }

        let now = Instant::now();
        let remaining = match deadline {
            Some(deadline) if now >= deadline => return Ok(Some(Value::Int(0))),
            Some(deadline) => deadline.saturating_duration_since(now),
            None => Duration::from_millis(10),
        };
        let mut held = [Value::Object(Some(this)), Value::Object(condition)];
        // Interpreter round i2 wave 67, lane L1: see [`process_timed_wait_kind`].
        // Wave 68 (lane L1): publishing the condition, as above.
        match condition {
            Some(condition) => {
                ctx.begin_blocking_region_as_on(condition, process_timed_wait_kind())
            }
            None => ctx.begin_blocking_region_as(process_timed_wait_kind()),
        }
        let slice = remaining.min(Duration::from_millis(10));
        let _ = exit_wait_slice(ctx, t, &exit_wait, handle, seen, slice);
        ctx.end_blocking_region_refs(&mut held);
        pb_trace_leave(handle, "Process.waitFor(long, TimeUnit) stand-in");
        if let Value::Object(Some(updated)) = held[0] {
            this = updated;
        }
        if let Value::Object(Some(updated)) = held[1] {
            condition = Some(updated);
        }
    }
}

/// The object a VM-spawned child's `waitFor` stand-ins publish as the
/// thread's park blocker (interpreter round i2 wave 68, lane L1,
/// `CRATONVM_STAND_IN_WAIT_OBJECTS`, default on;
/// `docs/feature-designs/jdk-only-mode.md`).
///
/// JDK 25's UNIX `ProcessImpl.waitFor()` / `waitFor(long, TimeUnit)` await a
/// `ReentrantLock` condition, so HotSpot's `ThreadInfo.getLockName()` reads
/// `java.util.concurrent.locks.AbstractQueuedSynchronizer$ConditionObject@..`
/// (measured on the host, JDK 25.0.4). The synthetic process carries no such
/// condition, so this makes one the way the JDK does,
/// `new ReentrantLock().newCondition()`: a real `ConditionObject` a debugger
/// or `LockSupport.getBlocker` can use, which no one signals (the stand-in
/// waits on the child, not on it). Made once per process since wave 69
/// ([`process_wait_condition_of`] keeps it in [`PROC_FIELD_CONDITION`]);
/// `CRATONVM_PROCESS_ONE_WAIT_CONDITION=0`: one per `waitFor` call, so two
/// calls name two identity hashes where HotSpot names one.
///
/// `None` on Windows (HotSpot blocks in the native `waitForInterruptibly`,
/// RUNNABLE, no lock), with `CRATONVM_CLASSIFIED_STAND_IN_WAITS=0` (the region
/// is not a park), with the switch off, or when making it fails (then the
/// region publishes nothing, as before). Runs Java: the caller pins what it
/// holds.
fn process_wait_condition(ctx: &mut dyn NativeContext) -> Option<ObjectRef> {
    if cfg!(windows) || !classified_stand_in_waits() || !ctx.stand_in_wait_objects() {
        return None;
    }
    let lock =
        match ctx.new_object_initialized("java/util/concurrent/locks/ReentrantLock", "()V", &[]) {
            Ok(Some(Value::Object(Some(lock)))) => lock,
            _ => return None,
        };
    match ctx.invoke_virtual(
        lock,
        "newCondition",
        "()Ljava/util/concurrent/locks/Condition;",
        &[],
    ) {
        Ok(Some(Value::Object(Some(condition)))) => Some(condition),
        _ => None,
    }
}

/// The condition a VM-spawned child's `waitFor` stand-in publishes for
/// `this` (interpreter round i2 wave 69, lane L1,
/// `CRATONVM_PROCESS_ONE_WAIT_CONDITION`, default on; builds on
/// `CRATONVM_STAND_IN_WAIT_OBJECTS`;
/// `i68-L1-proposal-one-condition-per-synthetic-process-20261101.md`).
///
/// JDK 25's UNIX `ProcessImpl` owns one `ReentrantLock` and one `condition`
/// (final fields), so every `waitFor` caller of one process, concurrent or
/// one after another, parks on the same `ConditionObject` and HotSpot's
/// `ThreadInfo.getLockName()` names the same identity hash for all of them.
/// The first call that publishes makes the condition
/// ([`process_wait_condition`], which runs Java: `this` is pinned across it
/// and re-read, hence `&mut`) and installs it in [`PROC_FIELD_CONDITION`]
/// with a compare-and-swap, so two first callers racing agree on the
/// winner's; every later call reads the slot and runs no Java. The slot is a
/// reference the process object keeps (and a collection forwards).
///
/// `=0`: a fresh condition per call, as in wave 68. `None` wherever
/// [`process_wait_condition`] answers `None` (nothing is published then, and
/// the slot is not read).
fn process_wait_condition_of(
    ctx: &mut dyn NativeContext,
    this: &mut ObjectRef,
) -> Option<ObjectRef> {
    if cfg!(windows) || !classified_stand_in_waits() || !ctx.stand_in_wait_objects() {
        return None;
    }
    let one = process_one_wait_condition();
    if one {
        if let Value::Object(Some(kept)) = ctx.get_field(*this, PROC_FIELD_CONDITION) {
            return Some(kept);
        }
    }
    let made = {
        let this_pin = ctx.pin_native_root(*this);
        let made = process_wait_condition(ctx);
        *this = ctx.read_native_pin(this_pin, *this);
        ctx.unpin_native_roots(this_pin);
        made
    }?;
    if !one {
        return Some(made);
    }
    Some(install_process_wait_condition(ctx, *this, made))
}

/// Install `made` as `this`'s one wait condition unless another `waitFor`
/// got there first, and answer the one that is installed (`made` when the
/// slot cannot be read back: the caller still publishes a real condition).
/// No Java runs, so `made` and `this` stay valid across it.
fn install_process_wait_condition(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    made: ObjectRef,
) -> ObjectRef {
    if ctx.compare_and_swap_field(
        this,
        PROC_FIELD_CONDITION,
        Value::Object(None),
        Value::Object(Some(made)),
    ) {
        return made;
    }
    match ctx.get_field(this, PROC_FIELD_CONDITION) {
        Value::Object(Some(kept)) => kept,
        _ => made,
    }
}

/// `CRATONVM_PROCESS_ONE_WAIT_CONDITION` (interpreter round i2 wave 69, lane
/// L1; default on, `=0` is the kill switch): see
/// [`process_wait_condition_of`].
fn process_one_wait_condition() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_PROCESS_ONE_WAIT_CONDITION")
}

/// `CRATONVM_CLASSIFIED_STAND_IN_WAITS` (interpreter round i2 wave 67, lane
/// L1; default on, `=0` is the kill switch;
/// `docs/feature-designs/jdk-only-mode.md`):
/// the `Process.waitFor` stand-ins say the kind of the JDK code they stand
/// for instead of opening a kind-less WAITING region.
fn classified_stand_in_waits() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_CLASSIFIED_STAND_IN_WAITS")
}

/// `kind` with [`classified_stand_in_waits`]; otherwise the kind-less
/// [`BlockKind::Waiting`](cratonvm_native_api::BlockKind::Waiting) region the
/// site opened before wave 67 (which the VM's unrecorded-wait census counts).
fn stand_in_wait_kind(kind: cratonvm_native_api::BlockKind) -> cratonvm_native_api::BlockKind {
    if classified_stand_in_waits() {
        kind
    } else {
        cratonvm_native_api::BlockKind::Waiting
    }
}

/// The kind of a `Process.waitFor(long, TimeUnit)` wait on a VM-spawned child
/// (interpreter round i2 wave 67, lane L1). JDK 25's UNIX `ProcessImpl`
/// awaits its `ReentrantLock` condition with `awaitNanos` (the untimed
/// `waitFor` measured WAITING on `AbstractQueuedSynchronizer$ConditionObject`
/// on the host), so HotSpot reads TIMED_WAITING / `PARKED`: `TimedParked`.
/// The Windows `ProcessImpl` blocks in the native `waitForTimeoutInterruptibly`
/// (RUNNABLE, as the untimed `waitForInterruptibly` was measured in
/// `L6W59BlockedNativeStates`): `InNative`. `=0`: the plain WAITING region.
fn process_timed_wait_kind() -> cratonvm_native_api::BlockKind {
    stand_in_wait_kind(if cfg!(windows) {
        cratonvm_native_api::BlockKind::InNative
    } else {
        cratonvm_native_api::BlockKind::TimedParked
    })
}

#[cfg(test)]
mod i67_process_wait_kind_tests {
    use super::{classified_stand_in_waits, process_timed_wait_kind, stand_in_wait_kind};
    use cratonvm_native_api::BlockKind;

    /// Interpreter round i2 wave 67, lane L1 (`CRATONVM_CLASSIFIED_STAND_IN_WAITS`):
    /// the timed `waitFor` stands for a condition `awaitNanos` (UNIX) or the
    /// native timed wait (Windows), a foreign process's poll for a sleep;
    /// `=0` restores the kind-less WAITING region.
    #[test]
    fn the_timed_wait_for_stands_for_its_jdk_body() {
        assert!(classified_stand_in_waits());
        let timed = if cfg!(windows) {
            BlockKind::InNative
        } else {
            BlockKind::TimedParked
        };
        assert_eq!(process_timed_wait_kind(), timed);
        assert_eq!(stand_in_wait_kind(BlockKind::Sleeping), BlockKind::Sleeping);
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_CLASSIFIED_STAND_IN_WAITS", Some("0"))],
            || {
                assert!(!classified_stand_in_waits());
                assert_eq!(process_timed_wait_kind(), BlockKind::Waiting);
                assert_eq!(stand_in_wait_kind(BlockKind::Sleeping), BlockKind::Waiting);
            },
        );
    }

    /// Interpreter round i2 wave 68, lane L1 (`CRATONVM_STAND_IN_WAIT_OBJECTS`):
    /// a context that does not publish wait objects (the trait default, as
    /// the VM with `=0`) makes no condition, so the `waitFor` regions run no
    /// Java and publish nothing, as before.
    #[test]
    fn no_condition_is_made_where_wait_objects_are_not_published() {
        use cratonvm_native_api::NativeThreadAccess;
        let mut ctx = crate::test_support::MockNativeContext::new();
        assert!(!ctx.stand_in_wait_objects());
        assert_eq!(super::process_wait_condition(&mut ctx), None);
    }

    /// Interpreter round i2 wave 69, lane L1 (`CRATONVM_PROCESS_ONE_WAIT_CONDITION`):
    /// the switch is on by default with an `=0` kill switch; a context that
    /// publishes nothing neither makes nor reads the process's condition.
    #[test]
    fn the_one_condition_switch_and_the_unpublished_arm() {
        assert!(super::process_one_wait_condition());
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_PROCESS_ONE_WAIT_CONDITION", Some("0"))],
            || assert!(!super::process_one_wait_condition()),
        );
        let mut ctx = crate::test_support::MockNativeContext::new();
        let mut process =
            ctx.alloc_object_with_class(super::PROC_FIELD_COUNT, super::SYNTHETIC_PROCESS_CLASS);
        let before = process;
        assert_eq!(
            super::process_wait_condition_of(&mut ctx, &mut process),
            None
        );
        assert_eq!(process, before);
    }

    /// Wave 69: a condition already installed in the process's slot wins over
    /// the one a racing first caller made; an empty slot keeps the caller's.
    /// The slot is past the six reserved `java.lang.Process` slots and the six
    /// bookkeeping slots, inside the layout.
    #[test]
    fn the_installed_condition_is_the_processs_one() {
        use cratonvm_native_api::NativeHeapAccess;
        assert_eq!(
            super::PROC_FIELD_CONDITION,
            super::JAVA_PROCESS_FIELD_COUNT + 6
        );
        assert!(super::PROC_FIELD_CONDITION < super::PROC_FIELD_COUNT);
        let mut ctx = crate::test_support::MockNativeContext::new();
        let process =
            ctx.alloc_object_with_class(super::PROC_FIELD_COUNT, super::SYNTHETIC_PROCESS_CLASS);
        let made = ctx.alloc_object(0);
        // The mock's compare-and-swap never succeeds, so an empty slot reads
        // back empty: the caller keeps (and publishes) its own.
        assert_eq!(
            super::install_process_wait_condition(&mut ctx, process, made),
            made
        );
        let kept = ctx.alloc_object(0);
        ctx.set_field(
            process,
            super::PROC_FIELD_CONDITION,
            cratonvm_types::Value::Object(Some(kept)),
        );
        assert_eq!(
            super::install_process_wait_condition(&mut ctx, process, made),
            kept
        );
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
    // Interpreter round i5 wave 101, lane L1: this VM's process tables.
    let t = &tables(ctx);
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
        try_exit_handle(t, handle)
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
        // Wave 79, lane L6: in native, as the reaper below.
        ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::InNative);
        let code = wait_for_handle(t, handle);
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
    // Interpreter round i5 wave 101, lane L1: this VM's process tables.
    let t = &tables(ctx);
    let process_pin = ctx.pin_native_root(process);
    let future_pin = ctx.pin_native_root(future);

    // Fallible since 2026-08-10 (JDK-only wave 2, step 3), and the refusal is
    // ABSORBED here rather than propagated, deliberately: this function's
    // failure channel is `false`, which it already answers when the reaper
    // thread cannot be created, and the caller's documented response is to
    // complete the future itself. A refused exit-waiter shape gets that same
    // answer. `refused_class` has recorded the violation upstream, so a strict
    // census still reports the request.
    let waiter_class = match ctx
        .try_ensure_synthetic_class(SYNTHETIC_PROCESS_EXIT_WAITER, EXIT_WAITER_FIELD_COUNT)
    {
        Ok(cid) => cid,
        Err(_) => {
            ctx.unpin_native_roots(process_pin);
            return false;
        }
    };
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
    // Interpreter round i3 wave 80, lane L6 (page
    // `i67-L1-process-exit-waiter-reads-waiting`): HotSpot's pool thread is
    // `process reaper` and renames itself for the wait (JDK 25
    // `ProcessHandleImpl.completion`: `t.setName("process reaper (pid " + pid
    // + ")")`); this thread lives for that wait only, so it carries the wait's
    // name from the start. See [`exit_waiter_thread_name`].
    let name = ctx.create_string(&exit_waiter_thread_name(pid_for_handle(t, handle)));
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
    // Wave 80 (lane L6): `Thread.MAX_PRIORITY`, as the JDK's
    // `InnocuousThread.newSystemThread("process reaper", .., MAX_PRIORITY)`.
    let _ = ctx.invoke_virtual(thread, "setPriority", "(I)V", &[Value::Int(10)]);
    let thread = ctx.read_native_pin(thread_pin, thread);
    ctx.unpin_native_roots(thread_pin);
    ctx.invoke_virtual(thread, "start", "()V", &[]).is_ok()
}

/// The name of the `onExit()` reaper thread for a child of `pid` (interpreter round i3
/// wave 80, lane L6): HotSpot's `process reaper (pid <pid>)`, the name JDK 25's
/// reaper carries while it waits; `process reaper` when the pid is unknown
/// (the JDK pool thread's own name).
fn exit_waiter_thread_name(pid: i64) -> String {
    if pid < 0 {
        "process reaper".to_string()
    } else {
        format!("process reaper (pid {pid})")
    }
}

/// `run()` of the `onExit()` reaper thread — blocks on the child, then
/// completes the future with the `Process`.
fn native_process_exit_waiter_run(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Interpreter round i5 wave 101, lane L1: this VM's process tables.
    let t = &tables(ctx);
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
    // Interpreter round i3 wave 79, lane L6 (page
    // `i67-L1-process-exit-waiter-reads-waiting`): HotSpot's `process reaper`
    // waits in the native `waitForProcessExit0` (`_thread_in_native`:
    // RUNNABLE, JMX `isInNative()` true); the plain door read WAITING.
    ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::InNative);
    let code = wait_for_handle(t, handle);
    // Interpreter round i5 wave 106, lane L1 (i97 attribution).
    pb_trace_wake_at(handle, "onExit-waiter");
    ctx.end_blocking_region_refs(&mut held);
    pb_trace_leave(handle, "Process.onExit waiter");

    let (Value::Object(Some(process)), Value::Object(Some(future))) = (held[0], held[1]) else {
        return Ok(None);
    };
    complete_exit_future(ctx, process, future, code)?;
    // `complete` returned: every dependent ran or was handed off, and the
    // `get()` waiter was unparked inside it.
    pb_trace_leave(handle, "Process.onExit waiter completed");
    Ok(None)
}

/// `java.lang.Process.exitValue()I`
///
/// Throws `IllegalThreadStateException` if the process is still running
/// (matches HotSpot behavior).
fn native_process_exit_value(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Interpreter round i5 wave 101, lane L1: this VM's process tables.
    let t = &tables(ctx);
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let handle = handle_of(ctx, this);
    if handle == 0 {
        return Ok(Some(ctx.get_field(this, PROC_FIELD_EXIT)));
    }
    match try_exit_handle(t, handle) {
        Some(code) => {
            ctx.set_field(this, PROC_FIELD_EXIT, Value::Int(code));
            Ok(Some(Value::Int(code)))
        }
        // Interpreter round i3 wave 80, lane L6: the JDK 25 text per platform
        // -- UNIX `ProcessImpl.exitValue` throws "process hasn't exited",
        // Windows' "process has not exited" (this read the Windows text on
        // every platform).
        None => Err(RuntimeError::IllegalThreadStateException {
            message: exit_value_not_exited_message().to_string(),
        }
        .into()),
    }
}

/// The `IllegalThreadStateException` message of `exitValue()` on a running
/// child: JDK 25's UNIX `ProcessImpl` says "process hasn't exited", the
/// Windows one "process has not exited" (interpreter round i3 wave 80, lane L6).
fn exit_value_not_exited_message() -> &'static str {
    if cfg!(windows) {
        "process has not exited"
    } else {
        "process hasn't exited"
    }
}

/// `java.lang.Process.isAlive()Z`
fn native_process_is_alive(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Interpreter round i5 wave 101, lane L1: this VM's process tables.
    let t = &tables(ctx);
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
    let alive = try_exit_handle(t, handle).is_none();
    Ok(Some(Value::Int(if alive { 1 } else { 0 })))
}

/// `java.lang.Process.destroy()V`
fn native_process_destroy(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Interpreter round i5 wave 101, lane L1: this VM's process tables.
    let t = &tables(ctx);
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let handle = handle_of(ctx, this);
    if handle != 0 {
        destroy_handle(t, handle, false);
    }
    Ok(None)
}

/// `java.lang.Process.destroyForcibly()Ljava/lang/Process;`
fn native_process_destroy_forcibly(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Interpreter round i5 wave 101, lane L1: this VM's process tables.
    let t = &tables(ctx);
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(args.first().copied()),
    };
    // GC: rooted across the call below; `safe_native_call` releases
    // the pin stack to its entry floor on return.
    let this_pin = ctx.pin_native_root(this);
    if !is_vm_process(ctx, this) {
        // `Process.destroyForcibly()` is `destroy(); return this;` — and
        // `destroy()` is abstract, so this reaches the subclass's override.
        ctx.invoke_virtual(this, "destroy", "()V", &[])?;
        let this = ctx.read_native_pin(this_pin, this);
        return Ok(Some(Value::Object(Some(this))));
    }
    let handle = handle_of(ctx, this);
    if handle != 0 {
        destroy_handle(t, handle, true);
    }
    Ok(Some(Value::Object(Some(this))))
}

/// `java.lang.Process.toHandle()Ljava/lang/ProcessHandle;` (JDK 9+).
///
/// Was entirely unregistered here, so any CratonVM-backed `Process` (real
/// or synthetic-JDK mode alike, since `spawn_and_wrap` always allocates the
/// object under `SYNTHETIC_PROCESS_CLASS`/`java/lang/Process` regardless of
/// mode) threw `NoSuchMethodError` on `.toHandle()`
/// (wildfly-process-tohandle-missing-FIXED.md).
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
    Ok(Some(Value::Object(Some(build_process_handle(ctx, pid)?))))
}

/// Build a 1-field `java/lang/ProcessHandle` (field 0 = pid) — the exact
/// layout `ProcessHandle.current()` in `phases_late.rs` already
/// establishes, so the `pid()`/`isAlive()` natives registered there
/// dispatch correctly against it regardless of real-JDK vs synthetic mode.
/// Mirrors `native-builtins`'s `alloc_concurrent_synthetic` (not reusable
/// here directly: `native-io` sits below `native-builtins` in the crate
/// dependency graph).
fn alloc_process_handle(
    ctx: &mut dyn NativeContext,
    pid: Value,
) -> Result<ObjectRef, MethodCallFailed> {
    let obj: ObjectRef = match ctx.ensure_class_initialized("java/lang/ProcessHandle") {
        Ok(cid) => {
            let real = ctx.class_num_total_fields(cid);
            let n = 1usize.max(real);
            ctx.alloc_object(cid, n)
        }
        // Fallible since 2026-08-10 (JDK-only wave 2, step 3): a 1-field
        // stand-in for the `java.lang.ProcessHandle` INTERFACE is a
        // compatibility substitution. On a complete image the `Ok` arm runs.
        Err(_) => {
            let cid = crate::refused_class(ctx, "java/lang/ProcessHandle", 1)?;
            ctx.alloc_object(cid, 1)
        }
    };
    ctx.set_field(obj, 0, pid);
    Ok(obj)
}

/// Build a ProcessHandle for a bare pid, preferring a real
/// java.lang.ProcessHandleImpl(pid, startTime) and falling back to the
/// 1-field synthetic java/lang/ProcessHandle layout when no real class is
/// loadable. Shared by Process.toHandle() and Process.descendants().
fn build_process_handle(
    ctx: &mut dyn NativeContext,
    pid: i64,
) -> Result<ObjectRef, MethodCallFailed> {
    match ctx.new_object_initialized(
        "java/lang/ProcessHandleImpl",
        "(JJ)V",
        &[Value::Long(pid), Value::Long(0)],
    ) {
        Ok(Some(Value::Object(Some(handle)))) => Ok(handle),
        _ => alloc_process_handle(ctx, Value::Long(pid)),
    }
}

/// A process-enumeration syscall that FAILED, as opposed to one that honestly
/// found nothing.
///
/// The point of the type is that `Vec::new()` cannot express that difference.
/// `os_snapshot_processes` (Windows) used to answer with an empty vector both when
/// `CreateToolhelp32Snapshot` refused to take a snapshot and when the machine
/// genuinely had nothing to report, and every caller was infallible — so
/// `getProcessPids0` reported `0` found and `parent0` reported `-1`, which are
/// precisely the answers a machine running nothing and a process with no parent
/// produce. `ProcessHandle.allProcesses()` was then an empty stream, and
/// `ProcessHandle.parent()` an empty `Optional`, with nothing anywhere saying a
/// syscall had failed.
///
/// That is the campaign's dominant defect species — a fabricated success where
/// the spec mandates a failure — re-entering through the ERROR path after wave 3
/// removed it from the default arms (W2-7 is the species inventory; W6-10
/// finding 4 is this instance).
///
/// HotSpot is the oracle and does not do it: `ProcessHandleImpl_md.c` throws
/// `java.lang.RuntimeException` when `CreateToolhelp32Snapshot` returns
/// `INVALID_HANDLE_VALUE` and again when `Process32First` fails, and
/// `ProcessHandleImpl_unix.c` throws it when `opendir("/proc")` fails.
/// `allProcesses()` being *restricted* is documented and legal; a list
/// truncated by an error is a different thing and is not.
struct ProcessScanError {
    /// The `RuntimeException`'s message. Names the syscall AND carries the OS
    /// error, because "the snapshot failed" alone cannot be triaged from a log
    /// — `ERROR_ACCESS_DENIED` (a locked-down session) and `ERROR_BAD_LENGTH`
    /// (a snapshot taken while the process table churns, which the caller may
    /// legitimately retry) are the same sentence without it.
    detail: String,
}

impl ProcessScanError {
    fn new(detail: String) -> Self {
        Self { detail }
    }
}

/// Turn a failed process scan into the `java.lang.RuntimeException` HotSpot
/// raises for it.
///
/// `RuntimeError` (`types/src/error.rs`) has no plain `RuntimeException`
/// variant, and the nearest one it does have — `IllegalStateException` — is a
/// *subclass*: catchable by the same `catch`, but the wrong `getClass()`, and a
/// subclass is not good enough when the spec names a class. So the throwable is
/// constructed directly, which is how this crate already raises every exception
/// class `RuntimeError` cannot spell (`stream_decoder.rs`'s
/// `UnsupportedEncodingException`, `socket_channel.rs`'s `java.nio.channels`
/// family). No new mechanism.
///
/// GC: built by `nio_native::construct_thrown` (round 15 wave 4, lane
/// jdkonly4; `r15w3-jdkonly3-residual-new-object-init-builders-patch-FIXED-20260930.md`
/// #7), which roots the object and its message across `<init>` and throws the
/// forwarded reference; a throwing constructor is propagated. It was
/// `new_object` + an `invoke` whose failure was ignored (a half-built
/// exception thrown), with the detail interned into the never-pruned pool.
///
/// The `IllegalStateException` fallback fires only if `java/lang/RuntimeException`
/// itself cannot be constructed, i.e. when there is no `RuntimeException` to
/// throw at all; a thrown subclass still beats a fabricated empty list.
fn process_scan_exception(ctx: &mut dyn NativeContext, err: ProcessScanError) -> MethodCallFailed {
    let detail = ctx.create_string_uninterned(&err.detail);
    crate::nio_native::construct_thrown(
        ctx,
        "java/lang/RuntimeException",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(detail))],
        || {
            RuntimeError::IllegalStateException {
                message: err.detail,
            }
            .into()
        },
    )
}

/// Direct child pids of `pid`, sourced from /proc/<pid>/task/*/children —
/// every thread's children file is read since a child can be reparented to
/// any thread of a multi-threaded parent.
///
/// Fallible only in the sense the caller needs: an unreadable
/// `/proc/<pid>/task` is `Ok` with whatever was collected, because the node
/// being expanded is a single process that may simply have exited mid-walk —
/// the unix oracle does not throw for that either, and a walk that aborted
/// every time a descendant died would be useless. The `Err` arm exists for a
/// platform that has no probe AT ALL (below), where an empty answer would be
/// the fabrication this lane is removing.
#[cfg(target_os = "linux")]
fn direct_child_pids(pid: i64) -> Result<Vec<i64>, ProcessScanError> {
    let mut children = Vec::new();
    let task_dir = format!("/proc/{pid}/task");
    let Ok(entries) = std::fs::read_dir(&task_dir) else {
        return Ok(children);
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
    Ok(children)
}

/// No process-tree probe is wired up for this target.
///
/// The `Vec::new()` this replaces is the same fabrication wave 3 removed from
/// the Windows arm and this lane removes from the error path: "this VM cannot
/// enumerate processes here" and "this process has no children" are different
/// facts, and only one of them is true. Reporting it as a failure is the honest
/// answer until a `sysctl(KERN_PROC_ALL)` arm exists — HotSpot has one on
/// macOS/BSD and answers a real tree, so an empty stream is not oracle-faithful
/// either way.
#[cfg(not(any(target_os = "linux", windows)))]
fn direct_child_pids(_pid: i64) -> Result<Vec<i64>, ProcessScanError> {
    Err(ProcessScanError::new(
        "process enumeration is not implemented on this platform: \
         no /proc and no Toolhelp snapshot"
            .to_string(),
    ))
}

/// Every live descendant (children, grandchildren, ...) of `pid`, in
/// breadth-first discovery order — Win32.
///
/// **One machine-wide snapshot for the whole walk.** The Win32 primitive here
/// is `CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS)`, which enumerates ALL `N`
/// processes on the machine and hands back `th32ParentProcessID` for each — the
/// entire parent/child topology, in one call. The previous shape asked for that
/// whole snapshot once per node of the walk: `direct_child_pids(cur)` ->
/// `os_list_processes(cur)` -> `os_snapshot_processes()`, filtered down to the
/// one parent it cared about and threw the other `N - k` rows away. A tree with
/// `D` descendants therefore cost `1 + D` machine-wide snapshots — `O(N * D)`
/// work, and `1 + D` kernel snapshot allocations — for an answer the FIRST
/// snapshot already contained in full.
///
/// Indexing that one snapshot by parent pid makes the walk `O(N + D)` and costs
/// exactly ONE snapshot no matter how deep the tree is. Nothing about the
/// answer changes: `by_parent[cur]` holds precisely the rows
/// `os_list_processes(cur)` selected (`ppid == cur`), in the same snapshot
/// order, so the breadth-first sequence this returns is identical to the one
/// the per-node version produced.
///
/// It is still derived from [`os_snapshot_processes`], the single source
/// `os_parent_pid` and `os_list_processes` also read, so `Process.descendants()`
/// and `ProcessHandle.descendants()` cannot report different trees.
///
/// Linux is deliberately NOT restructured this way: its per-node probe is
/// `/proc/<pid>/task/*/children`, a direct read of the node being expanded
/// rather than a machine-wide scan, so there is no re-enumeration to hoist.
#[cfg(windows)]
fn collect_descendant_pids(pid: i64) -> Result<Vec<i64>, ProcessScanError> {
    let mut result = Vec::new();
    if pid <= 0 {
        return Ok(result);
    }
    // The one snapshot, turned into parent -> children before the walk starts.
    // `os_snapshot_processes` only emits rows with `pid > 0`, so every value
    // reachable through this map already satisfies `direct_child_pids`' old
    // `pid <= 0` guard.
    //
    // The `?` is the whole of finding 4 on this arm: a snapshot that FAILED
    // used to arrive here as an empty `Vec`, and an empty `by_parent` makes
    // every node a leaf — so `descendants()` reported a childless tree for a
    // process that had a subtree, and said nothing.
    let mut by_parent: std::collections::HashMap<i64, Vec<i64>> = std::collections::HashMap::new();
    for (child, parent) in os_snapshot_processes()? {
        by_parent.entry(parent).or_default().push(child);
    }
    let mut seen = std::collections::HashSet::new();
    seen.insert(pid);
    let mut queue = std::collections::VecDeque::new();
    queue.push_back(pid);
    while let Some(cur) = queue.pop_front() {
        // A pid with no row is a leaf; `continue` is the empty-vec case the
        // per-node version reached by way of an empty `os_list_processes`.
        let Some(children) = by_parent.get(&cur) else {
            continue;
        };
        for child in children {
            if seen.insert(*child) {
                result.push(*child);
                queue.push_back(*child);
            }
        }
    }
    Ok(result)
}

/// Every live descendant (children, grandchildren, ...) of `pid`, in
/// breadth-first discovery order — the same "descendants" contract as
/// java.lang.Process.descendants()/ProcessHandle.descendants().
///
/// The `?` propagates a probe that could not run at all (a platform with no
/// `/proc`); a node whose own `task/` directory is unreadable is an `Ok` empty
/// expansion inside [`direct_child_pids`], because that is a process that
/// exited, not a scan that failed.
#[cfg(not(windows))]
fn collect_descendant_pids(pid: i64) -> Result<Vec<i64>, ProcessScanError> {
    let mut result = Vec::new();
    let mut seen = std::collections::HashSet::new();
    seen.insert(pid);
    let mut queue = std::collections::VecDeque::new();
    queue.push_back(pid);
    while let Some(cur) = queue.pop_front() {
        for child in direct_child_pids(cur)? {
            if seen.insert(child) {
                result.push(child);
                queue.push_back(child);
            }
        }
    }
    Ok(result)
}

/// java.lang.Process.descendants() -> Stream<ProcessHandle> (JDK 9+).
///
/// Was entirely unregistered, so keycloak-test-framework's
/// ProcessUtils.getKeycloakPid() (which calls
/// keycloakProcess.descendants().toList() to tell apart the kc.sh wrapper
/// script's pid from the exec'd java process's pid) threw NoSuchMethodError
/// before a single test could start its managed Keycloak server — see
/// pom-xml-declaration-char-corruption-breaks-quarkus-maven-bootstrap-FIXED.md
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
    // GC: rooted across the call below; `safe_native_call` releases
    // the pin stack to its entry floor on return.
    let this_pin = ctx.pin_native_root(this);
    // `descendants()` is CONCRETE on `java.lang.Process` and — unlike
    // `isAlive`, `pid`, `toHandle`, `destroyForcibly` and
    // `waitFor(long, TimeUnit)` — it was never given the [`is_vm_process`]
    // guard those five carry, even though `is_vm_process`' own doc comment
    // says "every concrete `java.lang.Process` native now asks". It did not,
    // and the omission is measurable rather than theoretical:
    //
    // `javap -p java.lang.ProcessImpl` (JDK 25.0.3, Windows image) lists the
    // methods the real subclass overrides — `exitValue`, `waitFor()`,
    // `waitFor(long, TimeUnit)`, `destroy`, `onExit`, `toHandle`,
    // `supportsNormalTermination`, `destroyForcibly`, `pid`, `isAlive`, the
    // three stream getters, `toString` — and `descendants()` is NOT among
    // them. So under `--jdk-only`, where `ProcessBuilder.start()` really does
    // return a `java.lang.ProcessImpl`, dispatch walks up to
    // `java/lang/Process` and lands HERE with a receiver whose layout is the
    // JDK's, not `PROC_FIELD_COUNT` slots of subprocess bookkeeping.
    // `PROC_FIELD_PID` is slot 10; on a `ProcessImpl` that slot holds one of
    // the stream references, so the read below did not even yield a `Long` —
    // it fell to `-1`, `collect_descendant_pids(-1)` returned `Ok(vec![])`,
    // and `Process.descendants()` answered an EMPTY STREAM. That is the
    // fabricated success this file's `ProcessScanError` machinery was built to
    // remove, arriving by the one route the machinery cannot see: a correct
    // scan of the wrong process.
    //
    // The foreign answer is the JDK's own concrete body, verbatim —
    // `return toHandle().descendants();` — which reaches the real
    // `ProcessHandleImpl.descendants()` and therefore `getProcessPids0`, the
    // same probe this native would have used, for the right pid.
    if !is_vm_process(ctx, this) {
        let handle = ctx.invoke_virtual(this, "toHandle", "()Ljava/lang/ProcessHandle;", &[])?;
        let Some(Value::Object(Some(handle))) = handle else {
            // `Process.toHandle()`'s own default body throws this, and
            // `descendants()` is specified to propagate it: "the process is not
            // supported by this operating system" is a refusal, and an empty
            // stream is not how it is spelled.
            return Err(RuntimeError::UnsupportedOperationException {
                message: "Process.descendants()".to_string(),
            }
            .into());
        };
        return ctx.invoke_virtual(handle, "descendants", "()Ljava/util/stream/Stream;", &[]);
    }
    let this = ctx.read_native_pin(this_pin, this);
    let pid = match ctx.get_field(this, PROC_FIELD_PID) {
        Value::Long(p) => p,
        _ => -1,
    };
    // A scan that FAILED must not arrive here as an empty list: `descendants()`
    // would then hand back an empty stream, which is what a genuinely childless
    // process returns, and the caller (keycloak-test-framework's
    // `ProcessUtils.getKeycloakPid()` is the known one) would conclude the
    // wrapper script had exec'd nothing. See [`ProcessScanError`].
    let descendant_pids = match collect_descendant_pids(pid) {
        Ok(pids) => pids,
        Err(err) => return Err(process_scan_exception(ctx, err)),
    };

    let al_class = "java/util/ArrayList";
    let al_cid = ctx.ensure_class_initialized(al_class).map_err(|_| {
        MethodCallFailed::InternalError(VmError::Internal {
            message: format!("{al_class}: not loaded"),
        })
    })?;
    let list = ctx.alloc_object(al_cid, ctx.class_num_total_fields(al_cid).max(4));
    // GC: nothing in Java refers to `list` yet, and `<init>`, one
    // `build_process_handle` per descendant and one `add` per descendant all
    // run Java between here and the `stream()` call that consumes it.
    let list_pin = ctx.pin_native_root(list);
    let mut list = list;
    ctx.invoke(al_class, "<init>", "()V", &[Value::Object(Some(list))])?;
    for cpid in descendant_pids {
        let handle = build_process_handle(ctx, cpid)?;
        list = ctx.read_native_pin(list_pin, list);
        ctx.invoke(
            al_class,
            "add",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(list)), Value::Object(Some(handle))],
        )?;
    }
    list = ctx.read_native_pin(list_pin, list);
    ctx.unpin_native_roots(list_pin);
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

/// Fallible since 2026-08-10 (JDK-only wave 2, step 3): the pipe streams are
/// `cratonvm/synthetic/*` stand-ins for `java.lang.ProcessImpl`'s own pipe
/// streams, so a strict run must refuse them rather than substitute.
fn alloc_pipe_stream(
    ctx: &mut dyn NativeContext,
    class_name: &str,
    fd_id: i32,
) -> Result<Value, MethodCallFailed> {
    let class_id = crate::refused_class(ctx, class_name, 1)?;
    let stream = ctx.alloc_object(class_id, 1);
    ctx.set_field(stream, 0, Value::Int(fd_id));
    Ok(Value::Object(Some(stream)))
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
    ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::InNative);
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
    ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::InNative);
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
    // Interpreter round i5 wave 101, lane L1: this VM's process tables.
    let t = &tables(ctx);
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Long(-1))),
    };
    // GC: rooted across the call below; `safe_native_call` releases
    // the pin stack to its entry floor on return.
    let this_pin = ctx.pin_native_root(this);
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
    let this = ctx.read_native_pin(this_pin, this);
    let handle = handle_of(ctx, this);
    if handle == 0 {
        // Stub Process — fall back to whatever's in field 4.
        return Ok(Some(ctx.get_field(this, PROC_FIELD_PID)));
    }
    Ok(Some(Value::Long(pid_for_handle(t, handle))))
}

/// The `fd` / `handle` pair of a process-pipe descriptor: the platform's
/// spelling for a live pipe (`CRATONVM_FD_FIELDS_PER_PLATFORM`, JIT r23 w1 L6:
/// HotSpot's Unix `ProcessImpl` sets `fd`, its Windows one `handle`, the other
/// field stays -1), both -1 for an absent pipe.
fn store_process_descriptor(ctx: &mut dyn NativeContext, fd_obj: ObjectRef, fd_id: i32) {
    if fd_id >= 0 {
        crate::nio_native::store_descriptor_id(ctx, fd_obj, fd_id as FdId);
    } else {
        ctx.set_field_by_name(fd_obj, "fd", Value::Int(fd_id));
        ctx.set_field_by_name(fd_obj, "handle", Value::Long(fd_id as i64));
    }
}

/// Wrap an `FdTable` id in a real JDK stream object (`stream_class` must
/// declare a public `(Ljava/io/FileDescriptor;)V` constructor, i.e.
/// `java/io/FileInputStream` or `java/io/FileOutputStream`).
///
/// The descriptor carries the id in the platform's field (`fd` on Unix,
/// `handle` on Windows; both with `CRATONVM_FD_FIELDS_PER_PLATFORM=0`), the
/// same contract as `fis_set_fd` in `lib.rs`, so every existing
/// read/write/available/close native resolves it.
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
    store_process_descriptor(ctx, fd_obj, fd_id);
    // GC: the stream's own constructor runs Java, and BOTH the descriptor and
    // the stream are read again below — the descriptor is deliberately
    // re-seeded after construction, which is exactly the window.
    let fd_pin = ctx.pin_native_root(fd_obj);
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
    let fd_obj = ctx.read_native_pin(fd_pin, fd_obj);
    ctx.unpin_native_roots(fd_pin);
    store_process_descriptor(ctx, fd_obj, fd_id);

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
    // Interpreter round i5 wave 101, lane L1: this VM's process tables.
    let t = &tables(ctx);
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some("Process stream getter: null this".to_string()),
            }
            .into())
        }
    };
    // JIT r23 wave 4, lane L6: one stream object per pipe (see
    // `PROC_FIELD_STDIN_STREAM`), on a receiver with the synthetic layout.
    let stream_slot = match fd_field {
        PROC_FIELD_STDIN_FD => Some(PROC_FIELD_STDIN_STREAM),
        PROC_FIELD_STDOUT_FD => Some(PROC_FIELD_STDOUT_STREAM),
        PROC_FIELD_STDERR_FD => Some(PROC_FIELD_STDERR_STREAM),
        _ => None,
    }
    .filter(|_| ctx.object_num_fields(this) >= PROC_FIELD_COUNT);
    if let Some(slot) = stream_slot {
        if let Value::Object(Some(cached)) = ctx.get_field(this, slot) {
            return Ok(Some(Value::Object(Some(cached))));
        }
    }
    let mut fd_id = match ctx.get_field(this, fd_field) {
        Value::Int(v) => v,
        _ => -1,
    };
    if fd_id < 0 {
        let handle = handle_of(ctx, this);
        if handle != 0 {
            if let Some(pipes) = t.pipes.lock().get(&handle).copied() {
                // Only an id that still names the pipe spawned under it
                // (`PipeFds::live_fd`): an OS-keyed pipe's value is reused
                // once it is closed.
                fd_id = match fd_field {
                    PROC_FIELD_STDIN_FD => pipes.live_fd(&*ctx, pipes.stdin_fd, 0),
                    PROC_FIELD_STDOUT_FD => pipes.live_fd(&*ctx, pipes.stdout_fd, 1),
                    PROC_FIELD_STDERR_FD => pipes.live_fd(&*ctx, pipes.stderr_fd, 2),
                    _ => -1,
                };
            }
        }
    }
    // `alloc_pipe_stream` allocates: `this` is pinned across it to cache the
    // stream on the (possibly moved) Process.
    let pin = ctx.pin_native_root(this);
    let stream = alloc_pipe_stream(ctx, stream_class, fd_id);
    let this = ctx.read_native_pin(pin, this);
    ctx.unpin_native_roots(pin);
    let stream = stream?;
    if let Some(slot) = stream_slot {
        // JIT r23 w4 review: another thread's first getter call may have
        // cached its stream while this one allocated (the allocation is a GC
        // point and other mutators run meanwhile). Hand back THAT stream and
        // drop ours unpublished, so there is still one stream per pipe -- two
        // would hold the same raw id, and closing one would leave the other
        // on a value the OS may reuse.
        if let Value::Object(Some(cached)) = ctx.get_field(this, slot) {
            return Ok(Some(Value::Object(Some(cached))));
        }
        ctx.set_field(this, slot, stream);
    }
    Ok(Some(stream))
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
///
/// `Result` for the signature's sake only — this arm never fails. That is the
/// oracle's own shape: `ProcessHandleImpl_unix.c` throws `RuntimeException` for
/// a failed `opendir("/proc")` (see [`os_list_processes`]) but its `parent0`
/// returns `-1` for a process it cannot read, because a single unreadable
/// `/proc/<pid>/status` means that process is gone or not ours, not that the
/// procfs scan failed. The uniform return type is what lets `parent0` stay one
/// un-`cfg`'d body across all three platform arms.
#[cfg(target_os = "linux")]
fn os_parent_pid(pid: i64) -> Result<i64, ProcessScanError> {
    if pid <= 0 {
        return Ok(-1);
    }
    let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status")) else {
        return Ok(-1);
    };
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("PPid:") {
            if let Ok(v) = rest.trim().parse::<i64>() {
                return Ok(if v > 0 { v } else { -1 });
            }
        }
    }
    Ok(-1)
}

/// One `(pid, ppid)` row per visible process — Win32 Toolhelp snapshot.
///
/// The single source both [`os_parent_pid`] and [`os_list_processes`] read on
/// this platform, so the two can never disagree about the shape of the process
/// tree. `CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS)` is the same call
/// HotSpot's `ProcessHandleImpl_md.c` makes on Windows.
///
/// Both of those functions used to be `-1` / empty off Linux, and that is a
/// fabricated answer in the shape this wave is hunting: "this process has no
/// parent" and "the machine is running no processes" are indistinguishable from
/// true empty answers, so `ProcessHandle.parent()` was permanently
/// `Optional.empty()` and `children()` / `descendants()` / `allProcesses()`
/// permanently empty streams. `RJdkProcess.java:185` (`a live forked child must
/// report a parent`) is what measures the first, `:188`/`:189` the second.
///
/// The System Idle Process (pid 0) is dropped: it is not a process a handle can
/// name, and Linux's `/proc` has no `0` entry either, so both arms report the
/// same universe.
///
/// **Fallible, since 2026-08-11 (W6-10 finding 4).** An `Err` means the
/// enumeration did not happen; `Ok(vec![])` would mean the machine reported no
/// processes. Those were the same value until now, which is why a refused
/// snapshot read back as an empty machine — see [`ProcessScanError`]. The two
/// failure points are the two HotSpot's `ProcessHandleImpl_md.c` throws
/// `RuntimeException` at, in the same order.
#[cfg(windows)]
fn os_snapshot_processes() -> Result<Vec<(i64, i64)>, ProcessScanError> {
    use std::ffi::c_void;
    type Handle = *mut c_void;
    type Bool = i32;
    const TH32CS_SNAPPROCESS: u32 = 0x0000_0002;

    /// `PROCESSENTRY32W`. `th32DefaultHeapID` is a `ULONG_PTR`, which is what
    /// makes the struct 8-aligned on x86_64 and inserts the 4 padding bytes
    /// after `th32ProcessID` that MSVC also emits — `#[repr(C)]` reproduces that
    /// layout exactly, and `dwSize` must equal it or `Process32FirstW` fails.
    #[repr(C)]
    // Every field must be declared for the layout to be right; only three are
    // read.
    #[allow(dead_code)]
    struct ProcessEntry32W {
        dw_size: u32,
        cnt_usage: u32,
        th32_process_id: u32,
        th32_default_heap_id: usize,
        th32_module_id: u32,
        cnt_threads: u32,
        th32_parent_process_id: u32,
        pc_pri_class_base: i32,
        dw_flags: u32,
        sz_exe_file: [u16; 260],
    }

    #[link(name = "Kernel32")]
    extern "system" {
        fn CreateToolhelp32Snapshot(dw_flags: u32, th32_process_id: u32) -> Handle;
        fn Process32FirstW(h_snapshot: Handle, lppe: *mut ProcessEntry32W) -> Bool;
        fn Process32NextW(h_snapshot: Handle, lppe: *mut ProcessEntry32W) -> Bool;
        fn CloseHandle(h_object: Handle) -> Bool;
    }

    let mut out = Vec::new();
    // SAFETY: `entry` is a live, fully-zeroed local of exactly the layout the
    // API documents, with `dwSize` set before every call as required; the
    // enumeration functions write only into it. The snapshot handle is closed on
    // every path out, and both the null and `INVALID_HANDLE_VALUE` failure
    // returns are refused before use.
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap.is_null() || snap as isize == -1 {
            // Read the OS error BEFORE anything else can overwrite it — there
            // is no handle to close on this path, but the ordering rule is the
            // same one the `Process32FirstW` arm below depends on.
            return Err(ProcessScanError::new(format!(
                "CreateToolhelp32Snapshot failed: {}",
                std::io::Error::last_os_error()
            )));
        }
        let mut entry: ProcessEntry32W = std::mem::zeroed();
        entry.dw_size = std::mem::size_of::<ProcessEntry32W>() as u32;
        let first = Process32FirstW(snap, &mut entry) != 0;
        if !first {
            // `CloseHandle` succeeds here and would reset the thread's last
            // error, so the failure is captured first. HotSpot throws on this
            // condition unconditionally — including for `ERROR_NO_MORE_FILES`,
            // which would mean a snapshot with no rows at all — and diverging
            // to "empty machine" is exactly the fabrication being removed. A
            // Toolhelp snapshot always contains at least the System process, so
            // there is no honest empty case to protect.
            let err = ProcessScanError::new(format!(
                "Process32First failed: {}",
                std::io::Error::last_os_error()
            ));
            CloseHandle(snap);
            return Err(err);
        }
        let mut more = true;
        while more {
            let pid = entry.th32_process_id as i64;
            if pid > 0 {
                out.push((pid, entry.th32_parent_process_id as i64));
            }
            entry.dw_size = std::mem::size_of::<ProcessEntry32W>() as u32;
            more = Process32NextW(snap, &mut entry) != 0;
        }
        CloseHandle(snap);
    }
    Ok(out)
}

/// Parent pid of `pid`, or `-1` when the snapshot ran and simply does not
/// contain `pid` — Win32.
///
/// The distinction the `Result` carries is the whole of finding 4 on this path:
/// `-1` now means "enumerated, no such row", and an `Err` means "not
/// enumerated". They were the same `-1` before, so a refused snapshot made
/// `ProcessHandle.parent()` answer `Optional.empty()` — the documented answer
/// for the process with no parent — for every process on the machine.
#[cfg(windows)]
fn os_parent_pid(pid: i64) -> Result<i64, ProcessScanError> {
    if pid <= 0 {
        return Ok(-1);
    }
    Ok(os_snapshot_processes()?
        .into_iter()
        .find(|(p, _)| *p == pid)
        .map(|(_, ppid)| if ppid > 0 { ppid } else { -1 })
        .unwrap_or(-1))
}

/// No parent-pid probe is wired up for this target.
///
/// `Err` rather than `-1` for the same reason [`direct_child_pids`]' fallback
/// arm reports one: `-1` is the JDK's answer for a process that HAS no parent
/// (pid 1, or a reparented orphan), and claiming that for every process on a
/// platform this VM simply cannot inspect is the fabrication, not the report.
#[cfg(not(any(target_os = "linux", windows)))]
fn os_parent_pid(_pid: i64) -> Result<i64, ProcessScanError> {
    Err(ProcessScanError::new(
        "ProcessHandleImpl.parent0: no parent-pid probe on this platform".to_string(),
    ))
}

/// `(pid, ppid)` for every visible process when `of_pid == 0`, or for the
/// direct children of `of_pid` otherwise — the two modes the JDK's
/// `getProcessPids0` contract defines.
///
/// The one failure this arm reports is the one the oracle reports: a `/proc`
/// that will not open at all. `ProcessHandleImpl_unix.c` throws
/// `RuntimeException` there, and it is the same fact the Windows arm's refused
/// snapshot is — the enumeration did not run. A per-entry read that fails
/// (`os_parent_pid` below, on a process that exited between `read_dir` and the
/// read) is NOT that: the scan ran, and one row of it is stale.
#[cfg(target_os = "linux")]
fn os_list_processes(of_pid: i64) -> Result<Vec<(i64, i64)>, ProcessScanError> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir("/proc") {
        Ok(entries) => entries,
        Err(e) => return Err(ProcessScanError::new(format!("Unable to open /proc: {e}"))),
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Ok(pid) = name.parse::<i64>() else {
            continue;
        };
        let ppid = os_parent_pid(pid)?;
        if of_pid == 0 || ppid == of_pid {
            out.push((pid, ppid.max(0)));
        }
    }
    Ok(out)
}

#[cfg(windows)]
fn os_list_processes(of_pid: i64) -> Result<Vec<(i64, i64)>, ProcessScanError> {
    Ok(os_snapshot_processes()?
        .into_iter()
        .filter(|(_, ppid)| of_pid == 0 || *ppid == of_pid)
        .map(|(pid, ppid)| (pid, ppid.max(0)))
        .collect())
}

/// No process-enumeration probe is wired up for this target.
///
/// The `Vec::new()` this replaces made `ProcessHandle.allProcesses()` an empty
/// stream and `children()` an empty stream, which are the answers a machine
/// running nothing gives — the same fabrication wave 3 removed from the Windows
/// arm by implementing it. Until a `sysctl(KERN_PROC_ALL)` arm exists, the
/// honest report is that the enumeration did not happen.
#[cfg(not(any(target_os = "linux", windows)))]
fn os_list_processes(_of_pid: i64) -> Result<Vec<(i64, i64)>, ProcessScanError> {
    Err(ProcessScanError::new(
        "ProcessHandleImpl.getProcessPids0: no process-enumeration probe on this platform"
            .to_string(),
    ))
}

/// What `ProcessHandleImpl$Info.info0` writes into `command`, `arguments` and
/// `commandLine` from the OS's command-line record (interpreter round i5
/// wave 101, lane L1). Each `None` leaves the field's constructor `null`.
#[derive(Debug, Default, PartialEq, Eq)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
struct OsCmdline {
    command: Option<String>,
    arguments: Option<Vec<String>>,
    command_line: Option<String>,
}

/// The Linux `info0` reading, as `ProcessHandleImpl_linux.c`
/// `os_getCmdlineAndUserInfo` and `ProcessHandleImpl_unix.c`
/// `unix_fillArgArray` make it (interpreter round i5 wave 101, lane L1):
///
/// * `command` is the link `/proc/<pid>/exe` -- the resolved executable
///   (`/usr/bin/dash` for a child started as `sh`), `null` for a process this
///   VM may not read -- NOT `argv[0]`, which this function answered before
///   wave 101;
/// * `arguments` is `argv[1..]`, unset when an argument is empty (the C loop
///   returns at an empty string, before it stores the array or the command
///   line) -- empty arguments were dropped and the rest shifted;
/// * `commandLine` is the whole record, NUL-separated words joined by single
///   spaces, unset when the record filled the page HotSpot reads (truncated).
///
/// No `/proc/<pid>/cmdline` at all: nothing (HotSpot returns before it reads
/// the link).
#[cfg(target_os = "linux")]
fn os_process_cmdline(pid: i64) -> Option<OsCmdline> {
    if pid <= 0 {
        return None;
    }
    let mut raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let command = std::fs::read_link(format!("/proc/{pid}/exe"))
        .ok()
        .map(|p| p.to_string_lossy().into_owned());
    // HotSpot reads at most one page.
    // SAFETY: `sysconf` has no preconditions.
    let page = usize::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) })
        .ok()
        .filter(|&p| p > 0)
        .unwrap_or(4096);
    let truncated = raw.len() >= page && raw.get(page - 1) != Some(&0);
    raw.truncate(page);
    let words: Vec<&[u8]> = {
        let body = raw.strip_suffix(&[0u8]).unwrap_or(&raw[..]);
        if body.is_empty() {
            Vec::new()
        } else {
            body.split(|b| *b == 0).collect()
        }
    };
    let mut out = OsCmdline {
        command,
        ..OsCmdline::default()
    };
    if words.is_empty() {
        return Some(out);
    }
    let decode = |w: &[u8]| String::from_utf8_lossy(w).into_owned();
    if words[1..].iter().any(|w| w.is_empty()) {
        // `unix_fillArgArray` stops at the empty argument: the command only.
        return Some(out);
    }
    out.arguments = Some(words[1..].iter().map(|&w| decode(w)).collect());
    if !truncated {
        out.command_line = Some(
            words
                .iter()
                .map(|&w| decode(w))
                .collect::<Vec<_>>()
                .join(" "),
        );
    }
    Some(out)
}

#[cfg(not(target_os = "linux"))]
fn os_process_cmdline(_pid: i64) -> Option<OsCmdline> {
    None
}

/// `java.lang.ProcessHandleImpl.parent0(long pid, long startTime) -> long`
///
/// `-1` is the JDK's "no parent" answer and yields `Optional.empty()` from
/// `ProcessHandle.parent()`. It is reported only for a scan that RAN and did not
/// find `pid`; a scan that could not run throws, because otherwise every process
/// on the machine looks parentless and nothing says why (W6-10 finding 4).
fn native_proc_handle_parent0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
    match os_parent_pid(pid) {
        Ok(ppid) => Ok(Some(Value::Long(ppid))),
        Err(err) => Err(process_scan_exception(ctx, err)),
    }
}

/// `java.lang.ProcessHandleImpl.getProcessPids0(long, long[], long[], long[]) -> int`
///
/// Returns the number of processes found. The JDK caller grows its arrays and
/// retries whenever the count exceeds their length, so filling only as far as
/// each array reaches (and still reporting the true total) is the contract.
///
/// A count of `0` is therefore a load-bearing claim — `allProcesses()` and
/// `children()` become empty streams on it — and it is only ever made for an
/// enumeration that RAN. One that failed throws `java.lang.RuntimeException`,
/// which is what HotSpot's `ProcessHandleImpl_md.c` / `_unix.c` do at the same
/// two points; `allProcesses()` being *restricted* is documented and legal, a
/// list truncated by an error is not (W6-10 finding 4).
fn native_proc_handle_get_process_pids0(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let of_pid = match args.first() {
        Some(Value::Long(p)) => *p,
        _ => return Ok(Some(Value::Int(0))),
    };
    let found = match os_list_processes(of_pid) {
        Ok(found) => found,
        Err(err) => return Err(process_scan_exception(ctx, err)),
    };
    let arr_of = |idx: usize| -> Option<ObjectRef> {
        match args.get(idx) {
            Some(Value::Object(Some(a))) => Some(*a),
            _ => None,
        }
    };
    let pids = arr_of(1);
    let ppids = arr_of(2);
    let starts = arr_of(3);
    // Array lengths are loop-INVARIANT — `set_array_element` cannot allocate,
    // cannot resize, and cannot move any of these refs — so they are probed
    // once rather than three times per enumerated process. On a machine with
    // several hundred processes `ProcessHandle.allProcesses()` was making
    // ~3N virtual `array_length` calls per snapshot and ~6N across the JDK
    // caller's mandatory retry (`ProcessHandleImpl` sizes its arrays at 100 and
    // re-invokes whenever the returned count exceeds that). Nothing observable
    // changes; this is the free half of W6-10's remaining cost.
    //
    // The EXPENSIVE half is deliberately still inside the loop and still
    // guarded: `start_time_or_any` is one `OpenProcess`/`GetProcessTimes`/
    // `CloseHandle` per row on Windows, and `PROCESSENTRY32` carries no
    // creation time, so there is no cheaper documented source. Paying it only
    // for indices the caller's array can actually hold is what keeps the first
    // (100-element) pass at 100 opens instead of N.
    let pids_len = pids.map_or(0, |a| ctx.array_length(a));
    let ppids_len = ppids.map_or(0, |a| ctx.array_length(a));
    let starts_len = starts.map_or(0, |a| ctx.array_length(a));
    for (i, (pid, ppid)) in found.iter().enumerate() {
        if let Some(a) = pids {
            if i < pids_len {
                ctx.set_array_element(a, i, Value::Long(*pid));
            }
        }
        if let Some(a) = ppids {
            if i < ppids_len {
                ctx.set_array_element(a, i, Value::Long(*ppid));
            }
        }
        if let Some(a) = starts {
            if i < starts_len {
                // `children()` and `allProcesses()` build their handles straight
                // out of this array — `new ProcessHandleImpl(cpids[i],
                // stimes[i])` — so a 0 here is what left every handle in those
                // streams unable to tell itself apart from a recycled pid.
                ctx.set_array_element(a, i, Value::Long(start_time_or_any(*pid)));
            }
        }
        // Every remaining row is past the end of all three arrays; the caller
        // only wants the COUNT from here on, and `found.len()` already has it.
        if i >= pids_len && i >= ppids_len && i >= starts_len {
            break;
        }
    }
    Ok(Some(Value::Int(found.len() as i32)))
}

/// `java.lang.ProcessHandleImpl$Info.info0(long pid)V` — an INSTANCE method,
/// so `args[0]` is the `Info` receiver whose fields are filled in and
/// `args[1]` is the pid.
///
/// **`startTime` is not optional here, on any platform.** The only caller is
/// `Info.info(long pid, long startTime)`, whose bytecode (JDK 25, verified with
/// `javap -c java.lang.ProcessHandleImpl$Info`) is:
///
/// ```text
/// Info info = new Info();      // command=null, startTime=-1, totalTime=-1
/// info.info0(pid);
/// if (startTime != info.startTime) {
///     info.command = null; info.arguments = null;
///     info.startTime = -1; info.totalTime = -1; info.user = null;
/// }
/// return info;
/// ```
///
/// That comparison is a bare `!=` with no wildcard — unlike `equals()` and
/// `isAlive()`, which both treat `STARTTIME_ANY` (0) as "matches anything". So
/// an `info0` that filled `command` but left `startTime` at its `-1` default
/// had every field it just wrote thrown away by the caller, on Linux as well as
/// on Windows, and `ProcessHandle.current().info()` reported an entirely empty
/// record while nothing threw. The two `RJdkProcess.java` checks that sit behind
/// `info.command().isPresent()` (:135) and `info.startInstant().isPresent()`
/// (:138) then never ran, and the vector still printed `PASS` — two checks
/// lower than HotSpot's 53.
///
/// `start_time_or_any` is the single source for all three of `isAlive0`, this
/// native and `current_process_start_time`, which is what makes the comparison
/// agree by construction rather than by luck. When the OS will not tell us
/// (no permission, no probe on this platform) all three degrade to the same
/// `STARTTIME_ANY` 0, the comparison still holds, and `startInstant()` reports
/// `Optional.empty()` because its own guard is `startTime > 0`.
///
/// What is populated is what the platform actually knows, and matches the real
/// JDK's own per-platform split: Linux reports command + commandLine +
/// arguments from `/proc/<pid>/cmdline`; Windows reports the image path only,
/// which is exactly what HotSpot 25 answers there (`commandLine()` and
/// `arguments()` measured EMPTY on real HotSpot for this host).
///
/// **Every field this native declines to write is an `Optional.empty()` the
/// JDK's own accessor derives from the constructor default**, and that is the
/// specified answer, not a shortfall. `ProcessHandle.Info`'s javadoc: *"The
/// attributes of a process vary by operating system and are not available in all
/// implementations. Information about processes is limited by the operating
/// system privileges of the process making the request. The return types are
/// `Optional<T>` allowing explicit tests and actions if the value is
/// available."* The three sentinels, all confirmed by `javap -c`: `command` /
/// `commandLine` / `arguments` / `user` are `null` -> `Optional.ofNullable`;
/// `startTime` guards on `> 0`; `totalTime` guards on `!= -1`. Writing a
/// plausible-looking value into any of them instead — a synthesised command line
/// with no arguments, this VM's own user, a zero CPU total — produces a PRESENT
/// `Optional` carrying something the OS never said, which no caller can tell
/// from a real reading.
///
/// `user` is now sourced on both Linux and Windows (`os_process_user`), and
/// `totalTime` on both as well: Windows from `GetProcessTimes`, Linux from
/// `/proc/<pid>/stat` fields 14/15 over `sysconf(_SC_CLK_TCK)`. Both had been
/// recorded as residuals of the lane that fixed `startTime`
/// (docs/known-issues/jdk-only/W5-2-two-silently-skipped-process-checks.md).
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

    // Every OS read happens before the first allocation, so no `ctx` call sits
    // between a probe and the write of what it produced.
    // ONE probe for both: on Windows these are the two halves of a single
    // `GetProcessTimes`, and asking twice cost a second `OpenProcess` for a
    // struct we had already read. `start_time_and_cpu`'s first element is
    // `start_time_or_any(pid)` by construction, so the number written to
    // `startTime` below is still the number `isAlive0` returns and the number
    // `current_process_start_time` stamps on `ProcessHandle.current()` — which
    // is what stops `Info.info(pid, startTime)`'s bare `!=` from wiping the
    // record.
    let (start_time, cpu_nanos) = start_time_and_cpu(pid);
    let cmdline = os_process_cmdline(pid);
    // Only consulted when there is no `/proc`-style command line to prefer.
    let image_name = if cmdline.is_none() {
        os_process_image_name(pid)
    } else {
        None
    };
    let user = os_process_user(pid);

    // Pin `this` across every allocation below — each `create_string` /
    // `new_array` can trigger a moving young GC that would relocate it.
    let this_pin = ctx.pin_native_root(this);
    if let Some(OsCmdline {
        command,
        arguments,
        command_line,
    }) = cmdline
    {
        // Interpreter round i5 wave 101, lane L1: each field as the OS record
        // gave it ([`OsCmdline`]); `Info.commandLine()` itself joins `command`
        // and `arguments` when both are present, and reads this field only
        // otherwise.
        if let Some(command) = command {
            let cmd_str = ctx.create_string(&command);
            let this_cur = ctx.read_native_pin(this_pin, this);
            ctx.set_field_by_name(this_cur, "command", Value::Object(Some(cmd_str)));
        }
        if let Some(command_line) = command_line {
            let line_str = ctx.create_string(&command_line);
            let this_cur = ctx.read_native_pin(this_pin, this);
            ctx.set_field_by_name(this_cur, "commandLine", Value::Object(Some(line_str)));
        }
        // An unset `arguments` keeps its `null` (`Optional.empty()`).
        if let Some(arguments) = arguments {
            // `String[]`, not `Object[]`. `ProcessHandle.Info.arguments()` is
            // declared `Optional<String[]>`, and `new_array(Reference, n)` produces
            // an array whose runtime component type is `java.lang.Object`. On Linux
            // this is the ONLY reason `RJdkOptionalShape` fails:
            //
            //   AssertionError: process.info.arguments: get() on a PRESENT Optional
            //   must return a [Ljava.lang.String;, got [Ljava.lang.Object;
            //
            // It is invisible on Windows because `os_process_cmdline` finds no
            // command line for most pids there, so the field stays null and the
            // vector's law skips the row -- a platform-shaped blind spot, not a
            // platform-shaped defect.
            //
            // MERGE NOTE 2026-08-22. WORKER 3 and WORKER 4 found and fixed this
            // independently, in the same hour, from the same red vector; the
            // comment above is WORKER 3's and the call below is WORKER 4's. The
            // difference is scope: `crate::new_string_array` also carries the
            // `File.list()` site, which already had the correct four-line idiom,
            // and `watch.rs::pollEventNames0`, whose registered descriptor is
            // `(I)[Ljava/lang/String;` and which had the wrong one. Three sites,
            // one implementation -- which is the whole reason to prefer a helper
            // over the inline `match` this replaces.
            let arr = crate::new_string_array(ctx, arguments.len());
            let arr_pin = ctx.pin_native_root(arr);
            for (i, a) in arguments.iter().enumerate() {
                let s = ctx.create_string(a);
                let arr_cur = ctx.read_native_pin(arr_pin, arr);
                ctx.set_array_element(arr_cur, i, Value::Object(Some(s)));
            }
            let this_cur = ctx.read_native_pin(this_pin, this);
            let arr_cur = ctx.read_native_pin(arr_pin, arr);
            // `arr_pin` is deliberately not released here: the final
            // `unpin_native_roots(this_pin)` below frees every root from `this_pin`
            // onward, which includes it.
            ctx.set_field_by_name(this_cur, "arguments", Value::Object(Some(arr_cur)));
        }
    } else if let Some(command) = image_name {
        let cmd_str = ctx.create_string(&command);
        let this_cur = ctx.read_native_pin(this_pin, this);
        ctx.set_field_by_name(this_cur, "command", Value::Object(Some(cmd_str)));
    }

    // `user` is only written when the OS named an account. Left unwritten it
    // keeps the constructor's `null`, and `user()` is `Optional.ofNullable(user)`
    // — so absence is reported as absence. Writing a placeholder (this VM's own
    // user, the numeric uid, the raw SID) would be indistinguishable to the
    // caller from a real answer, and the javadoc explicitly provides for the
    // value not being there: *"Information about processes is limited by the
    // operating system privileges of the process making the request."*
    if let Some(user) = user {
        let user_str = ctx.create_string(&user);
        let this_cur = ctx.read_native_pin(this_pin, this);
        ctx.set_field_by_name(this_cur, "user", Value::Object(Some(user_str)));
    }

    // Scalars last: neither write can allocate, so `this` cannot move between
    // the two of them.
    let this_cur = ctx.read_native_pin(this_pin, this);
    ctx.set_field_by_name(this_cur, "startTime", Value::Long(start_time));
    if let Some(nanos) = cpu_nanos {
        ctx.set_field_by_name(this_cur, "totalTime", Value::Long(nanos));
    }
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
// AMENDED 2026-08-07 (W3-6). `ProcessImpl.create` was in that last group for a
// reason the census could not see: it was registered under a descriptor the
// image does not declare, so of course nothing resolved to it. On the JDK 25
// Windows image it IS `ACC_NATIVE`, and so are the nine siblings that had no
// registration at all. All ten are now stated `Bridge` under `cfg(windows)`,
// which moves the Windows arm from 9 bridges to 19 and leaves the Linux arm
// unchanged. See docs/known-issues/jdk-only/W3-6-processimpl-missing-natives.md
// for the per-row `javap` census.
//
// Details and the per-row table:
// l5-native-io-bridge-residuals-RETIRED-20260810.md
// WHERE `java/lang/Runtime.exec` IS *NOT*. Recorded 2026-08-20 (H3-1) because
// `G89-1` N2 points a reader at this file for it and the tree says otherwise.
//
// This registrar owns **26 `SyntheticStub` rows** — 13 on
// `SYNTHETIC_PROCESS_CLASS`, 1 `ProcessExitWaiter.run`, 5 on the synthetic pipe
// OUTPUT stream, 6 on the synthetic pipe INPUT stream, and
// `java/lang/ProcessBuilder.start()Ljava/lang/Process;` — and **none of them is
// a `Runtime.exec` overload**. All six `exec` overloads are registered in
// `native-builtins/src/lib.rs` (search `"java/lang/Runtime"` + `"exec"`), which
// contract §8 places off limits for wave 1. `G89-1` §3b names that file
// correctly; only N2's prose points here.
//
// The analysis N2 asks for is nevertheless settled, twice over:
//
//   * `javap -p java.lang.Runtime` on the JDK 25 image (Microsoft build
//     25.0.3+9, re-verified 2026-08-20): the ONLY `ACC_NATIVE` members are
//     `availableProcessors`, `freeMemory`, `totalMemory`, `maxMemory` and
//     `gc`. **All six `exec` overloads are ordinary bytecode**, five of them
//     one-line delegations and the sixth
//     `exec(String[],String[],File)` = `new ProcessBuilder(cmdarray)
//     .environment(envp).directory(dir).start()`.
//   * The registration site in `lib.rs` already states exactly that, dated
//     2026-08-12 (W7-17 N1), with a measured two-arm probe, and it is why all
//     six are `register_with_kind(..., SyntheticStub)` today. They are
//     therefore ALREADY dropped under `--jdk-only`, and the JDK's own `exec`
//     already runs there — reaching `ProcessBuilder.start()` and, below it,
//     `ProcessImpl.create` (windows) / `ProcessImpl.forkAndExec` (linux), both
//     of which THIS registrar supplies as genuine `Bridge`s.
//
// So the honest verdict on all six is KEEP-AS-STUB-WITH-A-STATED-REASON, and
// the residual work is a `Compatible`-mode deletion inside a §8-reserved file,
// i.e. wave 2. Do not "fix" it by adding a seventh copy here.

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
    //
    // The full Windows `ProcessImpl` surface, from
    // `javap -p -s java.lang.ProcessImpl` on the JDK 25 image. All ten are
    // `ACC_NATIVE` there, so all ten are §1.5 bridges, and nine of them had no
    // registration at all — `getStillActive`, which `<clinit>` calls to seed
    // `STILL_ACTIVE`, is simply the first one the class reaches.
    //
    // `create` had one, under a descriptor the image does not declare
    // (`[Ljava/lang/String;` for what is a single envblock `String`), so it
    // never bound either; see `native_process_impl_create`.
    //
    // Gated on `windows` as a group. The Linux image's `ProcessImpl` is a
    // different class with a different native set — `forkAndExec` below, plus
    // `init`/`destroyProcess` — and none of these names exist on it, so
    // registering them there could only ever add unresolvable rows to the
    // `--jdk-only` census.
    #[cfg(windows)]
    {
        registry.register_with_kind(
            "java/lang/ProcessImpl",
            "create",
            "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;[JZ)J",
            native_process_impl_create,
            NativeKind::Bridge,
        );
        registry.register_with_kind(
            "java/lang/ProcessImpl",
            "getStillActive",
            "()I",
            native_process_impl_get_still_active,
            NativeKind::Bridge,
        );
        registry.register_with_kind(
            "java/lang/ProcessImpl",
            "getExitCodeProcess",
            "(J)I",
            native_process_impl_get_exit_code,
            NativeKind::Bridge,
        );
        registry.register_with_kind(
            "java/lang/ProcessImpl",
            "isProcessAlive",
            "(J)Z",
            native_process_impl_is_process_alive,
            NativeKind::Bridge,
        );
        registry.register_with_kind(
            "java/lang/ProcessImpl",
            "terminateProcess",
            "(J)V",
            native_process_impl_terminate,
            NativeKind::Bridge,
        );
        registry.register_with_kind(
            "java/lang/ProcessImpl",
            "waitForInterruptibly",
            "(J)V",
            native_process_impl_wait_for_interruptibly,
            NativeKind::Bridge,
        );
        registry.register_with_kind(
            "java/lang/ProcessImpl",
            "waitForTimeoutInterruptibly",
            "(JJ)V",
            native_process_impl_wait_for_timeout,
            NativeKind::Bridge,
        );
        registry.register_with_kind(
            "java/lang/ProcessImpl",
            "getProcessId0",
            "(J)I",
            native_process_impl_get_process_id0,
            NativeKind::Bridge,
        );
        registry.register_with_kind(
            "java/lang/ProcessImpl",
            "closeHandle",
            "(J)Z",
            native_process_impl_close_handle,
            NativeKind::Bridge,
        );
        registry.register_with_kind(
            "java/lang/ProcessImpl",
            "openForAtomicAppend",
            "(Ljava/lang/String;)J",
            native_process_impl_open_for_atomic_append,
            NativeKind::Bridge,
        );
    }

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
    // Real JDK 25 signature: destroy0(pid, startTime, forcibly) -> boolean.
    registry.register_with_kind(
        "java/lang/ProcessHandleImpl",
        "destroy0",
        "(JJZ)Z",
        |ctx, args| {
            // Interpreter round i5 wave 101, lane L1: this VM's process tables.
            let t = &tables(ctx);
            // args = (pid, startTime, force). Keyed by pid -- see
            // `handle_for_pid`; `startTime` is the JDK's staleness check
            // against a recycled pid, which we cannot answer (we record no
            // start time) and do not need to: the process table maps a pid to
            // OUR child, and a recycled pid is not in it.
            let pid = match args.first() {
                Some(Value::Long(p)) => *p,
                _ => return Ok(Some(Value::Int(0))),
            };
            let start_time = match args.get(1) {
                Some(Value::Long(t)) => *t,
                _ => PROCESS_STARTTIME_ANY,
            };
            let force = matches!(args.get(2), Some(Value::Int(1)));
            let ok = match handle_for_pid(t, pid) {
                // Our own child: go through the table, which also keeps the
                // exit-status bookkeeping straight. Its pid cannot be stale —
                // we hold the `Child`, so it has not been reaped.
                Some(handle) => destroy_handle(t, handle, force),
                // Someone else's. This used to answer a flat `false`: safe, but
                // wrong, and `ProcessHandle.destroy()` on a process this VM did
                // not spawn simply did nothing where HotSpot kills it. The
                // reason given was that the bridge had no way to detect a
                // recycled pid — which was true only because `isAlive0` reported
                // no start time. It does now, so run the JDK's own check:
                //
                //   jlong start = os_getStartTime(pid);
                //   if (start == startTime || startTime == 0) { kill(pid, sig); }
                //
                // A caller that never learned a start time (0) is trusted, which
                // is what HotSpot does and is the only way `ProcessHandle`s built
                // before this change keep working.
                None => start_time_matches(pid, start_time) && signal_pid(pid, force),
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
/// runtime-exec-returned-a-process-with-no-streams-FIXED-20260806.md.
pub fn native_process_builder_start(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
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
    // GC: rooted across the call below; `safe_native_call` releases
    // the pin stack to its entry floor on return.
    let this_pin = ctx.pin_native_root(this);

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
    let this = ctx.read_native_pin(this_pin, this);
    // GC: rooted across the call below; `safe_native_call` releases
    // the pin stack to its entry floor on return.
    let this_pin = ctx.pin_native_root(this);

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
    let this = ctx.read_native_pin(this_pin, this);
    // GC: rooted across the call below; `safe_native_call` releases
    // the pin stack to its entry floor on return.
    let this_pin = ctx.pin_native_root(this);
    let redirects = read_process_redirects(ctx, this);
    let program = cmd_strings[0].clone();
    let rest: Vec<String> = cmd_strings.into_iter().skip(1).collect();
    let this = ctx.read_native_pin(this_pin, this);
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
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    /// Serializes the tests that call `spawn_and_wrap`.
    ///
    /// `set_spawn_policy_hook` installs a PROCESS-GLOBAL hook, and
    /// `spawn_policy_hook_is_consulted_and_can_refuse_the_fork` spends part of
    /// its run with that hook set to refuse everything. Any other test
    /// spawning through `spawn_and_wrap` at that moment is refused too, which
    /// showed up as a flaky `SecurityException` from an unrelated test.
    static SPAWN_TEST_LOCK: Mutex<()> = Mutex::new(());

    /// The table set the mock context's natives use (no VM slot), which
    /// the direct calls below share with them.
    fn tt() -> &'static Arc<ProcessTables> {
        detached_tables_ref()
    }

    /// A stand-in `TimeUnit` for the mock context (interpreter round i5 wave
    /// 102): an object whose next `toMillis(J)J` answers `millis`. The timed
    /// `waitFor` stand-in now throws the JDK bodies' NPE for a null unit.
    fn mock_millis_unit(ctx: &mut MockNativeContext, millis: i64) -> ObjectRef {
        ctx.script("toMillis", "(J)J", Ok(Some(Value::Long(millis))));
        ctx.alloc_object(1)
    }

    /// Interpreter round i5 wave 102, lane L1: `waitFor(long, null)` throws
    /// the NPE every JDK body throws first (`unit.toNanos(timeout)`, "throw
    /// NPE before other conditions"), for a VM-spawned child that has exited
    /// and for a foreign receiver alike.
    #[test]
    fn w102_l1_a_null_unit_is_the_jdk_bodies_npe() {
        let mut ctx = MockNativeContext::new();
        let foreign = ctx.alloc_object(2);
        for timeout in [0i64, 5] {
            let thrown = native_process_wait_for_timeout(
                &mut ctx,
                &[
                    Value::Object(Some(foreign)),
                    Value::Long(timeout),
                    Value::Object(None),
                ],
            )
            .expect_err("a null unit throws");
            assert!(
                matches!(
                    thrown,
                    MethodCallFailed::InternalError(VmError::Runtime(
                        RuntimeError::NullPointerException { message: Some(ref m) }
                    )) if m == WAIT_FOR_NULL_UNIT_NPE
                ),
                "{thrown:?}"
            );
        }
    }

    /// Interpreter round i5 wave 101, lane L1: two VMs' registries hold two
    /// table sets. A child installed in one is not found by pid, handle,
    /// liveness or destroy through the other, and its exit is published in
    /// its own set only.
    #[test]
    fn w101_l1_two_registries_hold_apart_process_tables() {
        let reg_a = NativeMethodRegistry::new();
        let reg_b = NativeMethodRegistry::new();
        let a = tables_in(reg_a.process_tables_slot());
        let b = tables_in(reg_b.process_tables_slot());
        assert!(!Arc::ptr_eq(&a, &b), "one table set per registry");
        assert!(
            Arc::ptr_eq(&a, &tables_in(reg_a.process_tables_slot())),
            "the slot keeps its set"
        );
        #[cfg(windows)]
        let child = Command::new("cmd")
            .args(["/c", "exit", "4"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn cmd");
        #[cfg(not(windows))]
        let child = Command::new("/bin/sh")
            .args(["-c", "exit 4"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn /bin/sh");
        let pid = child.id() as i64;
        let handle = a.next_handle.fetch_add(1, Ordering::Relaxed);
        a.children.lock().insert(handle, Arc::new(Mutex::new(child)));
        a.exits.lock().insert(
            handle,
            ExitCache {
                pid,
                exit_code: None,
            },
        );
        assert_eq!(handle_for_pid(&a, pid), Some(handle));
        assert_eq!(handle_for_pid(&b, pid), None, "B never saw A's child");
        assert_eq!(pid_for_handle(&b, handle), -1);
        assert!(!destroy_handle(&b, handle, true), "B cannot destroy it");
        let seen_b = process_exit_generation(&b);
        assert_eq!(wait_for_handle(&a, handle), 4);
        assert_eq!(try_exit_handle(&a, handle), Some(4));
        assert_eq!(try_exit_handle(&b, handle), None);
        assert_eq!(process_exit_generation(&b), seen_b, "A's exit is A's");
        drop(reg_a);
        assert_eq!(Arc::strong_count(&a), 1, "the registry released its set");
    }

    /// Interpreter round i5 wave 101, lane L1: a spawn failure's OS half is
    /// the JDK spawn native's text (`ProcessBuilder.start` adds `Cannot run
    /// program ...`), and Rust's code-less "program not found" (Windows `PATH`
    /// resolution) reads as `ERROR_FILE_NOT_FOUND`.
    #[test]
    fn w101_l1_spawn_failure_detail_is_the_jdk_natives_text() {
        let enoent = spawn_failure_detail(&std::io::Error::from_raw_os_error(2));
        if cfg!(windows) {
            assert!(enoent.starts_with("CreateProcess error=2, "), "{enoent}");
            assert!(!enoent.ends_with('.'), "{enoent}");
        } else {
            assert_eq!(enoent, "Exec failed, error: 2 (No such file or directory) ");
        }
        assert!(!enoent.contains("os error"), "{enoent}");
        let unresolved = std::io::Error::new(std::io::ErrorKind::NotFound, "program not found");
        assert_eq!(spawn_failure_detail(&unresolved), enoent);
        assert_eq!(
            cannot_run_program("nosuch", Some("d"), "error=2, x"),
            "Cannot run program \"nosuch\" (in directory \"d\"): error=2, x"
        );
    }

    fn install_child_for_test(child: Child) -> (i64, i64) {
        let handle = tt().next_handle.fetch_add(1, Ordering::Relaxed);
        let pid = child.id() as i64;
        tt().children.lock().insert(handle, Arc::new(Mutex::new(child)));
        tt().exits.lock().insert(
            handle,
            ExitCache {
                pid,
                exit_code: None,
            },
        );
        (handle, pid)
    }

    /// The merged one-`OpenProcess` probe must answer the SAME start time the
    /// separate `os_process_start_time` does.
    ///
    /// This is not a tautology and it is the whole reason the merge is safe to
    /// make. `ProcessHandleImpl$Info.info(pid, startTime)` — real JDK bytecode
    /// — compares the handle's start time with the one `info0` wrote using a
    /// bare `!=` with no `STARTTIME_ANY` wildcard, and wipes `command`,
    /// `arguments`, `startTime`, `totalTime` and `user` off the record on a
    /// mismatch of one millisecond. `isAlive0` supplies the handle's number and
    /// `info0` supplies the other, so the moment `isAlive0` sources its start
    /// time from a *different* probe than `start_time_or_any`, W5-2's two
    /// silently-skipped `RJdkProcess` checks come straight back.
    ///
    /// The other two rows are the arms the old boolean `foreign_pid_is_alive`
    /// carried and this probe inherited: a pid outside the `DWORD` range is
    /// refused without a syscall (which is what keeps
    /// `ProcessHandle.of(Long.MAX_VALUE)` cheap and empty), and a pid no
    /// process holds is not alive.
    #[test]
    #[cfg(windows)]
    fn one_open_reports_the_same_start_time_as_the_separate_probe() {
        let me = std::process::id() as i64;
        let (alive, start) = win_liveness_and_start_time(me);
        assert!(alive, "this process is running");
        assert_eq!(
            start,
            os_process_start_time(me),
            "the merged probe and `start_time_or_any`'s source must be ONE \
             number; `Info.info(pid, startTime)` compares them with a bare `!=`"
        );
        assert_eq!(
            foreign_start_time_or_dead(me),
            start_time_or_any(me),
            "and so must what `isAlive0` hands back"
        );

        assert_eq!(
            win_liveness_and_start_time(i64::MAX),
            (false, None),
            "a pid outside the DWORD range cannot name a process, and is \
             refused without a syscall"
        );
        assert_eq!(
            foreign_start_time_or_dead(i64::MAX),
            -1,
            "-1 is the only value `ProcessHandleImpl.isAlive()` reads as dead"
        );
    }

    /// The Linux twin of the test above, asserting the same identity for the
    /// same reason — and it is the only scheduled witness this arm has.
    ///
    /// The Linux merge has no Java-visible witness: what it removes is a
    /// pid-recycle attribution window and a race between two `/proc` reads,
    /// neither of which a regression-suite vector can provoke on demand. What
    /// IS assertable, and is the thing that would break if the merge were wrong,
    /// is the identity `Info.info(pid, startTime)` depends on — `isAlive0`'s
    /// start time and `start_time_or_any`'s must be ONE number, compared there
    /// with a bare `!=` that wipes five fields on a mismatch of one
    /// millisecond. The moment `linux_liveness_and_start_time` parses field 22
    /// differently from `linux_proc_stat_times`, W5-2's two silently-skipped
    /// `RJdkProcess` checks come straight back; a shared
    /// `linux_stat_line_times` is what makes that unfalsifiable, and this is
    /// what checks it stayed shared.
    ///
    /// The third row is the arm the deleted boolean `foreign_pid_is_alive`
    /// carried: a pid no process holds is not alive, and `-1` is the only value
    /// `ProcessHandleImpl.isAlive()` reads that way. `i64::MAX` is far outside
    /// `pid_max`, so `/proc/<pid>/stat` is `ENOENT` for it on any host.
    #[test]
    #[cfg(target_os = "linux")]
    fn one_stat_read_reports_the_same_start_time_as_the_separate_probe() {
        let me = std::process::id() as i64;
        let (alive, start) = linux_liveness_and_start_time(me);
        assert!(alive, "this process is running");
        assert_eq!(
            start,
            os_process_start_time(me),
            "the merged probe and `start_time_or_any`'s source must be ONE \
             number; `Info.info(pid, startTime)` compares them with a bare `!=`"
        );
        assert_eq!(
            foreign_start_time_or_dead(me),
            start_time_or_any(me),
            "and so must what `isAlive0` hands back"
        );

        assert_eq!(
            linux_liveness_and_start_time(i64::MAX),
            (false, None),
            "no process holds that pid, and an absent /proc/<pid>/stat is how \
             this platform says so"
        );
        assert_eq!(
            foreign_start_time_or_dead(i64::MAX),
            -1,
            "-1 is the only value `ProcessHandleImpl.isAlive()` reads as dead"
        );
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

        assert_eq!(handle_for_pid(tt(), pid), Some(handle), "by pid");

        // `isAlive0` answers a START TIME, not a boolean: >= 0 is alive and -1
        // is the only way to say otherwise.
        let alive =
            native_proc_handle_is_alive0(&mut MockNativeContext::new(), &[Value::Long(pid)])
                .unwrap();
        let Some(Value::Long(start)) = alive else {
            panic!("isAlive0 must answer a long, got {alive:?}");
        };
        assert!(start >= 0, "a running child is alive, got {start}");

        destroy_handle(tt(), handle, true);
        let code = wait_for_handle(tt(), handle);
        assert!(code != 0, "killed child should not report success: {code}");

        let dead = native_proc_handle_is_alive0(&mut MockNativeContext::new(), &[Value::Long(pid)])
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

    /// The Windows environment block is one NUL-separated String, not the
    /// `String[]` the old `create` registration decoded.
    ///
    /// The block is NUL-*terminated*, so the last split piece is empty and must
    /// not become a variable; `=C:=...` entries are Windows' per-drive
    /// current-directory pseudo-variables and are dropped rather than forwarded
    /// as a variable literally named `=C:`. An empty VALUE is legal and is kept.
    #[test]
    #[cfg(windows)]
    fn an_environment_block_splits_on_nul_and_drops_the_drive_pseudo_vars() {
        assert_eq!(
            parse_env_block("PATH=C:\\bin\0HOME=C:\\me\0"),
            vec![
                ("PATH".to_string(), "C:\\bin".to_string()),
                ("HOME".to_string(), "C:\\me".to_string()),
            ]
        );
        assert_eq!(
            parse_env_block("=C:=C:\\work\0FOO=bar\0"),
            vec![("FOO".to_string(), "bar".to_string())],
            "a per-drive cwd pseudo-variable is not an environment variable"
        );
        assert_eq!(
            parse_env_block("EMPTY=\0"),
            vec![("EMPTY".to_string(), String::new())],
            "an empty value is legal and must survive"
        );
        assert!(parse_env_block("").is_empty());
        assert!(
            parse_env_block("NOEQUALS\0").is_empty(),
            "an entry with no '=' is not a variable"
        );
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
    ///
    /// Windows reaches the same decoder through `ProcessImpl.create`'s
    /// `long[] stdHandles` rather than `forkAndExec`'s `int[] fds` — the values
    /// are `FdTable` ids on both platforms — so the test covers both arms.
    #[test]
    #[cfg(any(target_os = "linux", windows))]
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

        let waiter = std::thread::spawn(move || wait_for_handle(tt(), handle));
        // Give the waiter time to actually be inside `Child::wait()`. If it is
        // not yet, `destroy_handle` takes the fast path and the test still
        // asserts the thing that matters — that the child dies.
        std::thread::sleep(Duration::from_millis(200));

        assert!(destroy_handle(tt(), handle, true), "destroy must report success");
        let code = waiter.join().expect("waiter thread");
        assert_eq!(code, 128 + 9, "SIGKILL is reported as 128+signum");
    }

    /// A pid we never spawned resolves to nothing, and the bridge says so
    /// rather than reaching into the table with it.
    ///
    /// The literal -2 is written out on purpose: it is the value
    /// `ProcessHandleImpl.completion` compares against, so a test that spelled
    /// it `PROCESS_NOT_A_CHILD` on both sides would pass no matter what the
    /// constant was changed to.
    #[test]
    fn an_unspawned_pid_resolves_to_no_handle() {
        let foreign = 0x7fff_0000i64; // far above any pid this host will mint
        assert_eq!(handle_for_pid(tt(), foreign), None);
        let mut ctx = MockNativeContext::new();
        let rc =
            native_proc_handle_wait_for_process_exit0(&mut ctx, &[Value::Long(foreign)]).unwrap();
        // Windows (wave 100, lane L1): HotSpot's Windows native opens any pid
        // and answers -1 for one it cannot open; it never answers NOT_A_CHILD.
        #[cfg(windows)]
        assert_eq!(rc, Some(Value::Int(-1)), "no such process: -1, as HotSpot's Windows native");
        #[cfg(not(windows))]
        assert_eq!(rc, Some(Value::Int(-2)), "NOT_A_CHILD, not an exit status");
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
        let answer = native_proc_handle_is_alive0(&mut ctx, &[Value::Long(self_pid)]).unwrap();
        let Some(Value::Long(start)) = answer else {
            panic!("isAlive0 must answer a long, got {answer:?}");
        };
        assert!(start >= 0, "this process exists, got {start}");
    }

    /// The three answers `ProcessHandleImpl$Info.info(pid, startTime)` compares
    /// must be ONE number.
    ///
    /// That method is real JDK bytecode this VM cannot influence:
    ///
    /// ```text
    /// info.info0(pid);
    /// if (startTime != info.startTime) { info.command = null; ... }
    /// ```
    ///
    /// — a bare `!=`, with none of the `STARTTIME_ANY` wildcarding that
    /// `equals()` and `isAlive()` apply. So `info0`'s stamp, `isAlive0`'s
    /// return (which is what the handle's `startTime` field is built from) and
    /// `current_process_start_time` (which builds THE `current()` handle) have
    /// to agree exactly, or every field `info0` just wrote is discarded by its
    /// own caller and `ProcessHandle.current().info()` reports an empty record
    /// while nothing throws.
    ///
    /// This asserts the agreement, not a particular value: a host that cannot
    /// read a start time at all legitimately answers `STARTTIME_ANY` (0) from
    /// all three, and that is still a match.
    #[test]
    fn info0_stamps_the_start_time_isalive0_and_current_both_report() {
        let mut ctx = MockNativeContext::new();
        let pid = std::process::id() as i64;

        let info = MockNativeContext::alloc_object(&mut ctx, 0);
        native_proc_handle_info0(&mut ctx, &[Value::Object(Some(info)), Value::Long(pid)])
            .expect("info0 must not throw");
        let stamped = ctx.get_field_by_name(info, "startTime");

        let answer = native_proc_handle_is_alive0(&mut ctx, &[Value::Long(pid)]).unwrap();
        let Some(alive) = answer else {
            panic!("isAlive0 must answer a long, got {answer:?}");
        };

        assert_eq!(
            stamped, alive,
            "info0's Info.startTime and isAlive0's return are the two sides of \
             `startTime != info.startTime` in ProcessHandleImpl$Info.info; a \
             difference silently empties ProcessHandle.current().info()"
        );
        assert_eq!(
            stamped,
            Value::Long(current_process_start_time()),
            "and current_process_start_time builds the handle that supplies the \
             left-hand side of that same comparison"
        );
    }

    /// `info0` must report a command for this very process.
    ///
    /// `RJdkProcess.java:135` guards its check with
    /// `if (info.command().isPresent())`, so an empty `command` does not fail
    /// the vector — it makes one `check(...)` silently not run, and HotSpot 25
    /// runs it. `command` is the one `Info` field both platform arms can
    /// source: `/proc/<pid>/cmdline` on Linux, `QueryFullProcessImageNameW` on
    /// Windows.
    #[test]
    #[cfg(any(target_os = "linux", windows))]
    fn info0_reports_a_command_for_this_process() {
        let mut ctx = MockNativeContext::new();
        let info = MockNativeContext::alloc_object(&mut ctx, 0);
        native_proc_handle_info0(
            &mut ctx,
            &[
                Value::Object(Some(info)),
                Value::Long(std::process::id() as i64),
            ],
        )
        .expect("info0 must not throw");
        assert!(
            matches!(
                ctx.get_field_by_name(info, "command"),
                Value::Object(Some(_))
            ),
            "Info.command must be a String, not the null the constructor leaves \
             behind — an absent one takes RJdkProcess.java:136 with it"
        );
    }

    /// Interpreter round i5 wave 101, lane L1: Linux `Info.command` is the
    /// resolved `/proc/<pid>/exe` (HotSpot's `os_getCmdlineAndUserInfo`), not
    /// `argv[0]`; the arguments and the command line are the record's words.
    #[test]
    #[cfg(target_os = "linux")]
    fn w101_l1_linux_info_command_is_the_resolved_executable() {
        let mut child = Command::new("/bin/sh")
            .args(["-c", "sleep 30; true"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn /bin/sh");
        let pid = child.id() as i64;
        let read = os_process_cmdline(pid).expect("/proc cmdline");
        let _ = child.kill();
        let _ = child.wait();
        let exe = std::fs::canonicalize("/bin/sh")
            .expect("canonical /bin/sh")
            .to_string_lossy()
            .into_owned();
        assert_eq!(read.command.as_deref(), Some(exe.as_str()));
        assert_eq!(
            read.arguments,
            Some(vec!["-c".to_string(), "sleep 30; true".to_string()])
        );
        assert_eq!(read.command_line.as_deref(), Some("/bin/sh -c sleep 30; true"));
    }

    /// `destroy0` must refuse a pid whose start time disagrees with the
    /// caller's — that is the whole guard against acting on a recycled pid —
    /// and must trust a caller that never learned one.
    #[test]
    #[cfg(target_os = "linux")]
    fn destroy_refuses_a_stale_start_time_and_trusts_an_absent_one() {
        let child = Command::new("/bin/sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn /bin/sleep");
        let pid = child.id() as i64;
        // Deliberately NOT installed in the process table: this is the foreign
        // path, and a child in the table is destroyed through its `Child`.
        let mut child = child;

        let real = os_process_start_time(pid).expect("/proc start time");
        assert!(start_time_matches(pid, real), "its own start time");
        assert!(
            !start_time_matches(pid, real - 1),
            "a start time from some earlier holder of this pid"
        );
        assert!(
            start_time_matches(pid, PROCESS_STARTTIME_ANY),
            "a caller that never learned one is trusted, as in HotSpot"
        );
        // An exited process has no start time to disagree with, so the check
        // cannot be what stops a signal — `kill` failing is.
        assert!(
            start_time_matches(pid + 4_000_000, real),
            "unknown is not disagreement"
        );

        let _ = child.kill();
        let _ = child.wait();
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

        assert_eq!(wait_for_handle(tt(), handle), 0);
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
        let marker =
            std::env::temp_dir().join(format!("cratonvm-spawn-policy-{}", std::process::id()));
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
                assert_eq!(wait_for_handle(tt(), handle), 0);
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
        let Value::Long(start) = alive else {
            panic!("isAlive0 must answer a long, got {alive:?}");
        };
        assert!(start >= 0, "a running child must be alive, got {start}");

        // On a host with `/proc` the answer is a real epoch-millisecond start
        // time, and the value is what makes a pid safe to act on — so it has to
        // be STABLE. A start time that changed between calls would pass
        // `destroy0`'s staleness check only by accident. Not asserted as a
        // literal: it is different on every run, and different again on a host
        // without `/proc`, where the honest answer is the `STARTTIME_ANY` 0.
        let again = native_proc_handle_is_alive0(&mut ctx, &[Value::Long(pid)])
            .unwrap()
            .unwrap();
        assert_eq!(again, alive, "the start time must not move under us");
        if cfg!(target_os = "linux") {
            assert!(
                start > 0,
                "on Linux the start time comes from /proc/<pid>/stat, got {start}"
            );
            assert!(
                start_time_matches(pid, start),
                "the value isAlive0 reports must be the one destroy0 accepts"
            );
            assert!(
                !start_time_matches(pid, start + 1),
                "and a value it did NOT report must be refused, or the check is \
                 decoration"
            );
        }

        // A pid that is neither ours nor anyone's must be -1. (Deliberately
        // NOT asserted for the internal handle id: on Linux a small integer is
        // a perfectly plausible live pid -- `/proc/1` is init -- so that
        // assertion would be testing the host, not this code.)
        let nobody = native_proc_handle_is_alive0(&mut ctx, &[Value::Long(pid + 4_000_000)])
            .unwrap()
            .unwrap();
        assert_eq!(nobody, Value::Long(-1), "an unused pid is not alive");

        assert!(destroy_handle(tt(), handle, true));
        let _ = wait_for_handle(tt(), handle);

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

        // A pid we never spawned cannot be waited for, and the way to say so
        // is `NOT_A_CHILD` (-2), not -1. This assertion read -1 when it was
        // written; -1 is a value `ProcessHandleImpl.completion` hands the
        // caller as the process's exit status, so it completed
        // `ProcessHandle.of(pid).onExit()` immediately for a live process.
        // See `probes/ForeignHandleProbe.java`.
        let unknown = native_proc_handle_wait_for_process_exit0(
            &mut ctx,
            &[Value::Long(pid + 4_000_000), Value::Int(0)],
        )
        .unwrap()
        .unwrap();
        assert_eq!(unknown, Value::Int(-2));

        let killed =
            native_proc_handle_destroy_process0(&mut ctx, &[Value::Long(pid), Value::Int(1)])
                .unwrap()
                .unwrap();
        assert_eq!(
            killed,
            Value::Int(1),
            "destroyProcess0 must find the child by pid"
        );

        let code =
            native_proc_handle_wait_for_process_exit0(&mut ctx, &[Value::Long(pid), Value::Int(0)])
                .unwrap()
                .unwrap();
        assert!(
            matches!(code, Value::Int(c) if c != -1 && c != -2),
            "waitForProcessExit0 must find the child by pid, got {code:?}"
        );

        let _ = wait_for_handle(tt(), handle);
    }

    /// Interpreter round i2 wave 70, lane L1 (`CRATONVM_PROCESS_EXIT_SIGNAL`):
    /// an exit reaper publishes the child's exit through the signal, so a
    /// waiter that read the generation before the exit wakes without waiting
    /// out its slice, and a second `ensure` for the same handle starts no
    /// second reaper.
    #[test]
    fn the_exit_reaper_publishes_the_exit_through_the_signal() {
        #[cfg(target_os = "windows")]
        let child = Command::new("cmd")
            .args(["/c", "exit", "3"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn cmd /c exit 3");
        #[cfg(not(target_os = "windows"))]
        let child = Command::new("/bin/sh")
            .args(["-c", "exit 3"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn /bin/sh -c 'exit 3'");
        let handle = 42424343i64;
        let pid = child.id() as i64;
        tt().children.lock().insert(handle, Arc::new(Mutex::new(child)));
        tt().exits.lock().insert(
            handle,
            ExitCache {
                pid,
                exit_code: None,
            },
        );

        let deadline = Instant::now() + Duration::from_secs(30);
        let mut seen = process_exit_generation(tt());
        assert!(ensure_process_exit_reaper(tt(), handle));
        assert!(
            ensure_process_exit_reaper(tt(), handle),
            "a second ensure is a no-op"
        );
        let code = loop {
            if let Some(code) = tt().exits.lock().get(&handle).and_then(|c| c.exit_code) {
                break code;
            }
            assert!(
                Instant::now() < deadline,
                "the reaper never published the exit"
            );
            wait_process_exit_signal(tt(), seen, Duration::from_millis(500));
            seen = process_exit_generation(tt());
        };
        assert_eq!(code, 3);
        assert_eq!(try_exit_handle(tt(), handle), Some(3));
        // A generation that already moved returns at once, whatever the slice.
        let before = Instant::now();
        wait_process_exit_signal(
            tt(),
            process_exit_generation(tt()).wrapping_sub(1),
            Duration::from_secs(30),
        );
        assert!(before.elapsed() < Duration::from_secs(5));
    }

    /// Interpreter round i5 wave 102, lane L1 (`CRATONVM_PROCESS_PIDFD_WAIT`):
    /// a pidfd wait wakes at the child's exit and answers its code, reaped
    /// (no zombie: a second `try_exit_handle` answers from the cache and
    /// `waitid` finds nothing left), and a killed child answers 128 + signal.
    /// Skipped where `pidfd_open` is refused (a pre-5.3 kernel, seccomp).
    #[cfg(target_os = "linux")]
    #[test]
    fn w102_l1_a_pidfd_wait_answers_the_exit_and_reaps() {
        for (script, expected, handle) in [
            ("exit 7", 7, 42424410i64),
            ("kill -9 $$", 128 + 9, 42424411i64),
        ] {
            let child = Command::new("/bin/sh")
                .args(["-c", script])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn /bin/sh");
            let pid = child.id() as i64;
            tt().children.lock().insert(handle, Arc::new(Mutex::new(child)));
            tt().exits.lock().insert(
                handle,
                ExitCache {
                    pid,
                    exit_code: None,
                },
            );
            let Some(wait) = LinuxPidfdWait::open(tt(), handle) else {
                eprintln!("pidfd_open refused here: skipped");
                return;
            };
            let deadline = Instant::now() + Duration::from_secs(30);
            let code = loop {
                if let Some(code) = wait.wait(handle, Duration::from_millis(500)) {
                    break code;
                }
                assert!(Instant::now() < deadline, "the pidfd never reported the exit");
            };
            assert_eq!(code, expected, "{script}");
            assert_eq!(try_exit_handle(tt(), handle), Some(expected));
            // Reaped by the waiter: nothing is left for `waitid`.
            // SAFETY: an all-zero `siginfo_t` is a valid out-parameter.
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            // SAFETY: `info` is a live local.
            let r = unsafe {
                libc::waitid(
                    libc::P_PID,
                    pid as libc::id_t,
                    &mut info,
                    libc::WEXITED | libc::WNOHANG,
                )
            };
            assert_eq!(r, -1, "the child was left a zombie");
            drop(wait);
        }
    }

    /// Interpreter round i7 wave 136, lane L1b (`CRATONVM_PROCESS_PIDFD_REAPER`):
    /// the pidfd reaper waits with the child's mutex free (another thread's
    /// `try_lock` succeeds while it waits), answers the exit code, caches it
    /// and leaves no zombie. Skipped where `pidfd_open` is refused.
    #[cfg(target_os = "linux")]
    #[test]
    fn i136_l1b_the_pidfd_reaper_waits_without_the_childs_mutex() {
        let handle = 42424420i64;
        let child = Command::new("/bin/sh")
            .args(["-c", "sleep 1; exit 5"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn /bin/sh");
        let pid = child.id() as i64;
        let entry = Arc::new(Mutex::new(child));
        tt().children.lock().insert(handle, Arc::clone(&entry));
        tt().exits.lock().insert(
            handle,
            ExitCache {
                pid,
                exit_code: None,
            },
        );
        if LinuxPidfdWait::open(tt(), handle).is_none() {
            eprintln!("pidfd_open refused here: skipped");
            return;
        }
        let waiter = std::thread::spawn(move || wait_for_handle_pidfd(tt(), handle));
        std::thread::sleep(Duration::from_millis(200));
        assert!(entry.try_lock().is_some(), "the reaper must not hold the child's mutex");
        assert_eq!(try_exit_handle(tt(), handle), None, "still running");
        assert_eq!(waiter.join().expect("the reaper"), Some(5));
        assert_eq!(try_exit_handle(tt(), handle), Some(5), "cached");
        // SAFETY: an all-zero `siginfo_t` is a valid out-parameter.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: `info` is a live local.
        let r = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG,
            )
        };
        assert_eq!(r, -1, "the child was left a zombie");
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
        tt().children.lock().insert(handle, Arc::new(Mutex::new(child)));
        tt().exits.lock().insert(
            handle,
            ExitCache {
                pid,
                exit_code: None,
            },
        );

        let code = wait_for_handle(tt(), handle);
        assert_eq!(code, 0, "echo should exit 0");
        // Second call must be idempotent and return the cached code.
        let code_again = wait_for_handle(tt(), handle);
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
        tt().children.lock().insert(handle, Arc::new(Mutex::new(child)));
        tt().exits.lock().insert(
            handle,
            ExitCache {
                pid,
                exit_code: None,
            },
        );

        let killed = destroy_handle(tt(), handle, true);
        assert!(killed, "destroy_handle should succeed for live child");

        // After kill, waitFor returns some exit code (non-zero on
        // Unix; Windows returns 1).
        let _code = wait_for_handle(tt(), handle);
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
        // Interpreter round i3 wave 80, lane L6: a child that has already
        // exited answers before any region (the JDK body's `hasExited` fast
        // path), so the count is one region or none, always balanced.
        let (begin, end) = ctx.blocking_region_counts();
        assert!(begin <= 1, "at most one region, got {begin}");
        assert_eq!(begin, end, "blocked-region enter/leave must balance");
    }

    /// Interpreter round i3 wave 80, lane L6
    /// (`i70-L1-synthetic-process-waitfor-ignores-interrupt-20261103.md`):
    /// an interrupted waiter on a RUNNING child gets `InterruptedException`
    /// (no message) from both stand-ins, with its interrupt status cleared, as
    /// from the JDK's `ConditionObject.await` / `awaitNanos`.
    #[cfg(not(target_os = "windows"))]
    #[test]
    fn i80_process_wait_for_throws_when_interrupted_on_a_running_child() {
        let child = Command::new("/bin/sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sleep");
        let (handle, pid) = install_child_for_test(child);
        let mut ctx = MockNativeContext::new();
        let proc_ref = mock_process(&mut ctx, handle, pid);

        let is_ie = |ctx: &MockNativeContext, failed: &MethodCallFailed| match failed {
            MethodCallFailed::ExceptionThrown(exc) => {
                ctx.class_name_of_id(ctx.class_id_of_object(*exc))
                    .as_deref()
                    == Some("java/lang/InterruptedException")
            }
            _ => false,
        };

        ctx.set_interrupted(true);
        let started = Instant::now();
        let untimed = native_process_wait_for(&mut ctx, &[Value::Object(Some(proc_ref))])
            .expect_err("an interrupted waitFor() must throw");
        assert!(
            is_ie(&ctx, &untimed),
            "waitFor() throws InterruptedException"
        );
        assert!(
            !ctx.is_interrupted(false),
            "the throw consumes the interrupt"
        );

        ctx.set_interrupted(true);
        // Wave 102: a null unit is the JDK bodies' NPE now; a scripted one.
        let unit = mock_millis_unit(&mut ctx, 60_000);
        let timed = native_process_wait_for_timeout(
            &mut ctx,
            &[
                Value::Object(Some(proc_ref)),
                Value::Long(60_000),
                Value::Object(Some(unit)),
            ],
        )
        .expect_err("an interrupted waitFor(60 s) must throw");
        assert!(
            is_ie(&ctx, &timed),
            "waitFor(long, TimeUnit) throws InterruptedException"
        );
        assert!(
            !ctx.is_interrupted(false),
            "the throw consumes the interrupt"
        );
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "neither wait may outlast the interrupt"
        );
        let (begin, end) = ctx.blocking_region_counts();
        assert_eq!(begin, end, "blocked-region enter/leave must balance");

        let _ = destroy_handle(tt(), handle, true);
        let _ = wait_for_handle(tt(), handle);
    }

    /// Interpreter round i4 wave 96, lane L1
    /// (`interpreter-i96-L1-windows-processimpl-wait-ignores-interrupt-FIXED-20261128.md`): the
    /// Windows `ProcessImpl.waitForInterruptibly` / `waitForTimeoutInterruptibly`
    /// natives return at once for an interrupted waiter on a RUNNING child
    /// (the Java body then throws), leaving the status for that body to read,
    /// and return when the child exits, not at a poll.
    #[cfg(windows)]
    #[test]
    fn i96_windows_processimpl_waits_see_the_interrupt_and_the_exit() {
        let child = Command::new("ping")
            .args(["-n", "30", "127.0.0.1"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn ping");
        let (handle, _pid) = install_child_for_test(child);
        let mut ctx = MockNativeContext::new();

        ctx.set_interrupted(true);
        let started = Instant::now();
        native_process_impl_wait_for_interruptibly(&mut ctx, &[Value::Long(handle)])
            .expect("the native never throws");
        native_process_impl_wait_for_timeout(
            &mut ctx,
            &[Value::Long(handle), Value::Long(60_000)],
        )
        .expect("the native never throws");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "an interrupted waiter must not wait for the child"
        );
        assert!(
            ctx.is_interrupted(false),
            "the natives leave the status for the Java body's Thread.interrupted()"
        );
        assert!(try_exit_handle(tt(), handle).is_none(), "the child still runs");

        ctx.set_interrupted(false);
        let killer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            let _ = destroy_handle(tt(), handle, true);
        });
        let started = Instant::now();
        native_process_impl_wait_for_timeout(
            &mut ctx,
            &[Value::Long(handle), Value::Long(60_000)],
        )
        .expect("the native never throws");
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "the timed wait returns when the child exits"
        );
        assert!(try_exit_handle(tt(), handle).is_some(), "the child has exited");
        native_process_impl_wait_for_interruptibly(&mut ctx, &[Value::Long(handle)])
            .expect("the native never throws");
        let (begin, end) = ctx.blocking_region_counts();
        assert_eq!(begin, end, "blocked-region enter/leave must balance");
        let _ = killer.join();
    }

    /// Interpreter round i3 wave 80, lane L6: the `onExit()` waiter carries the
    /// name HotSpot's reaper takes for its wait.
    #[test]
    fn i80_exit_waiter_thread_is_named_like_the_jdk_reaper() {
        assert_eq!(exit_waiter_thread_name(4242), "process reaper (pid 4242)");
        assert_eq!(exit_waiter_thread_name(-1), "process reaper");
    }

    /// Interpreter round i3 wave 80, lane L6: `exitValue()` on a running child
    /// raises the platform's JDK 25 `ProcessImpl` text.
    #[test]
    fn i80_exit_value_message_is_the_platform_jdks() {
        let want = if cfg!(windows) {
            "process has not exited"
        } else {
            "process hasn't exited"
        };
        assert_eq!(exit_value_not_exited_message(), want);
    }

    /// Interpreter round i3 wave 80, lane L6: a pre-interrupted `waitFor()` on
    /// a child that has already exited answers its exit code on UNIX (the JDK
    /// body's `while (!hasExited)` never awaits) and keeps the status set.
    #[cfg(not(target_os = "windows"))]
    #[test]
    fn i80_process_wait_for_on_an_exited_child_ignores_the_interrupt() {
        let child = Command::new("/bin/echo")
            .arg("x")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn");
        let (handle, pid) = install_child_for_test(child);
        let _ = wait_for_handle(tt(), handle);
        let mut ctx = MockNativeContext::new();
        let proc_ref = mock_process(&mut ctx, handle, pid);
        ctx.set_interrupted(true);
        let code = native_process_wait_for(&mut ctx, &[Value::Object(Some(proc_ref))])
            .unwrap()
            .unwrap();
        assert_eq!(code, Value::Int(0));
        assert!(
            ctx.is_interrupted(false),
            "no wait, so the status stays set"
        );
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
        let result =
            native_proc_handle_wait_for_process_exit0(&mut ctx, &[Value::Long(pid), Value::Int(0)])
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

        // Wave 102: a null unit is the JDK bodies' NPE now; a scripted one.
        let unit = mock_millis_unit(&mut ctx, 20);
        let result = native_process_wait_for_timeout(
            &mut ctx,
            &[
                Value::Object(Some(proc_ref)),
                Value::Long(20),
                Value::Object(Some(unit)),
            ],
        )
        .unwrap()
        .unwrap();
        assert_eq!(result, Value::Int(0));
        let (begin, end) = ctx.blocking_region_counts();
        assert!(begin >= 1, "timed wait should enter a blocked region");
        assert_eq!(begin, end, "blocked-region enter/leave must balance");

        let _ = destroy_handle(tt(), handle, true);
        let _ = wait_for_handle(tt(), handle);
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

        // Wave 102: a null unit is the JDK bodies' NPE now; a scripted one.
        let unit = mock_millis_unit(&mut ctx, 20);
        let result = native_process_wait_for_timeout(
            &mut ctx,
            &[
                Value::Object(Some(foreign)),
                Value::Long(20),
                Value::Object(Some(unit)),
            ],
        )
        .unwrap()
        .unwrap();

        // The mock's `invoke_virtual` has no `exitValue` to run, so the poll
        // never sees an exit and the wait times out — which is the specified
        // answer for a process that has not exited, and the answer the
        // pre-guard code got wrong by reading slot bytes off a stranger.
        assert_eq!(
            result,
            Value::Int(0),
            "a never-exiting process waits out its timeout"
        );
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
        tt().children.lock().insert(handle, Arc::new(Mutex::new(child)));
        tt().exits.lock().insert(
            handle,
            ExitCache {
                pid,
                exit_code: None,
            },
        );
        assert_eq!(pid_for_handle(tt(), handle), pid);
        let _ = wait_for_handle(tt(), handle);
        // pid should still be retrievable post-wait.
        assert_eq!(pid_for_handle(tt(), handle), pid);
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

/// JIT round 23 wave 4, lane L6: child pipes are OS-keyed
/// (`CRATONVM_FD_TABLE_OS_KEYS`), so the synthetic `Process` must never hand a
/// closed pipe's id to a second holder.
#[cfg(test)]
mod r23_w4_l6_process_pipe_tests {
    use super::*;
    use crate::test_support::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    /// Each getter returns ONE stream object per pipe, as the JDK's
    /// `ProcessImpl` does: closing it closes the pipe for every holder.
    #[test]
    fn each_getter_returns_one_stream_per_pipe() {
        let mut ctx = MockNativeContext::new();
        let process = ctx.alloc_object_with_class(PROC_FIELD_COUNT, SYNTHETIC_PROCESS_CLASS);
        for slot in [PROC_FIELD_STDIN_FD, PROC_FIELD_STDOUT_FD, PROC_FIELD_STDERR_FD] {
            ctx.set_field(process, slot, Value::Int(-1));
        }
        let args = [Value::Object(Some(process))];
        let a = native_process_get_input_stream(&mut ctx, &args).expect("stream");
        let b = native_process_get_input_stream(&mut ctx, &args).expect("stream");
        assert!(matches!(a, Some(Value::Object(Some(_)))), "{a:?}");
        assert_eq!(a, b, "getInputStream() == getInputStream()");
        let e = native_process_get_error_stream(&mut ctx, &args).expect("stream");
        let o = native_process_get_output_stream(&mut ctx, &args).expect("stream");
        assert_ne!(a, e, "stdout and stderr are different pipes");
        assert_ne!(a, o);
        assert_eq!(e, native_process_get_error_stream(&mut ctx, &args).expect("stream"));
        assert_eq!(o, native_process_get_output_stream(&mut ctx, &args).expect("stream"));
    }

    /// The row fallback hands out an id only while the table still holds the
    /// entry spawned under it.
    #[test]
    fn the_row_fallback_refuses_a_closed_or_reused_id() {
        let ctx = MockNativeContext::new();
        let path = std::env::temp_dir().join(format!(
            "cratonvm_r23w4_pipe_{}.txt",
            std::process::id()
        ));
        std::fs::write(&path, b"x").unwrap();
        let fd = ctx.fd_table().open_read(&path.to_string_lossy()).unwrap();
        let gen = ctx.fd_table().generation(fd).unwrap();
        let row = PipeFds {
            stdin_fd: -1,
            stdout_fd: fd as i32,
            stderr_fd: -1,
            gens: [0, gen, 0],
        };
        assert_eq!(row.live_fd(&ctx, row.stdout_fd, 1), fd as i32);
        assert_eq!(row.live_fd(&ctx, row.stdin_fd, 0), -1, "no pipe");
        let stale = PipeFds {
            gens: [0, gen.wrapping_add(1), 0],
            ..row
        };
        assert_eq!(stale.live_fd(&ctx, stale.stdout_fd, 1), -1, "another owner's generation");
        ctx.fd_table().close(fd).unwrap();
        assert_eq!(row.live_fd(&ctx, row.stdout_fd, 1), -1, "closed");
        let _ = std::fs::remove_file(&path);
    }
}
