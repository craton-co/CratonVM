// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

#![deny(deprecated)]
// site_keyed_memos! recurses once per doc-comment line on its memos; W4B's notes exceeded the default 128.
#![recursion_limit = "256"]

//! CratonVM — A Java Virtual Machine implemented in Rust.
//!
//! This crate provides the core VM implementation including:
//!
//! - **Bytecode interpreter** with 140+ fast-path opcodes
//! - **x86-64 JIT compiler** with LICM, BCE, AVX2 SIMD, and OSR
//! - **Generational garbage collector** with write barriers and card table
//! - **Class loading** with verification, resolution, and initialization
//! - **3,100+ native method registrations** for the Java standard library
//! - **Multi-threading** with monitors, locks, and virtual threads
//!
//! # Crate Structure
//!
//! - [`vm`] — Main VM orchestrator (`Vm`, `SharedVm`)
//! - [`runtime`] — Interpreter, frames, call stack, exception handling
//! - [`jit`] — x86-64 JIT compiler and executable buffer management
//! - [`memory`] — Heap allocation, generational GC, root scanning
//! - [`classloading`] — Class loading, linking, verification, resolution
//! - [`native`] — Native method registry and implementations
//! - [`threading`] — Thread management, monitors, virtual thread scheduler
//! - [`types`] — JVM value representation with SoA layout
//!
//! # Embedding
//!
//! For a worked example of embedding CratonVM as a library in a Rust
//! application, see [`docs/EMBEDDING.md`](https://github.com/craton-co/CVM/blob/main/docs/EMBEDDING.md)
//! in the workspace root. The first-party `vm-cli` binary is the
//! reference embedder.

pub mod classloading;
pub mod config;
#[cfg(feature = "experimental-debug")]
pub mod debug;

/// gcj j1/st item 2: `-XX:+PrintStringTableStatistics`' exit block (the
/// Symbol and String tables' statistics on stdout), once per VM and only under
/// the flag. The launcher calls this on every exit arm, after the
/// `safepoint,stats` exit summary.
pub fn print_string_table_statistics_at_exit(shared: &vm::SharedVm) {
    memory::string_table::print_string_table_statistics_at_exit(shared);
}

