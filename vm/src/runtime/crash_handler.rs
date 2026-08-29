// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Crash recovery and `hs_err_pid` log generation.
//!
//! Installs a Rust panic hook that produces HotSpot-compatible crash log files
//! (`hs_err_pid<pid>.log`) with thread, process, and system information.
//! On Unix, also registers a signal handler for SIGSEGV/SIGBUS/SIGFPE via
//! raw libc so that hardware faults produce the same diagnostic output.
//!
//! # Async-signal-safety discipline (READ BEFORE EDITING)
//!
//! This module has TWO entry points that look superficially similar but live
//! in very different execution contexts:
//!
//! 1. **The Rust panic hook** (`install_crash_handler` -> closure passed to
//!    `std::panic::set_hook`). This runs in *normal* Rust context — the
//!    allocator is fine, mutexes are fine, `format!` / `eprintln!` /
//!    `std::fs::File::create` are all fine. The full report is written here.
//!
//! 2. **The Unix signal handler** (`crash_signal_handler` in
//!    `install_signal_handlers`). This runs in *async-signal context*: it
//!    can be invoked at literally any instruction boundary, including while
//!    the malloc lock or stdio buffer lock is held. POSIX permits only a
//!    tiny whitelist of functions to be called here (`signal(7)` /
//!    `signal-safety(7)`). In particular it is UB / deadlock-prone to call:
//!      - any allocator function (`malloc`, `Box::new`, `String`, `format!`,
//!        `Vec::push`, `to_string`),
//!      - any locking primitive (`Mutex`, `RwLock`, `OnceLock` populated
//!        lazily, the stdio locks behind `eprintln!`/`println!`),
//!      - any `std::fs` API (they call `malloc`),
//!      - any `std::backtrace::Backtrace` (allocates + locks symbol tables).
//!
//! Inside the signal handler we use ONLY:
//!   - direct libc syscalls (`write`, `open`, `close`, `signal`, `raise`,
//!     `getpid`),
//!   - reads of immutable `static` data,
//!   - reads/writes of `AtomicBool` / `AtomicI32` with relaxed semantics,
//!   - pre-allocated static / thread-local byte buffers,
//!   - the `itoa_into_buf` helper, which is allocation-free.
//!
//! If you find yourself wanting to add anything else to the signal path,
//! please STOP and put it on the panic-hook path instead — or skip it.
//! A deadlocked crash handler is worse than a sparser one.

use std::fmt::Write as FmtWrite;
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

// Guard against recursive crashes inside the handler itself.
static CRASH_IN_PROGRESS: AtomicBool = AtomicBool::new(false);

// spring-bug-10 watchpoint: set true once the VEH catches the -2 write to a
// shadow savebase slot, so the JIT arm-helper stops re-arming the HW breakpoint.
pub static SAVEBASE_WATCH_CAUGHT: AtomicBool = AtomicBool::new(false);
// Count of -2 writes the watchpoint VEH has observed (caps log spam).
pub static SAVEBASE_WATCH_HITS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
/// ES-FAIL-FAMILY-20260710 hunt: generalization of the spring-bug-10
/// hardware watchpoint (see `jit::helpers::savebase_watcher`, reused
/// as-is via `arm_generic_heap_watch` below) for "report the writer of
/// ANY value at this address" instead of savebase's specific `-2`
/// sentinel match. Shares the watcher-thread's arm/disarm signaling
/// (`SAVEBASE_WATCH_CAUGHT`) since only one hardware watch is ever
/// active at a time in practice. When set, the VEH's DR0 branch reports
/// + disarms on the FIRST write to the watched address, regardless of
/// the value written.
pub static GENERIC_HEAP_WATCH_MODE: AtomicBool = AtomicBool::new(false);

/// Arm a hardware data-write breakpoint on `addr` (8 bytes) for the
/// CURRENT thread via the existing spring-bug-10 watcher-thread
/// infrastructure, but in "generic" mode: the VEH reports the RIP of
/// whatever instruction writes there NEXT, for any value — not just
/// savebase's `-2` sentinel. One-shot: disarms itself after the first
/// report (shared `SAVEBASE_WATCH_CAUGHT` latch).
#[cfg(windows)]
pub fn arm_generic_heap_watch(addr: usize) {
    GENERIC_HEAP_WATCH_MODE.store(true, Ordering::SeqCst);
    SAVEBASE_WATCH_CAUGHT.store(false, Ordering::SeqCst);
    // SAFETY: `publish` only suspends/resumes/SetThreadContext's a
    // duplicated handle to the CURRENT thread (see its own SAFETY
    // comment); calling it here from a normal native-call context is
    // exactly the same calling convention as its JIT-prologue caller.
    unsafe {
        crate::jit::helpers::savebase_watcher::publish(addr);
    }
}

#[cfg(not(windows))]
pub fn arm_generic_heap_watch(_addr: usize) {}

/// True once the savebase-watchpoint VEH has reported the -2 writer.
pub fn savebase_watch_caught() -> bool {
    SAVEBASE_WATCH_CAUGHT.load(Ordering::Relaxed)
}

// ── VM diagnostic snapshot ─────────────────────────────────────────────────
//
// A crash report that does not say *which* standard library, *which*
// collector, and *whether* a JIT frame was live is not triageable. All three
// facts are cheap to publish once at VM construction (or read live from
// lock-free counters at report time) and each of them has already cost this
// project multi-session debugging when it was missing:
//
//   * **JDK mode.** CratonVM ships two complete, materially different Java
//     class libraries (~5,200 Rust stubs vs ~300 natives over real JDK
//     bytecode). They have different semantics and different bugs. Until
//     `arch-2026-07-26/jdk-mode-determinism.md` the mode was
//     host-detected and printed nowhere; the launcher now prints it, but the
//     hardware-fault path below does NOT go through the launcher's panic hook,
//     so without this snapshot a SIGSEGV/access-violation report still carries
//     no mode. (`jdk-mode-determinism.md` §6.1.)
//   * **GC mode.** The default generational collector silently degrades its
//     young generation to a NON-MOVING sweep whenever a live JIT frame cannot
//     prove a complete rewritable root map. A heap-corruption report that does
//     not say whether the last cycles compacted is nearly undiagnosable, and
//     the degrade was invisible for a long time (see
//     `arch-2026-07-26/moving-young-precise-roots.md`).
//   * **JIT state.** Whether the faulting thread was inside compiled code,
//     and whether an unregistered JIT frame was on the stack, separates a
//     codegen bug from an interpreter/GC bug on the first read.
//
// Publication is one-shot and lock-free; every reader is either a `OnceLock`
// load or a relaxed atomic load, so nothing here can deadlock a crash path
// against a lock the faulting thread already held.

/// [`ACTIVE_JDK_MODE_CODE`] sentinel: `publish_jdk_mode` has not run.
pub const JDK_MODE_CODE_UNKNOWN: u8 = 0;
/// [`ACTIVE_JDK_MODE_CODE`] sentinel: real JDK class files.
pub const JDK_MODE_CODE_REAL: u8 = 1;
/// [`ACTIVE_JDK_MODE_CODE`] sentinel: synthetic Rust class library.
pub const JDK_MODE_CODE_SYNTHETIC: u8 = 2;

/// Async-signal-safe encoding of the active JDK mode.
///
/// The Unix signal handler may not touch `OnceLock`, `String`, or the
/// allocator (see the module doc), so the mode is *also* kept as a plain
/// `AtomicU8` that the handler can turn into one of three `&'static [u8]`
/// literals with no allocation and no locking.
static ACTIVE_JDK_MODE_CODE: std::sync::atomic::AtomicU8 =
    std::sync::atomic::AtomicU8::new(JDK_MODE_CODE_UNKNOWN);

/// The rich form of the active JDK mode, for the two allocating report paths
/// (the Rust panic hook and the Windows vectored exception handler, both of
/// which run in ordinary thread context).
static ACTIVE_JDK_MODE: std::sync::OnceLock<(crate::config::JdkMode, Option<String>)> =
    std::sync::OnceLock::new();

/// The collector selected by `VmConfig::gc_algorithm`, as a stable identifier.
static ACTIVE_GC_ALGORITHM: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();

/// The primordial thread's published Java frame trace.
///
/// Shared (`Arc`) with `JvmThread::frame_trace`, which the interpreter
/// republishes at every blocking/safepoint deposit point. Read with
/// `try_lock` only: a crash may well have happened *while* the faulting
/// thread held this mutex, and blocking there would turn a diagnosable crash
/// into a hang.
#[allow(clippy::type_complexity)]
static PRIMORDIAL_FRAME_TRACE: std::sync::OnceLock<
    std::sync::Arc<parking_lot::Mutex<Vec<cratonvm_native_api::StackTraceEntry>>>,
> = std::sync::OnceLock::new();

/// Publish the class library this process actually booted with.
///
/// Called once from `vm_init` (which owns the `VmConfig`). Idempotent — the
/// first call wins, so a test fixture that builds several `SharedVm`s does not
/// flip the reported mode underneath a later crash.
pub fn publish_jdk_mode(mode: crate::config::JdkMode, java_home: Option<&str>) {
    let code = match mode {
        crate::config::JdkMode::Real => JDK_MODE_CODE_REAL,
        crate::config::JdkMode::Synthetic => JDK_MODE_CODE_SYNTHETIC,
    };
    // First-write-wins on BOTH cells, or they can disagree: `OnceLock::set`
    // already ignores a second publish, so an unconditional `store` here would
    // let the signal-safe byte form report the *second* VM's mode while
    // `jdk_mode_line()` reports the first one's. A crash report that
    // contradicts itself about the class library is worse than one that omits
    // it.
    let _ = ACTIVE_JDK_MODE_CODE.compare_exchange(
        JDK_MODE_CODE_UNKNOWN,
        code,
        Ordering::Relaxed,
        Ordering::Relaxed,
    );
    let _ = ACTIVE_JDK_MODE.set((mode, java_home.map(str::to_owned)));
}

/// The published JDK mode, if `publish_jdk_mode` has run.
pub fn active_jdk_mode() -> Option<crate::config::JdkMode> {
    ACTIVE_JDK_MODE.get().map(|(m, _)| *m)
}

/// One-line "which class library is this" summary, mirroring the launcher's
/// `active_jdk_mode_line()` in `vm-cli/src/main.rs`.
pub fn jdk_mode_line() -> String {
    match ACTIVE_JDK_MODE.get() {
        Some((mode, Some(home))) => format!("jdk mode: {mode} (java.home={home})"),
        Some((mode, None)) => format!("jdk mode: {mode}"),
        None => "jdk mode: <not published — crashed before VM construction>".to_string(),
    }
}

/// Async-signal-safe rendering of the JDK mode.
///
/// Returns a `&'static [u8]` chosen by a single relaxed atomic load — no
/// allocation, no locking, no formatting machinery — so the Unix signal
/// handler may call it. Keep it that way.
pub fn jdk_mode_bytes() -> &'static [u8] {
    match ACTIVE_JDK_MODE_CODE.load(Ordering::Relaxed) {
        JDK_MODE_CODE_REAL => b"real-jdk",
        JDK_MODE_CODE_SYNTHETIC => b"synthetic-jdk",
        _ => b"<unpublished>",
    }
}

/// Publish the selected collector. Called once from `vm_init`.
pub fn publish_gc_algorithm(name: &'static str) {
    let _ = ACTIVE_GC_ALGORITHM.set(name);
}

/// Publish the primordial thread's frame-trace handle. Called once from
/// `Vm::new`, alongside the equivalent `ThreadRegistry::set_frame_trace`.
pub fn publish_primordial_frame_trace(
    trace: std::sync::Arc<parking_lot::Mutex<Vec<cratonvm_native_api::StackTraceEntry>>>,
) {
    let _ = PRIMORDIAL_FRAME_TRACE.set(trace);
}

/// Collector identity plus the *actual* moving/non-moving verdict of the
/// young generation.
///
/// `moving_young_enabled()` is the configured policy; the two counters are
/// what really happened. A report showing `policy=moving` with a nonzero
/// fallback count means the young generation was NOT a copying collector for
/// those cycles — which is exactly the state a heap-corruption or
/// stale-`ObjectRef` bug needs to be read against.
pub fn gc_state_lines() -> Vec<String> {
    gc_state_lines_for(
        ACTIVE_GC_ALGORITHM
            .get()
            .copied()
            .unwrap_or("<unpublished>"),
    )
}

/// The generational young collector's `moving_young_*` state is meaningful in
/// a crash report only when that collector is the one running.
///
/// `record_moving_young_cycle` and `record_moving_young_coverage_fallback` are
/// each called from exactly one place — `gen_heap.rs` — and `moving_young_enabled()`
/// is read nowhere in `g1.rs`. So under `-XX:+UseG1GC` the policy line described
/// a collector that was not running and both counters were zero *by
/// construction*, not by measurement. Printed together they read as "G1's
/// moving young generation relocated nothing", which is a claim the report
/// cannot make.
///
/// That is not hypothetical: it is how
/// fixed-suite-bugs/tomcat/g1-sigsegv-unguarded-callee-jit-frame-FIXED.md got
/// its title. Four G1 crash dumps carried `young-gen policy: moving (Cheney
/// young copy)` beside `last incomplete-coverage reason:
/// innermost-rbp-belongs-to-unguarded-callee`, and the page concluded G1's
/// moving young collector had an unguarded-callee root-coverage gap. The real
/// defect was an unaligned G1 TLAB carve, and root scanning was never involved.
///
/// The incomplete-coverage reason and the two JIT-frame lines below stay for
/// every collector: `g1.rs` does call `mark_moving_young_coverage_incomplete_because`,
/// and `set_unregistered_jit_frame_on_stack` comes from the collector-agnostic
/// `jit::conservative_roots`.
///
/// Split out from [`gc_state_lines`] so the collector can be varied in a test;
/// `ACTIVE_GC_ALGORITHM` is a process-wide `OnceLock`.
pub(crate) fn gc_state_lines_for(collector: &str) -> Vec<String> {
    use cratonvm_gc::gc_quiescence as q;

    let generational = collector == "generational";
    let moving_cycles = q::moving_young_cycle_count();
    let fallbacks = q::moving_young_coverage_fallback_count();
    let policy = if q::moving_young_enabled() {
        "moving (Cheney young copy)"
    } else {
        "non-moving (STW mark-sweep young)"
    };

    let mut lines = vec![format!("gc collector: {collector}")];
    if generational {
        lines.push(format!("gc young-gen policy: {policy}"));
        lines.push(format!(
            "gc young-gen actual: {moving_cycles} moving cycle(s), \
             {fallbacks} cycle(s) diverted to the NON-MOVING sweep"
        ));
    } else {
        lines.push(
            "gc young-gen policy/actual: n/a — those counters are the GENERATIONAL \
             young collector's and are never incremented by this one, so they would \
             read zero whatever happened"
                .to_string(),
        );
    }
    if (generational && fallbacks > 0) || q::moving_young_coverage_incomplete() {
        lines.push(format!(
            "gc young-gen last incomplete-coverage reason: {}",
            q::incomplete_reason::label(q::moving_young_incomplete_reason())
        ));
    }
    // Both flags below are thread-local and cleared at the start of every
    // root-gathering pass, so what they report is *this* (faulting) thread's
    // verdict for the most recent pass — which is the one that matters when
    // the fault is a stale/relocated reference.
    if q::force_non_moving_jit_roots() {
        lines.push(
            "gc young-gen: force-non-moving-jit-roots is ARMED on the faulting thread \
             (a live JIT frame pinned the last cycle to the non-moving sweep)"
                .to_string(),
        );
    }
    if q::unregistered_jit_frame_on_stack() {
        lines.push(
            "gc young-gen: the faulting thread had an UNREGISTERED JIT frame on its \
             native stack in the last root-gathering pass (no precise root map for it)"
                .to_string(),
        );
    }
    lines
}

/// JIT state at the moment of the crash.
///
/// `fault_pc` is the faulting instruction pointer when the caller has one (the
/// hardware-fault path); the panic path passes `None`.
///
/// The quiescence depth is a *process-wide* counter of guarded JIT entries
/// (`JitEntryGuard`), not a per-thread one — the label says so, because
/// "depth=3" would otherwise read as three compiled frames under the faulting
/// thread.
pub fn jit_state_lines(fault_pc: Option<usize>) -> Vec<String> {
    use cratonvm_gc::gc_quiescence as q;

    let depth = q::depth();
    let mut lines = vec![
        format!(
            "jit: guarded compiled frames live process-wide: {} (quiescence depth={depth})",
            if q::is_active() { "YES" } else { "no" }
        ),
        format!(
            "jit: {} compiled code range(s), cache generation {}",
            cratonvm_jit::jit_code_range_count(),
            cratonvm_jit::jit_code_ranges_generation()
        ),
    ];
    if let Some(pc) = fault_pc {
        match cratonvm_jit::lookup_jit_method_name(pc) {
            Some(name) => lines.push(format!("jit: faulting pc is inside compiled method {name}")),
            None if !cratonvm_jit::jit_names_enabled() => lines.push(
                "jit: faulting pc not attributed to a compiled method \
                 (JIT method names are off; re-run with CRATONVM_DBG_JIT_NAMES=1)"
                    .to_string(),
            ),
            None => {
                lines.push("jit: faulting pc is not inside any compiled code range".to_string())
            }
        }
    }
    lines
}

/// The faulting thread's last published Java frames, falling back to the
/// primordial thread's.
///
/// This is a *deposit-point* snapshot, not a live walk: the interpreter
/// republishes it whenever the thread blocks or reaches a safepoint deposit,
/// so for a thread stuck in a native/blocking call it is exact, and for a
/// thread crashing in the middle of a hot bytecode loop it is the last known
/// good position. The report says so rather than implying it is live.
///
/// CR-VXC-1 (`arch-2026-07-26/vm-exec-closeout.md` §5.1): the
/// body below reads one process-wide `OnceLock` published from `Vm::new`, so a
/// fault on a spawned worker or on a virtual-thread carrier used to render the
/// *primordial* thread's frames — never the faulting thread's. The two crash
/// classes that most need a Java stack (virtual-thread resume heap corruption,
/// STW-takeover deadlock) both fault on workers. `faulting_thread_java_stack_lines`
/// reads a per-OS-thread cell published by an RAII guard at platform-worker
/// spawn, at virtual-thread mount, and at JNI attach; it returns `None` on any
/// thread that has nothing published (the primordial thread included), so this
/// is strictly additive and strictly a fallback.
///
/// Never blocks and never panics, on either path: the faulting-thread reader is
/// `try_with` + `try_borrow` + `try_lock` throughout, and `try_lock` failure
/// here is reported, not waited on.
///
/// Both paths **allocate**, so this belongs only to the two allocating report
/// paths (the Rust panic hook via `CrashReport::render`, and the Windows
/// vectored exception handler). It must not be called from the Unix
/// async-signal-safe handler.
pub fn java_stack_lines(max_frames: usize) -> Vec<String> {
    if let Some(lines) = crate::vm::faulting_thread_java_stack_lines(max_frames) {
        return lines;
    }
    let Some(trace) = PRIMORDIAL_FRAME_TRACE.get() else {
        return vec![
            "Java frames: <not published — crashed before the primordial thread was registered>"
                .to_string(),
        ];
    };
    let Some(frames) = trace.try_lock() else {
        return vec![
            "Java frames: <frame-trace mutex was held at crash time; not waiting on it>"
                .to_string(),
        ];
    };
    if frames.is_empty() {
        return vec!["Java frames (primordial thread): <none published yet>".to_string()];
    }
    let mut lines = vec![format!(
        "Java frames (primordial thread, {} frame(s), published at the last \
         blocking/safepoint deposit — may lag the faulting instruction):",
        frames.len()
    )];
    for entry in frames.iter().take(max_frames) {
        let source = entry.source_file.as_deref().unwrap_or("<unknown>");
        lines.push(format!(
            "  at {}.{}({}:{})",
            entry.class_name, entry.method_name, source, entry.line_number
        ));
    }
    if frames.len() > max_frames {
        lines.push(format!("  ... ({} more)", frames.len() - max_frames));
    }
    lines
}

/// The full VM-state block every crash path emits: JDK mode, GC mode, JIT
/// state, and the primordial thread's Java frames.
pub fn vm_diagnostic_lines(fault_pc: Option<usize>) -> Vec<String> {
    let mut lines = vec![jdk_mode_line()];
    lines.extend(gc_state_lines());
    lines.extend(jit_state_lines(fault_pc));
    lines.extend(java_stack_lines(64));
    lines
}

// ── CrashInfo ──────────────────────────────────────────────────────────────

/// Captures the essential facts about a crash / fatal signal.
#[derive(Debug, Clone)]
pub struct CrashInfo {
    /// Signal number (e.g. 11 for SIGSEGV) or 0 for a Rust panic.
    pub signal: i32,
    /// Human-readable signal name (e.g. "SIGSEGV", "RUST_PANIC").
    pub signal_name: String,
    /// Process ID.
    pub pid: u32,
    /// Thread ID of the crashing thread.
    pub tid: u64,
    /// Wall-clock time of the crash.
    pub timestamp: SystemTime,
    /// Name of the crashing thread, if available.
    pub thread_name: Option<String>,
    /// Panic message extracted from `PanicHookInfo`, if any.
    pub panic_message: Option<String>,
    /// Source location of the panic (file:line), if available.
    pub panic_location: Option<String>,
}

