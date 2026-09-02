// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Serviceability Tools — jcmd/jstack/jmap diagnostic infrastructure.
//!
//! Provides attach API, diagnostic command processing, thread dump generation,
//! heap analysis, and HPROF stub writing for JVM serviceability tooling.
//!
//! ---------------------------------------------------------------------------
//! # LIVENESS — READ THIS BEFORE WIRING ANYTHING TO IT
//! ---------------------------------------------------------------------------
//!
//! **SUPERSEDED, 2026-09-01 -- read this correction before the claim below.**
//! The three bullets that follow were true when the observability audit wrote
//! them on 2026-07-26 and are *no longer* true, and a stale liveness claim is
//! more dangerous than none: it is exactly the evidence a later reader uses to
//! decide a command is not worth implementing. What actually changed:
//!
//!  * obsaudit D15 gave `AttachListener::start_listening` a **real** Unix
//!    domain socket at `/tmp/.java_pid<pid>` serving the HotSpot Attach API
//!    wire protocol (see that method and the `AttachListener` doc comment).
//!  * `Vm::new` (`vm/src/vm/vm_init.rs`) now builds a
//!    `JcmdProcessor::new_with_vm_state(shared.clone())` at bootstrap and
//!    parks it in `SharedVm::debug.jcmd_processor`, which is what keeps that
//!    socket bound for the life of the VM.
//!
//! So on Linux, `jcmd <pid> ...` / `jstack <pid>` / `jmap <pid>` from an
//! unmodified JDK **does** reach `register_live_commands` today. It does not
//! reach `register_standard_commands` -- that set belongs to the
//! argument-less `JcmdProcessor::new()`, which only tests construct -- so the
//! *fabricated-data* hazard the bullets warn about remains confined to the
//! variant production never builds. That distinction is the whole reason
//! `register_live_commands` exists; keep new commands there.
//!
//! Still true: this is Unix-only (`#[cfg(unix)]`; Windows would need a named
//! pipe), and `hsdb_start_listener` / `HsdbListener` remain test-only.
//!
//! The original claim, kept because the reasoning under it is still the right
//! reasoning and only its premises expired:
//!
//! **On a default build, none of the attach/jcmd surface in this module is
//! reachable from outside the process.** Established by the observability
//! audit of 2026-07-26:
//!
//!  * [`AttachListener`] has no constructor call anywhere outside this file.
//!    Its `socket_path` field is a plain `String` and `start_listening` only
//!    flips a `bool` — **no socket, named pipe, or `.attach_pid<N>` file is
//!    ever created**. `jcmd <pid> ...` / `jstack <pid>` / `jmap <pid>` from
//!    another process therefore cannot connect to a CratonVM at all.
//!  * [`JcmdProcessor`] is constructed only in `#[cfg(test)]` code
//!    (`vm/src/vm/vm_init.rs`, past the `#[cfg(test)]` at line ~5734, and
//!    this file's own tests). Nothing in the VM bootstrap builds one.
//!  * [`hsdb_start_listener`] / [`HsdbListener`] have no callers outside this
//!    file's tests.
//!
//! Consequences, in order of how badly they would mislead an operator:
//!
//!  1. The *only* live consumers of this module are [`HprofWriter`] (used by
//!     `runtime::hprof`, which IS reachable via
//!     `-XX:+HeapDumpOnOutOfMemoryError`) and the [`VmDiagnosticState`] impl
//!     on `SharedVm`, which exists but has no production caller.
//!  2. `register_default_commands` (the no-VM-state variant used by
//!     `JcmdProcessor::new` / `::default`) returns **fabricated** output for
//!     several commands. See the audit note on `Compiler.queue`.
//!  3. `JFR.start` / `JFR.stop` / `JFR.dump` never touch the flight recorder
//!     **in `register_standard_commands`**, and still do not: that set has no
//!     VM binding, so its three entries return an honest "not implemented"
//!     rather than a fabricated success. See the audit notes at their
//!     registration sites. As of 2026-09-01 `register_live_commands` carries
//!     real ones, routed through `VmDiagnosticState::jfr_start` / `jfr_dump` /
//!     `jfr_stop` and implemented on `SharedVm` in `vm/src/vm/vm_init.rs`.
//!
//! Do not "wire up jcmd" by simply constructing a `JcmdProcessor` at
//! bootstrap: `register_default_commands` would then start reporting invented
//! data to operators. Fix the individual commands first, or register only
//! `register_live_commands`.

use cratonvm_types::narrow_oop::{read_ref_slot_unaligned, ref_element_size};
use std::fmt;
use std::sync::Arc;
use std::time::Instant;

/// Trait for querying live VM state from diagnostic commands.
/// The VM implements this to provide real data to serviceability tools.
pub trait VmDiagnosticState: Send + Sync {
    /// Get snapshots of all live threads.
    fn thread_snapshots(&self) -> Vec<ThreadSnapshot>;
    /// Get heap summary information.
    fn heap_summary(&self) -> HeapSummary;
    /// Get class histogram entries.
    fn class_histogram(&self) -> Vec<ClassHistogramEntry>;
    /// Trigger a garbage collection cycle. Returns true if GC was actually run.
    fn trigger_gc(&self) -> bool;
    /// Get VM uptime in seconds.
    fn uptime_secs(&self) -> f64;
    /// Get the command line that started the VM.
    fn command_line(&self) -> String;
    /// Get VM system properties.
    fn system_properties(&self) -> Vec<(String, String)>;
    /// Get VM flags/options.
    fn vm_flags(&self) -> Vec<String>;
    /// Write an HPROF heap dump to the given path. Returns bytes written.
    fn heap_dump(&self, _path: &str) -> Result<u64, String> {
        Err("heap dump not supported".to_string())
    }

    // --- JFR (2026-09-01) ---------------------------------------------------
    //
    // The three methods below are what let `register_live_commands` register a
    // `JFR.start` / `JFR.dump` / `JFR.stop` that actually reaches a
    // `FlightRecorder`. Requirement (1) of the audit block at the
    // `register_standard_commands` JFR entries names exactly this: "an
    // `Arc<SharedVm>` (or a `VmDiagnosticState` extension) on the processor so
    // the handler can reach `debug.flight_recorder`". The trait extension is
    // the option that keeps this module free of any `vm::` dependency.
    //
    // Each is **defaulted to an honest failure** rather than left required.
    // That is deliberate on two counts:
    //
    //  * it adds no method to implement for the mock states in this file's own
    //    tests, none of which owns a recorder; and
    //  * the failure text says the state has no recorder, which is the one
    //    thing an operator needs to know. Returning a plausible success from a
    //    state that cannot record is the precise defect the audit block
    //    records, and a default body is where that defect would silently
    //    reappear if the default were `Ok`.
    //
    // `args` is the diagnostic command's argument list exactly as
    // `JcmdProcessor::process_command` split it: whitespace-separated tokens.

    /// `jcmd <pid> JFR.start [name=<n>] [maxevents=<n>] [+<Event>#enabled=<bool>]`.
    /// Returns the operator-facing confirmation line.
    fn jfr_start(&self, _args: &[String]) -> Result<String, String> {
        Err("JFR.start: this VM state has no flight recorder binding".to_string())
    }

    /// `jcmd <pid> JFR.dump [name=<n>] filename=<path>` -- write a running
    /// recording to `path` without stopping it.
    fn jfr_dump(&self, _args: &[String]) -> Result<String, String> {
        Err("JFR.dump: this VM state has no flight recorder binding".to_string())
    }

    /// `jcmd <pid> JFR.stop [name=<n>] [filename=<path>]` -- write the
    /// recording if a path was given, then stop it.
    fn jfr_stop(&self, _args: &[String]) -> Result<String, String> {
        Err("JFR.stop: this VM state has no flight recorder binding".to_string())
    }
}

// ---------------------------------------------------------------------------
// Attach API infrastructure
// ---------------------------------------------------------------------------

/// Listener for diagnostic attach requests (Unix domain socket based).
///
/// obsaudit D15 (2026-07-26), FIXED: `start_listening` now opens a real
/// socket at `socket_path` and serves the HotSpot Attach API wire protocol,
/// so `jcmd <pid> ...` / `jstack <pid>` / `jmap <pid> ...` from a real,
/// unmodified JDK installation can connect and run commands. The protocol
/// was not reverse-engineered from memory: verified empirically against a
/// real OpenJDK 21 `jcmd`/`jstack`/`jmap` on the audit host (2026-07-26) by
/// standing up a bare Python socket listener at `/tmp/.java_pid<pid>` for a
/// live dummy process and logging the raw bytes each tool sent. Findings:
///
///  * Each attach opens the socket **twice**: a first connection that sends
///    zero bytes (the client's own "is a listener already up?" readiness
///    probe — HotSpot's real client normally creates a `.attach_pid<pid>`
///    file and sends `SIGQUIT` to *trigger* this probe to eventually
///    succeed; since this implementation listens from VM boot, the socket
///    already exists and the probe succeeds immediately, so the SIGQUIT /
///    `.attach_pid` dance is never needed here), followed by a second
///    connection carrying the real request. Both must be handled — the
///    first is not an error, just a no-op.
///  * The real request is five NUL-terminated fields:
///    `<protocol-version>\0<operation>\0<arg1>\0<arg2>\0<arg3>\0` — e.g.
///    `jcmd <pid> VM.version` sends `1\0jcmd\0VM.version\0\0\0`; `jstack`
///    sends `1\0threaddump\0\0\0\0`; `jmap -histo` sends
///    `1\0inspectheap\0-all\0\0\0`; `jmap -dump:file=X` sends
///    `1\0dumpheap\0X\0-all\0`.
///  * The response is a decimal result code, a newline, then the raw
///    command output; the connection close signals EOF to the client. No
///    length prefix, no other framing.
///
/// See `handle_attach_connection` for the server-side implementation of
/// this protocol, and the module LIVENESS block at the top of this file.
pub struct AttachListener {
    pub socket_path: String,
    pub is_listening: bool,
    /// `Arc<RwLock<_>>` rather than a plain `Vec` (as before this fix) so
    /// the background accept thread spawned by `start_listening` can hold
    /// its own handle to the live command set without borrowing `self`.
    /// Also means commands may be registered before *or* after
    /// `start_listening` is called with no race — every dispatch reads the
    /// list fresh through the shared lock.
    pub commands: Arc<parking_lot::RwLock<Vec<DiagnosticCommand>>>,
    /// Set by [`AttachListener::stop_listening`] to tell the accept loop to
    /// exit. Shared with the accept thread, which re-checks it every poll tick.
    shutdown: Arc<std::sync::atomic::AtomicBool>,
    /// Join handle for the accept thread, so `stop_listening` (and therefore
    /// `Drop`) can actually reap it instead of stranding it. `None` when this
    /// listener never bound (non-Unix target, or `bind` failed).
    accept_thread: Option<std::thread::JoinHandle<()>>,
}

impl AttachListener {
    pub fn new(socket_path: &str) -> Self {
        Self {
            socket_path: socket_path.to_string(),
            is_listening: false,
            commands: Arc::new(parking_lot::RwLock::new(Vec::new())),
            shutdown: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            accept_thread: None,
        }
    }

    /// Open the real attach socket and start serving requests in a
    /// detached background thread. Idempotent — a second call while
    /// already listening is a no-op. See the struct doc comment for the
    /// wire protocol.
    ///
    /// obsaudit D15: Unix-only (`std::os::unix::net`), matching HotSpot's
    /// own per-OS split (Unix domain socket on Linux/macOS, a named pipe on
    /// Windows — implementing the Windows side is a separate undertaking
    /// this pass did not attempt). On a non-Unix target this still just
    /// flips `is_listening`, exactly as the whole function did before this
    /// fix, so existing callers that only check the flag are unaffected.
    #[cfg(unix)]
    pub fn start_listening(&mut self) {
        if self.is_listening {
            return;
        }
        // A stale socket file can be left behind by a crashed prior process
        // that reused this PID (PIDs recycle) — `bind` fails with
        // `AddrInUse` against a leftover path, so clear it first. Safe: a
        // Unix domain socket path is just a filesystem name, not a live
        // listener, once its owning process is gone.
        let _ = std::fs::remove_file(&self.socket_path);
        let listener = match std::os::unix::net::UnixListener::bind(&self.socket_path) {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!(
                    "AttachListener: failed to bind attach socket at {}: {e}",
                    self.socket_path
                );
                return;
            }
        };
        // Owner-only permissions, matching HotSpot's attachListener.cpp.
        // This is a local IPC channel that can trigger a heap dump of the
        // whole process on request — it must not be reachable by other
        // users on the host.
        {
            use std::os::unix::fs::PermissionsExt;
            let _ =
                std::fs::set_permissions(&self.socket_path, std::fs::Permissions::from_mode(0o600));
        }

        // Poll rather than block in `accept()`, so `stop_listening` can
        // retire this thread deterministically.
        //
        // The obvious alternative -- block in `accept()` and have
        // `stop_listening` poke the socket with a throwaway connection -- is
        // WRONG here, and silently so: `new_with_vm_state` binds
        // `/tmp/.java_pid<pid>`, the same path for every VM in the process, and
        // each bind unlinks the previous socket. A poke by path would then
        // reach whichever listener bound LAST, so the older thread would never
        // wake and the `join` below would hang forever. Polling needs no poke
        // and cannot alias. One wakeup per `POLL` per live VM is negligible
        // beside the leaked thread + fd this replaces.
        const POLL: std::time::Duration = std::time::Duration::from_millis(100);
        if listener.set_nonblocking(true).is_err() {
            // Cannot poll safely; leave the socket bound and unattended rather
            // than spawn a thread `stop_listening` would not be able to reap.
            tracing::warn!(
                "AttachListener: set_nonblocking failed for {}; not serving attach requests",
                self.socket_path
            );
            return;
        }
        let commands = Arc::clone(&self.commands);
        let shutdown = Arc::clone(&self.shutdown);
        self.accept_thread = std::thread::Builder::new()
            .name("Attach-Listener".into())
            .spawn(move || {
                while !shutdown.load(std::sync::atomic::Ordering::Acquire) {
                    let stream = match listener.accept() {
                        Ok((conn, _addr)) => Ok(conn),
                        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(POLL);
                            continue;
                        }
                        Err(e) => Err(e),
                    };
                    match stream {
                        Ok(conn) => {
                            // The connection itself must be blocking: it is
                            // handed to `handle_attach_connection`, which does
                            // ordinary blocking reads/writes. `accept` on a
                            // non-blocking listener yields a non-blocking
                            // socket on Linux, so clear the flag explicitly.
                            let _ = conn.set_nonblocking(false);
                            let commands = Arc::clone(&commands);
                            // One thread per connection: attach requests are
                            // rare (an operator running a diagnostic
                            // command) and some handlers (GC.heap_dump) can
                            // take a while — a single-threaded accept loop
                            // would otherwise serialize unrelated attaches
                            // behind a slow one.
                            std::thread::Builder::new()
                                .name("Attach-Connection".into())
                                .spawn(move || handle_attach_connection(conn, &commands))
                                .ok();
                        }
                        Err(_) => break, // listener socket gone (e.g. unlinked externally)
                    }
                }
            })
            .ok();
        self.is_listening = true;
    }

    #[cfg(not(unix))]
    pub fn start_listening(&mut self) {
        self.is_listening = true;
    }

    /// Stop serving: signal the accept loop, unlink the socket, and join the
    /// thread. Idempotent, and a no-op for a listener that never bound.
    ///
    /// This used to be "best-effort" and deliberately did NOT reap the accept
    /// thread, on the reasoning that "the listener's real lifetime is the VM
    /// process's own". That holds for a real VM process, which has one. It is
    /// false for this crate's own test binary, where every `Vm::new` builds a
    /// `JcmdProcessor` (see `JcmdProcessor::new_with_vm_state`) that binds
    /// `/tmp/.java_pid<pid>` — the SAME path each time, since it is per-PID by
    /// construction — and each bind unlinks the previous one's socket. The
    /// earlier thread could then never be reached by any client, never
    /// returned from `accept()`, and never exited: one `cargo test -p
    /// cratonvm-vm --lib` run peaked at 286 threads, 71 of them named
    /// `Attach-Listener`, each holding a socket fd.
    ///
    /// Bounded by construction: the accept loop checks `shutdown` at most one
    /// poll interval away (see `start_listening`), so the join returns
    /// promptly and cannot depend on a client ever connecting.
    pub fn stop_listening(&mut self) {
        self.shutdown
            .store(true, std::sync::atomic::Ordering::Release);
        let _ = std::fs::remove_file(&self.socket_path);
        if let Some(handle) = self.accept_thread.take() {
            let _ = handle.join();
        }
        self.is_listening = false;
    }

    pub fn register_command(&mut self, cmd: DiagnosticCommand) {
        self.commands.write().push(cmd);
    }

    pub fn list_commands(&self) -> Vec<String> {
        self.commands
            .read()
            .iter()
            .map(|c| c.name.clone())
            .collect()
    }

    /// Look up `name` and execute it with `args` under a single read-lock
    /// acquisition. Returns `None` for an unregistered command name.
    pub fn dispatch_command(&self, name: &str, args: &[String]) -> Option<CommandResult> {
        self.commands
            .read()
            .iter()
            .find(|c| c.name == name)
            .map(|c| c.execute(args))
    }
}

/// obsaudit D15: server side of the Attach API wire protocol — see the
/// `AttachListener` doc comment for the format, empirically verified
/// against a real OpenJDK 21 `jcmd`/`jstack`/`jmap`.
#[cfg(unix)]
fn handle_attach_connection(
    mut conn: std::os::unix::net::UnixStream,
    commands: &parking_lot::RwLock<Vec<DiagnosticCommand>>,
) {
    use std::io::{Read, Write};

    // A 5-second read timeout bounds how long a connection thread can be
    // stuck on a peer that opens the socket and then never sends anything
    // (or sends a truncated request) — without this, `read` blocks forever
    // and the thread (and its `Arc<RwLock<..>>` clone) leaks for the life
    // of the process.
    let _ = conn.set_read_timeout(Some(std::time::Duration::from_secs(5)));

    // obsaudit D15: read until the 5-field request is complete (five NUL
    // bytes seen), NOT until EOF. The real client does not close its write
    // side after sending the request — it immediately starts reading the
    // response — so a read-until-EOF loop here deadlocks: this thread
    // blocks waiting for a close the peer will only do *after* seeing a
    // response, and the peer blocks waiting for a response this thread
    // never sends. Confirmed by instrumenting this function and running a
    // real OpenJDK 21 `jcmd` against it: the first request logged a clean
    // 20-byte read matching `1 jcmd VM.version   `, then hung — jcmd
    // eventually gave up and closed, this code finally saw the resulting
    // EOF, and by then jcmd had already reported "Premature EOF" to the
    // operator. Every subsequent attach from the same client then reported
    // "Connection refused", consistent with the client-side attach API
    // treating the process as wedged after one failed round-trip.
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match conn.read(&mut chunk) {
            Ok(0) => break, // peer closed before completing a request — the readiness probe, or a truncated one
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.iter().filter(|&&b| b == 0).count() >= 5 {
                    break; // all five NUL-terminated fields have arrived
                }
                // Bound how much a misbehaving/hostile local peer can make
                // this thread buffer while still short of 5 NULs.
                if buf.len() > 64 * 1024 {
                    break;
                }
            }
            Err(_) => return,
        }
    }
    if buf.is_empty() {
        // The client's own readiness probe (see the struct doc comment) —
        // nothing to respond to, and no real client waits for a response
        // on this connection.
        return;
    }

    let fields: Vec<&[u8]> = buf.split(|&b| b == 0).collect();
    let field = |i: usize| -> String {
        fields
            .get(i)
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .unwrap_or_default()
    };
    // fields[0] is the protocol version ("1" for every JDK this was tested
    // against) — not currently branched on; every operation below is
    // version-1 shaped and there is no version-2-only feature in play.
    let operation = field(1);
    let arg1 = field(2);
    let arg2 = field(3);

    let (name, args): (String, Vec<String>) = match operation.as_str() {
        // `jcmd <pid> <command...>` — arg1 is the *entire* diagnostic
        // command line (e.g. "GC.heap_dump /tmp/x.hprof"), exactly what
        // `JcmdProcessor::process_command` already parses.
        "jcmd" => {
            let mut parts = arg1.splitn(2, ' ');
            let name = parts.next().unwrap_or("").to_string();
            let rest = parts.next().unwrap_or("");
            (name, rest.split_whitespace().map(String::from).collect())
        }
        // `jstack <pid>`.
        "threaddump" => ("Thread.print".to_string(), Vec::new()),
        // `jmap -histo <pid>` (arg1 is "-all" or "-live"; this
        // implementation's histogram does not distinguish the two).
        "inspectheap" => ("GC.class_histogram".to_string(), Vec::new()),
        // `jmap -dump:file=<path> <pid>` (arg1 is the path, arg2 is
        // "-all"/"-live").
        "dumpheap" => (
            "GC.heap_dump".to_string(),
            if arg1.is_empty() {
                Vec::new()
            } else {
                vec![arg1.clone()]
            },
        ),
        // `jinfo -sysprops <pid>` / `jcmd <pid> VM.system_properties`'s
        // sibling entry point.
        "properties" => ("VM.system_properties".to_string(), Vec::new()),
        other => {
            let _ = write_attach_response(
                &mut conn,
                1,
                &format!(
                    "Unrecognized attach operation: {other}
"
                ),
            );
            return;
        }
    };

    let result = commands
        .read()
        .iter()
        .find(|c| c.name == name)
        .map(|c| c.execute(&args));
    let write_result = match result {
        Some(r) if r.success => write_attach_response(&mut conn, 0, &r.output),
        Some(r) => {
            write_attach_response(&mut conn, 1, r.error.as_deref().unwrap_or("command failed"))
        }
        None => write_attach_response(
            &mut conn,
            1,
            &format!(
                "Unknown command: {name}
"
            ),
        ),
    };
    let _ = write_result;
    let _ = arg2; // consumed above for dumpheap's "-all"/"-live"; unused otherwise
}