/// JDWP `VM_DEATH` for an attached debugger (`debug::report_vm_death`); a
/// no-op in a build without `experimental-debug`, which has no JDWP server.
/// The launcher calls this on both exit arms, whatever the build's features.
pub fn report_debugger_vm_death(shared: &vm::SharedVm) {
    #[cfg(feature = "experimental-debug")]
    debug::report_vm_death(shared);
    #[cfg(not(feature = "experimental-debug"))]
    let _ = shared;
}
/// Start the JDWP server `-agentlib:jdwp` asked for, at the point HotSpot's
/// back end gets `VMInit` (`debug::start_jdwp_agent`; interpreter round i4
/// wave 85, lane L1). `Ok` without the option, and always in a build without
/// `experimental-debug` (whose launcher leaves `-agentlib:jdwp` to the
/// startup-agent path, which refuses it with HotSpot's missing-library
/// error).
pub fn start_jdwp_agent(
    shared: &std::sync::Arc<vm::SharedVm>,
    thread: &mut threading::jvm_thread::JvmThread,
) -> Result<(), config::JdwpAgentFailure> {
    #[cfg(feature = "experimental-debug")]
    {
        debug::start_jdwp_agent(shared, thread)
    }
    #[cfg(not(feature = "experimental-debug"))]
    {
        let _ = (shared, thread);
        Ok(())
    }
}
/// The `CRATONVM_DBG=access` exit total of `shared`'s JVMS §5.4.4 member-access
/// census (one line; nothing when the category is off). The launcher calls this
/// on both exit arms. Interpreter round i1 wave 27, lane L5. Followed by the
/// loader census (`[ACCESS-DBG] LOADER census: …`: loader throws that failed a
/// resolution, racing resolutions that adopted another thread's outcome;
/// wave 38, lane L5). Then the
/// `[direct-throwable] ...` line of `CRATONVM_DBG_DIRECT_THROWABLE_BUILD` (JIT
/// round 23 wave 1b, lane L3: throwables built directly, counted against
/// those built through their constructors; nothing with the flag unset).
pub fn report_member_access_census_at_exit(shared: &vm::SharedVm) {
    runtime::interpreter::field_access::report_member_access_census_at_exit(shared);
    runtime::resolve::loader_throw::report_loader_census_at_exit(shared);
    runtime::exceptions::report_direct_throwable_census_at_exit(shared);
}
/// The `CRATONVM_DBG_UNRECORDED_WAIT_KINDS` exit table of `shared`: every
/// blocking region that opened with no recorded wait kind, by the Rust call
/// site of its door (nothing when the census is off). The launcher calls this
/// on both exit arms. Interpreter round i2 wave 66, lane L1
/// (`docs/feature-designs/real-jdk-mode.md`).
pub fn report_unrecorded_wait_regions_at_exit(shared: &vm::SharedVm) {
    shared
        .threads
        .thread_registry
        .report_unrecorded_wait_regions();
}
pub use vm::vm_init::warm_metaspace_tally;
/// `(class name, source)` of each class defined from the application class path,
/// the source as the loader recorded it (a class path directory, for one found there). A boot image whose capture loaded the main class lists
/// these files and checks them at restore.
pub fn application_class_sources(shared: &vm::SharedVm) -> Vec<(String, String)> {
    let cm = shared.classes.class_manager.read();
    cm.class_store()
        .iter()
        .filter_map(|c| match &c.origin {
            cratonvm_classloading::ClassOrigin::ApplicationClassPath { source } => Some((c.name.to_string(), source.to_string())),
            _ => None,
        })
        .collect()
}
/// Unlink the jvmstat perf-data file `shared` wrote
/// (`CRATONVM_PERF_DATA_FILE`), as HotSpot's `perfMemory_exit` does before
/// the process exits; a no-op when none was written. The launcher calls this
/// on both exit arms (the `System.exit` one from its pre-exit hook), since
/// `std::process::exit` drops no VM. Interpreter round i3 wave 81, lane L1.
pub fn remove_perf_data_file_at_exit(shared: &vm::SharedVm) {
    // The sampler first, then the file, outside the lock (wave 87).
    runtime::perf_data_file::stop_sampler(shared);
    let file = shared.debug.perf_data_file.lock().take();
    drop(file);
}
/// Abort the process as HotSpot's `os::abort` does on Unix (`::abort()`,
/// `SIGABRT`), with the VM's crash handler taken off `SIGABRT` first so no
/// `hs_err` report is written: HotSpot installs no handler for that signal.
/// The launcher's JPLIS `abortJVM` path (interpreter round i4 wave 84, lane
/// L1). Unix only; HotSpot's Windows `os::abort` is `ExitProcess(1)`.
#[cfg(unix)]
pub fn abort_without_crash_report() -> ! {
    // SAFETY: restores the default disposition of one signal; the process
    // aborts on the next line.
    unsafe {
        libc::signal(libc::SIGABRT, libc::SIG_DFL);
    }
    std::process::abort()
}
pub mod dispatch_trace;
/// G1 parallel-evacuation CAS losses — see
/// [`cratonvm_gc::g1::evacuate_cas_loser_forwards`]. Re-exported so `vm-cli`
/// can print it beside the other exit counters without taking a direct
/// dependency on the gc crate for one number.
pub fn g1_evacuate_cas_loser_forwards() -> u64 {
    cratonvm_gc::g1::evacuate_cas_loser_forwards()
}
/// G1 parallel-evacuation waits on a peer's unpublished claim — see
/// [`cratonvm_gc::g1::parallel_evac_claim_waits`].
pub fn g1_parallel_evac_claim_waits() -> u64 {
    cratonvm_gc::g1::parallel_evac_claim_waits()
}

pub mod error;
pub mod jck_capture;
pub mod jit;
#[cfg(feature = "experimental-debug")]
pub mod jvmti;
pub mod memory;
pub mod native;
pub mod runtime;
pub mod threading;
pub mod types;
pub mod vm;

pub use classloading::{
    Class, ClassId, ClassLoaderId, ClassPath, ClassState, ClassStore, ManifestInfo,
};
pub use config::VmConfig;
pub use error::{MethodCallFailed, MethodCallResult, VmError};
pub use threading::{JvmThread, ThreadId};
pub use vm::{SharedVm, StackTraceFrame, Vm};

/// The map-view rebuild-elision census (`CRATONVM_DBG=map-view-cache`),
/// re-exported so the CLI can print it at exit without taking a direct
/// dependency on `cratonvm-native-collections`.
pub use cratonvm_native_collections::report_map_view_cache_at_exit;