impl CrashInfo {
    /// Build a `CrashInfo` from a Rust `PanicHookInfo`.
    pub fn from_panic(info: &std::panic::PanicHookInfo<'_>) -> Self {
        let message = if let Some(s) = info.payload().downcast_ref::<&str>() {
            Some((*s).to_string())
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            Some(s.clone())
        } else {
            Some("(non-string panic payload)".to_string())
        };

        let location = info
            .location()
            .map(|loc| format!("{}:{}", loc.file(), loc.line()));

        let thread_name = std::thread::current().name().map(String::from);

        Self {
            signal: 0,
            signal_name: "RUST_PANIC".to_string(),
            pid: get_pid(),
            tid: get_tid(),
            timestamp: SystemTime::now(),
            thread_name,
            panic_message: message,
            panic_location: location,
        }
    }

    /// Build a `CrashInfo` for a caught signal (Unix).
    #[cfg(unix)]
    pub fn from_signal(sig: i32) -> Self {
        Self {
            signal: sig,
            signal_name: signal_name(sig),
            pid: get_pid(),
            tid: get_tid(),
            timestamp: SystemTime::now(),
            thread_name: std::thread::current().name().map(String::from),
            panic_message: None,
            panic_location: None,
        }
    }
}

// ── CrashReport ────────────────────────────────────────────────────────────

/// Formats a complete HotSpot-compatible crash log from a `CrashInfo`.
pub struct CrashReport<'a> {
    info: &'a CrashInfo,
}

impl<'a> CrashReport<'a> {
    pub fn new(info: &'a CrashInfo) -> Self {
        Self { info }
    }

    /// Render the full crash report as a string.
    pub fn render(&self) -> String {
        let mut buf = String::with_capacity(4096);
        self.write_header(&mut buf);
        self.write_thread_section(&mut buf);
        self.write_vm_section(&mut buf);
        self.write_process_section(&mut buf);
        self.write_system_section(&mut buf);
        buf
    }

    /// Which class library, which collector, and whether the JIT was live —
    /// the three facts that decide how the rest of the report is read.
    /// See the `VM diagnostic snapshot` section above for why each is here.
    fn write_vm_section(&self, buf: &mut String) {
        let _ = writeln!(buf, "---------------  V M  S T A T E  ---------------");
        let _ = writeln!(buf);
        for line in vm_diagnostic_lines(None) {
            let _ = writeln!(buf, "{}", line);
        }
        let _ = writeln!(buf);
    }

    fn write_header(&self, buf: &mut String) {
        let ts = format_timestamp(self.info.timestamp);
        let sig_hex = format!("{:#x}", self.info.signal);

        let _ = writeln!(
            buf,
            "# A fatal error has been detected by the CratonVM Runtime Environment:"
        );
        let _ = writeln!(buf, "#");
        if self.info.signal == 0 {
            // Rust panic
            let msg = self.info.panic_message.as_deref().unwrap_or("unknown");
            let loc = self.info.panic_location.as_deref().unwrap_or("unknown");
            let _ = writeln!(
                buf,
                "#  RUST_PANIC at {}, pid={}, tid={}",
                loc, self.info.pid, self.info.tid
            );
            let _ = writeln!(buf, "#  Message: {}", msg);
        } else {
            let _ = writeln!(
                buf,
                "#  {} ({}) at pc=0x0, pid={}, tid={}",
                self.info.signal_name, sig_hex, self.info.pid, self.info.tid
            );
        }
        let _ = writeln!(buf, "#");
        let _ = writeln!(buf, "# JRE version: CratonVM 25.0");
        // The class library goes in the HEADER, not just the VM section: it is
        // the first thing a triager needs and the first thing a truncated
        // report loses. See `jdk-mode-determinism.md`.
        let _ = writeln!(buf, "# {}", jdk_mode_line());

        // Rust compiler version (baked in at build time).
        let _ = writeln!(buf, "# Rust version: {}", rust_version());

        let _ = writeln!(buf, "# OS: {}", get_os_info());
        let _ = writeln!(buf, "#");
        let _ = writeln!(
            buf,
            "# If you would like to submit a bug report, please include this file."
        );
        let _ = writeln!(buf, "# Crash timestamp: {}", ts);
        let _ = writeln!(buf);
    }

    fn write_thread_section(&self, buf: &mut String) {
        let _ = writeln!(buf, "---------------  T H R E A D  ---------------");
        let _ = writeln!(buf);

        let tname = self.info.thread_name.as_deref().unwrap_or("<unnamed>");
        let _ = writeln!(
            buf,
            "Current thread (0x{:x}): \"{}\" tid=0x{:x}",
            self.info.tid, tname, self.info.tid
        );

        if let Some(ref msg) = self.info.panic_message {
            let _ = writeln!(buf, "Panic message: {}", msg);
        }
        if let Some(ref loc) = self.info.panic_location {
            let _ = writeln!(buf, "Panic location: {}", loc);
        }

        let _ = writeln!(buf);

        // Backtrace — capture at report time.
        let _ = writeln!(buf, "Native frames:");
        let bt = std::backtrace::Backtrace::force_capture();
        let bt_str = bt.to_string();
        // Limit to a reasonable number of frames.
        let mut frame_count = 0;
        for line in bt_str.lines() {
            if frame_count >= 64 {
                let _ = writeln!(buf, "  ... (truncated)");
                break;
            }
            let _ = writeln!(buf, "  {}", line);
            frame_count += 1;
        }

        let _ = writeln!(buf);
    }

    fn write_process_section(&self, buf: &mut String) {
        let _ = writeln!(buf, "---------------  P R O C E S S  ---------------");
        let _ = writeln!(buf);

        let _ = writeln!(buf, "VM state: {}", get_vm_state());
        let _ = writeln!(buf);

        let _ = writeln!(buf, "Heap:");
        let _ = writeln!(buf, "  {}", get_heap_info());
        let _ = writeln!(buf);
    }

    fn write_system_section(&self, buf: &mut String) {
        let _ = writeln!(buf, "---------------  S Y S T E M  ---------------");
        let _ = writeln!(buf);
        let _ = writeln!(buf, "OS:     {}", get_os_info());
        let _ = writeln!(buf, "CPU:    {}", get_cpu_info());
        let _ = writeln!(buf, "Memory: {}", get_memory_info());
        let _ = writeln!(buf);
    }
}

// ── Public API ─────────────────────────────────────────────────────────────

/// Build the full `hs_err_pid` report as a string.
pub fn generate_crash_report(info: &CrashInfo) -> String {
    CrashReport::new(info).render()
}

/// Write the crash report to a file.
pub fn write_crash_report(info: &CrashInfo, path: &Path) -> io::Result<()> {
    let content = generate_crash_report(info);
    let mut f = std::fs::File::create(path)?;
    f.write_all(content.as_bytes())?;
    f.flush()?;
    Ok(())
}

// ── Windows hardware-fault handler (vectored exception handler) ─────────────
//
// On Windows a hardware fault (access violation, illegal instruction, …) is a
// *structured exception*, NOT a Rust panic — so it bypasses the panic hook
// entirely and the process dies via the OS default handler with no diagnostic
// (a bare `STATUS_ACCESS_VIOLATION` exit code and empty stderr). That is why
// the JIT-dispatch SEGV, the sunflow Java2D SEGV, and the fop SEGV were all
// "un-localizable" on this dev box: there was nothing capturing the faulting
// PC or a backtrace.
//
// `install_hardware_fault_handler` registers a vectored exception handler
// (VEH) that, on a genuinely fatal fault, prints the faulting exception code,
// the faulting instruction address, and a symbolized native backtrace to
// stderr (and writes an `hs_err_pid<pid>.log`), THEN returns
// `EXCEPTION_CONTINUE_SEARCH` so normal exception processing continues and the
// process still terminates exactly as before. It does not swallow the fault
// and it does not alter control flow — it is a pure diagnostic tap.
//
// Symbol resolution requires the binary to carry debug info: build with the
// `release-with-debug` profile (`--profile release-with-debug`), which keeps
// line tables and does not strip. A plain `release` build (which strips
// debuginfo) still prints the faulting address, usable with the `.map` file.
#[cfg(windows)]
mod windows_fault {
    use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

    #[repr(C)]
    struct ExceptionRecord {
        exception_code: u32,
        exception_flags: u32,
        exception_record: *mut ExceptionRecord,
        exception_address: *mut core::ffi::c_void,
        number_parameters: u32,
        exception_information: [usize; 15],
    }

    #[repr(C)]
    struct ExceptionPointers {
        exception_record: *mut ExceptionRecord,
        context_record: *mut core::ffi::c_void,
    }

    type VectoredHandler = unsafe extern "system" fn(*mut ExceptionPointers) -> i32;

    extern "system" {
        fn AddVectoredExceptionHandler(
            first: u32,
            handler: VectoredHandler,
        ) -> *mut core::ffi::c_void;
        fn RtlCaptureStackBackTrace(
            frames_to_skip: u32,
            frames_to_capture: u32,
            back_trace: *mut *mut core::ffi::c_void,
            back_trace_hash: *mut u32,
        ) -> u16;
        fn GetModuleHandleW(name: *const u16) -> *mut core::ffi::c_void;
        fn GetCurrentProcess() -> *mut core::ffi::c_void;
        fn GetLastError() -> u32;
        fn GetModuleFileNameW(module: *mut core::ffi::c_void, filename: *mut u16, size: u32)
            -> u32;
        fn VirtualQuery(
            address: *const core::ffi::c_void,
            buffer: *mut MemoryBasicInformation,
            length: usize,
        ) -> usize;
    }

    #[repr(C)]
    struct MemoryBasicInformation {
        base_address: *mut core::ffi::c_void,
        allocation_base: *mut core::ffi::c_void,
        allocation_protect: u32,
        partition_id: u16,
        _pad: u16,
        region_size: usize,
        state: u32,
        protect: u32,
        type_: u32,
    }

    const MEM_COMMIT: u32 = 0x1000;
    // Page protections that permit reads (any of these ⇒ readable).
    const PAGE_READABLE_MASK: u32 = 0x02  /* READONLY */
        | 0x04  /* READWRITE */
        | 0x08  /* WRITECOPY */
        | 0x20  /* EXECUTE_READ */
        | 0x40  /* EXECUTE_READWRITE */
        | 0x80; /* EXECUTE_WRITECOPY */

    /// True if `[addr, addr+len)` lies in a single committed, readable region.
    /// Used to guard raw memory dumps in the crash handler so probing a wild
    /// pointer cannot itself fault and abort the report.
    unsafe fn is_readable(addr: usize, len: usize) -> bool {
        if addr == 0 {
            return false;
        }
        let mut mbi: MemoryBasicInformation = core::mem::zeroed();
        let n = VirtualQuery(
            addr as *const core::ffi::c_void,
            &mut mbi,
            core::mem::size_of::<MemoryBasicInformation>(),
        );
        if n == 0 || mbi.state != MEM_COMMIT || (mbi.protect & PAGE_READABLE_MASK) == 0 {
            return false;
        }
        // Ensure the whole window stays inside this region.
        let region_end = (mbi.base_address as usize).saturating_add(mbi.region_size);
        addr.saturating_add(len) <= region_end
    }

    // dbghelp symbolization of a *known* address. Unlike a full stack walk
    // (std's `Backtrace::force_capture`, which re-faults on the corrupted /
    // JIT-mixed stacks these crashes produce), SymFromAddr only resolves one
    // address and does not touch the broken stack — so it survives.
    #[link(name = "dbghelp")]
    extern "system" {
        fn SymSetOptions(options: u32) -> u32;
        fn SymInitializeW(
            process: *mut core::ffi::c_void,
            search_path: *const u16,
            invade_process: i32,
        ) -> i32;
        fn SymLoadModuleExW(
            process: *mut core::ffi::c_void,
            file: *mut core::ffi::c_void,
            image_name: *const u16,
            module_name: *const u16,
            base_of_dll: u64,
            dll_size: u32,
            data: *mut core::ffi::c_void,
            flags: u32,
        ) -> u64;
        fn SymFromAddr(
            process: *mut core::ffi::c_void,
            address: u64,
            displacement: *mut u64,
            symbol: *mut SymbolInfo,
        ) -> i32;
        fn SymGetLineFromAddrW64(
            process: *mut core::ffi::c_void,
            address: u64,
            displacement: *mut u32,
            line: *mut ImagehlpLineW64,
        ) -> i32;
    }

    // Matches DbgHelp.h IMAGEHLP_LINEW64. repr(C) reproduces the x64 padding
    // (4 bytes after `size_of_struct` before the pointer, 4 after `line_number`).
    #[repr(C)]
    struct ImagehlpLineW64 {
        size_of_struct: u32,
        key: *mut core::ffi::c_void,
        line_number: u32,
        file_name: *mut u16,
        address: u64,
    }

    // Matches DbgHelp.h SYMBOL_INFO (the trailing `name` is a flexible array;
    // callers over-allocate). repr(C) reproduces the field padding exactly.
    #[repr(C)]
    struct SymbolInfo {
        size_of_struct: u32,
        type_index: u32,
        reserved: [u64; 2],
        index: u32,
        size: u32,
        mod_base: u64,
        flags: u32,
        value: u64,
        address: u64,
        register: u32,
        scope: u32,
        tag: u32,
        name_len: u32,
        max_name_len: u32,
        name: [u8; 1],
    }

    const SYMOPT_UNDNAME: u32 = 0x0000_0002;
    const SYMOPT_DEFERRED_LOADS: u32 = 0x0000_0004;
    const SYMOPT_LOAD_LINES: u32 = 0x0000_0010;

    /// Resolve a single instruction address to `function+0xNN` via dbghelp.
    /// `process` must be the value from `GetCurrentProcess()` and dbghelp must
    /// already be initialized (see the handler). Returns None if unresolved.
    unsafe fn symbolize(process: *mut core::ffi::c_void, addr: usize) -> Option<String> {
        if addr == 0 {
            return None;
        }
        // Over-allocate: header + room for a long demangled Rust symbol.
        let mut buf = [0u64; 320];
        let sym = buf.as_mut_ptr() as *mut SymbolInfo;
        (*sym).size_of_struct = core::mem::size_of::<SymbolInfo>() as u32;
        (*sym).max_name_len = 2000;
        let mut disp: u64 = 0;
        if SymFromAddr(process, addr as u64, &mut disp, sym) == 0 {
            return None;
        }
        let name_off = core::mem::offset_of!(SymbolInfo, name);
        let name_ptr = (sym as *const u8).add(name_off);
        let len = ((*sym).name_len as usize).min(2000);
        let bytes = core::slice::from_raw_parts(name_ptr, len);
        let s = String::from_utf8_lossy(bytes).into_owned();
        let mut out = if disp != 0 {
            format!("{}+0x{:X}", s, disp)
        } else {
            s
        };
        // Best-effort file:line via the line table (line-tables-only builds
        // still populate this). Disambiguates fat-LTO inlined frames where
        // `function+0xDISP` alone points into inlined code.
        let mut line: ImagehlpLineW64 = core::mem::zeroed();
        line.size_of_struct = core::mem::size_of::<ImagehlpLineW64>() as u32;
        let mut line_disp: u32 = 0;
        if SymGetLineFromAddrW64(process, addr as u64, &mut line_disp, &mut line) != 0
            && !line.file_name.is_null()
        {
            // file_name is a NUL-terminated wide string.
            let mut n = 0usize;
            while n < 4096 && *line.file_name.add(n) != 0 {
                n += 1;
            }
            let wfile = core::slice::from_raw_parts(line.file_name, n);
            let file = String::from_utf16_lossy(wfile);
            out.push_str(&format!("  [{}:{}]", file, line.line_number));
        }
        Some(out)
    }

    // Win32 NTSTATUS exception codes we treat as fatal hardware faults.
    const EXCEPTION_ACCESS_VIOLATION: u32 = 0xC000_0005;
    const EXCEPTION_IN_PAGE_ERROR: u32 = 0xC000_0006;
    const EXCEPTION_ILLEGAL_INSTRUCTION: u32 = 0xC000_001D;
    const EXCEPTION_PRIV_INSTRUCTION: u32 = 0xC000_0096;
    const EXCEPTION_INT_DIVIDE_BY_ZERO: u32 = 0xC000_0094;
    const EXCEPTION_STACK_OVERFLOW: u32 = 0xC000_00FD;
    // Raised by `__fastfail` / `RaiseFailFastException` — in particular the
    // MSVC `/GS` stack-cookie check calling `__report_gsfailure` on a
    // detected stack-buffer overrun. This is exactly the "Exception code:
    // 0xc0000409" signature Windows Error Reporting shows for these crashes
    // when there is no VEH watching for it: the process fastfails with an
    // empty stderr and no hs_err log. Treating it as fatal here lets us
    // capture the faulting PC/RVA and a raw backtrace before the OS
    // terminates the process (fastfail exceptions still reach registered
    // vectored handlers even though they are non-continuable).
    const EXCEPTION_STACK_BUFFER_OVERRUN: u32 = 0xC000_0409;

    const EXCEPTION_CONTINUE_SEARCH: i32 = 0;

    static INSTALLED: AtomicBool = AtomicBool::new(false);
    // 0 = idle, 1 = handling. A plain bool swap would let a fault *inside* the
    // handler recurse forever; this latches so a second fault falls straight
    // through to the OS.
    static HANDLING: AtomicU8 = AtomicU8::new(0);