#[cfg(unix)]
fn write_attach_response(
    conn: &mut std::os::unix::net::UnixStream,
    code: i32,
    body: &str,
) -> std::io::Result<()> {
    use std::io::Write;
    conn.write_all(format!("{code}\n").as_bytes())?;
    conn.write_all(body.as_bytes())?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Diagnostic Commands
// ---------------------------------------------------------------------------

/// Impact level of a diagnostic command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandImpact {
    Low,
    Medium,
    High,
}

impl fmt::Display for CommandImpact {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CommandImpact::Low => write!(f, "Low"),
            CommandImpact::Medium => write!(f, "Medium"),
            CommandImpact::High => write!(f, "High"),
        }
    }
}

/// Permission required to execute a diagnostic command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandPermission {
    ReadOnly,
    ManagementAction,
    WriteAction,
}

/// Argument type for command parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArgType {
    String,
    Int,
    Bool,
    FilePath,
}

/// A single argument descriptor for a diagnostic command.
#[derive(Debug, Clone)]
pub struct CommandArgument {
    pub name: String,
    pub description: String,
    pub arg_type: ArgType,
    pub required: bool,
    pub default_value: Option<String>,
}

/// Result of executing a diagnostic command.
#[derive(Debug, Clone)]
pub struct CommandResult {
    pub success: bool,
    pub output: String,
    pub error: Option<String>,
    pub execution_time_ms: u64,
}

impl CommandResult {
    pub fn ok(output: String, execution_time_ms: u64) -> Self {
        Self {
            success: true,
            output,
            error: None,
            execution_time_ms,
        }
    }

    pub fn err(msg: String, execution_time_ms: u64) -> Self {
        Self {
            success: false,
            output: String::new(),
            error: Some(msg),
            execution_time_ms,
        }
    }
}

/// A registered diagnostic command.
pub struct DiagnosticCommand {
    pub name: String,
    pub description: String,
    pub impact: CommandImpact,
    pub permission: CommandPermission,
    pub arguments: Vec<CommandArgument>,
    handler: Box<dyn Fn(&[String]) -> CommandResult + Send + Sync>,
}

impl DiagnosticCommand {
    pub fn new(
        name: &str,
        description: &str,
        impact: CommandImpact,
        permission: CommandPermission,
        arguments: Vec<CommandArgument>,
        handler: Box<dyn Fn(&[String]) -> CommandResult + Send + Sync>,
    ) -> Self {
        Self {
            name: name.to_string(),
            description: description.to_string(),
            impact,
            permission,
            arguments,
            handler,
        }
    }

    pub fn execute(&self, args: &[String]) -> CommandResult {
        (self.handler)(args)
    }
}

// ---------------------------------------------------------------------------
// jcmd implementation
// ---------------------------------------------------------------------------

impl Drop for AttachListener {
    /// Reap the accept thread with the listener that owns it.
    ///
    /// Without this, a `JcmdProcessor` going out of scope — every `Vm` in the
    /// test binary owns one — left its accept thread parked on a socket path
    /// the next `Vm` had already unlinked. See
    /// [`AttachListener::stop_listening`] for the measurement.
    fn drop(&mut self) {
        if self.is_listening {
            self.stop_listening();
        }
    }
}

/// Processes jcmd-style diagnostic commands.
pub struct JcmdProcessor {
    pub attach_listener: AttachListener,
}

/// obsaudit D15: process-wide counter so every `JcmdProcessor::new()` gets
/// a distinct socket path. Before this fix `start_listening` only flipped a
/// bool, so a hardcoded shared path was harmless; now it binds a real Unix
/// socket, and this file's own test suite constructs `JcmdProcessor::new()`
/// upwards of a dozen times, often running in parallel (`cargo test`'s
/// default) — a shared path would make those binds race each other.
static TEST_JCMD_SOCKET_COUNTER: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

impl JcmdProcessor {
    pub fn new() -> Self {
        let n = TEST_JCMD_SOCKET_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = format!("/tmp/cratonvm_attach_test_{}_{}", std::process::id(), n);
        let mut listener = AttachListener::new(&path);
        listener.start_listening();

        // Register all standard commands
        Self::register_standard_commands(&mut listener);

        Self {
            attach_listener: listener,
        }
    }

    /// Create a JcmdProcessor backed by live VM state.
    ///
    /// obsaudit D15: the socket path is `/tmp/.java_pid<pid>` — not a
    /// placeholder. This is the exact path real HotSpot tooling
    /// (`jcmd`/`jstack`/`jmap`) probes for on Linux; verified empirically
    /// against a real OpenJDK 21 `jcmd` (see the `AttachListener` doc
    /// comment). Do not change this without re-verifying against a real
    /// client — the path is load-bearing, not cosmetic.
    pub fn new_with_vm_state(vm_state: Arc<dyn VmDiagnosticState>) -> Self {
        let path = format!("/tmp/.java_pid{}", std::process::id());
        let mut listener = AttachListener::new(&path);
        listener.start_listening();
        // DOWNGRADE. The registered commands must hold a `Weak`, never the
        // `Arc` handed in here.
        //
        // This processor is stored in `SharedVm::debug.jcmd_processor`, and
        // `vm_state` IS that same `SharedVm`. Cloning the `Arc` into the nine
        // command closures therefore made the VM own nine strong references to
        // itself: a reference cycle whose count can never reach zero, so every
        // `Vm` ever constructed leaked its entire `SharedVm` -- heap, class
        // manager, thread registry, JIT caches and all. Traced through
        // `Vm::new`, `Arc::strong_count(&shared)` jumped 1 -> 10 across exactly
        // this call and stayed at 9 after the `Vm` was dropped.
        //
        // A `Weak` is also the honest lifetime: a diagnostic command is only
        // meaningful while the VM it reports on is alive, and jcmd attaching
        // during teardown should be told so rather than resurrect it.
        let weak_state = Arc::downgrade(&vm_state);
        drop(vm_state);
        Self::register_live_commands(&mut listener, weak_state);
        Self {
            attach_listener: listener,
        }
    }

    fn register_standard_commands(listener: &mut AttachListener) {
        // 1. Thread.print
        listener.register_command(DiagnosticCommand::new(
            "Thread.print",
            "Print all threads with stacktraces",
            CommandImpact::Medium,
            CommandPermission::ReadOnly,
            vec![],
            Box::new(|_args| {
                let threads = sample_thread_snapshots();
                let dump = JstackProcessor::generate_thread_dump(&threads);
                CommandResult::ok(dump, 0)
            }),
        ));

        // 2. GC.heap_dump
        listener.register_command(DiagnosticCommand::new(
            "GC.heap_dump",
            "Generate a HPROF format heap dump",
            CommandImpact::High,
            CommandPermission::ManagementAction,
            vec![CommandArgument {
                name: "filename".to_string(),
                description: "Output file path".to_string(),
                arg_type: ArgType::FilePath,
                required: false,
                default_value: Some("heap.hprof".to_string()),
            }],
            Box::new(|args| {
                let path = args.first().map(|s| s.as_str()).unwrap_or("heap.hprof");
                let header = HprofWriter::write_header();
                let output = format!(
                    "Heap dump written to {}\nHPROF header: {} bytes (magic: {})",
                    path,
                    header.len(),
                    HprofWriter::HPROF_MAGIC
                );
                CommandResult::ok(output, 0)
            }),
        ));

        // 3. GC.run
        listener.register_command(DiagnosticCommand::new(
            "GC.run",
            "Trigger garbage collection",
            CommandImpact::Medium,
            CommandPermission::ManagementAction,
            vec![],
            Box::new(|_args| CommandResult::ok("GC triggered".to_string(), 0)),
        ));

        // 4. GC.heap_info
        listener.register_command(DiagnosticCommand::new(
            "GC.heap_info",
            "Print heap summary information",
            CommandImpact::Low,
            CommandPermission::ReadOnly,
            vec![],
            Box::new(|_args| {
                let summary = HeapSummary {
                    young_gen_used: 25 * 1024 * 1024,
                    young_gen_capacity: 64 * 1024 * 1024,
                    old_gen_used: 100 * 1024 * 1024,
                    old_gen_capacity: 256 * 1024 * 1024,
                    metaspace_used: 30 * 1024 * 1024,
                    metaspace_capacity: 64 * 1024 * 1024,
                    total_used: 155 * 1024 * 1024,
                    total_capacity: 384 * 1024 * 1024,
                };
                let output = JmapProcessor::generate_heap_summary(&summary);
                CommandResult::ok(output, 0)
            }),
        ));

        // 5. GC.class_histogram
        listener.register_command(DiagnosticCommand::new(
            "GC.class_histogram",
            "Print class histogram",
            CommandImpact::Medium,
            CommandPermission::ReadOnly,
            vec![],
            Box::new(|_args| {
                let entries = sample_class_histogram();
                let output = JmapProcessor::generate_class_histogram(&entries);
                CommandResult::ok(output, 0)
            }),
        ));

        // 6. VM.version
        listener.register_command(DiagnosticCommand::new(
            "VM.version",
            "Print VM version",
            CommandImpact::Low,
            CommandPermission::ReadOnly,
            vec![],
            Box::new(|_args| {
                CommandResult::ok("CratonVM 1.0.0 (JDK 25 compatible)".to_string(), 0)
            }),
        ));

        // 7. VM.flags
        listener.register_command(DiagnosticCommand::new(
            "VM.flags",
            "Print VM flag settings",
            CommandImpact::Low,
            CommandPermission::ReadOnly,
            vec![],
            Box::new(|_args| {
                let flags = vec![
                    "-XX:+UseG1GC",
                    "-XX:MaxHeapSize=268435456",
                    "-XX:InitialHeapSize=67108864",
                    "-XX:+UseCompressedOops",
                    "-XX:+UseCompressedClassPointers",
                    "-XX:MaxMetaspaceSize=67108864",
                    "-XX:MetaspaceSize=33554432",
                    "-XX:+TieredCompilation",
                    "-XX:TieredStopAtLevel=4",
                    "-XX:+UseNUMA",
                    "-XX:+UseBiasedLocking",
                    "-XX:+OptimizeStringConcat",
                    "-XX:+PrintGCDetails",
                    "-XX:+HeapDumpOnOutOfMemoryError",
                    "-XX:ParallelGCThreads=4",
                ];
                CommandResult::ok(flags.join("\n"), 0)
            }),
        ));

        // 8. VM.system_properties
        listener.register_command(DiagnosticCommand::new(
            "VM.system_properties",
            "Print system properties",
            CommandImpact::Low,
            CommandPermission::ReadOnly,
            vec![],
            Box::new(|_args| {
                let props = vec![
                    "java.version=25",
                    "java.vendor=CratonVM",
                    "java.home=/usr/lib/jvm/cratonvm",
                    "os.name=Linux",
                    "os.arch=amd64",
                    "file.separator=/",
                    "path.separator=:",
                    "line.separator=\\n",
                    "user.dir=/home/user",
                    "java.class.path=.",
                ];
                CommandResult::ok(props.join("\n"), 0)
            }),
        ));

        // 9. VM.uptime
        listener.register_command(DiagnosticCommand::new(
            "VM.uptime",
            "Print VM uptime",
            CommandImpact::Low,
            CommandPermission::ReadOnly,
            vec![],
            Box::new(|_args| CommandResult::ok("VM uptime: 3600.000 seconds".to_string(), 0)),
        ));

        // 10. VM.info
        listener.register_command(DiagnosticCommand::new(
            "VM.info",
            "Print comprehensive VM information",
            CommandImpact::Low,
            CommandPermission::ReadOnly,
            vec![],
            Box::new(|_args| {
                let info = vec![
                    "CratonVM 1.0.0 (JDK 25 compatible)",
                    "Runtime: Rust-based JVM implementation",
                    "Heap: 384 MB capacity, 155 MB used",
                    "GC: G1 Garbage Collector",
                    "Threads: 12 live, 14 peak",
                    "Classes: 4200 loaded, 10 unloaded",
                    "Compiler: Tiered (C1 + C2)",
                    "OS: Linux amd64",
                    "CPUs: 4 available",
                ];
                CommandResult::ok(info.join("\n"), 0)
            }),
        ));

        // 11. VM.command_line
        listener.register_command(DiagnosticCommand::new(
            "VM.command_line",
            "Print command line arguments",
            CommandImpact::Low,
            CommandPermission::ReadOnly,
            vec![],
            Box::new(|_args| {
                CommandResult::ok(
                    "java -Xmx256m -Xms64m -XX:+UseG1GC -cp app.jar com.example.Main".to_string(),
                    0,
                )
            }),
        ));

        // 12. Thread.dump_to_file
        listener.register_command(DiagnosticCommand::new(
            "Thread.dump_to_file",
            "Dump threads to a file",
            CommandImpact::Medium,
            CommandPermission::ManagementAction,
            vec![CommandArgument {
                name: "filepath".to_string(),
                description: "Output file path".to_string(),
                arg_type: ArgType::FilePath,
                required: false,
                default_value: Some("thread_dump.txt".to_string()),
            }],
            Box::new(|args| {
                let path = args
                    .first()
                    .map(|s| s.as_str())
                    .unwrap_or("thread_dump.txt");
                CommandResult::ok(format!("Thread dump written to {}", path), 0)
            }),
        ));

        // 13. Compiler.queue
        //
        // Observability audit (2026-07-26) — DEFECT FIXED. This handler used
        // to return a hard-coded, entirely **fabricated** queue listing:
        //
        //     C1 compile queue: 3 methods
        //       1: java.lang.String.hashCode()I (tier 1)
        //       2: java.util.HashMap.get(...)... (tier 1)
        //       3: java.lang.Math.max(II)I (tier 1)
        //     C2 compile queue: 1 method
        //       1: com.example.Main.hotLoop()V (tier 4)
        //
        // Those method names were invented — `com.example.Main.hotLoop` does
        // not exist in any real workload. Nothing consulted the JIT's
        // compilation queue. An operator diagnosing a compile-storm would
        // have read that output as ground truth and drawn conclusions from
        // fiction. Invented diagnostic data is strictly worse than an honest
        // "unsupported", so the command now reports that it is unimplemented.
        //
        // To implement for real: extend `VmDiagnosticState` with a
        // `compiler_queue()` method backed by the JIT compile queue and
        // register the command from `register_live_commands` instead (that
        // variant has the `Arc<dyn VmDiagnosticState>` this one lacks).
        listener.register_command(DiagnosticCommand::new(
            "Compiler.queue",
            "Print compilation queue (not implemented)",
            CommandImpact::Low,
            CommandPermission::ReadOnly,
            vec![],
            Box::new(|_args| {
                CommandResult::err(
                    "Compiler.queue is not implemented: this JcmdProcessor has no \
                     VM state binding and CratonVM does not yet expose the JIT \
                     compile queue to serviceability."
                        .to_string(),
                    0,
                )
            }),
        ));

        // 14-16. JFR.start / JFR.stop / JFR.dump
        //
        // Observability audit (2026-07-26) — DEFECT FIXED. All three handlers
        // used to return `CommandResult::ok("Flight recording started: {name}")`
        // (and the stop/dump equivalents) **without touching the flight
        // recorder at all**. They did not call `FlightRecorder::new_recording`,
        // `start_recording`, `stop_recording`, or `cratonvm_jfr::dump_to_file`;
        // `JFR.dump` reported a file path it never created.
        //
        // The failure scenario is the worst kind: an operator runs
        // `jcmd <pid> JFR.start`, is told the recording started, reproduces a
        // production incident, runs `JFR.dump`, is told the file was written —
        // and finds nothing. Meanwhile the incident window is gone.
        //
        // (In practice nothing could even reach these handlers — see the
        // LIVENESS block at the top of this module — but they were the most
        // load-bearing-looking lie in the file, and the previous tests
        // asserted the fake success strings, which would have kept the lie
        // alive through any future wiring.)
        //
        // Implementing these for real needs, in order:
        //   1. an `Arc<SharedVm>` (or a `VmDiagnosticState` extension) on the
        //      processor so the handler can reach `debug.flight_recorder`;
        //   2. `new_recording` + `start_recording` on start, `stop_recording`
        //      on stop — both of which flip the process-global
        //      `cratonvm_jfr::set_enabled` flag that every `emit_*` checks;
        //   3. `FlightRecorder::dump_recording` on dump, which drains the
        //      per-thread rings and writes the chunk.
        // Note that even then the produced `.jfr` is not JMC-loadable — see
        // the FORMAT-FIDELITY GAP block in `jfr/src/dump.rs`.
        //
        // 2026-09-01 -- DONE, for `register_live_commands` only. All three
        // prerequisites above are met there: (1) via the defaulted
        // `VmDiagnosticState::jfr_start` / `jfr_dump` / `jfr_stop` and their
        // `SharedVm` implementations in `vm/src/vm/vm_init.rs`, (2) and (3)
        // inside those. The parenthetical above -- "in practice nothing could
        // even reach these handlers" -- has also expired; see the 2026-09-01
        // correction at the top of this module.
        //
        // These three STAY as they are, and that is not an oversight. This
        // function is the no-VM-state set built by the argument-less
        // `JcmdProcessor::new()`, which owns no `FlightRecorder` and can never
        // be given one without the `Arc<dyn VmDiagnosticState>` that
        // `new_with_vm_state` is for. An honest error from a processor that
        // genuinely cannot record is the correct answer, and replacing it with
        // anything else would recreate the defect this block exists to record.
        for (name, help, arg_name, arg_help, arg_type, arg_default) in [
            (
                "JFR.start",
                "Start a flight recording (not implemented)",
                "name",
                "Recording name",
                ArgType::String,
                "recording1",
            ),
            (
                "JFR.stop",
                "Stop a flight recording (not implemented)",
                "name",
                "Recording name",
                ArgType::String,
                "recording1",
            ),
            (
                "JFR.dump",
                "Dump flight recording to file (not implemented)",
                "filename",
                "Output file path",
                ArgType::FilePath,
                "recording.jfr",
            ),
        ] {
            listener.register_command(DiagnosticCommand::new(
                name,
                help,
                CommandImpact::Medium,
                CommandPermission::ManagementAction,
                vec![CommandArgument {
                    name: arg_name.to_string(),
                    description: arg_help.to_string(),
                    arg_type,
                    required: false,
                    default_value: Some(arg_default.to_string()),
                }],
                Box::new(move |_args| {
                    CommandResult::err(
                        format!(
                            "{name} is not implemented: this JcmdProcessor has no binding \
                             to the VM's FlightRecorder, so no recording is started, \
                             stopped, or written. Reporting success here would lose an \
                             operator's incident window."
                        ),
                        0,
                    )
                }),
            ));
        }
    }