/// The `CRATONVM_DBG_WATCH_PUN` census, re-exported for the same reason. Its
/// two DENOMINATORS are why it exists: a watch that reports no punned cell has
/// said nothing until it also says how many times it looked.
/// Engagement census for the blocked-peer native-stack remap, as
/// `(captured, adopted, written, skipped, discarded, dropped, unrouted,
/// below_wake, enabled)`.
///
/// Re-exported because `vm-cli` does not link `cratonvm-gc` directly, the same
/// reason `evacuate_cas_loser_forwards` above is. `written == 0` means the
/// repair never engaged on the run and nothing may be concluded from its
/// result -- see `gc_quiescence::PEER_STACK_SLOTS_CAPTURED`.
pub fn blocked_peer_stack_remap_census() -> (u64, u64, u64, u64, u64, u64, u64, u64, bool) {
    use std::sync::atomic::Ordering;
    (
        cratonvm_gc::gc_quiescence::PEER_STACK_SLOTS_CAPTURED.load(Ordering::Relaxed),
        cratonvm_gc::gc_quiescence::PEER_STACK_SLOTS_ADOPTED.load(Ordering::Relaxed),
        cratonvm_gc::gc_quiescence::PEER_STACK_SLOTS_WRITTEN.load(Ordering::Relaxed),
        cratonvm_gc::gc_quiescence::PEER_STACK_SLOTS_SKIPPED.load(Ordering::Relaxed),
        cratonvm_gc::gc_quiescence::PEER_STACK_SLOTS_DISCARDED.load(Ordering::Relaxed),
        cratonvm_gc::gc_quiescence::PEER_STACK_SLOTS_DROPPED.load(Ordering::Relaxed),
        cratonvm_gc::gc_quiescence::PEER_STACK_SLOTS_UNROUTED.load(Ordering::Relaxed),
        cratonvm_gc::gc_quiescence::PEER_STACK_SLOTS_REFUSED_BELOW_WAKE.load(Ordering::Relaxed),
        cratonvm_gc::gc_quiescence::blocked_peer_stack_remap_enabled(),
    )
}

/// G1 `is_object_address`'s `(calls, accepted, nanos)`.
pub use cratonvm_gc::g1_object_address_census;
/// The collector's own account of the last cycle and the decision histogram
/// behind it. Re-exported because `vm-cli` prints it at shutdown under
/// `--verbose:gc` / `CRATONVM_GC_STATS` and does not depend on `cratonvm-gc`
/// directly.
pub use cratonvm_gc::gc_metrics::collector_decision_report;
/// G1's `G1YoungGenSizer` texts, for the launcher's
/// `VmConfig::hotspot_gc_ergo_warnings` on a command line naming no collector.
pub use cratonvm_gc::g1::g1_young_sizer_from_flags;
/// gen r5w2/obs6: `vm-cli` publishes the VM's allocation total before its
/// shutdown GC summary, so `[GC] cards/alloc:` reports allocation, not occupancy.
pub use cratonvm_gc::gc_metrics::record_vm_allocation_totals;
/// The GC-trigger publish verifier's `(checks, divergences)` -- see
/// `cratonvm_gc::gen_heap::gc_trigger_verify_counts`. Re-exported for the same
/// reason its neighbours are: `vm-cli` depends on `cratonvm-vm`, not on
/// `cratonvm-gc`.
pub use cratonvm_gc::gen_heap::gc_trigger_verify_counts;
/// `(hits, misses)` for the exact object-start fast path in
/// `is_object_address` (`CRATONVM_GC_OBJECT_STARTS`). Re-exported for the
/// shutdown census in `vm-cli`.
pub use cratonvm_gc::gen_heap::object_start_counts;
/// Bytes the generational young collector has returned to the OS
/// (`CRATONVM_GEN_UNCOMMIT`). Re-exported for the shutdown census in `vm-cli`,
/// which does not depend on `cratonvm-gc` directly.
pub use cratonvm_gc::gen_heap::young_bytes_uncommitted;
/// `-XX:MaxTenuringThreshold` / `-XX:InitialTenuringThreshold` /
/// `-XX:TargetSurvivorRatio` / `-XX:+PrintTenuringDistribution`: the request
/// type `VmConfig::tenuring` carries (gen r4w6/young6). Re-exported because
/// `vm-cli` does not depend on `cratonvm-gc` directly.
pub use cratonvm_gc::gen_heap::{TenuringConfig, DEFAULT_MAX_TENURING_THRESHOLD};
/// HotSpot 25's young-size ergonomics texts for the launcher
/// (`VmConfig::hotspot_gc_ergo_warnings`): Serial's `GenArguments`, G1's
/// `G1YoungGenSizer`, and Serial's bootstrap failure on a too-small eden.
pub use cratonvm_gc::gen_young_sizing::{
    hotspot_gc_before_init_lines, hotspot_serial_boot_failing_eden,
    hotspot_serial_young_ergonomics, SerialYoungFlags,
};
/// `-Xmn` / `-XX:NewSize` / `-XX:MaxNewSize` / `-XX:NewRatio`: the request type
/// `VmConfig::young_gen_sizing` carries and the resolver the launcher prints
/// its clamp notes from (gen r4w2/alloc2). Re-exported because `vm-cli` does
/// not depend on `cratonvm-gc` directly.
pub use cratonvm_gc::gen_young_sizing::{plan_young_gen, YoungGenPlan, YoungGenSizing};
/// `MarkBitmap::clear`'s `(calls, worked, words, nanos)`.
pub use cratonvm_gc::mark_bitmap::clear_census as mark_bitmap_clear_census;
/// The TLAB census lines and the "a tripwire fired" predicate, so `vm-cli` can
/// print the guard line whenever a tripwire fired, not only under GC stats
/// (gen r4w2/alloc2).
pub use cratonvm_gc::tlab::{tlab_census_lines, tlab_guard_line_is_urgent};
/// The young-mark drain's `(calls, parallel_calls, workers_last, nanos)`.
pub use cratonvm_gc::young_mark::drain_census as young_mark_drain_census;
pub use cratonvm_gc::zgc::report_punned_watch_at_exit;