    fn code_name(code: u32) -> &'static str {
        match code {
            EXCEPTION_ACCESS_VIOLATION => "EXCEPTION_ACCESS_VIOLATION (SIGSEGV)",
            EXCEPTION_IN_PAGE_ERROR => "EXCEPTION_IN_PAGE_ERROR",
            EXCEPTION_ILLEGAL_INSTRUCTION => "EXCEPTION_ILLEGAL_INSTRUCTION (SIGILL)",
            EXCEPTION_PRIV_INSTRUCTION => "EXCEPTION_PRIV_INSTRUCTION",
            EXCEPTION_INT_DIVIDE_BY_ZERO => "EXCEPTION_INT_DIVIDE_BY_ZERO (SIGFPE)",
            EXCEPTION_STACK_OVERFLOW => "EXCEPTION_STACK_OVERFLOW",
            EXCEPTION_STACK_BUFFER_OVERRUN => "EXCEPTION_STACK_BUFFER_OVERRUN (fastfail)",
            _ => "UNKNOWN",
        }
    }

    fn is_fatal(code: u32) -> bool {
        matches!(
            code,
            EXCEPTION_ACCESS_VIOLATION
                | EXCEPTION_IN_PAGE_ERROR
                | EXCEPTION_ILLEGAL_INSTRUCTION
                | EXCEPTION_PRIV_INSTRUCTION
                | EXCEPTION_INT_DIVIDE_BY_ZERO
                | EXCEPTION_STACK_OVERFLOW
                | EXCEPTION_STACK_BUFFER_OVERRUN
        )
    }

    unsafe extern "system" fn vectored_handler(info: *mut ExceptionPointers) -> i32 {
        if info.is_null() {
            return EXCEPTION_CONTINUE_SEARCH;
        }
        let rec = (*info).exception_record;
        if rec.is_null() {
            return EXCEPTION_CONTINUE_SEARCH;
        }
        let code = (*rec).exception_code;
        // spring-bug-10 watchpoint: a HW data breakpoint fires STATUS_SINGLE_STEP.
        // If DR6 shows one of DR0-3 tripped, this is OUR savebase watchpoint —
        // inspect the value just written; if it is the corrupt 0xFFFF…FFFE, report
        // the writing instruction's RIP (the long-hunted -2 writer) and disarm.
        // Non-(-2) writes (the legitimate push, or other frames reusing the stack
        // qword) are acknowledged silently and the watchpoint stays armed.
        const EXCEPTION_SINGLE_STEP: u32 = 0x8000_0004;
        const EXCEPTION_CONTINUE_EXECUTION: i32 = -1;
        if code == EXCEPTION_SINGLE_STEP {
            let ctx = (*info).context_record as *mut u8;
            if ctx.is_null() {
                return EXCEPTION_CONTINUE_SEARCH;
            }
            let dr6 = core::ptr::read_unaligned(ctx.add(0x68) as *const u64);
            if dr6 & 0xF == 0 {
                return EXCEPTION_CONTINUE_SEARCH; // not our HW breakpoint
            }
            let dr0 = core::ptr::read_unaligned(ctx.add(0x48) as *const u64);
            let rip = core::ptr::read_unaligned(ctx.add(0xF8) as *const u64);
            // ES-FAIL-FAMILY-20260710 hunt: generic mode reports the writer of
            // ANY value at the watched address (armed via
            // `arm_generic_heap_watch`), unlike savebase's `-2`-specific match
            // below. HEAP address, not a stack slot, so there's no
            // "cross-frame" stack-reuse distinction to make — report and
            // disarm on the very first hit.
            if dr0 != 0
                && crate::runtime::crash_handler::GENERIC_HEAP_WATCH_MODE.load(Ordering::Relaxed)
                && !crate::runtime::crash_handler::SAVEBASE_WATCH_CAUGHT
                    .swap(true, Ordering::SeqCst)
            {
                let val = core::ptr::read_unaligned(dr0 as *const u64);
                let mb = GetModuleHandleW(core::ptr::null()) as u64;
                let jit = cratonvm_jit::lookup_jit_method_name(rip as usize)
                    .unwrap_or_else(|| "<none>".to_string());
                let rva = if rip >= mb { rip - mb } else { 0 };
                let wrsp = core::ptr::read_unaligned(ctx.add(0x98) as *const u64);
                let wrbp = core::ptr::read_unaligned(ctx.add(0xA0) as *const u64);
                eprintln!(
                    "[HEAPWATCH] write @0x{dr0:016X} val=0x{val:016X} RIP=0x{rip:016X} (exe+0x{rva:X}) jit={jit} wrsp=0x{wrsp:016X} wrbp=0x{wrbp:016X}\n{}",
                    std::backtrace::Backtrace::force_capture(),
                );
                core::ptr::write_unaligned(ctx.add(0x70) as *mut u64, 0); // disarm DR7
                core::ptr::write_unaligned(ctx.add(0x68) as *mut u64, 0); // clear DR6 (ack)
                return EXCEPTION_CONTINUE_EXECUTION;
            }
            if dr0 != 0 {
                let val = core::ptr::read_unaligned(dr0 as *const u64);
                if val == 0xFFFF_FFFF_FFFF_FFFE {
                    let mb = GetModuleHandleW(core::ptr::null()) as u64;
                    let wrsp = core::ptr::read_unaligned(ctx.add(0x98) as *const u64);
                    let wrbp = core::ptr::read_unaligned(ctx.add(0xA0) as *const u64);
                    let jit = cratonvm_jit::lookup_jit_method_name(rip as usize)
                        .unwrap_or_else(|| "<none>".to_string());
                    let rva = if rip >= mb { rip - mb } else { 0 };
                    // cross-frame: the written addr is ABOVE the writer's whole
                    // frame ⇒ it reaches into a live CALLER (the real corruptor).
                    // Within the writer's frame ⇒ a frame-local write to reused
                    // stack (coincidental, after reset returned).
                    let cross = dr0 > wrsp.wrapping_add(0x800);
                    // Log the first several -2 writes (cap to avoid spam).
                    let n = crate::runtime::crash_handler::SAVEBASE_WATCH_HITS
                        .fetch_add(1, Ordering::Relaxed);
                    if n < 24 {
                        eprintln!(
                            "[WATCH] -2 write @0x{:016X} RIP=0x{:016X} (exe+0x{:X}) jit={} wrsp=0x{:016X} wrbp=0x{:016X} cross_frame={}",
                            dr0, rip, rva, jit, wrsp, wrbp, cross
                        );
                    }
                    // One-shot DISARM only on the live (cross-frame) corruptor.
                    if cross
                        && !crate::runtime::crash_handler::SAVEBASE_WATCH_CAUGHT
                            .swap(true, Ordering::SeqCst)
                    {
                        core::ptr::write_unaligned(ctx.add(0x70) as *mut u64, 0); // disarm DR7
                        eprintln!(
                            "[WATCH] *** LIVE CORRUPTOR: -2 -> savebase 0x{:016X} by exe+0x{:X} (jit={}) ***",
                            dr0, rva, jit
                        );
                    }
                }
            }
            core::ptr::write_unaligned(ctx.add(0x68) as *mut u64, 0); // clear DR6 (ack)
            return EXCEPTION_CONTINUE_EXECUTION;
        }
        // Ignore everything that is not a genuine fatal hardware fault — in
        // particular Rust's own SEH unwind exceptions and debugger
        // breakpoints must pass through untouched.
        if !is_fatal(code) {
            return EXCEPTION_CONTINUE_SEARCH;
        }
        // Re-entry latch. If a fault occurs while we are already reporting one
        // (e.g. dbghelp itself trips), bail to the OS immediately rather than
        // looping. Never reset — one report per process is enough.
        if HANDLING.swap(1, Ordering::SeqCst) != 0 {
            return EXCEPTION_CONTINUE_SEARCH;
        }

        let fault_addr = (*rec).exception_address as usize;
        // For an access violation, exception_information[0] is the access type
        // (0=read, 1=write, 8=execute) and [1] is the faulting data address.
        let (op, data_addr) = if code == EXCEPTION_ACCESS_VIOLATION && (*rec).number_parameters >= 2
        {
            let op = match (*rec).exception_information[0] {
                0 => "read",
                1 => "write",
                8 => "execute",
                _ => "?",
            };
            (op, (*rec).exception_information[1])
        } else {
            ("", 0)
        };

        // We are about to die regardless. A Windows VEH runs in ordinary thread
        // context (not an interrupt), so allocation / stdio are permitted.
        //
        // CRITICAL ORDERING for multi-threaded crashes: several worker threads
        // can fault on the same bug almost simultaneously. The first one in
        // latches HANDLING; a second faulting thread sees the latch, returns
        // CONTINUE_SEARCH, and its unhandled fault TerminateProcess-es us — which
        // can truncate a slow, per-line stderr dump mid-walk. So we:
        //   1. capture the raw stack ONCE (fast, fills the whole array),
        //   2. build the ENTIRE report into a String (no I/O yet),
        //   3. write it to the hs_err file in a SINGLE write_all + flush, then
        //      mirror to stderr in a single write,
        //   4. only THEN attempt fragile dbghelp symbolization.
        // That makes the complete raw frame list survive the race.
        //
        // `writeln!` into the `report` String needs `fmt::Write` in scope; the
        // file/stderr sinks use fully-qualified `std::io::Write` calls so the
        // two traits don't collide.
        use core::fmt::Write as _;

        let module_base = unsafe { GetModuleHandleW(core::ptr::null()) } as usize;
        let mut raw: [*mut core::ffi::c_void; 62] = [core::ptr::null_mut(); 62];
        // EXCEPTION_STACK_OVERFLOW fires on an exhausted stack: walking it
        // (RtlCaptureStackBackTrace / dbghelp) would itself fault. Skip the walk
        // (n=0 disables both the raw loop and the symbolize loop); the faulting
        // PC + RVA from the exception record still pinpoint the recursion site.
        let n = if code == EXCEPTION_STACK_OVERFLOW {
            0
        } else {
            let captured =
                unsafe { RtlCaptureStackBackTrace(0, 62, raw.as_mut_ptr(), core::ptr::null_mut()) };
            captured as usize
        };

        let tname = std::thread::current()
            .name()
            .map(String::from)
            .unwrap_or_else(|| "<unnamed>".to_string());
        let pid = std::process::id();

        let mut report = String::with_capacity(4096);
        let _ = writeln!(report, "\n#");
        let _ = writeln!(
            report,
            "# A fatal error has been detected by the CratonVM Runtime Environment:"
        );
        let _ = writeln!(report, "#");
        let _ = writeln!(
            report,
            "#  {} (0x{:08X}) at pc=0x{:016X}",
            code_name(code),
            code,
            fault_addr
        );
        if !op.is_empty() {
            let _ = writeln!(
                report,
                "#  Faulting access: {} at address 0x{:016X}",
                op, data_addr
            );
        }
        let _ = writeln!(report, "#  pid={} tid={}", pid, super::get_tid());
        let _ = writeln!(report, "#  thread: \"{}\"", tname);
        let _ = writeln!(report, "#  exe module base: 0x{:016X}", module_base);
        if module_base != 0 && fault_addr >= module_base {
            let _ = writeln!(report, "#  faulting RVA: 0x{:X}", fault_addr - module_base);
        }
        // WHICH BUILD these RVAs are RVAs into.
        //
        // Every address in this report is relative to a binary the report does
        // not name, and `CRATONVM_SYMBOLIZE` resolves an RVA against WHATEVER
        // binary it is handed — a near-miss build answers with plausible,
        // entirely wrong function names and four-digit offsets. That is exactly
        // how `known-issues/hibernate/hib-orm-json-xml-function-tests-segfault-g1-zgc-20260820.md`
        // spent two sessions unable to symbolize eight of its own crash logs:
        // by the time anyone looked, nothing recorded which build produced
        // them, and ten surviving binaries all disagreed.
        //
        // `TimeDateStamp` + `SizeOfImage` are what a symbol server keys a PE
        // on, they are already mapped at `module_base`, and reading them is two
        // loads with no allocation and no lock — which is what a fault handler
        // can afford.
        if module_base != 0 {
            // SAFETY: `module_base` is the loaded image base of this process's
            // own exe. A loaded PE always has a readable DOS header at +0 and
            // NT headers at +e_lfanew; the bound below refuses a nonsense
            // `e_lfanew` rather than trusting it.
            unsafe {
                let e_lfanew =
                    core::ptr::read_unaligned((module_base + 0x3C) as *const u32) as usize;
                if e_lfanew > 0 && e_lfanew < 0x1000 {
                    let nt = module_base + e_lfanew;
                    // "PE\0\0"
                    if core::ptr::read_unaligned(nt as *const u32) == 0x0000_4550 {
                        // IMAGE_FILE_HEADER.TimeDateStamp is at NT+4+4;
                        // IMAGE_OPTIONAL_HEADER64.SizeOfImage at NT+24+56.
                        let timestamp = core::ptr::read_unaligned((nt + 8) as *const u32);
                        let size_of_image = core::ptr::read_unaligned((nt + 24 + 56) as *const u32);
                        let _ = writeln!(
                            report,
                            "#  exe build id: timestamp=0x{:08X} size_of_image=0x{:X} \
                             (CRATONVM_SYMBOLIZE is only meaningful against the binary \
                             carrying BOTH of these)",
                            timestamp, size_of_image
                        );
                    }
                }
            }
        }
        if let Some(name) = cratonvm_jit::lookup_jit_method_name(fault_addr) {
            let _ = writeln!(report, "#  faulting JIT method: {}", name);
        }
        // jdk-mode-determinism.md §6.1: this VEH is the ONE crash path that
        // bypasses the launcher's panic hook, so without these lines a
        // hardware-fault report names neither the class library, nor the
        // collector, nor whether the JIT was live — and none of those can be
        // reconstructed after the fact. Every reader here is a `OnceLock` load,
        // a relaxed atomic load, or a `try_lock`, so nothing below can block on
        // a lock the faulting thread was already holding.
        let _ = writeln!(report, "#");
        for line in super::vm_diagnostic_lines(Some(fault_addr)) {
            let _ = writeln!(report, "#  {}", line);
        }
        let _ = writeln!(report, "#");
        let _ = writeln!(report, "Native frames (most recent call first) [raw]:");
        for (i, &a) in raw.iter().take(n).enumerate() {
            let a = a as usize;
            if module_base != 0 && a >= module_base && a < module_base + 0x8000_0000 {
                let _ = writeln!(
                    report,
                    "  {:2}: 0x{:016X}  (exe+0x{:X})",
                    i,
                    a,
                    a - module_base
                );
            } else if let Some(name) = cratonvm_jit::lookup_jit_method_name(a) {
                // spring-bug-11: name the JIT method whose code range contains
                // this return address (requires CRATONVM_DBG_JIT_NAMES=1).
                let _ = writeln!(report, "  {:2}: 0x{:016X}  (jit: {})", i, a, name);
            } else {
                let _ = writeln!(report, "  {:2}: 0x{:016X}  (external/jit)", i, a);
            }
        }
        let _ = writeln!(report, "#");

        // Register dump (x64 CONTEXT). The GPRs pinpoint which value was used
        // as a bad pointer / branch target — for an `execute` fault, the
        // register whose value == the faulting address is the corrupted call
        // target, and rsp/rbp distinguish a corrupted-return-address `ret`
        // from an indirect `call reg`.
        let ctx = (*info).context_record as *const u8;
        if !ctx.is_null() {
            unsafe fn rd(ctx: *const u8, off: usize) -> u64 {
                core::ptr::read_unaligned(ctx.add(off) as *const u64)
            }
            let _ = writeln!(report, "Registers:");
            // x86-64 CONTEXT integer-register byte offsets (winnt.h).
            let names_offs: [(&str, usize); 17] = [
                ("rax", 0x78),
                ("rcx", 0x80),
                ("rdx", 0x88),
                ("rbx", 0x90),
                ("rsp", 0x98),
                ("rbp", 0xA0),
                ("rsi", 0xA8),
                ("rdi", 0xB0),
                ("r8", 0xB8),
                ("r9", 0xC0),
                ("r10", 0xC8),
                ("r11", 0xD0),
                ("r12", 0xD8),
                ("r13", 0xE0),
                ("r14", 0xE8),
                ("r15", 0xF0),
                ("rip", 0xF8),
            ];
            for chunk in names_offs.chunks(4) {
                let mut line = String::new();
                for (name, off) in chunk {
                    let _ = write!(line, "  {:>3}=0x{:016X}", name, unsafe { rd(ctx, *off) });
                }
                let _ = writeln!(report, "{}", line);
            }
            // Decode the faulting DATA address as an indexed load, when it is
            // one: `base + index*scale`, over every register pair and the four
            // x86-64 scales.
            //
            // This is 1156 integer compares in a path that runs once per fatal
            // fault, and it answers the question a raw register dump makes the
            // reader answer by hand. It is not hypothetical: all eight crash
            // logs behind
            // `known-issues/hibernate/hib-orm-json-xml-function-tests-segfault-g1-zgc-20260820.md`
            // satisfy `fault == r10 + rax*4` — a jump-table load with a
            // garbage index — and nothing said so, so two sessions read the
            // scattered addresses as random corruption instead.
            //
            // A `*4`/`*8` hit whose index register holds a LARGE value is the
            // signature to look for: an `int[]`/jump-table index or an enum
            // discriminant that should have been small, read out of memory that
            // did not hold one.
            if !op.is_empty() && data_addr != 0 {
                let regs: Vec<(&str, u64)> = names_offs
                    .iter()
                    .map(|(n, o)| (*n, unsafe { rd(ctx, *o) }))
                    .collect();
                if let Some(line) = super::decode_indexed_load(&regs, data_addr) {
                    let _ = writeln!(report, "{line}");
                }
            }
        }

        // Name the JIT method whose body made the bad call, if recorded
        // (requires CRATONVM_DBG_JIT_PUTFIELD=1 to populate the thread-local).
        let callee = crate::jit::helpers::current_jit_callee_for_crash();
        if !callee.is_empty() {
            let _ = writeln!(report, "current_jit_callee = {}", callee);
        }

        // Name the innermost native (Rust) callback active on THIS (faulting)
        // thread, if the lightweight per-thread tracker is armed
        // (CRATONVM_TRACK_NATIVE=1). For the HIB-CV-37 lambda/native GC-stranding
        // class this is the stream/collection intrinsic that was driving a lambda
        // while holding a now-stale Rust-local ObjectRef — i.e. the native that
        // needs `pin_native_root`/`read_native_pin`. Print both the absolute fn
        // pointer and its exe-relative RVA so it can be symbolized offline with
        // `CRATONVM_SYMBOLIZE` against the SAME binary (closure symbols survive in
        // the profsym/debug build). Best-effort `name_of` too (resolves only if
        // the registry's ring names were flushed). Zero cost when disabled.
        let cur_native = cratonvm_native_api::native_ring::innermost_native_cb();
        if cur_native != 0 {
            let name = cratonvm_native_api::native_ring::name_of(cur_native)
                .unwrap_or_else(|| "<unflushed; symbolize the RVA>".to_string());
            if module_base != 0 && cur_native >= module_base {
                let _ = writeln!(
                    report,
                    "current_native (faulting thread) = exe+0x{:X}  {}",
                    cur_native - module_base,
                    name
                );
            } else {
                let _ = writeln!(
                    report,
                    "current_native (faulting thread) = 0x{:016X}  {}",
                    cur_native, name
                );
            }
        }

        // Disassembly aid: dump the instruction bytes immediately *before* each
        // JIT-region return address (most-recent first). A return address points
        // just past the CALL that pushed it, so the preceding ~32 bytes contain
        // the faulting indirect call and whatever set up its target register —
        // exactly what is needed to see why R10 held a bad pointer.
        // VirtualQuery-guarded so a wild RA can't re-fault the handler.
        let dump_before = |report: &mut String, ra: usize, label: &str| {
            const PRE: usize = 32;
            if ra <= PRE {
                return;
            }
            let start = ra - PRE;
            if !unsafe { is_readable(start, PRE) } {
                let _ = writeln!(report, "  [{}] 0x{:016X}: <unreadable>", label, ra);
                return;
            }
            let bytes = unsafe { core::slice::from_raw_parts(start as *const u8, PRE) };
            let mut hex = String::new();
            for b in bytes {
                let _ = write!(hex, "{:02X} ", b);
            }
            let _ = writeln!(
                report,
                "  [{}] bytes [RA-0x{:X}..RA] @0x{:016X}:\n    {}",
                label, PRE, start, hex
            );
        };
        if n >= 2 {
            let _ = writeln!(report, "Code bytes preceding JIT return addresses:");
            for (i, &a) in raw.iter().take(n).enumerate().skip(1).take(3) {
                let a = a as usize;
                if !(module_base != 0 && a >= module_base && a < module_base + 0x8000_0000) {
                    dump_before(&mut report, a, &format!("frame{}", i));
                }
            }
        }

        // Dump the memory the bad call dereferenced through R10 (the MIC/PIC
        // slot the dispatch used `CALL [R10+8]` on): [R10-0x10 .. R10+0x20].
        if !ctx.is_null() {
            let r10 = unsafe { core::ptr::read_unaligned(ctx.add(0xC8) as *const u64) } as usize;
            let win_start = r10.wrapping_sub(0x10);
            if unsafe { is_readable(win_start, 0x30) } {
                let qs = unsafe { core::slice::from_raw_parts(win_start as *const u64, 6) };
                let _ = writeln!(report, "Memory around R10 (0x{:016X}):", r10);
                for (j, q) in qs.iter().enumerate() {
                    let off = -0x10i64 + (j as i64) * 8;
                    let _ = writeln!(report, "    [R10{:+#x}] = 0x{:016X}", off, q);
                }
            } else {
                let _ = writeln!(report, "Memory around R10 (0x{:016X}): <unreadable>", r10);
            }
        }

        // spring-bug-10: dump the live ShadowStack fields (top@+0x1B8,
        // end@+0x1C0, base@+0x1C8) via R10 (= the cached thread ptr the faulting
        // reload reloaded), so a corrupt `top` vs a corrupt savebase slot can be
        // told apart.
        if !ctx.is_null() {
            let r10 = unsafe { core::ptr::read_unaligned(ctx.add(0xC8) as *const u64) } as usize;
            let ss = r10.wrapping_add(0x1B8);
            if r10 != 0 && unsafe { is_readable(ss, 0x18) } {
                let qs = unsafe { core::slice::from_raw_parts(ss as *const u64, 3) };
                let _ = writeln!(
                    report,
                    "ShadowStack @ R10+0x1B8: top=0x{:016X} end=0x{:016X} base=0x{:016X}",
                    qs[0], qs[1], qs[2]
                );
            }
        }

        // spring-bug-10: dump the current JIT frame's slots [rbp-0x40 .. rbp]
        // so the shadow-stack slots (thread @ rbp-thread_off, savetop, savebase)
        // and the locals are visible at the fault — to see whether savebase was
        // ever written or got corrupted by the safepoint call/GC.
        if !ctx.is_null() {
            let rbp = unsafe { core::ptr::read_unaligned(ctx.add(0xA0) as *const u64) } as usize;
            let win_start = rbp.wrapping_sub(0x40);
            if rbp != 0 && unsafe { is_readable(win_start, 0x48) } {
                let qs = unsafe { core::slice::from_raw_parts(win_start as *const u64, 9) };
                let _ = writeln!(report, "Frame slots around RBP (0x{:016X}):", rbp);
                for (j, q) in qs.iter().enumerate() {
                    let off = -0x40i64 + (j as i64) * 8;
                    let _ = writeln!(report, "    [RBP{:+#06x}] = 0x{:016X}", off, q);
                }
            }
        }

        let _ = writeln!(report, "#");
        let _ = writeln!(
            report,
            "# Symbolize offline with the SAME binary:\n\
             #   CRATONVM_SYMBOLIZE=<comma-separated exe+0x RVAs> cratonvm X"
        );

        // (2->3) Single-shot file write FIRST (most reliable sink).
        if let Ok(mut f) =
            std::fs::File::create(std::path::PathBuf::from(format!("hs_err_pid{}.log", pid)))
        {
            let _ = std::io::Write::write_all(&mut f, report.as_bytes());
            let _ = std::io::Write::flush(&mut f);
        }
        // Mirror to stderr in one write.
        {
            let mut err = std::io::stderr().lock();
            let _ = std::io::Write::write_all(&mut err, report.as_bytes());
            let _ = std::io::Write::flush(&mut err);
        }

        // Best-effort: dump the last-256-dispatch ring on a stack overflow
        // specifically. The raw/symbolized native stack walks above are
        // skipped for EXCEPTION_STACK_OVERFLOW (walking an exhausted stack
        // would itself fault — see the `n = 0` branch above), so the
        // faulting PC/RVA is otherwise the ONLY clue to what was recursing.
        // `dispatch_trace` records bytecode-method entries and native
        // dispatches independently of the native call stack, so it survives
        // even when the stack itself is unwalkable. Only useful with
        // `CRATONVM_DBG_LETSGO=1` set (the ring is empty otherwise, but the
        // dump header still confirms that plainly rather than leaving the
        // reader to guess). Same best-effort risk profile as the
        // symbolization step below: if this itself re-faults on the
        // depleted guard page, the primary report above already persisted.
        if code == EXCEPTION_STACK_OVERFLOW {
            crate::dispatch_trace::dump_to_stderr_unconditional("stack-overflow");
        }

        // (4) Best-effort in-process symbolization. dbghelp can itself re-fault
        // on a wrecked thread state; the raw report above is already persisted,
        // so a death here loses nothing actionable.
        let process = unsafe { GetCurrentProcess() };
        unsafe {
            SymSetOptions(SYMOPT_UNDNAME | SYMOPT_DEFERRED_LOADS | SYMOPT_LOAD_LINES);
            SymInitializeW(process, core::ptr::null(), 0);
        }
        let mut sym_report = String::with_capacity(2048);
        let _ = writeln!(sym_report, "Native frames [symbolized, best-effort]:");
        for (i, &a) in raw.iter().take(n).enumerate() {
            if let Some(name) = unsafe { symbolize(process, a as usize) } {
                let _ = writeln!(sym_report, "  {:2}: {}", i, name);
            }
        }
        {
            let mut err = std::io::stderr().lock();
            let _ = std::io::Write::write_all(&mut err, sym_report.as_bytes());
            let _ = std::io::Write::flush(&mut err);
        }

        // Continue searching — let the OS finish terminating the process the
        // same way it would have without us. We only added the diagnostic.
        EXCEPTION_CONTINUE_SEARCH
    }

    /// Register the vectored exception handler. Idempotent.
    pub fn install() {
        if INSTALLED.swap(true, Ordering::SeqCst) {
            return;
        }
        // `first = 1` -> run before any frame-based (SEH) handlers, so we see
        // the fault even if some inner frame would otherwise swallow it. We
        // still return CONTINUE_SEARCH, so a legitimate handler downstream is
        // unaffected.
        unsafe {
            AddVectoredExceptionHandler(1, vectored_handler);
        }
    }

    /// Symbolize exe-relative RVAs in a NORMAL (non-crash) context, where
    /// dbghelp is reliable — used by the `CRATONVM_SYMBOLIZE` startup hook to
    /// resolve the raw addresses the VEH prints for a multi-threaded crash
    /// (whose racy teardown truncates in-handler symbolization). Resolves each
    /// `module_base + rva` against the running exe's own symbols/PDB.
    pub fn symbolize_rvas(rvas: &[usize]) -> Vec<(usize, Option<String>)> {
        let module_base = unsafe { GetModuleHandleW(core::ptr::null()) } as usize;
        let process = unsafe { GetCurrentProcess() };
        let verbose =
            cratonvm_types::flags::runtime_var("CRATONVM_SYMBOLIZE_DBG").as_deref() == Ok("1");
        // Build a search path = the exe's own directory, so dbghelp finds the
        // co-located cratonvm.pdb regardless of cwd / _NT_SYMBOL_PATH.
        let mut exe_path = [0u16; 1024];
        let exe_len =
            unsafe { GetModuleFileNameW(core::ptr::null_mut(), exe_path.as_mut_ptr(), 1024) }
                as usize;
        // Strip the file name to get the directory (find last '\\').
        let dir_end = exe_path[..exe_len]
            .iter()
            .rposition(|&c| c == b'\\' as u16)
            .unwrap_or(exe_len);
        let mut search_path: Vec<u16> = exe_path[..dir_end].to_vec();
        search_path.push(0);
        unsafe {
            SymSetOptions(SYMOPT_UNDNAME | SYMOPT_DEFERRED_LOADS | SYMOPT_LOAD_LINES);
            let ok = SymInitializeW(process, search_path.as_ptr(), 1);
            if verbose {
                let dir = String::from_utf16_lossy(&exe_path[..dir_end]);
                eprintln!(
                    "[sym] SymInitialize ok={} base=0x{:X} dir={} err={}",
                    ok,
                    module_base,
                    dir,
                    GetLastError()
                );
            }
            // Explicitly load the exe's own module + PDB so SymFromAddr can
            // resolve addresses inside it. invade=FALSE above keeps init cheap;
            // we load just the main module here. image_name = full exe path.
            let mut path_buf = [0u16; 1024];
            let len = GetModuleFileNameW(
                core::ptr::null_mut(),
                path_buf.as_mut_ptr(),
                path_buf.len() as u32,
            );
            let loaded = if len > 0 {
                // DllSize=0 leaves SymFromAddr unable to tell whether a
                // queried address falls within this module's range (it
                // reliably fails with ERROR_INVALID_ADDRESS/487 even though
                // SymLoadModuleExW itself reports success) — pass a
                // generous over-estimate; SymLoadModuleExW only needs the
                // registered range to COVER real addresses, not match the
                // image size exactly.
                const DLL_SIZE_OVERESTIMATE: u32 = 0x8000000; // 128 MiB
                SymLoadModuleExW(
                    process,
                    core::ptr::null_mut(),
                    path_buf.as_ptr(),
                    core::ptr::null(),
                    module_base as u64,
                    DLL_SIZE_OVERESTIMATE,
                    core::ptr::null_mut(),
                    0,
                )
            } else {
                0
            };
            if verbose {
                eprintln!(
                    "[sym] SymLoadModuleExW -> base=0x{:X} err={}",
                    loaded,
                    GetLastError()
                );
            }
        }
        rvas.iter()
            .map(|&rva| {
                let abs = module_base.wrapping_add(rva);
                let r = unsafe { symbolize(process, abs) };
                if verbose && r.is_none() {
                    eprintln!("[sym] SymFromAddr 0x{:X} failed err={}", abs, unsafe {
                        GetLastError()
                    });
                }
                (rva, r)
            })
            .collect()
    }
}