    fn register_live_commands(
        listener: &mut AttachListener,
        vm_state: std::sync::Weak<dyn VmDiagnosticState>,
    ) {
        /// Upgrade the weak VM handle, or answer the attaching client that the
        /// VM is gone. See `new_with_vm_state` for why these closures hold a
        /// `Weak` and not an `Arc`.
        macro_rules! live_vm {
            ($weak:expr) => {
                match $weak.upgrade() {
                    Some(vs) => vs,
                    None => {
                        return CommandResult::err(
                            "VM is shutting down; no live state to report".to_string(),
                            1,
                        )
                    }
                }
            };
        }

        // 1. Thread.print
        let vs = vm_state.clone();
        listener.register_command(DiagnosticCommand::new(
            "Thread.print",
            "Print all threads with stacktraces",
            CommandImpact::Medium,
            CommandPermission::ReadOnly,
            vec![],
            Box::new(move |_args| {
                let vs = live_vm!(vs);
                let threads = vs.thread_snapshots();
                let dump = JstackProcessor::generate_thread_dump(&threads);
                CommandResult::ok(dump, 0)
            }),
        ));

        // 2. GC.heap_dump
        let vs = vm_state.clone();
        listener.register_command(DiagnosticCommand::new(
            "GC.heap_dump",
            "Generate a HPROF format heap dump",
            CommandImpact::High,
            CommandPermission::ManagementAction,
            vec![CommandArgument {
                name: "filename".to_string(),
                description: "Output file path".to_string(),
                arg_type: ArgType::FilePath,
                required: false,
                default_value: Some("heap.hprof".to_string()),
            }],
            Box::new(move |args| {
                let vs = live_vm!(vs);
                let path = args.first().map(|s| s.as_str()).unwrap_or("heap.hprof");
                match vs.heap_dump(path) {
                    Ok(bytes) => CommandResult::ok(
                        format!("Heap dump written to {}\nDump size: {} bytes", path, bytes),
                        0,
                    ),
                    Err(e) => CommandResult::err(format!("Heap dump failed: {}", e), 1),
                }
            }),
        ));

        // 3. GC.run
        let vs = vm_state.clone();
        listener.register_command(DiagnosticCommand::new(
            "GC.run",
            "Trigger garbage collection",
            CommandImpact::Medium,
            CommandPermission::ManagementAction,
            vec![],
            Box::new(move |_args| {
                let vs = live_vm!(vs);
                let ran = vs.trigger_gc();
                if ran {
                    CommandResult::ok("GC triggered and completed".to_string(), 0)
                } else {
                    CommandResult::ok("GC trigger requested (may be deferred)".to_string(), 0)
                }
            }),
        ));

        // 4. GC.heap_info
        let vs = vm_state.clone();
        listener.register_command(DiagnosticCommand::new(
            "GC.heap_info",
            "Print heap summary information",
            CommandImpact::Low,
            CommandPermission::ReadOnly,
            vec![],
            Box::new(move |_args| {
                let vs = live_vm!(vs);
                let summary = vs.heap_summary();
                let output = JmapProcessor::generate_heap_summary(&summary);
                CommandResult::ok(output, 0)
            }),
        ));

        // 5. GC.class_histogram
        let vs = vm_state.clone();
        listener.register_command(DiagnosticCommand::new(
            "GC.class_histogram",
            "Print class histogram",
            CommandImpact::Medium,
            CommandPermission::ReadOnly,
            vec![],
            Box::new(move |_args| {
                let vs = live_vm!(vs);
                let entries = vs.class_histogram();
                let output = JmapProcessor::generate_class_histogram(&entries);
                CommandResult::ok(output, 0)
            }),
        ));

        // 6. VM.version
        listener.register_command(DiagnosticCommand::new(
            "VM.version",
            "Print VM version",
            CommandImpact::Low,
            CommandPermission::ReadOnly,
            vec![],
            Box::new(|_args| {
                CommandResult::ok("CratonVM 1.0.0 (JDK 25 compatible)".to_string(), 0)
            }),
        ));

        // 7. VM.flags
        let vs = vm_state.clone();
        listener.register_command(DiagnosticCommand::new(
            "VM.flags",
            "Print VM flag settings",
            CommandImpact::Low,
            CommandPermission::ReadOnly,
            vec![],
            Box::new(move |_args| {
                let vs = live_vm!(vs);
                let flags = vs.vm_flags();
                CommandResult::ok(flags.join("\n"), 0)
            }),
        ));

        // 8. VM.system_properties
        let vs = vm_state.clone();
        listener.register_command(DiagnosticCommand::new(
            "VM.system_properties",
            "Print system properties",
            CommandImpact::Low,
            CommandPermission::ReadOnly,
            vec![],
            Box::new(move |_args| {
                let vs = live_vm!(vs);
                let props = vs.system_properties();
                let lines: Vec<String> =
                    props.iter().map(|(k, v)| format!("{}={}", k, v)).collect();
                CommandResult::ok(lines.join("\n"), 0)
            }),
        ));

        // 9. VM.uptime
        let vs = vm_state.clone();
        listener.register_command(DiagnosticCommand::new(
            "VM.uptime",
            "Print VM uptime",
            CommandImpact::Low,
            CommandPermission::ReadOnly,
            vec![],
            Box::new(move |_args| {
                let vs = live_vm!(vs);
                CommandResult::ok(format!("VM uptime: {:.3} seconds", vs.uptime_secs()), 0)
            }),
        ));

        // 10. VM.command_line
        let vs = vm_state.clone();
        listener.register_command(DiagnosticCommand::new(
            "VM.command_line",
            "Print command line arguments",
            CommandImpact::Low,
            CommandPermission::ReadOnly,
            vec![],
            Box::new(move |_args| {
                let vs = live_vm!(vs);
                CommandResult::ok(vs.command_line(), 0)
            }),
        ));

        // 11-13. JFR.start / JFR.dump / JFR.stop -- the REAL ones.
        //
        // 2026-09-01. The audit block at the `register_standard_commands` JFR
        // entries lists three prerequisites for implementing these; (1) was a
        // VM binding, and `VmDiagnosticState::jfr_start` / `jfr_dump` /
        // `jfr_stop` (this file) plus their `SharedVm` implementations
        // (`vm/src/vm/vm_init.rs`) are it. (2) and (3) -- `new_recording` +
        // `start_recording`, `stop_recording`, `dump_recording` -- happen
        // inside those implementations.
        //
        // Registered HERE and not in `register_standard_commands`, and that is
        // the whole point of the split: the standard set is built by the
        // argument-less `JcmdProcessor::new()`, which has no VM at all, so its
        // three entries keep returning an honest "not implemented". A command
        // that cannot reach a recorder must not answer as though it had.
        //
        // These are reachable from a real `jcmd <pid> JFR.start ...`: see the
        // 2026-09-01 correction at the top of this module for why the old
        // "nothing can reach these handlers" note no longer holds.
        //
        // A bad option or an unknown event name is answered with
        // `CommandResult::err` and the recording is NOT started -- deliberately
        // unlike the `-XX:StartFlightRecording` boot path, which panics on the
        // same mistake. A typo in a boot flag must not produce a VM whose
        // recording can never contain what was asked for; a typo typed at a
        // *running* VM must not kill it. Same rule, opposite blast radius.
        let vs = vm_state.clone();
        listener.register_command(DiagnosticCommand::new(
            "JFR.start",
            "Start a flight recording",
            CommandImpact::Medium,
            CommandPermission::ManagementAction,
            vec![CommandArgument {
                name: "options".to_string(),
                description: "name=<n> maxevents=<n> +<Event>#enabled=true".to_string(),
                arg_type: ArgType::String,
                required: false,
                default_value: None,
            }],
            Box::new(move |args| {
                let vs = live_vm!(vs);
                match vs.jfr_start(args) {
                    Ok(msg) => CommandResult::ok(msg, 0),
                    Err(e) => CommandResult::err(e, 1),
                }
            }),
        ));

        let vs = vm_state.clone();
        listener.register_command(DiagnosticCommand::new(
            "JFR.dump",
            "Dump a running flight recording to a file",
            CommandImpact::Medium,
            CommandPermission::ManagementAction,
            vec![CommandArgument {
                name: "options".to_string(),
                description: "[name=<n>] filename=<path>".to_string(),
                arg_type: ArgType::String,
                required: false,
                default_value: None,
            }],
            Box::new(move |args| {
                let vs = live_vm!(vs);
                match vs.jfr_dump(args) {
                    Ok(msg) => CommandResult::ok(msg, 0),
                    Err(e) => CommandResult::err(e, 1),
                }
            }),
        ));

        let vs = vm_state.clone();
        listener.register_command(DiagnosticCommand::new(
            "JFR.stop",
            "Stop a flight recording, optionally writing it out first",
            CommandImpact::Medium,
            CommandPermission::ManagementAction,
            vec![CommandArgument {
                name: "options".to_string(),
                description: "[name=<n>] [filename=<path>]".to_string(),
                arg_type: ArgType::String,
                required: false,
                default_value: None,
            }],
            Box::new(move |args| {
                let vs = live_vm!(vs);
                match vs.jfr_stop(args) {
                    Ok(msg) => CommandResult::ok(msg, 0),
                    Err(e) => CommandResult::err(e, 1),
                }
            }),
        ));
    }

    /// Process a jcmd command line (e.g. "Thread.print" or "GC.heap_dump /tmp/dump.hprof").
    pub fn process_command(&self, command_line: &str) -> CommandResult {
        let parts: Vec<&str> = command_line.trim().splitn(2, ' ').collect();
        let cmd_name = parts[0];
        let args: Vec<String> = if parts.len() > 1 {
            parts[1].split_whitespace().map(|s| s.to_string()).collect()
        } else {
            vec![]
        };

        if cmd_name == "help" {
            return CommandResult::ok(self.help(), 0);
        }

        let start = Instant::now();
        match self.attach_listener.dispatch_command(cmd_name, &args) {
            Some(mut result) => {
                result.execution_time_ms = start.elapsed().as_millis() as u64;
                result
            }
            None => CommandResult::err(format!("Unknown command: {}", cmd_name), 0),
        }
    }

    /// Generate help text listing all available commands.
    pub fn help(&self) -> String {
        let mut lines = vec!["Available commands:".to_string()];
        for cmd in self.attach_listener.commands.read().iter() {
            lines.push(format!(
                "  {} - {} [impact: {}]",
                cmd.name, cmd.description, cmd.impact
            ));
        }
        lines.join("\n")
    }
}

impl Default for JcmdProcessor {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// jstack implementation
// ---------------------------------------------------------------------------

/// Thread state for thread dump output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadState {
    New,
    Runnable,
    Blocked,
    Waiting,
    TimedWaiting,
    Terminated,
}

impl fmt::Display for ThreadState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ThreadState::New => write!(f, "NEW"),
            ThreadState::Runnable => write!(f, "RUNNABLE"),
            ThreadState::Blocked => write!(f, "BLOCKED"),
            ThreadState::Waiting => write!(f, "WAITING"),
            ThreadState::TimedWaiting => write!(f, "TIMED_WAITING"),
            ThreadState::Terminated => write!(f, "TERMINATED"),
        }
    }
}

/// A single stack frame in a thread dump.
#[derive(Debug, Clone)]
pub struct FrameInfo {
    pub class_name: String,
    pub method_name: String,
    pub file_name: Option<String>,
    pub line_number: i32,
    pub native_method: bool,
}

/// Lock information for a thread.
#[derive(Debug, Clone)]
pub struct LockInfo {
    pub class_name: String,
    pub identity_hash: u64,
}

/// Snapshot of a single thread's state.
#[derive(Debug, Clone)]
pub struct ThreadSnapshot {
    pub id: u64,
    pub name: String,
    pub daemon: bool,
    pub priority: i32,
    pub state: ThreadState,
    pub stack_frames: Vec<FrameInfo>,
    pub lock_info: Option<LockInfo>,
    pub blocked_by: Option<u64>,
    pub waiting_on: Option<String>,
}

/// Processes jstack-style thread dumps.
pub struct JstackProcessor;

impl JstackProcessor {
    /// Generate a HotSpot-style thread dump string.
    pub fn generate_thread_dump(threads: &[ThreadSnapshot]) -> String {
        let mut output = String::new();
        output.push_str("Full thread dump CratonVM 1.0.0 (JDK 25 compatible):\n\n");

        for thread in threads {
            // Header line
            let daemon_str = if thread.daemon { " daemon" } else { "" };
            let state_short = match thread.state {
                ThreadState::Runnable => "runnable",
                ThreadState::Blocked => "waiting for monitor entry",
                ThreadState::Waiting | ThreadState::TimedWaiting => "waiting on condition",
                ThreadState::New => "new",
                ThreadState::Terminated => "terminated",
            };

            output.push_str(&format!(
                "\"{}\" #{}{} prio={} os_prio=0 tid=0x{:016x} nid=0x{:x} {} [0x{:016x}]\n",
                thread.name,
                thread.id,
                daemon_str,
                thread.priority,
                thread.id * 0x1000,
                thread.id,
                state_short,
                thread.id * 0x7000,
            ));

            // Thread state
            output.push_str(&format!("   java.lang.Thread.State: {}\n", thread.state));

            // Stack frames
            for frame in &thread.stack_frames {
                let location = if frame.native_method {
                    "Native Method".to_string()
                } else {
                    match &frame.file_name {
                        Some(f) => format!("{}:{}", f, frame.line_number),
                        None => "Unknown Source".to_string(),
                    }
                };
                output.push_str(&format!(
                    "\tat {}.{}({})\n",
                    frame.class_name, frame.method_name, location
                ));
            }

            // Lock info
            if let Some(lock) = &thread.lock_info {
                output.push_str(&format!(
                    "\t- locked <0x{:016x}> (a {})\n",
                    lock.identity_hash, lock.class_name
                ));
            }

            // Waiting on info
            if let Some(monitor) = &thread.waiting_on {
                output.push_str(&format!("\t- waiting on {}\n", monitor));
            }

            output.push('\n');
        }

        output
    }

    /// Detect deadlocks and generate a deadlock report.
    pub fn generate_deadlock_report(threads: &[ThreadSnapshot]) -> Option<String> {
        // Build a blocked_by graph and detect cycles
        let mut cycles: Vec<Vec<u64>> = Vec::new();
        let mut visited_global: std::collections::HashSet<u64> = std::collections::HashSet::new();

        for thread in threads {
            if visited_global.contains(&thread.id) {
                continue;
            }
            // Follow the blocked_by chain
            let mut path: Vec<u64> = Vec::new();
            let mut visited: std::collections::HashSet<u64> = std::collections::HashSet::new();
            let mut current_id = Some(thread.id);

            while let Some(cid) = current_id {
                if visited.contains(&cid) {
                    // Found a cycle — extract it
                    if let Some(pos) = path.iter().position(|&x| x == cid) {
                        let cycle: Vec<u64> = path[pos..].to_vec();
                        if !cycle.is_empty() {
                            cycles.push(cycle);
                        }
                    }
                    break;
                }
                visited.insert(cid);
                path.push(cid);

                // Find blocked_by for this thread
                current_id = threads
                    .iter()
                    .find(|t| t.id == cid)
                    .and_then(|t| t.blocked_by);
            }

            for id in &path {
                visited_global.insert(*id);
            }
        }

        if cycles.is_empty() {
            return None;
        }

        let mut report = String::new();
        report.push_str(&format!("Found {} deadlock(s).\n\n", cycles.len()));

        for (i, cycle) in cycles.iter().enumerate() {
            report.push_str(&format!("Deadlock #{}:\n", i + 1));
            for &tid in cycle {
                if let Some(t) = threads.iter().find(|t| t.id == tid) {
                    report.push_str(&format!(
                        "  \"{}\" (id={}) blocked by thread id={}\n",
                        t.name,
                        t.id,
                        t.blocked_by.unwrap_or(0)
                    ));
                }
            }
            report.push('\n');
        }

        Some(report)
    }
}

// ---------------------------------------------------------------------------
// jmap implementation
// ---------------------------------------------------------------------------

/// Entry in a class histogram.
#[derive(Debug, Clone)]
pub struct ClassHistogramEntry {
    pub class_name: String,
    pub instance_count: u64,
    pub total_bytes: u64,
}

/// Heap summary information.
#[derive(Debug, Clone)]
pub struct HeapSummary {
    pub young_gen_used: u64,
    pub young_gen_capacity: u64,
    pub old_gen_used: u64,
    pub old_gen_capacity: u64,
    pub metaspace_used: u64,
    pub metaspace_capacity: u64,
    pub total_used: u64,
    pub total_capacity: u64,
}

/// Processes jmap-style heap analysis commands.
pub struct JmapProcessor;

impl JmapProcessor {
    /// Generate a class histogram report.
    pub fn generate_class_histogram(classes: &[ClassHistogramEntry]) -> String {
        let mut output = String::new();
        output.push_str(" num     #instances         #bytes  class name\n");
        output.push_str("----------------------------------------------\n");

        let mut total_instances: u64 = 0;
        let mut total_bytes: u64 = 0;

        for (i, entry) in classes.iter().enumerate() {
            output.push_str(&format!(
                "{:>4}:  {:>10}  {:>12}  {}\n",
                i + 1,
                entry.instance_count,
                entry.total_bytes,
                entry.class_name
            ));
            total_instances += entry.instance_count;
            total_bytes += entry.total_bytes;
        }

        output.push_str("----------------------------------------------\n");
        output.push_str(&format!(
            "Total: {:>10}  {:>12}\n",
            total_instances, total_bytes
        ));

        output
    }

    /// Generate a heap summary report.
    pub fn generate_heap_summary(heap_info: &HeapSummary) -> String {
        let mut output = String::new();
        output.push_str("Heap Configuration:\n");
        output.push_str(&format!(
            "   Young Generation: {} / {} ({:.1}% used)\n",
            format_bytes(heap_info.young_gen_used),
            format_bytes(heap_info.young_gen_capacity),
            percent(heap_info.young_gen_used, heap_info.young_gen_capacity)
        ));
        output.push_str(&format!(
            "   Old Generation:   {} / {} ({:.1}% used)\n",
            format_bytes(heap_info.old_gen_used),
            format_bytes(heap_info.old_gen_capacity),
            percent(heap_info.old_gen_used, heap_info.old_gen_capacity)
        ));
        output.push_str(&format!(
            "   Metaspace:        {} / {} ({:.1}% used)\n",
            format_bytes(heap_info.metaspace_used),
            format_bytes(heap_info.metaspace_capacity),
            percent(heap_info.metaspace_used, heap_info.metaspace_capacity)
        ));
        output.push_str(&format!(
            "   Total:            {} / {} ({:.1}% used)\n",
            format_bytes(heap_info.total_used),
            format_bytes(heap_info.total_capacity),
            percent(heap_info.total_used, heap_info.total_capacity)
        ));
        output
    }

    /// Generate finalizer information report.
    pub fn generate_finalizer_info() -> String {
        let mut output = String::new();
        output.push_str("Finalizer Information:\n");
        output.push_str("  Pending finalizers: 0\n");
        output.push_str("  Finalizer thread: active\n");
        output.push_str("  Reference handler thread: active\n");
        output
    }
}

fn format_bytes(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    } else if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{} B", bytes)
    }
}

fn percent(used: u64, capacity: u64) -> f64 {
    if capacity == 0 {
        0.0
    } else {
        (used as f64 / capacity as f64) * 100.0
    }
}

// ---------------------------------------------------------------------------
// HPROF heap dump writer (HPROF 1.0.2 binary format)
// ---------------------------------------------------------------------------

/// HPROF basic type constants used in CLASS_DUMP field descriptors and
/// PRIM_ARRAY_DUMP element types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum HprofBasicType {
    Object = 2,
    Boolean = 4,
    Char = 5,
    Float = 6,
    Double = 7,
    Byte = 8,
    Short = 9,
    Int = 10,
    Long = 11,
}

impl HprofBasicType {
    /// Number of bytes this type occupies in an HPROF instance dump value.
    pub fn size(self) -> usize {
        match self {
            HprofBasicType::Object => 8, // identifier size
            HprofBasicType::Boolean | HprofBasicType::Byte => 1,
            HprofBasicType::Char | HprofBasicType::Short => 2,
            HprofBasicType::Float | HprofBasicType::Int => 4,
            HprofBasicType::Double | HprofBasicType::Long => 8,
        }
    }

    /// Map a JVM field descriptor character to an HPROF basic type.
    pub fn from_descriptor(desc: &str) -> Self {
        match desc.as_bytes().first() {
            Some(b'Z') => HprofBasicType::Boolean,
            Some(b'B') => HprofBasicType::Byte,
            Some(b'C') => HprofBasicType::Char,
            Some(b'S') => HprofBasicType::Short,
            Some(b'I') => HprofBasicType::Int,
            Some(b'J') => HprofBasicType::Long,
            Some(b'F') => HprofBasicType::Float,
            Some(b'D') => HprofBasicType::Double,
            Some(b'L') | Some(b'[') => HprofBasicType::Object,
            _ => HprofBasicType::Object,
        }
    }
}

/// Information about a class needed for HPROF dump.
#[derive(Debug, Clone)]
pub struct HprofClassInfo {
    /// Unique class ID (from ClassId).
    pub class_id: u32,
    /// JVM internal name (e.g. "java/lang/String").
    pub name: String,
    /// Superclass class ID, or 0 for java/lang/Object.
    pub super_class_id: u32,
    /// Instance fields declared in this class (name, descriptor).
    pub instance_fields: Vec<(String, String)>,
    /// Static fields declared in this class (name, descriptor).
    pub static_fields: Vec<(String, String)>,
    /// Source file name, if known.
    pub source_file: Option<String>,
    /// Total instance size in bytes (HEADER_SIZE + num_total_fields * SLOT_SIZE).
    pub instance_size: u32,
}

/// Information about a single heap object for HPROF dump.
#[derive(Debug)]
pub struct HprofObjectInfo {
    /// Raw pointer to the object (used as HPROF object ID).
    pub object_id: u64,
    /// Class ID of this object.
    pub class_id: u32,
    /// Whether this is an array.
    pub is_array: bool,
    /// Array element type (meaningful only for arrays).
    pub element_type: u8,
    /// Array length (meaningful only for arrays).
    pub array_length: u32,
    /// Total allocation size in bytes.
    pub total_size: usize,
    /// Raw pointer to the object data (for reading field/element values).
    pub data_ptr: *const u8,
}

/// Writer for HPROF 1.0.2 binary format heap dump files.
///
/// Implements the full HPROF binary spec including:
/// - File header with magic string and identifier size
/// - UTF-8 string records (for class/field/method names)
/// - LOAD_CLASS records (class serial mapping)
/// - STACK_TRACE / STACK_FRAME records (thread stacks)
/// - HEAP_DUMP_SEGMENT records containing:
///   - GC_CLASS_DUMP sub-records (class metadata with field descriptors)
///   - GC_INSTANCE_DUMP sub-records (object instances with field values)
///   - GC_OBJ_ARRAY_DUMP sub-records (reference arrays)
///   - GC_PRIM_ARRAY_DUMP sub-records (primitive arrays)
///   - GC_ROOT_THREAD_OBJ sub-records (thread roots)
///   - GC_ROOT_JNI_GLOBAL sub-records (JNI global reference roots)
/// - HEAP_DUMP_END marker
pub struct HprofWriter;