// ---------------------------------------------------------------------------
// T17.E.2 — Windows test-harness teardown shim
// ---------------------------------------------------------------------------
//
// ## Root cause
//
// On Windows, `cargo test -p cratonvm-vm --lib` crashes with
// `STATUS_ACCESS_VIOLATION (0xC0000005)` *after* every test has
// reported `ok` but *before* libtest can emit the final
// `test result: ok. ... passed; ...` line. The crash fires inside
// libtest's own result-collection phase — i.e. between the last
// `test X ok` line and the summary print, on Windows MSVC only —
// when the accumulated state from ~3,300 tests (every
// `OnceLock<Arc<RwLock<...>>>`, every `thread_local!` holding
// `Option<Arc<SharedVm>>`, and every `LazyLock<RwLock<HashMap>>`
// on the JNI side) starts tearing down in a partially-racing order
// under MSVC's `_execute_onexit_table`. The interaction is not
// practical to repair at the call-site level given the number of
// globals.
//
// ## Fix strategy
//
// The shim bypasses the CRT exit sequence via two Win32 hooks:
//
// 1. **`atexit(on_exit)`** — fires once libtest's `main` returns
//    normally (no teardown crash). Calls `ExitProcess(0)` so the
//    subsequent CRT static-destructor pass cannot run.
//
// 2. **`SetUnhandledExceptionFilter(unhandled_filter)`** — catches
//    the `0xC0000005` fired during libtest's result-collection
//    phase. `main` never returned, so `atexit` cannot run; the
//    SEH filter is the one remaining trap-door. Calls
//    `ExitProcess(101)` — it used to report success here, which is
//    the change described under "The panic tripwire" below.
//
// Both hooks are installed from a `.CRT$XCU` section entry so they
// are live before libtest's `main` begins running tests. The ctor
// also records `CTOR_RAN = true` so the regression test can verify
// the wiring without triggering the failure path.
//
// ## Why the two hooks report what they do
//
// libtest on Windows uses `std::process::exit` to propagate test
// failures (`ERROR_EXIT_CODE = 101`). Rust's `std::process::exit`
// compiles to `ExitProcess`, which **bypasses both `atexit`
// callbacks and unhandled-exception filters**. That means a real
// test failure reaches cargo as exit code 101 *before* either
// of our hooks can fire. The hooks are therefore only reached on:
//
//   (a) the clean-success path where libtest's `main` returned 0
//       (every test passed) — `atexit`, exit **0**; and
//   (b) the teardown-crash path where libtest's post-test cleanup
//       faulted — the SEH filter, exit **101**.
//
// (a) is a certainty rather than an inference: **we cannot silently
// mask a genuine test failure, because the failure path never hits
// our hooks** — it terminates earlier through libtest's own
// `process::exit(101)`. Measured, not merely read out of libtest: on
// 2026-08-05 a run with one genuinely failing test exited 101 with
// no shim message at all, proving `atexit` never ran, while a run
// with every test passing printed the shim's own line and exited
// through `on_exit`.
//
// (b) used to report success too, on the theory that a crash arriving
// after every test said `ok` still means every test passed. It does
// not: the fault can equally arrive before the summary is written, or
// on a worker with tests still queued behind it. A crash is not a
// pass. Verified by injecting a read of address `0x10` on the
// `atexit` path — the filter's `main` arm fires and the run exits
// 101, where the previous code exited 0.
//
// ## The panic tripwire that used to live here (deleted 2026-08-05)
//
// An `exit_code()` helper answered "was a failure in flight when
// teardown hit us" by comparing a process-wide panic count against a
// compile-time budget, and both exit paths consulted it. It was
// removed, in two steps, and the reasoning is worth keeping because
// the shape recurs.
//
// On the `atexit` path it was a pure liability. `atexit` runs only
// when `main` RETURNED, which already means every test passed, so a
// heuristic there could only ever overturn a certainty with a guess.
// It did: the budget was 32 while a clean run panicked 33 times, so
// `cargo test -p cratonvm-vm --lib` printed
// `test result: ok. 2410 passed; 0 failed` and then exited 1, on
// every run, silently.
//
// The budget was unmaintainable in principle, not merely stale. Its
// drift guard counted only the `#[should_panic]` half; the ambient
// half had grown 4 -> 9 unwatched, while 3 of the 28 `#[should_panic]`
// attributes never fire at all. Two errors of opposite sign that
// happened to sum across the line. And a single failing test
// contributes ONE panic to a count that drifts by several — so even
// maintained, it could not separate "a failure was pending" from "the
// ambient set moved again".
//
// On the SEH path it is now gone too, because that path does not
// happen. Measured over 20 consecutive runs on dev: the filter fired
// **0 times**; all 15 green runs exited through `atexit` (the other 5
// were ordinary test failures, exit 101 via libtest). The
// `0xC0000005` this shim was built for does not reproduce, and no
// history of it survives — the shim arrived in the squashed
// open-source import.
//
// So the filter no longer guesses an exit code. **Any unhandled SEH
// exception is now reported and exits 101**, on whichever thread it
// fires. If the teardown crash ever returns it will be a loud red
// naming itself, which is the correct outcome for a crash and is what
// the old code's "report success if fewer than N panics happened"
// could not deliver.
//
// ## Regression coverage
//
// `harness_exit_shim_tests::shim_installed_at_startup` asserts the
// `.CRT$XCU` constructor fired. A deliberately-failing test gated
// on `--cfg test_harness_exit_code` asserts that a genuine failure
// still reaches cargo as 101 rather than being rewritten by a hook.
//
// No-op outside `#[cfg(all(test, windows))]`: Linux / macOS
// `cargo test` teardowns are clean, and release binaries never
// touch the test harness in the first place.