/// Symbolize a list of exe-relative RVAs against the running binary's symbols.
/// Windows-only diagnostic helper for the `CRATONVM_SYMBOLIZE` startup hook;
/// returns `(rva, Some("function+0xNN"))` per input. Empty on non-Windows.
pub fn symbolize_rvas(_rvas: &[usize]) -> Vec<(usize, Option<String>)> {
    #[cfg(windows)]
    {
        windows_fault::symbolize_rvas(_rvas)
    }
    #[cfg(not(windows))]
    {
        Vec::new()
    }
}

/// Describe the faulting data address as `base + index*scale` over the fault's
/// own registers, if any pair explains it.
///
/// A register dump alone leaves this to the reader, and the reader does not do
/// it: all eight crash logs behind
/// `docs/known-issues/hibernate/hib-orm-json-xml-function-tests-segfault-g1-zgc-20260820.md`
/// satisfy `fault == r10 + rax*4` — an unchecked jump-table load with a garbage
/// index — and because nothing said so, two sessions read the wildly scattered
/// fault addresses as random corruption and looked for an environmental cause.
///
/// A `*4` or `*8` hit whose INDEX register holds a large value is the signature
/// worth acting on: an array index, or an enum discriminant that should have
/// been small, read out of memory that did not hold one.
///
/// `base == 0` and `index == 0` are skipped because they make every address
/// trivially decodable, and `base == data_addr` because a zero index is not an
/// explanation. At most six matches are listed; more than that means the
/// registers are too degenerate for the decode to say anything.
pub(crate) fn decode_indexed_load(regs: &[(&str, u64)], data_addr: usize) -> Option<String> {
    use std::fmt::Write as _;
    let mut decoded = String::new();
    let hits = for_each_indexed_load_match(regs, data_addr, |bname, iname, scale, nth| {
        if nth < 6 {
            let _ = write!(decoded, " [{bname}+{iname}*{scale}]");
        }
    });
    if hits == 0 {
        return None;
    }
    Some(format!(
        "Faulting address decodes as an indexed load:{}{}",
        decoded,
        if hits > 6 { " ..." } else { "" }
    ))
}

/// The search itself, separated from how the answer is rendered.
///
/// Two reporters need it and they cannot share a formatter: the Windows path
/// builds a `String`, and the Linux path runs inside a signal handler, where
/// allocating is not allowed. Factoring the loop rather than writing it twice
/// is what keeps the two reports saying the same thing about the same fault —
/// Linux carried no decode at all until 2026-08-23, which is why the two
/// Tomcat SIGSEGVs of 2026-08-22 arrived as bare addresses.
///
/// `on_match` is handed the base name, the index name, the scale, and the
/// 0-based match ordinal; the return value is the TOTAL number of matches,
/// which can exceed the number the caller chose to render.
pub(crate) fn for_each_indexed_load_match<F>(
    regs: &[(&str, u64)],
    data_addr: usize,
    mut on_match: F,
) -> usize
where
    F: FnMut(&str, &str, usize, usize),
{
    if data_addr == 0 {
        return 0;
    }
    let mut hits = 0usize;
    for (bname, bval) in regs {
        let base = *bval as usize;
        if base == 0 || base == data_addr {
            continue;
        }
        for (iname, ival) in regs {
            let idx = *ival as usize;
            if idx == 0 {
                continue;
            }
            for scale in [1usize, 2, 4, 8] {
                if base.wrapping_add(idx.wrapping_mul(scale)) == data_addr {
                    on_match(bname, iname, scale, hits);
                    hits += 1;
                }
            }
        }
    }
    hits
}

#[cfg(test)]
mod indexed_load_decode_tests {
    use super::decode_indexed_load;

    /// The real thing: `hs_err_pid3512`'s own registers and fault address.
    /// If this stops matching, the decoder has stopped being able to fire on
    /// the crash it was written for.
    #[test]
    fn decodes_the_json_xml_segv_jump_table_load() {
        let regs = [
            ("rax", 0x0000_0000_EAF8_2DA0u64),
            ("rcx", 0x0000_008D_769D_2C88),
            ("rbx", 0x5B),
            ("rbp", 6),
            ("r10", 0x0000_7FF6_35D7_9014),
            ("r11", 0),
        ];
        let line = decode_indexed_load(&regs, 0x0000_7FF9_E1B8_4694).expect("must decode");
        assert!(line.contains("[r10+rax*4]"), "got: {line}");
    }

    #[test]
    fn declines_when_nothing_explains_the_address() {
        let regs = [("rax", 1u64), ("rbx", 2), ("rcx", 3)];
        assert!(decode_indexed_load(&regs, 0xDEAD_BEEF).is_none());
    }

    /// A zero index would make every base "explain" the address.
    #[test]
    fn zero_index_and_zero_base_are_not_explanations() {
        let regs = [("rax", 0u64), ("r10", 0x1000)];
        assert!(decode_indexed_load(&regs, 0x1000).is_none());
    }
}

/// Install a process-wide hardware-fault diagnostic handler.
///
/// On Windows this registers a vectored exception handler that prints the
/// faulting PC + a symbolized backtrace on an access violation / illegal
/// instruction / etc., then lets the process terminate normally (see
/// [`windows_fault`]). On other platforms it is currently a no-op (the Unix
/// signal path lives in [`install_crash_handler`]).
///
/// Unlike [`install_crash_handler`], this does NOT touch the Rust panic hook,
/// so it can be called alongside a caller that installs its own panic hook
/// (as `vm-cli` does).
pub fn install_hardware_fault_handler() {
    #[cfg(windows)]
    windows_fault::install();
    // The Unix equivalent of the Windows vectored exception handler is the
    // SIGSEGV/SIGBUS/SIGILL `sigaction` set below. It used to be reachable
    // only from `install_crash_handler`, which `vm-cli` never calls (it
    // installs its own panic hook) — so on Linux a hardware fault produced no
    // stderr banner and no `hs_err_pid<pid>.log` at all. Installing signals
    // here does NOT touch the panic hook, so it composes with that caller.
    #[cfg(unix)]
    install_signal_handlers();
}

/// Install the crash handler (Rust panic hook + platform signal handlers).
///
/// Should be called once during VM startup. It is safe to call multiple times;
/// only the first invocation actually installs the hook.
pub fn install_crash_handler() {
    // Chain on top of the existing panic hook so that default error output
    // (and any previously installed hooks) still fires.
    let prev = std::panic::take_hook();

    std::panic::set_hook(Box::new(move |panic_info| {
        // The native allocators' heap-exhaustion unwind is a HANDLED condition
        // (`safe_native_call` turns it into a catchable
        // `java.lang.OutOfMemoryError`), not a crash. Writing an
        // `hs_err_pid<pid>.log` for it would be wrong on its own AND would latch
        // the one-report-per-process guard below, silently suppressing the
        // report for a later real crash. See `crate::runtime::native_oom`.
        if crate::runtime::native_oom::is_native_oom_panic(panic_info) {
            return;
        }
        // Prevent recursive entry if the handler itself panics.
        if CRASH_IN_PROGRESS
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            // SECURITY FIX (V17): A report is already being (or has been)
            // written by the first panic — do NOT write a second one (that
            // race produced interleaved/garbled hs_err_pid<pid>.log output).
            // Still chain to the previous hook so this panic's default output
            // fires and the process aborts exactly as before; we just skip the
            // report-writing section.
            prev(panic_info);
            return;
        }

        let crash = CrashInfo::from_panic(panic_info);
        let pid = crash.pid;
        let filename = format!("hs_err_pid{}.log", pid);
        let path = std::path::PathBuf::from(&filename);

        match write_crash_report(&crash, &path) {
            Ok(()) => {
                eprintln!("#");
                eprintln!("# A fatal error has been detected by the CratonVM Runtime Environment:");
                eprintln!("#");
                if let Some(ref msg) = crash.panic_message {
                    eprintln!("#  {}", msg);
                }
                eprintln!("#");
                eprintln!("# An error report file with more information is saved as:");
                eprintln!("#  {}", path.display());
                eprintln!("#");
            }
            Err(e) => {
                eprintln!(
                    "# CratonVM crash handler: failed to write {}: {}",
                    filename, e
                );
            }
        }

        // Invoke the previous hook so the default Rust output is preserved.
        prev(panic_info);

        // SECURITY FIX (V17): Do NOT reset CRASH_IN_PROGRESS here. The previous
        // code stored `false`, which let two near-simultaneous panics both pass
        // the CAS guard above and race to write the same hs_err_pid<pid>.log,
        // producing interleaved output. We now latch the guard for the life of
        // the process — matching the Windows VEH `HANDLING` latch (one report
        // per process). Any subsequent/concurrent panic short-circuits at the
        // guard above: it still chains to `prev` (so the process aborts) but
        // does not write a second, interleaved report.
    }));

    // Platform-specific signal installation (Unix only).
    #[cfg(unix)]
    install_signal_handlers();
}

// ── Async-signal-safe primitives ───────────────────────────────────────────
//
// Everything in this section MUST be callable from a signal handler. See the
// module-level docstring for the discipline.

/// Format a non-negative integer into `buf` in decimal, allocation-free.
///
/// Returns the number of bytes written. The output is left-justified at the
/// start of `buf` (i.e. `buf[..n]` is the digits, in normal reading order).
/// If `buf` is too small the function writes as many digits as fit, starting
/// with the most-significant digit that fits, and returns `buf.len()`.
///
/// This helper is the ONLY number-formatting routine used inside the signal
/// handler. It does not allocate, does not lock, and does not call into the
/// standard library's formatting machinery.
///
/// # Examples
///
/// ```
/// use cratonvm_vm::runtime::crash_handler::itoa_into_buf;
///
/// let mut buf = [0u8; 32];
/// let n = itoa_into_buf(&mut buf, 0);
/// assert_eq!(&buf[..n], b"0");
///
/// let n = itoa_into_buf(&mut buf, 12345);
/// assert_eq!(&buf[..n], b"12345");
///
/// let n = itoa_into_buf(&mut buf, u64::MAX);
/// assert_eq!(&buf[..n], b"18446744073709551615");
/// ```
pub fn itoa_into_buf(buf: &mut [u8], n: u64) -> usize {
    if buf.is_empty() {
        return 0;
    }
    if n == 0 {
        buf[0] = b'0';
        return 1;
    }

    // Write digits least-significant-first into a stack scratch (u64::MAX is
    // 20 digits), then reverse-copy the most-significant `out_len` digits
    // into `buf`. If `buf` is too small, low-order digits are dropped — the
    // signal handler's pre-sized buffers always have room for u64::MAX.
    let mut scratch = [0u8; 20];
    let mut len = 0usize;
    let mut v = n;
    while v > 0 && len < scratch.len() {
        scratch[len] = b'0' + (v % 10) as u8;
        v /= 10;
        len += 1;
    }

    let out_len = core::cmp::min(len, buf.len());
    for i in 0..out_len {
        // scratch is little-endian-digits; reverse to put the most-
        // significant digit first.
        buf[i] = scratch[len - 1 - i];
    }
    out_len
}

#[cfg(unix)]
mod async_signal_safe {
    //! Async-signal-safe helpers used by the SIGSEGV handler.
    //!
    //! Every function here must avoid: allocation, locking, stdio buffering,
    //! `std::fs`, `format!`, panics. Verified by manual audit only — there is
    //! no compiler check for async-signal safety in Rust today.

    use core::sync::atomic::{AtomicI32, Ordering};

    /// STDERR file descriptor number on every POSIX system.
    pub const STDERR_FD: i32 = 2;

    /// Cached process id, populated at installation time so the signal path
    /// does not need to call `getpid` (which IS safe, but caching one libc
    /// call per signal is a minor improvement and keeps the path obvious).
    pub static CACHED_PID: AtomicI32 = AtomicI32::new(0);

    /// Read the calling thread's `errno`. Async-signal-safe: POSIX guarantees
    /// `errno` is thread-local and that *reading* it is permitted from a signal
    /// handler. We deliberately only ever READ it here (never set it), so there
    /// is no reentrancy hazard — the pointer returned by `__errno_location` /
    /// `__error` is a stable per-thread address obtained without allocation.
    fn errno() -> i32 {
        // The accessor symbol differs per libc:
        //   glibc/musl (Linux)            -> __errno_location
        //   macOS/iOS/FreeBSD/Dragonfly   -> __error
        //   Android/NetBSD/OpenBSD        -> __errno
        #[cfg(target_os = "linux")]
        unsafe {
            *libc::__errno_location()
        }
        #[cfg(any(
            target_os = "macos",
            target_os = "ios",
            target_os = "freebsd",
            target_os = "dragonfly"
        ))]
        unsafe {
            *libc::__error()
        }
        #[cfg(any(target_os = "android", target_os = "openbsd", target_os = "netbsd"))]
        unsafe {
            *libc::__errno()
        }
        #[cfg(not(any(
            target_os = "linux",
            target_os = "macos",
            target_os = "ios",
            target_os = "freebsd",
            target_os = "dragonfly",
            target_os = "android",
            target_os = "openbsd",
            target_os = "netbsd"
        )))]
        {
            // Unknown Unix: we cannot read errno portably, so treat every
            // short/failed write as non-retryable (return 0, never == EINTR).
            0
        }
    }

    /// Write the entire `bytes` slice to `fd` using raw `write(2)`, correctly
    /// resuming from the unwritten offset on a short (partial) write and
    /// retrying the remainder on `EINTR`.
    ///
    /// `write(2)` may return fewer bytes than requested (a short write) or fail
    /// with `EINTR` if a signal interrupts it before any byte is transferred.
    /// In both cases the loop advances `off` by the number of bytes ACTUALLY
    /// written and re-issues the syscall for the remaining tail, so the crash
    /// report is never truncated. Any other error (or a `write` returning 0)
    /// is unrecoverable and we give up silently — a partial report is better
    /// than spinning forever inside a crashing process.
    ///
    /// Async-signal-safe: `write` is on the POSIX whitelist, and [`errno`] is
    /// read-only (see its doc).
    pub fn write_all(fd: i32, bytes: &[u8]) {
        let mut off = 0usize;
        while off < bytes.len() {
            let ptr = unsafe { bytes.as_ptr().add(off) } as *const libc::c_void;
            let want = bytes.len() - off;
            let r = unsafe { libc::write(fd, ptr, want) };
            if r > 0 {
                // Partial or full write: advance by exactly what was written
                // and continue with the remaining tail.
                off += r as usize;
            } else if r < 0 {
                // EINTR -> the syscall was interrupted before transferring any
                // byte; retry the same remainder. Any other errno is a real,
                // non-transient failure -> stop.
                if errno() == libc::EINTR {
                    continue;
                }
                return;
            } else {
                // r == 0: no progress is possible (e.g. fd closed / zero-length
                // device). Avoid an infinite loop.
                return;
            }
        }
    }

    /// Open `path` for writing (create + truncate, mode 0644) and return the
    /// fd, or -1 on failure. `path` MUST be NUL-terminated.
    ///
    /// Async-signal-safe: `open` is on the POSIX whitelist.
    pub fn open_write_trunc(path_nul: &[u8]) -> i32 {
        debug_assert!(path_nul.last() == Some(&0));
        let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC;
        let mode = 0o644 as libc::c_int;
        unsafe { libc::open(path_nul.as_ptr() as *const libc::c_char, flags, mode) }
    }

    /// Close a file descriptor, ignoring errors.
    pub fn close(fd: i32) {
        unsafe { libc::close(fd) };
    }

    /// Map a signal number to a `&'static [u8]` name. Static-data lookup is
    /// async-signal-safe.
    pub fn signal_name_bytes(sig: i32) -> &'static [u8] {
        match sig {
            libc::SIGSEGV => b"SIGSEGV",
            libc::SIGBUS => b"SIGBUS",
            libc::SIGFPE => b"SIGFPE",
            libc::SIGILL => b"SIGILL",
            libc::SIGABRT => b"SIGABRT",
            _ => b"SIG?",
        }
    }
}