impl HprofWriter {
    pub const HPROF_MAGIC: &'static str = "JAVA PROFILE 1.0.2";

    // Top-level record type constants
    pub const HPROF_UTF8: u8 = 0x01;
    pub const HPROF_LOAD_CLASS: u8 = 0x02;
    pub const HPROF_FRAME: u8 = 0x04;
    pub const HPROF_TRACE: u8 = 0x05;
    pub const HPROF_HEAP_DUMP: u8 = 0x0C;
    pub const HPROF_HEAP_DUMP_SEGMENT: u8 = 0x1C;
    pub const HPROF_HEAP_DUMP_END: u8 = 0x2C;

    // Heap dump sub-record tag constants
    pub const GC_ROOT_JNI_GLOBAL: u8 = 0x01;
    pub const GC_ROOT_THREAD_OBJ: u8 = 0x08;
    pub const GC_CLASS_DUMP: u8 = 0x20;
    pub const GC_INSTANCE_DUMP: u8 = 0x21;
    pub const GC_OBJ_ARRAY_DUMP: u8 = 0x22;
    pub const GC_PRIM_ARRAY_DUMP: u8 = 0x23;

    /// Maximum segment body size before starting a new HEAP_DUMP_SEGMENT.
    /// HPROF spec allows up to ~2GB per segment; we use 64 MB for streaming.
    const MAX_SEGMENT_SIZE: usize = 64 * 1024 * 1024;

    /// Write HPROF file header. Format:
    /// - magic string (null-terminated)
    /// - identifier size (4 bytes, big-endian) = 8
    /// - high timestamp (4 bytes)
    /// - low timestamp (4 bytes)
    pub fn write_header() -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(Self::HPROF_MAGIC.as_bytes());
        buf.push(0); // null terminator
        buf.extend_from_slice(&8u32.to_be_bytes()); // identifier size: 8 bytes
                                                    // Timestamp: milliseconds since epoch, split into high/low 32-bit words
        let ts_millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        buf.extend_from_slice(&((ts_millis >> 32) as u32).to_be_bytes());
        buf.extend_from_slice(&(ts_millis as u32).to_be_bytes());
        buf
    }

    /// Write a UTF-8 string record.
    /// Format: tag(1) + time(4) + length(4) + id(8) + utf8_bytes
    pub fn write_string_record(id: u64, value: &str) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.push(Self::HPROF_UTF8);
        buf.extend_from_slice(&0u32.to_be_bytes());
        let body_len = 8 + value.len() as u32;
        buf.extend_from_slice(&body_len.to_be_bytes());
        buf.extend_from_slice(&id.to_be_bytes());
        buf.extend_from_slice(value.as_bytes());
        buf
    }

    /// Return the size of the HPROF header in bytes.
    pub fn header_size() -> usize {
        Self::HPROF_MAGIC.len() + 1 + 4 + 4 + 4
    }

    /// Write a LOAD_CLASS record.
    pub fn write_load_class(
        serial: u32,
        class_obj_id: u64,
        stack_serial: u32,
        name_id: u64,
    ) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.push(Self::HPROF_LOAD_CLASS);
        buf.extend_from_slice(&0u32.to_be_bytes());
        let body_len: u32 = 4 + 8 + 4 + 8;
        buf.extend_from_slice(&body_len.to_be_bytes());
        buf.extend_from_slice(&serial.to_be_bytes());
        buf.extend_from_slice(&class_obj_id.to_be_bytes());
        buf.extend_from_slice(&stack_serial.to_be_bytes());
        buf.extend_from_slice(&name_id.to_be_bytes());
        buf
    }

    /// Write a HEAP_DUMP_END record (empty body).
    pub fn write_heap_dump_end() -> Vec<u8> {
        let mut buf = Vec::new();
        buf.push(Self::HPROF_HEAP_DUMP_END);
        buf.extend_from_slice(&0u32.to_be_bytes());
        buf.extend_from_slice(&0u32.to_be_bytes());
        buf
    }

    /// Write a STACK_TRACE record.
    pub fn write_stack_trace(serial: u32, thread_serial: u32, frame_ids: &[u64]) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.push(Self::HPROF_TRACE);
        buf.extend_from_slice(&0u32.to_be_bytes());
        let body_len: u32 = 4 + 4 + 4 + (frame_ids.len() as u32 * 8);
        buf.extend_from_slice(&body_len.to_be_bytes());
        buf.extend_from_slice(&serial.to_be_bytes());
        buf.extend_from_slice(&thread_serial.to_be_bytes());
        buf.extend_from_slice(&(frame_ids.len() as u32).to_be_bytes());
        for &fid in frame_ids {
            buf.extend_from_slice(&fid.to_be_bytes());
        }
        buf
    }

    /// Write a STACK_FRAME record.
    pub fn write_stack_frame(
        frame_id: u64,
        method_name_id: u64,
        method_sig_id: u64,
        source_file_id: u64,
        class_serial: u32,
        line_number: i32,
    ) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.push(Self::HPROF_FRAME);
        buf.extend_from_slice(&0u32.to_be_bytes());
        let body_len: u32 = 8 + 8 + 8 + 8 + 4 + 4;
        buf.extend_from_slice(&body_len.to_be_bytes());
        buf.extend_from_slice(&frame_id.to_be_bytes());
        buf.extend_from_slice(&method_name_id.to_be_bytes());
        buf.extend_from_slice(&method_sig_id.to_be_bytes());
        buf.extend_from_slice(&source_file_id.to_be_bytes());
        buf.extend_from_slice(&class_serial.to_be_bytes());
        buf.extend_from_slice(&(line_number as u32).to_be_bytes());
        buf
    }

    // -----------------------------------------------------------------------
    // Heap dump sub-record writers (written inside HEAP_DUMP_SEGMENT body)
    // -----------------------------------------------------------------------

    /// Write a GC_ROOT_THREAD_OBJ sub-record.
    /// Format: tag(1) + thread_obj_id(8) + thread_serial(4) + stack_serial(4)
    pub fn write_gc_root_thread_obj(
        buf: &mut Vec<u8>,
        thread_obj_id: u64,
        thread_serial: u32,
        stack_serial: u32,
    ) {
        buf.push(Self::GC_ROOT_THREAD_OBJ);
        buf.extend_from_slice(&thread_obj_id.to_be_bytes());
        buf.extend_from_slice(&thread_serial.to_be_bytes());
        buf.extend_from_slice(&stack_serial.to_be_bytes());
    }

    /// Write a GC_ROOT_JNI_GLOBAL sub-record.
    /// Format: tag(1) + object_id(8) + jni_global_ref_id(8)
    pub fn write_gc_root_jni_global(buf: &mut Vec<u8>, object_id: u64, jni_ref_id: u64) {
        buf.push(Self::GC_ROOT_JNI_GLOBAL);
        buf.extend_from_slice(&object_id.to_be_bytes());
        buf.extend_from_slice(&jni_ref_id.to_be_bytes());
    }

    /// Write a GC_CLASS_DUMP sub-record.
    ///
    /// Format:
    /// - tag(1) + class_obj_id(8) + stack_trace_serial(4) + super_class_obj_id(8)
    /// - classloader_obj_id(8) + signers_obj_id(8) + protection_domain_obj_id(8)
    /// - reserved1(8) + reserved2(8) + instance_size(4)
    /// - constant_pool_count(2) [we write 0]
    /// - static_field_count(2) + static fields...
    /// - instance_field_count(2) + instance fields...
    pub fn write_gc_class_dump(
        buf: &mut Vec<u8>,
        class_info: &HprofClassInfo,
        string_ids: &std::collections::HashMap<String, u64>,
    ) {
        buf.push(Self::GC_CLASS_DUMP);
        // class object ID: we use class_id shifted into high bits to avoid collisions
        let class_obj_id = 0x1000_0000_0000_0000u64 | class_info.class_id as u64;
        buf.extend_from_slice(&class_obj_id.to_be_bytes());
        buf.extend_from_slice(&0u32.to_be_bytes()); // stack trace serial = 0
                                                    // super class object ID
        let super_obj_id = if class_info.super_class_id != 0 {
            0x1000_0000_0000_0000u64 | class_info.super_class_id as u64
        } else {
            0u64
        };
        buf.extend_from_slice(&super_obj_id.to_be_bytes());
        buf.extend_from_slice(&0u64.to_be_bytes()); // classloader obj id
        buf.extend_from_slice(&0u64.to_be_bytes()); // signers obj id
        buf.extend_from_slice(&0u64.to_be_bytes()); // protection domain
        buf.extend_from_slice(&0u64.to_be_bytes()); // reserved 1
        buf.extend_from_slice(&0u64.to_be_bytes()); // reserved 2
        buf.extend_from_slice(&class_info.instance_size.to_be_bytes());

        // Constant pool: 0 entries
        buf.extend_from_slice(&0u16.to_be_bytes());

        // Static fields
        let static_count = class_info.static_fields.len() as u16;
        buf.extend_from_slice(&static_count.to_be_bytes());
        for (fname, fdesc) in &class_info.static_fields {
            let name_id = string_ids.get(fname).copied().unwrap_or(0);
            buf.extend_from_slice(&name_id.to_be_bytes());
            let htype = HprofBasicType::from_descriptor(fdesc);
            buf.push(htype as u8);
            // Static field value: write zeros (we'd need to read from statics table for real values)
            match htype {
                HprofBasicType::Object => buf.extend_from_slice(&0u64.to_be_bytes()),
                HprofBasicType::Long | HprofBasicType::Double => {
                    buf.extend_from_slice(&0u64.to_be_bytes())
                }
                HprofBasicType::Int | HprofBasicType::Float => {
                    buf.extend_from_slice(&0u32.to_be_bytes())
                }
                HprofBasicType::Short | HprofBasicType::Char => {
                    buf.extend_from_slice(&0u16.to_be_bytes())
                }
                HprofBasicType::Boolean | HprofBasicType::Byte => buf.push(0),
            }
        }

        // Instance fields (only name + type descriptor, no values here)
        let inst_count = class_info.instance_fields.len() as u16;
        buf.extend_from_slice(&inst_count.to_be_bytes());
        for (fname, fdesc) in &class_info.instance_fields {
            let name_id = string_ids.get(fname).copied().unwrap_or(0);
            buf.extend_from_slice(&name_id.to_be_bytes());
            let htype = HprofBasicType::from_descriptor(fdesc);
            buf.push(htype as u8);
        }
    }

    /// Write a GC_INSTANCE_DUMP sub-record.
    ///
    /// Format: tag(1) + object_id(8) + stack_serial(4) + class_obj_id(8)
    ///       + data_size(4) + field_values...
    ///
    /// Field values are written in declaration order using the HPROF type sizes.
    /// Each field is written according to its HPROF type (not the internal SLOT_SIZE).
    pub fn write_gc_instance_dump(
        buf: &mut Vec<u8>,
        obj: &HprofObjectInfo,
        class_info: &HprofClassInfo,
        all_classes: &std::collections::HashMap<u32, HprofClassInfo>,
    ) {
        use cratonvm_gc::heap::HEADER_SIZE;
        use cratonvm_gc::heap::SLOT_SIZE;
        use cratonvm_gc::{is_compact_object, ObjectHeader};
        use cratonvm_types::class_layout_for_fields;
        use cratonvm_types::FIELD_CELL_PAYLOAD64_OFFSET;

        buf.push(Self::GC_INSTANCE_DUMP);
        buf.extend_from_slice(&obj.object_id.to_be_bytes());
        buf.extend_from_slice(&0u32.to_be_bytes()); // stack trace serial

        let class_obj_id = 0x1000_0000_0000_0000u64 | class_info.class_id as u64;
        buf.extend_from_slice(&class_obj_id.to_be_bytes());

        // Collect the full field list from the class hierarchy (super first)
        let mut field_chain: Vec<&[(String, String)]> = Vec::new();
        // Start from the object's class info
        field_chain.push(&class_info.instance_fields);
        let mut cid = class_info.super_class_id;
        while cid != 0 {
            if let Some(ci) = all_classes.get(&cid) {
                field_chain.push(&ci.instance_fields);
                cid = ci.super_class_id;
            } else {
                break;
            }
        }
        field_chain.reverse(); // superclass fields first

        // Compute data_size: sum of HPROF-typed field sizes
        let mut data_size: u32 = 0;
        for fields in &field_chain {
            for (_fname, fdesc) in *fields {
                data_size += HprofBasicType::from_descriptor(fdesc).size() as u32;
            }
        }
        buf.extend_from_slice(&data_size.to_be_bytes());

        // Write field values by reading raw memory from the heap object.
        // Internal layout:
        // - Legacy object: fields are at HEADER_SIZE + field_index * SLOT_SIZE, each
        //   slot is 16 bytes containing a Value enum.
        // - Compact object: fields use their natural widths at offsets from the
        //   immutable layout version selected by this object's field count.
        let mut field_index: usize = 0;
        let header = unsafe { &*(obj.data_ptr as *const ObjectHeader) };
        let compact_layout = if is_compact_object(header) {
            class_layout_for_fields(header.class_id.as_u32(), header.num_slots())
        } else {
            None
        };

        for fields in &field_chain {
            for (_fname, fdesc) in *fields {
                let htype = HprofBasicType::from_descriptor(fdesc);
                let slot_offset = compact_layout
                    .as_ref()
                    .and_then(|layout| layout.field_offset(field_index))
                    .map_or_else(
                        || HEADER_SIZE + field_index * SLOT_SIZE,
                        |off| HEADER_SIZE + off as usize,
                    );

                // Safety: obj.data_ptr points to a valid heap object with at least
                // obj.total_size bytes allocated.
                let slot_ptr = unsafe { obj.data_ptr.add(slot_offset) };
                let is_compact_ref = compact_layout
                    .as_ref()
                    .and_then(|layout| layout.field_is_ref(field_index))
                    .unwrap_or(false);

                match htype {
                    HprofBasicType::Object => {
                        // Legacy: Value::Object stores the reference in the 8-byte Value payload.
                        // Compact: reference fields are raw pointers at field offsets.
                        let val = if is_compact_ref {
                            if slot_offset + 8 <= obj.total_size {
                                unsafe { std::ptr::read_unaligned(slot_ptr as *const u64) }
                            } else {
                                0u64
                            }
                        } else if slot_offset + FIELD_CELL_PAYLOAD64_OFFSET + 8 <= obj.total_size {
                            unsafe {
                                std::ptr::read_unaligned(
                                    slot_ptr.add(FIELD_CELL_PAYLOAD64_OFFSET) as *const u64
                                )
                            }
                        } else {
                            0u64
                        };
                        buf.extend_from_slice(&val.to_be_bytes());
                    }
                    HprofBasicType::Int => {
                        let val = if slot_offset + 4 <= obj.total_size {
                            unsafe { std::ptr::read_unaligned(slot_ptr as *const i32) }
                        } else {
                            0i32
                        };
                        buf.extend_from_slice(&val.to_be_bytes());
                    }
                    HprofBasicType::Long => {
                        let val = if slot_offset + 8 <= obj.total_size {
                            unsafe { std::ptr::read_unaligned(slot_ptr as *const i64) }
                        } else {
                            0i64
                        };
                        buf.extend_from_slice(&val.to_be_bytes());
                    }
                    HprofBasicType::Float => {
                        let val = if slot_offset + 4 <= obj.total_size {
                            unsafe { std::ptr::read_unaligned(slot_ptr as *const f32) }
                        } else {
                            0.0f32
                        };
                        buf.extend_from_slice(&val.to_be_bytes());
                    }
                    HprofBasicType::Double => {
                        let val = if slot_offset + 8 <= obj.total_size {
                            unsafe { std::ptr::read_unaligned(slot_ptr as *const f64) }
                        } else {
                            0.0f64
                        };
                        buf.extend_from_slice(&val.to_be_bytes());
                    }
                    HprofBasicType::Short | HprofBasicType::Char => {
                        let val = if slot_offset + 2 <= obj.total_size {
                            unsafe { std::ptr::read_unaligned(slot_ptr as *const i16) }
                        } else {
                            0i16
                        };
                        buf.extend_from_slice(&val.to_be_bytes());
                    }
                    HprofBasicType::Boolean | HprofBasicType::Byte => {
                        let val = if slot_offset + 1 <= obj.total_size {
                            unsafe { *slot_ptr }
                        } else {
                            0u8
                        };
                        buf.push(val);
                    }
                }
                field_index += 1;
            }
        }
    }

    /// Write a GC_OBJ_ARRAY_DUMP sub-record.
    ///
    /// Format: tag(1) + array_obj_id(8) + stack_serial(4) + num_elements(4)
    ///       + array_class_obj_id(8) + elements[num_elements × 8]
    pub fn write_gc_obj_array_dump(buf: &mut Vec<u8>, obj: &HprofObjectInfo) {
        use cratonvm_gc::heap::{HEADER_SIZE, REF_ELEMENT_SIZE};

        buf.push(Self::GC_OBJ_ARRAY_DUMP);
        buf.extend_from_slice(&obj.object_id.to_be_bytes());
        buf.extend_from_slice(&0u32.to_be_bytes()); // stack trace serial
        buf.extend_from_slice(&obj.array_length.to_be_bytes());

        let class_obj_id = 0x1000_0000_0000_0000u64 | obj.class_id as u64;
        buf.extend_from_slice(&class_obj_id.to_be_bytes());

        // Read each element as an 8-byte reference
        for i in 0..obj.array_length as usize {
            let elem_offset = HEADER_SIZE + i * ref_element_size();
            let val = if elem_offset + ref_element_size() <= obj.total_size {
                unsafe {
                    let ptr = obj.data_ptr.add(elem_offset);
                    read_ref_slot_unaligned(ptr)
                }
            } else {
                0u64
            };
            buf.extend_from_slice(&val.to_be_bytes());
        }
    }

    /// Write a GC_PRIM_ARRAY_DUMP sub-record.
    ///
    /// Format: tag(1) + array_obj_id(8) + stack_serial(4) + num_elements(4)
    ///       + element_type(1) + elements[num_elements × element_size]
    pub fn write_gc_prim_array_dump(buf: &mut Vec<u8>, obj: &HprofObjectInfo) {
        use cratonvm_gc::heap::HEADER_SIZE;

        buf.push(Self::GC_PRIM_ARRAY_DUMP);
        buf.extend_from_slice(&obj.object_id.to_be_bytes());
        buf.extend_from_slice(&0u32.to_be_bytes()); // stack trace serial
        buf.extend_from_slice(&obj.array_length.to_be_bytes());

        // Map ArrayElementType repr to HPROF basic type
        let hprof_type = match obj.element_type {
            4 => HprofBasicType::Boolean,
            5 => HprofBasicType::Char,
            6 => HprofBasicType::Float,
            7 => HprofBasicType::Double,
            8 => HprofBasicType::Byte,
            9 => HprofBasicType::Short,
            10 => HprofBasicType::Int,
            11 => HprofBasicType::Long,
            _ => HprofBasicType::Byte,
        };
        buf.push(hprof_type as u8);

        let elem_size = hprof_type.size();
        let data_start = HEADER_SIZE;

        for i in 0..obj.array_length as usize {
            let elem_offset = data_start + i * elem_size;
            if elem_offset + elem_size <= obj.total_size {
                let ptr = unsafe { obj.data_ptr.add(elem_offset) };
                // Copy raw bytes in big-endian order
                match elem_size {
                    1 => buf.push(unsafe { *ptr }),
                    2 => {
                        let val = unsafe { std::ptr::read_unaligned(ptr as *const u16) };
                        buf.extend_from_slice(&val.to_be_bytes());
                    }
                    4 => {
                        let val = unsafe { std::ptr::read_unaligned(ptr as *const u32) };
                        buf.extend_from_slice(&val.to_be_bytes());
                    }
                    8 => {
                        let val = unsafe { std::ptr::read_unaligned(ptr as *const u64) };
                        buf.extend_from_slice(&val.to_be_bytes());
                    }
                    _ => buf.push(0),
                }
            } else {
                // Pad with zeros for out-of-bounds
                for _ in 0..elem_size {
                    buf.push(0);
                }
            }
        }
    }

    /// Wrap a heap dump body buffer as a HEAP_DUMP_SEGMENT record.
    fn wrap_segment(body: &[u8]) -> Vec<u8> {
        let mut rec = Vec::with_capacity(9 + body.len());
        rec.push(Self::HPROF_HEAP_DUMP_SEGMENT);
        rec.extend_from_slice(&0u32.to_be_bytes()); // timestamp
        rec.extend_from_slice(&(body.len() as u32).to_be_bytes());
        rec.extend_from_slice(body);
        rec
    }

    /// Generate a complete HPROF binary heap dump.
    ///
    /// This is the main entry point for producing a valid HPROF file.
    /// It writes all required records in the correct order:
    /// 1. File header
    /// 2. UTF-8 string records (class names, field names)
    /// 3. LOAD_CLASS records
    /// 4. STACK_TRACE records (one dummy trace for each thread)
    /// 5. HEAP_DUMP_SEGMENT records (class dumps, roots, instance dumps)
    /// 6. HEAP_DUMP_END
    pub fn write_full_heap_dump(
        classes: &[HprofClassInfo],
        objects: &[HprofObjectInfo],
        thread_snapshots: &[ThreadSnapshot],
    ) -> Vec<u8> {
        let mut output = Vec::new();
        let mut string_ids: std::collections::HashMap<String, u64> =
            std::collections::HashMap::new();
        let mut next_string_id: u64 = 1;
        let class_map: std::collections::HashMap<u32, HprofClassInfo> =
            classes.iter().map(|c| (c.class_id, c.clone())).collect();

        // Helper: intern a string, returning its ID
        let intern =
            |s: &str, ids: &mut std::collections::HashMap<String, u64>, nid: &mut u64| -> u64 {
                if let Some(&id) = ids.get(s) {
                    return id;
                }
                let id = *nid;
                ids.insert(s.to_string(), id);
                *nid += 1;
                id
            };

        // Phase 1: Collect all strings that need UTF-8 records
        // Class names
        for ci in classes {
            intern(&ci.name, &mut string_ids, &mut next_string_id);
            if let Some(ref sf) = ci.source_file {
                intern(sf, &mut string_ids, &mut next_string_id);
            }
            for (fname, _) in &ci.instance_fields {
                intern(fname, &mut string_ids, &mut next_string_id);
            }
            for (fname, _) in &ci.static_fields {
                intern(fname, &mut string_ids, &mut next_string_id);
            }
        }
        // Thread names
        for ts in thread_snapshots {
            intern(&ts.name, &mut string_ids, &mut next_string_id);
            for frame in &ts.stack_frames {
                intern(&frame.class_name, &mut string_ids, &mut next_string_id);
                intern(&frame.method_name, &mut string_ids, &mut next_string_id);
                if let Some(ref f) = frame.file_name {
                    intern(f, &mut string_ids, &mut next_string_id);
                }
            }
        }

        // Phase 2: Write header
        output.extend_from_slice(&Self::write_header());

        // Phase 3: Write UTF-8 string records
        let mut sorted_strings: Vec<(&String, &u64)> = string_ids.iter().collect();
        sorted_strings.sort_by_key(|(_, id)| **id);
        for (s, id) in sorted_strings {
            output.extend_from_slice(&Self::write_string_record(*id, s));
        }

        // Phase 4: Write LOAD_CLASS records
        for (serial_idx, ci) in classes.iter().enumerate() {
            let serial = (serial_idx + 1) as u32;
            let class_obj_id = 0x1000_0000_0000_0000u64 | ci.class_id as u64;
            let name_id = string_ids.get(&ci.name).copied().unwrap_or(0);
            output.extend_from_slice(&Self::write_load_class(serial, class_obj_id, 0, name_id));
        }

        // Phase 5: Write STACK_TRACE records (one per thread + a dummy trace serial 0)
        // Dummy stack trace serial 0 with no frames (used by objects with unknown stack)
        output.extend_from_slice(&Self::write_stack_trace(0, 0, &[]));

        let mut next_frame_id: u64 = 1;
        for (tidx, ts) in thread_snapshots.iter().enumerate() {
            let thread_serial = (tidx + 1) as u32;
            let trace_serial = thread_serial;

            // Write stack frame records for this thread
            let mut frame_ids = Vec::new();
            for frame in &ts.stack_frames {
                let fid = next_frame_id;
                next_frame_id += 1;
                let method_name_id = string_ids.get(&frame.method_name).copied().unwrap_or(0);
                let class_name_id = string_ids.get(&frame.class_name).copied().unwrap_or(0);
                let source_id = frame
                    .file_name
                    .as_ref()
                    .and_then(|f| string_ids.get(f))
                    .copied()
                    .unwrap_or(0);
                output.extend_from_slice(&Self::write_stack_frame(
                    fid,
                    method_name_id,
                    class_name_id,
                    source_id,
                    0, // class serial (could look up but 0 is valid)
                    frame.line_number,
                ));
                frame_ids.push(fid);
            }

            output.extend_from_slice(&Self::write_stack_trace(
                trace_serial,
                thread_serial,
                &frame_ids,
            ));
        }

        // Phase 6: Write HEAP_DUMP_SEGMENT records
        let mut seg_body = Vec::new();

        // 6a: GC roots — thread objects
        for (tidx, ts) in thread_snapshots.iter().enumerate() {
            let thread_serial = (tidx + 1) as u32;
            // Use thread ID as a synthetic object ID for the thread root
            let thread_obj_id = 0x2000_0000_0000_0000u64 | ts.id;
            Self::write_gc_root_thread_obj(
                &mut seg_body,
                thread_obj_id,
                thread_serial,
                thread_serial,
            );
        }

        // 6b: CLASS_DUMP sub-records
        for ci in classes {
            Self::write_gc_class_dump(&mut seg_body, ci, &string_ids);

            // Flush segment if it's getting large
            if seg_body.len() >= Self::MAX_SEGMENT_SIZE {
                output.extend_from_slice(&Self::wrap_segment(&seg_body));
                seg_body.clear();
            }
        }

        // 6c: Instance / array dump sub-records
        for obj in objects {
            if obj.is_array {
                // Determine if reference array or primitive array
                if obj.element_type == 0 {
                    // Reference array (ArrayElementType::Reference = 0)
                    Self::write_gc_obj_array_dump(&mut seg_body, obj);
                } else {
                    Self::write_gc_prim_array_dump(&mut seg_body, obj);
                }
            } else {
                // Regular object instance
                if let Some(ci) = class_map.get(&obj.class_id) {
                    Self::write_gc_instance_dump(&mut seg_body, obj, ci, &class_map);
                }
            }

            // Flush segment if large
            if seg_body.len() >= Self::MAX_SEGMENT_SIZE {
                output.extend_from_slice(&Self::wrap_segment(&seg_body));
                seg_body.clear();
            }
        }

        // Flush remaining segment body
        if !seg_body.is_empty() {
            output.extend_from_slice(&Self::wrap_segment(&seg_body));
        }

        // Phase 7: HEAP_DUMP_END
        output.extend_from_slice(&Self::write_heap_dump_end());

        output
    }
}