#[cfg(all(test, windows))]
#[doc(hidden)]
pub mod harness_exit_shim;

// Non-Windows test builds: the shim module exists only as an empty
// placeholder so call-sites that might reference `install()`
// (e.g. from an integration test) compile cleanly on every platform.
#[cfg(all(test, not(windows)))]
#[doc(hidden)]
pub mod harness_exit_shim {
    /// No-op outside Windows; Unix cargo-test teardown does not hit
    /// the MSVC-specific static-destructor bug.
    pub fn install() {}
    /// No-op mirror of the Windows helper; always returns `true` so
    /// cross-platform regression tests can assert symmetry.
    pub fn ctor_ran() -> bool {
        true
    }
}

/// Final tally for G1's live-region memo (`CRATONVM_DBG_G1_LIVE_MEMO`).
///
/// Re-exported here because `cratonvm-cli` depends on this crate and not on
/// `cratonvm-gc`, and because the counter belongs beside the site-cache dump in
/// the CLI's shutdown reporting: both exist to say whether a lever fired before
/// anyone quotes a timing number for it.
pub fn dump_g1_live_region_memo_stats() {
    cratonvm_gc::g1::live_region_memo::stats::dump();
}

/// zt t19/x15: serializes the vm unit tests that flip, or read and rely on,
/// the PROCESS-WIDE ZGC read-barrier gate (`cratonvm_types::zgc_read_barrier_armed`,
/// written by `ZgcRealHeap::set_barrier_color`). zt t18/zb1's arming tests
/// (`arm_load_barrier_plain_stores` on a test VM's heap) were the first vm
/// tests to flip it, and a test that checked the gate at its start (the nio21
/// hardware-CAS tests) could see it armed half-way and take the lock path.
/// A test that assumes the gate unarmed WITHOUT reading it is not covered.
#[cfg(test)]
pub(crate) static ZGC_READ_GATE_TEST_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

#[cfg(test)]
#[allow(unexpected_cfgs)]
mod harness_exit_shim_tests;