#[cfg(unix)]
std::thread_local! {
    /// Per-thread scratch buffer the signal handler can write into without
    /// allocating. Populated lazily on first access from a *normal* (non-
    /// signal) context; the signal handler only reads it. 256 bytes is
    /// sufficient for the short async-signal-safe message we emit.
    ///
    /// NOTE: thread-local access itself is implementation-defined in async-
    /// signal context. On glibc + musl the fast path is just a TLS offset
    /// load, which is safe. On platforms where the first access initializes
    /// lazily via `pthread_setspecific`, we pre-touch the buffer at thread
    /// start (see `prime_signal_tls`) to avoid that case.
    static SIGNAL_SCRATCH: core::cell::UnsafeCell<[u8; 256]> =
        core::cell::UnsafeCell::new([0u8; 256]);
}

/// Pre-touch the thread-local signal scratch buffer so that the lazy TLS
/// initialization (which on some platforms calls into the allocator) happens
/// in *normal* context, not in the signal handler.
///
/// Call this from every thread that might receive a fatal signal. It is a
/// no-op on platforms without thread-local storage support.
#[cfg(unix)]
pub fn prime_signal_tls() {
    SIGNAL_SCRATCH.with(|cell| {
        // Touch the first byte to force initialization.
        unsafe { (*cell.get())[0] = 0 };
    });
}

#[cfg(not(unix))]
pub fn prime_signal_tls() {}

// ── Unix signal handlers ───────────────────────────────────────────────────

#[cfg(unix)]
fn install_signal_handlers() {
    use std::ffi::c_int;

    // Both `install_crash_handler` and `install_hardware_fault_handler` reach
    // here; installing twice would be harmless but would also re-arm the
    // handler after a deliberate override, so latch it.
    static INSTALLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }

    // Sampled here, never inside the handler: naming the faulting JIT method
    // goes through `lookup_jit_method_name`, which allocates a `String`. That
    // is not async-signal-safe, so it is opt-in via the same env var that
    // populates the table in the first place.
    static NAME_JIT_FRAMES: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);
    // Readability probe for the memory dumps below: `write(2)` returns EFAULT
    // for an unreadable buffer instead of faulting.
    //
    // This used to write to /dev/null — which does NOT work. The null device's
    // write handler returns the byte count without ever touching the user
    // buffer, so the probe called every address readable, and a fault carrying
    // a non-pointer in R10 (`r10=0x1` is the shape that exposed this) made the
    // handler dereference address 1, take a nested SIGSEGV, and re-raise with
    // SIG_DFL — truncating the report at `#  slot[r10]:`.
    //
    // A pipe copies from the buffer for real, so a bad address comes back
    // EFAULT. Non-blocking so a (impossible in practice: one crash per process,
    // ~120 bytes, 64 KiB of pipe) full pipe cannot wedge the handler; the read
    // end stays open for the process lifetime so the write can never EPIPE.
    static PROBE_FD: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);
    {
        let mut fds = [-1i32; 2];
        if unsafe { libc::pipe(fds.as_mut_ptr()) } == 0 {
            unsafe {
                libc::fcntl(fds[1], libc::F_SETFL, libc::O_NONBLOCK);
                libc::fcntl(fds[0], libc::F_SETFD, libc::FD_CLOEXEC);
                libc::fcntl(fds[1], libc::F_SETFD, libc::FD_CLOEXEC);
            }
            PROBE_FD.store(fds[1], Ordering::SeqCst);
        }
    }
    NAME_JIT_FRAMES.store(
        cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_JIT_NAMES").is_some(),
        Ordering::SeqCst,
    );

    // Signals that indicate a crash.
    const CRASH_SIGNALS: &[c_int] = &[
        libc::SIGSEGV,
        libc::SIGBUS,
        libc::SIGFPE,
        libc::SIGILL,
        libc::SIGABRT,
    ];

    // Pre-allocated constant strings used by the signal handler. Storing them
    // as `&'static [u8]` means no allocation is needed to reference them.
    const HDR: &[u8] =
        b"\n#\n# A fatal error has been detected by the CratonVM Runtime Environment:\n#  ";
    const AT_PC: &[u8] = b" at pc=0x";
    const ADDR_LBL: &[u8] = b", addr=0x";
    const PID_LBL: &[u8] = b", pid=";
    const TID_LBL: &[u8] = b", tid=";
    const NL_REPORT: &[u8] = b"\n#  Error report saved to: ";
    const FOOTER: &[u8] = b"\n#\n";
    // The class library is the single most load-bearing fact in a CratonVM
    // bug report (two complete, differently-buggy standard libraries — see the
    // `VM diagnostic snapshot` section). `jdk_mode_bytes()` is a relaxed
    // atomic load returning a `&'static [u8]`, which keeps this path
    // async-signal-safe: no allocation, no lock, no formatting.
    const JDK_MODE_LBL: &[u8] = b"\n#  jdk mode: ";
    const FILE_PREFIX: &[u8] = b"hs_err_pid";
    const FILE_SUFFIX: &[u8] = b".log";

    // Cache the pid in normal context so the handler doesn't need libc::getpid
    // (which is technically signal-safe, but we minimize syscalls).
    async_signal_safe::CACHED_PID.store(std::process::id() as i32, Ordering::Relaxed);

    // Prime TLS on the installing thread (each VM-spawned thread should also
    // call `prime_signal_tls` itself at startup).
    prime_signal_tls();

    // ── THE SIGNAL HANDLER ────────────────────────────────────────────────
    //
    // This function executes in async-signal context. See the module doc
    // for the rules. Roughly: only libc syscalls, atomics, and stack-local
    // arithmetic are allowed. No allocations, no locks, no formatting
    // machinery, no `std::fs`, no `Backtrace::capture`.
    extern "C" fn crash_signal_handler(
        sig: std::ffi::c_int,
        info: *mut libc::siginfo_t,
        ucontext: *mut std::ffi::c_void,
    ) {
        // Re-entry guard. `compare_exchange` on an `AtomicBool` is lock-free
        // and async-signal-safe on every architecture we target.
        if CRASH_IN_PROGRESS
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            unsafe {
                libc::signal(sig, libc::SIG_DFL);
                libc::raise(sig);
            }
            return;
        }

        // Stack buffers — no heap.
        let mut pid_buf = [0u8; 20];
        let mut tid_buf = [0u8; 20];
        let mut filename = [0u8; 64];
        let mut pc_buf = [0u8; 16];
        let mut addr_buf = [0u8; 16];

        // Faulting PC and address. Both come from the kernel-supplied
        // `siginfo_t` / `ucontext_t`, so reading them is async-signal-safe
        // (plain loads from the signal frame). The PC is the single most
        // useful fact in a JIT crash: an `rip` equal to `si_addr` means an
        // indirect call jumped to an unmapped/non-executable target, which is
        // what a stale inline-cache entry looks like.
        let fault_addr = if info.is_null() {
            0u64
        } else {
            unsafe { (*info).si_addr() as usize as u64 }
        };
        // Does `si_addr` mean anything at all? For a hardware fault `si_code`
        // is one of the small positive per-signal codes (SEGV_MAPERR,
        // SEGV_ACCERR, BUS_ADRERR, ...) and `si_addr` is the address that
        // faulted. For a signal someone SENT — `kill`, `tgkill`, `sigqueue` —
        // `si_code` is <= 0 (SI_USER is 0, SI_QUEUE is -1) and `si_addr` is
        // not an address, it is whatever that union member happens to hold.
        // 0x80 is SI_KERNEL, also address-free. Without this the report reads
        // `addr=0x0` off a `kill -SEGV` and then asserts a NULL-receiver
        // shape for a fault that never happened.
        let fault_addr_is_real =
            !info.is_null() && si_code_means_a_faulting_address(unsafe { (*info).si_code });
        let fault_pc = fault_pc_from_ucontext(ucontext);
        let pc_len = hex_into_buf(&mut pc_buf, fault_pc);
        let addr_len = hex_into_buf(&mut addr_buf, fault_addr);

        let pid = async_signal_safe::CACHED_PID.load(Ordering::Relaxed) as u64;
        let pid_len = itoa_into_buf(&mut pid_buf, pid);

        // Thread id via `pthread_self` -> usize cast. pthread_self is on the
        // POSIX async-signal-safe whitelist.
        let tid = unsafe { libc::pthread_self() as u64 };
        let tid_len = itoa_into_buf(&mut tid_buf, tid);

        // Build "hs_err_pid<pid>.log\0" into `filename` without allocating.
        let mut fpos = 0usize;
        let parts: [&[u8]; 3] = [FILE_PREFIX, &pid_buf[..pid_len], FILE_SUFFIX];
        for part in parts.iter() {
            for &b in part.iter() {
                if fpos + 1 < filename.len() {
                    filename[fpos] = b;
                    fpos += 1;
                }
            }
        }
        // NUL-terminate for `open(2)`.
        filename[fpos] = 0;
        let path_nul = &filename[..=fpos];

        let sig_name = async_signal_safe::signal_name_bytes(sig);

        // ── Emit message to stderr (write(2) is async-signal-safe) ─────────
        async_signal_safe::write_all(async_signal_safe::STDERR_FD, HDR);
        async_signal_safe::write_all(async_signal_safe::STDERR_FD, sig_name);
        async_signal_safe::write_all(async_signal_safe::STDERR_FD, AT_PC);
        async_signal_safe::write_all(async_signal_safe::STDERR_FD, &pc_buf[..pc_len]);
        async_signal_safe::write_all(async_signal_safe::STDERR_FD, ADDR_LBL);
        async_signal_safe::write_all(async_signal_safe::STDERR_FD, &addr_buf[..addr_len]);
        async_signal_safe::write_all(async_signal_safe::STDERR_FD, PID_LBL);
        async_signal_safe::write_all(async_signal_safe::STDERR_FD, &pid_buf[..pid_len]);
        async_signal_safe::write_all(async_signal_safe::STDERR_FD, TID_LBL);
        async_signal_safe::write_all(async_signal_safe::STDERR_FD, &tid_buf[..tid_len]);
        async_signal_safe::write_all(async_signal_safe::STDERR_FD, JDK_MODE_LBL);
        async_signal_safe::write_all(async_signal_safe::STDERR_FD, jdk_mode_bytes());
        async_signal_safe::write_all(async_signal_safe::STDERR_FD, NL_REPORT);
        // Write filename without the trailing NUL.
        async_signal_safe::write_all(async_signal_safe::STDERR_FD, &filename[..fpos]);
        async_signal_safe::write_all(async_signal_safe::STDERR_FD, FOOTER);

        // ORDERING RULE: everything that can be answered WITHOUT dereferencing
        // a register goes first, and the raw memory dumps go last. A fault
        // whose registers hold non-pointers is exactly when the provenance
        // verdicts matter most, and it is also exactly when a dump can fault
        // again — which costs the whole rest of the report.

        // Indirect-call registers. The JIT's inline MIC/PIC cascade and the
        // hashed megamorphic stub all fault as `MOV R11,[R10+disp]; CALL R11`,
        // so R10 names the cache slot and R11 the entry it produced. RSP/RBP
        // separate a CALL (which has already pushed a return address) from a
        // JMP, a fall-through, or a corrupted RET.
        let r10 = greg_from_ucontext(ucontext, GREG_R10);
        let rsp = greg_from_ucontext(ucontext, GREG_RSP);
        {
            let r11 = greg_from_ucontext(ucontext, GREG_R11);
            let rbp = greg_from_ucontext(ucontext, GREG_RBP);
            let mut rbuf = [0u8; 16];
            let n = hex_into_buf(&mut rbuf, r10);
            async_signal_safe::write_all(async_signal_safe::STDERR_FD, b"#  r10=0x");
            async_signal_safe::write_all(async_signal_safe::STDERR_FD, &rbuf[..n]);
            let n = hex_into_buf(&mut rbuf, r11);
            async_signal_safe::write_all(async_signal_safe::STDERR_FD, b" r11=0x");
            async_signal_safe::write_all(async_signal_safe::STDERR_FD, &rbuf[..n]);
            let n = hex_into_buf(&mut rbuf, rsp);
            async_signal_safe::write_all(async_signal_safe::STDERR_FD, b" rsp=0x");
            async_signal_safe::write_all(async_signal_safe::STDERR_FD, &rbuf[..n]);
            let n = hex_into_buf(&mut rbuf, rbp);
            async_signal_safe::write_all(async_signal_safe::STDERR_FD, b" rbp=0x");
            async_signal_safe::write_all(async_signal_safe::STDERR_FD, &rbuf[..n]);
            async_signal_safe::write_all(async_signal_safe::STDERR_FD, b"\n");

            // The rest of the general-purpose file, two lines, fixed order.
            // A crash report is read once, long after the process is gone, so
            // a register the report omitted is evidence that no longer exists:
            // the eight hibernate-orm JSON/XML crashes were decoded from `rax`
            // and `r10` together, and constant `rbx`/`rbp`/`rdi` across all
            // eight is what ruled out "memory corruption from anywhere".
            // Reading `gregs` and writing hex allocates nothing.
            for (label, which) in [
                (b"#  rax=0x".as_slice(), GREG_RAX),
                (b" rbx=0x".as_slice(), GREG_RBX),
                (b" rcx=0x".as_slice(), GREG_RCX),
                (b" rdx=0x".as_slice(), GREG_RDX),
                (b" rsi=0x".as_slice(), GREG_RSI),
                (b" rdi=0x".as_slice(), GREG_RDI),
            ] {
                let n = hex_into_buf(&mut rbuf, greg_from_ucontext(ucontext, which));
                async_signal_safe::write_all(async_signal_safe::STDERR_FD, label);
                async_signal_safe::write_all(async_signal_safe::STDERR_FD, &rbuf[..n]);
            }
            async_signal_safe::write_all(async_signal_safe::STDERR_FD, b"\n");
            for (label, which) in [
                (b"#  r8=0x".as_slice(), GREG_R8),
                (b" r9=0x".as_slice(), GREG_R9),
                (b" r12=0x".as_slice(), GREG_R12),
                (b" r13=0x".as_slice(), GREG_R13),
                (b" r14=0x".as_slice(), GREG_R14),
                (b" r15=0x".as_slice(), GREG_R15),
            ] {
                let n = hex_into_buf(&mut rbuf, greg_from_ucontext(ucontext, which));
                async_signal_safe::write_all(async_signal_safe::STDERR_FD, label);
                async_signal_safe::write_all(async_signal_safe::STDERR_FD, &rbuf[..n]);
            }
            async_signal_safe::write_all(async_signal_safe::STDERR_FD, b"\n");

            // Decode the faulting address as `base + index*scale` over the
            // registers just printed — the same search the Windows path has
            // done since 2026-08-22, which Linux did not have. That is not a
            // hypothetical gap: all eight logs behind
            // known-issues/hibernate/
            // hib-orm-json-xml-function-tests-segfault-g1-zgc-20260820.md
            // satisfy `fault == r10 + rax*4`, an unchecked jump-table load with
            // a garbage index, and two sessions read the scattered addresses as
            // random corruption because nothing said so.
            //
            // A `*4`/`*8` hit whose INDEX register holds a large value is the
            // signature worth acting on: an array index, or an enum
            // discriminant that should have been small, read out of memory that
            // did not hold one.
            //
            // The Windows reporter formats into a `String`; a signal handler
            // may not allocate, so both go through `for_each_indexed_load_match`
            // and only the rendering differs.
            if fault_addr_is_real {
                let regs: [(&str, u64); 16] = [
                    ("rax", greg_from_ucontext(ucontext, GREG_RAX)),
                    ("rbx", greg_from_ucontext(ucontext, GREG_RBX)),
                    ("rcx", greg_from_ucontext(ucontext, GREG_RCX)),
                    ("rdx", greg_from_ucontext(ucontext, GREG_RDX)),
                    ("rsi", greg_from_ucontext(ucontext, GREG_RSI)),
                    ("rdi", greg_from_ucontext(ucontext, GREG_RDI)),
                    ("r8", greg_from_ucontext(ucontext, GREG_R8)),
                    ("r9", greg_from_ucontext(ucontext, GREG_R9)),
                    ("r10", r10),
                    ("r11", greg_from_ucontext(ucontext, GREG_R11)),
                    ("r12", greg_from_ucontext(ucontext, GREG_R12)),
                    ("r13", greg_from_ucontext(ucontext, GREG_R13)),
                    ("r14", greg_from_ucontext(ucontext, GREG_R14)),
                    ("r15", greg_from_ucontext(ucontext, GREG_R15)),
                    ("rsp", rsp),
                    ("rbp", greg_from_ucontext(ucontext, GREG_RBP)),
                ];
                let mut wrote_header = false;
                let hits = for_each_indexed_load_match(
                    &regs,
                    fault_addr as usize,
                    |bname, iname, scale, nth| {
                        if nth >= 6 {
                            return;
                        }
                        if !wrote_header {
                            async_signal_safe::write_all(
                                async_signal_safe::STDERR_FD,
                                b"#  fault addr decodes as an indexed load:",
                            );
                            wrote_header = true;
                        }
                        async_signal_safe::write_all(async_signal_safe::STDERR_FD, b" [");
                        async_signal_safe::write_all(
                            async_signal_safe::STDERR_FD,
                            bname.as_bytes(),
                        );
                        async_signal_safe::write_all(async_signal_safe::STDERR_FD, b"+");
                        async_signal_safe::write_all(
                            async_signal_safe::STDERR_FD,
                            iname.as_bytes(),
                        );
                        async_signal_safe::write_all(async_signal_safe::STDERR_FD, b"*");
                        async_signal_safe::write_all(
                            async_signal_safe::STDERR_FD,
                            match scale {
                                1 => b"1".as_slice(),
                                2 => b"2".as_slice(),
                                4 => b"4".as_slice(),
                                _ => b"8".as_slice(),
                            },
                        );
                        async_signal_safe::write_all(async_signal_safe::STDERR_FD, b"]");
                    },
                );
                if wrote_header {
                    if hits > 6 {
                        async_signal_safe::write_all(async_signal_safe::STDERR_FD, b" ...");
                    }
                    async_signal_safe::write_all(async_signal_safe::STDERR_FD, b"\n");
                }

                // The other shape this tree has seen: an address inside the
                // first page is a field load off a NULL receiver, where the low
                // bits are the field offset and not an address. Both Tomcat
                // SIGSEGVs of 2026-08-22 are this (`addr=0x5`, `addr=0xf`), and
                // neither decodes as an indexed load — saying so is what keeps
                // them out of the hibernate family.
                if fault_addr < 4096 {
                    async_signal_safe::write_all(
                        async_signal_safe::STDERR_FD,
                        b"#  fault addr is below one page - a NULL receiver plus a field \
offset, not a stray pointer\n",
                    );
                }
            } else {
                async_signal_safe::write_all(
                    async_signal_safe::STDERR_FD,
                    b"#  this signal was SENT, not raised by a fault - the addr above is \
not an address\n",
                );
            }
        }

        // Which compiled body does the faulting PC (and, for an indirect call
        // that jumped to an unmapped target, the faulting ADDRESS) belong to?
        // For a stale inline-cache entry those are the same value, and the name
        // is the callee whose body was retired underneath the cache.
        //
        // The OUTCOME of every lookup is reported, not just the hits: a silent
        // absence used to mean either "no compiled body covers this address" or
        // "the name-registry mutex happened to be held", and those read very
        // differently in a report.
        if NAME_JIT_FRAMES.load(Ordering::Relaxed) {
            for (label, addr) in [
                (b"#  jit pc  : ".as_slice(), fault_pc),
                (b"#  jit addr: ".as_slice(), fault_addr),
            ] {
                async_signal_safe::write_all(async_signal_safe::STDERR_FD, label);
                match cratonvm_jit::lookup_jit_method_name_detailed(addr as usize) {
                    cratonvm_jit::JitNameLookup::Found(name) => {
                        async_signal_safe::write_all(async_signal_safe::STDERR_FD, name.as_bytes());
                    }
                    cratonvm_jit::JitNameLookup::NotFound => {
                        async_signal_safe::write_all(
                            async_signal_safe::STDERR_FD,
                            b"<no published body covers this address>",
                        );
                    }
                    cratonvm_jit::JitNameLookup::Locked => {
                        async_signal_safe::write_all(
                            async_signal_safe::STDERR_FD,
                            b"<name registry locked - this is NOT a negative>",
                        );
                    }
                }
                async_signal_safe::write_all(async_signal_safe::STDERR_FD, b"\n");
            }
        }

        // Did any JIT-emitted shadow push bail on the `end` guard before this
        // crash? A non-zero count is the fingerprint of a compiled method that
        // leaks shadow pushes — the guard keeps that from becoming a heap
        // overwrite, but the leak itself is still a live defect, and the label
        // names the method whose push bailed last
        // (`CRATONVM_SHADOW_OVERFLOW_DIAG=1`).
        {
            let mut rbuf = [0u8; 16];
            async_signal_safe::write_all(
                async_signal_safe::STDERR_FD,
                b"#  shadow_overflow_bails=0x",
            );
            let n = hex_into_buf(
                &mut rbuf,
                cratonvm_jit::SHADOW_OVERFLOW_COUNT.load(Ordering::Relaxed) as u64,
            );
            async_signal_safe::write_all(async_signal_safe::STDERR_FD, &rbuf[..n]);
            let label = cratonvm_jit::SHADOW_OVERFLOW_LABEL.load(Ordering::Relaxed) as *const u8;
            if !label.is_null() {
                async_signal_safe::write_all(async_signal_safe::STDERR_FD, b" last=");
                // SAFETY: when non-null this names a leaked, NUL-terminated,
                // immortal `str` the compiler produced for the bailing method.
                let mut len = 0usize;
                while len < 512 && unsafe { *label.add(len) } != 0 {
                    len += 1;
                }
                async_signal_safe::write_all(async_signal_safe::STDERR_FD, unsafe {
                    std::slice::from_raw_parts(label, len)
                });
            }
            async_signal_safe::write_all(async_signal_safe::STDERR_FD, b"\n");
        }

        // Was the faulting PC inside a code buffer this process had already
        // unmapped? `lookup_jit_method_name` above cannot say — it answers from
        // the range registry, which a *recycled* address also satisfies. This
        // ring is written at `munmap` time and needs no allocation or lock, so
        // it is safe here, and it carries the one number that turns "SIGSEGV in
        // JIT code" into a diagnosis: how many threads were executing compiled
        // code when the buffer was released.
        {
            let mut rbuf = [0u8; 16];
            async_signal_safe::write_all(async_signal_safe::STDERR_FD, b"#  code_frees_total=0x");
            let n = hex_into_buf(&mut rbuf, cratonvm_jit::code_frees_total() as u64);
            async_signal_safe::write_all(async_signal_safe::STDERR_FD, &rbuf[..n]);
            async_signal_safe::write_all(async_signal_safe::STDERR_FD, b"\n");
            if let Some((base, len, active, flags)) =
                cratonvm_jit::recent_code_free_covering(fault_pc as usize)
            {
                async_signal_safe::write_all(
                    async_signal_safe::STDERR_FD,
                    b"#  fault pc is inside a RECENTLY FREED code buffer: base=0x",
                );
                let n = hex_into_buf(&mut rbuf, base as u64);
                async_signal_safe::write_all(async_signal_safe::STDERR_FD, &rbuf[..n]);
                async_signal_safe::write_all(async_signal_safe::STDERR_FD, b" len=0x");
                let n = hex_into_buf(&mut rbuf, len as u64);
                async_signal_safe::write_all(async_signal_safe::STDERR_FD, &rbuf[..n]);
                async_signal_safe::write_all(
                    async_signal_safe::STDERR_FD,
                    b" active_jit_executions_at_free=0x",
                );
                let n = hex_into_buf(&mut rbuf, active as u64);
                async_signal_safe::write_all(async_signal_safe::STDERR_FD, &rbuf[..n]);
                async_signal_safe::write_all(async_signal_safe::STDERR_FD, b"\n");
                // The count above is NOT self-interpreting, and reading it as
                // if it were has already cost one investigation a session: it
                // is sampled at the `munmap`, not at the instant the release
                // was decided, and it is recorded for every executable buffer
                // including compile attempts that nothing ever pointed into.
                // These three arms are what make it readable.
                let published = flags & cratonvm_jit::CODE_FREE_PUBLISHED != 0;
                let authorised = flags & cratonvm_jit::CODE_FREE_AUTHORISED != 0;
                if !published {
                    async_signal_safe::write_all(
                        async_signal_safe::STDERR_FD,
                        b"#    NEVER PUBLISHED: a discarded compile attempt. No cache entry, baked call or trampoline could name it, so the count above is not evidence of anything.\n",
                    );
                } else if authorised {
                    async_signal_safe::write_all(
                        async_signal_safe::STDERR_FD,
                        b"#    published, released BY THE RETIREMENT QUEUE with a quiescence proof. A non-zero count above only means some OTHER thread entered compiled code between the proof and the unmap.\n",
                    );
                } else {
                    async_signal_safe::write_all(
                        async_signal_safe::STDERR_FD,
                        b"#    *** published body released OUTSIDE the retirement queue. This IS a use-after-free of executable memory. Re-run with CRATONVM_DBG_JIT_CODE_FREE=1 to name the release site. ***\n",
                    );
                }
            } else {
                async_signal_safe::write_all(
                    async_signal_safe::STDERR_FD,
                    b"#  fault pc is in NO recently freed code buffer\n",
                );
            }
        }

        // Is the faulting PC inside a code buffer this process still HOLDS? The
        // name registry knows only bodies `JitCache::put` published; every
        // `ExecutableBuffer` registers here, including OSR trampolines and
        // bodies a compiler thread is still emitting.
        {
            let mut rbuf = [0u8; 16];
            match cratonvm_jit::jit_code_region_covering(fault_pc as usize) {
                cratonvm_jit::JitRegionLookup::Found(base, cap) => {
                    async_signal_safe::write_all(
                        async_signal_safe::STDERR_FD,
                        b"#  fault pc is inside a LIVE registered code buffer: base=0x",
                    );
                    let n = hex_into_buf(&mut rbuf, base as u64);
                    async_signal_safe::write_all(async_signal_safe::STDERR_FD, &rbuf[..n]);
                    async_signal_safe::write_all(async_signal_safe::STDERR_FD, b" cap=0x");
                    let n = hex_into_buf(&mut rbuf, cap as u64);
                    async_signal_safe::write_all(async_signal_safe::STDERR_FD, &rbuf[..n]);
                    async_signal_safe::write_all(async_signal_safe::STDERR_FD, b"\n");
                }
                cratonvm_jit::JitRegionLookup::NotFound => {
                    async_signal_safe::write_all(
                        async_signal_safe::STDERR_FD,
                        b"#  fault pc is in NO live registered code buffer\n",
                    );
                }
                cratonvm_jit::JitRegionLookup::Locked => {
                    async_signal_safe::write_all(
                        async_signal_safe::STDERR_FD,
                        b"#  code-region registry locked - no verdict\n",
                    );
                }
            }
        }

        // The kernel's own answer, and the one fact a JIT crash report could
        // not give without a core dump: is the faulting page a hole between
        // mappings (a wild jump), or a mapping whose permissions forbid
        // execution (a buffer that is not RX)?
        #[cfg(target_os = "linux")]
        report_maps_neighborhood(fault_pc);

        // ── Raw memory dumps. Last, because they can fault. ────────────────
        {
            let mut rbuf = [0u8; 16];
            // The slot R10 points at, when readable. On the MIC/PIC path this
            // is the cache slot: class id, entry pointer, ABI flag.
            if r10 != 0 && probe_readable(PROBE_FD.load(Ordering::Relaxed), r10, 64) {
                let ptr = r10 as usize as *const u8;
                async_signal_safe::write_all(async_signal_safe::STDERR_FD, b"#  slot[r10]:");
                for i in 0..8usize {
                    let word = unsafe { std::ptr::read_unaligned((ptr as *const u64).add(i)) };
                    let n = hex_into_buf(&mut rbuf, word);
                    async_signal_safe::write_all(async_signal_safe::STDERR_FD, b" 0x");
                    async_signal_safe::write_all(async_signal_safe::STDERR_FD, &rbuf[..n]);
                }
                async_signal_safe::write_all(async_signal_safe::STDERR_FD, b"\n");
            } else {
                async_signal_safe::write_all(
                    async_signal_safe::STDERR_FD,
                    b"#  slot[r10]: UNREADABLE (r10 is not a readable pointer)\n",
                );
            }

            // Top of stack. For a CALL to a bad target `[rsp]` is the return
            // address, which names the CALLER.
            if rsp != 0 && probe_readable(PROBE_FD.load(Ordering::Relaxed), rsp, 48) {
                let ptr = rsp as usize as *const u8;
                async_signal_safe::write_all(async_signal_safe::STDERR_FD, b"#  [rsp]:");
                for i in 0..6usize {
                    let word = unsafe { std::ptr::read_unaligned((ptr as *const u64).add(i)) };
                    let n = hex_into_buf(&mut rbuf, word);
                    async_signal_safe::write_all(async_signal_safe::STDERR_FD, b" 0x");
                    async_signal_safe::write_all(async_signal_safe::STDERR_FD, &rbuf[..n]);
                }
                async_signal_safe::write_all(async_signal_safe::STDERR_FD, b"\n");
                if NAME_JIT_FRAMES.load(Ordering::Relaxed) {
                    let ret = unsafe { std::ptr::read_unaligned(ptr as *const u64) };
                    async_signal_safe::write_all(async_signal_safe::STDERR_FD, b"#  jit [rsp]: ");
                    match cratonvm_jit::lookup_jit_method_name_detailed(ret as usize) {
                        cratonvm_jit::JitNameLookup::Found(name) => {
                            async_signal_safe::write_all(
                                async_signal_safe::STDERR_FD,
                                name.as_bytes(),
                            );
                        }
                        cratonvm_jit::JitNameLookup::NotFound => {
                            async_signal_safe::write_all(
                                async_signal_safe::STDERR_FD,
                                b"<not in any published body - caller is native or interpreted>",
                            );
                        }
                        cratonvm_jit::JitNameLookup::Locked => {
                            async_signal_safe::write_all(
                                async_signal_safe::STDERR_FD,
                                b"<name registry locked - this is NOT a negative>",
                            );
                        }
                    }
                    async_signal_safe::write_all(async_signal_safe::STDERR_FD, b"\n");
                }
            }

            // WHO transferred here? `[rsp]` answers that only for a CALL. A
            // tail-position jump pushes nothing, so the top of stack still
            // belongs to a frame further out and the compiled caller is a few
            // words up. Scan a bounded window for the first word that lands in
            // a compiled body and name it: that is the innermost JIT frame, and
            // for a fault at a page-aligned ENTRY it is the method holding the
            // stale call site.
            if NAME_JIT_FRAMES.load(Ordering::Relaxed) && rsp != 0 {
                const WORDS: usize = 64;
                let mut found = false;
                for i in 0..WORDS {
                    let at = rsp.wrapping_add((i * 8) as u64);
                    if !probe_readable(PROBE_FD.load(Ordering::Relaxed), at, 8) {
                        break;
                    }
                    let word = unsafe { std::ptr::read_unaligned(at as usize as *const u64) };
                    if let cratonvm_jit::JitNameLookup::Found(name) =
                        cratonvm_jit::lookup_jit_method_name_detailed(word as usize)
                    {
                        async_signal_safe::write_all(
                            async_signal_safe::STDERR_FD,
                            b"#  innermost JIT frame on the stack: [rsp+0x",
                        );
                        let n = hex_into_buf(&mut rbuf, (i * 8) as u64);
                        async_signal_safe::write_all(async_signal_safe::STDERR_FD, &rbuf[..n]);
                        async_signal_safe::write_all(async_signal_safe::STDERR_FD, b"]=0x");
                        let n = hex_into_buf(&mut rbuf, word);
                        async_signal_safe::write_all(async_signal_safe::STDERR_FD, &rbuf[..n]);
                        async_signal_safe::write_all(async_signal_safe::STDERR_FD, b" ");
                        async_signal_safe::write_all(async_signal_safe::STDERR_FD, name.as_bytes());
                        async_signal_safe::write_all(async_signal_safe::STDERR_FD, b"\n");
                        found = true;
                        break;
                    }
                }
                if !found {
                    async_signal_safe::write_all(
                        async_signal_safe::STDERR_FD,
                        b"#  innermost JIT frame on the stack: none in the first 64 words\n",
                    );
                }
            }
        }

        // ── Write a minimal hs_err_pid file via raw open/write/close ──────
        //
        // We intentionally write a SHORT marker file rather than the full
        // HotSpot report. The full report requires allocation (Backtrace,
        // String, /proc parsing) which is forbidden here. The panic-hook
        // path writes the rich report for ordinary panics; for hardware
        // faults the user gets just enough to start debugging.
        let fd = async_signal_safe::open_write_trunc(path_nul);
        if fd >= 0 {
            async_signal_safe::write_all(fd, b"# CratonVM fatal signal: ");
            async_signal_safe::write_all(fd, sig_name);
            async_signal_safe::write_all(fd, b"\n# pid=");
            async_signal_safe::write_all(fd, &pid_buf[..pid_len]);
            async_signal_safe::write_all(fd, b"\n# tid=");
            async_signal_safe::write_all(fd, &tid_buf[..tid_len]);
            async_signal_safe::write_all(fd, b"\n# pc=0x");
            async_signal_safe::write_all(fd, &pc_buf[..pc_len]);
            async_signal_safe::write_all(fd, b"\n# addr=0x");
            async_signal_safe::write_all(fd, &addr_buf[..addr_len]);
            async_signal_safe::write_all(fd, b"\n# jdk mode: ");
            async_signal_safe::write_all(fd, jdk_mode_bytes());
            async_signal_safe::write_all(
                fd,
                b"\n# (truncated: full report requires allocator, unsafe in \
                  signal handler)\n",
            );
            async_signal_safe::close(fd);
        }

        // Re-raise with default handler so the exit code reflects the signal.
        unsafe {
            libc::signal(sig, libc::SIG_DFL);
            libc::raise(sig);
        }
    }

    // `sigaction` (not `signal`) so the handler receives `siginfo_t` +
    // `ucontext_t` and can report the faulting address and PC. `SA_ONSTACK`
    // keeps us usable on the sigaltstack Rust installs per thread, so a
    // stack-overflow SIGSEGV still reaches the handler instead of double
    // faulting.
    for &sig in CRASH_SIGNALS {
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = crash_signal_handler as usize;
            sa.sa_flags = libc::SA_SIGINFO | libc::SA_ONSTACK | libc::SA_RESTART;
            libc::sigemptyset(&mut sa.sa_mask);
            libc::sigaction(sig, &sa, std::ptr::null_mut());
        }
    }
}