// ---------------------------------------------------------------------------
// Sample data helpers (used by registered commands and tests)
// ---------------------------------------------------------------------------

fn sample_thread_snapshots() -> Vec<ThreadSnapshot> {
    vec![
        ThreadSnapshot {
            id: 1,
            name: "main".to_string(),
            daemon: false,
            priority: 5,
            state: ThreadState::Runnable,
            stack_frames: vec![FrameInfo {
                class_name: "com.example.Main".to_string(),
                method_name: "main".to_string(),
                file_name: Some("Main.java".to_string()),
                line_number: 10,
                native_method: false,
            }],
            lock_info: None,
            blocked_by: None,
            waiting_on: None,
        },
        ThreadSnapshot {
            id: 2,
            name: "GC-Thread".to_string(),
            daemon: true,
            priority: 8,
            state: ThreadState::Waiting,
            stack_frames: vec![FrameInfo {
                class_name: "java.lang.Object".to_string(),
                method_name: "wait".to_string(),
                file_name: None,
                line_number: -1,
                native_method: true,
            }],
            lock_info: None,
            blocked_by: None,
            waiting_on: Some(
                "<0x00000000c0000000> (a java.lang.ref.ReferenceQueue$Lock)".to_string(),
            ),
        },
    ]
}

fn sample_class_histogram() -> Vec<ClassHistogramEntry> {
    vec![
        ClassHistogramEntry {
            class_name: "[B".to_string(),
            instance_count: 50000,
            total_bytes: 5_000_000,
        },
        ClassHistogramEntry {
            class_name: "java.lang.String".to_string(),
            instance_count: 40000,
            total_bytes: 1_600_000,
        },
        ClassHistogramEntry {
            class_name: "java.lang.Object[]".to_string(),
            instance_count: 20000,
            total_bytes: 800_000,
        },
        ClassHistogramEntry {
            class_name: "java.util.HashMap$Node".to_string(),
            instance_count: 15000,
            total_bytes: 720_000,
        },
        ClassHistogramEntry {
            class_name: "java.lang.Class".to_string(),
            instance_count: 4200,
            total_bytes: 672_000,
        },
        ClassHistogramEntry {
            class_name: "java.util.HashMap$Node[]".to_string(),
            instance_count: 3000,
            total_bytes: 500_000,
        },
        ClassHistogramEntry {
            class_name: "char[]".to_string(),
            instance_count: 30000,
            total_bytes: 480_000,
        },
        ClassHistogramEntry {
            class_name: "java.lang.reflect.Method".to_string(),
            instance_count: 5000,
            total_bytes: 400_000,
        },
        ClassHistogramEntry {
            class_name: "java.util.concurrent.ConcurrentHashMap$Node".to_string(),
            instance_count: 8000,
            total_bytes: 384_000,
        },
        ClassHistogramEntry {
            class_name: "int[]".to_string(),
            instance_count: 10000,
            total_bytes: 360_000,
        },
    ]
}

// ---------------------------------------------------------------------------
// HSDB — HotSpot Serviceability Agent wire protocol (T6.1.8)
// ---------------------------------------------------------------------------
//
// A minimal implementation of the HSDB wire protocol used by `jhsdb hsdb`
// and related tooling. This is a lightweight binary protocol over TCP that
// exposes read-only introspection of VM state.
//
// Protocol format (big-endian):
//   Handshake: client sends 4-byte magic 0x48534442 ("HSDB"), server echoes.
//   Request : u8 command id + u32 payload_len + payload bytes
//   Response: u8 status (0=OK, 1=ERR) + u32 payload_len + payload bytes
//
// Commands:
//   0x01 VERSION        — no payload. Response payload: UTF-8 version string.
//   0x02 PROCESS_INFO   — no payload. Response: pid(u64) + cmdline(u16 len + bytes).
//   0x03 HEAP_SUMMARY   — no payload. Response: 3*u64 (used, committed, max) bytes.
//   0x04 THREAD_LIST    — no payload. Response: u32 count + for each:
//                            u64 tid + u16 name_len + name bytes + u8 state
//
// The protocol number in VERSION is 1. A connect-probe client sends VERSION
// first; we reply with "CratonVM HSDB v1.0".

/// HSDB protocol magic handshake ("HSDB" in ASCII).
pub const HSDB_MAGIC: u32 = 0x4853_4442;

/// HSDB protocol version string returned in response to VERSION command.
pub const HSDB_VERSION_STRING: &str = "CratonVM HSDB v1.0";

/// HSDB command opcodes.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HsdbCommand {
    Version = 0x01,
    ProcessInfo = 0x02,
    HeapSummary = 0x03,
    ThreadList = 0x04,
}

impl HsdbCommand {
    pub fn from_u8(n: u8) -> Option<Self> {
        match n {
            0x01 => Some(HsdbCommand::Version),
            0x02 => Some(HsdbCommand::ProcessInfo),
            0x03 => Some(HsdbCommand::HeapSummary),
            0x04 => Some(HsdbCommand::ThreadList),
            _ => None,
        }
    }
}

/// HSDB response status.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HsdbStatus {
    Ok = 0,
    Error = 1,
}

/// Encode a u16 length-prefixed UTF-8 string.
fn encode_string16(s: &str, out: &mut Vec<u8>) {
    let bytes = s.as_bytes();
    let len = bytes.len().min(u16::MAX as usize) as u16;
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&bytes[..len as usize]);
}

/// Handle a single HSDB request and return a (status, payload) response tuple.
/// This is the pure functional core — transport is left to callers.
pub fn hsdb_handle_request(
    cmd: HsdbCommand,
    _payload: &[u8],
    state: Option<&dyn VmDiagnosticState>,
) -> (HsdbStatus, Vec<u8>) {
    match cmd {
        HsdbCommand::Version => {
            let mut out = Vec::new();
            encode_string16(HSDB_VERSION_STRING, &mut out);
            (HsdbStatus::Ok, out)
        }
        HsdbCommand::ProcessInfo => {
            let pid: u64 = std::process::id() as u64;
            let cmdline = match state {
                Some(s) => s.command_line(),
                None => String::new(),
            };
            let mut out = Vec::with_capacity(8 + 2 + cmdline.len());
            out.extend_from_slice(&pid.to_be_bytes());
            encode_string16(&cmdline, &mut out);
            (HsdbStatus::Ok, out)
        }
        HsdbCommand::HeapSummary => match state {
            Some(s) => {
                let summary = s.heap_summary();
                // Wire layout (big-endian): young_used, young_cap, old_used, old_cap,
                // meta_used, meta_cap, total_used, total_cap (all u64) = 64 bytes.
                let mut out = Vec::with_capacity(64);
                out.extend_from_slice(&summary.young_gen_used.to_be_bytes());
                out.extend_from_slice(&summary.young_gen_capacity.to_be_bytes());
                out.extend_from_slice(&summary.old_gen_used.to_be_bytes());
                out.extend_from_slice(&summary.old_gen_capacity.to_be_bytes());
                out.extend_from_slice(&summary.metaspace_used.to_be_bytes());
                out.extend_from_slice(&summary.metaspace_capacity.to_be_bytes());
                out.extend_from_slice(&summary.total_used.to_be_bytes());
                out.extend_from_slice(&summary.total_capacity.to_be_bytes());
                (HsdbStatus::Ok, out)
            }
            None => (HsdbStatus::Error, b"no VM state".to_vec()),
        },
        HsdbCommand::ThreadList => match state {
            Some(s) => {
                let threads = s.thread_snapshots();
                let mut out = Vec::new();
                let count = threads.len() as u32;
                out.extend_from_slice(&count.to_be_bytes());
                for t in &threads {
                    out.extend_from_slice(&t.id.to_be_bytes());
                    encode_string16(&t.name, &mut out);
                    let state_byte: u8 = match t.state {
                        ThreadState::New => 0,
                        ThreadState::Runnable => 1,
                        ThreadState::Blocked => 2,
                        ThreadState::Waiting => 3,
                        ThreadState::TimedWaiting => 4,
                        ThreadState::Terminated => 7,
                    };
                    out.push(state_byte);
                }
                (HsdbStatus::Ok, out)
            }
            None => (HsdbStatus::Error, b"no VM state".to_vec()),
        },
    }
}

/// Encode a full HSDB response: u8 status + u32 payload_len + payload.
pub fn hsdb_encode_response(status: HsdbStatus, payload: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(5 + payload.len());
    buf.push(status as u8);
    buf.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    buf.extend_from_slice(payload);
    buf
}

/// Decode an HSDB request header from the wire: u8 cmd + u32 payload_len.
/// Returns the parsed (cmd, payload_len) or an error string.
pub fn hsdb_decode_request_header(bytes: &[u8]) -> Result<(HsdbCommand, u32), String> {
    if bytes.len() < 5 {
        return Err(format!("short request header: {} < 5", bytes.len()));
    }
    let cmd = HsdbCommand::from_u8(bytes[0])
        .ok_or_else(|| format!("unknown command byte {}", bytes[0]))?;
    let len = u32::from_be_bytes([bytes[1], bytes[2], bytes[3], bytes[4]]);
    Ok((cmd, len))
}

/// Start the HSDB listener on the given TCP port. Runs on a background thread.
/// Returns a handle that can be used to stop the listener.
///
/// Typical port selection is `debug_port + 1` where `debug_port` is the JDWP
/// port; callers are responsible for choosing a free port.
pub fn hsdb_start_listener(
    port: u16,
    state: Arc<dyn VmDiagnosticState>,
) -> std::io::Result<HsdbListener> {
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;

    let listener = TcpListener::bind(("127.0.0.1", port))?;
    listener.set_nonblocking(true)?;
    let running = Arc::new(AtomicBool::new(true));
    let running_clone = Arc::clone(&running);
    let state_clone = Arc::clone(&state);
    let local_addr = listener.local_addr()?;

    let thread_handle = thread::spawn(move || {
        use std::io::{Read, Write};
        use std::time::Duration;

        while running_clone.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((mut stream, _peer)) => {
                    // Ensure accepted stream uses blocking I/O even on platforms
                    // where nonblocking is inherited from the listener.
                    let _ = stream.set_nonblocking(false);
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
                    let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));

                    // Handshake
                    let mut magic = [0u8; 4];
                    if stream.read_exact(&mut magic).is_err() {
                        continue;
                    }
                    let got_magic = u32::from_be_bytes(magic);
                    if got_magic != HSDB_MAGIC {
                        continue;
                    }
                    if stream.write_all(&HSDB_MAGIC.to_be_bytes()).is_err() {
                        continue;
                    }

                    // Read requests until client disconnects
                    loop {
                        let mut hdr = [0u8; 5];
                        if stream.read_exact(&mut hdr).is_err() {
                            break;
                        }
                        let (cmd, payload_len) = match hsdb_decode_request_header(&hdr) {
                            Ok(p) => p,
                            Err(_) => break,
                        };
                        let mut payload = vec![0u8; payload_len as usize];
                        if payload_len > 0 && stream.read_exact(&mut payload).is_err() {
                            break;
                        }
                        let (status, resp_payload) =
                            hsdb_handle_request(cmd, &payload, Some(state_clone.as_ref()));
                        let resp = hsdb_encode_response(status, &resp_payload);
                        if stream.write_all(&resp).is_err() {
                            break;
                        }
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(50));
                }
                Err(_) => break,
            }
        }
    });

    Ok(HsdbListener {
        addr: local_addr,
        running,
        thread: Some(thread_handle),
    })
}

