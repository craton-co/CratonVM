// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

#![deny(deprecated)]

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
//! application, see [`docs/EMBEDDING.md`](https://github.com/craton-co/cratonvm/blob/main/docs/EMBEDDING.md)
//! in the workspace root. The first-party `vm-cli` binary is the
//! reference embedder.

pub mod classloading;
pub mod config;
#[cfg(feature = "experimental-debug")]
pub mod debug;
pub mod dispatch_trace;
/// G1 parallel-evacuation CAS losses — see
/// [`cratonvm_gc::g1::evacuate_cas_loser_forwards`]. Re-exported so `vm-cli`
/// can print it beside the other exit counters without taking a direct
/// dependency on the gc crate for one number.
pub fn g1_evacuate_cas_loser_forwards() -> u64 {
    cratonvm_gc::g1::evacuate_cas_loser_forwards()
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
/// `(captured, adopted, written, skipped, enabled)`.
///
/// Re-exported because `vm-cli` does not link `cratonvm-gc` directly, the same
/// reason `evacuate_cas_loser_forwards` above is. `written == 0` means the
/// repair never engaged on the run and nothing may be concluded from its
/// result -- see `gc_quiescence::PEER_STACK_SLOTS_CAPTURED`.
pub fn blocked_peer_stack_remap_census() -> (u64, u64, u64, u64, u64, u64, u64, bool) {
    use std::sync::atomic::Ordering;
    (
        cratonvm_gc::gc_quiescence::PEER_STACK_SLOTS_CAPTURED.load(Ordering::Relaxed),
        cratonvm_gc::gc_quiescence::PEER_STACK_SLOTS_ADOPTED.load(Ordering::Relaxed),
        cratonvm_gc::gc_quiescence::PEER_STACK_SLOTS_WRITTEN.load(Ordering::Relaxed),
        cratonvm_gc::gc_quiescence::PEER_STACK_SLOTS_SKIPPED.load(Ordering::Relaxed),
        cratonvm_gc::gc_quiescence::PEER_STACK_SLOTS_DISCARDED.load(Ordering::Relaxed),
        cratonvm_gc::gc_quiescence::PEER_STACK_SLOTS_DROPPED.load(Ordering::Relaxed),
        cratonvm_gc::gc_quiescence::PEER_STACK_SLOTS_UNROUTED.load(Ordering::Relaxed),
        cratonvm_gc::gc_quiescence::blocked_peer_stack_remap_enabled(),
    )
}

pub use cratonvm_gc::zgc::report_punned_watch_at_exit;
/// The collector's own account of the last cycle and the decision histogram
/// behind it. Re-exported because `vm-cli` prints it at shutdown under
/// `--verbose:gc` / `CRATONVM_GC_STATS` and does not depend on `cratonvm-gc`
/// directly.
pub use cratonvm_gc::gc_metrics::collector_decision_report;
/// Bytes the generational young collector has returned to the OS
/// (`CRATONVM_GEN_UNCOMMIT`). Re-exported for the shutdown census in `vm-cli`,
/// which does not depend on `cratonvm-gc` directly.
pub use cratonvm_gc::gen_heap::young_bytes_uncommitted;
/// `(hits, misses)` for the exact object-start fast path in
/// `is_object_address` (`CRATONVM_GC_OBJECT_STARTS`). Re-exported for the
/// shutdown census in `vm-cli`.
pub use cratonvm_gc::gen_heap::object_start_counts;
/// The GC-trigger publish verifier's `(checks, divergences)` -- see
/// `cratonvm_gc::gen_heap::gc_trigger_verify_counts`. Re-exported for the same
/// reason its neighbours are: `vm-cli` depends on `cratonvm-vm`, not on
/// `cratonvm-gc`.
pub use cratonvm_gc::gen_heap::gc_trigger_verify_counts;
/// `MarkBitmap::clear`'s `(calls, worked, words, nanos)`.
pub use cratonvm_gc::mark_bitmap::clear_census as mark_bitmap_clear_census;
/// G1 `is_object_address`'s `(calls, accepted, nanos)`.
pub use cratonvm_gc::g1_object_address_census;
/// The young-mark drain's `(calls, parallel_calls, workers_last, nanos)`.
pub use cratonvm_gc::young_mark::drain_census as young_mark_drain_census;

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
pub mod harness_exit_shim {
    use std::sync::atomic::{AtomicBool, Ordering};

    // Windows ExitProcess(u32) — unconditional process termination.
    // Calling this bypasses any further CRT teardown that would
    // otherwise crash 0xC0000005.
    #[link(name = "kernel32")]
    extern "system" {
        fn ExitProcess(uExitCode: u32) -> !;
        fn SetUnhandledExceptionFilter(
            filter: Option<unsafe extern "system" fn(*mut ()) -> i32>,
        ) -> Option<unsafe extern "system" fn(*mut ()) -> i32>;
    }

    // MSVC CRT atexit registration. Returns 0 on success. We don't
    // care about the return value — if registration fails the
    // original crash is what we would have hit anyway.
    extern "C" {
        fn atexit(cb: extern "C" fn()) -> i32;
    }

    /// One line to stderr, bypassing `eprintln!`.
    ///
    /// Every path that calls this is a path that terminates the process with
    /// `ExitProcess`, so the message has to reach the real handle: libtest
    /// redirects the print macros per test thread, and anything written
    /// through them dies with the process instead of reaching cargo. A silent
    /// `ExitProcess(1)` is what made this shim's own tripwire look like an
    /// unexplained teardown crash for a whole afternoon.
    fn report(msg: &str) {
        use std::io::Write;
        let mut err = std::io::stderr();
        let _ = err.write_all(b"\n[harness-exit-shim] ");
        let _ = err.write_all(msg.as_bytes());
        let _ = err.write_all(b"\n");
        let _ = err.flush();
    }

    extern "C" fn on_exit() {
        // atexit path: runs when `main` returns cleanly. Bypasses the
        // subsequent CRT static-destructor pass.
        //
        // Unconditionally 0. Reaching here means libtest's `main`
        // returned; libtest terminates a failing run through
        // `process::exit(ERROR_EXIT_CODE)`, which on Windows is
        // `ExitProcess` and does not run `atexit` handlers. So arriving
        // here is already proof that every test passed. This used to
        // consult a panic-count heuristic instead, which could only
        // overturn that proof with a guess — and did, for a permanent
        // silent exit-1 behind a `test result: ok` line.
        unsafe { ExitProcess(0) }
    }

    /// Windows unhandled-exception filter. Fires when the harness
    /// trips an SEH exception such as `STATUS_ACCESS_VIOLATION`.
    ///
    /// **Always reports failure (101).** An unhandled hardware fault
    /// means some part of this run did not happen: either a test died
    /// mid-run and everything queued behind it never executed, or the
    /// harness faulted during teardown. Neither is a passing run, and
    /// neither can be distinguished from a real failure by anything
    /// observable from inside this filter.
    ///
    /// It used to guess instead, routing a fault on `main` through a
    /// panic-count heuristic that could answer **success**. That is
    /// not hypothetical: before the `widened_obj_key` VM-scoping fix,
    /// `cargo test -p cratonvm-vm --lib --features synthetic-jdk
    /// vm::tests::linked_hashmap_` crashed in `lhm_link_tail` and
    /// exited **0** — a green run with the remaining tests unexecuted.
    /// The heuristic is gone (see the module header); the thread name
    /// now only selects which explanation to print.
    ///
    /// This is a deliberate policy change from the shim's original
    /// intent, which was to convert a teardown `0xC0000005` into a
    /// green run. That crash no longer reproduces — 20 consecutive
    /// runs, filter never fired — so what remains is a trap-door that
    /// only ever fires on something genuinely wrong. If it returns, it
    /// should be fixed, not painted green.
    unsafe extern "system" fn unhandled_filter(_info: *mut ()) -> i32 {
        // The filter runs on the crashing thread. `ExitProcess` is the
        // one Win32 call that's always safe here: it never unwinds, it
        // ignores corrupted CRT state, and it returns the exit code we
        // want cargo to see.
        //
        // The name comes from the OS, NOT from `std::thread::current()`.
        //
        // This filter runs on a thread that has just faulted, and
        // `std::thread::current()` PANICS -- it does not return `None` --
        // once that thread's thread-local data has been destroyed. This
        // function is `extern "system"`, so an unwind out of it is undefined
        // behaviour rather than a diagnosable failure, and the panic would
        // arrive while the process is already reporting a crash.
        //
        // Same species as the panic-hook site that aborted 183 Hibernate
        // Reactive classes on rc=134 while trying to *report* their first
        // panic; see `crash_handler::current_thread_name`, whose whole reason
        // for existing is that a handler must not call an API that panics.
        //
        // libtest runs each concurrent test on its own thread named
        // after the test, and does result collection on `main` — so the
        // name says which of the two stories to tell. Under
        // `--test-threads=1` tests run inline on `main` and the two are
        // indistinguishable; the exit code is 101 either way, so the
        // worst case is a slightly wrong explanation, not a wrong verdict.
        let thread_name = crate::runtime::crash_handler::current_thread_name();
        let on_worker = matches!(thread_name.as_deref(), Some(name) if name != "main");
        // The hardware-fault handler installed alongside this filter has
        // already printed the faulting PC, the thread name and a
        // symbolized backtrace.
        if on_worker {
            report(
                "a test crashed mid-run; tests after it never executed. \
                 Reporting failure (101) - see the fatal-error report above \
                 for the faulting test and PC.",
            );
        } else {
            report(
                "the harness faulted on `main`, after tests ran but before it \
                 could finish reporting. Reporting failure (101) - the run is \
                 not trustworthy, see the fatal-error report above.",
            );
        }
        // libtest's own ERROR_EXIT_CODE, so cargo reports this exactly as
        // it would an ordinary test failure.
        ExitProcess(101)
    }

    /// Records whether the one-shot `install` path has already run.
    /// Used by the regression test to assert the shim is wired in.
    pub static CTOR_RAN: AtomicBool = AtomicBool::new(false);

    /// Install the atexit handler + unhandled-exception filter. Safe to
    /// call more than once — `std::sync::Once` guards against
    /// double-install.
    ///
    /// No longer wraps the panic hook. The wrapper existed solely to
    /// feed a panic-count tripwire, which is gone; leaving it would keep
    /// a `set_hook` in the path of every panicking test for a counter
    /// nobody reads.
    pub fn install() {
        use std::sync::Once;
        static INIT: Once = Once::new();
        INIT.call_once(|| {
            unsafe {
                atexit(on_exit);
                // Install the unhandled-exception filter. This is the
                // one that actually catches the 0xC0000005 fired on
                // test-harness teardown — the atexit path alone cannot
                // fire because libtest's `main` never returns.
                SetUnhandledExceptionFilter(Some(unhandled_filter));
            }
            // The vectored handler runs *before* the unhandled filter and
            // prints the faulting PC plus a symbolized native backtrace,
            // which is the difference between "a test crashed" and knowing
            // where. `vm-cli` and `libcratonvm` both install it; the test
            // harness did not, which is why hardware faults under
            // `cargo test` produced no diagnostic at all. It is a pure
            // diagnostic tap (returns EXCEPTION_CONTINUE_SEARCH), so it
            // does not change which handler ultimately terminates us.
            crate::runtime::crash_handler::install_hardware_fault_handler();
            CTOR_RAN.store(true, Ordering::SeqCst);
        });
    }

    // CRT init hook: MSVC walks the `.CRT$XCU` section at process
    // startup and calls every function pointer there before `main`.
    // This is the same mechanism the `ctor` crate uses and it
    // guarantees our atexit registration and exception filter are in
    // place before libtest's `main` begins running tests.
    #[used]
    #[allow(non_upper_case_globals)]
    #[link_section = ".CRT$XCU"]
    static INIT_CTOR: extern "C" fn() = {
        extern "C" fn init() {
            install();
        }
        init
    };

    /// Testing hook: returns `true` iff the `.CRT$XCU` init ran during
    /// process startup (i.e. the `Once` guarding `install` has already
    /// fired). Used by the regression test to prove the shim is wired
    /// in without relying on test teardown behaviour.
    pub fn ctor_ran() -> bool {
        CTOR_RAN.load(Ordering::SeqCst)
    }
}

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

#[cfg(test)]
#[allow(unexpected_cfgs)]
mod harness_exit_shim_tests {
    //! T17.E.2 regression guard — asserts the Windows shim actually
    //! installed its panic hook + atexit during process startup.
    //! Does NOT exercise the failure path (that would kill the
    //! harness); instead verifies the CTOR_RAN sentinel so a future
    //! refactor that breaks `.CRT$XCU` placement or conditional
    //! compilation is caught immediately.

    #[test]
    fn shim_installed_at_startup() {
        assert!(
            super::harness_exit_shim::ctor_ran(),
            "CRT init hook did not run — \
             atexit shim is not in place; Windows teardown crash will resurface",
        );
    }

    // Deliberately-failing test that only runs when the extra
    // `--cfg test_harness_exit_code` flag is passed:
    //
    //   cargo test -p cratonvm-vm --lib --config \
    //     'build.rustflags=["--cfg","test_harness_exit_code"]'
    //
    // The invariant it proves is the one the whole shim rests on: a
    // genuine test failure must reach cargo as a failure, NOT be
    // rewritten by our hooks. **Expect exit code 101** — libtest's own
    // `ERROR_EXIT_CODE`, reached through `process::exit` before either
    // hook can fire.
    //
    // It used to assert exit 1, on the theory that a failure arrives
    // via `atexit` with the panic counter over budget. It does not:
    // `atexit` never runs on the failing path, which is exactly why
    // `on_exit` can return 0 unconditionally. Asserting 1 here would
    // have made this guard pass only while the bug it was meant to
    // catch was present.
    //
    // The outer `#[allow(unexpected_cfgs)]` on the module silences
    // the lint for this opt-in cfg that cargo doesn't learn about
    // from `Cargo.toml`.
    #[cfg(test_harness_exit_code)]
    #[test]
    fn harness_exit_code_regression() {
        panic!("T17.E.2 deliberate failure — the run must exit 101, not 0");
    }
}