/// `ucontext_t::uc_mcontext.gregs` indices used by the register dump. Only
/// meaningful on Linux/x86-64; other platforms report 0.
///
/// The whole general-purpose file is here, not just the four the dump started
/// with, because the analysis this dump exists to feed needs the operands of
/// the faulting instruction and not only its base. The eight hibernate-orm
/// JSON/XML crashes were decoded by testing `fault_address == r10 + rax*4` --
/// which the Linux report could not have answered, because it never printed
/// `rax` (docs/known-issues/hibernate/
/// hib-orm-json-xml-function-tests-segfault-g1-zgc-20260820.md).
#[cfg(unix)]
const GREG_RAX: usize = 0;
#[cfg(unix)]
const GREG_RBX: usize = 1;
#[cfg(unix)]
const GREG_RCX: usize = 2;
#[cfg(unix)]
const GREG_RDX: usize = 3;
#[cfg(unix)]
const GREG_RSI: usize = 4;
#[cfg(unix)]
const GREG_RDI: usize = 5;
#[cfg(unix)]
const GREG_R8: usize = 6;
#[cfg(unix)]
const GREG_R9: usize = 7;
#[cfg(unix)]
const GREG_R10: usize = 8;
#[cfg(unix)]
const GREG_R11: usize = 9;
#[cfg(unix)]
const GREG_R12: usize = 10;
#[cfg(unix)]
const GREG_R13: usize = 11;
#[cfg(unix)]
const GREG_R14: usize = 12;
#[cfg(unix)]
const GREG_R15: usize = 13;
#[cfg(unix)]
const GREG_RSP: usize = 14;
#[cfg(unix)]
const GREG_RBP: usize = 15;