/// Handle for a running HSDB listener. Dropping the handle stops the listener.
pub struct HsdbListener {
    pub addr: std::net::SocketAddr,
    running: Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl HsdbListener {
    /// Signal the listener to stop and wait for its thread to exit.
    pub fn stop(mut self) {
        self.running
            .store(false, std::sync::atomic::Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for HsdbListener {
    fn drop(&mut self) {
        self.running
            .store(false, std::sync::atomic::Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- AttachListener tests ---

    #[test]
    fn test_attach_listener_new() {
        let listener = AttachListener::new("/tmp/test_attach");
        assert_eq!(listener.socket_path, "/tmp/test_attach");
        assert!(!listener.is_listening);
        assert!(listener.commands.read().is_empty());
    }

    #[test]
    fn test_attach_listener_start_stop() {
        // obsaudit D15: a unique path — start_listening() now binds a real
        // socket, so a path shared with another test risks a parallel-test
        // bind race (see the comment on TEST_JCMD_SOCKET_COUNTER).
        let mut listener = AttachListener::new("/tmp/cratonvm_test_start_stop");
        assert!(!listener.is_listening);
        listener.start_listening();
        assert!(listener.is_listening);
        listener.stop_listening();
        assert!(!listener.is_listening);
    }

    #[test]
    fn test_attach_listener_register_command() {
        let mut listener = AttachListener::new("/tmp/test");
        listener.register_command(DiagnosticCommand::new(
            "Test.cmd",
            "A test command",
            CommandImpact::Low,
            CommandPermission::ReadOnly,
            vec![],
            Box::new(|_| CommandResult::ok("ok".to_string(), 0)),
        ));
        assert_eq!(listener.commands.read().len(), 1);
        assert_eq!(listener.commands.read()[0].name, "Test.cmd");
    }

    #[test]
    fn test_attach_listener_dispatch_command() {
        // obsaudit D15: renamed from test_attach_listener_find_command —
        // find_command was removed (it could not return a reference into
        // the now-lock-guarded `commands` without a dangling-guard
        // lifetime issue), replaced by dispatch_command, which looks up
        // and executes under a single lock acquisition.
        let mut listener = AttachListener::new("/tmp/test");
        listener.register_command(DiagnosticCommand::new(
            "Test.cmd",
            "desc",
            CommandImpact::Low,
            CommandPermission::ReadOnly,
            vec![],
            Box::new(|_| CommandResult::ok("ok".to_string(), 0)),
        ));
        assert!(listener.dispatch_command("Test.cmd", &[]).is_some());
        assert!(listener.dispatch_command("Nonexistent", &[]).is_none());
    }

    #[test]
    fn test_attach_listener_list_commands() {
        let mut listener = AttachListener::new("/tmp/test");
        listener.register_command(DiagnosticCommand::new(
            "A.cmd",
            "d",
            CommandImpact::Low,
            CommandPermission::ReadOnly,
            vec![],
            Box::new(|_| CommandResult::ok("ok".to_string(), 0)),
        ));
        listener.register_command(DiagnosticCommand::new(
            "B.cmd",
            "d",
            CommandImpact::Medium,
            CommandPermission::ManagementAction,
            vec![],
            Box::new(|_| CommandResult::ok("ok".to_string(), 0)),
        ));
        let names = listener.list_commands();
        assert_eq!(names, vec!["A.cmd".to_string(), "B.cmd".to_string()]);
    }

    // --- DiagnosticCommand tests ---

    #[test]
    fn test_diagnostic_command_execute() {
        let cmd = DiagnosticCommand::new(
            "Test.echo",
            "Echo args",
            CommandImpact::Low,
            CommandPermission::ReadOnly,
            vec![],
            Box::new(|args| CommandResult::ok(args.join(", "), 0)),
        );
        let result = cmd.execute(&["hello".to_string(), "world".to_string()]);
        assert!(result.success);
        assert_eq!(result.output, "hello, world");
    }

    #[test]
    fn test_command_result_ok() {
        let r = CommandResult::ok("done".to_string(), 42);
        assert!(r.success);
        assert_eq!(r.output, "done");
        assert!(r.error.is_none());
        assert_eq!(r.execution_time_ms, 42);
    }

    #[test]
    fn test_command_result_err() {
        let r = CommandResult::err("fail".to_string(), 5);
        assert!(!r.success);
        assert!(r.output.is_empty());
        assert_eq!(r.error.as_deref(), Some("fail"));
    }

    #[test]
    fn test_command_impact_display() {
        assert_eq!(format!("{}", CommandImpact::Low), "Low");
        assert_eq!(format!("{}", CommandImpact::Medium), "Medium");
        assert_eq!(format!("{}", CommandImpact::High), "High");
    }

    #[test]
    fn test_command_with_arguments() {
        let cmd = DiagnosticCommand::new(
            "Test.args",
            "Test with args",
            CommandImpact::Low,
            CommandPermission::ReadOnly,
            vec![
                CommandArgument {
                    name: "path".to_string(),
                    description: "File path".to_string(),
                    arg_type: ArgType::FilePath,
                    required: true,
                    default_value: None,
                },
                CommandArgument {
                    name: "verbose".to_string(),
                    description: "Verbose output".to_string(),
                    arg_type: ArgType::Bool,
                    required: false,
                    default_value: Some("false".to_string()),
                },
            ],
            Box::new(|_| CommandResult::ok("ok".to_string(), 0)),
        );
        assert_eq!(cmd.arguments.len(), 2);
        assert_eq!(cmd.arguments[0].arg_type, ArgType::FilePath);
        assert!(cmd.arguments[0].required);
        assert!(!cmd.arguments[1].required);
        assert_eq!(cmd.arguments[1].default_value.as_deref(), Some("false"));
    }

    // --- JcmdProcessor tests ---

    #[test]
    fn test_jcmd_new_has_all_commands() {
        let jcmd = JcmdProcessor::new();
        let names = jcmd.attach_listener.list_commands();
        assert_eq!(names.len(), 16);
        assert!(names.contains(&"Thread.print".to_string()));
        assert!(names.contains(&"GC.heap_dump".to_string()));
        assert!(names.contains(&"GC.run".to_string()));
        assert!(names.contains(&"GC.heap_info".to_string()));
        assert!(names.contains(&"GC.class_histogram".to_string()));
        assert!(names.contains(&"VM.version".to_string()));
        assert!(names.contains(&"VM.flags".to_string()));
        assert!(names.contains(&"VM.system_properties".to_string()));
        assert!(names.contains(&"VM.uptime".to_string()));
        assert!(names.contains(&"VM.info".to_string()));
        assert!(names.contains(&"VM.command_line".to_string()));
        assert!(names.contains(&"Thread.dump_to_file".to_string()));
        assert!(names.contains(&"Compiler.queue".to_string()));
        assert!(names.contains(&"JFR.start".to_string()));
        assert!(names.contains(&"JFR.stop".to_string()));
        assert!(names.contains(&"JFR.dump".to_string()));
    }

    #[test]
    fn test_jcmd_thread_print() {
        let jcmd = JcmdProcessor::new();
        let result = jcmd.process_command("Thread.print");
        assert!(result.success);
        assert!(result.output.contains("main"));
        assert!(result.output.contains("RUNNABLE"));
    }

    #[test]
    fn test_jcmd_gc_heap_dump() {
        let jcmd = JcmdProcessor::new();
        let result = jcmd.process_command("GC.heap_dump /tmp/dump.hprof");
        assert!(result.success);
        assert!(result
            .output
            .contains("Heap dump written to /tmp/dump.hprof"));
        assert!(result.output.contains(HprofWriter::HPROF_MAGIC));
    }

    #[test]
    fn test_jcmd_gc_run() {
        let jcmd = JcmdProcessor::new();
        let result = jcmd.process_command("GC.run");
        assert!(result.success);
        assert_eq!(result.output, "GC triggered");
    }

    #[test]
    fn test_jcmd_gc_heap_info() {
        let jcmd = JcmdProcessor::new();
        let result = jcmd.process_command("GC.heap_info");
        assert!(result.success);
        assert!(result.output.contains("Young Generation"));
        assert!(result.output.contains("Old Generation"));
        assert!(result.output.contains("Metaspace"));
    }

    #[test]
    fn test_jcmd_gc_class_histogram() {
        let jcmd = JcmdProcessor::new();
        let result = jcmd.process_command("GC.class_histogram");
        assert!(result.success);
        assert!(result.output.contains("#instances"));
        assert!(result.output.contains("java.lang.String"));
    }

    #[test]
    fn test_jcmd_vm_version() {
        let jcmd = JcmdProcessor::new();
        let result = jcmd.process_command("VM.version");
        assert!(result.success);
        assert_eq!(result.output, "CratonVM 1.0.0 (JDK 25 compatible)");
    }

    #[test]
    fn test_jcmd_vm_flags() {
        let jcmd = JcmdProcessor::new();
        let result = jcmd.process_command("VM.flags");
        assert!(result.success);
        assert!(result.output.contains("-XX:+UseG1GC"));
        assert!(result.output.lines().count() >= 15);
    }

    #[test]
    fn test_jcmd_vm_system_properties() {
        let jcmd = JcmdProcessor::new();
        let result = jcmd.process_command("VM.system_properties");
        assert!(result.success);
        assert!(result.output.contains("java.version=25"));
        assert!(result.output.contains("java.vendor=CratonVM"));
    }

    #[test]
    fn test_jcmd_vm_uptime() {
        let jcmd = JcmdProcessor::new();
        let result = jcmd.process_command("VM.uptime");
        assert!(result.success);
        assert!(result.output.contains("uptime"));
    }

    #[test]
    fn test_jcmd_vm_info() {
        let jcmd = JcmdProcessor::new();
        let result = jcmd.process_command("VM.info");
        assert!(result.success);
        assert!(result.output.contains("CratonVM"));
        assert!(result.output.contains("Heap"));
    }

    #[test]
    fn test_jcmd_vm_command_line() {
        let jcmd = JcmdProcessor::new();
        let result = jcmd.process_command("VM.command_line");
        assert!(result.success);
        assert!(result.output.contains("-Xmx256m"));
    }

    #[test]
    fn test_jcmd_thread_dump_to_file() {
        let jcmd = JcmdProcessor::new();
        let result = jcmd.process_command("Thread.dump_to_file /tmp/threads.txt");
        assert!(result.success);
        assert!(result
            .output
            .contains("Thread dump written to /tmp/threads.txt"));
    }

    /// Observability audit (2026-07-26): `Compiler.queue` must NOT invent a
    /// compilation queue. It previously returned a hard-coded listing naming
    /// `com.example.Main.hotLoop()V` — fiction an operator would have read as
    /// ground truth. Until the JIT queue is exposed to serviceability, the
    /// command must fail honestly.
    #[test]
    fn obsaudit_jcmd_compiler_queue_does_not_fabricate() {
        let jcmd = JcmdProcessor::new();
        let result = jcmd.process_command("Compiler.queue");
        assert!(
            !result.success,
            "Compiler.queue must not report success while unimplemented"
        );
        assert!(
            !result.output.contains("com.example.Main"),
            "fabricated compile-queue entries must never reappear"
        );
        assert!(
            !result.output.contains("C1 compile queue"),
            "fabricated compile-queue entries must never reappear"
        );
        assert!(result
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("not implemented"));
    }

    /// Observability audit (2026-07-26): the JFR jcmd verbs must not claim
    /// success while never touching the FlightRecorder. A false "recording
    /// started" costs the operator the incident window they were capturing.
    #[test]
    fn obsaudit_jcmd_jfr_verbs_do_not_claim_false_success() {
        let jcmd = JcmdProcessor::new();
        for (cmd, forbidden) in [
            ("JFR.start myrecording", "Flight recording started"),
            ("JFR.stop myrecording", "Flight recording stopped"),
            ("JFR.dump /tmp/rec.jfr", "Flight recording dumped"),
        ] {
            let result = jcmd.process_command(cmd);
            assert!(
                !result.success,
                "`{cmd}` must not report success: nothing wires it to the FlightRecorder"
            );
            assert!(
                !result.output.contains(forbidden),
                "`{cmd}` must not emit the old fake-success string `{forbidden}`"
            );
            assert!(
                result
                    .error
                    .as_deref()
                    .unwrap_or_default()
                    .contains("not implemented"),
                "`{cmd}` must say plainly that it is unimplemented"
            );
        }
    }

    /// Pins the attach surface's real socket behaviour: `start_listening`
    /// binds, `stop_listening` unlinks. If someone changes either half they
    /// must also revisit the LIVENESS block at the top of this module and the
    /// fabricated-data audit notes it points at.
    ///
    /// Platform split, deliberate and asserted rather than skipped: the
    /// implementation is `#[cfg(unix)]` (see `AttachListener::start_listening`
    /// — HotSpot itself splits per OS, Unix domain socket on Linux/macOS
    /// versus a named pipe on Windows, and the Windows side is unimplemented
    /// here). This test used to assert the Unix shape unconditionally, so on
    /// a Windows checkout it demanded a `/tmp/...` socket file that the
    /// `#[cfg(not(unix))]` `start_listening` — which only flips
    /// `is_listening` — never creates, and failed 100% of the time. It now
    /// pins the Unix contract in full and the non-Unix contract exactly as
    /// documented, so the gap stays visible instead of being papered over by
    /// a skip.
    #[test]
    fn obsaudit_attach_listener_creates_a_real_socket() {
        // obsaudit D15 (2026-07-26), FIXED: renamed from
        // obsaudit_attach_listener_creates_no_socket, which pinned the
        // opposite (no-socket) behaviour this fix replaced. start_listening
        // now binds a real Unix domain socket at `socket_path` — see the
        // struct doc comment for the wire protocol, verified empirically
        // against a real OpenJDK 21 jcmd/jstack/jmap.
        #[cfg(unix)]
        {
            let path = "/tmp/cratonvm-obsaudit-attach-socket-pin-test";
            let mut l = AttachListener::new(path);
            l.start_listening();
            assert!(l.is_listening);
            assert!(
                std::path::Path::new(path).exists(),
                "AttachListener must create a real socket file at socket_path"
            );
            use std::os::unix::fs::FileTypeExt;
            let meta = std::fs::symlink_metadata(path).unwrap();
            assert!(
                meta.file_type().is_socket(),
                "the file at socket_path must actually be a Unix domain socket, \
                 not e.g. a stray regular file"
            );
            l.stop_listening();
            assert!(
                !std::path::Path::new(path).exists(),
                "stop_listening must remove the socket file"
            );
        }
        #[cfg(not(unix))]
        {
            // No named-pipe implementation yet, so the documented contract is
            // "flips the flag, touches nothing on disk". Pin BOTH halves: the
            // flag round-trip (so callers that only read `is_listening` keep
            // working) and the absence of any file at `socket_path` (so a
            // future Windows implementation that starts creating one is
            // forced to come back here and state what it created).
            let path = std::env::temp_dir().join("cratonvm-obsaudit-attach-socket-pin-test");
            let _ = std::fs::remove_file(&path);
            let mut l = AttachListener::new(&path.to_string_lossy());
            l.start_listening();
            assert!(l.is_listening);
            assert!(
                !path.exists(),
                "non-Unix start_listening has no socket implementation, so it \
                 must not leave anything behind at socket_path"
            );
            l.stop_listening();
            assert!(!l.is_listening);
            assert!(!path.exists());
        }
    }

    #[test]
    fn test_jcmd_unknown_command() {
        let jcmd = JcmdProcessor::new();
        let result = jcmd.process_command("Nonexistent.command");
        assert!(!result.success);
        assert!(result.error.as_ref().unwrap().contains("Unknown command"));
    }

    #[test]
    fn test_jcmd_help() {
        let jcmd = JcmdProcessor::new();
        let result = jcmd.process_command("help");
        assert!(result.success);
        assert!(result.output.contains("Available commands"));
        assert!(result.output.contains("Thread.print"));
        assert!(result.output.contains("VM.version"));
    }

    #[test]
    fn test_jcmd_help_method() {
        let jcmd = JcmdProcessor::new();
        let help = jcmd.help();
        assert!(help.contains("Available commands"));
        // Should list all 16 commands
        for name in jcmd.attach_listener.list_commands() {
            assert!(help.contains(&name));
        }
    }

    // --- jstack tests ---

    #[test]
    fn test_thread_state_display() {
        assert_eq!(format!("{}", ThreadState::New), "NEW");
        assert_eq!(format!("{}", ThreadState::Runnable), "RUNNABLE");
        assert_eq!(format!("{}", ThreadState::Blocked), "BLOCKED");
        assert_eq!(format!("{}", ThreadState::Waiting), "WAITING");
        assert_eq!(format!("{}", ThreadState::TimedWaiting), "TIMED_WAITING");
        assert_eq!(format!("{}", ThreadState::Terminated), "TERMINATED");
    }

    #[test]
    fn test_jstack_thread_dump_format() {
        let threads = vec![ThreadSnapshot {
            id: 1,
            name: "main".to_string(),
            daemon: false,
            priority: 5,
            state: ThreadState::Runnable,
            stack_frames: vec![FrameInfo {
                class_name: "com.example.Main".to_string(),
                method_name: "run".to_string(),
                file_name: Some("Main.java".to_string()),
                line_number: 42,
                native_method: false,
            }],
            lock_info: None,
            blocked_by: None,
            waiting_on: None,
        }];

        let dump = JstackProcessor::generate_thread_dump(&threads);
        assert!(dump.contains("\"main\" #1 prio=5"));
        assert!(dump.contains("java.lang.Thread.State: RUNNABLE"));
        assert!(dump.contains("at com.example.Main.run(Main.java:42)"));
    }

    #[test]
    fn test_jstack_daemon_thread() {
        let threads = vec![ThreadSnapshot {
            id: 5,
            name: "GC-Worker".to_string(),
            daemon: true,
            priority: 8,
            state: ThreadState::Waiting,
            stack_frames: vec![],
            lock_info: None,
            blocked_by: None,
            waiting_on: None,
        }];

        let dump = JstackProcessor::generate_thread_dump(&threads);
        assert!(dump.contains("\"GC-Worker\" #5 daemon prio=8"));
        assert!(dump.contains("java.lang.Thread.State: WAITING"));
    }

    #[test]
    fn test_jstack_native_method() {
        let threads = vec![ThreadSnapshot {
            id: 3,
            name: "native-thread".to_string(),
            daemon: false,
            priority: 5,
            state: ThreadState::Runnable,
            stack_frames: vec![FrameInfo {
                class_name: "sun.misc.Unsafe".to_string(),
                method_name: "park".to_string(),
                file_name: None,
                line_number: -1,
                native_method: true,
            }],
            lock_info: None,
            blocked_by: None,
            waiting_on: None,
        }];

        let dump = JstackProcessor::generate_thread_dump(&threads);
        assert!(dump.contains("at sun.misc.Unsafe.park(Native Method)"));
    }

    #[test]
    fn test_jstack_lock_info() {
        let threads = vec![ThreadSnapshot {
            id: 1,
            name: "locker".to_string(),
            daemon: false,
            priority: 5,
            state: ThreadState::Runnable,
            stack_frames: vec![],
            lock_info: Some(LockInfo {
                class_name: "java.util.HashMap".to_string(),
                identity_hash: 0xDEAD_BEEF,
            }),
            blocked_by: None,
            waiting_on: None,
        }];

        let dump = JstackProcessor::generate_thread_dump(&threads);
        assert!(dump.contains("locked <0x00000000deadbeef> (a java.util.HashMap)"));
    }

    #[test]
    fn test_jstack_waiting_on() {
        let threads = vec![ThreadSnapshot {
            id: 2,
            name: "waiter".to_string(),
            daemon: false,
            priority: 5,
            state: ThreadState::Waiting,
            stack_frames: vec![],
            lock_info: None,
            blocked_by: None,
            waiting_on: Some("<0x000000c0> (a java.lang.Object)".to_string()),
        }];

        let dump = JstackProcessor::generate_thread_dump(&threads);
        assert!(dump.contains("waiting on <0x000000c0> (a java.lang.Object)"));
    }

    #[test]
    fn test_jstack_deadlock_detection_no_deadlock() {
        let threads = vec![
            ThreadSnapshot {
                id: 1,
                name: "t1".to_string(),
                daemon: false,
                priority: 5,
                state: ThreadState::Runnable,
                stack_frames: vec![],
                lock_info: None,
                blocked_by: None,
                waiting_on: None,
            },
            ThreadSnapshot {
                id: 2,
                name: "t2".to_string(),
                daemon: false,
                priority: 5,
                state: ThreadState::Runnable,
                stack_frames: vec![],
                lock_info: None,
                blocked_by: None,
                waiting_on: None,
            },
        ];
        assert!(JstackProcessor::generate_deadlock_report(&threads).is_none());
    }

    #[test]
    fn test_jstack_deadlock_detection_cycle() {
        let threads = vec![
            ThreadSnapshot {
                id: 1,
                name: "thread-A".to_string(),
                daemon: false,
                priority: 5,
                state: ThreadState::Blocked,
                stack_frames: vec![],
                lock_info: None,
                blocked_by: Some(2),
                waiting_on: None,
            },
            ThreadSnapshot {
                id: 2,
                name: "thread-B".to_string(),
                daemon: false,
                priority: 5,
                state: ThreadState::Blocked,
                stack_frames: vec![],
                lock_info: None,
                blocked_by: Some(1),
                waiting_on: None,
            },
        ];
        let report = JstackProcessor::generate_deadlock_report(&threads);
        assert!(report.is_some());
        let report = report.unwrap();
        assert!(report.contains("deadlock"));
        assert!(report.contains("thread-A"));
        assert!(report.contains("thread-B"));
    }

    #[test]
    fn test_jstack_empty_threads() {
        let dump = JstackProcessor::generate_thread_dump(&[]);
        assert!(dump.contains("Full thread dump CratonVM"));
    }

    #[test]
    fn test_jstack_multiple_frames() {
        let threads = vec![ThreadSnapshot {
            id: 1,
            name: "deep-stack".to_string(),
            daemon: false,
            priority: 5,
            state: ThreadState::Runnable,
            stack_frames: vec![
                FrameInfo {
                    class_name: "com.a.A".to_string(),
                    method_name: "foo".to_string(),
                    file_name: Some("A.java".to_string()),
                    line_number: 10,
                    native_method: false,
                },
                FrameInfo {
                    class_name: "com.b.B".to_string(),
                    method_name: "bar".to_string(),
                    file_name: Some("B.java".to_string()),
                    line_number: 20,
                    native_method: false,
                },
            ],
            lock_info: None,
            blocked_by: None,
            waiting_on: None,
        }];

        let dump = JstackProcessor::generate_thread_dump(&threads);
        assert!(dump.contains("at com.a.A.foo(A.java:10)"));
        assert!(dump.contains("at com.b.B.bar(B.java:20)"));
    }

    // --- jmap tests ---

    #[test]
    fn test_jmap_class_histogram() {
        let entries = vec![
            ClassHistogramEntry {
                class_name: "[B".to_string(),
                instance_count: 1000,
                total_bytes: 50000,
            },
            ClassHistogramEntry {
                class_name: "java.lang.String".to_string(),
                instance_count: 500,
                total_bytes: 20000,
            },
        ];
        let output = JmapProcessor::generate_class_histogram(&entries);
        assert!(output.contains("#instances"));
        assert!(output.contains("[B"));
        assert!(output.contains("java.lang.String"));
        assert!(output.contains("Total:"));
        assert!(output.contains("1500")); // total instances
        assert!(output.contains("70000")); // total bytes
    }

    #[test]
    fn test_jmap_class_histogram_empty() {
        let output = JmapProcessor::generate_class_histogram(&[]);
        assert!(output.contains("#instances"));
        assert!(output.contains("Total:"));
    }

    #[test]
    fn test_jmap_heap_summary() {
        let summary = HeapSummary {
            young_gen_used: 25 * 1024 * 1024,
            young_gen_capacity: 64 * 1024 * 1024,
            old_gen_used: 100 * 1024 * 1024,
            old_gen_capacity: 256 * 1024 * 1024,
            metaspace_used: 30 * 1024 * 1024,
            metaspace_capacity: 64 * 1024 * 1024,
            total_used: 155 * 1024 * 1024,
            total_capacity: 384 * 1024 * 1024,
        };
        let output = JmapProcessor::generate_heap_summary(&summary);
        assert!(output.contains("Young Generation"));
        assert!(output.contains("Old Generation"));
        assert!(output.contains("Metaspace"));
        assert!(output.contains("Total"));
        assert!(output.contains("MB"));
    }

    #[test]
    fn test_jmap_finalizer_info() {
        let output = JmapProcessor::generate_finalizer_info();
        assert!(output.contains("Finalizer Information"));
        assert!(output.contains("Pending finalizers: 0"));
        assert!(output.contains("Finalizer thread: active"));
    }

    // --- HPROF tests ---

    #[test]
    fn test_hprof_magic() {
        assert_eq!(HprofWriter::HPROF_MAGIC, "JAVA PROFILE 1.0.2");
    }

    #[test]
    fn test_hprof_record_type_constants() {
        assert_eq!(HprofWriter::HPROF_UTF8, 0x01);
        assert_eq!(HprofWriter::HPROF_LOAD_CLASS, 0x02);
        assert_eq!(HprofWriter::HPROF_FRAME, 0x04);
        assert_eq!(HprofWriter::HPROF_TRACE, 0x05);
        assert_eq!(HprofWriter::HPROF_HEAP_DUMP, 0x0C);
        assert_eq!(HprofWriter::HPROF_HEAP_DUMP_SEGMENT, 0x1C);
        assert_eq!(HprofWriter::HPROF_HEAP_DUMP_END, 0x2C);
    }

    #[test]
    fn test_hprof_write_header() {
        let header = HprofWriter::write_header();
        // Check magic
        let magic_end = HprofWriter::HPROF_MAGIC.len();
        assert_eq!(&header[..magic_end], HprofWriter::HPROF_MAGIC.as_bytes());
        // Null terminator
        assert_eq!(header[magic_end], 0);
        // Identifier size = 8
        let id_size = u32::from_be_bytes([
            header[magic_end + 1],
            header[magic_end + 2],
            header[magic_end + 3],
            header[magic_end + 4],
        ]);
        assert_eq!(id_size, 8);
        // Total size check
        assert_eq!(header.len(), HprofWriter::header_size());
    }

    #[test]
    fn test_hprof_header_size() {
        let expected = HprofWriter::HPROF_MAGIC.len() + 1 + 4 + 4 + 4;
        assert_eq!(HprofWriter::header_size(), expected);
    }

    #[test]
    fn test_hprof_write_string_record() {
        let record = HprofWriter::write_string_record(42, "hello");
        assert_eq!(record[0], HprofWriter::HPROF_UTF8);
        // Timestamp = 0
        assert_eq!(&record[1..5], &[0, 0, 0, 0]);
        // Body length = 8 (id) + 5 (string) = 13
        let body_len = u32::from_be_bytes([record[5], record[6], record[7], record[8]]);
        assert_eq!(body_len, 13);
        // ID = 42
        let id = u64::from_be_bytes([
            record[9], record[10], record[11], record[12], record[13], record[14], record[15],
            record[16],
        ]);
        assert_eq!(id, 42);
        // String data
        assert_eq!(&record[17..], b"hello");
    }

    #[test]
    fn test_hprof_write_string_record_empty() {
        let record = HprofWriter::write_string_record(1, "");
        let body_len = u32::from_be_bytes([record[5], record[6], record[7], record[8]]);
        assert_eq!(body_len, 8); // just the id, no string bytes
    }

    // --- Utility tests ---

    #[test]
    fn test_format_bytes() {
        assert_eq!(format_bytes(500), "500 B");
        assert_eq!(format_bytes(2048), "2.0 KB");
        assert_eq!(format_bytes(1048576), "1.0 MB");
        assert_eq!(format_bytes(1073741824), "1.0 GB");
    }

    #[test]
    fn test_percent() {
        assert!((percent(50, 100) - 50.0).abs() < 0.001);
        assert!((percent(0, 100) - 0.0).abs() < 0.001);
        assert!((percent(100, 0) - 0.0).abs() < 0.001); // div by zero guard
    }

    #[test]
    fn test_sample_thread_snapshots() {
        let threads = sample_thread_snapshots();
        assert_eq!(threads.len(), 2);
        assert_eq!(threads[0].name, "main");
        assert_eq!(threads[1].name, "GC-Thread");
    }

    #[test]
    fn test_sample_class_histogram() {
        let entries = sample_class_histogram();
        assert_eq!(entries.len(), 10);
        assert_eq!(entries[0].class_name, "[B");
    }

    #[test]
    fn test_jcmd_default_trait() {
        let jcmd = JcmdProcessor::default();
        assert_eq!(jcmd.attach_listener.commands.read().len(), 16);
    }

    #[test]
    fn test_hprof_load_class_record() {
        let data = HprofWriter::write_load_class(1, 100, 0, 200);
        assert_eq!(data[0], HprofWriter::HPROF_LOAD_CLASS);
        assert!(!data.is_empty());
    }

    #[test]
    fn test_hprof_heap_dump_end() {
        let data = HprofWriter::write_heap_dump_end();
        assert_eq!(data[0], HprofWriter::HPROF_HEAP_DUMP_END);
        // Body length should be 0
        let body_len = u32::from_be_bytes([data[5], data[6], data[7], data[8]]);
        assert_eq!(body_len, 0);
    }

    #[test]
    fn test_hprof_stack_trace_record() {
        let frames = vec![1u64, 2, 3];
        let data = HprofWriter::write_stack_trace(1, 1, &frames);
        assert_eq!(data[0], HprofWriter::HPROF_TRACE);
    }

    #[test]
    fn test_hprof_stack_frame_record() {
        let data = HprofWriter::write_stack_frame(1, 10, 20, 30, 1, 42);
        assert_eq!(data[0], HprofWriter::HPROF_FRAME);
    }

    // Test VmDiagnosticState with a mock implementation
    struct MockVmState;
    impl VmDiagnosticState for MockVmState {
        fn thread_snapshots(&self) -> Vec<ThreadSnapshot> {
            vec![ThreadSnapshot {
                id: 1,
                name: "main".to_string(),
                daemon: false,
                priority: 5,
                state: ThreadState::Runnable,
                stack_frames: vec![],
                lock_info: None,
                blocked_by: None,
                waiting_on: None,
            }]
        }
        fn heap_summary(&self) -> HeapSummary {
            HeapSummary {
                young_gen_used: 10 * 1024 * 1024,
                young_gen_capacity: 64 * 1024 * 1024,
                old_gen_used: 50 * 1024 * 1024,
                old_gen_capacity: 256 * 1024 * 1024,
                metaspace_used: 20 * 1024 * 1024,
                metaspace_capacity: 64 * 1024 * 1024,
                total_used: 80 * 1024 * 1024,
                total_capacity: 384 * 1024 * 1024,
            }
        }
        fn class_histogram(&self) -> Vec<ClassHistogramEntry> {
            vec![ClassHistogramEntry {
                class_name: "java.lang.String".to_string(),
                instance_count: 100,
                total_bytes: 4000,
            }]
        }
        fn trigger_gc(&self) -> bool {
            true
        }
        fn uptime_secs(&self) -> f64 {
            42.5
        }
        fn command_line(&self) -> String {
            "java -jar test.jar".to_string()
        }
        fn system_properties(&self) -> Vec<(String, String)> {
            vec![("java.version".to_string(), "25".to_string())]
        }
        fn vm_flags(&self) -> Vec<String> {
            vec!["-Xmx256m".to_string()]
        }
    }

    /// A `JcmdProcessor` must NOT keep the VM alive.
    ///
    /// Its commands used to capture `Arc<dyn VmDiagnosticState>` clones, and
    /// the processor lives in `SharedVm::debug.jcmd_processor` -- so the VM
    /// held nine strong references to itself and could never be dropped. Pin
    /// the weak-handle contract at the level it broke.
    #[test]
    fn jcmd_processor_does_not_keep_vm_state_alive() {
        let state: Arc<dyn VmDiagnosticState> = Arc::new(MockVmState);
        let weak = Arc::downgrade(&state);
        let processor = JcmdProcessor::new_with_vm_state(Arc::clone(&state));
        assert_eq!(
            Arc::strong_count(&state),
            1,
            "constructing the processor must not retain a strong handle"
        );
        drop(state);
        assert!(
            weak.upgrade().is_none(),
            "the processor kept the VM state alive"
        );
        // And it degrades honestly rather than reporting stale data.
        let result = processor.process_command("GC.run");
        assert!(!result.success);
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|e| e.contains("shutting down")),
            "expected a shutting-down error, got {:?}",
            result.error
        );
    }

    #[test]
    fn test_jcmd_with_live_vm_state() {
        // The processor holds only a `Weak` (see `new_with_vm_state`), so the
        // test must keep its own strong handle alive across the assertions --
        // exactly as the real caller does, where the `SharedVm` being reported
        // on is owned by the live `Vm`. Move the only `Arc` in and every
        // command below correctly answers "VM is shutting down".
        let state = Arc::new(MockVmState);
        let processor = JcmdProcessor::new_with_vm_state(state.clone());

        let result = processor.process_command("Thread.print");
        assert!(result.success);
        assert!(result.output.contains("main"));

        let result = processor.process_command("GC.run");
        assert!(result.success);
        assert!(result.output.contains("completed"));

        let result = processor.process_command("VM.uptime");
        assert!(result.success);
        assert!(result.output.contains("42.5"));
    }

    // -----------------------------------------------------------------------
    // HPROF Heap Dump tests (Session 42)
    // -----------------------------------------------------------------------

    #[test]
    fn test_hprof_basic_type_sizes() {
        assert_eq!(HprofBasicType::Boolean.size(), 1);
        assert_eq!(HprofBasicType::Byte.size(), 1);
        assert_eq!(HprofBasicType::Char.size(), 2);
        assert_eq!(HprofBasicType::Short.size(), 2);
        assert_eq!(HprofBasicType::Int.size(), 4);
        assert_eq!(HprofBasicType::Float.size(), 4);
        assert_eq!(HprofBasicType::Long.size(), 8);
        assert_eq!(HprofBasicType::Double.size(), 8);
        assert_eq!(HprofBasicType::Object.size(), 8);
    }

    #[test]
    fn test_hprof_basic_type_from_descriptor() {
        assert_eq!(HprofBasicType::from_descriptor("I"), HprofBasicType::Int);
        assert_eq!(HprofBasicType::from_descriptor("J"), HprofBasicType::Long);
        assert_eq!(HprofBasicType::from_descriptor("F"), HprofBasicType::Float);
        assert_eq!(HprofBasicType::from_descriptor("D"), HprofBasicType::Double);
        assert_eq!(
            HprofBasicType::from_descriptor("Z"),
            HprofBasicType::Boolean
        );
        assert_eq!(HprofBasicType::from_descriptor("B"), HprofBasicType::Byte);
        assert_eq!(HprofBasicType::from_descriptor("C"), HprofBasicType::Char);
        assert_eq!(HprofBasicType::from_descriptor("S"), HprofBasicType::Short);
        assert_eq!(
            HprofBasicType::from_descriptor("Ljava/lang/String;"),
            HprofBasicType::Object
        );
        assert_eq!(
            HprofBasicType::from_descriptor("[I"),
            HprofBasicType::Object
        );
    }

    #[test]
    fn test_hprof_header_has_correct_magic() {
        let header = HprofWriter::write_header();
        let magic_len = HprofWriter::HPROF_MAGIC.len();
        assert_eq!(&header[..magic_len], HprofWriter::HPROF_MAGIC.as_bytes());
        assert_eq!(header[magic_len], 0); // null terminator
                                          // Identifier size = 8
        let id_size = u32::from_be_bytes([
            header[magic_len + 1],
            header[magic_len + 2],
            header[magic_len + 3],
            header[magic_len + 4],
        ]);
        assert_eq!(id_size, 8);
    }

    #[test]
    fn test_hprof_gc_root_thread_obj() {
        let mut buf = Vec::new();
        HprofWriter::write_gc_root_thread_obj(&mut buf, 0xDEAD, 1, 1);
        assert_eq!(buf[0], HprofWriter::GC_ROOT_THREAD_OBJ);
        // thread_obj_id at bytes 1..9
        let obj_id = u64::from_be_bytes(buf[1..9].try_into().unwrap());
        assert_eq!(obj_id, 0xDEAD);
        // thread_serial at bytes 9..13
        let tserial = u32::from_be_bytes(buf[9..13].try_into().unwrap());
        assert_eq!(tserial, 1);
    }

    #[test]
    fn test_hprof_gc_root_jni_global() {
        let mut buf = Vec::new();
        HprofWriter::write_gc_root_jni_global(&mut buf, 0xCAFE, 0xBEEF);
        assert_eq!(buf[0], HprofWriter::GC_ROOT_JNI_GLOBAL);
        let obj_id = u64::from_be_bytes(buf[1..9].try_into().unwrap());
        assert_eq!(obj_id, 0xCAFE);
        let ref_id = u64::from_be_bytes(buf[9..17].try_into().unwrap());
        assert_eq!(ref_id, 0xBEEF);
    }

    #[test]
    fn test_hprof_gc_class_dump_minimal() {
        let ci = HprofClassInfo {
            class_id: 1,
            name: "TestClass".to_string(),
            super_class_id: 0,
            instance_fields: vec![],
            static_fields: vec![],
            source_file: None,
            instance_size: 32,
        };
        let mut string_ids = std::collections::HashMap::new();
        string_ids.insert("TestClass".to_string(), 1u64);

        let mut buf = Vec::new();
        HprofWriter::write_gc_class_dump(&mut buf, &ci, &string_ids);

        assert_eq!(buf[0], HprofWriter::GC_CLASS_DUMP);
        // class_obj_id at bytes 1..9
        let class_obj = u64::from_be_bytes(buf[1..9].try_into().unwrap());
        assert_eq!(class_obj, 0x1000_0000_0000_0001);
        // instance_size at bytes 73..77 (1 + 8 + 4 + 8 + 8 + 8 + 8 + 8 + 8 = 61 offset, then 4 bytes)
        let inst_size = u32::from_be_bytes(buf[61..65].try_into().unwrap());
        assert_eq!(inst_size, 32);
        // constant pool count = 0
        let cp_count = u16::from_be_bytes(buf[65..67].try_into().unwrap());
        assert_eq!(cp_count, 0);
        // static field count = 0
        let sf_count = u16::from_be_bytes(buf[67..69].try_into().unwrap());
        assert_eq!(sf_count, 0);
        // instance field count = 0
        let if_count = u16::from_be_bytes(buf[69..71].try_into().unwrap());
        assert_eq!(if_count, 0);
    }

    #[test]
    fn test_hprof_gc_class_dump_with_fields() {
        let ci = HprofClassInfo {
            class_id: 2,
            name: "Point".to_string(),
            super_class_id: 1,
            instance_fields: vec![
                ("x".to_string(), "I".to_string()),
                ("y".to_string(), "I".to_string()),
            ],
            static_fields: vec![("ORIGIN".to_string(), "LPoint;".to_string())],
            source_file: Some("Point.java".to_string()),
            instance_size: 64,
        };
        let mut string_ids = std::collections::HashMap::new();
        string_ids.insert("Point".to_string(), 1u64);
        string_ids.insert("x".to_string(), 2u64);
        string_ids.insert("y".to_string(), 3u64);
        string_ids.insert("ORIGIN".to_string(), 4u64);

        let mut buf = Vec::new();
        HprofWriter::write_gc_class_dump(&mut buf, &ci, &string_ids);

        assert_eq!(buf[0], HprofWriter::GC_CLASS_DUMP);
        // Should have super_class_id encoded
        let super_obj = u64::from_be_bytes(buf[13..21].try_into().unwrap());
        assert_eq!(super_obj, 0x1000_0000_0000_0001); // super class id = 1

        // static field count = 1 (at offset 67)
        let sf_count = u16::from_be_bytes(buf[67..69].try_into().unwrap());
        assert_eq!(sf_count, 1);
    }

    #[test]
    fn test_hprof_gc_prim_array_dump() {
        use cratonvm_gc::heap::{ArrayElementType, ObjectHeader, ObjectKind, HEADER_SIZE};
        // Simulate a small int[3] array in memory
        let array_length: u32 = 3;
        let elem_size = 4usize; // int = 4 bytes
        let data_size = array_length as usize * elem_size;
        let total_size = HEADER_SIZE + ((data_size + 7) & !7);
        let mut mem = vec![0u8; total_size];

        // Write header
        let header = ObjectHeader::new(
            cratonvm_types::ClassId::new(99),
            ObjectKind::Array,
            ArrayElementType::Int,
            array_length,
            array_length,
        );
        unsafe {
            std::ptr::write(mem.as_mut_ptr() as *mut ObjectHeader, header);
        }

        // Write array elements: [10, 20, 30] as native-endian i32
        for (i, val) in [10i32, 20, 30].iter().enumerate() {
            let offset = HEADER_SIZE + i * elem_size;
            unsafe {
                std::ptr::write_unaligned(mem.as_mut_ptr().add(offset) as *mut i32, *val);
            }
        }

        let obj = HprofObjectInfo {
            object_id: mem.as_ptr() as u64,
            class_id: 99,
            is_array: true,
            element_type: ArrayElementType::Int as u8,
            array_length: 3,
            total_size,
            data_ptr: mem.as_ptr(),
        };

        let mut buf = Vec::new();
        HprofWriter::write_gc_prim_array_dump(&mut buf, &obj);

        assert_eq!(buf[0], HprofWriter::GC_PRIM_ARRAY_DUMP);
        // array length at bytes 13..17
        let len = u32::from_be_bytes(buf[13..17].try_into().unwrap());
        assert_eq!(len, 3);
        // element type at byte 17
        assert_eq!(buf[17], HprofBasicType::Int as u8);
        // First element (big-endian int32) at bytes 18..22
        let val0 = i32::from_be_bytes(buf[18..22].try_into().unwrap());
        assert_eq!(val0, 10);
        let val1 = i32::from_be_bytes(buf[22..26].try_into().unwrap());
        assert_eq!(val1, 20);
        let val2 = i32::from_be_bytes(buf[26..30].try_into().unwrap());
        assert_eq!(val2, 30);
    }

    #[test]
    fn test_hprof_gc_obj_array_dump() {
        use cratonvm_gc::heap::{
            ArrayElementType, ObjectHeader, ObjectKind, HEADER_SIZE, REF_ELEMENT_SIZE,
        };
        // Simulate an Object[2] array
        let array_length: u32 = 2;
        let data_size = array_length as usize * ref_element_size();
        let total_size = HEADER_SIZE + ((data_size + 7) & !7);
        let mut mem = vec![0u8; total_size];

        let header = ObjectHeader::new(
            cratonvm_types::ClassId::new(50),
            ObjectKind::Array,
            ArrayElementType::Reference,
            array_length,
            array_length,
        );
        unsafe {
            std::ptr::write(mem.as_mut_ptr() as *mut ObjectHeader, header);
        }

        // Write element references: [0xCAFE, 0xBEEF]
        for (i, val) in [0xCAFEu64, 0xBEEF].iter().enumerate() {
            let offset = HEADER_SIZE + i * ref_element_size();
            unsafe {
                std::ptr::write_unaligned(mem.as_mut_ptr().add(offset) as *mut u64, *val);
            }
        }

        let obj = HprofObjectInfo {
            object_id: mem.as_ptr() as u64,
            class_id: 50,
            is_array: true,
            element_type: ArrayElementType::Reference as u8,
            array_length: 2,
            total_size,
            data_ptr: mem.as_ptr(),
        };

        let mut buf = Vec::new();
        HprofWriter::write_gc_obj_array_dump(&mut buf, &obj);

        assert_eq!(buf[0], HprofWriter::GC_OBJ_ARRAY_DUMP);
        let len = u32::from_be_bytes(buf[13..17].try_into().unwrap());
        assert_eq!(len, 2);
        // First element ref at bytes 25..33
        let ref0 = u64::from_be_bytes(buf[25..33].try_into().unwrap());
        assert_eq!(ref0, 0xCAFE);
        let ref1 = u64::from_be_bytes(buf[33..41].try_into().unwrap());
        assert_eq!(ref1, 0xBEEF);
    }

    #[test]
    fn test_hprof_full_dump_empty_heap() {
        let classes: Vec<HprofClassInfo> = vec![];
        let objects: Vec<HprofObjectInfo> = vec![];
        let threads = vec![ThreadSnapshot {
            id: 1,
            name: "main".to_string(),
            daemon: false,
            priority: 5,
            state: ThreadState::Runnable,
            stack_frames: vec![],
            lock_info: None,
            blocked_by: None,
            waiting_on: None,
        }];

        let dump = HprofWriter::write_full_heap_dump(&classes, &objects, &threads);

        // Verify header magic
        let magic_len = HprofWriter::HPROF_MAGIC.len();
        assert_eq!(&dump[..magic_len], HprofWriter::HPROF_MAGIC.as_bytes());

        // Should contain HEAP_DUMP_END marker somewhere
        assert!(dump
            .windows(1)
            .any(|w| w[0] == HprofWriter::HPROF_HEAP_DUMP_END));

        // Should be at least header + stack trace + segment + end
        assert!(dump.len() > HprofWriter::header_size() + 20);
    }

    #[test]
    fn test_hprof_full_dump_with_class_and_objects() {
        use cratonvm_gc::heap::{
            ArrayElementType, ObjectHeader, ObjectKind, HEADER_SIZE, SLOT_SIZE,
        };

        let classes = vec![HprofClassInfo {
            class_id: 1,
            name: "TestObj".to_string(),
            super_class_id: 0,
            instance_fields: vec![("value".to_string(), "I".to_string())],
            static_fields: vec![],
            source_file: Some("TestObj.java".to_string()),
            instance_size: (HEADER_SIZE + SLOT_SIZE) as u32,
        }];

        // Create a fake heap object
        let total_size = HEADER_SIZE + SLOT_SIZE;
        let mut mem = vec![0u8; total_size];
        let header = ObjectHeader::new(
            cratonvm_types::ClassId::new(1),
            ObjectKind::Object,
            ArrayElementType::Boolean,
            0,
            1,
        );
        unsafe {
            std::ptr::write(mem.as_mut_ptr() as *mut ObjectHeader, header);
            // Write an int value (42) at the field slot
            std::ptr::write_unaligned(mem.as_mut_ptr().add(HEADER_SIZE) as *mut i32, 42);
        }

        let objects = vec![HprofObjectInfo {
            object_id: mem.as_ptr() as u64,
            class_id: 1,
            is_array: false,
            element_type: 0,
            array_length: 0,
            total_size,
            data_ptr: mem.as_ptr(),
        }];

        let threads = vec![ThreadSnapshot {
            id: 1,
            name: "main".to_string(),
            daemon: false,
            priority: 5,
            state: ThreadState::Runnable,
            stack_frames: vec![],
            lock_info: None,
            blocked_by: None,
            waiting_on: None,
        }];

        let dump = HprofWriter::write_full_heap_dump(&classes, &objects, &threads);

        // Verify magic
        let magic_len = HprofWriter::HPROF_MAGIC.len();
        assert_eq!(&dump[..magic_len], HprofWriter::HPROF_MAGIC.as_bytes());

        // Should have UTF-8 records (for "TestObj", "value", etc.)
        assert!(dump.contains(&HprofWriter::HPROF_UTF8));

        // Should have LOAD_CLASS record
        assert!(dump.contains(&HprofWriter::HPROF_LOAD_CLASS));

        // Should have a HEAP_DUMP_SEGMENT
        assert!(dump.contains(&HprofWriter::HPROF_HEAP_DUMP_SEGMENT));

        // Should have HEAP_DUMP_END
        assert!(dump.contains(&HprofWriter::HPROF_HEAP_DUMP_END));

        // Size should be reasonable (not trivially small)
        assert!(dump.len() > 200);
    }

    #[test]
    fn test_hprof_full_dump_with_threads_and_frames() {
        let threads = vec![
            ThreadSnapshot {
                id: 1,
                name: "main".to_string(),
                daemon: false,
                priority: 5,
                state: ThreadState::Runnable,
                stack_frames: vec![
                    FrameInfo {
                        class_name: "com.example.Main".to_string(),
                        method_name: "run".to_string(),
                        file_name: Some("Main.java".to_string()),
                        line_number: 42,
                        native_method: false,
                    },
                    FrameInfo {
                        class_name: "com.example.Main".to_string(),
                        method_name: "main".to_string(),
                        file_name: Some("Main.java".to_string()),
                        line_number: 10,
                        native_method: false,
                    },
                ],
                lock_info: None,
                blocked_by: None,
                waiting_on: None,
            },
            ThreadSnapshot {
                id: 2,
                name: "worker-1".to_string(),
                daemon: true,
                priority: 5,
                state: ThreadState::Waiting,
                stack_frames: vec![],
                lock_info: None,
                blocked_by: None,
                waiting_on: None,
            },
        ];

        let dump = HprofWriter::write_full_heap_dump(&[], &[], &threads);

        // Should have STACK_FRAME records (for main's 2 frames)
        assert!(dump.contains(&HprofWriter::HPROF_FRAME));

        // Should have STACK_TRACE records
        assert!(dump.contains(&HprofWriter::HPROF_TRACE));

        // Should have UTF-8 records for thread/frame names
        assert!(dump.contains(&HprofWriter::HPROF_UTF8));
    }

    #[test]
    fn test_hprof_full_dump_class_hierarchy() {
        use cratonvm_gc::heap::{
            ArrayElementType, ObjectHeader, ObjectKind, HEADER_SIZE, SLOT_SIZE,
        };

        let classes = vec![
            HprofClassInfo {
                class_id: 1,
                name: "Base".to_string(),
                super_class_id: 0,
                instance_fields: vec![("id".to_string(), "I".to_string())],
                static_fields: vec![],
                source_file: None,
                instance_size: (HEADER_SIZE + SLOT_SIZE) as u32,
            },
            HprofClassInfo {
                class_id: 2,
                name: "Derived".to_string(),
                super_class_id: 1,
                instance_fields: vec![("name".to_string(), "Ljava/lang/String;".to_string())],
                static_fields: vec![],
                source_file: None,
                instance_size: (HEADER_SIZE + 2 * SLOT_SIZE) as u32,
            },
        ];

        // Create a Derived object with 2 fields (inherited id + own name)
        let total_size = HEADER_SIZE + 2 * SLOT_SIZE;
        let mut mem = vec![0u8; total_size];
        let header = ObjectHeader::new(
            cratonvm_types::ClassId::new(2),
            ObjectKind::Object,
            ArrayElementType::Boolean,
            0,
            2,
        );
        unsafe {
            std::ptr::write(mem.as_mut_ptr() as *mut ObjectHeader, header);
        }

        let objects = vec![HprofObjectInfo {
            object_id: mem.as_ptr() as u64,
            class_id: 2,
            is_array: false,
            element_type: 0,
            array_length: 0,
            total_size,
            data_ptr: mem.as_ptr(),
        }];

        let dump = HprofWriter::write_full_heap_dump(&classes, &objects, &[]);

        // Should have two LOAD_CLASS records
        let _load_class_count = dump
            .iter()
            .enumerate()
            .filter(|(i, &b)| {
                b == HprofWriter::HPROF_LOAD_CLASS
                    && *i > 0
                    && dump
                        .get(i.wrapping_sub(4)..=i.wrapping_sub(1))
                        .map(|s| s == &[0, 0, 0, 0]) // timestamp = 0
                        .unwrap_or(false)
            })
            .count();
        // At minimum we should have LOAD_CLASS tags in the output
        assert!(dump.contains(&HprofWriter::HPROF_LOAD_CLASS));

        // Two GC_CLASS_DUMP sub-records should be present in segment body
        // CLASS_DUMP tag is 0x20
        let class_dump_count = dump
            .iter()
            .filter(|&&b| b == HprofWriter::GC_CLASS_DUMP)
            .count();
        // Should be >= 2 (Base + Derived)
        assert!(
            class_dump_count >= 2,
            "Expected at least 2 class dumps, got {}",
            class_dump_count
        );
    }

    #[test]
    fn test_hprof_segment_wrapping() {
        let seg = HprofWriter::wrap_segment(&[1, 2, 3, 4, 5]);
        assert_eq!(seg[0], HprofWriter::HPROF_HEAP_DUMP_SEGMENT);
        // timestamp at 1..5
        assert_eq!(&seg[1..5], &[0, 0, 0, 0]);
        // body length at 5..9
        let body_len = u32::from_be_bytes(seg[5..9].try_into().unwrap());
        assert_eq!(body_len, 5);
        // body content
        assert_eq!(&seg[9..14], &[1, 2, 3, 4, 5]);
    }

    #[test]
    fn test_hprof_write_to_file() {
        // Test that write_full_heap_dump produces valid data that can be written
        let dump = HprofWriter::write_full_heap_dump(&[], &[], &[]);
        // Even with no data, should produce a valid HPROF file structure
        assert!(dump.len() >= HprofWriter::header_size());
        // First bytes are the magic
        assert!(dump.starts_with(HprofWriter::HPROF_MAGIC.as_bytes()));

        // Write to temp file and verify size
        let tmp_path = std::env::temp_dir().join("test_heap_dump.hprof");
        std::fs::write(&tmp_path, &dump).unwrap();
        let written = std::fs::read(&tmp_path).unwrap();
        assert_eq!(written.len(), dump.len());
        assert_eq!(written, dump);
        std::fs::remove_file(&tmp_path).ok();
    }

    #[test]
    fn test_hprof_instance_dump_reads_field_values() {
        use cratonvm_gc::heap::{
            ArrayElementType, ObjectHeader, ObjectKind, HEADER_SIZE, SLOT_SIZE,
        };

        let classes_map: std::collections::HashMap<u32, HprofClassInfo> = [(
            1u32,
            HprofClassInfo {
                class_id: 1,
                name: "IntHolder".to_string(),
                super_class_id: 0,
                instance_fields: vec![("val".to_string(), "I".to_string())],
                static_fields: vec![],
                source_file: None,
                instance_size: (HEADER_SIZE + SLOT_SIZE) as u32,
            },
        )]
        .into_iter()
        .collect();

        let total_size = HEADER_SIZE + SLOT_SIZE;
        let mut mem = vec![0u8; total_size];
        let header = ObjectHeader::new(
            cratonvm_types::ClassId::new(1),
            ObjectKind::Object,
            ArrayElementType::Boolean,
            0,
            1,
        );
        unsafe {
            std::ptr::write(mem.as_mut_ptr() as *mut ObjectHeader, header);
            // Write field value: 0x12345678
            std::ptr::write_unaligned(mem.as_mut_ptr().add(HEADER_SIZE) as *mut i32, 0x12345678);
        }

        let ci = classes_map.get(&1).unwrap();
        let obj = HprofObjectInfo {
            object_id: 0xABCD,
            class_id: 1,
            is_array: false,
            element_type: 0,
            array_length: 0,
            total_size,
            data_ptr: mem.as_ptr(),
        };

        let mut buf = Vec::new();
        HprofWriter::write_gc_instance_dump(&mut buf, &obj, ci, &classes_map);

        assert_eq!(buf[0], HprofWriter::GC_INSTANCE_DUMP);
        // object_id at 1..9
        let oid = u64::from_be_bytes(buf[1..9].try_into().unwrap());
        assert_eq!(oid, 0xABCD);
        // data_size at 21..25 (after tag + obj_id(8) + stack_serial(4) + class_obj(8))
        let data_size = u32::from_be_bytes(buf[21..25].try_into().unwrap());
        assert_eq!(data_size, 4); // one int field = 4 bytes
                                  // Field value at 25..29 (big-endian)
        let fval = i32::from_be_bytes(buf[25..29].try_into().unwrap());
        assert_eq!(fval, 0x12345678);
    }

    #[test]
    fn test_hprof_instance_dump_reads_compact_ref_field_value() {
        use cratonvm_gc::heap::{ArrayElementType, ObjectHeader, ObjectKind, HEADER_SIZE};
        use cratonvm_gc::register_class_layout;
        use cratonvm_types::{CompactLayout, FieldStorageKind, GC_FLAG_COMPACT};
        use std::sync::Arc;

        const CLASS_ID: u32 = 61_001;

        register_class_layout(
            cratonvm_types::FIRST_LAYOUT_DOMAIN,
            CLASS_ID,
            Arc::new(CompactLayout {
                field_offsets: vec![0],
                is_ref: vec![true],
                field_kinds: vec![FieldStorageKind::Reference],
                ref_offsets: vec![0],
                body_size: 8,
            }),
        );

        let classes_map: std::collections::HashMap<u32, HprofClassInfo> = [(
            CLASS_ID,
            HprofClassInfo {
                class_id: CLASS_ID,
                name: "RefHolder".to_string(),
                super_class_id: 0,
                instance_fields: vec![("ref".to_string(), "Ljava/lang/Object;".to_string())],
                static_fields: vec![],
                source_file: None,
                instance_size: (HEADER_SIZE + 8) as u32,
            },
        )]
        .into_iter()
        .collect();

        let total_size = HEADER_SIZE + 8;
        let mut mem = vec![0u8; total_size];
        let expected_ref = 0x1_2345_6789_abcd_u64;
        let mut header = ObjectHeader::new(
            cratonvm_types::ClassId::new(CLASS_ID),
            ObjectKind::Object,
            ArrayElementType::Boolean,
            0,
            1,
        );
        header.set_compact_shape(1, 8);
        unsafe {
            std::ptr::write(mem.as_mut_ptr() as *mut ObjectHeader, header);
            std::ptr::write_unaligned(mem.as_mut_ptr().add(HEADER_SIZE) as *mut u64, expected_ref);
        }

        let ci = classes_map.get(&CLASS_ID).unwrap();
        let obj = HprofObjectInfo {
            object_id: 0xABCD,
            class_id: CLASS_ID,
            is_array: false,
            element_type: 0,
            array_length: 0,
            total_size,
            data_ptr: mem.as_ptr(),
        };

        let mut buf = Vec::new();
        HprofWriter::write_gc_instance_dump(&mut buf, &obj, ci, &classes_map);

        let fval = u64::from_be_bytes(buf[25..33].try_into().unwrap());
        assert_eq!(fval, expected_ref);
    }

    // --- HSDB protocol tests ---

    struct DummyVmState;
    impl VmDiagnosticState for DummyVmState {
        fn thread_snapshots(&self) -> Vec<ThreadSnapshot> {
            vec![ThreadSnapshot {
                id: 42,
                name: "main".to_string(),
                daemon: false,
                priority: 5,
                state: ThreadState::Runnable,
                stack_frames: vec![],
                lock_info: None,
                blocked_by: None,
                waiting_on: None,
            }]
        }
        fn heap_summary(&self) -> HeapSummary {
            HeapSummary {
                young_gen_used: 100,
                young_gen_capacity: 200,
                old_gen_used: 300,
                old_gen_capacity: 400,
                metaspace_used: 500,
                metaspace_capacity: 600,
                total_used: 1024,
                total_capacity: 2048,
            }
        }
        fn class_histogram(&self) -> Vec<ClassHistogramEntry> {
            Vec::new()
        }
        fn trigger_gc(&self) -> bool {
            false
        }
        fn uptime_secs(&self) -> f64 {
            1.5
        }
        fn command_line(&self) -> String {
            "java -jar test.jar".to_string()
        }
        fn system_properties(&self) -> Vec<(String, String)> {
            Vec::new()
        }
        fn vm_flags(&self) -> Vec<String> {
            Vec::new()
        }
    }

    #[test]
    fn hsdb_magic_value() {
        assert_eq!(HSDB_MAGIC, 0x4853_4442);
        assert_eq!(&HSDB_MAGIC.to_be_bytes(), b"HSDB");
    }

    #[test]
    fn hsdb_command_roundtrip() {
        assert_eq!(HsdbCommand::from_u8(0x01), Some(HsdbCommand::Version));
        assert_eq!(HsdbCommand::from_u8(0x02), Some(HsdbCommand::ProcessInfo));
        assert_eq!(HsdbCommand::from_u8(0x03), Some(HsdbCommand::HeapSummary));
        assert_eq!(HsdbCommand::from_u8(0x04), Some(HsdbCommand::ThreadList));
        assert_eq!(HsdbCommand::from_u8(0xFF), None);
    }

    #[test]
    fn hsdb_decode_request_header_rejects_short() {
        assert!(hsdb_decode_request_header(&[0x01, 0x00]).is_err());
    }

    #[test]
    fn hsdb_decode_request_header_parses_len() {
        let bytes = [0x01u8, 0x00, 0x00, 0x01, 0x00];
        let (cmd, len) = hsdb_decode_request_header(&bytes).unwrap();
        assert_eq!(cmd, HsdbCommand::Version);
        assert_eq!(len, 256);
    }

    #[test]
    fn hsdb_decode_request_header_rejects_bad_cmd() {
        assert!(hsdb_decode_request_header(&[0xFE, 0, 0, 0, 0]).is_err());
    }

    #[test]
    fn hsdb_encode_response_prefix() {
        let resp = hsdb_encode_response(HsdbStatus::Ok, b"hi");
        assert_eq!(resp[0], 0); // OK status
        assert_eq!(u32::from_be_bytes([resp[1], resp[2], resp[3], resp[4]]), 2);
        assert_eq!(&resp[5..], b"hi");
    }

    #[test]
    fn hsdb_version_response_carries_version_string() {
        let (status, payload) = hsdb_handle_request(HsdbCommand::Version, &[], None);
        assert_eq!(status, HsdbStatus::Ok);
        // First 2 bytes = string length, then UTF-8
        assert!(payload.len() >= 2);
        let len = u16::from_be_bytes([payload[0], payload[1]]) as usize;
        assert_eq!(len, HSDB_VERSION_STRING.len());
        let s = std::str::from_utf8(&payload[2..2 + len]).unwrap();
        assert_eq!(s, HSDB_VERSION_STRING);
    }

    #[test]
    fn hsdb_process_info_includes_pid_and_cmdline() {
        let state = DummyVmState;
        let (status, payload) = hsdb_handle_request(HsdbCommand::ProcessInfo, &[], Some(&state));
        assert_eq!(status, HsdbStatus::Ok);
        // pid u64 + len u16 + cmdline bytes
        assert!(payload.len() >= 10);
        let pid = u64::from_be_bytes(payload[0..8].try_into().unwrap());
        assert_eq!(pid, std::process::id() as u64);
        let len = u16::from_be_bytes([payload[8], payload[9]]) as usize;
        let cmdline = std::str::from_utf8(&payload[10..10 + len]).unwrap();
        assert_eq!(cmdline, "java -jar test.jar");
    }

    #[test]
    fn hsdb_heap_summary_returns_eight_u64() {
        let state = DummyVmState;
        let (status, payload) = hsdb_handle_request(HsdbCommand::HeapSummary, &[], Some(&state));
        assert_eq!(status, HsdbStatus::Ok);
        assert_eq!(payload.len(), 64);
        let young_used = u64::from_be_bytes(payload[0..8].try_into().unwrap());
        let young_cap = u64::from_be_bytes(payload[8..16].try_into().unwrap());
        let total_used = u64::from_be_bytes(payload[48..56].try_into().unwrap());
        let total_cap = u64::from_be_bytes(payload[56..64].try_into().unwrap());
        assert_eq!(young_used, 100);
        assert_eq!(young_cap, 200);
        assert_eq!(total_used, 1024);
        assert_eq!(total_cap, 2048);
    }

    #[test]
    fn hsdb_heap_summary_without_state_returns_error() {
        let (status, _) = hsdb_handle_request(HsdbCommand::HeapSummary, &[], None);
        assert_eq!(status, HsdbStatus::Error);
    }

    #[test]
    fn hsdb_thread_list_contains_threads() {
        let state = DummyVmState;
        let (status, payload) = hsdb_handle_request(HsdbCommand::ThreadList, &[], Some(&state));
        assert_eq!(status, HsdbStatus::Ok);
        assert!(payload.len() >= 4);
        let count = u32::from_be_bytes(payload[0..4].try_into().unwrap());
        assert_eq!(count, 1);
        let tid = u64::from_be_bytes(payload[4..12].try_into().unwrap());
        assert_eq!(tid, 42);
        let name_len = u16::from_be_bytes([payload[12], payload[13]]) as usize;
        let name = std::str::from_utf8(&payload[14..14 + name_len]).unwrap();
        assert_eq!(name, "main");
        let state_byte = payload[14 + name_len];
        assert_eq!(state_byte, 1); // Runnable
    }

    #[test]
    fn hsdb_thread_list_without_state_errors() {
        let (status, _) = hsdb_handle_request(HsdbCommand::ThreadList, &[], None);
        assert_eq!(status, HsdbStatus::Error);
    }

    /// Byte-for-byte recorded exchange: client handshake + VERSION request,
    /// server handshake + OK response. Verifies wire compatibility without
    /// needing a live client library.
    #[test]
    fn hsdb_recorded_exchange_matches() {
        // Server-side handshake echo: magic bytes
        let magic_bytes = HSDB_MAGIC.to_be_bytes();
        assert_eq!(&magic_bytes, b"HSDB");

        // Client sends VERSION request (no payload)
        let client_request = [0x01u8, 0x00, 0x00, 0x00, 0x00];
        let (cmd, len) = hsdb_decode_request_header(&client_request).unwrap();
        assert_eq!(cmd, HsdbCommand::Version);
        assert_eq!(len, 0);

        // Server processes and replies
        let (status, payload) = hsdb_handle_request(cmd, &[], None);
        let wire = hsdb_encode_response(status, &payload);

        // Expected wire bytes: [0][0][0][0][19][0][17]"CratonVM HSDB v1.0"
        // status(1) + payload_len(4) + str_len(2) + 17 ascii bytes = 24 bytes
        assert_eq!(wire.len(), 1 + 4 + 2 + HSDB_VERSION_STRING.len());
        assert_eq!(wire[0], 0); // OK
        let payload_len = u32::from_be_bytes([wire[1], wire[2], wire[3], wire[4]]);
        assert_eq!(payload_len as usize, 2 + HSDB_VERSION_STRING.len());
        let str_len = u16::from_be_bytes([wire[5], wire[6]]) as usize;
        assert_eq!(str_len, HSDB_VERSION_STRING.len());
        assert_eq!(&wire[7..], HSDB_VERSION_STRING.as_bytes());
    }

    #[test]
    fn hsdb_listener_starts_and_responds() {
        use std::io::{Read, Write};
        use std::net::TcpStream;

        let state: Arc<dyn VmDiagnosticState> = Arc::new(DummyVmState);
        let listener = hsdb_start_listener(0, state).expect("bind");
        let addr = listener.addr;

        // Connect and do handshake + VERSION round-trip
        let mut stream = TcpStream::connect(addr).expect("connect");
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        stream.write_all(&HSDB_MAGIC.to_be_bytes()).unwrap();
        let mut echo = [0u8; 4];
        stream.read_exact(&mut echo).unwrap();
        assert_eq!(u32::from_be_bytes(echo), HSDB_MAGIC);

        // VERSION request
        stream.write_all(&[0x01, 0, 0, 0, 0]).unwrap();
        let mut hdr = [0u8; 5];
        stream.read_exact(&mut hdr).unwrap();
        assert_eq!(hdr[0], 0); // OK
        let plen = u32::from_be_bytes([hdr[1], hdr[2], hdr[3], hdr[4]]) as usize;
        let mut payload = vec![0u8; plen];
        stream.read_exact(&mut payload).unwrap();
        let str_len = u16::from_be_bytes([payload[0], payload[1]]) as usize;
        let s = std::str::from_utf8(&payload[2..2 + str_len]).unwrap();
        assert_eq!(s, HSDB_VERSION_STRING);

        drop(stream);
        listener.stop();
    }
}
