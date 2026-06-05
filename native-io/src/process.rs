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
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::OnceLock;

use parking_lot::Mutex;

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallResult, RuntimeError};
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

#[derive(Clone, Copy, Debug)]
struct ExitCache {
    pid: i64,
    exit_code: Option<i32>,
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

// ---------------------------------------------------------------------------
// Spawn + teardown primitives
// ---------------------------------------------------------------------------

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
) -> MethodCallResult {
    if program.is_empty() {
        return Err(RuntimeError::IllegalArgumentException {
            message: "ProcessBuilder: empty program".to_string(),
        }
        .into());
    }

    let mut command = Command::new(program);
    command.args(args);
    command.stdin(Stdio::piped());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());

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

    // Pull the three pipe handles out of `child` so we can hand them
    // to the fd_table.  They are all `Option`s because `stdin`/`stdout`/
    // `stderr` are inherited by default; `spawn()` only populates them
    // because we explicitly requested `Stdio::piped()` above.
    let stdin_fd = child
        .stdin
        .take()
        .map(|s| ctx.fd_table().insert_child_stdin(s))
        .map(|fd| fd as i32)
        .unwrap_or(-1);
    let stdout_fd = child
        .stdout
        .take()
        .map(|s| ctx.fd_table().insert_child_stdout(s))
        .map(|fd| fd as i32)
        .unwrap_or(-1);
    let stderr_fd = child
        .stderr
        .take()
        .map(|s| ctx.fd_table().insert_child_stderr(s))
        .map(|fd| fd as i32)
        .unwrap_or(-1);

    let pid = child.id() as i64;
    let handle = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
    process_table().lock().insert(handle, child);
    exit_cache().lock().insert(
        handle,
        ExitCache {
            pid,
            exit_code: None,
        },
    );

    // Allocate synthetic Process and populate its 6 fields.
    let proc_ref = ctx.alloc_object(cratonvm_types::ClassId::new(0), PROC_FIELD_COUNT);
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
        Err(_) => return -1,
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

/// Return the process-table handle stored in a `java/lang/Process` synthetic,
/// or 0 if the field is missing (unknown / reaped).
fn handle_of(ctx: &mut dyn NativeContext, proc_ref: ObjectRef) -> i64 {
    match ctx.get_field(proc_ref, PROC_FIELD_HANDLE) {
        Value::Long(h) => h,
        _ => 0,
    }
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
fn native_process_impl_create(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
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
                    .filter_map(|s| s.split_once('=').map(|(k, v)| (k.to_string(), v.to_string())))
                    .collect(),
            )
        }
        _ => None,
    };
    let work_dir = match args.get(2) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s),
        _ => None,
    };

    let result = spawn_and_wrap(
        ctx,
        program,
        rest,
        work_dir.as_deref(),
        env_vars.as_deref(),
        env_vars.is_some(),
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
fn native_unix_fork_and_exec(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
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
                text.split_once('=').map(|(k, v)| (k.to_string(), v.to_string()))
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

    let result = spawn_and_wrap(
        ctx,
        &program,
        &args_vec,
        work_dir.as_deref(),
        env_vars.as_deref(),
        env_vars.is_some(),
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
fn native_proc_handle_is_alive0(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
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
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let handle = match args.first() {
        Some(Value::Long(h)) => *h,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let code = wait_for_handle(handle);
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
fn native_process_wait_for(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let handle = handle_of(ctx, this);
    if handle == 0 {
        // Legacy / stub Process (from phases_late ProcessBuilder.start) —
        // fall back to reading the cached exit code in field 0.
        return Ok(Some(ctx.get_field(this, PROC_FIELD_EXIT)));
    }
    let code = wait_for_handle(handle);
    ctx.set_field(this, PROC_FIELD_EXIT, Value::Int(code));
    Ok(Some(Value::Int(code)))
}

/// `java.lang.Process.exitValue()I`
///
/// Throws `IllegalThreadStateException` if the process is still running
/// (matches HotSpot behavior).
fn native_process_exit_value(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
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
        None => Err(RuntimeError::IllegalStateException {
            message: "process has not exited".to_string(),
        }
        .into()),
    }
}

/// `java.lang.Process.isAlive()Z`
fn native_process_is_alive(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
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
fn native_process_destroy(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
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

/// `java.lang.Process.pid()J`
fn native_process_pid(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
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
    registry.register("java/lang/Process", "waitFor", "()I", native_process_wait_for);
    registry.register(
        "java/lang/Process",
        "exitValue",
        "()I",
        native_process_exit_value,
    );
    registry.register("java/lang/Process", "isAlive", "()Z", native_process_is_alive);
    registry.register("java/lang/Process", "destroy", "()V", native_process_destroy);
    registry.register(
        "java/lang/Process",
        "destroyForcibly",
        "()Ljava/lang/Process;",
        native_process_destroy_forcibly,
    );
    registry.register("java/lang/Process", "pid", "()J", native_process_pid);

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
fn native_process_builder_start(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
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

    // --- Field 1: directory (File) ---
    let work_dir: Option<String> = match ctx.get_field(this, 1) {
        Value::Object(Some(file_obj)) => match ctx.get_field(file_obj, 0) {
            Value::Object(Some(s)) => ctx.read_string(s),
            _ => None,
        },
        _ => None,
    };

    // --- Spawn ---
    let program = cmd_strings[0].clone();
    let rest: Vec<String> = cmd_strings.into_iter().skip(1).collect();
    spawn_and_wrap(ctx, &program, &rest, work_dir.as_deref(), None, false)
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn tokenize_keeps_quoted_paths_intact() {
        // A quoted program path with spaces must stay a single argv[0].
        let argv = tokenize_command_line(r#""C:\Program Files\Java\bin\java.exe" -version"#);
        assert_eq!(
            argv,
            vec![r"C:\Program Files\Java\bin\java.exe".to_string(), "-version".to_string()]
        );
    }

    #[test]
    fn tokenize_plain_and_quoted_args() {
        assert_eq!(
            tokenize_command_line("prog -cp \"a b\" Main"),
            vec!["prog".to_string(), "-cp".to_string(), "a b".to_string(), "Main".to_string()]
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