/// One general-purpose register out of the signal frame, selected by the
/// pseudo-indices above. Plain loads only — async-signal-safe.
///
/// Every index is spelled out. The earlier `_ => libc::REG_RBP` catch-all was
/// safe with four callers and would have quietly reported `rbp` under a
/// fifteenth register's label.
#[cfg(unix)]
fn greg_from_ucontext(ucontext: *mut std::ffi::c_void, which: usize) -> u64 {
    if ucontext.is_null() {
        return 0;
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    unsafe {
        let uc = ucontext as *const libc::ucontext_t;
        let idx = match which {
            GREG_RAX => libc::REG_RAX,
            GREG_RBX => libc::REG_RBX,
            GREG_RCX => libc::REG_RCX,
            GREG_RDX => libc::REG_RDX,
            GREG_RSI => libc::REG_RSI,
            GREG_RDI => libc::REG_RDI,
            GREG_R8 => libc::REG_R8,
            GREG_R9 => libc::REG_R9,
            GREG_R10 => libc::REG_R10,
            GREG_R11 => libc::REG_R11,
            GREG_R12 => libc::REG_R12,
            GREG_R13 => libc::REG_R13,
            GREG_R14 => libc::REG_R14,
            GREG_R15 => libc::REG_R15,
            GREG_RSP => libc::REG_RSP,
            GREG_RBP => libc::REG_RBP,
            _ => return 0,
        };
        (*uc).uc_mcontext.gregs[idx as usize] as u64
    }
    #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
    {
        let _ = (ucontext, which);
        0
    }
}

/// Does this signal's `si_addr` mean anything?
///
/// For a hardware fault `si_code` is one of the small positive per-signal codes
/// (`SEGV_MAPERR`, `SEGV_ACCERR`, `BUS_ADRERR`, …) and `si_addr` is the address
/// that faulted. For a signal someone SENT — `kill`, `tgkill`, `sigqueue` —
/// `si_code` is <= 0 (`SI_USER` is 0, `SI_QUEUE` is -1, `SI_TKILL` is -6) and
/// `si_addr` is not an address at all, it is whatever that union member happens
/// to hold. `SI_KERNEL` (0x80) is address-free too.
///
/// Without this the report reads `addr=0x0` off a `kill -SEGV` and then decodes
/// a faulting address that never existed.
#[cfg(unix)]
fn si_code_means_a_faulting_address(si_code: i32) -> bool {
    const SI_KERNEL: i32 = 0x80;
    si_code > 0 && si_code != SI_KERNEL
}

/// Faulting instruction pointer out of the signal frame, or 0 where the
/// platform layout is not known. Plain loads only — async-signal-safe.
#[cfg(unix)]
fn fault_pc_from_ucontext(ucontext: *mut std::ffi::c_void) -> u64 {
    if ucontext.is_null() {
        return 0;
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    unsafe {
        let uc = ucontext as *const libc::ucontext_t;
        (*uc).uc_mcontext.gregs[libc::REG_RIP as usize] as u64
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    unsafe {
        let uc = ucontext as *const libc::ucontext_t;
        (*uc).uc_mcontext.pc
    }
    #[cfg(not(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    )))]
    {
        let _ = ucontext;
        0
    }
}

/// Is `len` bytes at `addr` readable, without faulting to find out?
///
/// `write(2)` copies from the user buffer inside the kernel and returns
/// `EFAULT` rather than raising SIGSEGV, so a scratch fd answers the question
/// safely. `fd` must be a PIPE, not `/dev/null`: the null device's write
/// handler returns the byte count without ever touching the buffer, which makes
/// it report every address — including `0x1` — as readable.
#[cfg(unix)]
fn probe_readable(fd: i32, addr: u64, len: usize) -> bool {
    if fd < 0 || addr == 0 {
        return false;
    }
    let n = unsafe { libc::write(fd, addr as usize as *const libc::c_void, len) };
    // Widening: isize -> usize comparison guarded by the sign check.
    n >= 0 && n as usize == len
}

/// Print the `/proc/self/maps` neighbourhood of `addr` to stderr.
///
/// Async-signal-safe: `open`/`read`/`write`/`close`, fixed stack buffers, no
/// allocation and no lock. Read in small chunks rather than slurped, because
/// this runs on the signal alternate stack.
#[cfg(target_os = "linux")]
fn report_maps_neighborhood(addr: u64) {
    const LBL_MAPPED: &[u8] = b"#  maps: fault pc IS MAPPED - perms are on the `here` line\n";
    const LBL_GAP: &[u8] = b"#  maps: fault pc is NOT MAPPED - it is the hole between these two\n";
    const LBL_ABOVE: &[u8] = b"#  maps: fault pc is NOT MAPPED - above the last mapping\n";
    let fd = unsafe { libc::open(c"/proc/self/maps".as_ptr(), libc::O_RDONLY) };
    if fd < 0 {
        return;
    }
    let mut chunk = [0u8; 1024];
    let mut line = [0u8; 256];
    let mut prev = [0u8; 256];
    let mut line_len = 0usize;
    let mut prev_len = 0usize;
    let mut decided = false;
    'read: loop {
        let n = unsafe { libc::read(fd, chunk.as_mut_ptr() as *mut libc::c_void, chunk.len()) };
        if n <= 0 {
            break;
        }
        for i in 0..(n as usize) {
            let b = chunk[i];
            if b != b'\n' {
                if line_len < line.len() {
                    line[line_len] = b;
                    line_len += 1;
                }
                continue;
            }
            let (start, end) = parse_maps_range(&line[..line_len]);
            if end != 0 && addr >= start && addr < end {
                emit_maps_pair(LBL_MAPPED, &prev[..prev_len], &line[..line_len]);
                decided = true;
                break 'read;
            }
            if end != 0 && start > addr {
                emit_maps_pair(LBL_GAP, &prev[..prev_len], &line[..line_len]);
                decided = true;
                break 'read;
            }
            prev[..line_len].copy_from_slice(&line[..line_len]);
            prev_len = line_len;
            line_len = 0;
        }
    }
    unsafe {
        libc::close(fd);
    }
    if !decided {
        emit_maps_pair(LBL_ABOVE, &prev[..prev_len], b"");
    }
}

/// `write(2)` a maps verdict plus the one or two lines it refers to.
#[cfg(target_os = "linux")]
fn emit_maps_pair(label: &[u8], prev: &[u8], here: &[u8]) {
    async_signal_safe::write_all(async_signal_safe::STDERR_FD, label);
    for (tag, body) in [
        (b"#    prev: ".as_slice(), prev),
        (b"#    here: ".as_slice(), here),
    ] {
        if body.is_empty() {
            continue;
        }
        async_signal_safe::write_all(async_signal_safe::STDERR_FD, tag);
        async_signal_safe::write_all(async_signal_safe::STDERR_FD, body);
        async_signal_safe::write_all(async_signal_safe::STDERR_FD, b"\n");
    }
}

/// Parse the leading `<start>-<end>` hex range of a `/proc/self/maps` line.
/// Returns `(0, 0)` for anything that is not a range line.
#[cfg(target_os = "linux")]
fn parse_maps_range(line: &[u8]) -> (u64, u64) {
    fn hexval(b: u8) -> Option<u64> {
        match b {
            b'0'..=b'9' => Some((b - b'0') as u64),
            b'a'..=b'f' => Some((b - b'a' + 10) as u64),
            b'A'..=b'F' => Some((b - b'A' + 10) as u64),
            _ => None,
        }
    }
    let mut i = 0usize;
    let mut start = 0u64;
    while i < line.len() {
        match hexval(line[i]) {
            Some(v) => {
                start = (start << 4) | v;
                i += 1;
            }
            None => break,
        }
    }
    if i == 0 || i >= line.len() || line[i] != b'-' {
        return (0, 0);
    }
    i += 1;
    let mut end = 0u64;
    let mut any = false;
    while i < line.len() {
        match hexval(line[i]) {
            Some(v) => {
                end = (end << 4) | v;
                i += 1;
                any = true;
            }
            None => break,
        }
    }
    if !any {
        return (0, 0);
    }
    (start, end)
}

/// Lower-case hex of `n` into `buf` (no `0x` prefix, no leading zeros).
/// Allocation-free and branch-simple — safe to call from a signal handler.
pub fn hex_into_buf(buf: &mut [u8], n: u64) -> usize {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    if buf.is_empty() {
        return 0;
    }
    if n == 0 {
        buf[0] = b'0';
        return 1;
    }
    let mut scratch = [0u8; 16];
    let mut len = 0usize;
    let mut v = n;
    while v > 0 && len < scratch.len() {
        scratch[len] = DIGITS[(v & 0xf) as usize];
        v >>= 4;
        len += 1;
    }
    let out_len = core::cmp::min(len, buf.len());
    for i in 0..out_len {
        buf[i] = scratch[len - 1 - i];
    }
    out_len
}

// ── Platform helpers ───────────────────────────────────────────────────────

fn get_pid() -> u32 {
    std::process::id()
}

fn get_tid() -> u64 {
    // Thread ID as a u64. On most platforms this is the OS thread ID.
    #[cfg(unix)]
    {
        unsafe { libc::pthread_self() as u64 }
    }
    #[cfg(windows)]
    {
        extern "system" {
            fn GetCurrentThreadId() -> u32;
        }
        unsafe { GetCurrentThreadId() as u64 }
    }
    #[cfg(not(any(unix, windows)))]
    {
        // Fallback: hash the thread's name or use 0.
        0
    }
}

/// Map a signal number to its name.
#[cfg(unix)]
fn signal_name(sig: i32) -> String {
    match sig {
        libc::SIGSEGV => "SIGSEGV".to_string(),
        libc::SIGBUS => "SIGBUS".to_string(),
        libc::SIGFPE => "SIGFPE".to_string(),
        libc::SIGILL => "SIGILL".to_string(),
        libc::SIGABRT => "SIGABRT".to_string(),
        other => format!("SIG({})", other),
    }
}

/// Rust compiler version baked in at build time.
fn rust_version() -> &'static str {
    // `rustc_version_runtime` isn't available; use the build-env macro.
    // `CARGO_PKG_RUST_VERSION` gives the MSRV, not the actual compiler.
    // Best we can do without a build script is the env variable.
    option_env!("RUSTC_VERSION").unwrap_or(env!("CARGO_PKG_RUST_VERSION"))
}

/// Format a `SystemTime` as a human-readable timestamp.
fn format_timestamp(t: SystemTime) -> String {
    match t.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(d) => {
            let secs = d.as_secs();
            // Manual UTC decomposition (no chrono dependency).
            let days = secs / 86400;
            let time_of_day = secs % 86400;
            let hours = time_of_day / 3600;
            let minutes = (time_of_day % 3600) / 60;
            let seconds = time_of_day % 60;

            // Days since epoch to Y-M-D (simplified Gregorian).
            let (year, month, day) = days_to_ymd(days);
            format!(
                "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
                year, month, day, hours, minutes, seconds
            )
        }
        Err(_) => "unknown".to_string(),
    }
}

/// Convert days-since-Unix-epoch to (year, month, day).
fn days_to_ymd(days: u64) -> (u64, u64, u64) {
    // Algorithm from Howard Hinnant's `civil_from_days`.
    let z = days as i64 + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64; // day of era [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as u64, m, d)
}

// ── OS / CPU / Memory info ─────────────────────────────────────────────────

/// Collect operating system information.
pub fn get_os_info() -> String {
    #[cfg(target_os = "linux")]
    {
        get_os_info_linux()
    }
    #[cfg(target_os = "macos")]
    {
        get_os_info_macos()
    }
    #[cfg(target_os = "windows")]
    {
        get_os_info_windows()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        format!("{} {}", std::env::consts::OS, std::env::consts::ARCH)
    }
}

#[cfg(target_os = "linux")]
fn get_os_info_linux() -> String {
    // Try /etc/os-release first for distribution info.
    let distro = std::fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|content| {
            content
                .lines()
                .find(|l| l.starts_with("PRETTY_NAME="))
                .map(|l| {
                    l.trim_start_matches("PRETTY_NAME=")
                        .trim_matches('"')
                        .to_string()
                })
        })
        .unwrap_or_else(|| "Linux".to_string());

    let kernel = std::fs::read_to_string("/proc/version")
        .ok()
        .and_then(|v| v.split_whitespace().nth(2).map(String::from))
        .unwrap_or_else(|| "unknown".to_string());

    format!("{}, kernel {}, {}", distro, kernel, std::env::consts::ARCH)
}

#[cfg(target_os = "macos")]
fn get_os_info_macos() -> String {
    let version = std::process::Command::new("sw_vers")
        .arg("-productVersion")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    format!("macOS {}, {}", version, std::env::consts::ARCH)
}

#[cfg(target_os = "windows")]
fn get_os_info_windows() -> String {
    // Read from the registry via `ver`-like info or the environment.
    // `std::env::consts::OS` gives "windows"; supplement with version.
    let version = std::process::Command::new("cmd")
        .args(["/C", "ver"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "Windows (unknown version)".to_string());

    format!("{}, {}", version, std::env::consts::ARCH)
}

/// Collect CPU information.
pub fn get_cpu_info() -> String {
    #[cfg(target_os = "linux")]
    {
        get_cpu_info_linux()
    }
    #[cfg(target_os = "macos")]
    {
        get_cpu_info_macos()
    }
    #[cfg(target_os = "windows")]
    {
        get_cpu_info_windows()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        format!(
            "{} cores",
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1)
        )
    }
}

#[cfg(target_os = "linux")]
fn get_cpu_info_linux() -> String {
    let model = std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|content| {
            content
                .lines()
                .find(|l| l.starts_with("model name"))
                .and_then(|l| l.split(':').nth(1))
                .map(|s| s.trim().to_string())
        })
        .unwrap_or_else(|| "unknown".to_string());

    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);

    format!("{}, {} cores/threads available", model, cores)
}

#[cfg(target_os = "macos")]
fn get_cpu_info_macos() -> String {
    let brand = std::process::Command::new("sysctl")
        .args(["-n", "machdep.cpu.brand_string"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);

    format!("{}, {} cores/threads available", brand, cores)
}

#[cfg(target_os = "windows")]
fn get_cpu_info_windows() -> String {
    // Read the CPU name from the PROCESSOR_IDENTIFIER environment variable
    // or fall back to a wmic query.
    let model = cratonvm_types::flags::runtime_var("PROCESSOR_IDENTIFIER")
        .ok()
        .or_else(|| {
            std::process::Command::new("wmic")
                .args(["cpu", "get", "name", "/value"])
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .and_then(|s| {
                    s.lines()
                        .find(|l| l.starts_with("Name="))
                        .map(|l| l.trim_start_matches("Name=").trim().to_string())
                })
        })
        .unwrap_or_else(|| "unknown".to_string());

    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);

    format!("{}, {} cores/threads available", model, cores)
}

/// Collect memory information (total and available).
pub fn get_memory_info() -> String {
    #[cfg(target_os = "linux")]
    {
        get_memory_info_linux()
    }
    #[cfg(target_os = "macos")]
    {
        get_memory_info_macos()
    }
    #[cfg(target_os = "windows")]
    {
        get_memory_info_windows()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        "unknown".to_string()
    }
}

#[cfg(target_os = "linux")]
fn get_memory_info_linux() -> String {
    match std::fs::read_to_string("/proc/meminfo") {
        Ok(content) => {
            let mut total_kb: u64 = 0;
            let mut avail_kb: u64 = 0;
            for line in content.lines() {
                if line.starts_with("MemTotal:") {
                    total_kb = parse_meminfo_kb(line);
                } else if line.starts_with("MemAvailable:") {
                    avail_kb = parse_meminfo_kb(line);
                }
            }
            format!(
                "total={} MB, available={} MB",
                total_kb / 1024,
                avail_kb / 1024
            )
        }
        Err(_) => "unknown".to_string(),
    }
}

#[cfg(target_os = "linux")]
fn parse_meminfo_kb(line: &str) -> u64 {
    line.split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0)
}

#[cfg(target_os = "macos")]
fn get_memory_info_macos() -> String {
    let total = std::process::Command::new("sysctl")
        .args(["-n", "hw.memsize"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0);

    let total_mb = total / (1024 * 1024);
    // `vm_stat` provides free/inactive pages but requires parsing; just
    // report total for reliability.
    format!("total={} MB", total_mb)
}

#[cfg(target_os = "windows")]
fn get_memory_info_windows() -> String {
    // Use GlobalMemoryStatusEx via raw FFI.
    #[repr(C)]
    struct MemoryStatusEx {
        dw_length: u32,
        dw_memory_load: u32,
        ull_total_phys: u64,
        ull_avail_phys: u64,
        ull_total_page_file: u64,
        ull_avail_page_file: u64,
        ull_total_virtual: u64,
        ull_avail_virtual: u64,
        ull_avail_extended_virtual: u64,
    }

    extern "system" {
        fn GlobalMemoryStatusEx(lp_buffer: *mut MemoryStatusEx) -> i32;
    }

    let mut status = MemoryStatusEx {
        dw_length: std::mem::size_of::<MemoryStatusEx>() as u32,
        dw_memory_load: 0,
        ull_total_phys: 0,
        ull_avail_phys: 0,
        ull_total_page_file: 0,
        ull_avail_page_file: 0,
        ull_total_virtual: 0,
        ull_avail_virtual: 0,
        ull_avail_extended_virtual: 0,
    };

    let ok = unsafe { GlobalMemoryStatusEx(&mut status) };
    if ok != 0 {
        let total_mb = status.ull_total_phys / (1024 * 1024);
        let avail_mb = status.ull_avail_phys / (1024 * 1024);
        format!("total={} MB, available={} MB", total_mb, avail_mb)
    } else {
        "unknown".to_string()
    }
}

/// Return a summary of the VM state (version + the published class library).
///
/// We still hold no reference to the live `Vm` — crash handlers must be
/// self-contained and must not take VM locks — but the two facts that used to
/// be reported as "unavailable" (which class library, which collector) are
/// published lock-free at VM construction, so there is no reason to omit them.
/// The detailed per-collector counters live in the `V M  S T A T E` section.
pub fn get_vm_state() -> String {
    format!(
        "CratonVM 25.0 ({}, gc={})",
        String::from_utf8_lossy(jdk_mode_bytes()),
        ACTIVE_GC_ALGORITHM
            .get()
            .copied()
            .unwrap_or("<unpublished>"),
    )
}

/// Get heap information if available.
fn get_heap_info() -> String {
    // In a crash handler we cannot safely lock the VM heap. Report a
    // placeholder that documents this limitation.
    "heap information unavailable during crash (unsafe to inspect)".to_string()
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// The readability probe must reject a non-pointer.
    ///
    /// This is the regression for the defect that truncated every hardware-fault
    /// report whose registers held a non-pointer: the probe used to write to
    /// /dev/null, whose driver returns the byte count without ever reading the
    /// user buffer, so it called address `0x1` readable and the handler then
    /// dereferenced it and died. A pipe copies for real and returns EFAULT.
    #[cfg(unix)]
    #[test]
    fn probe_readable_rejects_a_non_pointer_through_a_pipe() {
        let mut fds = [-1i32; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        unsafe { libc::fcntl(fds[1], libc::F_SETFL, libc::O_NONBLOCK) };

        let live = [0u64; 8];
        assert!(
            probe_readable(fds[1], live.as_ptr() as u64, 64),
            "a live stack buffer must probe readable"
        );
        assert!(
            !probe_readable(fds[1], 1, 64),
            "address 0x1 must NOT probe readable - this is the whole point"
        );
        assert!(!probe_readable(fds[1], 0, 8), "null is never readable");
        assert!(
            !probe_readable(-1, live.as_ptr() as u64, 8),
            "no fd, no verdict"
        );

        unsafe {
            libc::close(fds[0]);
            libc::close(fds[1]);
        }
    }

    /// /dev/null is what the probe MUST NOT use. Pinning the reason down in a
    /// test keeps someone from "simplifying" the pipe back into an open of
    /// /dev/null: this asserts the broken behaviour is real.
    #[cfg(target_os = "linux")]
    #[test]
    fn dev_null_reports_even_a_bad_address_as_readable() {
        let fd = unsafe { libc::open(c"/dev/null".as_ptr(), libc::O_WRONLY) };
        assert!(fd >= 0);
        assert!(
            probe_readable(fd, 1, 64),
            "if this ever fails, /dev/null started faulting on bad buffers and \
             the pipe is no longer required"
        );
        unsafe { libc::close(fd) };
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn maps_range_parses_a_real_line() {
        let (start, end) = parse_maps_range(b"709281c12000-709281c14000 r-xp 00000000 00:00 0 ");
        assert_eq!(start, 0x709281c12000);
        assert_eq!(end, 0x709281c14000);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn maps_range_rejects_non_range_lines() {
        assert_eq!(parse_maps_range(b""), (0, 0));
        assert_eq!(parse_maps_range(b"not a maps line"), (0, 0));
        // A start with no end is not a usable range.
        assert_eq!(parse_maps_range(b"7f0000000000-"), (0, 0));
    }

    fn sample_crash_info() -> CrashInfo {
        CrashInfo {
            signal: 11,
            signal_name: "SIGSEGV".to_string(),
            pid: 12345,
            tid: 67890,
            timestamp: SystemTime::UNIX_EPOCH,
            thread_name: Some("main".to_string()),
            panic_message: Some("null pointer dereference".to_string()),
            panic_location: Some("vm/src/runtime/interpreter.rs:42".to_string()),
        }
    }

    #[test]
    fn crash_info_creation() {
        let info = sample_crash_info();
        assert_eq!(info.signal, 11);
        assert_eq!(info.signal_name, "SIGSEGV");
        assert_eq!(info.pid, 12345);
        assert_eq!(info.tid, 67890);
        assert_eq!(info.thread_name.as_deref(), Some("main"));
        assert_eq!(
            info.panic_message.as_deref(),
            Some("null pointer dereference")
        );
        assert_eq!(
            info.panic_location.as_deref(),
            Some("vm/src/runtime/interpreter.rs:42")
        );
    }

    #[test]
    fn crash_info_from_panic_fields() {
        // We cannot easily construct a PanicHookInfo, so test the
        // manual-construction path instead.
        let info = CrashInfo {
            signal: 0,
            signal_name: "RUST_PANIC".to_string(),
            pid: get_pid(),
            tid: get_tid(),
            timestamp: SystemTime::now(),
            thread_name: Some("test-thread".to_string()),
            panic_message: Some("assertion failed".to_string()),
            panic_location: Some("test.rs:1".to_string()),
        };
        assert_eq!(info.signal, 0);
        assert_eq!(info.signal_name, "RUST_PANIC");
        assert!(info.pid > 0);
        assert!(info.panic_message.is_some());
    }

    #[test]
    fn crash_report_contains_header() {
        let info = sample_crash_info();
        let report = generate_crash_report(&info);
        assert!(
            report.contains("A fatal error has been detected by the CratonVM Runtime Environment")
        );
        assert!(report.contains("SIGSEGV"));
        assert!(report.contains("pid=12345"));
        assert!(report.contains("tid=67890"));
    }

    #[test]
    fn crash_report_contains_thread_section() {
        let info = sample_crash_info();
        let report = generate_crash_report(&info);
        assert!(report.contains("T H R E A D"));
        assert!(report.contains("\"main\""));
        assert!(report.contains("null pointer dereference"));
    }

    #[test]
    fn crash_report_contains_process_section() {
        let info = sample_crash_info();
        let report = generate_crash_report(&info);
        assert!(report.contains("P R O C E S S"));
        assert!(report.contains("VM state:"));
        assert!(report.contains("Heap:"));
    }

    #[test]
    fn crash_report_contains_system_section() {
        let info = sample_crash_info();
        let report = generate_crash_report(&info);
        assert!(report.contains("S Y S T E M"));
        assert!(report.contains("OS:"));
        assert!(report.contains("CPU:"));
        assert!(report.contains("Memory:"));
    }

    #[test]
    fn crash_report_contains_jre_version() {
        let info = sample_crash_info();
        let report = generate_crash_report(&info);
        assert!(report.contains("JRE version: CratonVM 25.0"));
    }

    #[test]
    fn crash_report_rust_panic_format() {
        let info = CrashInfo {
            signal: 0,
            signal_name: "RUST_PANIC".to_string(),
            pid: 999,
            tid: 111,
            timestamp: SystemTime::now(),
            thread_name: Some("worker-1".to_string()),
            panic_message: Some("index out of bounds".to_string()),
            panic_location: Some("src/lib.rs:100".to_string()),
        };
        let report = generate_crash_report(&info);
        assert!(report.contains("RUST_PANIC"));
        assert!(report.contains("src/lib.rs:100"));
        assert!(report.contains("index out of bounds"));
        assert!(report.contains("\"worker-1\""));
    }

    #[test]
    fn write_crash_report_to_file() {
        let info = sample_crash_info();
        let dir = std::env::temp_dir();
        let path = dir.join("hs_err_pid_test.log");

        write_crash_report(&info, &path).expect("write should succeed");

        let content = std::fs::read_to_string(&path).expect("read back");
        assert!(content.contains("SIGSEGV"));
        assert!(content.contains("T H R E A D"));
        assert!(content.contains("S Y S T E M"));

        // Cleanup.
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn os_info_non_empty() {
        let info = get_os_info();
        assert!(!info.is_empty(), "OS info should not be empty");
        assert!(info.len() > 3, "OS info should have meaningful content");
    }

    #[test]
    fn cpu_info_non_empty() {
        let info = get_cpu_info();
        assert!(!info.is_empty(), "CPU info should not be empty");
        assert!(
            info.contains("core") || info.contains("thread") || info.contains("unknown"),
            "CPU info should mention cores/threads: {}",
            info
        );
    }

    #[test]
    fn memory_info_non_empty() {
        let info = get_memory_info();
        assert!(!info.is_empty(), "Memory info should not be empty");
    }

    #[test]
    fn vm_state_non_empty() {
        let state = get_vm_state();
        assert!(!state.is_empty());
        assert!(state.contains("CratonVM"));
    }

    #[test]
    fn timestamp_formatting() {
        let t = SystemTime::UNIX_EPOCH;
        let s = format_timestamp(t);
        assert_eq!(s, "1970-01-01T00:00:00Z");
    }

    #[test]
    fn days_to_ymd_epoch() {
        let (y, m, d) = days_to_ymd(0);
        assert_eq!((y, m, d), (1970, 1, 1));
    }

    #[test]
    fn itoa_into_buf_zero() {
        let mut buf = [0u8; 8];
        let n = itoa_into_buf(&mut buf, 0);
        assert_eq!(&buf[..n], b"0");
    }

    #[test]
    fn itoa_into_buf_small() {
        let mut buf = [0u8; 8];
        let n = itoa_into_buf(&mut buf, 1);
        assert_eq!(&buf[..n], b"1");

        let n = itoa_into_buf(&mut buf, 9);
        assert_eq!(&buf[..n], b"9");

        let n = itoa_into_buf(&mut buf, 10);
        assert_eq!(&buf[..n], b"10");

        let n = itoa_into_buf(&mut buf, 42);
        assert_eq!(&buf[..n], b"42");
    }

    #[test]
    fn itoa_into_buf_large() {
        let mut buf = [0u8; 32];
        let n = itoa_into_buf(&mut buf, 1_000_000);
        assert_eq!(&buf[..n], b"1000000");

        let n = itoa_into_buf(&mut buf, u64::MAX);
        assert_eq!(&buf[..n], b"18446744073709551615");
    }

    #[test]
    fn itoa_into_buf_empty_returns_zero() {
        let mut buf = [];
        assert_eq!(itoa_into_buf(&mut buf, 123), 0);
    }

    #[test]
    fn days_to_ymd_known_date() {
        // 2024-01-01 is day 19723 since epoch.
        let (y, m, d) = days_to_ymd(19723);
        assert_eq!((y, m, d), (2024, 1, 1));
    }

    // The async-signal-safe `write_all` only exists on Unix.
    #[cfg(unix)]
    #[test]
    fn write_all_handles_short_writes_via_pipe() {
        use super::async_signal_safe::write_all;
        use std::io::Read;

        // A pipe's kernel buffer is finite, so writing a payload larger than
        // the buffer forces `write(2)` to return short — exactly the partial
        // write the resume-from-offset loop must handle. A reader thread drains
        // the pipe so the writes can complete.
        let mut fds = [0i32; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "pipe() failed");
        let (read_fd, write_fd) = (fds[0], fds[1]);

        // Build a payload comfortably larger than a typical 64 KiB pipe buffer
        // and with a recognizable, position-dependent byte pattern so any
        // dropped/duplicated/reordered chunk is detected.
        let payload: Vec<u8> = (0..(512 * 1024)).map(|i| (i % 251) as u8).collect();

        let reader = {
            let expected_len = payload.len();
            std::thread::spawn(move || {
                let mut f = unsafe {
                    <std::fs::File as std::os::unix::io::FromRawFd>::from_raw_fd(read_fd)
                };
                let mut got = Vec::with_capacity(expected_len);
                f.read_to_end(&mut got).expect("read pipe");
                got
            })
        };

        write_all(write_fd, &payload);
        // Close the write end so the reader sees EOF.
        unsafe { libc::close(write_fd) };

        let got = reader.join().expect("reader thread");
        assert_eq!(got.len(), payload.len(), "byte count mismatch");
        assert_eq!(got, payload, "payload corrupted across short writes");
    }

    // Writing to a closed/invalid fd must return promptly (real error, not
    // EINTR) rather than spinning forever.
    #[cfg(unix)]
    #[test]
    fn write_all_on_bad_fd_returns() {
        use super::async_signal_safe::write_all;
        // -1 is never a valid fd; `write` returns EBADF, which is not EINTR,
        // so the loop must give up immediately.
        write_all(-1, b"this should not hang");
    }

    // ── VM diagnostic snapshot ─────────────────────────────────────────────
    //
    // The publication cells are process-global `OnceLock`s / atomics, so these
    // tests are written to be order-independent: they assert invariants that
    // hold whether or not some earlier test in the same binary already
    // published a mode. `publish_and_report_are_consistent` does the one
    // publish and then pins every consumer against it.

    #[test]
    fn jdk_mode_bytes_is_one_of_three_static_literals() {
        // Must never allocate and never panic, at any publication state — the
        // Unix signal handler calls it.
        let b = jdk_mode_bytes();
        assert!(
            b == b"real-jdk" || b == b"synthetic-jdk" || b == b"<unpublished>",
            "unexpected jdk_mode_bytes(): {:?}",
            String::from_utf8_lossy(b)
        );
    }

    #[test]
    fn jdk_mode_line_never_silently_omits_the_mode() {
        // Either it names a mode, or it says out loud that it could not — the
        // one thing it must never do is render an empty/ambiguous string, which
        // is what the pre-change hardware-fault report effectively did.
        let line = jdk_mode_line();
        assert!(line.starts_with("jdk mode: "), "got {line:?}");
        assert!(line.len() > "jdk mode: ".len(), "got {line:?}");
    }

    #[test]
    fn publish_and_report_are_consistent() {
        // First-write-wins, so whichever test/VM published first owns the
        // value; publishing again must not change it and must not make the
        // byte form disagree with the rich form.
        publish_jdk_mode(crate::config::JdkMode::Synthetic, Some("/nonexistent/jdk"));
        publish_jdk_mode(crate::config::JdkMode::Real, None);

        let mode = active_jdk_mode().expect("a mode is published by now");
        let expected: &[u8] = match mode {
            crate::config::JdkMode::Real => b"real-jdk",
            crate::config::JdkMode::Synthetic => b"synthetic-jdk",
        };
        assert_eq!(
            jdk_mode_bytes(),
            expected,
            "signal-safe byte form disagrees with the OnceLock form"
        );
        assert!(
            jdk_mode_line().contains(mode.as_str()),
            "jdk_mode_line() must name the published mode"
        );
    }

    #[test]
    fn gc_state_lines_report_collector_and_the_moving_verdict() {
        let joined = gc_state_lines_for("generational").join("\n");
        assert!(joined.contains("gc collector: generational"), "{joined}");
        // The policy/actual split is the whole point: a report that only said
        // "generational" would not distinguish a compacting young generation
        // from one permanently diverted to the non-moving sweep.
        assert!(joined.contains("gc young-gen policy:"), "{joined}");
        assert!(joined.contains("gc young-gen actual:"), "{joined}");
        assert!(
            joined.contains("moving cycle(s)") && joined.contains("NON-MOVING sweep"),
            "the actual line must carry both counters: {joined}"
        );
        // And the collector line is not merely echoed back: an unpublished
        // collector must not be reported as a known one.
        assert!(gc_state_lines_for("<unpublished>")
            .join("\n")
            .contains("gc collector: <unpublished>"));
    }

    /// The counters behind `young-gen policy` / `young-gen actual` are
    /// incremented only by `gen_heap.rs`, so under any other collector they are
    /// zero by construction. Printing them there reads as a measurement of that
    /// collector and is how the Tomcat G1 SIGSEGV page
    /// (fixed-suite-bugs/tomcat/g1-sigsegv-unguarded-callee-jit-frame-FIXED.md)
    /// came to blame a moving young collector that was never running.
    #[test]
    fn gc_state_lines_omit_generational_only_counters_under_another_collector() {
        for collector in ["g1", "zgc", "<unpublished>"] {
            let joined = gc_state_lines_for(collector).join("\n");
            assert!(
                joined.contains(&format!("gc collector: {collector}")),
                "{joined}"
            );
            assert!(
                !joined.contains("gc young-gen policy:"),
                "{collector}: the generational policy line must not appear: {joined}"
            );
            assert!(
                !joined.contains("gc young-gen actual:"),
                "{collector}: the generational counters must not appear: {joined}"
            );
            assert!(
                !joined.contains("Cheney young copy"),
                "{collector}: must not name the generational young collector: {joined}"
            );
            assert!(
                joined.contains("gc young-gen policy/actual: n/a"),
                "{collector}: their absence must be stated, not silent: {joined}"
            );
        }
    }

    #[test]
    fn jit_state_lines_attribute_a_faulting_pc() {
        // No `fault_pc`: the pc-attribution line must be absent rather than
        // fabricated.
        let without = jit_state_lines(None).join("\n");
        assert!(without.contains("guarded compiled frames live process-wide:"));
        assert!(!without.contains("faulting pc"), "{without}");

        // With a pc that is certainly not in any compiled code range, the
        // report must say so explicitly (or explain that names are off) rather
        // than staying silent.
        let with = jit_state_lines(Some(1)).join("\n");
        assert!(with.contains("faulting pc"), "{with}");
    }

    #[test]
    fn java_stack_lines_never_block_and_always_explain_themselves() {
        // Unpublished, empty, or contended — every branch must produce a line
        // that says which case it is. A crash report with a silently missing
        // Java stack is indistinguishable from one with no Java frames.
        let lines = java_stack_lines(8);
        assert!(!lines.is_empty());
        assert!(lines[0].starts_with("Java frames"), "got {:?}", lines[0]);
    }

    /// CR-VXC-1. Each of these runs inside its own spawned thread: the
    /// publication cell is thread-local and cargo's harness reuses threads, so
    /// publishing on the harness thread would leak into unrelated tests.
    fn published_trace(class: &str, method: &str, line: i32) -> crate::vm::PublishedTraceHandle {
        std::sync::Arc::new(parking_lot::Mutex::new(vec![
            cratonvm_native_api::StackTraceEntry {
                class_name: class.into(),
                method_name: method.into(),
                source_file: Some("Probe.java".into()),
                line_number: line,
                byte_code_index: 0,
                class_id: None,
                method_index: None,
            },
        ]))
    }

    #[test]
    fn a_worker_thread_crash_now_renders_that_workers_java_frames() {
        // The whole point of CR-VXC-1: before it, a fault anywhere but the
        // primordial thread rendered the primordial thread's frames (or the
        // "not published" placeholder) and the reader had no way to tell.
        let lines = std::thread::spawn(|| {
            let _guard = crate::vm::PublishedFrameTrace::publish(
                "worker-7",
                7,
                published_trace("com/example/Worker", "run", 42),
            );
            java_stack_lines(8)
        })
        .join()
        .expect("worker thread");

        let joined = lines.join("\n");
        assert!(joined.contains("worker-7"), "{joined}");
        assert!(joined.contains("tid 7"), "{joined}");
        assert!(joined.contains("com/example/Worker.run"), "{joined}");
        assert!(joined.contains("Probe.java:42"), "{joined}");
        assert!(
            !joined.contains("primordial"),
            "the faulting thread's frames must not be labelled primordial: {joined}"
        );
    }

    #[test]
    fn an_unpublished_thread_still_falls_back_to_the_primordial_path() {
        // Strictly additive: the helper returns `None` wherever nothing is
        // published (the primordial thread included), so today's behaviour is
        // preserved everywhere it was the right behaviour.
        let lines = std::thread::spawn(|| java_stack_lines(8))
            .join()
            .expect("worker thread");
        assert!(!lines.is_empty());
        assert!(lines[0].starts_with("Java frames"), "got {:?}", lines[0]);
        assert!(
            !lines[0].contains("faulting thread"),
            "unpublished thread must not claim a faulting-thread trace: {:?}",
            lines[0]
        );
    }

    #[test]
    fn the_full_vm_state_section_carries_the_faulting_threads_frames() {
        // `vm_diagnostic_lines` is what both allocating report paths call
        // (`CrashReport::render` and the Windows VEH), so the wiring has to be
        // visible from there and not only from `java_stack_lines` directly.
        let lines = std::thread::spawn(|| {
            let _guard = crate::vm::PublishedFrameTrace::publish(
                "carrier-3",
                3,
                published_trace("com/example/Mounted", "loop", 9),
            );
            vm_diagnostic_lines(None)
        })
        .join()
        .expect("carrier thread");

        let joined = lines.join("\n");
        assert!(joined.contains("jdk mode: "), "{joined}");
        assert!(joined.contains("carrier-3"), "{joined}");
        assert!(joined.contains("com/example/Mounted.loop"), "{joined}");
    }

    #[test]
    fn a_held_frame_trace_mutex_is_reported_rather_than_waited_on() {
        // The actual crash-time situation: the faulting thread died in the
        // middle of republishing its own trace. A crash handler that blocks
        // here turns a diagnosable crash into a hang.
        let lines = std::thread::spawn(|| {
            let trace = published_trace("com/example/Wedged", "spin", 1);
            let _guard = crate::vm::PublishedFrameTrace::publish("wedged", 11, trace.clone());
            let _held = trace.lock();
            java_stack_lines(8)
        })
        .join()
        .expect("wedged thread");

        let joined = lines.join("\n");
        assert!(joined.contains("not waiting on it"), "{joined}");
        assert!(joined.contains("wedged"), "{joined}");
    }

    #[test]
    fn crash_report_carries_the_vm_state_section() {
        let info = sample_crash_info();
        let report = generate_crash_report(&info);
        assert!(report.contains("V M  S T A T E"), "{report}");
        assert!(report.contains("jdk mode: "), "{report}");
        assert!(report.contains("gc collector: "), "{report}");
        assert!(report.contains("jit: "), "{report}");
        assert!(report.contains("Java frames"), "{report}");
    }

    #[test]
    fn crash_report_header_names_the_class_library() {
        // Truncated reports lose the tail first, so the mode is duplicated into
        // the header. Guard the duplication so a future refactor cannot quietly
        // drop it back to the VM section only.
        let info = sample_crash_info();
        let report = generate_crash_report(&info);
        let header_end = report
            .find("---------------  T H R E A D")
            .expect("thread section marker");
        assert!(
            report[..header_end].contains("jdk mode: "),
            "header must name the class library: {}",
            &report[..header_end]
        );
    }

    #[test]
    fn vm_state_summary_reports_mode_and_collector() {
        let s = get_vm_state();
        assert!(s.starts_with("CratonVM 25.0 ("), "{s}");
        assert!(s.contains("gc="), "{s}");
        assert!(
            !s.contains("detailed VM info unavailable"),
            "the mode and collector are published lock-free; they are available: {s}"
        );
    }

    /// Every pseudo-index reads its OWN machine register, and an index the
    /// table does not know reports 0 rather than a plausible wrong one.
    ///
    /// The accessor used to end in `_ => libc::REG_RBP`. With four callers that
    /// was harmless. With the whole general-purpose file now printed, one
    /// mistyped arm would put `rbp`'s value under `r13`'s label — and a crash
    /// report is read once, long after the process is gone, by someone who
    /// cannot re-run the fault to check. The mapping is therefore stated twice,
    /// here and in the accessor, so a typo in either shows up as a mismatch.
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    fn every_gp_register_index_reads_its_own_slot() {
        let table: [(usize, libc::c_int, &str); 16] = [
            (GREG_RAX, libc::REG_RAX, "rax"),
            (GREG_RBX, libc::REG_RBX, "rbx"),
            (GREG_RCX, libc::REG_RCX, "rcx"),
            (GREG_RDX, libc::REG_RDX, "rdx"),
            (GREG_RSI, libc::REG_RSI, "rsi"),
            (GREG_RDI, libc::REG_RDI, "rdi"),
            (GREG_R8, libc::REG_R8, "r8"),
            (GREG_R9, libc::REG_R9, "r9"),
            (GREG_R10, libc::REG_R10, "r10"),
            (GREG_R11, libc::REG_R11, "r11"),
            (GREG_R12, libc::REG_R12, "r12"),
            (GREG_R13, libc::REG_R13, "r13"),
            (GREG_R14, libc::REG_R14, "r14"),
            (GREG_R15, libc::REG_R15, "r15"),
            (GREG_RSP, libc::REG_RSP, "rsp"),
            (GREG_RBP, libc::REG_RBP, "rbp"),
        ];

        let mut uc: libc::ucontext_t = unsafe { std::mem::zeroed() };
        // A distinct, non-zero value per machine register, written through the
        // libc index, so nothing the accessor does can produce it by accident.
        for (i, (_, libc_idx, _)) in table.iter().enumerate() {
            uc.uc_mcontext.gregs[*libc_idx as usize] = 0x1000 + i as libc::greg_t;
        }
        let ucp = (&mut uc as *mut libc::ucontext_t).cast::<std::ffi::c_void>();

        for (i, (pseudo, _, name)) in table.iter().enumerate() {
            assert_eq!(
                greg_from_ucontext(ucp, *pseudo),
                0x1000 + i as u64,
                "the pseudo-index for {name} read a different register's slot"
            );
        }

        // The pseudo-indices are 0..=15; anything past the table is a caller
        // bug and must report 0, not the last arm's register.
        assert_eq!(
            greg_from_ucontext(ucp, table.len()),
            0,
            "an unknown pseudo-index must report 0, never a plausible register"
        );
        assert_eq!(
            greg_from_ucontext(std::ptr::null_mut(), GREG_RAX),
            0,
            "a null ucontext must report 0"
        );
    }

    /// `si_addr` is only an address when `si_code` says a fault produced it.
    ///
    /// The signal handler's decode and its NULL-receiver line both hang off
    /// this. Get it wrong in the permissive direction and a `kill -SEGV` — the
    /// ordinary way to prod a wedged VM — prints a confident verdict about a
    /// fault that never happened, in a report whose whole value is that it is
    /// trustworthy.
    #[cfg(unix)]
    #[test]
    fn only_a_hardware_fault_has_a_faulting_address() {
        // Per-signal fault codes: SEGV_MAPERR/SEGV_ACCERR/SEGV_BNDERR/
        // SEGV_PKUERR, and BUS_ADRALN/BUS_ADRERR/BUS_OBJERR, are 1..=4.
        for code in 1..=4 {
            assert!(
                si_code_means_a_faulting_address(code),
                "si_code {code} is a hardware fault"
            );
        }
        // SI_USER (kill), SI_QUEUE (sigqueue), SI_TKILL (tgkill): sent, so
        // si_addr is an unrelated union member.
        for code in [0, -1, -6] {
            assert!(
                !si_code_means_a_faulting_address(code),
                "si_code {code} was SENT, si_addr is not an address"
            );
        }
        // SI_KERNEL is kernel-raised but still carries no faulting address.
        assert!(!si_code_means_a_faulting_address(0x80));
    }

    /// The indexed-load search, against the registers a real crash carried, and
    /// against the two that must NOT match it.
    ///
    /// `decode_indexed_load` had its own test for the hibernate row already;
    /// what is new here is that the search is now shared with the signal
    /// handler, so this pins the part both callers depend on — including that
    /// the total hit count is the TOTAL, not the number rendered, since the
    /// Windows path prints `...` off exactly that difference.
    #[test]
    fn the_shared_indexed_load_search_matches_what_the_windows_reporter_reports() {
        // hs_err_pid3512, g1: fault == r10 + rax*4.
        let regs = [("rax", 0xEAF8_2DA0u64), ("r10", 0x7FF6_35D7_9014u64)];
        let mut seen = Vec::new();
        let hits = for_each_indexed_load_match(&regs, 0x0000_7FF9_E1B8_4694, |b, i, s, n| {
            seen.push((b.to_string(), i.to_string(), s, n))
        });
        assert_eq!(hits, 1, "exactly one pair explains this address");
        assert_eq!(seen[0].0, "r10");
        assert_eq!(seen[0].1, "rax");
        assert_eq!(seen[0].2, 4);
        // The rendering the Windows path builds on top of the same search.
        assert!(decode_indexed_load(&regs, 0x0000_7FF9_E1B8_4694)
            .expect("must decode")
            .contains("[r10+rax*4]"));

        // The two Tomcat SIGSEGVs of 2026-08-22. Neither is an indexed load,
        // and that is the point: nothing may attach the hibernate family's
        // analysis to them.
        for (addr, r10) in [
            (0xfusize, 0x708e_467f_6e48u64),
            (0x5, 0x8000_0000_0000_0000),
        ] {
            let regs = [("r10", r10), ("rax", 0)];
            assert_eq!(
                for_each_indexed_load_match(&regs, addr, |_, _, _, _| {}),
                0,
                "addr {addr:#x} must not decode as an indexed load"
            );
            assert!(decode_indexed_load(&regs, addr).is_none());
        }

        // A zero address is not decodable, however degenerate the registers.
        assert_eq!(
            for_each_indexed_load_match(&[("rax", 7), ("rbx", 9)], 0, |_, _, _, _| {}),
            0
        );
    }
}
