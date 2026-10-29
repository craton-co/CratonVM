// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Tiered compilation subsystem.
//!
//! Implements a HotSpot-style tiered compilation pipeline:
//!
//! | Level | Name              | Description                                    |
//! |-------|-------------------|------------------------------------------------|
//! |   0   | Interpreter       | Bytecode interpretation with profiling          |
//! |   1   | C1                | Quick compile, no profiling                     |
//! |   2   | C1WithProfiling   | Quick compile with profiling (transition to C2) |
//! |   3   | FullProfile       | Profile collection only (not compiled)          |
//! |   4   | C2                | Full profile-guided optimization                |
//!
//! Transition graph:
//!
//! ```text
//! Interpreter ──► C1 ──► C2
//!       │                  │
//!       └──► C2 (if has profile + enough invocations)
//!                          │
//!       ◄──────────────────┘  (on a deopt that evicts the body)
//!       │
//!       └──► C1 (per-bci trap limit or per-method trap cutoff → c2_bailout, stays C1
//!                until a redefinition; trap decay only slows the counts, and under
//!                CRATONVM_JIT_PER_BCI_TRAP_LIMIT_KEEPS_C2 only the method cutoff bails)
//! ```

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use cratonvm_types::ClassId;
use parking_lot::{Condvar, Mutex, RwLock};
use rustc_hash::FxHashMap;

use crate::deopt::{DeoptAction, DeoptReason};

// j27 lane SEEDOWE (`CRATONVM_JIT_IR_IC_SEED_OWED_RETRY`, opt-in): the seed
// retry owed past a refusal that can lift.
mod ic_seed_owed;
pub use ic_seed_owed::{ic_seed_owed_retry_enabled, MAX_OWED_IC_SEED_RETRIES};

// j27 wave 6 lane T1TIERMGR (TIERMGR21-1, `CRATONVM_JIT_ONE_OWED_RETRY`,
// default ON since 2026-10-28, j27 WARNING flip; =0 restores): one owner per
// `AlreadyQueued` C2 retry.
mod owed_retry;
pub use owed_retry::{manager_owes_already_queued, manager_owes_answer, one_owed_retry_enabled};

// j27 wave 6 lane T1TIERMGR (TIERMGR21-4 = ACCEPT21-1 step 2, the timing
// hook; diagnostic under `CRATONVM_DBG_ACCEPT_CENSUS`).
mod accept_timing;
pub use accept_timing::{
    accept_timing_line, AcceptTimingFn, AcceptTimingSample, MAX_ACCEPT_TIMED_METHODS,
    MAX_ACCEPT_TIMING_PENDING,
};

/// Consecutive background-compile attempts allowed to fail (run but not
/// publish a body) at a given tier before `should_compile` gives up on that
/// method entirely. See `CompilerCore::finish` / `should_compile`.
const MAX_TIER_FAIL_RETRIES: u32 = 3;

/// JIT r23 wave 9, lane L6 (`CRATONVM_JIT_C2_PUBLICATION_REFUSAL_REFUND`): how
/// many refused optimizing publications per method are not charged to
/// [`MAX_TIER_FAIL_RETRIES`]. See [`publication_refusal_refund`].
const MAX_PUBLICATION_REFUSAL_REFUNDS: u8 = 6;

/// `CRATONVM_JIT_C2_PUBLICATION_REFUSAL_REFUND` (JIT r23 wave 9, lane L6;
/// perf, **default ON since j27 2026-10-28**, `=0` kills; was opt-in after the
/// n23-w9c A/B: mixed, `full` lost 3 of 3): a C2 body that compiled and was then
/// refused at `put` does not spend the method's retry budget, up to
/// [`MAX_PUBLICATION_REFUSAL_REFUNDS`] times. The refusal is a race: a baked
/// direct-call target retired while the body compiled
/// (`baked-callee-retired`), or a dependency moved. It is not broken
/// codegen.
///
/// Measured shape: `Pattern.compile()V` (a 110 KB optimizing body) on
/// `W8L6SplitParts compile`. Its callees republish while it compiles, so all
/// three charged attempts were dropped as `baked-callee-retired`. The retry
/// then answered `tier_fail_count exhausted`, and the method kept its
/// single-pass body for the rest of the process. That body calls the
/// `Character.charCount` native through the native door, about half of the
/// row in a RIP sample: 1569-1798 ns/op, against 971-1165 in runs where C2
/// published. Read per call (no static), so a test's thread override
/// reaches it.
pub fn c2_publication_refusal_refund_enabled() -> bool {
    // WARNING j27-user-flip-20261028 (default ON; =0 restores the old default)
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_C2_PUBLICATION_REFUSAL_REFUND")
}

/// Pure rule of the refund: given the charged `tier_fail_count` and the
/// refunds already spent, the new pair, or `None` when nothing is handed back.
/// Nothing is handed back when the switch is off, when the budget of refunds
/// is spent, or when nothing was charged.
fn publication_refusal_refund(
    tier_fail_count: u32,
    refunds: u8,
    enabled: bool,
) -> Option<(u32, u8)> {
    if !enabled || refunds >= MAX_PUBLICATION_REFUSAL_REFUNDS || tier_fail_count == 0 {
        return None;
    }
    Some((tier_fail_count - 1, refunds + 1))
}

#[cfg(test)]
mod r23w9_l6_publication_refusal_refund_tests {
    use super::*;

    #[test]
    fn a_refused_publication_hands_its_charge_back_until_the_refunds_run_out() {
        assert_eq!(publication_refusal_refund(1, 0, true), Some((0, 1)));
        assert_eq!(publication_refusal_refund(3, 5, true), Some((2, 6)));
        assert_eq!(
            publication_refusal_refund(3, MAX_PUBLICATION_REFUSAL_REFUNDS, true),
            None,
            "bounded: the old budget applies after the refunds"
        );
    }

    #[test]
    fn nothing_is_refunded_when_off_or_uncharged() {
        assert_eq!(publication_refusal_refund(2, 0, false), None);
        assert_eq!(publication_refusal_refund(0, 0, true), None);
    }

    /// Total attempts stay bounded: the charged budget plus the refunds.
    #[test]
    fn the_refunds_bound_the_attempts() {
        let (mut fails, mut refunds, mut attempts) = (0u32, 0u8, 0u32);
        while fails < MAX_TIER_FAIL_RETRIES {
            attempts += 1;
            fails += 1;
            if let Some((f, r)) = publication_refusal_refund(fails, refunds, true) {
                fails = f;
                refunds = r;
            }
            assert!(attempts < 100);
        }
        assert_eq!(
            attempts,
            MAX_TIER_FAIL_RETRIES + u32::from(MAX_PUBLICATION_REFUSAL_REFUNDS)
        );
    }

    /// Default ON since j27 2026-10-28 (`WARNING j27-user-flip-20261028`);
    /// `=0` restores the old opt-in default.
    #[test]
    fn the_switch_is_default_on_with_a_zero_kill() {
        const KEY: &str = "CRATONVM_JIT_C2_PUBLICATION_REFUSAL_REFUND";
        cratonvm_types::flags::with_thread_overrides(&[(KEY, None)], || {
            assert!(c2_publication_refusal_refund_enabled())
        });
        cratonvm_types::flags::with_thread_overrides(&[(KEY, Some("0"))], || {
            assert!(!c2_publication_refusal_refund_enabled())
        });
        cratonvm_types::flags::with_thread_overrides(&[(KEY, Some("1"))], || {
            assert!(c2_publication_refusal_refund_enabled())
        });
    }
}

/// Process-start timestamp, seeded from [`TieredCompilationManager::new`]
/// (constructed very early during VM init, well before any method can reach
/// a compile threshold). Backs the `elapsed_ms` field of the
/// `CRATONVM_DBG_TIER_ENQUEUE` diagnostic below -- see
/// `hib-misc-residuals-20260716-FIXED.md` for why
/// "how far into the process's life did this compile trigger" was the key
/// diagnostic needed to confirm the compile-time-tax mechanism.
fn process_start() -> &'static std::time::Instant {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START.get_or_init(std::time::Instant::now)
}

/// Milliseconds elapsed since [`process_start`] was first seeded.
///
/// `pub(crate)` because [`crate::deopt::DeoptimizationLog`] decays its trap
/// ledger on the same clock this module's [`decay_traps`] uses. Two trap
/// ledgers with two different clocks is how they drifted apart in the first
/// place; see [`deopt_reason_is_charged`].
pub(crate) fn process_uptime_ms() -> u64 {
    process_start().elapsed().as_millis() as u64
}

// ───────────────────────────────────────────────────────────────────────────────
// Compile-panic containment
// ───────────────────────────────────────────────────────────────────────────────

thread_local! {
    /// Depth of [`contain_compile_panic`] scopes active on this thread.
    ///
    /// Read by the VM's crash handler from inside the panic hook, which runs
    /// BEFORE `catch_unwind` receives the unwind. A panic raised while this is
    /// non-zero is about to be caught and turned into a declined compile, so
    /// the handler must neither write `hs_err_pid<pid>.log` nor latch its
    /// one-report-per-process guard: either would leave the next REAL crash
    /// with no report at all.
    static COMPILE_PANIC_SCOPES: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Whether the current thread is inside a [`contain_compile_panic`] scope.
///
/// `try_with`, never `with`: the crash handler calls this from a panic hook,
/// and a second panic raised there (this thread's TLS already destroyed)
/// aborts the process before anything is reported.
pub fn compile_panic_is_contained() -> bool {
    COMPILE_PANIC_SCOPES
        .try_with(|depth| depth.get() > 0)
        .unwrap_or(false)
}

/// One [`contain_compile_panic`] scope. The depth drops when the guard drops,
/// which during a panic is AFTER the hook has already asked.
///
/// JIT round 23 wave 3 (proxy-define page; `CRATONVM_JIT_PANIC_SCOPE_ONE_TLS`,
/// default ON, `=0` kill): the scope keeps the address of this thread's depth
/// cell from its one TLS lookup at `enter` and decrements through it at drop,
/// instead of a second lookup. Every guarded JIT helper (`jit_getfield`,
/// `jit_vectorized_hash_code_direct`, ...) opens one of these per call, and the
/// TLS access was 1.3-2.9 % of the proxy row's main thread (all of it on the
/// `try_with` line). The cell is a `Cell<u32>` without a destructor, so its
/// storage outlives every scope on the thread; the raw pointer keeps the scope
/// `!Send`, as it must be. The depth the crash hook reads is the same.
struct CompilePanicScope(Option<*const std::cell::Cell<u32>>);

/// `CRATONVM_JIT_PANIC_SCOPE_ONE_TLS` (see [`CompilePanicScope`]), memoised
/// (1 off, 2 on; re-derived when a test installs an override).
#[inline]
fn panic_scope_one_tls() -> bool {
    static SLOT: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    match SLOT.load() {
        1 => false,
        2 => true,
        _ => {
            let on = cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_JIT_PANIC_SCOPE_ONE_TLS",
            );
            SLOT.publish(if on { 2 } else { 1 });
            on
        }
    }
}

impl CompilePanicScope {
    #[inline]
    fn enter() -> Self {
        if panic_scope_one_tls() {
            let cell = COMPILE_PANIC_SCOPES
                .try_with(|depth| {
                    depth.set(depth.get().saturating_add(1));
                    depth as *const std::cell::Cell<u32>
                })
                .ok();
            return CompilePanicScope(cell);
        }
        let _ = COMPILE_PANIC_SCOPES.try_with(|depth| depth.set(depth.get().saturating_add(1)));
        CompilePanicScope(None)
    }
}

impl Drop for CompilePanicScope {
    #[inline]
    fn drop(&mut self) {
        if let Some(cell) = self.0 {
            // SAFETY: `cell` is this thread's `COMPILE_PANIC_SCOPES`, taken at
            // `enter` on this same thread (the scope is `!Send`); a
            // destructor-less `const` thread-local lives as long as the thread,
            // which outlives every scope on its stack.
            let depth = unsafe { &*cell };
            depth.set(depth.get().saturating_sub(1));
            return;
        }
        let _ = COMPILE_PANIC_SCOPES.try_with(|depth| depth.set(depth.get().saturating_sub(1)));
    }
}

/// Run one compile with its panics contained.
///
/// The workspace builds with `panic = "unwind"`, and before this existed a
/// panic anywhere in codegen unwound out of the only compile thread in the
/// process: the worker died with `active` still set, `inflight_epoch` still
/// non-zero and the method still marked queued, nothing restarted it, and
/// tiering stopped for the rest of the run with no diagnostic beyond the panic
/// message itself.
///
/// A compile is the one piece of VM work whose failure has an obvious safe
/// answer -- do not compile that method again -- which is what the callers do
/// with an `Err`. `AssertUnwindSafe` rests on two facts: nothing a compile
/// builds is visible to other threads until `JitCache::put` publishes it, and
/// the `parking_lot` locks it takes do not poison, so an unwind leaves no lock
/// held and no half-published body behind.
pub fn contain_compile_panic<R>(f: impl FnOnce() -> R) -> std::thread::Result<R> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _scope = CompilePanicScope::enter();
        f()
    }))
}

/// The human-readable part of a panic payload.
pub fn panic_payload_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "<non-string panic payload>".to_string())
}

// ───────────────────────────────────────────────────────────────────────────────
// Compile cost measurement and worker sizing
// ───────────────────────────────────────────────────────────────────────────────

/// CPU time the calling thread has consumed so far, or `None` where no
/// per-thread clock is wired up (the caller then falls back to wall time).
///
/// The C2 compile budget is charged in THIS unit. A compile thread that is
/// descheduled -- an oversubscribed CI host, a stop-the-world pause, a laptop
/// throttling -- accrues wall time while doing no work, and charging that to
/// the method demoted healthy methods to C1 for the rest of the process.
#[allow(unreachable_code)]
pub fn current_thread_cpu_time() -> Option<std::time::Duration> {
    #[cfg(windows)]
    {
        #[repr(C)]
        #[derive(Clone, Copy, Default)]
        struct FileTime {
            low: u32,
            high: u32,
        }
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetCurrentThread() -> *mut std::ffi::c_void;
            fn GetThreadTimes(
                thread: *mut std::ffi::c_void,
                creation: *mut FileTime,
                exit: *mut FileTime,
                kernel: *mut FileTime,
                user: *mut FileTime,
            ) -> i32;
        }
        let mut creation = FileTime::default();
        let mut exit = FileTime::default();
        let mut kernel = FileTime::default();
        let mut user = FileTime::default();
        // SAFETY: `GetCurrentThread` returns a pseudo-handle that is always
        // valid for the calling thread and needs no close, and every out
        // pointer is a live, writable `FILETIME` in this frame.
        let ok = unsafe {
            GetThreadTimes(
                GetCurrentThread(),
                &mut creation,
                &mut exit,
                &mut kernel,
                &mut user,
            )
        };
        if ok == 0 {
            return None;
        }
        // A FILETIME counts 100-nanosecond intervals in two 32-bit halves.
        let ticks = |t: FileTime| (u64::from(t.high) << 32) | u64::from(t.low);
        return Some(std::time::Duration::from_nanos(
            ticks(kernel)
                .saturating_add(ticks(user))
                .saturating_mul(100),
        ));
    }
    #[cfg(all(target_os = "linux", target_pointer_width = "64"))]
    {
        #[repr(C)]
        struct Timespec {
            tv_sec: i64,
            tv_nsec: i64,
        }
        unsafe extern "C" {
            fn clock_gettime(clock_id: i32, tp: *mut Timespec) -> i32;
        }
        // `CLOCK_THREAD_CPUTIME_ID` from `<time.h>`, the same on every Linux
        // architecture.
        const CLOCK_THREAD_CPUTIME_ID: i32 = 3;
        let mut ts = Timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: `ts` is a live, writable `struct timespec` (two `long`s on
        // an LP64 target) and the clock id is a kernel-defined constant.
        let rc = unsafe { clock_gettime(CLOCK_THREAD_CPUTIME_ID, &mut ts) };
        if rc != 0 {
            return None;
        }
        // Cast: a thread CPU clock is non-negative, and `tv_nsec < 1e9`.
        return Some(std::time::Duration::new(
            ts.tv_sec as u64,
            ts.tv_nsec as u32,
        ));
    }
    None
}

/// How many compile workers each lane gets, as `(c1, c2)`.
///
/// C1 defaults to one worker and C2 to `max(1, floor(log2(cpus)))`, the shape
/// of HotSpot's `CICompilerCount` split: the optimizing tier gets the larger
/// share because its compiles are the long ones. `CRATONVM_TIER_C1_THREADS`
/// and `CRATONVM_TIER_C2_THREADS` override either, clamped to `1..=64` -- a
/// lane with no worker would leave every request routed to it queued forever.
pub fn compiler_thread_counts() -> (usize, usize) {
    compiler_thread_counts_with(
        |name| cratonvm_types::flags::runtime_var(name).ok(),
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1),
    )
}

/// Slots in `CompilerCore::inflight_epochs` — one per compile that can be
/// running at once.
///
/// [`compiler_thread_counts_with`] clamps each of `CRATONVM_TIER_C1_THREADS`
/// and `CRATONVM_TIER_C2_THREADS` to `1..=64`, so 128 covers every
/// configuration the knobs admit. `dispatch_slot_count_covers_the_worker_clamp`
/// pins the two together: raise the clamp without raising this and a dispatch
/// silently stops recording its epoch.
pub const INFLIGHT_EPOCH_SLOTS: usize = 128;

/// The upper bound each of the two thread knobs is clamped to.
///
/// Named so [`INFLIGHT_EPOCH_SLOTS`] can be derived from it in a test rather
/// than both numbers being maintained by hand.
pub const MAX_COMPILER_THREADS_PER_LANE: usize = 64;

/// A claimed slot in `CompilerCore::inflight_epochs`, handed back to
/// `note_dispatch_done`.
///
/// A newtype rather than a bare `usize` so the pairing cannot be got wrong by
/// passing a lane index, a task id, or a count — the dispatch loop has all
/// three in scope at the call site.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InflightSlot(usize);

impl InflightSlot {
    /// No slot was available. `note_dispatch_done` accepts it and clears
    /// nothing, so the count still balances.
    pub const NONE: InflightSlot = InflightSlot(usize::MAX);
}

/// Testable core of [`compiler_thread_counts`].
pub fn compiler_thread_counts_with(
    get: impl Fn(&str) -> Option<String>,
    cpus: usize,
) -> (usize, usize) {
    let parse = |name: &str| -> Option<usize> {
        get(name)?
            .trim()
            .parse::<usize>()
            .ok()
            .map(|n| n.clamp(1, MAX_COMPILER_THREADS_PER_LANE))
    };
    let cpus = cpus.max(1);
    // floor(log2(cpus)); `cpus >= 1`, so the subtraction cannot underflow.
    let log2 = (usize::BITS - 1 - cpus.leading_zeros()) as usize;
    let c1 = parse("CRATONVM_TIER_C1_THREADS").unwrap_or(1);
    let c2 = parse("CRATONVM_TIER_C2_THREADS").unwrap_or(log2.max(1));
    (c1, c2)
}

// ───────────────────────────────────────────────────────────────────────────────
// CompilationTier
// ───────────────────────────────────────────────────────────────────────────────

/// The compilation level a method is currently executing at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CompilationTier {
    /// Level 0: Interpreter with profiling.
    Interpreter,
    /// Level 1: Quick compile, no profiling (client-compiler style).
    C1,
    /// Level 2: Quick compile with profiling (for transition to C2).
    C1WithProfiling,
    /// Level 3: Full profile-guided optimization (profile collection only).
    FullProfile,
    /// Level 4: Full optimization (server-compiler style, current JIT).
    C2,
}

// ───────────────────────────────────────────────────────────────────────────────
// CompilationPolicy
// ───────────────────────────────────────────────────────────────────────────────

/// Policy that decides when and how to compile methods.
pub struct CompilationPolicy {
    /// Invocation count threshold for C1 compilation.
    pub c1_threshold: u32,
    /// Invocation count threshold for C2 compilation.
    pub c2_threshold: u32,
    /// Back-edge count threshold for OSR compilation.
    ///
    /// Parsed (`CRATONVM_TIER_OSR_THRESHOLD`) but no longer consulted by the
    /// manager: its only reader was the counting `on_backedge` door, which had
    /// no production caller and was deleted. Back-edge OSR is throttled per
    /// frame by `Frame::should_try_osr` (`CRATONVM_TIER_OSR_BACKEDGE`), which
    /// calls [`TieredCompilationManager::request_osr`] directly.
    pub osr_threshold: u32,
    /// Whether tiered compilation is enabled.
    pub tiered_enabled: bool,
    /// Minimum number of invocations before considering C2.
    pub c2_min_invocations: u32,
    /// Whether to use profiling in the C1 tier.
    pub c1_profiling: bool,
}

impl Default for CompilationPolicy {
    fn default() -> Self {
        Self {
            // The invocation dispatch gate starts consulting this policy at
            // `CRATONVM_JIT_THRESHOLD` (500 by default).  Keep the first
            // background-tier admission aligned with that gate: delaying C1
            // until 1500 left reflection-heavy, short-lived bootstraps fully
            // interpreted for an additional thousand hot calls.  Those calls
            // dominate Hibernate/JAXB model construction, while the worker is
            // otherwise idle.  C2 remains deliberately conservative so a
            // one-shot process still avoids expensive optimizing recompiles.
            c1_threshold: 500,
            c2_threshold: 20_000,
            osr_threshold: 10_000,
            tiered_enabled: true,
            c2_min_invocations: 1_000,
            c1_profiling: true,
        }
    }
}

impl CompilationPolicy {
    /// wire-tiered-manager Step 6: a [`Default`] policy with `CRATONVM_TIER_*`
    /// environment overrides applied. HotSpot's defaults are the reference, but
    /// CratonVM's compile cost differs, so these knobs let the tiered pipeline
    /// be re-tuned on the app gauntlet without a recompile. Read once at VM init
    /// (cold path), so it consults the process environment directly.
    ///
    /// | env var                            | field                | default |
    /// |------------------------------------|----------------------|---------|
    /// | `CRATONVM_TIER_C1_THRESHOLD`       | `c1_threshold`       | 500     |
    /// | `CRATONVM_TIER_C2_THRESHOLD`       | `c2_threshold`       | 20000   |
    /// | `CRATONVM_TIER_OSR_THRESHOLD`      | `osr_threshold`      | 10000   |
    /// | `CRATONVM_TIER_C2_MIN_INVOCATIONS` | `c2_min_invocations` | 1000    |
    /// | `CRATONVM_TIER_ENABLED=0`          | `tiered_enabled`     | true    |
    ///
    /// (The per-frame back-edge OSR trigger — `Frame::should_try_osr` — is a
    /// separate live knob, `CRATONVM_TIER_OSR_BACKEDGE`, read VM-side because it
    /// is consulted on the default path too, not only under the tiered manager.)
    pub fn from_env() -> Self {
        Self::with_overrides(|name| cratonvm_types::flags::runtime_var(name).ok())
    }

    /// Testable core of [`from_env`]: apply the `CRATONVM_TIER_*` overrides
    /// resolved through `get` (production passes `cratonvm_types::flags::runtime_var`). Each numeric
    /// knob is parsed as `u32` and clamped to `>= 1` — a `0` threshold would
    /// compile/OSR on the first observation, defeating warmup. An absent or
    /// unparseable value leaves the [`Default`].
    pub fn with_overrides(get: impl Fn(&str) -> Option<String>) -> Self {
        let num =
            |name: &str| -> Option<u32> { get(name)?.trim().parse::<u32>().ok().map(|v| v.max(1)) };
        let mut p = Self::default();
        if let Some(v) = num("CRATONVM_TIER_C1_THRESHOLD") {
            p.c1_threshold = v;
        }
        if let Some(v) = num("CRATONVM_TIER_C2_THRESHOLD") {
            p.c2_threshold = v;
        }
        if let Some(v) = num("CRATONVM_TIER_OSR_THRESHOLD") {
            p.osr_threshold = v;
        }
        if let Some(v) = num("CRATONVM_TIER_C2_MIN_INVOCATIONS") {
            p.c2_min_invocations = v;
        }
        if let Some(s) = get("CRATONVM_TIER_ENABLED") {
            // `value_is_off` rather than a local `!= "0" && != "false"`: this
            // reads through an injected `get` (so the global
            // `runtime_flag_default_on` is not available here), but the TRUTH
            // TABLE must still be the crate's one, or `CRATONVM_TIER_ENABLED=off`
            // silently leaves tiering on. See `flags::runtime_flag_default_on`.
            p.tiered_enabled = !cratonvm_types::flags::value_is_off(&s);
        }
        p
    }
}

// ───────────────────────────────────────────────────────────────────────────────
// MethodKey
// ───────────────────────────────────────────────────────────────────────────────

/// Uniquely identifies a method: its declaring class's identity plus its name
/// and descriptor.
///
/// **Loader-aware.** Two classes of the same name in different loaders -- the
/// ByteBuddy / Mockito / servlet-container shape -- declare different methods,
/// and a key made of names alone merged them: one loader's `ineligible`
/// verdict, `c2_bailout`, OSR denial or queued request applied silently to the
/// other, and unloading one class discarded the tiering state of both.
/// `class_id` is what separates them. `ClassId(0)` means "not resolved" and is
/// what [`MethodKey::new`] produces; it remains for tests and for callers that
/// genuinely hold only a name, and such a key is matched by name.
///
/// The strings are `Arc<str>` so a key built from a `CachedBytecodeMethod`
/// costs three reference-count increments rather than three heap allocations:
/// the interpreter builds one at every tier-up stride.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MethodKey {
    pub class_id: ClassId,
    pub class_name: Arc<str>,
    pub method_name: Arc<str>,
    pub descriptor: Arc<str>,
}

impl MethodKey {
    /// A key with no resolved class identity. See the type's doc.
    pub fn new(
        class_name: impl Into<Arc<str>>,
        method_name: impl Into<Arc<str>>,
        descriptor: impl Into<Arc<str>>,
    ) -> Self {
        Self::with_class_id(ClassId::new(0), class_name, method_name, descriptor)
    }

    /// A key for a method of the loaded class `class_id`.
    pub fn with_class_id(
        class_id: ClassId,
        class_name: impl Into<Arc<str>>,
        method_name: impl Into<Arc<str>>,
        descriptor: impl Into<Arc<str>>,
    ) -> Self {
        Self {
            class_id,
            class_name: class_name.into(),
            method_name: method_name.into(),
            descriptor: descriptor.into(),
        }
    }

    /// Whether this key names a method of the class `(class_id, class_name)`.
    ///
    /// Identity decides when both sides carry one. When either side does not,
    /// the name decides, which is the old behaviour and errs towards
    /// invalidating too much rather than too little.
    pub fn belongs_to(&self, class_id: ClassId, class_name: &str) -> bool {
        if self.class_id.as_u32() != 0 && class_id.as_u32() != 0 {
            self.class_id == class_id
        } else {
            &*self.class_name == class_name
        }
    }
}

// ───────────────────────────────────────────────────────────────────────────────
// MethodState
// ───────────────────────────────────────────────────────────────────────────────

/// Tracks per-method compilation state.
///
/// There is no profile in here any more. A `MethodProfile` with branch,
/// receiver, type and null tables used to live on this struct, filled by
/// `record_branch` / `record_receiver` / `record_type_check` /
/// `record_null_check`, none of which had a caller outside this file's tests.
/// The live profile is `crate::profile::ProfileStore`.
#[derive(Debug, Clone)]
pub struct MethodState {
    /// Unique method identifier.
    pub method_key: MethodKey,
    /// Current compilation tier.
    pub current_tier: CompilationTier,
    /// Number of interpreter invocations.
    pub invocation_count: u64,
    /// Number of OSR requests made for this method -- each one a loop the
    /// interpreter's back-edge schedule judged hot.
    pub backedge_count: u64,
    /// Whether a METHOD-ENTRY request (`osr_bci == None`) for this method
    /// holds the method-entry in-flight slot: queued, or dispatched and still
    /// compiling. See [`CompilerCore::admit`].
    ///
    /// OSR requests have their own slot ([`Self::osr_in_flight`]) since round
    /// 9 wave 2. With ONE slot, `request_osr` was refused for the whole C1
    /// wait + compile and then the whole `Low` C2-supersede wait + compile --
    /// exactly the stretch in which a hot loop in a still-interpreted
    /// activation needs OSR -- and the refusal was not counted. The single
    /// slot's purpose, never compiling ONE artifact twice concurrently, does
    /// not need a method-entry body and an OSR body (a separate cache, keyed
    /// by bci) to exclude each other.
    pub queued_for_compilation: bool,
    /// Tier of the request holding the method-entry slot -- the identity a
    /// leaving request is matched against before it may release the slot. See
    /// [`CompilerCore::release_slot_for`].
    pub queued_tier: Option<CompilationTier>,
    /// Whether an OSR request (`osr_bci == Some`) for this method holds the
    /// OSR in-flight slot. One OSR request per method at a time, whatever the
    /// bci -- the interpreter's per-frame back-edge schedule re-asks.
    pub osr_in_flight: bool,
    /// Tier of the request holding the OSR slot (always C2 from `request_osr`;
    /// kept so the release match is the same `(tier, bci)` identity as before).
    pub osr_in_flight_tier: Option<CompilationTier>,
    /// OSR bci of the request holding the OSR slot. With
    /// `osr_in_flight_tier`, the identity a leaving OSR request is matched
    /// against before it may release the slot.
    pub queued_osr_bci: Option<u32>,
    /// t1 wave 11 lane JTIER (TIERMGR21-2, `CRATONVM_JIT_TIER_SLOT_TICKET`):
    /// the ticket of the LAST request admitted to the method-entry slot
    /// ([`CompilerCore::admit`]); `0` until one was. Not cleared on release, so
    /// a completion whose ticket differs is known not to be this state's
    /// request (see [`CompilerCore::completion_is_orphaned`]).
    pub entry_slot_ticket: u64,
    /// [`Self::entry_slot_ticket`] for the OSR slot.
    pub osr_slot_ticket: u64,
    /// The request holding the METHOD-ENTRY slot opened a branch-profile window
    /// (`profile::arm_branch_profiling_for_c2`), which must be closed exactly
    /// once on whichever way the request leaves: completion, a stale drop, a
    /// deoptimization drop, a class invalidation or shutdown.
    ///
    /// The window used to be closed on EVERY C2 completion instead. That
    /// closed windows nobody opened (OSR tasks, the straight-to-C2 invocation
    /// path) and never closed the ones whose request was dropped rather than
    /// compiled, so the census drifted in both directions at once.
    pub branch_window_armed: bool,
    /// Counted traps (deopts whose action threw the body away), decayed over
    /// time. Soft deopts are not in here; see [`deopt_is_counted_trap`].
    pub deopt_count: u32,
    /// Withdrawals of this method's optimizing method-entry body, never
    /// decayed ([`WITHDRAWAL_RECOMPILE_CUTOFF`]; JIT round 21 wave 4, lane
    /// codecache). Reset only by a redefinition of the class.
    pub c2_withdrawals: u32,
    /// Counted traps per `(reason, bci)`, decayed together with `deopt_count`.
    pub trap_counts: FxHashMap<(DeoptReason, u32), u32>,
    /// `invocation_count` at the last trap decay.
    pub trap_decay_invocations: u64,
    /// [`process_uptime_ms`] at the last trap decay.
    pub trap_decay_ms: u64,
    /// Successful method-entry C2 compiles that spent more than
    /// [`MAX_C2_COMPILE_TIME_MS`] of compile-thread CPU time.
    pub c2_budget_overruns: u32,
    /// Last compilation time in milliseconds.
    pub last_compile_time_ms: u64,
    /// Whether this method is too complex for C2.
    pub c2_bailout: bool,
    /// Consecutive background-compile attempts that ran but did not
    /// publish a compiled body (`finish(success=false)`). Distinct
    /// from `c2_bailout` (which tracks *runtime* deopt-driven demotion from
    /// an already-published C2 body) — this tracks the compile STEP itself
    /// never producing/publishing code, so `current_tier` never advances
    /// past whatever tier last actually succeeded. `should_compile` stops
    /// recommending further attempts once this saturates.
    ///
    /// Counts **compile attempts that ran and failed** only. A task the VM
    /// declined on policy grounds never lands here — see [`Self::ineligible`].
    ///
    /// **Method-entry attempts only.** OSR attempts are charged to
    /// [`Self::osr_fail_count`]; see there for why one counter for both
    /// pipelines could never saturate.
    pub tier_fail_count: u32,
    /// [`Self::tier_fail_count`] for the OSR pipeline: consecutive OSR compile
    /// attempts that ran and did not publish an artifact.
    ///
    /// # Why this is not the same counter
    ///
    /// It was. Both pipelines charged `tier_fail_count` and, in
    /// [`CompilerCore::finish`], **any** success cleared it — including a
    /// successful OSR publish, which advances no tier (an OSR artifact lives in
    /// its own cache). So for a loop-heavy method whose method-entry compile
    /// deterministically fails while its OSR compile succeeds — a shape the
    /// interpreter's per-frame back-edge schedule re-asks for constantly — the
    /// sequence was: method-entry attempt fails (1), OSR publishes (back to 0),
    /// method-entry attempt fails (1), OSR publishes (0), forever. The counter
    /// could not saturate, so `should_compile`'s
    /// `tier_fail_count >= MAX_TIER_FAIL_RETRIES` give-up arm was unreachable
    /// for exactly the population it exists to bound, and every stride past the
    /// hotness bar ran a whole wasted pipeline. `tier_fail_count`'s own doc says
    /// "`should_compile` stops recommending further attempts once this
    /// saturates" — it could not.
    ///
    /// Split rather than "only reset on a method-entry success", because that
    /// alternative turns the OSR budget from *consecutive* failures into
    /// *lifetime* failures: a method with a flaky OSR compile (fail, fail,
    /// publish, fail) would lose OSR service for the rest of the process. Each
    /// pipeline now has its own three consecutive attempts, cleared by its own
    /// publish.
    ///
    /// The OSR door still honours `tier_fail_count` as well
    /// ([`CompilerCore::admit`]), so the coupling that existed before is kept
    /// in the one direction it was ever asserted in; what is removed is the
    /// reset, which is what made the ceiling unreachable.
    pub osr_fail_count: u32,
    /// The VM declined to compile this method for a reason that cannot change
    /// for the life of the process — the JIT skip list rejected it, a
    /// permanent OSR denial applies, or its compile panicked.
    ///
    /// This exists because the two outcomes used to be conflated. A policy
    /// decline reported `success=false` exactly like a failed compile, so a
    /// permanently-ineligible method was enqueued and declined **three times**
    /// before `tier_fail_count` saturated and the tier gates gave up. Two of
    /// those three round-trips were pure waste (queue traffic, a worker
    /// wake-up and a skip-list evaluation each), and the hot interpreter path
    /// kept paying tier-up bookkeeping until the ban finally landed.
    ///
    /// The worse cost was diagnostic. `hot_but_stuck_in_interpreter` reported
    /// `tier_fail_count=3` identically for methods banned *by design*
    /// (`org/hibernate/` wholesale, `org/h2/` via HIB-LONGTAIL.1, the
    /// `AbstractQueuedSynchronizer` state family) and for methods whose
    /// codegen genuinely broke. That is what made "1531 of 1642 hot methods
    /// never compile" impossible to act on without re-deriving every entry by
    /// hand — see
    /// `smoketests-concurrent-query-throughput-20260723-RETIRED.md`.
    ///
    /// Those three bans have all since been deleted (2026-07-29 and
    /// 2026-07-31), so the same workload now reports `ineligible-by-policy=0`.
    /// The split is what makes that readable as "nothing is banned" rather than
    /// as a compiler that stopped failing; keep it.
    ///
    /// Recording the decline once, under its own flag, ends the churn on the
    /// first attempt and leaves `tier_fail_count` meaning only what its name
    /// says. A class redefinition clears it
    /// ([`TieredCompilationManager::on_class_redefined`]): the verdict was
    /// about the old bytecode.
    pub ineligible: bool,
    /// [`Self::ineligible`] for the METHOD-ENTRY pipeline only: set by a
    /// runtime `MakeNotCompilable` ([`TieredCompilationManager::on_deoptimization`]).
    ///
    /// r11-tier. That verdict is evidence about one call site's speculation,
    /// and `compile_gate::admit` deliberately does not raise it at the OSR door
    /// (its stated case: one cold `assert` under `-ea` is an unconditional
    /// reason-8 trap, `MakeNotCompilable` on the first occurrence, and must not
    /// take the method's hot loops their OSR entry). Recording it as
    /// `ineligible` re-created exactly that loss one layer up: `request_osr`
    /// and the OSR arm of `admit` refuse on `ineligible`, and the background
    /// OSR path is the default. Every method-entry gate asks this beside
    /// `ineligible`; the two OSR gates do not. Cleared by a redefinition.
    pub entry_ineligible: bool,
    /// The last request for this method was refused because the code cache
    /// is at its cap -- `CompilerCore::admit` asked, not a worker.
    ///
    /// A cap refusal is TRANSIENT (`compile_gate::CompileRefusal::is_transient`):
    /// a sweep or an invalidation that frees room makes the same request
    /// succeed. So it is not `ineligible`, and it is not charged to
    /// `tier_fail_count` -- which is what it used to be, three times, after
    /// which the method was settled for the life of the process and the room
    /// a sweep freed could never be used by it (the RT-8 soak, 2026-09-18).
    ///
    /// It does make the method SETTLED at the current generation, so the
    /// interpreter stops asking while the cap holds. The generation moves when
    /// room may have come back: `note_body_withdrawn` and
    /// `note_code_cache_pressure` both bump it.
    pub deferred_at_cap: bool,
    /// [`process_uptime_ms`] (never 0) when a method-entry request for this
    /// method was first admitted; `0` until then. With the two stamps below,
    /// the time-to-tier instrument ([`CompilationStats::nominate_to_first_body_ms_total`]).
    pub first_nominated_ms: u64,
    /// Uptime when this method's first method-entry body published; `0` until
    /// then.
    pub first_body_ms: u64,
    /// Uptime when this method's first method-entry C2 body published; `0`
    /// until then.
    pub first_c2_ms: u64,
    /// The raw back-edge word of this method's compiled body as the last
    /// entry-counter drain read it (`JitEntryCounter::back_edges`, unweighted),
    /// `None` until a drain reported one. Diagnostic only: printed as
    /// `drained-be=` on the `CRATONVM_DBG_TIER_ENQUEUE` line, so a reader can
    /// tell which door promoted a method (HANDLER8-2, JIT round 19 wave 9,
    /// lane handler9): `invocations=` is the drain's WEIGHTED count, which
    /// mixes entries and back edges and under `CRATONVM_JIT_C1_BACK_EDGE_C2_BAR`
    /// can be back edges alone.
    pub last_drained_back_edges: Option<u64>,
    /// Uptime (never 0) when a C1 body published OUTSIDE the manager
    /// (`note_published_outside_manager_with_c2_candidate`) was first found to
    /// pass `c2_upgrade_would_engage` -- the population HANDLER8-1's census
    /// counts; `0` when it never was, or once the method's count crossed the
    /// C2 bar (the census then has its answer and the stamp is spent).
    pub outside_c2_candidate_ms: u64,
    /// The method was counted ONCE as an outside-manager C2 candidate
    /// (HANDLER8-1 census): a second outside publish of the same method (a
    /// recompile after a withdrawal) is not a second candidate.
    pub outside_c2_candidate_seen: bool,
    /// JIT r25 wave 4 (`CRATONVM_JIT_OUTSIDE_C1_DRAIN_C2`): a C1-family body of
    /// this method was published OUTSIDE the manager (the callee compiler, the
    /// eager chain, the synchronized callee door), so it never got the
    /// manager path's immediate C1 -> C2 supersede.
    pub outside_c1_published: bool,
    /// The class-definition epoch at which this method's last method-entry
    /// compile parked "waiting on a class" ([`Completion::waiting_on_class`]),
    /// or `None`. While it is `Some` and the VM's epoch still equals it,
    /// `CompilerCore::admit` refuses method-entry requests without charging
    /// anything, except every [`WAITING_ON_CLASS_RELEASE_OFFERS`]-th one.
    /// Cleared when the epoch moved, on a publish and by a redefinition.
    /// Interpreter round i6 wave 116, lane L2 (page `i107-L2`).
    pub waiting_on_class_epoch: Option<u64>,
    /// Offers refused since the park (for the release stride).
    pub waiting_on_class_offers: u32,
    /// Uncharged waiting-on-class parks spent; bounded by
    /// [`MAX_WAITING_ON_CLASS_PARKS`], past which the completion is charged.
    pub waiting_on_class_parks: u32,
    /// JIT r22 wave 2, lane inline (INLINE22-1,
    /// `CRATONVM_JIT_C2_ACCEPTANCE_REOFFER`, opt-in): the invocation count at
    /// which a method whose C2 task published its single-pass fall-through
    /// after the acceptance gate refused the optimizing body is offered C2
    /// again ([`TieredCompilationManager::arm_acceptance_reoffer`]); `0` when
    /// not armed. Cleared when the re-offer fires.
    pub acceptance_reoffer_bar: u64,
    /// Re-offers armed so far; bounded by [`MAX_ACCEPTANCE_REOFFERS`].
    pub acceptance_reoffers: u8,
    /// The VM's redefine epoch (`JitVerdictRegistry`'s) when the re-offer was
    /// armed: a re-offer fires only for the bytecode it judged (JIT r22 wave
    /// 3, item 1 of `r22-review-w3-intrinsics-on-inline-FIXED-20261020.md`).
    pub acceptance_reoffer_epoch: u32,
    /// JIT r23 wave 9, lane L6 (`CRATONVM_JIT_C2_PUBLICATION_REFUSAL_REFUND`):
    /// refused optimizing publications whose `tier_fail_count` charge was
    /// handed back ([`publication_refusal_refund`]); bounded by
    /// [`MAX_PUBLICATION_REFUSAL_REFUNDS`].
    pub publication_refusal_refunds: u8,
    /// JIT r23 wave 11 (`CRATONVM_JIT_C2_AFTER_OPTIMIZING_OSR`): an OSR body
    /// of this method was published by a C2-tier OSR task (the optimizing
    /// body, or its single-pass fallback: either proves a hot loop). A single-pass method-entry body
    /// never migrates into it (only an interpreted frame takes the OSR door),
    /// so with the switch on a C1 entry body is promoted to C2 on that
    /// evidence ([`CompilerCore::promote_c1_after_optimizing_osr`]).
    pub optimizing_osr_published: bool,

    /// JIT round 21 lane tiermgr (INDYCONST21-1, `CRATONVM_JIT_TIER_RETRY_OWED`):
    /// a C2 retry ([`CompilerCore::request_c2_retry`], any door) that found the
    /// method-entry slot held and was answered `AlreadyQueued`, owed to the
    /// request holding the slot: the retry generation it was asked at
    /// ([`CompilerCore::retry_generation`]) and the door that asked (the newest
    /// such request wins; one owed retry per method). An optimizing compile of
    /// the method DISPATCHED at or after that generation answers it (it read
    /// the state the request was about); any other exit of the slot holder
    /// (an older compile, a single-pass one, a drop) re-asks the retry once
    /// the slot is free ([`CompilerCore::take_unanswered_retry`]). `None`
    /// when nothing is owed.
    pub retry_owed: Option<(u64, &'static str)>,
    /// A late-bind C2 retry ([`TieredCompilationManager::request_late_bind_recompile`])
    /// was answered [`C2RetryAnswer::AlreadyQueued`] while the method-entry
    /// slot was held; the worker asks the retry again once the task holding
    /// the slot finishes ([`CompilerCore::take_late_bind_retry_owed`],
    /// `CRATONVM_JIT_LATE_BIND_RETRY_OWED`).
    pub late_bind_retry_owed: bool,
    /// j27 lane SEEDOWE (`CRATONVM_JIT_IR_IC_SEED_OWED_RETRY`, opt-in): an
    /// `ic-profile-seed` retry refused by a refusal that can lift, paid by the
    /// invocation verdict once it lifts (`ic_seed_owed::pay`). `None` when
    /// nothing is owed.
    ic_seed_owed: Option<ic_seed_owed::OwedSeedRetry>,
    /// Seed retries owed so far; bounded by [`MAX_OWED_IC_SEED_RETRIES`].
    ic_seed_owes: u8,
}

impl MethodState {
    fn new(key: MethodKey) -> Self {
        Self {
            method_key: key,
            current_tier: CompilationTier::Interpreter,
            invocation_count: 0,
            backedge_count: 0,
            queued_for_compilation: false,
            queued_tier: None,
            osr_in_flight: false,
            osr_in_flight_tier: None,
            queued_osr_bci: None,
            entry_slot_ticket: 0,
            osr_slot_ticket: 0,
            branch_window_armed: false,
            deopt_count: 0,
            c2_withdrawals: 0,
            trap_counts: FxHashMap::default(),
            trap_decay_invocations: 0,
            trap_decay_ms: process_uptime_ms(),
            c2_budget_overruns: 0,
            last_compile_time_ms: 0,
            c2_bailout: false,
            tier_fail_count: 0,
            osr_fail_count: 0,
            ineligible: false,
            entry_ineligible: false,
            deferred_at_cap: false,
            first_nominated_ms: 0,
            first_body_ms: 0,
            first_c2_ms: 0,
            last_drained_back_edges: None,
            outside_c2_candidate_ms: 0,
            outside_c2_candidate_seen: false,
            outside_c1_published: false,
            waiting_on_class_epoch: None,
            waiting_on_class_offers: 0,
            waiting_on_class_parks: 0,
            acceptance_reoffer_bar: 0,
            acceptance_reoffers: 0,
            acceptance_reoffer_epoch: 0,
            publication_refusal_refunds: 0,
            optimizing_osr_published: false,
            retry_owed: None,
            late_bind_retry_owed: false,
            ic_seed_owed: None,
            ic_seed_owes: 0,
        }
    }

    /// Whether nothing the invocation hook can do will change this method's
    /// tiering until something invalidates the state.
    ///
    /// Deliberately narrow. A C1 method that has not bailed out of C2 is NOT
    /// settled: it can still be recommended for C2 once it is hot enough.
    fn tiering_is_settled(&self) -> bool {
        self.ineligible
            || self.entry_ineligible
            || self.entry_out_of_retries()
            || self.current_tier >= CompilationTier::C2
            || (self.c2_bailout && self.current_tier >= CompilationTier::C1)
    }

    /// Whether the METHOD-ENTRY pipeline's retry budget is spent: the one
    /// spelling every method-entry gate asks (JIT r22 wave 2, lane inline,
    /// INLINE22-2; byte-identical to the hand-written comparisons it replaced).
    /// The OSR pipeline's budget is [`osr_out_of_retries`].
    #[inline]
    fn entry_out_of_retries(&self) -> bool {
        self.tier_fail_count >= MAX_TIER_FAIL_RETRIES
    }
}

/// What one visit to the invocation hook decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvocationVerdict {
    /// The tier a compile was just queued at, if any.
    pub recommended: Option<CompilationTier>,
    /// Non-zero when the hook can no longer change this method's tiering. The
    /// value is the manager's settled generation at the moment of the answer;
    /// the caller stores it on the call site (`CachedBytecodeMethod::
    /// tiering_settled`) and skips the manager -- its global `methods` mutex,
    /// and the key it would build -- while
    /// [`TieredCompilationManager::tiering_settled`] still accepts the stamp.
    /// Any event that could unsettle a method (a deopt, an invalidation, a
    /// redefinition, a policy change) moves the generation, which expires
    /// every stamp at once.
    pub settled_generation: u32,
}

// ───────────────────────────────────────────────────────────────────────────────
// Trap accounting
// ───────────────────────────────────────────────────────────────────────────────

/// Counted traps at one `(reason, bci)` before the method stops being offered
/// to C2 (`c2_bailout`, for the life of the method: decay only slows the
/// count). HotSpot's `PerBytecodeTrapLimit` is 4 too, but there it only stops
/// the SPECULATION at that bci; the method stays C2-compilable. Under
/// `CRATONVM_JIT_PER_BCI_TRAP_LIMIT_KEEPS_C2` this limit no longer bails
/// ([`counted_trap_bails_c2`]); the VM's de-spec registry withdraws the
/// speculation at the same count (`PER_BCI_DESPEC_LIMIT`).
pub const PER_BCI_TRAP_LIMIT: u32 = 4;

/// Counted traps across the whole method, however they are spread over sites,
/// before it stops being offered to C2 -- the analogue of HotSpot's per-method
/// trap limit and recompilation cutoff.
pub const PER_METHOD_TRAP_CUTOFF: u32 = 16;

/// j27 wave 5 (lane TRAPPROF; page
/// `j27-per-bci-trap-limit-bans-c2-for-the-life-of-the-method-FIXED-20261029.md`):
/// `CRATONVM_JIT_PER_BCI_TRAP_LIMIT_KEEPS_C2`, **default OFF** (`1` turns it
/// on). A tiering-policy change that needs a host A/B before it is flipped.
pub fn per_bci_trap_limit_keeps_c2_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_PER_BCI_TRAP_LIMIT_KEEPS_C2")
}

/// Whether a counted trap that left its `(reason, bci)` at `at_site` traps
/// and its method at `deopt_count` takes the method off C2. The per-method
/// cutoff always does, which bounds a method that keeps trapping at many
/// bcis, or at one bci whose de-spec the planner never honours. The per-bci
/// limit does unless `site_limit_keeps_c2` (HotSpot's
/// `PerBytecodeTrapLimit`, which withdraws only the speculation).
fn counted_trap_bails_c2(at_site: u32, deopt_count: u32, site_limit_keeps_c2: bool) -> bool {
    deopt_count >= PER_METHOD_TRAP_CUTOFF
        || (at_site >= PER_BCI_TRAP_LIMIT && !site_limit_keeps_c2)
}

/// Withdrawals of a method's OPTIMIZING body -- a dependency invalidation (a
/// class gained a subtype, an inlined class was redefined or unloaded), an
/// eviction -- after which it is no longer offered to C2 and stays C1
/// (JIT round 21 wave 4, lane codecache). HotSpot's
/// `PerMethodRecompilationCutoff` (400): `nmethod::make_not_entrant` of a C2
/// body bumps the method's decompile count, a CHA deopt included, and past
/// the cutoff the method is not C2-compilable. The trap ledgers above cap the
/// trap-driven recompile loop; without this a body whose dependency is
/// invalidated on every round (a new subtype per round) was recompiled at C2
/// forever. Never decayed, like HotSpot's count; reset only by a redefinition
/// of the class (new bytecode, a new method in HotSpot's terms). Charged only
/// for a dependency invalidation or a trap eviction of an optimizing body
/// (rev-x64 item 8, wave 5): never for a cache-pressure eviction, a flush or an
/// unload sweep, so an unload/reload cycle cannot walk a method down to C1.
pub const WITHDRAWAL_RECOMPILE_CUTOFF: u32 = 400;

/// `CRATONVM_JIT_WITHDRAWAL_RECOMPILE_CUTOFF` (default ON, `=0` restores the
/// uncapped recompile): see [`WITHDRAWAL_RECOMPILE_CUTOFF`]. Read once per
/// withdrawal report, not per call.
fn withdrawal_recompile_cutoff_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_WITHDRAWAL_RECOMPILE_CUTOFF")
}

/// `CRATONVM_JIT_LATE_BIND_RETRY_OWED` (default ON, `=0` kills): a late-bind C2
/// retry answered `AlreadyQueued` is owed to the method, and the worker asks it
/// again right after the task holding the slot finishes
/// ([`MethodState::late_bind_retry_owed`]).
///
/// Page `jr21-osr-early-reentry-request-rows-FIXED-20261026.md`
/// (`docs/internal/fixed-bugs/`): the task holding the slot may have passed
/// its late-bind mark check BEFORE the mutator set the mark (the callee's
/// class loaded and compiled while it compiled), so it publishes the body with
/// the call on the blind dispatch helper, takes no mark, and nothing asks
/// again -- `R21AcceptOsrEarly late-callee-act` ran ~9-14 ms per 20 000-
/// iteration activation instead of ~45 us in about one run in fifteen under
/// `CRATONVM_JIT_OSR_EARLY_REENTRY`. The bit is set and taken under
/// `methods`, the lock `finish` releases the slot under, so no ordering of the
/// two loses it. Cost when the queued task had not started yet (it then takes
/// the mark itself): one redundant optimizing compile. Read only on that
/// answer (cold).
fn late_bind_retry_owed_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_LATE_BIND_RETRY_OWED")
}

/// The `request_c2_retry` door names that owe their retry on `AlreadyQueued`
/// ([`late_bind_retry_owed_enabled`]): the late-bind door and the worker's own
/// re-ask, which can find the slot held again by another tier's task.
const LATE_BIND_RETRY_DOORS: [&str; 2] = ["late-bind", "late-bind-owed"];

/// `CRATONVM_JIT_LATE_BIND_RETRY_OWED_ON_DROP` (t1 wave 11 lane JTIER;
/// correctness, default ON, `=0` kills): a method-entry request dropped before
/// it compiled also hands on the late-bind owed bit. See
/// [`CompilerCore::take_dropped_owed_retry`]. Read only on a drop (cold).
fn late_bind_retry_owed_on_drop_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_LATE_BIND_RETRY_OWED_ON_DROP")
}

/// JIT r22 wave 1, lane inline (`jit-r10-tier2-proposals.md` section 1):
/// `CRATONVM_JIT_OSR_OWN_FAIL_BUDGET` (opt-in, default OFF). With it on, the
/// OSR pipeline is gated by its OWN retry budget (`osr_fail_count`) only, so
/// three method-entry failures no longer close the OSR door of a method whose
/// OSR compile was never tried. Read only on the rare arm where the
/// method-entry budget is already spent (see [`osr_out_of_retries`]).
fn osr_own_fail_budget_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_OSR_OWN_FAIL_BUDGET")
}

/// t1 wave 11 lane JTIER (TIERMGR21-2 of
/// `docs/internal/proposals/jit/tier1/jit-r21-tiermgr-proposals.md`):
/// `CRATONVM_JIT_TIER_SLOT_TICKET` (correctness, default ON, `=0` restores the
/// `(tier, bci)` match). Every admission to an in-flight slot draws a ticket
/// ([`CompilerCore::admit`]); the request carries it through the queue, the
/// stale/drop paths and its [`Completion`], and [`CompilerCore::release_slot_for`]
/// releases the slot only for the ticket that holds it. Without it a compile
/// of a method whose state was removed while it ran (`invalidate_class` of an
/// identity-less key: the same `MethodKey` names the next class loaded under
/// that name) released a same-tier slot the NEW state granted meanwhile,
/// admitting a duplicate concurrent compile, and [`CompilerCore::finish`]
/// applied the old class's outcome (`current_tier`, fail counts) to the new
/// state. Read once per manager.
fn tier_slot_ticket_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_TIER_SLOT_TICKET")
}

/// JIT round 23 wave 1, lane L1 (THROWS22-1 of
/// `docs/feature-designs/jit-ir-optimizations.md`; page
/// `docs/feature-designs/deopt-osr.md`):
/// `CRATONVM_JIT_OSR_PROFIT_EXIT` (perf, default ON, `=0` kill). When the VM
/// accepts an optimizing OSR body that can be entered at every loop header of
/// a method whose SINGLE-PASS OSR body is published, that body is armed
/// (`JitCache::arm_osr_profit_exit`): its back-edge exit polls are forced and
/// the poll verdict sends a frame running it out to the interpreter at the
/// poll's header, whose next back edge enters the optimizing body. Before, a
/// `main`-shaped activation that entered the single-pass body first (the
/// worker's optimizing verdict still owed, a transient refusal) ran
/// single-pass for the rest of its life. Read once per VM, by
/// `JitCache::new`.
pub fn osr_profit_exit_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_OSR_PROFIT_EXIT")
}

/// The loop headers of `code` an OSR profit exit may leave a single-pass OSR
/// body toward: every target of a textual back edge
/// (`bytecode_analysis::back_edges`), sorted and deduplicated. A single-pass
/// body's exit poll leaves at its header (a `goto` back edge) or at its
/// conditional back-edge branch, whose re-execution in the interpreter takes
/// that same back edge, or falls out of the loop into code whose next back
/// edge targets another header of this set. So an optimizing OSR body that
/// can be entered at every pc returned here is the one the exiting frame
/// enters at its next back edge, whichever poll it left at (JIT round 23
/// wave 1, lane L1). Empty for a method without a loop.
pub fn osr_profit_exit_headers(code: &[u8]) -> Vec<usize> {
    let mut headers: Vec<usize> = crate::bytecode_analysis::back_edges(code, code.len())
        .into_iter()
        .map(|(target, _)| target)
        .collect();
    headers.sort_unstable();
    headers.dedup();
    headers
}

/// JIT r22 wave 2, lane inline (INLINE22-1 of
/// `docs/feature-designs/profile-guided-inlining.md`, SPLICE21-2):
/// `CRATONVM_JIT_C2_ACCEPTANCE_REOFFER` (perf, opt-in). A C2 task whose
/// optimizing body the acceptance gate refused ("no evidence") publishes its
/// single-pass fall-through and leaves the method at C2, settled: no door asks
/// again, however long the method then runs. With the switch the VM arms a
/// re-offer ([`TieredCompilationManager::arm_acceptance_reoffer`]); once the
/// drained live invocation count reaches [`ACCEPTANCE_REOFFER_GROWTH`] times
/// the count at the refusal, the drain admits one more C2 task and grants the
/// IR refusal memo one re-judgement
/// (`JitVerdictRegistry::grant_ir_reoffer`), which a fresh refusal re-stamps
/// against `MAX_INPUT_REFUSALS`. Read at arm time only.
pub fn acceptance_reoffer_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_C2_ACCEPTANCE_REOFFER")
}

/// How many times a method may be re-offered C2 after acceptance refusals
/// (each re-offer is one discarded build at most).
pub const MAX_ACCEPTANCE_REOFFERS: u8 = 2;

/// The live invocation count must grow by this factor past the count at the
/// refusal before the re-offer fires: a profile (and every IC the
/// single-pass body publishes) that has seen four times as many calls is the
/// "matured" evidence SPLICE21-2 asks for. Counted on the fall-through
/// body's own entry counter, which starts at zero, so the bar is
/// `(GROWTH - 1) x` the count at the refusal in NEW calls (JIT r22 wave 3,
/// see `arm_acceptance_reoffer`).
pub const ACCEPTANCE_REOFFER_GROWTH: u64 = 4;

/// The floor of the re-offer bar, so a method refused at a tiny count is not
/// re-offered after a handful of calls.
pub const ACCEPTANCE_REOFFER_MIN_BAR: u64 = 20_000;

/// Whether the OSR pipeline's retry budget is spent for `state`: its own
/// `osr_fail_count`, and (unless `CRATONVM_JIT_OSR_OWN_FAIL_BUDGET` is on) the
/// method-entry `tier_fail_count` too, the coupling the r10 split kept. The ONE
/// spelling `CompilerCore::admit`'s OSR arm and `request_osr_classified`'s
/// early check share.
///
/// The arm the r10 proposal asked to measure -- the method-entry budget alone
/// refusing an OSR request while `osr_fail_count == 0` -- prints
/// `[cratonvm-tier] osr-refused-by-entry-budget ..` under
/// `CRATONVM_DBG_TIER_ENQUEUE=1` (also when the switch admits it, then with
/// `admitted`), so a census can count the population the switch changes.
///
/// `own_budget` / `trace`: the two switches as the manager read them at
/// construction (`CompilerCore::osr_own_fail_budget` / `osr_budget_trace`;
/// JIT r22 wave 3, item 3 of `r22-review-w3-intrinsics-on-inline-FIXED-20261020.md`:
/// no environment read under the `methods` mutex).
fn osr_out_of_retries(state: &MethodState, own_budget: bool, trace: bool) -> bool {
    if state.osr_fail_count >= MAX_TIER_FAIL_RETRIES {
        return true;
    }
    if state.tier_fail_count < MAX_TIER_FAIL_RETRIES {
        return false;
    }
    if state.osr_fail_count == 0 && trace {
        eprintln!(
            "[cratonvm-tier] osr-refused-by-entry-budget {}.{}{}: tier_fail_count={} osr_fail_count=0 -> {}",
            state.method_key.class_name,
            state.method_key.method_name,
            state.method_key.descriptor,
            state.tier_fail_count,
            if own_budget {
                "admitted (CRATONVM_JIT_OSR_OWN_FAIL_BUDGET)"
            } else {
                "refused"
            },
        );
    }
    !own_budget
}

/// Charge one decompilation of an OPTIMIZING body of `state`'s method -- its
/// method-entry body or (CODECACHE21-4) its optimizing OSR body, withdrawn by
/// a dependency invalidation or a trap eviction
/// (`JitCache::report_charged_withdrawals` decides which withdrawals count;
/// rev-x64 item 8) -- and answer whether this charge set `c2_bailout`.
fn charge_optimizing_withdrawal(state: &mut MethodState, cutoff_on: bool) -> bool {
    state.c2_withdrawals = state.c2_withdrawals.saturating_add(1);
    if cutoff_on && !state.c2_bailout && state.c2_withdrawals >= WITHDRAWAL_RECOMPILE_CUTOFF {
        state.c2_bailout = true;
        return true;
    }
    false
}

/// JIT round 22 (lane sync, CODECACHE21-3): render the per-method withdrawal
/// census from `(c2_withdrawals, c2_bailout, display name)` rows, `None` when
/// there are none. The five methods with the most charged withdrawals, ties by
/// name so the line is deterministic:
///
/// `[cratonvm] JIT withdrawal census: methods=N bailed=B cutoff=400 | top: m=K(bailed) ...`
fn withdrawal_census_line(mut rows: Vec<(u32, bool, String)>) -> Option<String> {
    if rows.is_empty() {
        return None;
    }
    let methods = rows.len();
    let bailed = rows.iter().filter(|(_, b, _)| *b).count();
    rows.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.2.cmp(&b.2)));
    let top: Vec<String> = rows
        .iter()
        .take(5)
        .map(|(n, b, name)| format!("{name}={n}{}", if *b { "(bailed)" } else { "" }))
        .collect();
    Some(format!(
        "[cratonvm] JIT withdrawal census: methods={methods} bailed={bailed} cutoff={WITHDRAWAL_RECOMPILE_CUTOFF} | top: {}",
        top.join(" ")
    ))
}

/// Trap counts halve once the method has run this many further invocations...
pub const TRAP_DECAY_INVOCATIONS: u64 = 20_000;

/// ...or once this much process time has passed, whichever comes first.
///
/// The time arm exists because a method whose body is recompiled immediately
/// after each trap (the eager `RecompileAndReinterpret` re-queue in
/// `DeoptimizationController::deoptimize`) stops reaching the interpreter's
/// invocation hook, so its invocation count stops moving.
pub const TRAP_DECAY_MS: u64 = 60_000;

/// Whether a deopt for this REASON is charged to the method's trap ledger at
/// all — independently of what action any policy recommends for it.
///
/// This is the half of [`deopt_is_counted_trap`] that does not depend on the
/// action, lifted out so the *other* trap ledger
/// ([`crate::deopt::DeoptimizationLog`]) can ask exactly the same question.
/// Both ledgers used to answer it differently: this module excluded the two
/// soft reasons, the log excluded nothing, and since the log is what produces
/// `MakeNotCompilable` the weaker answer drove the strongest action. A hot
/// `try`/`catch` accrued ten `PendingException` events and the very next
/// genuine `UncommonTrap` came back `MakeNotEntrant` — the method blacklisted
/// for throwing.
///
/// A debugger's `TransferToInterpreter` and a pending exception's handler
/// hand-off are ordinary control flow, not failed speculation: recompiling
/// changes nothing and blacklisting permanently interprets every hot
/// `try`/`catch`. They are never charged, to anyone.
pub fn deopt_reason_is_charged(reason: DeoptReason) -> bool {
    !matches!(
        reason,
        DeoptReason::TransferToInterpreter | DeoptReason::PendingException
    )
}

/// Whether a deopt with this recommended action is a trap the method is
/// charged for.
///
/// Only actions that throw the body away count. `Reinterpret` is a soft exit:
/// the frame continues in the interpreter and the body is still good. Charging
/// those spent a method's whole C2 allowance on routine OSR loop exits -- three
/// of them banned C2 *and* OSR for the rest of the process. The reason filter
/// is [`deopt_reason_is_charged`].
pub fn deopt_is_counted_trap(reason: DeoptReason, action: DeoptAction) -> bool {
    if !deopt_reason_is_charged(reason) {
        return false;
    }
    matches!(
        action,
        DeoptAction::RecompileAndReinterpret
            | DeoptAction::MakeNotEntrant
            | DeoptAction::MakeNotCompilable
    )
}

/// Whether this deopt evicts the method-entry body from the JIT cache.
///
/// The single source of truth for `DeoptimizationController::deoptimize`, which
/// performs the eviction, and [`TieredCompilationManager::on_deoptimization`],
/// which must drop `current_tier` exactly when the body is gone: a tier that
/// reads "compiled" over an empty cache is a method that is never recommended
/// again and stays interpreted forever.
///
/// # What is never charged is never evicted
///
/// The exemption used to name exactly one `(reason, action)` pair — a soft
/// `OsrExit` — and that left `PendingException` and `TransferToInterpreter`
/// evicting a body that nothing ever charged for. Those two are the reasons
/// [`deopt_reason_is_charged`] rules out *by reason*, so the trap limits and
/// `c2_bailout` never see them: the method was evicted, re-crossed its hotness
/// threshold, was recompiled, threw again and was evicted again — forever, with
/// no back-off and no counter anywhere saying so. `recommend_action`'s own
/// comment says blacklisting a method for throwing "would permanently interpret
/// every hot try/catch"; the eviction predicate had simply never been taught
/// the same thing.
///
/// So the rule is now in two parts, and they compose to *what is never charged
/// is never evicted*:
///
///  * a reason that is structurally uncharged keeps its body, always — there is
///    no trap here to back off from, and evicting without charging is the
///    thrash;
///  * among charged reasons, a soft `OsrExit` keeps its body (the
///    long-standing exemption, argued in `recommend_action`'s `OsrExit` arm),
///    and everything else is evicted so the next call recompiles against a
///    profile that has seen the trap.
///
/// A first-occurrence count-based `Reinterpret` therefore still evicts, which
/// is deliberate: that IS the recompile-with-a-fresh-profile path, and making
/// `Reinterpret` alone the predicate would leave such a method trapping on
/// every call for the life of the process while nothing charged it.
pub fn deopt_evicts_method_body(reason: DeoptReason, action: DeoptAction) -> bool {
    if !deopt_reason_is_charged(reason) {
        return false;
    }
    !(reason == DeoptReason::OsrExit && action == DeoptAction::Reinterpret)
}

/// Halve a method's trap counts once per whole decay period (invocations or
/// time, whichever owes more) since the last decay.
///
/// Once PER PERIOD, not once per call (r9 wave 7). This is only ever asked on
/// the next counted trap, so a method that trapped three times, then ran
/// clean for an hour, used to come back with `3 / 2 = 1` charged at its site
/// -- one more trap from the limit instead of none -- while the deopt log's
/// ledger ([`crate::deopt::DeoptimizationLog`], the one that produces
/// `MakeNotCompilable`) applies every period it owes and read zero. The two
/// ledgers are documented as halving "on the same schedule"; now they do.
///
/// The time stamp advances by whole periods only, as the log's does, so the
/// remainder of the current period is not forgiven on every trap.
fn decay_traps(state: &mut MethodState, now_ms: u64) {
    let by_invocations = state
        .invocation_count
        .saturating_sub(state.trap_decay_invocations)
        / TRAP_DECAY_INVOCATIONS;
    let by_time = now_ms.saturating_sub(state.trap_decay_ms) / TRAP_DECAY_MS;
    let periods = by_invocations.max(by_time);
    if periods == 0 {
        return;
    }
    // A `u32` count is zero after 32 halvings; `checked_shr` answers `None` for
    // a shift of 32, which is exactly that zero.
    let halvings = periods.min(32) as u32; // Cast: capped at 32 above
    state.deopt_count = state.deopt_count.checked_shr(halvings).unwrap_or(0);
    state.trap_counts.retain(|_, n| {
        *n = n.checked_shr(halvings).unwrap_or(0);
        *n > 0
    });
    state.trap_decay_invocations = state.invocation_count;
    state.trap_decay_ms = if by_time > 0 {
        state
            .trap_decay_ms
            .saturating_add(by_time.saturating_mul(TRAP_DECAY_MS))
    } else {
        // Decayed by invocations alone: the time period restarts here, as it
        // always did.
        now_ms
    };
}

// ───────────────────────────────────────────────────────────────────────────────
// CompilationQueue
// ───────────────────────────────────────────────────────────────────────────────

/// What one priority band is worth, in milliseconds of waiting, to
/// [`CompilationQueue::dequeue`]'s aging score.
///
/// A `Low` request that has waited `2 * QUEUE_AGING_MS` outranks a `High` one
/// that has just arrived. 250 ms is chosen to be long compared with a single
/// compile (single-digit milliseconds for the kernels that reach the optimizing
/// tier; [`MAX_C2_COMPILE_TIME_MS`] is the pathological bound) and short
/// compared with a warm-up phase, so ordinary bursts keep strict priority and
/// only a genuinely starving band jumps.
pub const QUEUE_AGING_MS: u64 = 250;

/// Priority of a compilation task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompilationPriority {
    High,
    Normal,
    Low,
}

/// A single compilation task.
///
/// `PartialEq`/`Eq` so a test can assert on the exact request a queue handed
/// back or dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompilationTask {
    pub method_key: MethodKey,
    pub target_tier: CompilationTier,
    pub priority: CompilationPriority,
    pub enqueue_time_ms: u64,
    /// If this is an OSR compilation, the bytecode index.
    pub osr_bci: Option<u32>,
}

/// What [`TieredCompilationManager::request_osr_classified`] did with a
/// background OSR request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OsrRequest {
    /// Admitted and queued; the worker will compile it.
    Queued(CompilationTask),
    /// Not queued, but waiting is right: the method's OSR slot is held by a
    /// request still queued or compiling, or the code cache is at its cap.
    Pending,
    /// Not queued, and re-offering cannot change that until a redefinition or
    /// an install-epoch move: OSR denied, `c2_bailout`, `ineligible`, a retry
    /// budget spent, or tiered compilation disabled.
    Refused,
}

/// What a C2 retry door ([`TieredCompilationManager::request_late_bind_recompile`]
/// and its siblings) did with the request. JIT round 20 (lane stash, EAGER2-2 of
/// `jit-r19-eager2-proposals.md`; `r19-late-bind-pending-marks-outlive-a-refused-retry`):
/// the VM's late-bind door keeps a pending mark only when a compile of the
/// method will run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum C2RetryAnswer {
    /// Admitted and queued: a compile of the method will run.
    Enqueued,
    /// Not queued because the method's method-entry slot is held: a request
    /// already queued or compiling will publish (the EAGER19-3 race).
    AlreadyQueued,
    /// Not queued, and nothing will compile the method for this request:
    /// tiering disabled, `c2_bailout`, ineligible, the retry budget spent, the
    /// hotness gate, or the code-cache cap.
    Refused,
}

impl C2RetryAnswer {
    /// Will some compile of the method run (`Enqueued` or `AlreadyQueued`)?
    pub fn compile_will_run(self) -> bool {
        !matches!(self, Self::Refused)
    }
}

/// A queued [`CompilationTask`] plus the install epoch it was queued at.
///
/// The stamp lives on the queue entry, not on the task, on purpose:
/// [`CompilationTask`] is constructed by the VM (`vm/src/jit/helpers.rs`) as
/// well as by this module, and an epoch a caller has to remember to fill in is
/// an epoch that will eventually be filled in wrong — or, worse, filled in
/// with a *later* epoch than the request really carries, which reads as fresh.
/// [`CompilationQueue::enqueue`] stamps every request that enters the queue by
/// any door, so "everything queued carries the epoch it was queued at" holds
/// by construction rather than by convention.
///
/// See `docs/jit/broker-install-epoch.md` for which epoch this is and what it
/// answers.
#[derive(Debug, Clone, PartialEq, Eq)]
struct QueuedRequest {
    /// The manager's install epoch (`CompilerCore::current_install_epoch`: the
    /// owning VM's code-state epoch, else [`crate::jit_install_epoch`]) as of
    /// the moment this request entered the queue.
    install_epoch: u64,
    task: CompilationTask,
    /// t1 wave 11 lane JTIER (TIERMGR21-2): the slot ticket
    /// [`CompilerCore::admit`] drew for this request; `0` for a request queued
    /// without one (the raw queue tests).
    ticket: u64,
}

/// A queued request that will not be compiled, and why.
///
/// Carried *out* of the queue rather than acted on in place so the drop can be
/// counted and the method's in-flight slot released after the queue lock has
/// been dropped — see [`CompilerCore::retire_stale`] for the lock-order reason
/// that makes this mandatory rather than tidy.
#[derive(Debug, Clone, PartialEq, Eq)]
struct StaleRequest {
    task: CompilationTask,
    /// The [`crate::metrics::SCHEDULING_EVENTS`] name this drop is counted
    /// under.
    event: &'static str,
    /// Epoch the request was queued at, and the epoch now.
    queued_epoch: u64,
    current_epoch: u64,
    /// The request's slot ticket ([`QueuedRequest::ticket`]).
    ticket: u64,
}

/// Priority queue for compilation tasks. One per compile lane.
pub struct CompilationQueue {
    /// High priority: method-entry C2 and OSR requests.
    high: VecDeque<QueuedRequest>,
    /// Normal priority: C1 compilations.
    normal: VecDeque<QueuedRequest>,
    /// Low priority: C1→C2 supersedes and other speculative compilations.
    low: VecDeque<QueuedRequest>,
    /// Total tasks that left the queue, stale drops included.
    total_processed: u64,
}

impl CompilationQueue {
    fn new() -> Self {
        Self {
            high: VecDeque::new(),
            normal: VecDeque::new(),
            low: VecDeque::new(),
            total_processed: 0,
        }
    }

    /// [`Self::enqueue_ticketed`] with no slot ticket (`0`); the compile
    /// pipeline enqueues through `enqueue_ticketed` only.
    #[cfg(test)]
    fn enqueue(&mut self, task: CompilationTask, install_epoch: u64) {
        self.enqueue_ticketed(task, install_epoch, 0);
    }

    /// Push `task`, stamped with `install_epoch`, the slot ticket its
    /// admission drew ([`QueuedRequest::ticket`]) and — when the door left it
    /// at the placeholder `0` — with the time it entered the queue.
    ///
    /// Every policy door in this module built its task with
    /// `enqueue_time_ms: 0`, so the field was dead: nothing could age a request
    /// and the queue-latency it exists to express was unreadable. Stamping it
    /// *here* rather than at each door is the same argument [`QueuedRequest`]
    /// already makes for `install_epoch` — a value a caller has to remember to
    /// fill in is one that will eventually be filled in wrong. A door that DID
    /// supply a time keeps it.
    fn enqueue_ticketed(&mut self, mut task: CompilationTask, install_epoch: u64, ticket: u64) {
        let now = process_uptime_ms();
        // r11-tier: a stamp more than a day AHEAD of process uptime is on
        // another clock — the VM's deopt re-queue (`DeoptimizationController::
        // deoptimize`) supplies wall-clock UNIX milliseconds. `dequeue` read it
        // as age 0 forever, so that `High` "recompile now" request never aged
        // and sank below every `Normal` request older than one band. Restamped
        // on this queue's clock like an unset one.
        if task.enqueue_time_ms == 0 || task.enqueue_time_ms > now.saturating_add(86_400_000) {
            // `.max(1)`: `0` is the "unset" sentinel, so a request queued in the
            // first millisecond of the process must not re-stamp as unset.
            task.enqueue_time_ms = now.max(1);
        }
        let entry = QueuedRequest {
            install_epoch,
            task,
            ticket,
        };
        match entry.task.priority {
            CompilationPriority::High => self.high.push_back(entry),
            CompilationPriority::Normal => self.normal.push_back(entry),
            CompilationPriority::Low => self.low.push_back(entry),
        }
    }

    /// Pop the next request **without** consulting its stamp.
    ///
    /// The raw form, kept for [`TieredCompilationManager::dequeue_compilation`]
    /// — a manual/diagnostic drain that is not the compile pipeline. A caller
    /// that is about to *compile* the result must use [`Self::dequeue_fresh`].
    ///
    /// # Priority with aging, not strict priority
    ///
    /// This was `high.or(normal).or(low)` with no aging term, and the C2 lane
    /// carries both kinds of request: OSR is `High`
    /// ([`TieredCompilationManager::request_osr`]) and the C1→C2 supersede —
    /// "the path that produces nearly all C2 compiles", per
    /// [`CompilerCore::request_c2_upgrade`]'s own comment — is `Low`. During a
    /// warm-up phase with many hot loops the High band never empties, so under
    /// strict priority no C1→C2 upgrade runs at all: the methods that would
    /// have reached the optimizing tier stay at C1 for as long as the loops
    /// keep coming.
    ///
    /// The score is [`QUEUE_AGING_MS`] per band plus the request's age, so a
    /// band is skipped only while the band above it has been waiting less than
    /// one band's worth of time. Ties go to the higher band, which makes the
    /// all-same-age case (every task stamped in the same millisecond, i.e.
    /// every test in this file that does not sleep) byte-identical to the old
    /// strict order.
    fn dequeue(&mut self) -> Option<QueuedRequest> {
        let now = process_uptime_ms();
        let score = |band: &VecDeque<QueuedRequest>, rank: u64| -> Option<u64> {
            let front = band.front()?;
            // `saturating_sub`, so a caller-supplied stamp slightly ahead of
            // this clock reads as age 0 rather than as an enormous age that
            // would jump every band. (`enqueue` restamps one on another clock
            // entirely, such as the deopt re-queue's wall-clock ms.)
            let age = now.saturating_sub(front.task.enqueue_time_ms);
            Some(rank.saturating_mul(QUEUE_AGING_MS).saturating_add(age))
        };
        let best = [
            (2u64, score(&self.high, 2)),
            (1, score(&self.normal, 1)),
            (0, score(&self.low, 0)),
        ]
        .into_iter()
        .filter_map(|(rank, s)| s.map(|s| (rank, s)))
        // `max_by_key` on `(score, rank)` returns the LAST maximum, and the
        // array is ordered high → low, so a tie must prefer the earlier entry:
        // compare on rank as well and let the higher band win it.
        .max_by_key(|&(rank, s)| (s, rank))
        .map(|(rank, _)| rank);
        let entry = match best? {
            2 => self.high.pop_front(),
            1 => self.normal.pop_front(),
            _ => self.low.pop_front(),
        };
        if entry.is_some() {
            self.total_processed += 1;
        }
        entry
    }

    /// Pop the highest-priority request that is still current at install epoch
    /// `current`, pushing every request queued at an older epoch onto `stale`
    /// on the way past.
    ///
    /// Returns `None` only when the queue is empty. A run of stale requests
    /// therefore cannot starve a fresh one sitting behind them, and cannot
    /// make the worker read an occupied queue as empty.
    ///
    /// Nothing is dropped silently: every request that leaves the queue leaves
    /// through either the return value or `stale`, and the caller is required
    /// to retire `stale`. That is the whole fail-closed contract — a request
    /// that vanished here with no counter and no released slot would be a
    /// method that never compiles again, with nothing anywhere to say so.
    ///
    /// `total_processed` counts stale entries too: it means "left the queue",
    /// and a request that was dropped did leave. The drop-specific count is
    /// `crate::metrics::SCHEDULING_EVENTS[0]`.
    fn dequeue_fresh(
        &mut self,
        current: u64,
        stale: &mut Vec<StaleRequest>,
    ) -> Option<(CompilationTask, u64)> {
        while let Some(entry) = self.dequeue() {
            if entry.install_epoch >= current {
                return Some((entry.task, entry.ticket));
            }
            stale.push(StaleRequest {
                task: entry.task,
                event: crate::metrics::SCHEDULING_EVENTS[0],
                queued_epoch: entry.install_epoch,
                current_epoch: current,
                ticket: entry.ticket,
            });
        }
        None
    }

    fn len(&self) -> usize {
        self.high.len() + self.normal.len() + self.low.len()
    }

    /// Rank of the highest non-empty band: 2 for High, 1 for Normal, 0 for
    /// Low, `None` when empty. Strict priority: [`Self::dequeue`] can hand out
    /// an aged lower band's request ahead of it, so this is the lane-choice
    /// hint `pop_across_lanes` compares, not a prediction of that dequeue.
    fn front_rank(&self) -> Option<u8> {
        if !self.high.is_empty() {
            Some(2)
        } else if !self.normal.is_empty() {
            Some(1)
        } else if !self.low.is_empty() {
            Some(0)
        } else {
            None
        }
    }

    /// Remove and return every queued task `pred` selects, with its slot
    /// ticket, keeping the order of what remains.
    fn remove_matching(
        &mut self,
        pred: impl Fn(&CompilationTask) -> bool,
    ) -> Vec<(CompilationTask, u64)> {
        let mut removed = Vec::new();
        for band in [&mut self.high, &mut self.normal, &mut self.low] {
            band.retain(|entry| {
                if pred(&entry.task) {
                    removed.push((entry.task.clone(), entry.ticket));
                    false
                } else {
                    true
                }
            });
        }
        removed
    }

    /// Remove and return every queued request.
    ///
    /// Used at worker shutdown so abandonment is an event with a count rather
    /// than a queue that quietly stopped being drained.
    fn drain_all(&mut self) -> Vec<(CompilationTask, u64)> {
        self.high
            .drain(..)
            .chain(self.normal.drain(..))
            .chain(self.low.drain(..))
            .map(|entry| (entry.task, entry.ticket))
            .collect()
    }
}

// ───────────────────────────────────────────────────────────────────────────────
// CompilerCore — shared between the manager and its compile workers
// ───────────────────────────────────────────────────────────────────────────────

/// The lane serving the single-pass (C1-family) backend.
const C1_LANE: usize = 0;
/// The lane serving the optimizing backend: method-entry C2 and every OSR
/// request.
const C2_LANE: usize = 1;

/// The lane `task` is compiled on.
fn lane_of(task: &CompilationTask) -> usize {
    if task.osr_bci.is_some() || tier_uses_optimized_backend(task.target_tier) {
        C2_LANE
    } else {
        C1_LANE
    }
}

/// One compile lane: a queue, and the condvar its workers park on.
///
/// There are two because one worker used to serve everything, and a C1
/// compile -- the cheap one, and the one that gets a method out of the
/// interpreter -- waited behind every High-priority C2 and OSR compile queued
/// ahead of it. Each lane has its own workers ([`compiler_thread_counts`]), so
/// the cheap tier waits only on other cheap work. Ordering within a lane, and
/// the install-epoch gate, are exactly what the single queue had.
struct CompileLane {
    queue: Mutex<CompilationQueue>,
    wake: Condvar,
    /// zs s9/rs2 (`CRATONVM_TIER_C2_STANDBY`): how many of this lane's
    /// standby workers have been promoted (0 = none). A standby worker `k`
    /// (`1..`) parks on [`Self::standby_wake`] until this exceeds `k - 1`.
    standby_promoted: Mutex<usize>,
    standby_wake: Condvar,
    /// The backlog that promotes the next standby worker: queue depth right
    /// after an enqueue. 0 = no standby workers on this lane (the default).
    standby_depth: std::sync::atomic::AtomicUsize,
    /// Standby workers parked on this lane and not promoted yet.
    standby_parked: std::sync::atomic::AtomicUsize,
    /// zt t2/fp (`CRATONVM_TIER_C2_STANDBY_SCALE`, opt-in): the backlog that
    /// promotes the next standby worker grows with the live worker count
    /// (`depth x live`), instead of being `depth` for every promotion.
    standby_scale: AtomicBool,
    /// zt t2/fp: the compiler start a standby worker was spawned by. Bumped
    /// (under `standby_promoted`) by every start, so a worker parked by an
    /// earlier start that was shut down from a compile worker (its handles
    /// detached, not joined) leaves instead of being promoted into the new
    /// start's pool as an extra, uncounted worker.
    standby_epoch: std::sync::atomic::AtomicUsize,
    /// zs s9/rs2, made per VM in JIT r23 wave 11: standby workers this lane
    /// has started, and promoted, over every start of this manager
    /// (diagnostic only: [`c2_standby_line`]). They were process statics,
    /// which counted every VM's workers (page
    /// `r23-w11-c2-standby-counters-are-process-wide-FIXED-20261023.md`).
    standby_started_total: AtomicU64,
    standby_promoted_total: AtomicU64,
}

impl CompileLane {
    fn new() -> Self {
        Self {
            queue: Mutex::new(CompilationQueue::new()),
            wake: Condvar::new(),
            standby_promoted: Mutex::new(0),
            standby_wake: Condvar::new(),
            standby_depth: std::sync::atomic::AtomicUsize::new(0),
            standby_parked: std::sync::atomic::AtomicUsize::new(0),
            standby_scale: AtomicBool::new(false),
            standby_epoch: std::sync::atomic::AtomicUsize::new(0),
            standby_started_total: AtomicU64::new(0),
            standby_promoted_total: AtomicU64::new(0),
        }
    }

    /// Promote one parked standby worker if the backlog `depth` reached the
    /// lane's standby depth. Called after an enqueue, with no lock held.
    fn maybe_promote_standby(&self, depth: usize) {
        let need = self.standby_depth.load(Ordering::Relaxed);
        if need == 0 || depth < need || self.standby_parked.load(Ordering::Acquire) == 0 {
            return;
        }
        let mut promoted = self.standby_promoted.lock();
        // Re-check under the lock: two enqueues racing past the parked count
        // promote at most the workers that are parked.
        if self.standby_parked.load(Ordering::Acquire) == 0 {
            return;
        }
        // zt t2/fp: under the scale switch the k-th promotion (live workers
        // `1 + promoted`) waits for a backlog of `need x live`.
        if self.standby_scale.load(Ordering::Relaxed)
            && depth < need.saturating_mul(promoted.saturating_add(1))
        {
            return;
        }
        *promoted += 1;
        self.standby_parked.fetch_sub(1, Ordering::AcqRel);
        self.standby_promoted_total.fetch_add(1, Ordering::Relaxed);
        self.standby_wake.notify_all();
    }

    /// Park standby worker `k` (`1..`), spawned by the compiler start whose
    /// epoch is `epoch`, until it is promoted, the compiler shuts down, or a
    /// later start replaced its pool. Returns `false` unless promoted.
    fn park_standby(&self, shutdown: &AtomicBool, k: usize, epoch: usize) -> bool {
        let mut promoted = self.standby_promoted.lock();
        loop {
            // The epoch is written under this lock, so the reads here and
            // after every wake see the start that last held it.
            if shutdown.load(Ordering::Acquire)
                || self.standby_epoch.load(Ordering::Acquire) != epoch
            {
                return false;
            }
            if *promoted >= k {
                return true;
            }
            self.standby_wake.wait(&mut promoted);
        }
    }
}

/// zs s9/rs2: `CRATONVM_TIER_C2_STANDBY` -- opt-in (`1` / `on`); `0`, unset or
/// empty = off. With it, only the first C2 worker serves the queue from the
/// start; the others are spawned parked and each is promoted for good when an
/// enqueue leaves at least `CRATONVM_TIER_C2_STANDBY_DEPTH` (default 4, floor
/// 1) requests queued on the C2 lane. A parked worker has allocated nothing,
/// so it owns no allocator pages (the zs s6a host row `C2T1`: one C2 thread
/// instead of three cut boot RSS 7-13 MB at flat CPU).
pub fn c2_standby_depth_with(get: impl Fn(&str) -> Option<String>) -> usize {
    let on = get("CRATONVM_TIER_C2_STANDBY")
        .map(|v| cratonvm_types::flags::value_is_off(&v) || v.trim().is_empty())
        .map(|off| !off)
        .unwrap_or(false);
    if !on {
        return 0;
    }
    get("CRATONVM_TIER_C2_STANDBY_DEPTH")
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(4)
        .max(1)
}

/// [`c2_standby_depth_with`] over the process flags.
pub fn c2_standby_depth() -> usize {
    c2_standby_depth_with(|name| cratonvm_types::flags::runtime_var(name).ok())
}

/// zt t2/fp: `CRATONVM_TIER_C2_STANDBY_SCALE` -- opt-in (`1` / `on`); `0`,
/// unset or empty = off. Acts only with `CRATONVM_TIER_C2_STANDBY`: the
/// backlog that promotes the next standby worker is the depth times the
/// workers already live (first promotion at `depth`, the second at
/// `2 x depth`, ...), where without it every promotion waits for `depth`.
/// With `CRATONVM_TIER_C2_STANDBY_DEPTH=2` that is "add a C2 thread while the
/// queue holds at least two requests per live thread", the shape of HotSpot's
/// `UseDynamicNumberOfCompilerThreads` (by reading of `CompileBroker::
/// possibly_add_compiler_threads`, not quoted; HotSpot also weighs free
/// memory and code cache, and retires idle threads, which this does not).
pub fn c2_standby_scale_with(get: impl Fn(&str) -> Option<String>) -> bool {
    get("CRATONVM_TIER_C2_STANDBY_SCALE")
        .map(|v| !(cratonvm_types::flags::value_is_off(&v) || v.trim().is_empty()))
        .unwrap_or(false)
}

/// [`c2_standby_scale_with`] over the process flags.
pub fn c2_standby_scale() -> bool {
    c2_standby_scale_with(|name| cratonvm_types::flags::runtime_var(name).ok())
}

/// The `[jit] c2-standby:` line: the configured depth and scale, and the
/// standby workers `owner` (the VM's manager) has started and promoted so far;
/// 0 and 0 without an owner. Per VM since JIT r23 wave 11.
pub fn c2_standby_line(owner: Option<&TieredCompilationManager>) -> String {
    let (started, promoted) = owner.map_or((0, 0), |m| m.c2_standby_counts());
    format!(
        "[jit] c2-standby: depth={} scale={} started={started} promoted={promoted}",
        c2_standby_depth(),
        u8::from(c2_standby_scale()),
    )
}

/// `CRATONVM_JIT_CAP_REFUSAL_WITNESSED` (JIT round 22 wave 11, lane runtime;
/// behaviour, default ON, `=0` the old re-ask in `finish`): see
/// [`Completion::cap_refused`]. Read once per finished compile, not cached.
fn cap_refusal_witnessed_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_CAP_REFUSAL_WITNESSED")
}

/// zt t18/pl8: whether a finished attempt counts in `sun.ci.totalBailouts`:
/// it ran (no permanent decline), published nothing, and was not the
/// code-cache-cap deferral (or the waiting-on-class park) `finish` books
/// instead. Pure.
fn is_compile_bailout(c: &Completion, deferred_at_cap: bool) -> bool {
    !c.success && !c.declined_permanently && !deferred_at_cap
}

/// How one compile attempt ended, as [`CompilerCore::finish`] applies it.
#[derive(Debug, Clone, Copy)]
struct Completion {
    tier: CompilationTier,
    /// OSR bci of the request (`None` for method entry). With `tier`, the
    /// identity the in-flight slot is released against.
    osr_bci: Option<u32>,
    /// The request was for an OSR artifact.
    osr: bool,
    /// Wall-clock compile time, for the statistics.
    compile_time_ms: u64,
    /// zt t18/pl8: the same wall-clock time in nanoseconds, as the worker
    /// measured it around the compile callback (`complete_task`: the
    /// milliseconds scaled), for the perf-data `sun.ci.*` times.
    compile_time_ns: u64,
    /// What the C2 budget is charged: compile-thread CPU time where the
    /// platform reports it, wall time otherwise.
    budget_ms: u64,
    success: bool,
    declined_permanently: bool,
    /// The install epoch moved while this compile ran (the
    /// `inflight_epoch_moved` scheduling event). An unpublished outcome is
    /// then most likely `JitCache::put`/`put_osr` refusing a body stamped
    /// below the flush barrier -- the world moved, the compiler did not fail
    /// -- so [`CompilerCore::finish`] must not spend a retry on it, for
    /// exactly the reason [`CompilerCore::retire_stale`] does not.
    epoch_moved: bool,
    /// The compile declined because a class it needs is not loaded (or, for
    /// a static owner, not initialized) YET, and said so through
    /// [`note_compile_waiting_on_class`]: `Some(class-definition epoch read
    /// before the compile ran)`. [`CompilerCore::finish`] parks an
    /// unpublished method-entry completion of this kind instead of charging
    /// `tier_fail_count` ([`MethodState::waiting_on_class_epoch`]). `None`
    /// for every other completion, and whenever the VM installed no epoch
    /// source ([`TieredCompilationManager::set_class_epoch_source`]).
    /// Interpreter round i6 wave 116, lane L2 (page `i107-L2`).
    waiting_on_class: Option<u64>,
    /// JIT round 22 wave 11, lane runtime (`CRATONVM_JIT_CAP_REFUSAL_WITNESSED`,
    /// default ON, `=0` the old re-ask): the compile did not publish and the
    /// code-cache cap gate refused a compile while it ran (the process-wide
    /// cap-refusal count moved across the call). [`CompilerCore::finish`] then
    /// defers it at the cap instead of charging `tier_fail_count`, whatever
    /// the cap reads by the time `finish` asks. See the arm there.
    cap_refused: bool,
    /// t1 wave 11 lane JTIER (TIERMGR21-2, `CRATONVM_JIT_TIER_SLOT_TICKET`):
    /// the slot ticket of the request this completes ([`QueuedRequest::ticket`]);
    /// `0` for a completion with no ticket (the synchronous
    /// `compilation_complete` door, tests), which keeps the `(tier, bci)` match.
    ticket: u64,
}

thread_local! {
    /// Set by [`note_compile_waiting_on_class`] during a compile, read and
    /// cleared by the worker loop around its `compile_fn` call.
    ///
    /// JIT round 22 wave 12 (lane review): two bits, so the cap-refusal witness
    /// shares this cell instead of adding a static. [`COMPILE_WAITING_ON_CLASS_BIT`]
    /// is the original flag; [`COMPILE_CAP_REFUSED_BIT`] is set by the code-cache
    /// cap gate's refusal on THIS thread ([`note_compile_cap_refused_on_this_thread`]).
    static COMPILE_WAITING_ON_CLASS: std::cell::Cell<u8> = const { std::cell::Cell::new(0) };
}

/// [`COMPILE_WAITING_ON_CLASS`]'s waiting-on-class bit.
const COMPILE_WAITING_ON_CLASS_BIT: u8 = 1;
/// [`COMPILE_WAITING_ON_CLASS`]'s cap-refused bit (JIT round 22 wave 12).
const COMPILE_CAP_REFUSED_BIT: u8 = 2;

/// Called by the code-cache cap gate (`lib.rs` `note_jit_code_cache_cap_refusal`)
/// on the thread whose compile it refused. Only the worker loop reads it, and
/// it clears it before every compile; on an eager or inline compile's thread it
/// is inert, like [`note_compile_waiting_on_class`].
pub(crate) fn note_compile_cap_refused_on_this_thread() {
    COMPILE_WAITING_ON_CLASS.with(|c| c.set(c.get() | COMPILE_CAP_REFUSED_BIT));
}

fn take_compile_cap_refused() -> bool {
    COMPILE_WAITING_ON_CLASS.with(|c| {
        let v = c.get();
        c.set(v & !COMPILE_CAP_REFUSED_BIT);
        v & COMPILE_CAP_REFUSED_BIT != 0
    })
}

/// `CRATONVM_JIT_CAP_REFUSAL_WITNESS_PER_THREAD` (JIT round 22 wave 12, lane
/// review; correctness, default ON, `=0` the wave-11 process-wide sample): the
/// cap-refusal witness of [`Completion::cap_refused`] is a refusal on the
/// compiling thread itself, not a move of the process-wide
/// `jit_code_cache_cap_refusals()` across the call. That counter is shared by
/// every worker, every eager / OSR door on a mutator and every VM in the
/// process, so a compile that failed for a genuine codegen reason while ANY
/// other compile was refused at the cap read as cap-refused: deferred at the
/// cap, never charged to `tier_fail_count`, and offered again every time the
/// pressure cleared -- under a cache that stays near its cap, for as long as
/// the process runs. Read once per finished compile, not cached.
fn cap_refusal_witness_per_thread_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_CAP_REFUSAL_WITNESS_PER_THREAD")
}

/// Called by a compile that declines TRANSIENTLY because a class it needs is
/// not loaded or initialized yet (interpreter round i6 wave 116, lane L2:
/// the aarch64 block's `arm64_cold_field_site` bail under
/// `CRATONVM_JIT_ARM64_FIELD_SITE_TRANSIENT`). On a background worker the
/// completion is then parked until a class is defined
/// ([`MethodState::waiting_on_class_epoch`]) instead of being charged to the
/// method's compile-failure budget. On any other thread (an eager or inline
/// compile) it is inert: only the worker loop reads it, and it clears it
/// before every compile.
pub fn note_compile_waiting_on_class() {
    COMPILE_WAITING_ON_CLASS.with(|c| c.set(c.get() | COMPILE_WAITING_ON_CLASS_BIT));
}

fn take_compile_waiting_on_class() -> bool {
    COMPILE_WAITING_ON_CLASS.with(|c| {
        let v = c.get();
        c.set(v & !COMPILE_WAITING_ON_CLASS_BIT);
        v & COMPILE_WAITING_ON_CLASS_BIT != 0
    })
}

/// Unpublished waiting-on-class completions a method may park
/// ([`MethodState::waiting_on_class_parks`]) before they are charged like any
/// failed compile. Each park costs one compile that bailed early; the bound
/// keeps a method whose class never loads (or whose resolver answers `None`
/// for a reason that is not a missing class) from compiling for the life of
/// the process.
const MAX_WAITING_ON_CLASS_PARKS: u32 = 64;

/// Offers a parked method refuses while the class-definition epoch has not
/// moved, before one is let through anyway (uncharged, if it parks again).
/// A static owner that is LOADED but not yet initialized never moves the
/// definition epoch; this is how its method still gets compiled once the
/// owner's `<clinit>` has run. At the interpreter's stride of 64 calls per
/// offer, one attempt per ~1000 calls.
const WAITING_ON_CLASS_RELEASE_OFFERS: u32 = 16;

/// The VM's reader of its class-definition epoch (see
/// [`TieredCompilationManager::set_class_epoch_source`]).
pub type ClassEpochSource = Box<dyn Fn() -> u64 + Send + Sync + 'static>;

/// State the compile workers share with the manager.
///
/// The lanes, the "compiler is running" and shutdown flags and every per-method
/// verdict live here behind an [`Arc`] so the spawned workers can drain the
/// queues off the mutator thread without a back-reference to the whole
/// [`TieredCompilationManager`] (which is owned by `SharedVm` by value).
struct CompilerCore {
    /// Per-method compilation state.
    /// T10.9.B: FxHashMap — MethodKey (internal class/name/desc) is trusted.
    ///
    /// Lives here (rather than on the manager) so a worker, which only holds
    /// an `Arc<CompilerCore>`, can update a method's tier / queued flag on
    /// completion under the same lock the mutator-side API uses.
    methods: Mutex<FxHashMap<MethodKey, MethodState>>,
    /// The two compile lanes, indexed by [`C1_LANE`] / [`C2_LANE`].
    lanes: [CompileLane; 2],
    /// Whether this manager's workers are running.
    active: AtomicBool,
    /// Requests the workers to stop: each finishes any in-flight compile, then
    /// exits at its next queue check (remaining queued tasks are drained and
    /// counted by [`BackgroundCompiler::shutdown`]).
    shutdown: AtomicBool,
    /// Number of tasks the workers have finished compiling (for tests/diagnostics).
    completed: AtomicU64,
    /// Requests discarded instead of compiled — the local mirror of
    /// `crate::metrics::scheduling_dropped_total`, so a test can assert on
    /// one manager's behaviour without reading a process-wide table shared
    /// with every other manager and every sibling test.
    dropped: AtomicU64,
    /// Requests refused because the method already held the in-flight slot.
    deduplicated: AtomicU64,
    /// gcj pcp (`CRATONVM_JIT_COMPILE_YIELDS_TO_GC`): set while the owning
    /// VM runs a stop-the-world collection ([`TieredCompilationManager::
    /// gc_pause_scope`]). A worker finishes the compile it is in but starts no
    /// new one until it clears ([`CompilerCore::wait_out_gc_pause`]).
    gc_pause: Mutex<bool>,
    /// Signalled when `gc_pause` clears.
    gc_pause_end: Condvar,
    /// Compiles a worker held back until a pause ended (engagement).
    gc_pause_waits: AtomicU64,
    /// Requests refused by [`CompilerCore::admit`]'s eligibility gates: the
    /// method is `ineligible`, or its compiles have failed
    /// [`MAX_TIER_FAIL_RETRIES`] times. Expected to stay near zero for the same
    /// reason `deduplicated` is — the policy doors ask first, so this counts
    /// doors that did not (today: the VM's deopt re-queue).
    refused_ineligible: AtomicU64,
    /// Requests refused by [`CompilerCore::admit`] because the code cache was
    /// at its cap. See [`MethodState::deferred_at_cap`].
    refused_at_cap: AtomicU64,
    /// Unpublished method-entry completions parked waiting on a class
    /// instead of charged ([`Completion::waiting_on_class`]; wave 116, L2).
    parked_waiting_on_class: AtomicU64,
    /// Whether the code cache was at its cap the last time the VM reported
    /// it. See [`TieredCompilationManager::note_code_cache_pressure`].
    cap_pressure: std::sync::atomic::AtomicBool,
    /// Compiles whose `compile_fn` panicked and was contained.
    worker_panics: AtomicU64,
    /// Whether the first contained panic has been logged (later ones are only
    /// counted).
    panic_logged: AtomicBool,
    /// One slot per concurrently-running compile, holding `epoch + 1` (so `0`
    /// means "free" without reserving a real epoch value).
    ///
    /// # Why an array, and not the single word this used to be
    ///
    /// The dispatch-time gate can only refuse a request whose epoch had
    /// *already* moved. This records the epoch a running compile was
    /// dispatched at, so a worker can tell afterwards whether the world moved
    /// underneath it — the window that only the per-cache flush barrier in
    /// `JitCache::put` can close, and the one this record makes visible.
    ///
    /// It was one `AtomicU64`, written by `note_dispatch` as a plain store
    /// after the count bumped, and cleared by `note_dispatch_done` when the
    /// count fell to zero. **There is more than one worker**:
    /// `compiler_thread_counts` gives `1` C1 thread plus `log2(cpus)` C2
    /// threads, so four workers on an eight-core box. Two dispatching
    /// concurrently could land their stores in either order, leaving the word
    /// holding the *older* dispatch's epoch while the newer compile ran — and
    /// the reset-on-last-done rule meant a worker finishing out of order left
    /// a stale value standing. The word quiesced to `0` correctly and was
    /// wrong in between, which is the shape that survives every test that only
    /// looks at the idle state.
    ///
    /// Nothing gated an install on it — the accessor's only caller was a test
    /// — so this was a latent trap rather than a live defect. It is fixed
    /// rather than deleted because the question it answers ("is a compile
    /// running that was dispatched before the world moved?") is one a
    /// redefinition or a class-unload audit will want, and a diagnostic that
    /// is wrong under concurrency is worse than no diagnostic.
    ///
    /// With a slot per compile the answer is exact and the reader is a `min`:
    /// see [`CompilerCore::inflight_oldest_epoch`].
    inflight_epochs: Box<[AtomicU64]>,
    /// Compiles currently running across all workers.
    inflight_count: AtomicU64,
    /// Dispatches that found no free slot in [`Self::inflight_epochs`].
    ///
    /// Structurally impossible while the array is sized from the same clamp
    /// `compiler_thread_counts` applies, so a non-zero value means that
    /// invariant broke — reported rather than silently degrading the epoch
    /// record.
    inflight_slot_exhausted: AtomicU64,
    /// Generation that [`InvocationVerdict::settled_generation`] stamps are
    /// checked against. Starts at 1 so a zeroed stamp never matches.
    settled_generation: AtomicU32,
    /// Branch-profile windows opened by this manager minus those it closed.
    /// Zero whenever no C2 nomination is outstanding.
    branch_window_balance: AtomicI64,
    /// OSR denials, each stamped with the install epoch it was recorded at.
    /// A denial whose stamp is no longer current has expired: see
    /// [`TieredCompilationManager::is_osr_denied`].
    osr_denied: RwLock<FxHashMap<MethodKey, u64>>,
    /// `CRATONVM_JIT_OSR_OWN_FAIL_BUDGET` as this manager read it at
    /// construction ([`osr_out_of_retries`]; JIT r22 wave 3).
    osr_own_fail_budget: bool,
    /// `CRATONVM_DBG_TIER_ENQUEUE` as read at construction, for
    /// [`osr_out_of_retries`]' census line (JIT r22 wave 3).
    osr_budget_trace: bool,
    /// This VM's compile verdicts (bail list, refusal reasons, OSR entry
    /// rejects, recursion-cycle sets). Per manager, so per VM; see
    /// [`crate::JitVerdictRegistry`] and [`TieredCompilationManager::verdicts`].
    verdicts: crate::JitVerdictRegistry,
    /// When set, the source of "the current install epoch" instead of the
    /// process-wide [`crate::jit_install_epoch`]. OSR denials and queued
    /// requests are stamped with it and compared against it.
    ///
    /// The VM passes its own code-state epoch here (`JitRealm::code_state_epoch`,
    /// advanced by that VM's redefinitions and code-cache flushes; interpreter
    /// round i1 wave 14), so another VM's redefinition neither re-opens this
    /// VM's OSR denials nor drops its queued requests. `None` (no VM: backend
    /// tests, [`TieredCompilationManager::new`]) reads the process epoch.
    ///
    /// It is also the test seam for the scheduling rule "a request queued
    /// before an invalidation is not compiled after it", a statement about
    /// epoch *ordering*: driving it from an injected counter makes the test
    /// deterministic and hermetic, where the global would make it depend on
    /// whatever every other test in the process happened to flush.
    install_epoch_source: Option<Arc<AtomicU64>>,
    /// Invocation count the C1→C2 supersede requires, or `0` for "no gate" —
    /// the historical, and still default, behaviour.
    ///
    /// [`Self::request_c2_upgrade`] consulted no invocation count at all:
    /// **every** successful C1 publish enqueued a C2 recompile, gated only by
    /// the eligibility flags and by `c2_upgrade_would_engage`'s bytecode scan.
    /// That path produces nearly all of the C2 compiles in a real run, so C2
    /// entry is structural rather than hotness-driven — and
    /// `CRATONVM_TIER_C2_THRESHOLD` was **inert** exactly where it mattered:
    /// raising it to 2e9 on `ZipContentTests` still produced `c2=97` against a
    /// baseline `c2=95`. A knob that reads as "the C2 tier-up threshold" while
    /// governing only one of the two doors to that tier is worse than no knob,
    /// because it answers an A/B with the baseline twice and the reader cannot
    /// tell.
    ///
    /// Setting `CRATONVM_TIER_C2_THRESHOLD` now closes this door too, so the
    /// lever means what it says. It is deliberately NOT applied when the knob
    /// is unset: gating the supersede at the default 20 000 would change which
    /// methods reach C2 for every workload, and nothing measured here says that
    /// is an improvement — the JIT deficit this was found under turned out to be
    /// a leaked JMX owned-monitor set, not admission. The gate exists so the
    /// trade can be priced on the gauntlet, not so it can be flipped.
    ///
    /// Mirrored onto the core because the supersede runs on a worker, which
    /// holds only an `Arc<CompilerCore>` and cannot reach the manager's policy
    /// mutex. One relaxed load on a path that already takes `methods`.
    c2_upgrade_min_invocations: AtomicU64,
    /// The invocation count `should_compile`'s C1→C2 arm asks for, mirrored so
    /// [`Self::promote_hot_c1_to_c2`] can ask it from a worker — or `u64::MAX`
    /// when nothing may pass (tiering disabled).
    ///
    /// ## Why this exists: the C1→C2 arm was unreachable
    ///
    /// `should_compile`'s `C1 | C1WithProfiling` arm promotes a method to C2
    /// once `invocation_count` crosses `max(c2_threshold, c2_min_invocations)`.
    /// The only thing that ever *advances* `invocation_count`, and the only
    /// thing that ever *evaluates* that arm, is
    /// [`TieredCompilationManager::on_method_invocation_settling`] — and every
    /// caller of it in the VM sits on an interpreter path that is taken ONLY
    /// while the method has no published method-entry body:
    ///
    ///  * `dispatch_static.rs` probes `jit_cache` first and, on a hit, rewrites
    ///    the call site to `CachedInvokeTarget::Jit` and `return`s — the
    ///    counting block below it is not reached again;
    ///  * `dispatch_virtual.rs` (both sites) counts inside the `None` arm of
    ///    that same probe;
    ///  * `invoke_fast.rs`'s door declines outright when
    ///    `callee_has_compiled_body`;
    ///  * `execute_jit_call` — the path a compiled method is actually entered
    ///    through — counts nothing, and the x64 backend emits no invocation
    ///    counter in its prologue.
    ///
    /// `current_tier` only becomes `C1` when a C1 body was actually PUBLISHED
    /// (see [`Self::finish`]), which is precisely the event that stops every
    /// one of those call sites from asking again. So the arm could only ever be
    /// evaluated for a method whose C1 body had been thrown away again (a
    /// deopt, an invalidation, a redefinition) — never in steady state. The
    /// manager's `invocation_count` freezes at whatever the method had reached
    /// when its C1 body landed (measured: ~5 000 of 100 000 real calls), and no
    /// later call can ever push it over the C2 bar.
    ///
    /// The consequence is not a missing optimization on one method: it is that
    /// the hotness door into the optimizing tier is shut for the whole VM.
    /// `request_c2_upgrade` — the *structural* door, taken on every successful
    /// C1 publish — is what produced essentially all C2 compiles instead, and
    /// its own comment relies on the hotness door still existing ("A method
    /// held back here is not held back forever: `should_compile`'s own C2 arm
    /// admits it once it really is that hot"). It does not, which is why a
    /// method the structural door refuses — an allocation-bearing one, say,
    /// while `CRATONVM_JIT_C2_ALLOC_UPGRADE` is off by default — never reaches
    /// C2 however hot it gets, and why per-method escape analysis (which lives
    /// at C2) reports zero candidates on a probe built to produce them.
    ///
    /// The fix does not try to keep the count live in compiled code; that means
    /// a counter in the prologue of every JIT'd method and it is a different,
    /// much larger change. It asks the question ONCE, at the single moment the
    /// count is both fresh and final — the C1 publish itself.
    c2_hotness_bar: AtomicU64,
    /// Aggregate compilation statistics (shared so the workers can update them).
    stats: CompilationStats,
    /// Run by an idle C1-lane worker once per [`IDLE_TICK`]. See
    /// [`TieredCompilationManager::set_idle_tick`]. Unset, the workers wait
    /// untimed exactly as they always did. Cleared by
    /// [`BackgroundCompiler::shutdown`]: the VM's tick holds a `Weak` of the
    /// VM, and the core can outlive it (a worker detached by a shutdown that
    /// ran on that worker still holds its `Arc`). [`DIAG_CORE`] is weak since
    /// interpreter round i1 wave 15.
    idle_tick: Mutex<Option<Arc<dyn Fn() + Send + Sync + 'static>>>,
    /// j27 wave 1, lane TIER (`CRATONVM_JIT_ENTRY_TRIP`): set by an entry trip
    /// ([`CompilerCore::request_idle_tick`]); an idle C1-lane worker that
    /// finds it set runs the idle tick at once instead of waiting out
    /// [`IDLE_TICK`].
    idle_tick_requested: AtomicBool,
    /// j27 TIER: this manager's entry-trip sink, built on first use by
    /// [`TieredCompilationManager::entry_trip_sink`]. Its wake holds only a
    /// `Weak` of this core.
    entry_trip_sink: std::sync::OnceLock<Arc<crate::entry_counter::EntryTripSink>>,
    /// What [`Self::finish`] asks about a successful method-entry publish
    /// (tier proposal W4-1). See
    /// [`TieredCompilationManager::set_live_entry_probe`]. Unset, `finish`
    /// infers it from `current_tier` as before.
    live_entry_probe: std::sync::OnceLock<LiveEntryProbe>,
    /// Told the key of every admitted request (CONF21-1). See
    /// [`TieredCompilationManager::set_compile_admission_sink`]. Unset,
    /// nothing is told.
    compile_admission_sink: std::sync::OnceLock<CompileAdmissionSink>,
    /// The VM's class-definition epoch, for the waiting-on-class park
    /// ([`Completion::waiting_on_class`]); unset, nothing parks.
    class_epoch_source: std::sync::OnceLock<ClassEpochSource>,
    /// HANDLER8-1 (JIT round 19 wave 9, lane handler9): the outside-manager C1
    /// publish census and the opt-in supersede's per-VM budget. See
    /// [`OutsideManagerC2Census`].
    outside_c2: OutsideManagerC2Census,
    /// JIT round 21 lane tiermgr (INDYCONST21-1; re-landed t1 wave 8): the
    /// retry generation. Moved by every C2 retry answered `AlreadyQueued`
    /// (stamping [`MethodState::retry_owed`]) and read by a worker as it
    /// DISPATCHES a compile: a compile dispatched at generation `g` started
    /// after every request stamped `<= g`, so it read the state those requests
    /// were about. Per core, so per VM.
    retry_generation: AtomicU64,
    /// t1 wave 11 lane JTIER (TIERMGR21-2): the next in-flight slot ticket
    /// [`Self::admit`] draws (starts at 1; `0` means "no ticket"). Per core.
    next_slot_ticket: AtomicU64,
    /// `CRATONVM_JIT_TIER_SLOT_TICKET` as this manager read it at construction
    /// ([`tier_slot_ticket_enabled`]).
    slot_ticket: bool,
    /// j27 wave 6 lane T1TIERMGR (TIERMGR21-4): the gate-refused bodies
    /// waiting to be timed, and the VM's timer. Per core, so per VM.
    accept_timing: accept_timing::AcceptTiming,
}

/// The default of `CRATONVM_JIT_OUTSIDE_MANAGER_C2_UPGRADE_BUDGET`: how many
/// Low-priority C2 supersedes [`CompilerCore::request_outside_manager_c2_upgrade`]
/// may ADMIT per VM (HANDLER8-1).
///
/// Why a budget and not a hotness floor: the population this door exists for
/// is exactly the eager-callee kernels that never reach the manager's own C1
/// bar (500 calls) and whose loops never cross the HANDLER7-1 back-edge door
/// (40 000 back edges); any floor high enough to bound a storm by hotness
/// re-creates one of those two doors and serves nobody new. A count bound
/// caps the worst case absolutely (at most this many extra C2 compiles in the
/// VM's life, on the optimizing lane at `Low` priority, behind every
/// manager-scheduled `High` C2 request), whatever the eager chain's fan-out.
/// 128 is about twice the candidates a JIT probe produces and a small
/// fraction of a javac run's C1 publishes; the census's `candidates=` on the
/// batteries says whether it binds (`budget-refused=` > 0).
pub const OUTSIDE_MANAGER_C2_UPGRADE_DEFAULT_BUDGET: u64 = 128;

/// HANDLER8-1 (JIT round 19 wave 9, lane handler9): what one core counts about
/// C1 bodies published by a door the manager did not schedule (the eager
/// callee chain, the synchronous callee compile, the optimizing doors' callee
/// compiler; `TieredCompilationManager::note_published_outside_manager`).
///
/// A manager-scheduled C1 publish of an IR-eligible method gets an immediate,
/// ungated `Low` C2 (`request_c2_upgrade`); an outside one gets none and must
/// earn C2 through the drained weight. The census answers "how many outside
/// C1 publishes would the manager path have superseded at once
/// (`candidates`), and how many of those reached the C2 bar anyway
/// (`crossed-bar`), after how long at C1 (`crossed-ms-*`)". `never-crossed`
/// is the population the opt-in supersede
/// (`CRATONVM_JIT_OUTSIDE_MANAGER_C2_UPGRADE`) would give C2 that nothing else
/// would; `crossed-ms-total` is the C1 time the supersede would save the rest.
///
/// Per core, so per VM (no process global). Printed at exit under
/// `CRATONVM_DBG_OUTSIDE_MANAGER_C2_CENSUS` (`CRATONVM_DBG=outside-manager-c2-census`)
/// only; the armed supersede alone counts but does not print (JIT r23 triage).
#[derive(Debug)]
struct OutsideManagerC2Census {
    /// Every C1-family outside publish recorded (any method, any number of
    /// times per method).
    c1_publishes: AtomicU64,
    /// Methods whose outside C1 publish passed `c2_upgrade_would_engage`
    /// (counted once per method; evaluated by the VM only when the census or
    /// the supersede is armed).
    candidates: AtomicU64,
    /// Of `candidates`, the ones whose manager count was already at or above
    /// the C2 bar when the C1 body published.
    over_bar_at_publish: AtomicU64,
    /// Of `candidates`, the ones whose manager count crossed the C2 bar
    /// AFTER their outside C1 publish (the drain's weight, or the interpreter's
    /// stride count).
    crossed_bar: AtomicU64,
    /// Sum and max of (bar crossing - outside C1 publish) uptime, in ms.
    crossed_ms_total: AtomicU64,
    crossed_ms_max: AtomicU64,
    /// Supersede requests admitted by the opt-in door.
    upgrades_admitted: AtomicU64,
    /// Supersede requests the door's own gates refused (queued, already C2,
    /// bailed out, ineligible, a raised `CRATONVM_TIER_C2_THRESHOLD` gate,
    /// tiering disabled).
    upgrades_refused: AtomicU64,
    /// Supersede requests refused because the budget was spent.
    budget_refused: AtomicU64,
    /// Supersede requests refused because the method's interpreter profile was
    /// thinner than the manager path's (wave-9 follow-up; see
    /// `note_published_outside_manager_with_c2_candidate`'s `profile_warm`).
    profile_refused: AtomicU64,
    /// HANDLER9-2 (JIT round 19 wave 11, lane handler11): C2 publications
    /// that replaced a C1 body of a census candidate (any door: the supersede,
    /// the drain's bar crossing, HANDLER7-1), as the VM's C2 publish reports
    /// them ([`TieredCompilationManager::note_c2_supersede_of_outside_candidate`]).
    c2_published: AtomicU64,
    /// ... of which the superseded C1 body was made not entrant, its stub
    /// forwarding to the C2 body (hm5): every caller that baked a direct call
    /// to the C1 body reaches the C2 body from its next call.
    c1_forwarded: AtomicU64,
    /// ... of which the C1 body stayed entrant (the pair's stub could not
    /// forward, or `CRATONVM_JIT_SUPERSEDE_NOT_ENTRANT=0`): its baked callers
    /// keep running C1, so for those callers the supersede is dead.
    c1_kept_entrant: AtomicU64,
    /// HANDLER11-6 (JIT round 19 wave 12, lane handler12): supersede requests
    /// withheld because the C1 body predicts a forward the C2 publish could
    /// not install (`CRATONVM_JIT_OUTSIDE_MANAGER_C2_UPGRADE_NEEDS_FORWARD`).
    forward_refused: AtomicU64,
    /// The VM's budget, read once at construction
    /// (`CRATONVM_JIT_OUTSIDE_MANAGER_C2_UPGRADE_BUDGET`, default
    /// [`OUTSIDE_MANAGER_C2_UPGRADE_DEFAULT_BUDGET`]).
    budget: u64,
    /// Budget left: taken before an attempt, refunded if not admitted.
    budget_left: AtomicU64,
    /// The exit line was printed (by the exit hook, or by the core's drop).
    printed: AtomicBool,
}

impl OutsideManagerC2Census {
    fn new() -> Self {
        let budget =
            cratonvm_types::flags::runtime_var("CRATONVM_JIT_OUTSIDE_MANAGER_C2_UPGRADE_BUDGET")
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
                .unwrap_or(OUTSIDE_MANAGER_C2_UPGRADE_DEFAULT_BUDGET);
        Self {
            c1_publishes: AtomicU64::new(0),
            candidates: AtomicU64::new(0),
            over_bar_at_publish: AtomicU64::new(0),
            crossed_bar: AtomicU64::new(0),
            crossed_ms_total: AtomicU64::new(0),
            crossed_ms_max: AtomicU64::new(0),
            upgrades_admitted: AtomicU64::new(0),
            upgrades_refused: AtomicU64::new(0),
            budget_refused: AtomicU64::new(0),
            profile_refused: AtomicU64::new(0),
            c2_published: AtomicU64::new(0),
            c1_forwarded: AtomicU64::new(0),
            c1_kept_entrant: AtomicU64::new(0),
            forward_refused: AtomicU64::new(0),
            budget,
            budget_left: AtomicU64::new(budget),
            printed: AtomicBool::new(false),
        }
    }

    /// Take one unit of budget; `false` when none is left.
    fn take_budget(&self) -> bool {
        self.budget_left
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |left| {
                left.checked_sub(1)
            })
            .is_ok()
    }

    fn refund_budget(&self) {
        self.budget_left.fetch_add(1, Ordering::Relaxed);
    }

    /// The exit line.
    fn line(&self) -> String {
        let load = |c: &AtomicU64| c.load(Ordering::Relaxed);
        let candidates = load(&self.candidates);
        let over = load(&self.over_bar_at_publish);
        let crossed = load(&self.crossed_bar);
        format!(
            "[cratonvm-tier] outside-manager C2 census: c1-publishes={} candidates={} \
             over-bar-at-publish={} crossed-bar={} never-crossed={} crossed-ms-total={} \
             crossed-ms-max={} | supersede={} admitted={} refused={} budget-refused={} budget={} \
             profile-refused={} forward-refused={} | c2-published={} c1-forwarded={} \
             c1-kept-entrant={}",
            load(&self.c1_publishes),
            candidates,
            over,
            crossed,
            candidates.saturating_sub(over).saturating_sub(crossed),
            load(&self.crossed_ms_total),
            load(&self.crossed_ms_max),
            if outside_manager_c2_upgrade_enabled() {
                "on"
            } else {
                "off"
            },
            load(&self.upgrades_admitted),
            load(&self.upgrades_refused),
            load(&self.budget_refused),
            self.budget,
            load(&self.profile_refused),
            load(&self.forward_refused),
            load(&self.c2_published),
            load(&self.c1_forwarded),
            load(&self.c1_kept_entrant),
        )
    }

    /// Print the exit line once, when the census is armed.
    ///
    /// JIT r23 triage (`docs/feature-designs/jit-ir-optimizations.md`):
    /// only the DBG switch prints. The supersede switch alone used to print it too,
    /// so flipping `CRATONVM_JIT_OUTSIDE_MANAGER_C2_UPGRADE` to default ON would
    /// have added this line to every run's stderr. The supersede still COUNTS
    /// (its own gates), it just does not print.
    fn print_once(&self) {
        if !outside_manager_c2_census_enabled() {
            return;
        }
        if self.printed.swap(true, Ordering::AcqRel) {
            return;
        }
        eprintln!("{}", self.line());
    }
}

/// `CRATONVM_DBG_OUTSIDE_MANAGER_C2_CENSUS` (HANDLER8-1 census, diagnostic;
/// default OFF): the VM evaluates `c2_upgrade_would_engage` on every outside
/// C1 publish and the core prints [`OutsideManagerC2Census`] at exit.
pub fn outside_manager_c2_census_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_OUTSIDE_MANAGER_C2_CENSUS")
}

/// `CRATONVM_JIT_OUTSIDE_MANAGER_C2_UPGRADE` (HANDLER8-1, JIT round 19 wave 9,
/// lane handler9; opt-in, default OFF): an outside-manager C1 publish of a
/// method that passes `c2_upgrade_would_engage` gets the manager path's
/// immediate `Low` C2 supersede, bounded by a per-VM budget
/// ([`OUTSIDE_MANAGER_C2_UPGRADE_DEFAULT_BUDGET`]).
pub fn outside_manager_c2_upgrade_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_OUTSIDE_MANAGER_C2_UPGRADE")
}

/// Print the first core's HANDLER8-1 census line at exit (the launcher's
/// shutdown hook; a no-op unless armed, and once per core: a core dropped
/// before this printed its own line from `Drop`).
pub fn print_outside_manager_c2_census_at_exit() {
    if let Some(core) = DIAG_CORE.get().and_then(|d| d.core.upgrade()) {
        core.outside_c2.print_once();
    }
}

/// The invocation count the C1→C2 supersede should require, or `0` for "no
/// gate" (the default, and what every build did before this existed).
///
/// # The number is [`c2_hotness_bar`]'s, not `c2_threshold` alone
///
/// This returned `policy.c2_threshold` while its own doc claimed "every other
/// door to C2 asks for `invocation_count >= c2_threshold`". Every other door
/// actually asks for `max(c2_threshold, c2_min_invocations)` —
/// `should_compile`'s two C2 arms spell out both conjuncts, and
/// [`c2_hotness_bar`] exists to fold exactly that pair into one number. So an
/// operator who set `CRATONVM_TIER_C2_THRESHOLD` **below**
/// `c2_min_invocations` (1 000 by default) armed this door at the lower number
/// and every other door at the higher one: one knob, four doors, two different
/// bars, and the door that disagreed is the one
/// [`CompilerCore::request_c2_upgrade`]'s own comment calls "the path that
/// produces nearly all C2 compiles". Identical for every value at or above
/// `c2_min_invocations`, which includes the default 20 000 and every raise.
///
/// Returns non-zero only when the operator actually asked, which is the whole
/// point: see [`CompilerCore::c2_upgrade_min_invocations`]. The env var is read
/// here rather than inferred from `policy.c2_threshold`, because a policy that
/// happens to equal the default is indistinguishable from one nobody set — and
/// "the operator set this knob" is exactly the distinction being made.
///
/// "Set" means set to a value [`CompilationPolicy::with_overrides`] actually
/// APPLIES -- one that parses as a `u32`. It used to mean merely present, so
/// `CRATONVM_TIER_C2_THRESHOLD=` (empty, what a shell script produces from an
/// unset variable it meant to substitute) or a typo left the policy at its
/// default 20 000 *and* armed this gate at that 20 000, which closes the
/// supersede door to nearly every method: the count it is compared against
/// froze at the C1 publish, typically a few thousand. An ignored knob must be
/// ignored by both of its readers.
fn c2_upgrade_gate(policy: &CompilationPolicy) -> u64 {
    if c2_threshold_knob_applies(
        cratonvm_types::flags::runtime_var("CRATONVM_TIER_C2_THRESHOLD")
            .ok()
            .as_deref(),
    ) {
        c2_hotness_bar(policy)
    } else {
        0
    }
}

/// Whether a `CRATONVM_TIER_C2_THRESHOLD` value is one the policy applies --
/// the exact parse [`CompilationPolicy::with_overrides`] uses. Split out so
/// the rule is testable without the process environment.
fn c2_threshold_knob_applies(value: Option<&str>) -> bool {
    value.is_some_and(|v| v.trim().parse::<u32>().is_ok())
}

/// The invocation count `should_compile`'s C1→C2 arm requires under `policy`,
/// or `u64::MAX` when that arm can never fire.
///
/// Exactly the arm's own predicate, folded into one number: it asks for
/// `invocation_count >= c2_threshold` **and** `>= c2_min_invocations`, which is
/// `>= max(c2_threshold, c2_min_invocations)`. Folding it here rather than
/// mirroring both fields keeps the promotion a single relaxed load, and keeps
/// there being ONE place where the two policies could drift apart.
///
/// `tiered_enabled == false` maps to `u64::MAX` rather than to a flag, because
/// that is exactly what the invocation hook does with it — it returns before
/// `should_compile` is ever consulted — and a saturated bar cannot be crossed
/// by a `u64` count.
/// HotSpot's `Tier4MinInvocationThreshold` ([`tier4_i_plus_b_due`]).
pub(crate) const TIER4_I_PLUS_B_MIN_INVOCATIONS: u64 = 600;

/// HotSpot's `Tier4CompileThreshold` ([`tier4_i_plus_b_due`]).
pub(crate) const TIER4_I_PLUS_B_COMPILE_THRESHOLD: u64 = 15_000;

/// `CRATONVM_JIT_TIER4_I_PLUS_B` (JIT round 19 wave 12, lane mon12, MON11-4 of
/// `docs/feature-designs/jit-ir-optimizations.md`; **default ON** since the
/// zr7ca residual close, 2026-10-25; `=0` kill): the C1 arm of `should_compile` also promotes on
/// HotSpot's Tier4 call predicate ([`tier4_i_plus_b_due`]). Read per decision (no memo, no new
/// static). `tools/probes/jit/r21/Z7KernelPromote.java`: a 32-iteration allocating kernel called
/// from an OSR body crossed the 8:1-weighted 20 000 bar at ~45 000 calls and ran its single-pass
/// body (1 300 ns a call) until then; with the predicate it is C2 (260 ns, HotSpot 25: ~290) by
/// its second thousand calls.
fn tier4_i_plus_b_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_TIER4_I_PLUS_B")
}

/// HotSpot's Tier4 call predicate on its back-edge half
/// (`CompilationPolicy::call_predicate_helper`, `CompLevel_full_profile`):
/// `i >= Tier4MinInvocationThreshold && i + b >= Tier4CompileThreshold`.
///
/// `invocation_count` is the manager's count, which the entry-counter drain
/// moves to the body's WEIGHTED count (entries plus `b >> 3`,
/// `entry_counter::weighted_count`); `drained_back_edges` is the raw back-edge
/// word that drain read (`MethodState::last_drained_back_edges`). `i` takes
/// the 8:1 share back off, so `b` is not counted twice. (Under
/// `CRATONVM_JIT_C1_BACK_EDGE_C2_BAR` the drained count may be `b >> 1`, and
/// `i` is then an underestimate: the predicate only fires later.)
///
/// What it adds over the 8:1 weight: a C1 body of a SHORT loop called often.
/// 20 back edges a call cross the 20 000 bar through `i + b/8` at ~5 700
/// calls, and HotSpot's predicate at ~715 (`715 + 14 300 >= 15 000`). For long
/// loops the 8:1 weight is already the earlier door.
pub(crate) fn tier4_i_plus_b_due(invocation_count: u64, drained_back_edges: u64) -> bool {
    let entries = invocation_count.saturating_sub(drained_back_edges >> 3);
    entries >= TIER4_I_PLUS_B_MIN_INVOCATIONS
        && entries.saturating_add(drained_back_edges) >= TIER4_I_PLUS_B_COMPILE_THRESHOLD
}

/// `CRATONVM_JIT_C2_AFTER_OPTIMIZING_OSR` (JIT r23 wave 11, lane L1; **default
/// ON** since the w2c/churn close, 2026-10-24; `=0` kill): a published
/// optimizing OSR body promotes the method's single-pass entry body to C2
/// ([`CompilerCore::promote_c1_after_optimizing_osr`]).
/// `Zt4ChChurnShapes method` / `tools/probes/jit/r23/ChurnCtor.java`: `round`
/// gets its C2 OSR body in its first call, but its single-pass entry body
/// (which calls `Node.<init>`, the splice walk rolls back at the
/// constructor's `newarray`) runs every later call until the drain clock
/// promotes it ~60-120 calls later. Linux host, `ChurnCtor 500 1`, main
/// thread CPU: 90-149 ms off, 17-19 ms on (HotSpot 25: 25). Read per
/// completion.
fn c2_after_optimizing_osr_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_C2_AFTER_OPTIMIZING_OSR")
}

fn c2_hotness_bar(policy: &CompilationPolicy) -> u64 {
    if !policy.tiered_enabled {
        return u64::MAX;
    }
    u64::from(policy.c2_threshold).max(u64::from(policy.c2_min_invocations))
}

impl CompilerCore {
    fn with_install_epoch_source(install_epoch_source: Option<Arc<AtomicU64>>) -> Self {
        Self::with_epoch_sources(install_epoch_source, None)
    }

    /// `redefine_epoch_source`: the owning VM's redefine epoch, which its
    /// verdict registry then shares (interpreter round i1 wave 17); it is
    /// read only together with a code-state epoch, since a VM hands in both.
    fn with_epoch_sources(
        install_epoch_source: Option<Arc<AtomicU64>>,
        redefine_epoch_source: Option<Arc<AtomicU32>>,
    ) -> Self {
        Self {
            methods: Mutex::new(FxHashMap::default()),
            lanes: [CompileLane::new(), CompileLane::new()],
            active: AtomicBool::new(false),
            shutdown: AtomicBool::new(false),
            completed: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            deduplicated: AtomicU64::new(0),
            gc_pause: Mutex::new(false),
            gc_pause_end: Condvar::new(),
            gc_pause_waits: AtomicU64::new(0),
            refused_ineligible: AtomicU64::new(0),
            refused_at_cap: AtomicU64::new(0),
            parked_waiting_on_class: AtomicU64::new(0),
            cap_pressure: std::sync::atomic::AtomicBool::new(false),
            worker_panics: AtomicU64::new(0),
            panic_logged: AtomicBool::new(false),
            inflight_epochs: (0..INFLIGHT_EPOCH_SLOTS)
                .map(|_| AtomicU64::new(0))
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            inflight_count: AtomicU64::new(0),
            inflight_slot_exhausted: AtomicU64::new(0),
            settled_generation: AtomicU32::new(1),
            branch_window_balance: AtomicI64::new(0),
            osr_denied: RwLock::new(FxHashMap::default()),
            // JIT r22 wave 3 (item 3 of `r22-review-w3-intrinsics-on-inline-FIXED-20261020.md`):
            // read once per manager, never under the `methods` mutex.
            osr_own_fail_budget: osr_own_fail_budget_enabled(),
            osr_budget_trace: cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_TIER_ENQUEUE"),
            // The VM's code-state epoch is its verdicts' flush epoch too: one
            // counter, which the VM advances (interpreter round i1 wave 15);
            // and its redefine epoch their redefine epoch (wave 17).
            verdicts: match (&install_epoch_source, redefine_epoch_source) {
                (Some(epoch), Some(redefine)) => {
                    crate::JitVerdictRegistry::with_vm_epochs(Arc::clone(epoch), redefine)
                }
                (Some(epoch), None) => {
                    crate::JitVerdictRegistry::with_flush_epoch(Arc::clone(epoch))
                }
                (None, _) => crate::JitVerdictRegistry::default(),
            },
            install_epoch_source,
            // 0 = no gate. Raised by the manager's constructor and by
            // `set_policy` only when the operator set the knob explicitly.
            c2_upgrade_min_invocations: AtomicU64::new(0),
            // `u64::MAX` = "no method may take the hotness door", which is what
            // every build before `promote_hot_c1_to_c2` existed did. The
            // manager's constructor and `set_policy` mirror the real bar onto
            // it immediately; a core that somehow never gets one behaves
            // exactly as it did before.
            c2_hotness_bar: AtomicU64::new(u64::MAX),
            stats: CompilationStats::default(),
            idle_tick: Mutex::new(None),
            idle_tick_requested: AtomicBool::new(false),
            entry_trip_sink: std::sync::OnceLock::new(),
            live_entry_probe: std::sync::OnceLock::new(),
            compile_admission_sink: std::sync::OnceLock::new(),
            class_epoch_source: std::sync::OnceLock::new(),
            outside_c2: OutsideManagerC2Census::new(),
            retry_generation: AtomicU64::new(0),
            next_slot_ticket: AtomicU64::new(1),
            slot_ticket: tier_slot_ticket_enabled(),
            accept_timing: accept_timing::AcceptTiming::default(),
        }
    }

    /// The install epoch as of right now: the owning VM's code-state epoch
    /// when one was handed in, else the process epoch. One atomic load either
    /// way.
    fn class_definition_epoch(&self) -> Option<u64> {
        self.class_epoch_source.get().map(|source| source())
    }

    /// Does the waiting-on-class park ([`MethodState::waiting_on_class_epoch`])
    /// refuse this method-entry offer? Clears the park once the VM's
    /// class-definition epoch has moved, and lets every
    /// [`WAITING_ON_CLASS_RELEASE_OFFERS`]-th refused offer through. Called
    /// under `methods`; the source is one atomic load on the VM side.
    fn waiting_on_class_refuses(&self, state: &mut MethodState) -> bool {
        let Some(parked) = state.waiting_on_class_epoch else {
            return false;
        };
        match self.class_definition_epoch() {
            Some(now) if now == parked => {}
            // The epoch moved (or the source is gone): unpark.
            _ => {
                state.waiting_on_class_epoch = None;
                state.waiting_on_class_offers = 0;
                return false;
            }
        }
        state.waiting_on_class_offers = state.waiting_on_class_offers.saturating_add(1);
        if state.waiting_on_class_offers >= WAITING_ON_CLASS_RELEASE_OFFERS {
            state.waiting_on_class_offers = 0;
            return false;
        }
        true
    }

    fn current_install_epoch(&self) -> u64 {
        match &self.install_epoch_source {
            Some(cell) => cell.load(Ordering::Acquire),
            None => crate::jit_install_epoch(),
        }
    }

    /// Whether `CRATONVM_TIER_ENABLED=0` is in force, as a worker can ask it.
    ///
    /// A worker holds only an `Arc<CompilerCore>` and cannot reach the
    /// manager's policy mutex, so the kill switch reaches it through
    /// [`Self::c2_hotness_bar`]: [`c2_hotness_bar`] answers `u64::MAX` for a
    /// disabled policy and `max(c2_threshold, c2_min_invocations)` — two `u32`s,
    /// so at most `u32::MAX` — otherwise. The sentinel is therefore exact, not
    /// a number that a configuration could also produce.
    ///
    /// The doors that need it are [`Self::request_c2_upgrade`] and
    /// [`Self::request_deferred_new_retry`]. Both are reached from the worker
    /// loop on a published body, not from the invocation hook, so neither ever
    /// passed the hook's `!policy.tiered_enabled` early return: with tiering
    /// off, a C1 body published through a door that bypasses the manager (the
    /// VM's eager deopt re-queue, `enqueue_compilation`, which asks no policy)
    /// still enqueued a C2 supersede. `request_osr` asks the policy directly
    /// and `promote_hot_c1_to_c2` already reads this same bar, so those two
    /// were closed; these two were the hole.
    fn tiering_is_disabled(&self) -> bool {
        self.c2_hotness_bar.load(Ordering::Relaxed) == u64::MAX
    }

    /// Ask an idle C1-lane worker to run the idle tick now (j27 wave 1, lane
    /// TIER; an entry trip, `CRATONVM_JIT_ENTRY_TRIP`). Called on a mutator,
    /// from a compiled prologue's helper: no allocation, no safepoint, only
    /// the lane's short-held queue lock.
    ///
    /// The flag is set first and the queue lock taken before the notify, so a
    /// worker that is between its flag test and its `wait_for` (both under
    /// that lock) cannot miss the wake: it either sees the flag, or is parked
    /// by the time the notify runs. A busy lane picks the flag up at its next
    /// idle wait; its compiles drain the counters anyway (the second clock).
    fn request_idle_tick(&self) {
        self.idle_tick_requested.store(true, Ordering::Release);
        let lane = &self.lanes[C1_LANE];
        drop(lane.queue.lock());
        lane.wake.notify_one();
    }

    /// Push a task onto its lane, stamped with the install epoch it was queued
    /// at, and wake one of that lane's workers.
    ///
    /// Every enqueue funnels through here (via [`Self::admit`]), so no door
    /// into either queue can produce an unstamped request.
    fn enqueue(&self, task: CompilationTask, ticket: u64) {
        // CONF21-1: HotSpot resolves String constants when a compile is
        // requested. An optimizing method-entry request and every OSR request
        // (the worker builds the optimizing OSR body first) count as C2's. The
        // sink only records the key: this runs under `methods`.
        if let Some(sink) = self.compile_admission_sink.get() {
            sink(
                &task.method_key,
                task.target_tier == CompilationTier::C2 || task.osr_bci.is_some(),
            );
        }
        let epoch = self.current_install_epoch();
        let lane = &self.lanes[lane_of(&task)];
        let depth = {
            let mut q = lane.queue.lock();
            q.enqueue_ticketed(task, epoch, ticket);
            q.len()
        };
        lane.wake.notify_one();
        // zs s9/rs2: a backlog wakes a standby worker (no-op by default).
        lane.maybe_promote_standby(depth);
    }

    /// Give `task` the method's in-flight slot and queue it, or refuse it.
    /// Returns whether it was queued. A method has two slots: one for a
    /// method-entry request and one for an OSR request (`task.osr_bci`), so
    /// "one compile per method" means one per artifact kind.
    ///
    /// Call with `methods` held: this takes a lane's queue lock, which is the
    /// established `methods` → `queue` order.
    ///
    /// **One slot per method**, whatever the tier or the bci. With a single
    /// compile thread a second request for a method only cost a wasted
    /// compile. With a worker per lane it is the same method compiled
    /// concurrently into two code buffers, the duplicate-install shape this VM
    /// has already paid for in code reclamation. `enqueue_compilation` -- the
    /// VM's deopt re-queue door -- used to push unconditionally and was the
    /// concrete way a method acquired two tasks. The policy doors check the
    /// flag themselves before building a task, so a refusal here is counted:
    /// it only happens on a door that did not.
    ///
    /// **The eligibility gates live here too, and that is the point.**
    /// `should_compile`, `request_osr` and `request_c2_upgrade` each refuse an
    /// `ineligible` method and one past [`MAX_TIER_FAIL_RETRIES`] before
    /// building a task. `enqueue_compilation` -- the eager re-queue door
    /// `DeoptimizationController::deoptimize` uses -- checked only the
    /// in-flight slot, so a method whose compile keeps failing could be
    /// re-enqueued indefinitely by the deopt path: trap, re-queue, fail,
    /// release the slot, trap again. Asking here instead of at each door
    /// removes the whole class rather than the one instance of it, and the
    /// doors' own early checks are kept because they also avoid *building* a
    /// task (and, for `request_c2_upgrade`, opening a branch-profile window)
    /// that this would only throw away.
    fn admit(&self, state: &mut MethodState, task: CompilationTask) -> bool {
        // Two slots per method: one method-entry request and one OSR request
        // may be in flight together -- see `MethodState::queued_for_compilation`.
        let osr = task.osr_bci.is_some();
        let slot_held = if osr {
            state.osr_in_flight
        } else {
            state.queued_for_compilation
        };
        if slot_held {
            self.deduplicated.fetch_add(1, Ordering::Relaxed);
            crate::metrics::record_scheduling_event(crate::metrics::SCHEDULING_EVENTS[4]);
            return false;
        }
        // The retry budget is per PIPELINE. A method-entry request asks only
        // `tier_fail_count`; an OSR request asks its own budget AND
        // `tier_fail_count`, which is the coupling that existed before the two
        // were split (see `MethodState::osr_fail_count`) -- what the split
        // removes is a successful OSR publish resetting the method-entry
        // budget, which made that budget unable to saturate.
        // JIT r22 (lane inline): the OSR arm is `osr_out_of_retries`, which
        // drops the `tier_fail_count` coupling under
        // `CRATONVM_JIT_OSR_OWN_FAIL_BUDGET`.
        let out_of_retries = if osr {
            osr_out_of_retries(state, self.osr_own_fail_budget, self.osr_budget_trace)
        } else {
            state.entry_out_of_retries()
        };
        if state.ineligible || (!osr && state.entry_ineligible) || out_of_retries {
            // A caller that opened a branch-profile window for THIS request is
            // owed its close, exactly as on the cap arm below. Safe to do here
            // and not on the `slot_held` arm above: the flag is set immediately
            // before this call and is cleared by `release_slot_for`, so with
            // the slot NOT held an armed window can only be this request's,
            // whereas with the slot held it may be the holder's. Inert for
            // every door today -- all three window-opening doors ask these two
            // gates before they open one -- and it stops the next door that
            // does not from pinning branch profiling on for the process.
            if !osr && std::mem::take(&mut state.branch_window_armed) {
                self.close_branch_window();
            }
            self.refused_ineligible.fetch_add(1, Ordering::Relaxed);
            // The process-wide mirror, so this arm reaches `metrics::summary()`
            // and its JSON the way the deduplication arm above already does.
            // Without it `queue_refused_ineligible` sits at zero forever and a
            // reader cannot distinguish "no method was ever refused" from "the
            // counter was never wired" — which is the failure mode a metric
            // exists to rule out, and the reason the row's own doc comment in
            // `metrics.rs` said it had no producer.
            //
            // A REFUSAL, not a drop: nothing was queued, so it is correctly
            // absent from `scheduling_dropped_total` (which sums indices 0, 1,
            // 3 and 5), exactly like the `queue_deduplicated` arm above.
            //
            // The index is pinned by
            // `metrics::tests::the_ineligible_refusal_row_exists_at_the_index_the_scheduler_records_by`,
            // so the constant cannot move out from under this line.
            crate::metrics::record_scheduling_event(crate::metrics::SCHEDULING_EVENTS[7]);
            return false;
        }
        // Interpreter round i6 wave 116, lane L2 (page `i107-L2`): a method
        // whose last method-entry compile was waiting on a class is not asked
        // again until a class has been defined, or until the release stride
        // lets one attempt through. Nothing is charged, nothing is queued.
        if !osr && self.waiting_on_class_refuses(state) {
            if std::mem::take(&mut state.branch_window_armed) {
                self.close_branch_window();
            }
            return false;
        }
        // The code-cache cap, asked HERE rather than left to the worker's
        // `compile_gate::admit`. There it came back as an unpublished compile,
        // `finish` charged it to `tier_fail_count` like broken codegen, and
        // three of them settled the method permanently -- a transient refusal
        // made sticky. See `MethodState::deferred_at_cap`.
        //
        // `jit_code_cache_at_capacity`, not the cheaper
        // `jit_code_cache_under_pressure`: it drains the retirement queue
        // before it answers, and a sweep's withdrawals sit in that queue.
        // Without the drain a method re-offered after a sweep would find the
        // bytes still committed and be deferred again. It runs under
        // `methods`; the drain takes the thread registry, the retirement
        // queue and the arena, none of which ever takes `methods`.
        if crate::jit_code_cache_at_capacity() {
            state.deferred_at_cap = true;
            // A caller that opened a branch-profile window for this request
            // is owed its close: nothing was queued to close it later. Only a
            // method-entry request opens one; an OSR request refused here
            // must not close the window of a method-entry request that holds
            // the other slot.
            if !osr && std::mem::take(&mut state.branch_window_armed) {
                self.close_branch_window();
            }
            self.refused_at_cap.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        state.deferred_at_cap = false;
        // t1 wave 11 lane JTIER (TIERMGR21-2): the admission's ticket, drawn
        // under `methods` like the slot flag it names. `0` with the switch off.
        let ticket = if self.slot_ticket {
            self.next_slot_ticket.fetch_add(1, Ordering::Relaxed)
        } else {
            0
        };
        if osr {
            state.osr_in_flight = true;
            state.osr_in_flight_tier = Some(task.target_tier);
            state.queued_osr_bci = task.osr_bci;
            state.osr_slot_ticket = ticket;
        } else {
            state.queued_for_compilation = true;
            state.queued_tier = Some(task.target_tier);
            state.entry_slot_ticket = ticket;
            if state.first_nominated_ms == 0 {
                state.first_nominated_ms = process_uptime_ms().max(1);
            }
        }
        // j27 lane SEEDOWE: a method-entry C2 task reads the seeded row.
        ic_seed_owed::answered_by_admission(state, &task);
        self.enqueue(task, ticket);
        true
    }

    /// Release the in-flight slot for a request leaving by any exit, closing
    /// the branch-profile window it opened. `osr_bci` picks the slot: `Some`
    /// is the OSR slot, `None` the method-entry slot.
    ///
    /// A request that no longer holds the slot does not release it. That
    /// happens when a deopt dropped a queued request, the slot was re-granted,
    /// and the OLD request is only now reaching completion or retirement:
    /// clearing the flag then would admit a duplicate beside the new request.
    ///
    /// t1 wave 11 lane JTIER (TIERMGR21-2, `CRATONVM_JIT_TIER_SLOT_TICKET`):
    /// with a ticket (`ticket != 0`) the match is exact: only the request the
    /// slot was last granted to releases it. The `(tier, bci)` match below
    /// could not tell an old same-tier request (one whose state was removed
    /// by `invalidate_class` while it compiled, and re-created under the same
    /// identity-less key) from the request holding the slot now. `ticket == 0`
    /// (the synchronous `compilation_complete` door, the switch off) keeps the
    /// `(tier, bci)` match.
    fn release_slot_for(
        &self,
        state: &mut MethodState,
        tier: CompilationTier,
        osr_bci: Option<u32>,
        ticket: u64,
    ) {
        if self.request_is_orphaned(state, osr_bci, ticket) {
            return;
        }
        if osr_bci.is_some() {
            if state.osr_in_flight
                && (state.osr_in_flight_tier != Some(tier) || state.queued_osr_bci != osr_bci)
            {
                return;
            }
            state.osr_in_flight = false;
            state.osr_in_flight_tier = None;
            state.queued_osr_bci = None;
            // No window to close: no OSR door opens one
            // (`branch_window_armed` belongs to the method-entry slot).
            return;
        }
        if state.queued_for_compilation && state.queued_tier != Some(tier) {
            return;
        }
        state.queued_for_compilation = false;
        state.queued_tier = None;
        if std::mem::take(&mut state.branch_window_armed) {
            self.close_branch_window();
        }
    }

    /// t1 wave 11 lane JTIER (TIERMGR21-2, `CRATONVM_JIT_TIER_SLOT_TICKET`):
    /// whether `c` completes a request `state` never admitted to the slot it
    /// names -- the state was removed (`invalidate_class`) while the compile
    /// ran and re-created under the same key (an identity-less key: the next
    /// class loaded under the name). A dispatched request holds its slot until
    /// its completion (only queued requests are removed by the deopt and
    /// bailout drops), so a ticket other than the slot's last one cannot be
    /// this state's. [`Self::finish_owed`] then leaves the state alone, exactly
    /// as it does for an identified key whose state the invalidation removed.
    /// `false` for a ticketless completion and with the switch off.
    fn completion_is_orphaned(&self, state: &MethodState, c: &Completion) -> bool {
        self.request_is_orphaned(state, c.osr_bci, c.ticket)
    }

    /// [`Self::completion_is_orphaned`] for any leaving request: its slot
    /// (`osr_bci` picks it, as in [`Self::release_slot_for`]) was last granted
    /// under another ticket.
    fn request_is_orphaned(&self, state: &MethodState, osr_bci: Option<u32>, ticket: u64) -> bool {
        if ticket == 0 || !self.slot_ticket {
            return false;
        }
        let held_by = if osr_bci.is_some() {
            state.osr_slot_ticket
        } else {
            state.entry_slot_ticket
        };
        held_by != ticket
    }

    /// Open a branch-profile window for the C2 nomination `state` is about to
    /// receive. Paired with exactly one [`Self::close_branch_window`], through
    /// `branch_window_armed`.
    fn open_branch_window(&self, state: &mut MethodState) {
        state.branch_window_armed = true;
        self.branch_window_balance.fetch_add(1, Ordering::Relaxed);
        crate::profile::arm_branch_profiling_for_c2();
    }

    fn close_branch_window(&self) {
        self.branch_window_balance.fetch_sub(1, Ordering::Relaxed);
        crate::profile::disarm_branch_profiling_for_c2();
    }

    /// Release the in-flight slot of every dropped request, count the drop,
    /// and trace it under `CRATONVM_DBG_TIER_ENQUEUE`.
    ///
    /// ## Lock order
    ///
    /// This takes `methods` and **must not** be called while a lane's queue is
    /// held. The established order in this file is `methods` → `queue`:
    /// `should_compile_inner` holds `methods` across [`Self::admit`], which
    /// takes `queue`. A worker discovers stale requests while holding its
    /// queue, so it collects them into a `Vec`, drops the queue guard, and
    /// only then calls this. Taking `methods` under `queue` here would invert
    /// the order and deadlock against any thread on the invocation hook.
    ///
    /// ## What is deliberately NOT touched
    ///
    /// `current_tier`, `tier_fail_count` and `ineligible` are all left alone.
    /// A stale request is not a compile failure and not a policy decline —
    /// nothing was compiled and nothing was decided. Routing this through
    /// [`Self::finish`] with `success = false` would spend one of the
    /// method's [`MAX_TIER_FAIL_RETRIES`], so three redefinitions during
    /// warmup would leave a hot method permanently un-compilable with no
    /// diagnostic — precisely the silent loss this whole path exists to
    /// prevent. Releasing the slot (and closing its branch window) is the
    /// entire state change, and it is what lets the next invocation re-admit
    /// the method against the bytecode that is actually loaded.
    fn retire_stale(&self, stale: &[StaleRequest]) {
        if stale.is_empty() {
            return;
        }
        let trace = cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_TIER_ENQUEUE");
        // JIT round 21 lane tiermgr (INDYCONST21-1): owed C2 retries a dropped
        // request held and nothing re-admitted, asked once `methods` is free.
        let mut owed: Vec<(MethodKey, &'static str)> = Vec::new();
        {
            let mut methods = self.methods.lock();
            for request in stale {
                // TIERMGR21-2: a request the state never admitted (it was
                // re-created under the key) leaves the state alone.
                if let Some(state) = methods
                    .get_mut(&request.task.method_key)
                    .filter(|s| !self.request_is_orphaned(s, request.task.osr_bci, request.ticket))
                {
                    self.release_slot_for(
                        state,
                        request.task.target_tier,
                        request.task.osr_bci,
                        request.ticket,
                    );
                    self.readmit_dropped_optimizing_request(
                        state,
                        request.task.target_tier,
                        request.task.osr_bci,
                    );
                    if request.task.osr_bci.is_none() {
                        if let Some(door) = Self::take_dropped_owed_retry(state) {
                            owed.push((request.task.method_key.clone(), door));
                        }
                    }
                }
            }
        }
        // Not at shutdown: nothing would compile it, and the drain that called
        // this must leave the queues empty.
        if !self.shutdown.load(Ordering::Acquire) {
            for (key, door) in owed {
                self.ask_owed_retry(&key, door);
            }
        }
        for request in stale {
            crate::metrics::record_scheduling_event(request.event);
            if trace {
                eprintln!(
                    "[cratonvm-tier] drop {}.{}{} tier={:?}{} reason={} queued_epoch={} epoch={}",
                    request.task.method_key.class_name,
                    request.task.method_key.method_name,
                    request.task.method_key.descriptor,
                    request.task.target_tier,
                    request
                        .task
                        .osr_bci
                        .map(|b| format!(" osr_bci={b}"))
                        .unwrap_or_default(),
                    request.event,
                    request.queued_epoch,
                    request.current_epoch,
                );
            }
        }
        self.dropped
            .fetch_add(stale.len() as u64, Ordering::Release);
    }

    /// Drop a dequeued method-entry single-pass request whose method already
    /// runs an OPTIMIZING body published after it was queued (tier proposal
    /// R12-4). Returns whether it was dropped.
    ///
    /// The route: a C1 task waits in its lane while a door the manager did
    /// not schedule (the optimizing callee compiler, the eager callee chain)
    /// publishes a C2 body and reports it through
    /// [`TieredCompilationManager::note_published_outside_manager`], which
    /// raises `current_tier` to C2. The worker's C1 compile does not look at
    /// the cache (`try_jit_compile_wrapped_entry` compiles fresh), so its
    /// publish REPLACED the C2 body with a single-pass one and `finish` then
    /// lowered `current_tier` to C1, until the C1 -> C2 arm re-promoted the
    /// method and compiled C2 a second time. `current_tier` is lowered by
    /// every removal (`note_body_withdrawn`, an evicting deopt, and since
    /// round 12 wave 4 every other withdrawal, which the VM's cache reports
    /// through `crate::JitBodyWithdrawalSink`; before that a class-hierarchy
    /// change, a define or an unload left it standing), so an optimizing
    /// `current_tier` names a live optimizing body and the request has
    /// nothing left to do.
    ///
    /// Released like a stale drop (the slot, a C2 window if one was armed)
    /// and counted in `dropped`, with no retry spent and no tier change. An
    /// OSR request is never dropped here: its artifact serves loop entry and
    /// `current_tier` says nothing about it. The window after dispatch (the
    /// outside publish lands while the C1 compile runs) is not closed here;
    /// see `r12w2-tier-c1-publish-supersedes-a-live-c2-body`.
    /// `CRATONVM_JIT_DROP_OVERTAKEN_TASKS=0` keeps every such request.
    /// `ticket` is the request's slot ticket ([`QueuedRequest::ticket`]).
    fn retire_overtaken(&self, task: &CompilationTask, ticket: u64) -> bool {
        if task.osr_bci.is_some()
            || tier_uses_optimized_backend(task.target_tier)
            || !cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_DROP_OVERTAKEN_TASKS")
        {
            return false;
        }
        let mut owed_retry = None;
        let overtaken_by = {
            let mut methods = self.methods.lock();
            match methods.get_mut(&task.method_key) {
                Some(state)
                    if tier_uses_optimized_backend(state.current_tier)
                        && !self.request_is_orphaned(state, task.osr_bci, ticket) =>
                {
                    self.release_slot_for(state, task.target_tier, task.osr_bci, ticket);
                    // JIT round 21 lane tiermgr (INDYCONST21-1): a dropped
                    // single-pass request answers no owed C2 retry (nor the
                    // late-bind owed bit, t1 wave 11 JTIER).
                    owed_retry = Self::take_dropped_owed_retry(state);
                    Some(state.current_tier)
                }
                _ => None,
            }
        };
        let Some(current) = overtaken_by else {
            return false;
        };
        if let Some(door) = owed_retry {
            self.ask_owed_retry(&task.method_key, door);
        }
        self.dropped.fetch_add(1, Ordering::Release);
        if cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_TIER_ENQUEUE") {
            eprintln!(
                "[cratonvm-tier] drop {}.{}{} tier={:?} reason=overtaken current_tier={:?}",
                task.method_key.class_name,
                task.method_key.method_name,
                task.method_key.descriptor,
                task.target_tier,
                current,
            );
        }
        true
    }

    /// Re-queue a dropped method-entry C2 request for a method that already
    /// runs a published body, because nothing else will ever form it again.
    ///
    /// [`Self::retire_stale`]'s "the next invocation re-admits the method"
    /// holds only for a request the interpreter's invocation hook formed. The
    /// C1->C2 supersede ([`Self::request_c2_upgrade`]), [`Self::promote_hot_c1_to_c2`]
    /// and [`Self::request_deferred_new_retry`] are formed once, right after a
    /// publish, for a method whose calls from then on route through that body
    /// and never reach the hook. The install epoch is process-wide, so one class
    /// redefinition (any coverage agent) dropped every such request for every
    /// class, and the methods stayed at C1. The same holds for a C2 compile the
    /// epoch overtook ([`Self::finish`]'s uncharged arm).
    ///
    /// Call with `methods` held, AFTER `release_slot_for`. A method that was
    /// itself redefined is excluded by `current_tier`: `on_class_redefined`
    /// resets it to `Interpreter`. Every gate the forming doors ask is asked
    /// again, and the slot is checked before the branch window opens, so a
    /// refusal cannot leak a window. Enqueued at the CURRENT epoch
    /// (`enqueue_time_ms: 0` restamps), so it cannot be dropped for the same
    /// move twice; each re-admission costs one real epoch move.
    fn readmit_dropped_optimizing_request(
        &self,
        state: &mut MethodState,
        tier: CompilationTier,
        osr_bci: Option<u32>,
    ) {
        if osr_bci.is_some()
            || !tier_uses_optimized_backend(tier)
            || !matches!(
                state.current_tier,
                CompilationTier::C1 | CompilationTier::C1WithProfiling | CompilationTier::C2
            )
            || state.queued_for_compilation
            || state.c2_bailout
            || state.ineligible
            || state.entry_ineligible
            || state.entry_out_of_retries()
            || self.tiering_is_disabled()
            // JIT round 21 lane tiermgr (review): not at shutdown. The
            // shutdown drain (`BackgroundCompiler::shutdown`) and a worker's
            // last `retire_stale` reach here with the workers gone or going:
            // a re-admitted request then sat in a queue nobody drains, held
            // the method's slot, and left its branch-profile window open --
            // `profile`'s window is process-wide, so one VM's teardown pinned
            // interpreter branch profiling on for every other VM in the
            // process. Unconditional (an accounting fix).
            || self.shutdown.load(Ordering::Acquire)
        {
            return;
        }
        self.open_branch_window(state);
        let key = state.method_key.clone();
        self.admit(
            state,
            CompilationTask {
                method_key: key,
                target_tier: CompilationTier::C2,
                priority: CompilationPriority::Low,
                enqueue_time_ms: 0,
                osr_bci: None,
            },
        );
    }

    /// Pop the highest-priority request across BOTH lanes, for the manual
    /// drains ([`TieredCompilationManager::dequeue_compilation`] and
    /// [`TieredCompilationManager::next_fresh_task`]) -- the compile pipeline
    /// drains one lane per worker instead.
    ///
    /// With `current = Some(epoch)`, requests queued before `epoch` are pushed
    /// onto `stale` on the way past, exactly as [`CompilationQueue::dequeue_fresh`]
    /// does; with `None` the stamp is ignored.
    fn pop_across_lanes(
        &self,
        current: Option<u64>,
        stale: &mut Vec<StaleRequest>,
    ) -> Option<CompilationTask> {
        // Both lane locks, always C1 then C2. A worker only ever holds its own
        // lane's lock, and nothing takes `methods` under either, so this
        // order cannot deadlock.
        let mut c1 = self.lanes[C1_LANE].queue.lock();
        let mut c2 = self.lanes[C2_LANE].queue.lock();
        loop {
            let from_c2 = match (c1.front_rank(), c2.front_rank()) {
                (None, None) => return None,
                (Some(_), None) => false,
                (None, Some(_)) => true,
                // Equal ranks go to the optimizing lane.
                (Some(r1), Some(r2)) => r2 >= r1,
            };
            let entry = if from_c2 { c2.dequeue() } else { c1.dequeue() }?;
            match current {
                Some(now) if entry.install_epoch < now => stale.push(StaleRequest {
                    task: entry.task,
                    event: crate::metrics::SCHEDULING_EVENTS[0],
                    queued_epoch: entry.install_epoch,
                    current_epoch: now,
                    ticket: entry.ticket,
                }),
                _ => return Some(entry.task),
            }
        }
    }

    /// Remove every queued (not yet dispatched) request for `key` from both
    /// lanes, each with its slot ticket. Call with `methods` held.
    fn remove_queued_for(&self, key: &MethodKey) -> Vec<(CompilationTask, u64)> {
        let mut removed = Vec::new();
        for lane in &self.lanes {
            removed.extend(
                lane.queue
                    .lock()
                    .remove_matching(|task| task.method_key == *key),
            );
        }
        removed
    }

    /// Number of requests queued across both lanes.
    fn queue_len(&self) -> usize {
        self.lanes.iter().map(|lane| lane.queue.lock().len()).sum()
    }

    /// Requests that have left either lane, stale drops included.
    #[cfg(test)]
    fn total_processed(&self) -> u64 {
        self.lanes
            .iter()
            .map(|lane| lane.queue.lock().total_processed)
            .sum()
    }

    /// Expire every [`InvocationVerdict::settled_generation`] stamp handed out
    /// so far.
    fn bump_settled_generation(&self) {
        // A wrap to 0 is harmless: a 0 stamp never counts as settled.
        self.settled_generation.fetch_add(1, Ordering::AcqRel);
    }

    /// Whether OSR is denied for `key` at the current install epoch.
    fn is_osr_denied(&self, key: &MethodKey) -> bool {
        let denied = self.osr_denied.read();
        match denied.get(key) {
            Some(&stamp) => stamp == self.current_install_epoch(),
            None => false,
        }
    }

    fn mark_osr_denied(&self, key: MethodKey) {
        let epoch = self.current_install_epoch();
        self.osr_denied.write().insert(key, epoch);
    }

    /// Claim a slot for a compile dispatched at `epoch`; returns the slot to
    /// hand back to [`Self::note_dispatch_done`].
    ///
    /// `epoch + 1` is stored so that `0` can mean "free" without reserving a
    /// real epoch value — install epoch `0` is a legal state (it is what
    /// `AtomicU64::new(0)` starts at in `with_install_epoch_source`), and a
    /// sentinel that collides with a legal value is how this kind of record
    /// goes quietly wrong.
    ///
    /// `InflightSlot::NONE` when every slot is taken, which the array's sizing
    /// makes structurally impossible; the counter records it either way.
    fn note_dispatch(&self, epoch: u64) -> InflightSlot {
        self.inflight_count.fetch_add(1, Ordering::AcqRel);
        let stamp = epoch.saturating_add(1);
        for (i, slot) in self.inflight_epochs.iter().enumerate() {
            if slot
                .compare_exchange(0, stamp, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                return InflightSlot(i);
            }
        }
        self.inflight_slot_exhausted.fetch_add(1, Ordering::Relaxed);
        InflightSlot::NONE
    }

    /// Charge the time `task` spent queued on `lane` to that lane's wait
    /// statistics, as of `now_ms` on [`process_uptime_ms`]'s clock. Called by
    /// the worker at dispatch. See [`CompilationStats::queue_wait_ms_total`].
    ///
    /// A stamp from another clock -- or `0`, the "unset" sentinel a direct
    /// caller of this function might pass -- counts the dispatch but charges no
    /// wait: a wall-clock stamp is far in `process_uptime_ms`'s future and
    /// would otherwise read as age 0 anyway, and an unstamped one has no age.
    fn note_queue_wait(&self, lane: usize, task: &CompilationTask, now_ms: u64) {
        let Some(dispatched) = self.stats.dispatched.get(lane) else {
            return;
        };
        dispatched.fetch_add(1, Ordering::Relaxed);
        if task.enqueue_time_ms == 0 || task.enqueue_time_ms > now_ms {
            return;
        }
        let waited = now_ms - task.enqueue_time_ms;
        self.stats.queue_wait_ms_total[lane].fetch_add(waited, Ordering::Relaxed);
        self.stats.queue_wait_ms_max[lane].fetch_max(waited, Ordering::Relaxed);
    }

    /// Charge the time-to-tier instrument for a method-entry publish at
    /// `tier`: the first body of any tier is timed from the first admission,
    /// and the first C2 body from the first body. Each is charged once per
    /// method (a redefinition that resets the state does not reset the
    /// stamps; the recompile is not "reaching" the tier for the first time).
    fn note_time_to_tier(&self, state: &mut MethodState, tier: CompilationTier) {
        let now = process_uptime_ms().max(1);
        if state.first_body_ms == 0 {
            state.first_body_ms = now;
            if state.first_nominated_ms != 0 && state.first_nominated_ms <= now {
                let waited = now - state.first_nominated_ms;
                self.stats
                    .nominate_to_first_body_ms_total
                    .fetch_add(waited, Ordering::Relaxed);
                self.stats
                    .nominate_to_first_body_ms_max
                    .fetch_max(waited, Ordering::Relaxed);
                self.stats
                    .nominate_to_first_body
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
        if tier == CompilationTier::C2 && state.first_c2_ms == 0 {
            state.first_c2_ms = now;
            if state.first_body_ms <= now {
                // `0` for a method whose FIRST body was C2 (straight to C2):
                // counted, so the mean says how often the supersede wait was
                // skipped rather than hiding it.
                let waited = now - state.first_body_ms;
                self.stats
                    .first_body_to_c2_ms_total
                    .fetch_add(waited, Ordering::Relaxed);
                self.stats
                    .first_body_to_c2_ms_max
                    .fetch_max(waited, Ordering::Relaxed);
                self.stats.first_body_to_c2.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn note_dispatch_done(&self, slot: InflightSlot) {
        if let Some(cell) = self.inflight_epochs.get(slot.0) {
            cell.store(0, Ordering::Release);
        }
        self.inflight_count.fetch_sub(1, Ordering::AcqRel);
    }

    /// The install epoch of the OLDEST compile currently running, or `0` when
    /// every worker is idle.
    ///
    /// The oldest rather than the newest, because the question this answers is
    /// "is anything running that was dispatched before the world moved?" — and
    /// the oldest in-flight dispatch is the one that answers it. A scan of at
    /// most [`INFLIGHT_EPOCH_SLOTS`] relaxed loads; it is a diagnostic, not a
    /// hot path.
    ///
    /// Not an atomic snapshot: slots may be claimed and released while the
    /// scan walks. The answer is therefore some epoch that WAS in flight
    /// during the scan, or `0` if the scan saw an idle moment — which is the
    /// same guarantee any lock-free census here gives, and is stated rather
    /// than implied.
    fn inflight_oldest_epoch(&self) -> u64 {
        let mut best: Option<u64> = None;
        for slot in self.inflight_epochs.iter() {
            let stamp = slot.load(Ordering::Acquire);
            if stamp == 0 {
                continue;
            }
            let epoch = stamp - 1;
            best = Some(match best {
                Some(b) => b.min(epoch),
                None => epoch,
            });
        }
        best.unwrap_or(0)
    }

    /// Count a contained compile panic and log the first one.
    fn note_worker_panic(&self, task: &CompilationTask, payload: &(dyn std::any::Any + Send)) {
        self.worker_panics.fetch_add(1, Ordering::Relaxed);
        crate::metrics::record_scheduling_event(crate::metrics::SCHEDULING_EVENTS[6]);
        if !self.panic_logged.swap(true, Ordering::AcqRel) {
            tracing::warn!(
                target: "cratonvm::jit",
                "compile of {}.{}{} (tier={:?}{}) panicked and was contained: {}. The method is \
                 now ineligible and the compile worker keeps running; later panics are counted \
                 under the `worker_panic` scheduling event without this line.",
                task.method_key.class_name,
                task.method_key.method_name,
                task.method_key.descriptor,
                task.target_tier,
                task.osr_bci
                    .map(|b| format!(" osr_bci={b}"))
                    .unwrap_or_default(),
                panic_payload_message(payload),
            );
        }
    }

    /// One more C2 attempt for a method whose IR build bailed on a `new` whose
    /// class had not loaded yet.
    ///
    /// [`Self::request_c2_upgrade`] minus the `current_tier >= C2` clause, and
    /// nothing else: `queued_for_compilation`, `c2_bailout`, `ineligible`, the
    /// tier-failure budget and the hotness gate all still apply. Dropping that
    /// one clause is the whole point — the method IS at C2, with a single-pass
    /// body, because the C2 attempt fell through.
    ///
    /// Safe to call unconditionally because the caller has already consumed a
    /// one-shot memo (`cratonvm_jit::take_deferred_new_retry`), so a method can
    /// reach here at most once per process.
    ///
    /// JIT round 21 lane indyconst: returns the retry door's answer (the VM's
    /// doors owe an `AlreadyQueued` one to the running task).
    pub(crate) fn request_deferred_new_retry(&self, key: &MethodKey) -> C2RetryAnswer {
        self.request_c2_retry(key, "deferred-new")
    }

    /// Take `key`'s owed late-bind retry ([`MethodState::late_bind_retry_owed`]).
    /// Asked by the worker after `finish` released the method-entry slot.
    fn take_late_bind_retry_owed(&self, key: &MethodKey) -> bool {
        let mut methods = self.methods.lock();
        methods
            .get_mut(key)
            .is_some_and(|state| std::mem::take(&mut state.late_bind_retry_owed))
    }

    /// The C2 retry both [`Self::request_deferred_new_retry`] and
    /// [`TieredCompilationManager::request_self_lock_recompile`] take (round 15
    /// wave 5, lane sync4, patch
    /// `r15w4-sync3-tiered-self-lock-recompile-door-patch-FIXED-20260930.md`): one
    /// door, two callers, each bounded by its own caller, and `door` names the
    /// caller on the `CRATONVM_DBG_JITC` lines so a census of one does not
    /// count the other.
    ///
    /// JIT round 20 (lane stash, EAGER2-2 of `jit-r19-eager2-proposals.md`):
    /// the answer says whether a compile of the method will run
    /// ([`C2RetryAnswer`]); every caller but the late-bind door ignores it.
    /// JIT r23 wave 9, lane L6 (`CRATONVM_JIT_C2_PUBLICATION_REFUSAL_REFUND`):
    /// hand back the `tier_fail_count` charge of a method-entry C2 compile
    /// whose body `put` refused ([`publication_refusal_refund`]). Asked by
    /// the worker after `finish`, before the `refused-publication` retry.
    /// When `finish` did not charge (an epoch move, a park), this takes back
    /// an older charge instead; that is bounded by the same refund cap.
    fn refund_refused_publication(&self, key: &MethodKey) {
        let enabled = c2_publication_refusal_refund_enabled();
        if !enabled {
            return;
        }
        let mut methods = self.methods.lock();
        let identified = identify_unidentified_key(&methods, key);
        let key = identified.as_ref().unwrap_or(key);
        let Some(state) = methods.get_mut(key) else {
            return;
        };
        if let Some((fails, refunds)) = publication_refusal_refund(
            state.tier_fail_count,
            state.publication_refusal_refunds,
            enabled,
        ) {
            state.tier_fail_count = fails;
            state.publication_refusal_refunds = refunds;
            if cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_JITC") {
                eprintln!(
                    "[cratonvm-jitc] refused-publication refund {}.{}{}: tier_fail_count={} refunds={}/{}",
                    key.class_name,
                    key.method_name,
                    key.descriptor,
                    fails,
                    refunds,
                    MAX_PUBLICATION_REFUSAL_REFUNDS
                );
            }
        }
    }

    fn request_c2_retry(&self, key: &MethodKey, door: &'static str) -> C2RetryAnswer {
        let dbg = cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_JITC");
        macro_rules! refuse {
            ($why:expr) => {
                refuse!($why, C2RetryAnswer::Refused)
            };
            ($why:expr, $answer:expr) => {{
                if dbg {
                    eprintln!(
                        "[cratonvm-jitc] {} retry REFUSED {}.{}{}: {}",
                        door, key.class_name, key.method_name, key.descriptor, $why
                    );
                }
                return $answer;
            }};
        }
        if self.tiering_is_disabled() {
            refuse!("tiering disabled");
        }
        // JIT round 21 lane tiermgr (INDYLATE21-1): every retry says "an input
        // of this method's build changed". Moved BEFORE anything is admitted,
        // so the compile this request queues (or the one it is owed to) cannot
        // read the acceptance memo under the old stamp.
        self.note_retry_inputs_changed(key);
        let mut methods = self.methods.lock();
        // The VM's re-offer sweep names the class by `get_loaded_class_id(..)
        // .unwrap_or(0)`; an identity-less key must reach the state the
        // invocation hooks built, not mint an id-0 twin beside it.
        let identified = identify_unidentified_key(&methods, key);
        let key = identified.as_ref().unwrap_or(key);
        // `or_insert_with`, not `get_mut`. MEASURED: `get_mut` refused
        // every method the eager first-call door compiles, with "no tier
        // state for this method" -- that door hands the backend a method
        // the interpreter never counted invocations for, so the manager has
        // never seen its key. `RJitGc.make` is one, and it is the method
        // this whole path exists for.
        //
        // Creating the state here is what `on_invocation` / `request_osr`
        // do for their own keys, and it is inert for everything else: a
        // fresh `MethodState` is `current_tier = Interpreter`, unqueued,
        // and every other gate below still applies.
        let state = methods
            .entry(key.clone())
            .or_insert_with(|| MethodState::new(key.clone()));
        if state.queued_for_compilation {
            if LATE_BIND_RETRY_DOORS.contains(&door) && late_bind_retry_owed_enabled() {
                state.late_bind_retry_owed = true;
                refuse!(
                    "already queued (owed: asked again when that task finishes)",
                    C2RetryAnswer::AlreadyQueued
                );
            }
            // JIT round 21 lane tiermgr (INDYCONST21-1,
            // `CRATONVM_JIT_TIER_RETRY_OWED`): remembered, not dropped. The
            // request holding the slot answers it only if it is an optimizing
            // compile dispatched after this stamp; otherwise its exit re-asks
            // (`take_unanswered_retry`). The answer stays `AlreadyQueued`, so
            // the VM doors that owe it themselves are unchanged.
            if tier_retry_owed_enabled() {
                let stamp = self.retry_generation.fetch_add(1, Ordering::SeqCst) + 1;
                state.retry_owed = Some((stamp, door));
                refuse!(
                    "already queued; owed to the request holding the slot",
                    C2RetryAnswer::AlreadyQueued
                );
            }
            refuse!("already queued", C2RetryAnswer::AlreadyQueued);
        }
        if state.c2_bailout {
            refuse!("c2_bailout");
        }
        if state.ineligible || state.entry_ineligible {
            refuse!("ineligible");
        }
        if state.entry_out_of_retries() {
            refuse!("tier_fail_count exhausted");
        }
        let gate = self.c2_upgrade_min_invocations.load(Ordering::Relaxed);
        if gate != 0 && state.invocation_count < gate {
            // j27 lane SEEDOWE: paid when the count reaches the gate.
            ic_seed_owed::owe(state, door, ic_seed_owed::OwedSeedRetry::HotnessGate);
            refuse!("below the c2-upgrade hotness gate");
        }
        if dbg {
            eprintln!(
                "[cratonvm-jitc] {} retry ENQUEUED {}.{}{}",
                door, key.class_name, key.method_name, key.descriptor
            );
        }
        // Interpreter round i6 wave 117, lane L9 (review; page
        // `i117-L9-a-one-shot-c2-retry-is-eaten-by-the-waiting-on-class-park`):
        // every door of this retry is a one-shot EVENT -- a deferred field or
        // `new` site that just linked (`late-bind`, `deferred-new`), an
        // unenterable OSR body -- and the late-bind ones are precisely the
        // signal a waiting-on-class park waits for: a static owner whose
        // `<clinit>` ran links its site without moving the class-definition
        // epoch. `admit` would refuse it as one more parked offer and the
        // event, already taken from its table, would be gone. So the retry
        // ends the park first; a compile that is still waiting parks again,
        // within `MAX_WAITING_ON_CLASS_PARKS`.
        // Open the branch-profile window: there is now an optimizing compile
        // pending that will read the counts. Closed by whichever exit the
        // request takes -- see `MethodState::branch_window_armed`.
        // `admit` refuses past the code-cache cap (or a gate asked above):
        // nothing was queued then. All three steps: `admit_one_shot_c2`.
        // j27 lane SEEDOWE: the generation is read before `admit`, so a bump
        // racing the refusal still pays the owe.
        let generation = self.settled_generation.load(Ordering::Acquire);
        if self.admit_one_shot_c2(state) {
            C2RetryAnswer::Enqueued
        } else {
            if state.deferred_at_cap {
                ic_seed_owed::owe(
                    state,
                    door,
                    ic_seed_owed::OwedSeedRetry::CodeCacheCap { generation },
                );
            }
            C2RetryAnswer::Refused
        }
    }

    /// JIT round 21 lane tiermgr (INDYLATE21-1,
    /// `CRATONVM_JIT_IR_MEMO_METHOD_INPUT_STAMP`): a C2 retry is asked because
    /// something the method's optimizing build reads besides its bytecode now
    /// answers differently (a late-bind callee compiled, an indy target bound,
    /// a deferred `new` class loaded, a self-lock mirror minted, a publication
    /// refused). Move the method's input stamp in this VM's acceptance memo,
    /// so the build the retry gets is not SKIPPED by a refusal judged on the
    /// old inputs (`ir_evidence::IrRefusalMemo::already_refused`). Before
    /// this, only the indy doors forgot the refusal (round 21 lane indylate,
    /// `JitIndyGenericSite::reoffer_optimizing_build`); every other door's
    /// `Enqueued` retry could be skipped in a steady loop. Takes `methods`
    /// briefly only for an identity-less key.
    fn note_retry_inputs_changed(&self, key: &MethodKey) {
        if !crate::ir_evidence::ir_memo_method_input_stamp_enabled() {
            return;
        }
        let identified = if key.class_id == ClassId::new(0) {
            identify_unidentified_key(&self.methods.lock(), key)
        } else {
            None
        };
        let key = identified.as_ref().unwrap_or(key);
        if key.class_id == ClassId::new(0) {
            return;
        }
        self.verdicts.note_ir_method_inputs_changed(
            &key.class_name,
            &key.method_name,
            &key.descriptor,
            key.class_id,
        );
    }

    /// C1→C2 supersede: enqueue a Low-priority C2 recompile for a method
    /// whose C1 body just published. Idempotent: skipped when the method is
    /// already queued, already at C2, has bailed out of C2, or has exhausted
    /// its compile retries. Called by the worker loop AFTER [`Self::finish`]
    /// released the C1 task's slot.
    ///
    /// Returns whether a request was admitted (HANDLER8-1's budget refunds a
    /// refusal; the worker loop ignores the answer).
    fn request_c2_upgrade(&self, key: &MethodKey) -> bool {
        // `CRATONVM_TIER_ENABLED=0` closes this door too. See
        // `tiering_is_disabled`.
        if self.tiering_is_disabled() {
            return false;
        }
        let mut methods = self.methods.lock();
        let Some(state) = methods.get_mut(key) else {
            return false;
        };
        if state.queued_for_compilation
            || state.current_tier >= CompilationTier::C2
            || state.c2_bailout
            || state.ineligible
            || state.entry_ineligible
            || state.entry_out_of_retries()
        {
            return false;
        }
        // Hotness gate, off unless the operator set
        // `CRATONVM_TIER_C2_THRESHOLD`. Every other door to C2 asks for
        // `invocation_count >= max(c2_threshold, c2_min_invocations)` -- the
        // number `c2_hotness_bar` folds, and the number `c2_upgrade_gate` now
        // mirrors here so one knob cannot mean two bars; this one asked for
        // nothing,
        // which is what made that knob inert for the path that produces
        // nearly all C2 compiles. See the field for why the default stays
        // ungated. A method held back here is not held back forever:
        // `should_compile`'s own C2 arm admits it once it really is that hot.
        let gate = self.c2_upgrade_min_invocations.load(Ordering::Relaxed);
        if gate != 0 && state.invocation_count < gate {
            return false;
        }
        self.open_branch_window(state);
        self.admit(
            state,
            CompilationTask {
                method_key: key.clone(),
                target_tier: CompilationTier::C2,
                priority: CompilationPriority::Low,
                enqueue_time_ms: 0,
                osr_bci: None,
            },
        )
    }

    /// HANDLER8-1 (JIT round 19 wave 9, lane handler9;
    /// `CRATONVM_JIT_OUTSIDE_MANAGER_C2_UPGRADE`, opt-in): the C1 -> C2
    /// supersede [`Self::request_c2_upgrade`] gives a manager-scheduled C1
    /// publish, given to a C1 body published OUTSIDE the manager whose method
    /// passes `c2_upgrade_would_engage` -- the same door with the same
    /// refusals (queued, already C2, bailed out, ineligible, out of retries, a
    /// raised `CRATONVM_TIER_C2_THRESHOLD` gate, tiering disabled), plus a
    /// per-VM budget ([`OUTSIDE_MANAGER_C2_UPGRADE_DEFAULT_BUDGET`]) that a
    /// refusal does not spend. Call with `methods` NOT held.
    fn request_outside_manager_c2_upgrade(&self, key: &MethodKey) -> bool {
        let census = &self.outside_c2;
        if !census.take_budget() {
            census.budget_refused.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        let admitted = self.request_c2_upgrade(key);
        if admitted {
            census.upgrades_admitted.fetch_add(1, Ordering::Relaxed);
        } else {
            census.refund_budget();
            census.upgrades_refused.fetch_add(1, Ordering::Relaxed);
        }
        if cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_TIER_ENQUEUE") {
            eprintln!(
                "[cratonvm-tier] outside-manager-c2-upgrade {}.{}{} admitted={admitted}",
                key.class_name, key.method_name, key.descriptor,
            );
        }
        admitted
    }

    /// HANDLER8-1 census: a candidate method's count just moved to `count`;
    /// count its first crossing of the C2 bar. Call with `methods` held.
    fn note_outside_candidate_count(&self, state: &mut MethodState) {
        if state.outside_c2_candidate_ms == 0 {
            return;
        }
        let bar = self.c2_hotness_bar.load(Ordering::Relaxed);
        if state.invocation_count < bar {
            return;
        }
        let waited = process_uptime_ms().saturating_sub(state.outside_c2_candidate_ms);
        state.outside_c2_candidate_ms = 0;
        let census = &self.outside_c2;
        census.crossed_bar.fetch_add(1, Ordering::Relaxed);
        census.crossed_ms_total.fetch_add(waited, Ordering::Relaxed);
        census.crossed_ms_max.fetch_max(waited, Ordering::Relaxed);
    }

    /// `should_compile`'s C1→C2 arm, asked at the one moment it can still be
    /// answered: the C1 publish that has just been recorded on `state`.
    ///
    /// Call with `methods` held, from [`Self::finish`], AFTER `release_slot_for`
    /// has given the slot back — the same ordering the worker loop's
    /// `request_c2_upgrade` call depends on, and for the same reason: with the
    /// C1 task still holding the slot, [`Self::admit`] would refuse this task as
    /// a duplicate and the promotion would silently not happen.
    ///
    /// ## Why here and not on the next invocation
    ///
    /// Because there is no next invocation to ask on. See
    /// [`Self::c2_hotness_bar`] for the full chain; the short version is that
    /// every interpreter call site that consults the tiered manager is on a
    /// path guarded by "this method has no compiled body", and `current_tier ==
    /// C1` means one was just published. The count the manager holds is
    /// therefore not merely stale at this instant — it is FINAL, and this is the
    /// last instant at which anything looks at it.
    ///
    /// ## Why this is not a policy change
    ///
    /// The predicate is the arm's own, unmodified: `invocation_count >=
    /// max(c2_threshold, c2_min_invocations)`, plus the identical refusal set
    /// [`Self::request_c2_upgrade`] applies (queued, already optimized, bailed
    /// out, ineligible, out of compile retries). It cannot admit a method the
    /// invocation hook would have refused; it only admits methods the hook
    /// would have admitted had it ever been asked again. With the default
    /// policy (`c2_threshold = 20 000`, `c1_threshold = 500`) a method has to
    /// have accumulated 20 000 *counted* invocations before its C1 body landed
    /// to pass here, which is rare — so this is close to inert by default and
    /// is felt mainly by the operator who lowers `CRATONVM_TIER_C2_THRESHOLD`
    /// and, before this, watched it do nothing.
    ///
    /// Deliberately `Low` priority, matching [`Self::request_c2_upgrade`]: for
    /// a method that door would also have taken, the task queued here is then
    /// byte-identical to the one it would have queued, so nothing about compile
    /// scheduling changes for the population that already worked. (That door
    /// runs after this one and finds the slot taken, which is exactly the
    /// idempotence [`Self::admit`] exists to provide.)
    ///
    /// Returns whether a C2 task was queued.
    fn promote_hot_c1_to_c2(&self, state: &mut MethodState) -> bool {
        let bar = self.c2_hotness_bar.load(Ordering::Relaxed);
        // `u64::MAX` is the "never" encoding, and no count can reach it anyway;
        // testing it first keeps the common case one load and one compare.
        if bar == u64::MAX || state.invocation_count < bar {
            return false;
        }
        if state.queued_for_compilation
            || state.current_tier >= CompilationTier::C2
            || state.c2_bailout
            || state.ineligible
            || state.entry_ineligible
            || state.entry_out_of_retries()
        {
            return false;
        }
        // Checked `queued_for_compilation` above, so `admit` cannot refuse and
        // the window this opens is guaranteed to be closed by the request's
        // exit. Opening it before a refusal would leak a branch-profile window.
        self.open_branch_window(state);
        let key = state.method_key.clone();
        self.admit(
            state,
            CompilationTask {
                method_key: key,
                target_tier: CompilationTier::C2,
                priority: CompilationPriority::Low,
                enqueue_time_ms: 0,
                osr_bci: None,
            },
        )
    }

    /// JIT r25 wave 4 (`CRATONVM_JIT_OUTSIDE_C1_DRAIN_C2`): queue the C2
    /// compile of a method whose outside-manager C1 body the drain found at
    /// [`OUTSIDE_C1_DRAIN_C2_BAR`] entries. The caller checked the tier, the
    /// bar and the switch; this applies [`Self::promote_hot_c1_to_c2`]'s own
    /// refusal set and `Low` priority. Call with `methods` held. Returns whether
    /// a C2 task was queued.
    fn promote_outside_c1_by_drain(&self, state: &mut MethodState) -> bool {
        if state.queued_for_compilation
            || state.current_tier >= CompilationTier::C2
            || state.c2_bailout
            || state.ineligible
            || state.entry_ineligible
            || state.entry_out_of_retries()
        {
            return false;
        }
        self.open_branch_window(state);
        let key = state.method_key.clone();
        self.admit(
            state,
            CompilationTask {
                method_key: key,
                target_tier: CompilationTier::C2,
                priority: CompilationPriority::Low,
                enqueue_time_ms: 0,
                osr_bci: None,
            },
        )
    }

    /// JIT r23 wave 11 (page
    /// `churn-constructor-runs-as-its-own-c1-body-under-a-c2-caller-FIXED-20261024`): queue
    /// the method-entry C2 compile of a method whose live entry body is
    /// single-pass and which has a published optimizing OSR body. HotSpot
    /// moves such a frame into its C2 OSR body at the C1 loop's back-edge
    /// overflow; a single-pass body here has no such door, so the entry body
    /// is promoted instead. Same exclusions and `Low` priority as
    /// [`Self::promote_hot_c1_to_c2`], without its hotness bar (the OSR body
    /// is the evidence). Returns whether a C2 task was queued.
    fn promote_c1_after_optimizing_osr(&self, state: &mut MethodState) -> bool {
        if !matches!(
            state.current_tier,
            CompilationTier::C1 | CompilationTier::C1WithProfiling
        ) {
            return false;
        }
        if state.queued_for_compilation
            || state.c2_bailout
            || state.ineligible
            || state.entry_ineligible
            || state.entry_out_of_retries()
        {
            return false;
        }
        self.open_branch_window(state);
        let key = state.method_key.clone();
        self.admit(
            state,
            CompilationTask {
                method_key: key,
                target_tier: CompilationTier::C2,
                priority: CompilationPriority::Low,
                enqueue_time_ms: 0,
                osr_bci: None,
            },
        )
    }

    /// Record that `key` finished a compile attempt at `tier`, charging wall
    /// time to the C2 budget. The workers call [`Self::finish`] directly with
    /// a CPU-time budget; this is the shape
    /// [`TieredCompilationManager::compilation_complete`] and the tests use.
    fn complete_task(
        &self,
        key: &MethodKey,
        tier: CompilationTier,
        compile_time_ms: u64,
        success: bool,
        osr: bool,
        declined_permanently: bool,
    ) {
        let osr_bci = if osr {
            self.methods.lock().get(key).and_then(|s| s.queued_osr_bci)
        } else {
            None
        };
        self.finish(
            key,
            Completion {
                tier,
                osr_bci,
                osr,
                compile_time_ms,
                compile_time_ns: compile_time_ms.saturating_mul(1_000_000),
                budget_ms: compile_time_ms,
                success,
                declined_permanently,
                epoch_moved: false,
                waiting_on_class: None,
                cap_refused: false,
                ticket: 0,
            },
        );
    }

    /// Record that `key` finished a compile attempt.
    ///
    /// `success` reports whether the attempt actually produced and published a
    /// compiled body (vs. `compile_fn` running but bailing internally — a
    /// skip-listed construct, a resolver miss, the code-cache cap, a
    /// concurrent class redefine, etc.). Only on success does `current_tier`
    /// advance to `tier` and the per-tier compilation stat increment — a
    /// failed attempt must NOT be recorded as "compiled", or the method is
    /// silently stuck interpreting forever: `current_tier` would already read
    /// as "done" for that tier, so `should_compile` would never recommend it
    /// again, and nothing was ever inserted into `jit_cache` for the
    /// interpreter's fast-path lookup to find.
    ///
    /// `osr` marks a back-edge OSR task. A successful OSR publish goes into
    /// the SEPARATE OSR artifact cache — the method-entry cache is still
    /// empty — so it must NOT advance `current_tier`: `should_compile` reads
    /// `current_tier` as "method-entry compiled through this tier" and
    /// returns `None` at C2, which starved the method-entry compile of any
    /// loop-heavy method whose OSR body published first (the invocation
    /// counter kept firing but every recommendation was refused, so each
    /// fresh call re-entered the interpreter and re-OSR'd forever — observed
    /// as QuickBench `sieve` never retiring its per-call interpreter warmup
    /// across 20,000 invocations). This is the mirror image of the
    /// `request_osr` decoupling introduced with the independent OSR cache:
    /// OSR requests are not suppressed by method-entry C2, and method-entry
    /// tiering must not be suppressed by an OSR artifact. Fail counters still
    /// clear/advance normally and are shared by both pipelines; the in-flight
    /// slots are not (an OSR completion releases the OSR slot only -- see
    /// `MethodState::queued_for_compilation`).
    ///
    /// JIT round 21 lane tiermgr (INDYCONST21-1): a completion with no
    /// dispatch stamp (the synchronous `compilation_complete` door, tests)
    /// counts as dispatched after every owed retry, and an owed retry it does
    /// not answer is asked again here; the workers use [`Self::finish_owed`].
    fn finish(&self, key: &MethodKey, c: Completion) {
        if let Some(door) = self.finish_owed(key, c, u64::MAX) {
            self.ask_owed_retry(key, door);
        }
    }

    /// [`Self::finish`] for a compile DISPATCHED at retry generation
    /// `dispatched_at` ([`Self::retry_generation`], read by the worker just
    /// before `compile_fn`). Returns the door of an owed C2 retry
    /// ([`MethodState::retry_owed`]) this completion did not answer, which the
    /// caller asks again ([`Self::ask_owed_retry`]) with `methods` released --
    /// after its other post-completion requests, so one that queued a compile
    /// meanwhile turns the ask into a fresh owed stamp that compile answers.
    fn finish_owed(
        &self,
        key: &MethodKey,
        c: Completion,
        dispatched_at: u64,
    ) -> Option<&'static str> {
        let mut owed_retry = None;
        // The enqueue line's other half: what became of the request (JIT
        // round 21 lane reflect3/4, a re-entry-only body that never landed).
        if cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_TIER_ENQUEUE") {
            eprintln!(
                "[cratonvm-tier] finish {}.{}{} tier={:?} osr={} success={} declined_permanently={} epoch_moved={} waiting_on_class={:?} cap_refused={}",
                key.class_name,
                key.method_name,
                key.descriptor,
                c.tier,
                c.osr,
                c.success,
                c.declined_permanently,
                c.epoch_moved,
                c.waiting_on_class,
                c.cap_refused,
            );
        }
        // Tier proposal W4-1: what the cache holds under the key now, asked
        // before `methods` is taken (the probe reads the VM's cache).
        let live = if c.success && !c.osr {
            self.live_entry_probe.get().and_then(|probe| probe(key))
        } else {
            None
        };
        // zt t18/pl8: the code-cache-cap deferral below is no compile
        // failure, so it is no `sun.ci.totalBailouts`. Nor is a compile
        // parked waiting on a class (wave 116 L2; HotSpot compiles such a
        // method with an uncommon trap or a patching stub): the same flag.
        let mut deferred_at_cap = false;
        {
            let mut methods = self.methods.lock();
            // TIERMGR21-2 (`CRATONVM_JIT_TIER_SLOT_TICKET`): a completion the
            // state never admitted (re-created under the key while this
            // compile ran) leaves the state alone, as a removed state does.
            if let Some(state) = methods
                .get_mut(key)
                .filter(|s| !self.completion_is_orphaned(s, &c))
            {
                if c.success {
                    // A single-pass completion whose publish kept a live
                    // optimizing body (R12-4, `JitCache::put`) reports
                    // success, and the method still runs that body: do not
                    // lower the tier to the task's. When the cache answered,
                    // its answer decides (W4-1); `current_tier` alone read C2
                    // over a single-pass body published after the optimizing
                    // one was withdrawn or retired, and the method was then
                    // never promoted again.
                    let kept_optimizing = match live {
                        Some(body) => {
                            body == LiveEntryBody::Optimizing
                                && !tier_uses_optimized_backend(c.tier)
                        }
                        None => {
                            tier_uses_optimized_backend(state.current_tier)
                                && !tier_uses_optimized_backend(c.tier)
                                && keep_live_optimizing_body_enabled()
                        }
                    };
                    if !c.osr && !kept_optimizing {
                        state.current_tier = c.tier;
                        self.note_time_to_tier(state, c.tier);
                    } else if kept_optimizing
                        && live.is_some()
                        && !tier_uses_optimized_backend(state.current_tier)
                    {
                        // The cache runs an optimizing body the tier does not
                        // name (a demotion raced its republication). Left
                        // below C2, the method would be offered single-pass
                        // compiles that `put` keeps refusing.
                        state.current_tier = CompilationTier::C2;
                        self.note_time_to_tier(state, CompilationTier::C2);
                    }
                    // Clear the budget of the pipeline that actually published.
                    // An OSR publish is evidence about a loop-entry artifact and
                    // says nothing about whether method-entry codegen works, so
                    // it must not clear `tier_fail_count` -- when it did, that
                    // counter could never saturate for a method whose OSR
                    // compiles succeed and whose method-entry compiles do not,
                    // and the give-up arm it feeds was unreachable. See
                    // `MethodState::osr_fail_count`.
                    if c.osr {
                        state.osr_fail_count = 0;
                    } else {
                        state.tier_fail_count = 0;
                        // Wave 116, L2: a published body ends the wait.
                        state.waiting_on_class_epoch = None;
                        state.waiting_on_class_offers = 0;
                    }
                } else if c.declined_permanently {
                    // Policy verdict, not a compile failure: the skip list, the
                    // OSR-denial set and a contained panic are verdicts that
                    // re-asking cannot change. Record it once and stop; do NOT
                    // spend `tier_fail_count`, which exists to bound genuinely
                    // failing codegen. See `MethodState::ineligible`.
                    //
                    // r11-tier: an OSR decline is a verdict about the OSR door
                    // only. The VM's two OSR decline arms are the OSR-denial
                    // set and a bail-listing it has just mirrored into that set,
                    // and both EXPIRE with the install epoch; `ineligible` never
                    // does, and it also closes method-entry compiles. So an OSR
                    // decline spends one of the OSR pipeline's own retries
                    // instead — which still bounds a method whose OSR compiles
                    // keep panicking — and leaves method entry open.
                    if c.osr {
                        state.osr_fail_count = state.osr_fail_count.saturating_add(1);
                    } else {
                        state.ineligible = true;
                    }
                } else if !c.epoch_moved && (c.cap_refused || crate::jit_code_cache_at_capacity()) {
                    // JIT round 22 wave 11, lane runtime (page
                    // `r22-w7-code-cache-sweep-phases-bistable-FIXED-20261020.md`):
                    // `c.cap_refused` first. `jit_code_cache_at_capacity` DRAINS
                    // the retirement queue before it answers, and with the sweep
                    // driver's own windowed drain (wave 8) room routinely comes
                    // back between the worker's gate refusal and this question:
                    // the cap read "not full", the refusal fell through to the
                    // charging arm below, and three such landings settled a
                    // phase method for good -- a timing race (a slower worker,
                    // e.g. under `CRATONVM_JIT=print-compilation`, lost it less
                    // often). The witness says what the gate saw.
                    // r11-tier: the code-cache cap, reached between `admit`
                    // (which defers at the cap instead of charging) and this
                    // compile's publication. The worker's `compile_gate::admit`
                    // refuses at the cap and the VM reports an unpublished,
                    // not-declined compile, which the arm below charged like
                    // broken codegen — three such landings settled a healthy
                    // method for good, the exact transient-made-sticky loss
                    // `MethodState::deferred_at_cap` exists to prevent, just
                    // in the queue-to-dispatch window. Deferred like `admit`
                    // does, so the pressure clearing re-offers it. Same lock
                    // order as `admit`, which also asks this under `methods`.
                    state.deferred_at_cap = true;
                    deferred_at_cap = true;
                    self.refused_at_cap.fetch_add(1, Ordering::Relaxed);
                } else if let Some(epoch) = c.waiting_on_class.filter(|_| {
                    !c.osr && state.waiting_on_class_parks < MAX_WAITING_ON_CLASS_PARKS
                }) {
                    // Interpreter round i6 wave 116, lane L2 (page `i107-L2`):
                    // the compile was waiting on a class that is not loaded
                    // (or initialized) yet. Not a compiler failure, so not
                    // charged; parked until a class is defined
                    // (`waiting_on_class_refuses`). Bounded per method. Asked
                    // before the epoch-moved arm: either way nothing is
                    // charged, and this one also stops the re-asks.
                    state.waiting_on_class_parks += 1;
                    state.waiting_on_class_epoch = Some(epoch);
                    state.waiting_on_class_offers = 0;
                    self.parked_waiting_on_class.fetch_add(1, Ordering::Relaxed);
                    deferred_at_cap = true;
                } else if !c.epoch_moved {
                    // Charged to the failing pipeline's own budget.
                    if c.osr {
                        state.osr_fail_count = state.osr_fail_count.saturating_add(1);
                    } else {
                        state.tier_fail_count = state.tier_fail_count.saturating_add(1);
                    }
                }
                // `else`: the install epoch moved under this compile, so the
                // unpublished outcome is the flush barrier refusing a body
                // built against a world that no longer exists -- not a
                // compiler failure. Charging it let three redefinitions (or
                // code-cache flushes) landing during warmup settle a healthy
                // method for good, the same silent loss `retire_stale`
                // refuses to cause for a request dropped BEFORE dispatch.
                // Releasing the slot below is the whole state change; the
                // next invocation re-admits against the current bytecode.
                self.release_slot_for(state, c.tier, c.osr_bci, c.ticket);
                // The uncharged epoch-moved arm loses a C2 request the same way
                // a stale drop does; see `readmit_dropped_optimizing_request`.
                if c.epoch_moved && !c.success && !c.declined_permanently {
                    self.readmit_dropped_optimizing_request(state, c.tier, c.osr_bci);
                }
                state.last_compile_time_ms = c.compile_time_ms;
                // jit-inlining-and-ir-calls — tier-4 compile-time guard.
                //
                // The optimizing IR pipeline's admission rule was widened
                // substantially on 2026-07-26: the invoke / field / static-field
                // caps went from 5 to 64 and the bytecode-size cap from 200 to
                // HotSpot's 8000-byte HugeMethodLimit, so the population of
                // methods reaching C2 is now ordinary application code rather
                // than a handful of small arithmetic kernels. `ir_compatible`
                // and `ir::IR_MAX_GRAPH_NODES` bound the *static* inputs to a
                // compile, but nothing bounded the OBSERVED cost.
                //
                // This closes that: a method whose C2 compiles keep costing more
                // than `MAX_C2_COMPILE_TIME_MS` is not re-attempted at C2 and
                // degrades to C1. Reusing `c2_bailout` rather than adding new
                // state is deliberate: every existing degradation path already
                // consults it (`should_compile`, `request_osr`,
                // `request_c2_upgrade`), so the demotion is coherent with the
                // trap-driven one by construction.
                //
                // Only a compile that SUCCEEDED, at method entry, is charged. A
                // slow failure is already bounded by `tier_fail_count` and
                // charging it too punished a method twice for one attempt; an
                // OSR compile builds a different artifact (a loop-entry body)
                // and says nothing about what the method-entry compile costs.
                // The budget is compile-thread CPU time where the platform
                // reports it -- a descheduled worker is not a slow compile --
                // and a method gets `C2_BUDGET_OVERRUNS_BEFORE_DEMOTION`
                // overruns, not one, before it is demoted.
                if c.success
                    && !c.osr
                    && c.tier == CompilationTier::C2
                    && c.budget_ms > MAX_C2_COMPILE_TIME_MS
                    && !state.c2_bailout
                {
                    state.c2_budget_overruns = state.c2_budget_overruns.saturating_add(1);
                    if state.c2_budget_overruns >= C2_BUDGET_OVERRUNS_BEFORE_DEMOTION {
                        state.c2_bailout = true;
                        self.stats.c2_bailouts.fetch_add(1, Ordering::Relaxed);
                    }
                }
                // The hotness door into C2, asked on the C1 publish because
                // nothing will ask on a later invocation — the call sites that
                // would have are, from this instant, routing through the body
                // that was just published. See `promote_hot_c1_to_c2` and the
                // `c2_hotness_bar` field for the chain and the evidence.
                //
                // Last inside the lock scope, so it sees the state every other
                // effect of this completion has already had: the slot released
                // (or `admit` would refuse it as a duplicate), `current_tier`
                // advanced, `tier_fail_count` cleared, and any `c2_bailout` the
                // budget guard above decided already set.
                //
                // OSR completions are excluded by the tier test alone — an OSR
                // task always targets C2 (`request_osr`), so `c.tier` is never
                // C1-family for one — but `!c.osr` is spelled out anyway,
                // because relying on that coincidence is how the `c2=` stat
                // came to count OSR compiles as method-entry C2 compiles.
                if c.success
                    && !c.osr
                    && matches!(
                        c.tier,
                        CompilationTier::C1 | CompilationTier::C1WithProfiling
                    )
                {
                    self.promote_hot_c1_to_c2(state);
                }
                // JIT r23 wave 11 (`CRATONVM_JIT_C2_AFTER_OPTIMIZING_OSR`,
                // opt-in): an optimizing OSR body proves a hot loop, but a
                // single-pass entry body never migrates into it, so each call
                // runs its whole loop in the single-pass body until the drain
                // clock (up to 200 ms) moves the count over the C2 bar. Both
                // orders: the OSR body lands while a C1 entry body is live, or
                // the C1 entry body lands after the OSR body.
                if c.success && tier_uses_optimized_backend(c.tier) && c.osr {
                    state.optimizing_osr_published = true;
                }
                if c.success
                    && state.optimizing_osr_published
                    && (c.osr
                        || matches!(
                            c.tier,
                            CompilationTier::C1 | CompilationTier::C1WithProfiling
                        ))
                    && c2_after_optimizing_osr_enabled()
                {
                    self.promote_c1_after_optimizing_osr(state);
                }

                // JIT round 21 lane tiermgr (INDYCONST21-1): last, so a
                // request this completion admitted (the promotion above, the
                // epoch re-admission) is seen holding the slot. Only an
                // optimizing method-entry compile can answer an owed C2 retry;
                // an OSR completion leaves the method-entry slot's debt alone.
                if !c.osr {
                    let answered_from =
                        tier_uses_optimized_backend(c.tier).then_some(dispatched_at);
                    owed_retry = Self::take_unanswered_retry(state, answered_from);
                }
            }
        }
        if c.success && c.osr {
            // Counted HERE, on a published OSR artifact, beside the c1/c2
            // counts it is printed with (`compiles: c1= c2= osr=`). It used to
            // be counted in `request_osr` when the request was QUEUED, so a
            // request later dropped at a stale epoch, dropped by a counted
            // trap, abandoned at shutdown or failed by the backend still read
            // as a compile -- the one row of that line that measured intent
            // rather than outcome.
            self.stats.osr_compilations.fetch_add(1, Ordering::Relaxed);
        }
        // zt t15/pl6: the last compile the three counts above and below
        // count (`sun.ci.lastMethod` / `lastType`): HotSpot sets both at the
        // end of each successful compile (`CompileBroker::collect_statistics`).
        // One uncontended lock per published body.
        let counted_kind = c.osr
            || matches!(
                c.tier,
                CompilationTier::C1 | CompilationTier::C1WithProfiling | CompilationTier::C2
            );
        // zt pl4 item 1: the callback's sizes belong to this task only.
        let sizes = take_published_sizes();
        if c.success && counted_kind {
            // zt pl4 item 1: `sun.ci.standardBytes` / `osrBytes` /
            // `lastSize` / `nmethodSize` / `nmethodCodeSize`, booked with the
            // counts above (`collect_statistics`' success arm).
            if let Some(sizes) = sizes {
                let bytes = if c.osr {
                    &self.stats.osr_bytes
                } else {
                    &self.stats.standard_bytes
                };
                bytes.fetch_add(sizes.bytecode_bytes, Ordering::Relaxed);
                self.stats
                    .last_size
                    .store(sizes.bytecode_bytes, Ordering::Relaxed);
                self.stats
                    .nmethod_size
                    .fetch_add(sizes.total_size, Ordering::Relaxed);
                self.stats
                    .nmethod_code_size
                    .fetch_add(sizes.insts_size, Ordering::Relaxed);
            }
            *self.stats.last_compiled.lock() = Some(LastCompiled {
                class_name: Arc::clone(&key.class_name),
                method_name: Arc::clone(&key.method_name),
                osr: c.osr,
            });
            // zt t18/pl8: `sun.ci.standardTime` / `osrTime`, HotSpot's
            // per-kind times of the successful compiles only
            // (`CompileBroker::collect_statistics`).
            let per_kind = if c.osr {
                &self.stats.osr_compile_time_ns
            } else {
                &self.stats.standard_compile_time_ns
            };
            per_kind.fetch_add(c.compile_time_ns, Ordering::Relaxed);
        }
        // zt t18/pl8: `sun.ci.totalBailouts` / `lastFailedMethod` /
        // `lastFailedType`: a compile that ran and published nothing (a
        // backend failure, or an install the epoch or a moved dependency
        // refused: HotSpot's `register_method` dependency failure is a
        // bailout too). Not a decline (HotSpot makes no task for a
        // prohibited method) and not the code-cache-cap deferral. An
        // optimizing attempt that bailed inside the IR pipeline and fell back
        // to a published single-pass body is a successful task: HotSpot
        // counts each task once (compiled, bailed or invalidated).
        if is_compile_bailout(&c, deferred_at_cap) {
            self.stats.total_bailouts.fetch_add(1, Ordering::Relaxed);
            *self.stats.last_failed.lock() = Some(LastCompiled {
                class_name: Arc::clone(&key.class_name),
                method_name: Arc::clone(&key.method_name),
                osr: c.osr,
            });
        }
        if c.success {
            match c.tier {
                CompilationTier::C1 | CompilationTier::C1WithProfiling => {
                    self.stats.c1_compilations.fetch_add(1, Ordering::Relaxed);
                }
                // `&& !c.osr` added 2026-09-16. An OSR task ALWAYS targets C2
                // (`request_osr`), so without the test one OSR compile landed
                // in both `osr_compilations` and `c2_compilations` and the
                // exit dump read `compiles: c1=2 c2=1 osr=1` for a run with
                // ZERO method-entry C2 compiles. That line was read as evidence
                // the optimizing tier had engaged, when it said the opposite;
                // `osr_compilations` counts the OSR arm just above, so this
                // is the double count, not a lost one.
                CompilationTier::C2 if !c.osr => {
                    self.stats.c2_compilations.fetch_add(1, Ordering::Relaxed);
                }
                _ => {}
            }
        }
        self.stats
            .total_compile_time_ms
            .fetch_add(c.compile_time_ms, Ordering::Relaxed);
        // zt t18/pl8: every attempt, as `total_compile_time_ms`, in
        // nanoseconds (`java.ci.totalTime`, `CompilationMXBean`).
        self.stats
            .total_compile_time_ns
            .fetch_add(c.compile_time_ns, Ordering::Relaxed);
        self.completed.fetch_add(1, Ordering::Release);
        owed_retry
    }

    /// JIT round 21 lane tiermgr (INDYCONST21-1): at an exit of the request
    /// holding `state`'s method-entry slot, settle the owed C2 retry
    /// ([`MethodState::retry_owed`]). `answered_from` is the retry generation
    /// the exit's compile was dispatched at when it was an OPTIMIZING compile,
    /// `None` for an exit that built no optimizing body (a single-pass
    /// compile, a stale or overtaken drop).
    ///
    /// * dispatched at or after the owed stamp: answered (the compile read the
    ///   state the request was about) -- cleared, nothing to ask;
    /// * the slot is held again (a request admitted after the stamp: the C1
    ///   publish's promotion, an epoch re-admission): kept, that request's own
    ///   exit decides;
    /// * otherwise: taken, and its door returned for the caller to ask again
    ///   once `methods` is released ([`Self::ask_owed_retry`]).
    ///
    /// Call with `methods` held, after the slot release and any re-admission.
    fn take_unanswered_retry(
        state: &mut MethodState,
        answered_from: Option<u64>,
    ) -> Option<&'static str> {
        let (stamp, door) = state.retry_owed?;
        if answered_from.is_some_and(|dispatched_at| dispatched_at >= stamp) {
            state.retry_owed = None;
            if cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_JITC") {
                eprintln!(
                    "[cratonvm-jitc] {door} retry owed {}.{}{}: answered by the optimizing \
                     compile dispatched after it",
                    state.method_key.class_name,
                    state.method_key.method_name,
                    state.method_key.descriptor
                );
            }
            return None;
        }
        if state.queued_for_compilation {
            return None;
        }
        state.retry_owed = None;
        Some(door)
    }

    /// The owed retry a method-entry request DROPPED before it compiled (a
    /// stale drop, an overtaken drop) leaves behind, to ask once `methods` is
    /// free: [`Self::take_unanswered_retry`]'s, else the late-bind owed bit
    /// ([`MethodState::late_bind_retry_owed`]) as the `late-bind-owed` door.
    /// Call AFTER the slot was released.
    ///
    /// t1 wave 11 lane JTIER (review, `CRATONVM_JIT_LATE_BIND_RETRY_OWED_ON_DROP`,
    /// default ON, `=0` the old behaviour): the bit was taken only by the
    /// worker after a COMPILED task's `finish`, so a holder that left by a
    /// drop left it set with nobody to ask -- the late-bind retry it owed was
    /// lost until some later compile of the method happened to finish, and a
    /// method whose calls already run a published body (the overtaken drop's
    /// case) may never compile again. While the slot is held again (the stale
    /// drop's re-admission) the bit stays for that compile's completion, which
    /// was dispatched after the request and takes it. One ask covers both
    /// debts: any C2 retry dispatched after them answers either.
    fn take_dropped_owed_retry(state: &mut MethodState) -> Option<&'static str> {
        let owed = Self::take_unanswered_retry(state, None);
        if state.queued_for_compilation || !late_bind_retry_owed_on_drop_enabled() {
            return owed;
        }
        let late_bind = std::mem::take(&mut state.late_bind_retry_owed);
        owed.or(late_bind.then_some("late-bind-owed"))
    }

    /// JIT round 21 lane tiermgr (INDYCONST21-1; re-landed t1 wave 8): ask an
    /// owed C2 retry that the request holding the slot did not answer
    /// ([`Self::take_unanswered_retry`]). Call with `methods` NOT held.
    fn ask_owed_retry(&self, key: &MethodKey, door: &'static str) -> C2RetryAnswer {
        // Not at shutdown: nothing would compile it, and the shutdown drain
        // must leave the queues empty.
        if self.shutdown.load(Ordering::Acquire) {
            return C2RetryAnswer::Refused;
        }
        if cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_JITC") {
            eprintln!(
                "[cratonvm-jitc] {door} retry owed {}.{}{}: the request that held the slot \
                 did not answer it; asked again",
                key.class_name, key.method_name, key.descriptor
            );
        }
        self.request_c2_retry(key, door)
    }
}

/// Result of one background compile attempt, reported by the VM's
/// [`CompileFn`] callback.
#[derive(Debug, Clone, Copy)]
pub struct CompileOutcome {
    /// Wall-clock compile time in milliseconds.
    pub compile_time_ms: u64,
    /// `true` only if the attempt actually published a compiled body (see
    /// [`CompilerCore::finish`] for why this must not be conflated
    /// with "the task was processed").
    pub published: bool,
    /// C1→C2 supersede: the VM judged this method would take the optimizing
    /// IR pipeline at C2 AND is expected to benefit (see the VM-side
    /// `c2_upgrade_would_engage` predicate). After a successful C1-family
    /// publish the worker loop enqueues a Low-priority C2 recompile whose
    /// publish REPLACES the C1 body in the jit cache.
    pub c2_upgrade_candidate: bool,
    /// This compile's IR build bailed on a `new` whose class was not loaded
    /// yet, and the method is owed exactly one more optimizing attempt.
    ///
    /// Distinct from [`Self::c2_upgrade_candidate`], which is the C1->C2
    /// promotion and is refused for a task that was ALREADY at an optimized
    /// tier. This one has to survive that refusal: the bail it answers happens
    /// *inside* a C2 task, which then falls through to the single-pass backend
    /// and leaves the method recorded as done with C2.
    pub deferred_new_retry: bool,
    /// The attempt did not publish because the VM *declined* the method on
    /// grounds that are fixed for the life of the process (skip list, OSR
    /// denial) — as opposed to a compile that ran and failed.
    ///
    /// Set this and the tier manager records the decision once
    /// ([`MethodState::ineligible`]) instead of spending the method's
    /// `tier_fail_count` retry budget on a verdict that cannot change. Ignored
    /// when `published` is true.
    pub declined_permanently: bool,
    /// An optimizing task compiled a body that the cache's `put` refused (a baked
    /// direct-call target retired before publication, a moved dependency), while
    /// a single-pass body stayed live under the key. `published` is then false,
    /// so the tier is not recorded as C2, and the worker re-asks C2 for the
    /// method (`request_c2_retry`, door `refused-publication`). The tier-failure
    /// budget that the unpublished completion spends bounds the retries.
    pub optimizing_publication_refused: bool,
}

impl CompileOutcome {
    /// A compile that ran and failed — spends one retry.
    pub fn failed(compile_time_ms: u64) -> Self {
        Self {
            compile_time_ms,
            published: false,
            c2_upgrade_candidate: false,
            deferred_new_retry: false,
            declined_permanently: false,
            optimizing_publication_refused: false,
        }
    }

    /// The VM refused the method on policy grounds — recorded once, never
    /// retried, and not counted as a compile failure.
    pub fn declined(compile_time_ms: u64) -> Self {
        Self {
            compile_time_ms,
            published: false,
            c2_upgrade_candidate: false,
            deferred_new_retry: false,
            declined_permanently: true,
            optimizing_publication_refused: false,
        }
    }
}

/// A compile callback invoked on a worker thread for each drained task.
/// The actual codegen is supplied by the VM at startup; `tiered.rs` only owns
/// the scheduling. `Sync` because every worker of both lanes shares one.
pub type CompileFn = Box<dyn Fn(&CompilationTask) -> CompileOutcome + Send + Sync + 'static>;

/// Work an idle compile worker does on a timer (round 11 wave 4). The VM
/// installs one through [`TieredCompilationManager::set_idle_tick`]: the
/// compiled-entry counter drain.
pub type IdleTickFn = Box<dyn Fn() + Send + Sync + 'static>;

/// Which backend produced the method-entry body a VM's code cache holds under
/// a method's exact key (tier proposal W4-1, round 12 wave 5). See
/// [`TieredCompilationManager::set_live_entry_probe`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveEntryBody {
    /// The single-pass backend.
    SinglePass,
    /// The optimizing pipeline.
    Optimizing,
}

/// The VM's reader of its own code cache for [`LiveEntryBody`]. `None` when it
/// cannot tell: nothing published under the key's exact identity (a key with
/// no class id, a body published under another class's key), or the VM gone.
pub type LiveEntryProbe = Box<dyn Fn(&MethodKey) -> Option<LiveEntryBody> + Send + Sync + 'static>;

/// The VM's note of an admitted request, `true` for an optimizing or OSR one
/// (CONF21-1). See [`TieredCompilationManager::set_compile_admission_sink`].
pub type CompileAdmissionSink = Box<dyn Fn(&MethodKey, bool) + Send + Sync + 'static>;

/// How long an idle C1-lane worker waits for a task before it runs the idle
/// tick. It bounds how late a method that crossed the C2 hotness bar in
/// compiled code is noticed once nothing else ticks: 200 ms is well below a
/// probe's run and costs five short wakes a second while the lane is empty.
const IDLE_TICK: std::time::Duration = std::time::Duration::from_millis(200);

/// For a key built WITHOUT class identity (`class_id` 0, `MethodKey::new`) that
/// matches no tracked state: the key of the ONE tracked state with the same
/// class name, method name and descriptor. `None` for a key that has an
/// identity or an exact state, and for a name two or more states share (two
/// loaders defining the class) — the owner is then unknowable by name.
///
/// The VM's by-name doors (`DeoptimizationController::deoptimize`, the deferred
/// `new` re-offer) build their keys through `get_loaded_class_id(..)
/// .unwrap_or(0)`, and `MethodKey` hashes the id, so without this their events
/// reached no state at all. O(tracked methods), and only on that miss.
fn identify_unidentified_key(
    methods: &FxHashMap<MethodKey, MethodState>,
    key: &MethodKey,
) -> Option<MethodKey> {
    if key.class_id != ClassId::new(0) || methods.contains_key(key) {
        return None;
    }
    let mut named = methods.keys().filter(|k| {
        k.class_name == key.class_name
            && k.method_name == key.method_name
            && k.descriptor == key.descriptor
    });
    match (named.next(), named.next()) {
        (Some(only), None) => Some(only.clone()),
        _ => None,
    }
}

/// `CRATONVM_JIT_KEEP_OPTIMIZING_BODY` (default ON; `0`/`false`/`off`/`no`
/// turns it off): a single-pass publish never replaces a live optimizing body
/// (`CompiledMethod::keep_live_optimizing_body`, set by the VM's compile door),
/// and a single-pass completion never lowers an optimizing `current_tier`
/// (tier proposal R12-4). Read per call; no process static.
pub fn keep_live_optimizing_body_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_KEEP_OPTIMIZING_BODY")
}

/// `CRATONVM_JIT_TRAP_RECOMPILE_KEEPS_C2` (JIT r25 wave 2, lane regex rows;
/// perf, **default ON since j27 2026-10-28**, `=0` kills): the VM's eager re-queue after a trap evicted a
/// method body (`DeoptimizationController::deoptimize_in`,
/// `RecompileAndReinterpret`) asks for C2 when the evicted body was an
/// OPTIMIZING one, instead of always C1. The trap's speculation is withdrawn
/// (an `UnstableIf` after one charge, the de-spec registry per bci) by the
/// resume sink AFTER this re-queue, so an idle worker can read the registry
/// and the branch row before either is written and re-prune the same branch
/// (j27 wave 5, lane TRAPPROF: page
/// `j27-trap-recompile-reads-the-profile-before-the-resume-records-FIXED-20261029.md`;
/// `CRATONVM_JIT_PROF_BRANCH_TRAP_BIT` sets the bci's trap bit before the
/// re-queue, `crate::profile::ProfileStore::note_trap`). Four such charges at
/// one `(reason, bci)` set `c2_bailout` for the life of the method
/// ([`PER_BCI_TRAP_LIMIT`]; decay only slows the count). Before, the C1
/// re-queue put the method back on the single-pass body, which had to
/// re-cross the C2 hotness bar -- and a leaf reached only from other compiled
/// bodies (`ASCII.isWord` from the `\w` lambda) does not re-cross it
/// (r25 regex-groups: `ASCII.isWord C1 -> C2`, then `C1 -> C1`, `[c1]` 2.3 %).
/// Read once per evicting trap.
pub fn trap_recompile_keeps_c2_enabled() -> bool {
    // WARNING j27-user-flip-20261028 (default ON; =0 restores the old default)
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_TRAP_RECOMPILE_KEEPS_C2")
}

/// `CRATONVM_JIT_OUTSIDE_C1_DRAIN_C2` (JIT r25 wave 4, lane regex rows; perf,
/// **default ON since j27 2026-10-28**, `=0` kills): a C1 body published OUTSIDE the manager (the callee
/// compiler, the eager chain, the synchronized callee door:
/// `MethodState::outside_c1_published`) is promoted to C2 once the entry-counter
/// drain reports [`OUTSIDE_C1_DRAIN_C2_BAR`] invocations of it.
///
/// Why: a manager-scheduled C1 publish gets an immediate `Low` C2 supersede
/// (`request_c2_upgrade`); an outside one does not (HANDLER8-1's opt-in
/// supersede also wants a warm interpreter profile, which an eager-chain
/// compile at ~31 calls never has), so its only way up is the drain against the
/// full `max(c2_threshold, c2_min_invocations)` bar, 20 000 by default. A
/// constructor reached only through a compiled `new` -- `Pattern.<init>(String,
/// I)`, once per `Pattern.compile` -- then stays single-pass for most of a run
/// (r25 `compile` row: only `optimize=false` admissions of `Pattern.<init>` in
/// every debug run). 5 000 drained COMPILED entries is HotSpot's
/// `Tier4InvocationThreshold`. The request opens the same branch-profile
/// window every other C2 upgrade opens. Risk: the interpreter profile behind
/// such a body may be thin, so its C2 compile may prune arms that later trap
/// (bounded by the trap limits and the de-spec registry).
/// Read per drained report of such a method.
pub fn outside_c1_drain_c2_enabled() -> bool {
    // WARNING j27-user-flip-20261028 (default ON; =0 restores the old default)
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_OUTSIDE_C1_DRAIN_C2")
}

/// The drained entry count at which [`outside_c1_drain_c2_enabled`] promotes
/// an outside-manager C1 body: HotSpot's `Tier4InvocationThreshold`.
pub const OUTSIDE_C1_DRAIN_C2_BAR: u64 = 5_000;

/// `CRATONVM_JIT_TIER_RETRY_OWED` (JIT round 21 lane tiermgr, INDYCONST21-1 of
/// `jit-r21-indyconst-proposals.md`; **default ON**, `=0` off): a C2 retry that
/// finds the method-entry slot held is remembered in the manager
/// ([`MethodState::retry_owed`]) instead of being dropped, for EVERY retry door
/// (late-bind, indy constant / mutable guard, deferred `new`, static
/// self-lock, OSR-unenterable, refused publication). The slot holder answers
/// it when it is an optimizing compile dispatched after the request; any
/// other exit asks the retry again. Before, each VM door decided for itself
/// (late-bind kept a mark, EAGER19-3 owed behind an opt-in stamp, round 21
/// owed two latches through `late_bind_owed`, the indy sites re-asked from the
/// bridge), and a door that dropped the answer lost the request whenever the
/// queued compile had read the method before the event. Read only on an
/// `AlreadyQueued` answer (cold).
pub fn tier_retry_owed_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_TIER_RETRY_OWED")
}

/// Whether a target tier should use the **optimized** (C2-equivalent) backend.
///
/// wire-tiered-manager increment 2 / Step 3: this is the policy half of the
/// C1/C2 backend split. `C1`/`C1WithProfiling` map to the fast single-pass
/// (no-opt) backend; `C2` (and the `FullProfile` collection tier, which only
/// reaches codegen as a C2 promotion) map to the optimizing pipeline.
///
/// The *codegen* half lives VM-side (the `cratonvm-jit` crate cannot reference
/// `SharedVm` or the interpreter's compile entry points), so the VM-supplied
/// [`CompileFn`] consumes this to pick its compile strategy. As of
/// wire-tiered-manager Step 3 this drives **real backend routing**, not an
/// advisory hint: `background_compile_task` threads the returned boolean into
/// `jit::try_compile`'s trailing `optimize` flag — `true` runs the optimizing
/// IR pipeline, `false` skips it and routes to the single-pass `x64::compile`
/// (C1) backend.
#[inline]
pub fn tier_uses_optimized_backend(tier: CompilationTier) -> bool {
    matches!(tier, CompilationTier::C2 | CompilationTier::FullProfile)
}

thread_local! {
    /// Set on every compile worker thread.
    ///
    /// [`BackgroundCompiler::shutdown`] reads it so it never joins the thread
    /// it is running on: a worker upgrades a `Weak<SharedVm>` per task, can end
    /// up holding the last `Arc`, and then drops the VM -- and with it this
    /// handle -- on the worker itself.
    static ON_COMPILE_WORKER: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Handle to one manager's compile workers.
///
/// Dropping the handle (or calling [`BackgroundCompiler::shutdown`]) signals the
/// workers to finish and joins them, so no compile thread outlives its VM.
pub struct BackgroundCompiler {
    core: Arc<CompilerCore>,
    handles: Vec<JoinHandle<()>>,
}

impl BackgroundCompiler {
    /// Request shutdown and join the worker threads.
    pub fn shutdown(&mut self) {
        // Store the flag, then take and release each lane's lock before
        // notifying it. Without the lock a lost-wakeup race deadlocks `join()`:
        // a worker can load `shutdown == false` (under its lane lock), this
        // thread then stores `true` + `notify_all` before the worker reaches
        // `wake.wait()`, and the worker parks AFTER the notify and never wakes.
        // Acquiring the lane lock forces this thread to wait until such a
        // worker has parked (`parking_lot::Condvar::wait` registers the waiter
        // before releasing the lock), so the notify that follows reaches it; a
        // worker that loads the flag after the store sees `true`.
        self.core.shutdown.store(true, Ordering::Release);
        for lane in &self.core.lanes {
            drop(lane.queue.lock());
            lane.wake.notify_all();
            // zs s9/rs2: parked standby workers check the flag under this
            // lock before waiting, so the same lock-then-notify reaches them.
            drop(lane.standby_promoted.lock());
            lane.standby_wake.notify_all();
        }
        let on_worker = ON_COMPILE_WORKER.try_with(|w| w.get()).unwrap_or(false);
        for handle in self.handles.drain(..) {
            if on_worker {
                // Detach instead: joining could wait on this very thread. The
                // workers see the flag and exit on their own.
                continue;
            }
            let _ = handle.join();
        }
        // Before `active` drops, so a restart installs its own tick rather
        // than having it cleared here. The tick holds a `Weak` of the VM and
        // the core can outlive it (a detached worker's `Arc`).
        *self.core.idle_tick.lock() = None;
        // j27 w6 T1TIERMGR: the accept-timing timer holds the VM too.
        self.core.accept_timing.clear();
        self.core.active.store(false, Ordering::Release);
        // Whatever was still queued is abandoned here. That is correct at
        // teardown, but it is still a set of compilation requests that will
        // never be serviced, so it is counted rather than left to be inferred
        // from a queue that simply stopped moving. A non-zero
        // `queue_shutdown_abandoned` in the middle of a run says the workers
        // were stopped with work outstanding, which is a different — and worse
        // — story than the same number at exit.
        //
        // Drained (not merely counted) so a restarted worker cannot resume
        // requests stamped at a pre-teardown epoch, and so the count and the
        // queue can never disagree. Joined first: the workers own the queues
        // until then.
        let abandoned: Vec<(CompilationTask, u64)> = self
            .core
            .lanes
            .iter()
            .flat_map(|lane| lane.queue.lock().drain_all())
            .collect();
        if !abandoned.is_empty() {
            let stale: Vec<StaleRequest> = abandoned
                .into_iter()
                .map(|(task, ticket)| StaleRequest {
                    task,
                    event: crate::metrics::SCHEDULING_EVENTS[3],
                    queued_epoch: 0,
                    current_epoch: 0,
                    ticket,
                })
                .collect();
            self.core.retire_stale(&stale);
        }
    }
}

impl Drop for BackgroundCompiler {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Diagnostic-only handle to the FIRST [`CompilerCore`] this process built —
/// see [`TieredCompilationManager::new`] and [`dump_method_stats_to_stderr`].
///
/// Not a scheduling input. Workers, queues and verdicts all live on the
/// manager, so a second VM in the process is fully functional; it is merely
/// absent from this one exit-time dump.
static DIAG_CORE: std::sync::OnceLock<DiagCore> = std::sync::OnceLock::new();

/// What [`DIAG_CORE`] keeps of the first core: a WEAK handle, that manager's
/// `c1_threshold`, and the core's report once the core is gone.
///
/// Weak since interpreter round i1 wave 15 (lane L4): a strong `Arc` kept the
/// first VM's whole tiering state — every counted method, both queues, its
/// verdict registry — alive for the life of the process, after that VM was
/// dropped (an embedder that creates and destroys VMs; the unit-test binary).
/// The launcher's normal-return arm reaches the dump after `run()` has dropped
/// its `Vm`, so a core released before the dump leaves its report in
/// `retired` from [`CompilerCore`]'s `Drop` — only while the report is armed
/// (`flags().jit.method_stats`), so an unarmed drop pays one pointer compare.
struct DiagCore {
    core: std::sync::Weak<CompilerCore>,
    /// That manager's own `c1_threshold`: every later manager (a second VM,
    /// each test) overwrote a process-wide copy until interpreter round i1
    /// wave 14, so the dump judged the first VM's methods against the
    /// last-built policy.
    c1_threshold: u64,
    /// The core's report, taken as it dropped (see the type's doc).
    retired: Mutex<Option<CoreStatsReport>>,
}

impl DiagCore {
    fn new(core: &Arc<CompilerCore>, c1_threshold: u64) -> Self {
        Self {
            core: Arc::downgrade(core),
            c1_threshold,
            retired: Mutex::new(None),
        }
    }

    /// Whether `core` is the one this slot reports.
    fn is_for(&self, core: &CompilerCore) -> bool {
        std::ptr::eq(self.core.as_ptr(), core)
    }

    /// Keep `core`'s report for a dump that runs after it is gone.
    fn retire(&self, core: &CompilerCore) {
        let report = CoreStatsReport::of(core, self.c1_threshold);
        *self.retired.lock() = Some(report);
    }

    /// The report of the live core, else the one it left as it dropped.
    fn report(&self) -> Option<CoreStatsReport> {
        match self.core.upgrade() {
            Some(core) => Some(CoreStatsReport::of(&core, self.c1_threshold)),
            None => self.retired.lock().take(),
        }
    }
}

impl Drop for CompilerCore {
    fn drop(&mut self) {
        // HANDLER8-1: this VM's census line, unless the exit hook printed it.
        self.outside_c2.print_once();
        // See `DiagCore`: only the reported core, and only when armed.
        if let Some(diag) = DIAG_CORE.get() {
            if diag.is_for(self) && cratonvm_types::flags().jit.method_stats {
                diag.retire(self);
            }
        }
    }
}

/// One `hot_but_stuck` row of [`dump_method_stats_to_stderr`]:
/// `(invocations, queued, tier_fail_count, ineligible, display name, the
/// refusal site the compiler recorded for it)`.
type HotButStuckRow = (u64, bool, u32, bool, String, String);

/// One `jit-read-path-census` row (i73-L5): `(invocations, tier, deopts,
/// tier_fail_count, ineligible, queued, display name, recorded refusal)`.
type ReadPathRow = (u64, CompilationTier, u32, u32, bool, bool, String, String);

/// The class-path read path's JDK classes (JIT round 19 proposal wave 10, lane
/// zip10, i73-L5): what a `URLClassLoader` / app-loader class load runs between
/// the resource lookup and `defineClass` -- `URLClassPath` and its loaders,
/// `JarFile` / `ZipFile` and its `Source` / `ZipCoder` / `ZipUtils`, the
/// entry's `URL` and `ParseUtil.encodePath`, the bounds checks. A class name
/// is on the path when it starts with one of these (internal names; a prefix
/// ending in `/` is a package, any other an exact class or its nested ones).
pub const READ_PATH_CENSUS_PREFIXES: &[&str] = &[
    "java/util/zip/",
    "java/util/jar/",
    "jdk/internal/loader/",
    "java/net/URL",
    "java/net/URLClassLoader",
    "sun/net/www/ParseUtil",
    "sun/net/www/protocol/jar/",
    "sun/net/www/protocol/file/",
    "jdk/internal/util/Preconditions",
    "java/security/SecureClassLoader",
    "java/security/CodeSource",
];

/// Is `class_name` (internal form) on the class-path read path
/// ([`READ_PATH_CENSUS_PREFIXES`])? A non-package prefix matches the class
/// itself and its nested classes (`java/net/URL`, `java/net/URL$...`), not a
/// longer sibling name (`java/net/URLConnection` is not `java/net/URL`).
pub fn read_path_census_class(class_name: &str) -> bool {
    READ_PATH_CENSUS_PREFIXES.iter().any(|p| {
        if p.ends_with('/') {
            class_name.starts_with(p)
        } else {
            class_name
                .strip_prefix(p)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with('$'))
        }
    })
}

/// How many `jit-read-path-census` rows the report prints (the page asks for
/// the top 20 by interpreted invocations; the rest are summed).
const READ_PATH_CENSUS_ROWS: usize = 20;

/// `CRATONVM_DBG_JIT_READ_PATH_CENSUS` (default OFF; i73-L5): add the read-path
/// census to the method-stats report. Read when the report is built, no static.
fn read_path_census_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_JIT_READ_PATH_CENSUS")
}

/// Everything [`dump_method_stats_to_stderr`] reads from ONE core, read in
/// one pass so the report can outlive the core ([`DiagCore`]). Plain values:
/// the lines are rendered from these exactly as they were from the live core.
#[derive(Default)]
struct CoreStatsReport {
    c1_threshold: u64,
    snap: MethodPromotionSnapshot,
    /// Every Interpreter-tier method whose invocation count crossed
    /// `c1_threshold`, unsorted.
    hot_but_stuck: Vec<HotButStuckRow>,
    /// Per-method invocation counts for ranking the work that actually ran.
    top_methods: Vec<(u64, CompilationTier, String)>,
    ineligible_by_policy: u64,
    c1_compilations: u64,
    c2_compilations: u64,
    osr_compilations: u64,
    deoptimizations: u64,
    c2_bailouts: u64,
    total_compile_time_ms: u64,
    /// Per lane: `(dispatched, wait_ms_total, wait_ms_max)`.
    lanes: [(u64, u64, u64); 2],
    queued_now: usize,
    inflight_now: u64,
    osr_refused_in_flight: u64,
    /// `(n, ms_total, ms_max)`.
    nominate_to_first_body: (u64, u64, u64),
    /// `(n, ms_total, ms_max)`.
    first_body_to_c2: (u64, u64, u64),
    /// [`verdict_count_fields`] of the core's registry.
    verdict_fields: String,
    /// JIT round 19 (lane selflock19, SYNCDOOR618-2 / SDB17-2): the core's
    /// registry's self-lock census line, `None` when every row is zero.
    self_lock_census: Option<String>,
    /// JIT round 22 (lane sync, CODECACHE21-3): every method with a non-zero
    /// `c2_withdrawals`, as `(withdrawals, c2_bailout, display name)`,
    /// unsorted; the line prints the top five. Empty on a run with no charged
    /// optimizing-body withdrawal, and then nothing prints.
    withdrawals: Vec<(u32, bool, String)>,
    /// i73-L5 (lane zip10): every tracked read-path method, unsorted; empty
    /// unless `CRATONVM_DBG_JIT_READ_PATH_CENSUS` is on.
    read_path: Vec<ReadPathRow>,
}

impl CoreStatsReport {
    fn of(core: &CompilerCore, c1_threshold: u64) -> Self {
        let mut report = Self {
            c1_threshold,
            ..Self::default()
        };
        let read_path_census = read_path_census_enabled();
        {
            let snap = &mut report.snap;
            let methods = core.methods.lock();
            for state in methods.values() {
                if read_path_census && read_path_census_class(&state.method_key.class_name) {
                    report.read_path.push((
                        state.invocation_count,
                        state.current_tier,
                        state.deopt_count,
                        state.tier_fail_count,
                        state.ineligible || state.entry_ineligible,
                        state.queued_for_compilation,
                        format!(
                            "{}.{}{}",
                            state.method_key.class_name,
                            state.method_key.method_name,
                            state.method_key.descriptor
                        ),
                        core.verdicts
                            .bail_reason_for(
                                state.method_key.class_id,
                                &state.method_key.class_name,
                                &state.method_key.method_name,
                                &state.method_key.descriptor,
                            )
                            .unwrap_or_else(|| "-".to_string()),
                    ));
                }
                if state.c2_withdrawals > 0 {
                    report.withdrawals.push((
                        state.c2_withdrawals,
                        state.c2_bailout,
                        format!(
                            "{}.{}{}",
                            state.method_key.class_name,
                            state.method_key.method_name,
                            state.method_key.descriptor
                        ),
                    ));
                }
                report.top_methods.push((
                    state.invocation_count,
                    state.current_tier,
                    format!(
                        "{}.{}{}",
                        state.method_key.class_name,
                        state.method_key.method_name,
                        state.method_key.descriptor
                    ),
                ));
                snap.distinct_methods += 1;
                snap.total_invocations += state.invocation_count;
                if state.invocation_count > 0 {
                    snap.methods_ever_invoked += 1;
                }
                match state.current_tier {
                    CompilationTier::Interpreter => {
                        snap.methods_still_interpreted += 1;
                        if state.invocation_count >= c1_threshold {
                            if state.ineligible || state.entry_ineligible {
                                report.ineligible_by_policy += 1;
                            }
                            report.hot_but_stuck.push((
                                state.invocation_count,
                                state.queued_for_compilation,
                                state.tier_fail_count,
                                state.ineligible || state.entry_ineligible,
                                format!(
                                    "{}.{}{}",
                                    state.method_key.class_name,
                                    state.method_key.method_name,
                                    state.method_key.descriptor
                                ),
                                core.verdicts
                                    .bail_reason_for(
                                        state.method_key.class_id,
                                        &state.method_key.class_name,
                                        &state.method_key.method_name,
                                        &state.method_key.descriptor,
                                    )
                                    .unwrap_or_else(|| "unrecorded".to_string()),
                            ));
                        }
                    }
                    CompilationTier::C1 | CompilationTier::C1WithProfiling => {
                        snap.methods_at_c1 += 1
                    }
                    CompilationTier::FullProfile => snap.methods_at_full_profile += 1,
                    CompilationTier::C2 => snap.methods_at_c2 += 1,
                }
            }
        }
        let stats = &core.stats;
        let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        report.c1_compilations = load(&stats.c1_compilations);
        report.c2_compilations = load(&stats.c2_compilations);
        report.osr_compilations = load(&stats.osr_compilations);
        report.deoptimizations = load(&stats.deoptimizations);
        report.c2_bailouts = load(&stats.c2_bailouts);
        report.total_compile_time_ms = load(&stats.total_compile_time_ms);
        for lane in [C1_LANE, C2_LANE] {
            report.lanes[lane] = (
                load(&stats.dispatched[lane]),
                load(&stats.queue_wait_ms_total[lane]),
                load(&stats.queue_wait_ms_max[lane]),
            );
        }
        report.queued_now = core.queue_len();
        report.inflight_now = load(&core.inflight_count);
        report.osr_refused_in_flight = load(&stats.osr_refused_in_flight);
        report.nominate_to_first_body = (
            load(&stats.nominate_to_first_body),
            load(&stats.nominate_to_first_body_ms_total),
            load(&stats.nominate_to_first_body_ms_max),
        );
        report.first_body_to_c2 = (
            load(&stats.first_body_to_c2),
            load(&stats.first_body_to_c2_ms_total),
            load(&stats.first_body_to_c2_ms_max),
        );
        report.verdict_fields = verdict_count_fields(&core.verdicts);
        report.self_lock_census = core.verdicts.self_lock_census().line();
        report
    }
}

/// Per-method invocation-vs-promotion counts, aggregated across every method
/// this process has ever tracked. See [`dump_method_stats_to_stderr`].
#[derive(Default)]
struct MethodPromotionSnapshot {
    distinct_methods: u64,
    methods_ever_invoked: u64,
    total_invocations: u64,
    methods_still_interpreted: u64,
    methods_at_c1: u64,
    methods_at_full_profile: u64,
    methods_at_c2: u64,
}

/// The tail of the `JIT method stats:` line: how many methods one VM's
/// [`crate::JitVerdictRegistry`] holds bail-listed, how many (method, entry
/// pc) OSR entry rejects still count in it, and how many methods its
/// compile-time recursion-cycle sets hold (`cycle-members=`, and the
/// `sticky=` subset that keeps statically bound calls on dispatch).
///
/// `bail-list-shortcircuits=` on the `JIT admission gate:` line counts how
/// often the list refused a compile, which cannot tell "one method refused
/// 40,000 times" from "40,000 methods refused once"; this is the other half.
/// Interpreter round i1 wave 12: until then no report read any of these.
///
/// The last group is the redefinition census (interpreter round i1 wave 17,
/// stage 1 of `i14-L3-proposal-class-scoped-redefinition-invalidation`):
/// `redefinitions=` this VM's class redefinitions, `redefine-evicted-bodies=`
/// the compiled bodies their full flushes evicted, and
/// `redefine-dependent-bodies=` how many of those a class-scoped eviction
/// would also have evicted. The difference is the recompilation a scoped
/// eviction would save.
pub(crate) fn verdict_count_fields(verdicts: &crate::JitVerdictRegistry) -> String {
    let (cycle_members, sticky) = verdicts.recursion_cycle_member_counts();
    let (redefinitions, evicted, dependent) = verdicts.redefinition_census();
    format!(
        " | bail-listed={} osr-entry-rejects={} cycle-members={cycle_members} sticky={sticky} \
         | redefinitions={redefinitions} redefine-evicted-bodies={evicted} \
         redefine-dependent-bodies={dependent}",
        verdicts.bail_list_size(),
        verdicts.osr_entry_reject_count()
    )
}

/// `CRATONVM_DBG_JIT_METHOD_STATS=1` diagnostic: dump, to stderr, how many
/// distinct methods this process ever tracked, how many were actually
/// invoked, and how many reached each compilation tier — plus the aggregate
/// compile counts/time already tracked in [`CompilationStats`]. Written to
/// characterize whether a slow run is dominated by code that genuinely never
/// gets hot enough to promote past the interpreter (as opposed to a stuck
/// lock, a cache-thrashing hot path, or some other fixable inefficiency) —
/// see `ES-PERF-20260719-testSlicesDense-interpreter-throughput-FIXED.md`.
/// No-op if no [`TieredCompilationManager`] was ever constructed this process
/// (should not happen in the normal VM binary, but keeps this safe to call
/// unconditionally from an exit hook).
pub fn dump_method_stats_to_stderr() {
    // The admission gate and the OSR metadata checks, first and unconditional.
    //
    // Every number here was previously computed and stored by a `pub fn` with
    // **no caller anywhere in the tree** — `jit_bail_shortcircuits`,
    // `jit_code_cache_cap_refusals`, `osr_contract_violations`,
    // `stale_install_epoch_refusals`. Each one's own doc says it is "expected
    // to stay zero" and that a diagnostic nobody enables is how a compiler bug
    // stays unnoticed; none of them could be enabled at all. Printed before the
    // `DIAG_CORE` early return so a run with no tiered manager still reports
    // them.
    //
    // How to read the three groups:
    //
    //   * `admitted`/`refused` per door — a door whose `admitted` is 0 on a
    //     workload that clearly used it is not calling the gate.
    //   * `ungated-backend-entries` — MUST be 0. Non-zero means some path
    //     reached `x64::compile_with_param_slots` without an admission, which
    //     is the drift `compile_gate` exists to prevent.
    //   * per-door `undeclared-direct-bind-rows` — MUST be 0 (`H20-1` N3).
    //     Non-zero means that door bound a direct-call helper's address
    //     without ever asking `CompileAdmission::declare_direct_call_policy`
    //     for it — the exact bypass `DirectCallPolicy`'s exhaustive `match`
    //     exists to make a number instead of a silence. Read it beside the
    //     door's own `admitted`: a door with admissions and zero undeclared
    //     rows has been taught the question, a door with no admissions has
    //     not run at all.
    //   * `osr-contract-violations` / `osr-coordinate-mismatches` — both MUST
    //     be 0. Non-zero means an artifact's OSR metadata contradicted itself
    //     and was dropped, so the method silently lost OSR service.
    {
        use crate::compile_gate::{
            admissions, refusals, undeclared_direct_bind_rows, ungated_backend_entries, CompileDoor,
        };
        let doors: Vec<String> = CompileDoor::ALL
            .iter()
            .map(|d| {
                format!(
                    "{}: admitted={} refused={} undeclared-direct-bind-rows={}",
                    d.label(),
                    admissions(*d),
                    refusals(*d),
                    undeclared_direct_bind_rows(*d),
                )
            })
            .collect();
        eprintln!(
            "[cratonvm] JIT admission gate: {} | ungated-backend-entries={} \
             | bail-list-shortcircuits={} code-cache-cap-refusals={} \
             | osr-contract-violations={} osr-coordinate-mismatches={} \
             stale-install-epoch-refusals={}",
            doors.join(" | "),
            ungated_backend_entries(),
            crate::jit_bail_shortcircuits(),
            crate::jit_code_cache_cap_refusals(),
            crate::osr_contract::osr_contract_violations(),
            crate::osr_coords::osr_coordinate_mismatches(),
            crate::stale_install_epoch_refusals(),
        );
        // The OSR lifecycle, on the same line's heels and for the same reason:
        // the counters were ungated by the `osr-02` lane precisely because "a
        // silent OSR exit is indistinguishable from never having entered", and
        // then nothing printed them. This is also the only thing that makes
        // OVER-refusal visible — a new entry-time refusal that quietly costs a
        // workload its OSR shows up here as `osr_entered` collapsing while
        // `osr_refused_entry` rises, and nowhere else. Read `osr_exited`
        // against `osr_entered`, never alone.
        //
        // The four `osr_exit_*` rows partition the exits that arrived carrying
        // a reconstructed frame, and two of them — `osr_exit_map_missing` and
        // `osr_exit_bci_unrecorded` — are cross-checks between metadata one
        // function writes, not classifications, so they MUST read zero.
        // `regression-suite/perf/osr-exit-differential.sh` parses this line and
        // fails its run on either of those, or on a forced-exit arm that took
        // no entry — without which that whole harness would be a test of the
        // interpreter.
        eprintln!(
            "[cratonvm] OSR lifecycle: {}",
            crate::metrics::osr_counts()
                .iter()
                .map(|(n, c)| format!("{n}={c}"))
                .collect::<Vec<_>>()
                .join(" ")
        );
        // Round 16 (lane osrdoor, OD16-4): why optimizing bodies got (or did
        // not get) their ordinary OSR entries, run-wide. Nothing when no body
        // planned one.
        if let Some(line) = crate::ir_lower::osr_entry_census_line() {
            eprintln!("{line}");
        }
        // Round 17 wave 6 (lane sbzgc2, SBZGC17-5): the inline
        // `StringBuilder.toString()`'s emitted sites, and -- for code compiled
        // under `CRATONVM_DBG_JITC=1` -- its run-time paths and decline causes
        // (the ZGC start-bit pre-check's among them). Nothing when no body was
        // emitted (`CRATONVM_JIT_SB_TOSTRING_INLINE=0`).
        if let Some(line) = crate::metrics::sb_to_string_census_line() {
            eprintln!("{line}");
        }
        // The OSR ADMISSION census, which until now printed from exactly one
        // place: `craton_gpu.rs`'s exit summary. An instrument that only
        // reports under `--gpu` is not an instrument — the refusal tally and
        // the optimizing-tier reach it now carries were both unreadable on
        // every ordinary run, which is the same "a zero from a one-door
        // counter" failure the counters themselves exist to prevent. It
        // self-gates on `attempts == 0`, so a run that never OSR-compiled
        // prints nothing.
        cratonvm_types::osr_refusal_census::exit_summary();
        // Why the compiled frames in this run's stack traces did or did not
        // carry a line. Unconditional and including zeros, for the reason the
        // rows above are: `(Unknown Source)` is what a trace prints for FOUR
        // different refusals plus a kill switch, and nothing else in the system
        // can tell them apart. An all-zero line means no stack trace this run
        // crossed a compiled frame, which is itself the answer to "did this
        // path engage at all". See `crate::compiled_frame_line_counts`.
        //
        // The two emitter-side censuses ride along because they answer the
        // next question this one raises. `inline-map-at-return` separates an
        // exact `native_pc_offset` hit from a miss -- the distinction whose
        // absence let an emitter defect file oop maps 9-25 bytes past the
        // return address while a fallback answered and nothing said which
        // evidence produced it. `miss-edge-poison` says how many chains were
        // deliberately given up so a frame could not be handed the callees of
        // a splice that did not run. Both were `pub fn`s with no caller
        // anywhere in the tree until now, which is the same defect this row
        // exists to fix one level up.
        eprintln!(
            "[cratonvm] compiled-frame lines: {} | inline-map-at-return={:?} \
             miss-edge-poison={:?}",
            crate::compiled_frame_line_counts()
                .iter()
                .zip(crate::FRAME_LINE_SLOT_NAMES.iter())
                .map(|(c, n)| format!("{n}={c}"))
                .collect::<Vec<_>>()
                .join(" "),
            crate::x64::inline_call_map_at_return_counts(),
            crate::x64::inline_miss_edge_poison_counts(),
        );
        // What the operand-spill cursor did, and WHERE every word went:
        // `res-push` + `flush-reserved` + `res-invalidate` + `res-inline-locals`
        // + `res-inline-merge` + `res-call-service` + `res-helper-args` sum to
        // `res-total` by construction, because `SpillReason` is a parameter of
        // `reserve_spill_slots`. The first cut of this census named three sites
        // by hand and left 22-30% in an unnamed remainder, which is the same
        // shape as the defect it was built to find.
        //
        // `exhausted` is a REFUSED COMPILE:
        // the method keeps running interpreted and the only thing that ever
        // said so was a single-slot "last bail site" with no count, so "does
        // this happen, and on what?" had no answer at all. A non-zero
        // `flush-canonical` is the engagement counter for the canonical-home
        // flush — a zero there beside a non-zero `flush-reserved` means that
        // path never ran, which is a different finding from it running and not
        // helping. `peak-words` is a MAX over compiles, never a sum.
        eprintln!(
            "[cratonvm] spill cursor: {}",
            crate::spill_cursor_counts()
                .iter()
                .zip(crate::SPILL_CURSOR_SLOT_NAMES.iter())
                .map(|(c, n)| format!("{n}={c}"))
                .collect::<Vec<_>>()
                .join(" ")
        );
        // And how often a compiled entry was dropped as the same activation as
        // an interpreter frame. `call-opcode` is the row worth reading: that
        // rule's revert shape is asserted by no test, because the only arm that
        // ever claimed to isolate it used an environment variable that does not
        // exist. A counter cannot say the rule is RIGHT; it can say whether it
        // fires, and a permanent zero is itself a finding.
        eprintln!(
            "[cratonvm] stack-walk dedupe: {}",
            crate::stack_walk_dedupe_counts()
                .iter()
                .zip(crate::DEDUPE_SLOT_NAMES.iter())
                .map(|(c, n)| format!("{n}={c}"))
                .collect::<Vec<_>>()
                .join(" ")
        );
    }
    // The unresumable-trap refusal's census, printed UNCONDITIONALLY and
    // including zeros — before the `DIAG_CORE` early return, like the rows
    // above it, so a run that never built a tiered manager still reports.
    //
    // `shape` counts methods whose protected range carries a trap this tier
    // lowers to an unresumable deopt; `refused` counts the ones the guard
    // actually declined the optimizing tier to. They diverge only under
    // `CRATONVM_JIT_IR_UNRESUMABLE_TRAP_GUARD=0` (measurement only, and unsound
    // — see that switch's doc), where `refused` drops to zero while `shape`
    // keeps counting. That difference IS the size of the A/B: it names how many
    // methods the OFF arm moved, so a flat wall-clock result can be read as
    // "the refusal is cheap here" instead of "the switch did nothing".
    //
    // Suppressing the zero line was the first draft and was wrong: a silent
    // instrument cannot be told from an absent one.
    {
        let (shape, refused) = crate::ir_unresumable_trap_counts();
        eprintln!("[cratonvm] IR unresumable-trap refusal: shape={shape} refused={refused}");
    }
    // The first core's report: read from it while it lives, else the one it
    // left as it dropped (`DiagCore`). No manager, or a core dropped while
    // the report was unarmed: nothing per-core to print.
    let Some(mut report) = DIAG_CORE.get().and_then(DiagCore::report) else {
        return;
    };
    let mut hot_but_stuck: Vec<HotButStuckRow> = std::mem::take(&mut report.hot_but_stuck);
    let snap = &report.snap;
    let c1_threshold = report.c1_threshold;
    // (invocation_count, queued_for_compilation, tier_fail_count, ineligible,
    // name) for every Interpreter-tier method whose invocation_count already
    // crossed c1_threshold. Splitting `ineligible` out matters: a method the
    // skip list refuses is stuck BY DESIGN and is not evidence of anything,
    // whereas one with a non-zero `tier_fail_count` is a compiler failure.
    // Reporting both as `tier_fail_count=3` is what made an earlier
    // "1531 of 1642 hot methods never compile" reading unactionable.
    // (invocations, queued, tier_fail_count, ineligible, display name, the
    // refusal site the compiler recorded for it — see
    // crate::JitVerdictRegistry::bail_reason_for). Taken above.
    let ineligible_by_policy = report.ineligible_by_policy;
    // The verdict counts go at the END of the line: scripts parse its earlier
    // fields (`scripts/jdk-only-bench.sh` cuts `| compiles: ... |`).
    eprintln!(
        "[cratonvm] JIT method stats: {} distinct methods tracked, {} ever invoked, {} total invocations \
         | still-interpreted={} c1={} full-profile={} c2={} \
         | compiles: c1={} c2={} osr={} deopts={} c2_bailouts={} total_compile_time_ms={} \
         | code_buffer_bails={} (discarded_compile_ms={}) \
         | inline_live_slot_clamps={} inline_locals_floor_bumps={} \
         | c1_threshold={} hot_but_stuck_in_interpreter={} (of which ineligible-by-policy={}, compile-failures={}){}",
        snap.distinct_methods,
        snap.methods_ever_invoked,
        snap.total_invocations,
        snap.methods_still_interpreted,
        snap.methods_at_c1,
        snap.methods_at_full_profile,
        snap.methods_at_c2,
        report.c1_compilations,
        report.c2_compilations,
        report.osr_compilations,
        report.deoptimizations,
        report.c2_bailouts,
        report.total_compile_time_ms,
        crate::code_buffer_bail_cost().0,
        crate::code_buffer_bail_cost().1,
        crate::x64::inline_live_slot_clamps(),
        crate::x64::inline_locals_floor_bumps(),
        c1_threshold,
        hot_but_stuck.len(),
        ineligible_by_policy,
        hot_but_stuck
            .iter()
            .filter(|(_, _, fail, inelig, _, _)| *fail > 0 && !*inelig)
            .count(),
        report.verdict_fields,
    );
    // JIT round 19 (lane selflock19): the self-lock admission / monitor-drop
    // census of the same core's registry (`self_lock_census.rs`), on its own
    // line so scripts that parse the stats line above are untouched. Nothing
    // when no synchronized method was compiled and no drop site saw a hold.
    if let Some(line) = report.self_lock_census.take() {
        eprintln!("{line}");
    }
    // JIT round 22 (lane sync, CODECACHE21-3): the per-method withdrawal
    // census. A CHA / redefinition storm in a real workload is named here
    // (method, charged optimizing-body withdrawals, whether
    // `WITHDRAWAL_RECOMPILE_CUTOFF` bailed it to C1) instead of being inferred
    // from `max_versions_for_one_method`. Own line, after the parsed stats
    // line; nothing when no method was charged.
    if let Some(line) = withdrawal_census_line(std::mem::take(&mut report.withdrawals)) {
        eprintln!("{line}");
    }
    // Queue latency per lane: how long a request waited between being queued
    // and a worker picking it up. `enqueue_time_ms` was stamped on every
    // request for aging and never reported, so a backed-up lane was invisible
    // beside an idle one. Printed including zeros, like the rows above.
    {
        let mean = |total: u64, n: u64| -> u64 {
            if n == 0 {
                0
            } else {
                total / n
            }
        };
        let lane_line = |lane: usize, name: &str| {
            let (n, total, max) = report.lanes[lane];
            let mean = mean(total, n);
            format!("{name}: dispatched={n} wait_ms_total={total} wait_ms_mean={mean} wait_ms_max={max}")
        };
        eprintln!(
            "[cratonvm] JIT compile queue: {} | {} | queued_now={} inflight_now={} osr_refused_in_flight={}",
            lane_line(C1_LANE, "c1-lane"),
            lane_line(C2_LANE, "c2-lane"),
            report.queued_now,
            report.inflight_now,
            report.osr_refused_in_flight,
        );
        let (first_n, first_total, first_max) = report.nominate_to_first_body;
        let (c2_n, c2_total, c2_max) = report.first_body_to_c2;
        eprintln!(
            "[cratonvm] JIT time-to-tier: nominated->first-body: n={} mean_ms={} max_ms={} \
             | first-body->c2: n={} mean_ms={} max_ms={}",
            first_n,
            mean(first_total, first_n),
            first_max,
            c2_n,
            mean(c2_total, c2_n),
            c2_max,
        );
    }
    // The String-intrinsic pin's ENGAGEMENT census (`crate::string_intrinsic_pin_census`).
    //
    // A site count and an engagement count answer different questions, and the
    // whole value of this line is that a ZERO is readable. An audit measured
    // `String.charAt` at a flat 186-196 ns/char where a byte-identical body
    // reaches 3.0 ns/char elsewhere in the same binary, and neither documented
    // lever moved it: `CRATONVM_JIT_NO_STRING_INTRINSIC_PIN=1` gave 326.3/328.6
    // ns/char against a default of 329.5/333.7. Switching the pin OFF costing
    // nothing is exactly what "it was never on" looks like from the outside,
    // and no timing can tell that apart from "it fired and did not help".
    // `fired=0` can, in one line, at the end of any run.
    //
    // Those two numbers are pre-emitter and pre-2026-09-02 and must not be
    // quoted as current: `java/lang/String` is final, so the method-entry
    // door's devirtualisation was taking every String access site away from
    // the intrinsic before the gate saw it, and BOTH arms of that A/B measured
    // a program with no String intrinsic in it. The same rows read ~3.2
    // ns/char once the rewrite yields. That is the line printed below this one.
    //
    // The other three counts are printed beside it because they are the
    // candidate REASONS for a `fired=0`: no resolved `java/lang/String` field
    // layout, no constant-pool invoke resolver at the door that asked, or the
    // fail-closed rule declining on a missing resolver's behalf. All three
    // production doors pass a resolver, so `blind-no-resolver` is EXPECTED to
    // read zero — which is what makes a non-zero one worth the line: it would
    // name a door nobody knew existed.
    let (sp_fired, sp_no_layout, sp_no_resolver, sp_fail_closed) =
        crate::string_intrinsic_pin_census();
    eprintln!(
        "[cratonvm] JIT String-intrinsic pin: fired={sp_fired} blind-no-layout={sp_no_layout} \
         blind-no-resolver={sp_no_resolver} fail-closed={sp_fail_closed}"
    );
    // Beside the pin, because the two answer the halves of one question. The
    // pin says whether a String-accessor method was kept OFF the optimizing
    // tier; this says, for the ones that reached it, whether the expansion's
    // `value`/`coder` reads are inline loads or `jit_getfield` helper CALLs.
    // Before the rows existed, every expanded `charAt` paid two CALLs per
    // character -- 917,203,334 of them on one `probes/CharAtCostCurve.java`
    // run -- and nothing in this dump said so: `getfield helper calls` counted
    // them without naming the source, and `emitted_charAt` reported the
    // expansion as a success.
    eprintln!(
        "[cratonvm] JIT String-access inline rows: sites={}",
        crate::string_access_compact_rows()
    );
    // Sites the `final`-class devirtualisation handed BACK to a call-site
    // intrinsic (`crate::devirt_yielded_to_intrinsic_count`).
    //
    // Printed beside the pin because it answers the question the pin's four
    // counters could not: `java/lang/String` is final, so before 2026-09-02
    // every String access site was statically bound and left the invoke loop
    // BEFORE the instance-intrinsic gate, which is `invoke_kind == 0 || == 2`.
    // Not declined, not blind, not counted -- gone. `probes/CharAtDoorProbe`
    // read 349.64 ns/char on the arm that took this path against 3.2-4.3 on
    // four byte-identical siblings that did not.
    eprintln!(
        "[cratonvm] JIT devirt yielded to intrinsic: {}",
        crate::devirt_yielded_to_intrinsic_count()
    );
    // Methods sealed out of compilation BEFORE any attempt, by reason. A
    // different and larger population than `hot_but_stuck` — a Spring Boot
    // context startup seals ~856 here against ~69 refused compiles — and until
    // the reasons were split they all read as one opaque label.
    let seal_census = crate::jit_skip_seal_census();
    if !seal_census.is_empty() {
        let total: u64 = seal_census.iter().map(|(_, n)| *n).sum();
        eprintln!(
            "[cratonvm] JIT skip-seal census: {total} method(s) sealed before any compile | {}",
            seal_census
                .iter()
                .map(|(r, n)| format!("{r}={n}"))
                .collect::<Vec<_>>()
                .join(" "),
        );
    }
    // Which arm of the native-shadow predicate fired. `direct`/`inherited` are
    // precise facts about a specific call; `interface-blind` is the class-blind
    // probe, and its share is the number that says whether making that arm
    // precise is worth anything.
    // The positive gate memo. `fills` is bounded by the number of distinct
    // eligible methods; `hits` is how many `execute()` entries no longer re-run
    // the full gate (including the O(bytecode) native-shadow scan) because of
    // it. A hits:fills ratio near 1 would mean the memo is not paying.
    let (gate_hits, gate_fills) = crate::jit_gate_pass_census();
    if gate_hits > 0 || gate_fills > 0 {
        eprintln!(
            "[cratonvm] JIT gate-pass memo: hits={gate_hits} fills={gate_fills} \
             (gate evaluations avoided: {})",
            gate_hits,
        );
    }
    // Census-driven thin direct helpers: how many sites each compile door
    // actually bound. Printed unconditionally (even at 0) because the whole
    // point is to distinguish "the bind did not help" from "the bind never
    // happened" — the failure mode `LEAF_NATIVE_HITS` exists for, and the one
    // that made an earlier ByteBuffer accessor rewrite read as a no-op when in
    // fact it was on a path with zero invocations.
    let (check_index_sites, fence_sites) = crate::census_direct_helper_sites();
    let (long_value_of_sites, long_long_value_sites) = crate::long_box_direct_helper_sites();
    let (vh_read_sp, vh_read_osr) = crate::varhandle_read_direct_helper_sites();
    let nio_byte_sites = crate::nio_byte_element_sites();
    let nio_byte_sites_ir = crate::nio_byte_element_sites_ir();
    let nio_byte_sites_osr = crate::nio_byte_element_sites_osr();
    let nio_byte_refused = crate::nio_byte_element_sites_refused();
    let (bs_sp, bs_ir, bs_osr, bs_served, bs_declined) = crate::buffer_session_census();
    let (sm_sp, sm_ir, sm_osr, sm_served, sm_declined) = crate::scoped_memory_census();
    let md_update_sites = crate::md_update_byte_sites();
    eprintln!(
        "[cratonvm] JIT thin direct-helper binds: Preconditions.checkIndex={check_index_sites} \
         Reference.reachabilityFence={fence_sites} Long.valueOf={long_value_of_sites} \
         Long.longValue={long_long_value_sites} VarHandle.read={vh_read_sp}/{vh_read_osr} \
         ByteBuffer.byteElement=sp:{nio_byte_sites}/ir:{nio_byte_sites_ir}/osr:{nio_byte_sites_osr} \
         (profile-refused={nio_byte_refused}) MessageDigest.update={md_update_sites}",
    );
    // `Buffer.session()`: bound sites per door, and the calls the fast path
    // actually answered against the ones it sent back to the funnel. Printed
    // together because a bind count alone cannot distinguish "installed" from
    // "installed and declining everything" -- the exact failure the
    // `ByteBuffer.byteElement` line above was added for after it happened.
    eprintln!(
        "[cratonvm] JIT Buffer.session direct: sites sp:{bs_sp}/ir:{bs_ir}/osr:{bs_osr} \
         served={bs_served} declined={bs_declined}",
    );
    eprintln!(
        "[cratonvm] JIT ScopedMemoryAccess direct: sites sp:{sm_sp}/ir:{sm_ir}/osr:{sm_osr} \
         served={sm_served} declined={sm_declined}",
    );
    // Non-virtual `invokevirtual` reclassification, by RULE. Printed together
    // because they are the same transformation with two different soundness
    // arguments, and apart because only a per-rule number can say which one a
    // workload exercised — a combined tally that moves proves neither.
    let private_pinned = crate::private_invokevirtual_pinned();
    let final_pinned = crate::final_invokevirtual_pinned();
    if private_pinned | final_pinned != 0 {
        eprintln!(
            "[cratonvm] JIT invokevirtual pinned non-virtual: private={private_pinned} \
             final={final_pinned}",
        );
    }
    // Inline `checkcast`, by cause. The runtime engagement number is the
    // `membership walks by JIT site: checkcast=` line above: every walk the
    // fast path avoids is one that line does not report.
    let (cc_sp, cc_ir, cc_prim, cc_no_target, cc_untrusted) = crate::checkcast_inline_sites();
    if cc_sp | cc_ir | cc_prim | cc_no_target | cc_untrusted != 0 {
        eprintln!(
            "[cratonvm] JIT checkcast inline sites: single-pass={cc_sp} optimizing={cc_ir} \
             prim-array={cc_prim} refused-no-target-id={cc_no_target} \
             refused-untrusted-operand={cc_untrusted}",
        );
    }
    let (nio_served, nio_declined, md_served, md_declined) = crate::byte_element_helper_calls();
    if nio_served | nio_declined | md_served | md_declined != 0 {
        eprintln!(
            "[cratonvm] JIT byte-element helper calls: ByteBuffer served={nio_served} \
             declined={nio_declined} MessageDigest served={md_served} declined={md_declined}",
        );
    }
    let shadow_census = crate::jit_native_shadow_cause_census();
    if !shadow_census.is_empty() {
        eprintln!(
            "[cratonvm] JIT native-shadow verdicts by arm: {}",
            shadow_census
                .iter()
                .map(|(r, n)| format!("{r}={n}"))
                .collect::<Vec<_>>()
                .join(" "),
        );
    }
    for line in read_path_census_lines(std::mem::take(&mut report.read_path)) {
        eprintln!("{line}");
    }
    if !report.top_methods.is_empty() {
        report.top_methods.sort_by(|a, b| b.0.cmp(&a.0));
        eprintln!(
            "[cratonvm] JIT method stats: top {} methods by invocations (tier, count, name):",
            report.top_methods.len().min(50)
        );
        for (count, tier, name) in report.top_methods.iter().take(50) {
            eprintln!("[cratonvm]   {tier:?} {count:>12} {name}");
        }
    }
    if !hot_but_stuck.is_empty() {
        hot_but_stuck.sort_by(|a, b| b.0.cmp(&a.0));
        eprintln!(
            "[cratonvm] JIT method stats: top {} hot-but-stuck methods (invocations, queued, tier_fail_count, why, name):",
            hot_but_stuck.len().min(30)
        );
        for (count, queued, fail, inelig, name, reason) in hot_but_stuck.iter().take(30) {
            let why = if *inelig {
                "ineligible-by-policy"
            } else if *fail > 0 {
                "compile-failed"
            } else {
                "not-yet-attempted"
            };
            eprintln!(
                "[cratonvm]   {count:>10} queued={queued:<5} tier_fail_count={fail:<3} {why:<20} {name} reason={reason}"
            );
        }
        // The compile failures are the entries worth looking at — a policy
        // decline is stuck by design — but they are usually a tiny minority
        // and get buried under the policy ones when the list is ranked by
        // invocation count (measured on the Hibernate concurrency workload:
        // 1513 policy declines vs 6 real failures, none of which appeared in
        // the top 30). List them separately so the actionable set is never
        // hidden by the expected one.
        //
        // This list used to be headed "these are bugs". That over-claimed:
        // `ineligible` covers only the tier manager's OWN declines, so a
        // deliberate correctness gate INSIDE the compiler (an RBC.6 handler
        // that reads a local the exceptional-frame handoff cannot restore, say)
        // lands here looking like a defect. `reason=` is what tells the two
        // apart, so print it and let the reader classify.
        let failures: Vec<_> = hot_but_stuck
            .iter()
            .filter(|(_, _, fail, inelig, _, _)| *fail > 0 && !*inelig)
            .collect();
        if !failures.is_empty() {
            eprintln!(
                "[cratonvm] JIT method stats: {} hot method(s) whose COMPILE FAILED \
                 (the compiler was asked and refused; `reason=` names the refusing site):",
                failures.len()
            );
            for (count, queued, fail, _, name, reason) in failures.iter().take(30) {
                eprintln!(
                    "[cratonvm]   {count:>10} queued={queued:<5} tier_fail_count={fail:<3} {name} reason={reason}"
                );
            }
        }
    }
}

/// The `jit-read-path-census` lines (i73-L5, lane zip10) for `rows`: one
/// summary line, then the top [`READ_PATH_CENSUS_ROWS`] methods by invocation
/// count, each with its tier (`Interpreter` = never compiled), deopts, compile
/// failures, the tier manager's own decline (`ineligible`), whether a compile
/// is queued, and the refusal site the compiler recorded (`-`: none). Nothing
/// when no read-path method was tracked. Pure, so the format is pinned by a
/// test; scripts parse `key=value` fields and the trailing name.
fn read_path_census_lines(mut rows: Vec<ReadPathRow>) -> Vec<String> {
    if rows.is_empty() {
        return Vec::new();
    }
    rows.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.6.cmp(&b.6)));
    let interpreted = rows
        .iter()
        .filter(|r| r.1 == CompilationTier::Interpreter)
        .count();
    let interpreted_invocations: u64 = rows
        .iter()
        .filter(|r| r.1 == CompilationTier::Interpreter)
        .map(|r| r.0)
        .sum();
    let refused = rows
        .iter()
        .filter(|r| r.1 == CompilationTier::Interpreter && (r.3 > 0 || r.4))
        .count();
    let deopted = rows.iter().filter(|r| r.2 > 0).count();
    let mut out = Vec::with_capacity(rows.len().min(READ_PATH_CENSUS_ROWS) + 1);
    out.push(format!(
        "[cratonvm] jit-read-path-census summary methods={} interpreted={} compiled={} \
         refused={} deopted={} interpreted-invocations={} total-invocations={}",
        rows.len(),
        interpreted,
        rows.len() - interpreted,
        refused,
        deopted,
        interpreted_invocations,
        rows.iter().map(|r| r.0).sum::<u64>(),
    ));
    for (rank, (count, tier, deopts, fails, inelig, queued, name, reason)) in
        rows.iter().take(READ_PATH_CENSUS_ROWS).enumerate()
    {
        out.push(format!(
            "[cratonvm] jit-read-path-census rank={} inv={count} tier={tier:?} deopts={deopts} \
             fails={fails} ineligible={inelig} queued={queued} reason={} {name}",
            rank + 1,
            reason.replace(' ', "_"),
        ));
    }
    out
}

/// Idempotently start `mgr`'s background compile workers.
///
/// Safe to call on every interpreter invocation hook: once the manager's
/// workers are running this is one atomic load. `make_compile_fn` builds the
/// codegen callback all of them share.
///
/// The worker handle lives on the manager, so each VM in a process gets its
/// own workers and dropping the manager joins them. It used to be a
/// process-wide `Once` plus a global handle slot tied to whichever manager
/// asked first: a second VM in the same process enqueued into a queue no
/// worker ever drained.
pub fn ensure_background_compiler<F>(mgr: &TieredCompilationManager, make_compile_fn: F)
where
    F: FnOnce() -> CompileFn,
{
    mgr.ensure_background_compiler(make_compile_fn);
}

// ───────────────────────────────────────────────────────────────────────────────
// CompilationStats
// ───────────────────────────────────────────────────────────────────────────────

/// Aggregate statistics for the compilation subsystem.
#[derive(Debug, Default)]
pub struct CompilationStats {
    pub c1_compilations: AtomicU64,
    pub c2_compilations: AtomicU64,
    pub osr_compilations: AtomicU64,
    /// Every deoptimization reported, soft or counted.
    pub deoptimizations: AtomicU64,
    /// Of `deoptimizations`, the ones not charged as traps — see
    /// [`deopt_is_counted_trap`].
    pub soft_deoptimizations: AtomicU64,
    pub c2_bailouts: AtomicU64,
    pub total_compile_time_ms: AtomicU64,
    /// Requests a compile worker dispatched, per lane (`[0]` = the C1 lane,
    /// `[1]` = the optimizing lane: method-entry C2 and every OSR request).
    ///
    /// With the two fields below, the answer to "how much time is lost in the
    /// queue". `enqueue_time_ms` was stamped on every request so the dequeue
    /// could AGE it, and nothing ever reported the wait itself -- so a C2 lane
    /// whose Low supersedes sat behind a burst of OSR requests, or a C1 lane
    /// backed up behind one slow compile, looked exactly like a quiet one.
    pub dispatched: [AtomicU64; 2],
    /// Summed milliseconds between a dispatched request's `enqueue_time_ms`
    /// and its dispatch, per lane. Divide by [`Self::dispatched`] for the
    /// mean. A request whose stamp is not on [`process_uptime_ms`]'s clock
    /// (the VM's deopt re-queue passes wall-clock milliseconds) contributes 0.
    pub queue_wait_ms_total: [AtomicU64; 2],
    /// The longest single wait, per lane, in milliseconds.
    pub queue_wait_ms_max: [AtomicU64; 2],
    /// `request_osr` calls refused because the method's OSR slot was held by
    /// its own earlier OSR request (queued or compiling). The interpreter's
    /// back-edge schedule re-asks while it waits, so this prices how long OSR
    /// requests sit in flight, in back-edge-schedule ticks. Printed on the
    /// `JIT compile queue` line.
    pub osr_refused_in_flight: AtomicU64,
    /// Methods whose first method-entry body published after a request
    /// admitted through this manager -- the population the two fields below
    /// are summed over. Time-to-peak, split into its two waits: "nominated ->
    /// first compiled body" (queue wait + compile of the first tier) here, and
    /// "first body -> C2" (the supersede's `Low` wait + the C2 compile) below.
    /// Together with the `JIT compile queue` line this separates "waited in
    /// the queue" from "was never offered". Printed as `JIT time-to-tier`.
    pub nominate_to_first_body: AtomicU64,
    /// Summed milliseconds from first admission to first method-entry body.
    pub nominate_to_first_body_ms_total: AtomicU64,
    /// Longest single "nominated -> first body" wait, in milliseconds.
    pub nominate_to_first_body_ms_max: AtomicU64,
    /// Methods whose first method-entry C2 body published.
    pub first_body_to_c2: AtomicU64,
    /// Summed milliseconds from first method-entry body to first C2 body (0
    /// for a method whose first body was C2).
    pub first_body_to_c2_ms_total: AtomicU64,
    /// Longest single "first body -> C2" wait, in milliseconds.
    pub first_body_to_c2_ms_max: AtomicU64,
    /// zt t15/pl6: the method of the last published compile counted in
    /// `c1_compilations` / `c2_compilations` / `osr_compilations`, for the
    /// perf-data `sun.ci.lastMethod` / `sun.ci.lastType`; `None` before one.
    pub last_compiled: Mutex<Option<LastCompiled>>,
    /// zt t18/pl8: every attempt's wall time in nanoseconds (the worker's
    /// measurement; `total_compile_time_ms` sums whole milliseconds), for
    /// `java.ci.totalTime` and `CompilationMXBean.getTotalCompilationTime()`.
    pub total_compile_time_ns: AtomicU64,
    /// zt t18/pl8: the published method-entry compiles' nanoseconds
    /// (`sun.ci.standardTime`).
    pub standard_compile_time_ns: AtomicU64,
    /// zt t18/pl8: the published OSR compiles' nanoseconds
    /// (`sun.ci.osrTime`).
    pub osr_compile_time_ns: AtomicU64,
    /// zt t18/pl8: compiles that ran and published nothing
    /// (`is_compile_bailout`; `sun.ci.totalBailouts`).
    pub total_bailouts: AtomicU64,
    /// zt t18/pl8: the last of those (`sun.ci.lastFailedMethod` /
    /// `lastFailedType`); `None` before one.
    pub last_failed: Mutex<Option<LastCompiled>>,
    /// zt pl4 item 1: the bytecode bytes of the published method-entry
    /// compiles counted beside `standard_compile_time_ns` -- the method's
    /// own code plus every inlined callee's (HotSpot's `code_size() +
    /// num_inlined_bytecodes()`, `sun.ci.standardBytes`).
    pub standard_bytes: AtomicU64,
    /// zt pl4 item 1: the same for the published OSR compiles
    /// (`sun.ci.osrBytes`).
    pub osr_bytes: AtomicU64,
    /// zt pl4 item 1: the last counted compile's bytes (`sun.ci.lastSize`);
    /// 0 before one, HotSpot's `no_compile`.
    pub last_size: AtomicU64,
    /// zt pl4 item 1: the published bodies' code-cache bytes (their
    /// buffers' capacity) and instruction bytes (`sun.ci.nmethodSize` /
    /// `nmethodCodeSize`, HotSpot's `nm_total_size()` / `nm_insts_size()`).
    pub nmethod_size: AtomicU64,
    pub nmethod_code_size: AtomicU64,
}

/// zt pl4 item 1: a published compile's sizes, as the compile worker's VM
/// callback reports them ([`note_published_sizes`]) for
/// [`CompilerCore::finish`] on the same thread.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PublishedSizes {
    /// The method's bytecode bytes plus its inlined callees'.
    pub bytecode_bytes: u64,
    /// The body's code-cache bytes.
    pub total_size: u64,
    /// The body's instruction bytes.
    pub insts_size: u64,
}

thread_local! {
    /// zt pl4 item 1: the sizes the worker's callback reported for the
    /// compile `finish` is about to book. Per worker thread, transient.
    static PUBLISHED_SIZES: std::cell::Cell<Option<PublishedSizes>> =
        const { std::cell::Cell::new(None) };
}

/// zt pl4 item 1: report the sizes of the body this thread's compile task
/// just published; [`CompilerCore::finish`] takes them for that task.
pub fn note_published_sizes(sizes: PublishedSizes) {
    PUBLISHED_SIZES.with(|c| c.set(Some(sizes)));
}

/// zt pl4 item 1: [`note_published_sizes`] unless this task already reported
/// sizes (the first report names the body the task published).
pub fn note_published_sizes_once(sizes: PublishedSizes) {
    PUBLISHED_SIZES.with(|c| {
        if c.get().is_none() {
            c.set(Some(sizes));
        }
    });
}

fn take_published_sizes() -> Option<PublishedSizes> {
    PUBLISHED_SIZES.with(std::cell::Cell::take)
}

/// zt t15/pl6: [`CompilationStats::last_compiled`]: the compiled method's
/// class (as the [`MethodKey`] names it) and name, and whether the compile
/// was an OSR one.
#[derive(Debug, Clone)]
pub struct LastCompiled {
    pub class_name: Arc<str>,
    pub method_name: Arc<str>,
    pub osr: bool,
}

/// [`TieredCompilationManager::note_body_withdrawn`] for a whole withdrawal
/// at once (tier proposal W3-1): one lock, one generation bump.
///
/// The bump only when a tier actually came down. An evicting deopt has
/// already lowered its own method (`on_deoptimization`), and bumps only if
/// that unsettled it; bumping again here for every evicting trap would put
/// every settled call site back on the `methods` mutex once per trap, the
/// storm `on_deoptimization` is careful to avoid. Room coming back for the
/// methods deferred at the cap is released by
/// [`TieredCompilationManager::note_code_cache_pressure`] when the pressure
/// clears, which the sweeper's caller reports on every call.
impl crate::JitBodyWithdrawalSink for CompilerCore {
    fn bodies_withdrawn(&self, keys: &[crate::WithdrawnMethodKey]) {
        let mut lowered = false;
        {
            let mut methods = self.methods.lock();
            for (class_name, method_name, descriptor, class_id) in keys {
                let key = MethodKey::with_class_id(
                    *class_id,
                    Arc::clone(class_name),
                    Arc::clone(method_name),
                    Arc::clone(descriptor),
                );
                if let Some(state) = methods.get_mut(&key) {
                    if state.current_tier != CompilationTier::Interpreter {
                        state.current_tier = CompilationTier::Interpreter;
                        lowered = true;
                    }
                }
            }
        }
        if lowered {
            self.bump_settled_generation();
        }
    }

    /// JIT round 21 wave 4 (lane codecache; rev-x64 item 8): an optimizing
    /// body -- method-entry, or OSR under CODECACHE21-4 (HotSpot charges a C2
    /// OSR nmethod's `make_not_entrant` to the decompile count too) --
    /// withdrawn by a dependency invalidation or a trap eviction is charged to
    /// [`WITHDRAWAL_RECOMPILE_CUTOFF`]. The tier is left alone (the
    /// withdrawal's own report demotes it). Bailing out of C2 also closes
    /// `request_osr`, as the trap cutoffs do.
    fn optimizing_bodies_invalidated(&self, keys: &[crate::WithdrawnMethodKey]) {
        let cutoff_on = withdrawal_recompile_cutoff_enabled();
        let mut bailed = false;
        {
            let mut methods = self.methods.lock();
            for (class_name, method_name, descriptor, class_id) in keys {
                let key = MethodKey::with_class_id(
                    *class_id,
                    Arc::clone(class_name),
                    Arc::clone(method_name),
                    Arc::clone(descriptor),
                );
                if let Some(state) = methods.get_mut(&key) {
                    if charge_optimizing_withdrawal(state, cutoff_on) {
                        self.stats.c2_bailouts.fetch_add(1, Ordering::Relaxed);
                        bailed = true;
                    }
                }
            }
        }
        if bailed {
            self.bump_settled_generation();
        }
    }

    /// STASH20-1 (JIT round 20 wave 19, lane yStash): this VM's cycle verdict,
    /// as `jit_not_entrant_entry` asks it before it installs a forward. Reads
    /// the verdict registry's cycle sets only (no `methods` lock).
    fn forward_must_dispatch(
        &self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        class_id: u32,
    ) -> bool {
        self.verdicts
            .direct_call_requires_dispatch_of(class_name, method_name, descriptor, || {
                (class_id != 0).then_some(ClassId::new(class_id))
            })
    }
}

// ───────────────────────────────────────────────────────────────────────────────
// TieredCompilationManager
// ───────────────────────────────────────────────────────────────────────────────

/// Central coordinator for tiered compilation decisions.
pub struct TieredCompilationManager {
    /// Per-method state, the compile lanes, stats and flags, shared with the
    /// compile workers (which hold an `Arc<CompilerCore>`).
    core: Arc<CompilerCore>,
    /// Compilation policy.
    policy: Mutex<CompilationPolicy>,
    /// This manager's running workers, when started through
    /// [`Self::ensure_background_compiler`]. Dropped with the manager, which
    /// signals shutdown and joins them.
    background: Mutex<Option<BackgroundCompiler>>,
    /// The owning VM's runtime de-speculation registry
    /// (`JitRealm::runtime_despec`), which [`Self::invalidate_class`] and
    /// [`Self::on_class_redefined`] forget a class's verdicts in. `None` for a
    /// manager with no VM (tests), which falls back to every live registry
    /// (`compile_gate::forget_runtime_not_compilable_for_class`). Until
    /// interpreter round i1 wave 18 every manager fanned out: a class unload
    /// in one VM dropped the verdicts of another VM's unrelated class with the
    /// same `ClassId`.
    runtime_despec: Option<Arc<crate::compile_gate::RuntimeDespecRegistry>>,
}

/// Compile-thread CPU budget for a single C2 (optimizing IR) compile, in
/// milliseconds.
///
/// A method whose successful method-entry C2 compiles exceed this
/// [`C2_BUDGET_OVERRUNS_BEFORE_DEMOTION`] times is demoted to C1 for the rest
/// of the process, via the same `c2_bailout` flag the trap limits use. See the
/// guard in `CompilerCore::finish` for the full rationale.
///
/// 250 ms is roughly two orders of magnitude above a typical C2 compile in this
/// JIT (single-digit milliseconds for the arithmetic kernels that historically
/// reached tier 4), so it never fires on healthy input — it exists to catch a
/// pathological method that slips past `ir_compatible`'s static budgets and
/// `ir::IR_MAX_GRAPH_NODES`, not to tune throughput.
pub const MAX_C2_COMPILE_TIME_MS: u64 = 250;

/// Over-budget C2 compiles a method is allowed before it is demoted.
///
/// One is not evidence. A single overrun is as likely to be a cold code cache,
/// a first-time class-hierarchy walk or a page-fault storm as a pathological
/// method, and the demotion it triggers lasts the whole process.
pub const C2_BUDGET_OVERRUNS_BEFORE_DEMOTION: u32 = 2;

/// gcj pcp: clears the manager's GC-pause gate when dropped (see
/// [`TieredCompilationManager::gc_pause_scope`]).
pub struct GcPauseScope<'m> {
    core: Option<&'m CompilerCore>,
}

impl Drop for GcPauseScope<'_> {
    fn drop(&mut self) {
        if let Some(core) = self.core {
            *core.gc_pause.lock() = false;
            core.gc_pause_end.notify_all();
        }
    }
}

impl CompilerCore {
    /// gcj pcp: block while the owning VM's collection pause is in progress.
    /// Bounded per wait (a pause that never clears its gate cannot park the
    /// worker past shutdown), and inert without a pause.
    fn wait_out_gc_pause(&self) {
        let mut on = self.gc_pause.lock();
        if !*on {
            return;
        }
        self.gc_pause_waits.fetch_add(1, Ordering::Relaxed);
        while *on && !self.shutdown.load(Ordering::Acquire) {
            let _ = self
                .gc_pause_end
                .wait_for(&mut on, std::time::Duration::from_millis(50));
        }
    }
}

impl TieredCompilationManager {
    /// gcj pcp (`CRATONVM_JIT_COMPILE_YIELDS_TO_GC`, default ON, `=0` kills):
    /// hold this manager's compile workers at their next compile while the
    /// returned scope lives -- the owning VM's stop-the-world collection. A
    /// compile already running finishes; nothing waits for it.
    pub fn gc_pause_scope(&self) -> GcPauseScope<'_> {
        if !cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_COMPILE_YIELDS_TO_GC") {
            return GcPauseScope { core: None };
        }
        *self.core.gc_pause.lock() = true;
        GcPauseScope { core: Some(&self.core) }
    }

    /// gcj pcp: compiles the workers held back until a collection pause ended.
    pub fn gc_pause_waits(&self) -> u64 {
        self.core.gc_pause_waits.load(Ordering::Relaxed)
    }

    /// Create a new manager with the given policy.
    pub fn new(policy: CompilationPolicy) -> Self {
        Self::with_install_epoch_source(policy, None)
    }

    /// [`Self::new`], but reading "the current install epoch" from `source`
    /// instead of the process-wide [`crate::jit_install_epoch`].
    ///
    /// The VM's constructor passes its per-VM code-state epoch
    /// ([`Self::with_env_policy_and_install_epoch_source`]); see
    /// `CompilerCore::install_epoch_source`. It is also the seam that makes
    /// the stale-request drop testable without a clock, without a sleep, and
    /// without a real class redefinition: the rule under test is an ordering
    /// statement about epochs, so a test bumps the injected counter exactly
    /// where a redefinition would have bumped it and asserts on what the queue
    /// then does. Driving it from the real global would make the test depend
    /// on every other test in the process that happens to flush a `JitCache`.
    ///
    /// `None` is the no-VM configuration and is what [`Self::new`] passes.
    pub fn with_install_epoch_source(
        policy: CompilationPolicy,
        install_epoch_source: Option<Arc<AtomicU64>>,
    ) -> Self {
        Self::with_epoch_sources(policy, install_epoch_source, None)
    }

    /// [`Self::with_install_epoch_source`], also handing the verdict registry
    /// the owning VM's redefine epoch (see `crate::RedefineEpoch`), which that
    /// VM advances once per class redefinition (interpreter round i1 wave 17).
    pub fn with_epoch_sources(
        policy: CompilationPolicy,
        install_epoch_source: Option<Arc<AtomicU64>>,
        redefine_epoch_source: Option<Arc<AtomicU32>>,
    ) -> Self {
        // Seed the process-start timestamp as early as possible (this
        // manager is constructed during VM init) so `process_uptime_ms`
        // reports genuine process age, not "time since first compile".
        process_start();
        let core = Arc::new(CompilerCore::with_epoch_sources(
            install_epoch_source,
            redefine_epoch_source,
        ));
        // Diagnostic-only: the first manager in the process is the one
        // `dump_method_stats_to_stderr` reports, held weakly with its own
        // threshold. See `DIAG_CORE` / `DiagCore`.
        if DIAG_CORE.get().is_none() {
            let _ = DIAG_CORE.set(DiagCore::new(&core, u64::from(policy.c1_threshold)));
        }
        core.c2_upgrade_min_invocations
            .store(c2_upgrade_gate(&policy), Ordering::Relaxed);
        core.c2_hotness_bar
            .store(c2_hotness_bar(&policy), Ordering::Relaxed);
        Self {
            core,
            policy: Mutex::new(policy),
            background: Mutex::new(None),
            runtime_despec: None,
        }
    }

    /// Forget the runtime de-speculation verdicts about the class
    /// `(class_id, class_name)` in this manager's VM, or in every live
    /// registry when it has none (see the `runtime_despec` field).
    fn forget_runtime_despec_for_class(&self, class_id: ClassId, class_name: &str) {
        match &self.runtime_despec {
            Some(registry) => {
                registry.forget_for_class(class_id, class_name);
            }
            None => {
                crate::compile_gate::forget_runtime_not_compilable_for_class(class_id, class_name);
            }
        }
    }

    /// This VM's compile verdicts: the bail list, refusal reasons, OSR entry
    /// rejects and the recursion-cycle sets. Every VM compile door hands it to
    /// the compile (`CompileRequest::verdicts`, `compile_gate::admit`) and
    /// records into it; [`Self::invalidate_class`] and the redefinition path
    /// forget a class's entries here. See [`crate::JitVerdictRegistry`].
    pub fn verdicts(&self) -> &crate::JitVerdictRegistry {
        &self.core.verdicts
    }

    /// Remove queued and historical tiering state for an unloaded class.
    ///
    /// Called from the VM's class-unload path (`vm/src/memory/gc.rs`) with the
    /// class's identity, so unloading one loader's `com/example/Foo` leaves
    /// another loader's alone. The queued requests it removes are counted as
    /// drops — they are compilation requests that will never be serviced, and
    /// "the class went away" is a perfectly good reason that is still worth
    /// being able to see. It is also the one drop reason that is genuinely
    /// final: unlike a stale-epoch drop, there is no next invocation to
    /// re-admit the method.
    ///
    /// Also forgets the class's OSR denials, and closes the branch-profile
    /// window of any removed method whose C2 nomination was still outstanding.
    pub fn invalidate_class(&self, class_id: ClassId, class_name: &str) {
        let mut windows_to_close = 0u32;
        let dropped = {
            let mut methods = self.core.methods.lock();
            methods.retain(|key, state| {
                if !key.belongs_to(class_id, class_name) {
                    return true;
                }
                windows_to_close += u32::from(state.branch_window_armed);
                false
            });
            // Under the same `methods` guard, in the established `methods` →
            // `queue` order, so no invocation hook can re-admit one of these
            // methods between the two removals.
            self.core
                .lanes
                .iter()
                .map(|lane| {
                    lane.queue
                        .lock()
                        .remove_matching(|task| task.method_key.belongs_to(class_id, class_name))
                        .len() as u64
                })
                .sum::<u64>()
        };
        self.core
            .osr_denied
            .write()
            .retain(|key, _| !key.belongs_to(class_id, class_name));
        // And this VM's compile verdicts about the class (bail list, refusal
        // reasons, OSR entry rejects): a class that is gone must not leave
        // refusals behind for the next class loaded under its id or name. By
        // identity, so a same-named class in another loader keeps its own.
        self.core.verdicts.forget_for_class(class_id, class_name);
        // And its recursion-cycle members, by exact identity (a class that is
        // gone can no longer be bound; kept on redefinition, see the method).
        self.core
            .verdicts
            .forget_cycle_members_of_unloaded_class(class_id);
        // And the runtime de-speculation verdicts (N3), which live in a
        // registry of their own precisely because they must NOT reach the OSR
        // door. An unload moves neither process epoch, so nothing else retires
        // them. Within one VM a `ClassId` is never reused (`ClassStore::add`
        // appends, `remove` leaves a tombstone), so in the VM's own registry
        // this is what stops a row per unloaded method living for the VM's
        // life; an id-less row matches by name, so it also keeps a class
        // redeployed under the same name from inheriting a verdict about
        // bytecode it has never run. The no-VM fallback reaches every live
        // registry, where the same id DOES name different classes (every
        // `ClassStore` counts from 0) -- forgetting there costs at most one
        // compile attempt. (gc-common w36-c corrected "`ClassId`s are
        // recycled", which the w9-e handoff noted is not true within a VM.)
        self.forget_runtime_despec_for_class(class_id, class_name);
        for _ in 0..windows_to_close {
            self.core.close_branch_window();
        }
        if dropped > 0 {
            crate::metrics::record_scheduling_events(crate::metrics::SCHEDULING_EVENTS[1], dropped);
            self.core.dropped.fetch_add(dropped, Ordering::Release);
        }
        self.core.bump_settled_generation();
    }

    /// Batch form of [`Self::invalidate_class`] for a whole unloaded set, the
    /// one the VM's class-unload transaction calls (gc-common w36-c): ONE
    /// `methods.retain`, one `remove_matching` per lane, one
    /// `osr_denied.retain` and one pass over each verdict set, instead of one
    /// of each per class. The predicate is `MethodKey::belongs_to` asked of
    /// every member ([`crate::UnloadedClassSet::owns`]). Windows closed and
    /// drops counted as the loop would, summed. Proven equivalent to the loop
    /// by `invalidate_classes_matches_a_loop_of_invalidate_class`.
    pub fn invalidate_classes(&self, classes: &crate::UnloadedClassSet<'_>) {
        if classes.is_empty() {
            return;
        }
        let mut windows_to_close = 0u32;
        let dropped = {
            let mut methods = self.core.methods.lock();
            methods.retain(|key, state| {
                if !classes.owns(key.class_id, &key.class_name) {
                    return true;
                }
                windows_to_close += u32::from(state.branch_window_armed);
                false
            });
            // Same `methods` → `queue` order as `invalidate_class`.
            self.core
                .lanes
                .iter()
                .map(|lane| {
                    lane.queue
                        .lock()
                        .remove_matching(|task| {
                            classes.owns(task.method_key.class_id, &task.method_key.class_name)
                        })
                        .len() as u64
                })
                .sum::<u64>()
        };
        self.core
            .osr_denied
            .write()
            .retain(|key, _| !classes.owns(key.class_id, &key.class_name));
        self.core.verdicts.forget_for_classes(classes);
        self.core
            .verdicts
            .forget_cycle_members_of_unloaded_classes(classes);
        self.forget_runtime_despec_for_classes(classes);
        for _ in 0..windows_to_close {
            self.core.close_branch_window();
        }
        if dropped > 0 {
            crate::metrics::record_scheduling_events(crate::metrics::SCHEDULING_EVENTS[1], dropped);
            self.core.dropped.fetch_add(dropped, Ordering::Release);
        }
        self.core.bump_settled_generation();
    }

    /// [`Self::forget_runtime_despec_for_class`] for every member of
    /// `classes`. Still one `forget_for_class` per class: the registry's
    /// verdicts are private to `compile_gate.rs`, which this lane does not
    /// own; its table is bounded by the methods that reached
    /// `MakeNotCompilable` at runtime, which is small by construction. The
    /// one-pass form is
    /// `docs/feature-designs/jit-ir-optimizations.md`.
    fn forget_runtime_despec_for_classes(&self, classes: &crate::UnloadedClassSet<'_>) {
        for (class_id, class_name) in classes.iter() {
            self.forget_runtime_despec_for_class(class_id, class_name);
        }
    }

    /// A method's compiled body was withdrawn from the code cache, and NOT
    /// because anything it was compiled under turned out to be wrong — RT-8's
    /// cold-body sweep. Put it back at [`CompilationTier::Interpreter`] so it
    /// can be compiled again.
    ///
    /// # Why a withdrawal without this is worse than no withdrawal
    ///
    /// `current_tier` only ever advances, and it advances when a body is
    /// PUBLISHED. Nothing lowers it when one is removed, because until the
    /// sweeper every removal was an invalidation — after which the method is
    /// usually recompiled from a fresh request, or is genuinely gone. A swept
    /// method is neither. It goes back to the interpreter with
    /// `current_tier == C1`, and `should_compile`'s C1 -> C2 arm is the only
    /// one left open to it: that arm asks for ~20 000 invocations from
    /// `MethodState::invocation_count`, which has no writer once a C1 body has
    /// been published (see [`crate::entry_counter`]). So the method would be
    /// interpreted for the rest of the process, and the sweeper would have
    /// turned "reclaim cold code" into "permanently deoptimise whatever it
    /// touched" — silently, since nothing counts a method that simply never
    /// gets offered.
    ///
    /// # What is deliberately NOT reset
    ///
    /// Only the tier, and only downwards, and only from a tier above
    /// `Interpreter`. `ineligible`, `c2_bailout`, both fail counters and the
    /// trap counts were learned from bytecode that has not changed, so unlike
    /// [`Self::on_class_redefined`] there is nothing here to unlearn. The
    /// invocation count is kept for the same reason it is kept there: the
    /// method re-admits at its next stride rather than re-warming from zero,
    /// which matters more here, because a method worth sweeping is one whose
    /// count has stopped moving and would take a very long time to rebuild.
    ///
    /// Unknown keys are ignored: a body can be withdrawn for a method the
    /// manager has no state for at all (an eagerly compiled first call).
    pub fn note_body_withdrawn(&self, key: &MethodKey) {
        {
            let mut methods = self.core.methods.lock();
            if let Some(state) = methods.get_mut(key) {
                if state.current_tier != CompilationTier::Interpreter {
                    state.current_tier = CompilationTier::Interpreter;
                }
            }
        }
        // Demoting the state is half of it. A method at C2 was SETTLED, and its
        // call sites hold that stamp: until the generation moves they skip
        // this manager entirely, so the demotion above was never read and the
        // method stayed interpreted for good. The RT-8 soak (2026-09-18) found
        // it. A withdrawal is also room coming back, which the methods
        // deferred at the cap are waiting on.
        self.core.bump_settled_generation();
    }

    /// This manager as its VM's [`crate::JitBodyWithdrawalSink`], for
    /// `JitCache::install_withdrawal_sink` (tier proposal W3-1, round 12
    /// wave 4): every withdrawal from that cache then demotes the methods it
    /// left without a body, whichever door withdrew them. `Weak`, so the
    /// cache does not keep this manager's core alive.
    pub fn withdrawal_sink(&self) -> std::sync::Weak<dyn crate::JitBodyWithdrawalSink> {
        // Downgrade first, then coerce `Weak<CompilerCore>` to the trait object.
        let weak: std::sync::Weak<CompilerCore> = Arc::downgrade(&self.core);
        weak
    }

    /// A body for `key` was just PUBLISHED by a door this manager did not
    /// schedule: the optimizing doors' callee compiler, the eager callee chain,
    /// the caller-held synchronized callee compile. Raise the method's
    /// `current_tier` to `tier` (C1 for a single-pass body, C2 for an optimizing
    /// one); never lower it.
    ///
    /// Round 11 wave 18, P1 of `r11-orch-callee-compiled-bodies-are-never-promoted`.
    /// Such a method was left at `Interpreter`, so the hotness door reached it
    /// only through `should_compile`'s skip-to-C2 clause, and the drain's
    /// r9w3-tiering3 rule (a drained method at `Interpreter` is never
    /// recommended for C1) had to stand in for the missing fact "a body
    /// exists". Recorded here, `should_compile`'s C1 arm governs, as it does for
    /// a body the manager published itself; and an optimizing body is settled
    /// instead of being recommended a C2 recompile of itself once its entries
    /// cross the bar.
    ///
    /// # Why raising is safe
    ///
    /// A tier that only rises re-opens no compile, and every removal already
    /// lowers it: `note_body_withdrawn` (the sweeper), an evicting deopt
    /// (`on_deoptimization`), a redefinition, an unload. Raising can only settle
    /// a method (C2), which makes no live settled stamp stale, so the
    /// generation does not move. The caller must pass the tier of a body that
    /// IS published under `key` — not merely compiled: a tier recorded for a
    /// body `JitCache::put` refused would keep the interpreter off a method
    /// with nothing to run.
    ///
    /// Deliberately not charged to the time-to-tier instrument
    /// (`note_time_to_tier`): its "first body" stamp would pre-empt the
    /// nominate-to-first-body reading of a background compile finishing after
    /// this.
    ///
    /// `CRATONVM_JIT_CALLEE_PROMOTION=0` makes this a no-op. A key with no class
    /// identity is resolved to the one tracked state of that name, as
    /// `on_deoptimization` does; a new state is created otherwise, as the drain
    /// does for a body it sees first.
    ///
    /// Test-only since JIT round 22 wave 7 (lane review), with
    /// [`Self::note_published_outside_manager_with_c2_candidate`]: the VM calls
    /// [`Self::note_published_outside_manager_gated`].
    #[cfg(test)]
    pub fn note_published_outside_manager(&self, key: &MethodKey, tier: CompilationTier) {
        self.note_published_outside_manager_with_c2_candidate(key, tier, false, false);
    }

    /// [`Self::note_published_outside_manager`], also told whether the method
    /// passes `c2_upgrade_would_engage` (HANDLER8-1, JIT round 19 wave 9, lane
    /// handler9). The VM evaluates that only when
    /// [`outside_manager_c2_census_enabled`] or
    /// [`outside_manager_c2_upgrade_enabled`] is on; `false` otherwise, which
    /// is exactly [`Self::note_published_outside_manager`].
    ///
    /// For a C1-family publish with `c2_candidate`: the census counts the
    /// method once (and whether its count already stood at the C2 bar), and
    /// stamps it so a later crossing of the bar is counted
    /// ([`CompilerCore::note_outside_candidate_count`]); with
    /// `CRATONVM_JIT_OUTSIDE_MANAGER_C2_UPGRADE` on, the method is offered the
    /// manager path's `Low` C2 supersede at once, under the per-VM budget
    /// ([`CompilerCore::request_outside_manager_c2_upgrade`]). Nothing changes
    /// with both switches off.
    ///
    /// `profile_warm` (wave-9 follow-up, after the loaded `R16LocaleWide`
    /// stress failed 1/40 with the supersede armed, 0/40 without): the VM's
    /// answer to "has this method's interpreter profile at least the depth a
    /// manager-path C1 publish has behind it" (profile-store invocations >=
    /// `CRATONVM_JIT_OUTSIDE_MANAGER_C2_UPGRADE_MIN_PROFILE`, default the C1
    /// tier-up threshold). A manager-scheduled C1 publish, the only one
    /// `request_c2_upgrade` served before, is reached after that many
    /// interpreted calls; an outside one can come at 31 calls (the eager
    /// chain's warmth floor) or at the first call (the sync-direct door has no
    /// warmth gate), and a C2 built from that profile turns every branch the
    /// few calls never took into an uncommon trap. A candidate whose profile is
    /// thinner is counted (`profile-refused=`) and NOT superseded; the census
    /// still counts it. Test-only since JIT round 22 wave 7 (lane review).
    #[cfg(test)]
    pub fn note_published_outside_manager_with_c2_candidate(
        &self,
        key: &MethodKey,
        tier: CompilationTier,
        c2_candidate: bool,
        profile_warm: bool,
    ) {
        self.note_published_outside_manager_gated(key, tier, c2_candidate, profile_warm, true);
    }

    /// [`Self::note_published_outside_manager_with_c2_candidate`], also told
    /// whether the C1 body predicts that a C2 publish could forward its baked
    /// callers (`forward_ok`; HANDLER11-6, JIT round 19 wave 12, lane
    /// handler12, the VM's `CRATONVM_JIT_OUTSIDE_MANAGER_C2_UPGRADE_NEEDS_FORWARD`).
    /// A warm candidate whose forward is predicted refused is counted
    /// (`forward-refused=`) and not superseded: the callers that made it a
    /// candidate would keep running the C1 body (HANDLER9-2's `c1-kept-entrant=`).
    pub fn note_published_outside_manager_gated(
        &self,
        key: &MethodKey,
        tier: CompilationTier,
        c2_candidate: bool,
        profile_warm: bool,
        forward_ok: bool,
    ) {
        if tier == CompilationTier::Interpreter || !crate::entry_counter::callee_promotion_enabled()
        {
            return;
        }
        let c1_family = matches!(tier, CompilationTier::C1 | CompilationTier::C1WithProfiling);
        let upgrade_key = {
            let mut methods = self.core.methods.lock();
            let identified = identify_unidentified_key(&methods, key);
            let key = identified.as_ref().unwrap_or(key);
            let state = methods
                .entry(key.clone())
                .or_insert_with(|| MethodState::new(key.clone()));
            if state.current_tier < tier {
                state.current_tier = tier;
            }
            if c1_family {
                state.outside_c1_published = true;
                let census = &self.core.outside_c2;
                census.c1_publishes.fetch_add(1, Ordering::Relaxed);
                if c2_candidate && !state.outside_c2_candidate_seen {
                    state.outside_c2_candidate_seen = true;
                    census.candidates.fetch_add(1, Ordering::Relaxed);
                    let bar = self.core.c2_hotness_bar.load(Ordering::Relaxed);
                    if state.invocation_count >= bar {
                        census.over_bar_at_publish.fetch_add(1, Ordering::Relaxed);
                    } else {
                        state.outside_c2_candidate_ms = process_uptime_ms().max(1);
                    }
                }
            }
            let wanted = c1_family && c2_candidate && outside_manager_c2_upgrade_enabled();
            if wanted && !profile_warm {
                self.core
                    .outside_c2
                    .profile_refused
                    .fetch_add(1, Ordering::Relaxed);
            }
            if wanted && profile_warm && !forward_ok {
                self.core
                    .outside_c2
                    .forward_refused
                    .fetch_add(1, Ordering::Relaxed);
            }
            (wanted && profile_warm && forward_ok).then(|| key.clone())
        };
        // `request_c2_upgrade` takes `methods` itself.
        if let Some(key) = upgrade_key {
            self.core.request_outside_manager_c2_upgrade(&key);
        }
    }

    /// HANDLER9-2 (JIT round 19 wave 11, lane handler11; diagnostic, on the
    /// census switch, no switch of its own): a C2 body was published over a C1
    /// body of `key`, and the C1 body was (`c1_made_not_entrant`) or was not
    /// made not entrant. Counted only for a census candidate (an outside C1
    /// publish that passed `c2_upgrade_would_engage`) and only while the
    /// census or the supersede is armed, so the line says how many outside
    /// supersedes are live for the callers that baked the C1 body
    /// (`c1-forwarded=`) and how many are dead for them (`c1-kept-entrant=`):
    /// the question HANDLER9-2 asked of two drain reads, answered by the one
    /// fact that decides it (hm5's not-entrant forward). Takes `methods`.
    pub fn note_c2_supersede_of_outside_candidate(
        &self,
        key: &MethodKey,
        c1_made_not_entrant: bool,
    ) {
        if !(outside_manager_c2_census_enabled() || outside_manager_c2_upgrade_enabled()) {
            return;
        }
        let candidate = {
            let methods = self.core.methods.lock();
            let identified = identify_unidentified_key(&methods, key);
            let key = identified.as_ref().unwrap_or(key);
            methods
                .get(key)
                .is_some_and(|state| state.outside_c2_candidate_seen)
        };
        if !candidate {
            return;
        }
        let census = &self.core.outside_c2;
        census.c2_published.fetch_add(1, Ordering::Relaxed);
        if c1_made_not_entrant {
            census.c1_forwarded.fetch_add(1, Ordering::Relaxed);
        } else {
            census.c1_kept_entrant.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// HANDLER8-1's census line as it would print now (tests and embedders;
    /// the launcher prints it at exit through
    /// [`print_outside_manager_c2_census_at_exit`]). Test-only since JIT round 22 wave 7 (lane review).
    #[cfg(test)]
    pub fn outside_manager_c2_census_line(&self) -> String {
        self.core.outside_c2.line()
    }

    /// Print this manager's HANDLER8-1 census line now, once (a no-op unless
    /// the census DBG switch is armed).
    pub fn print_outside_manager_c2_census(&self) {
        self.core.outside_c2.print_once();
    }

    /// Forget the verdicts recorded against a class whose bytecode was just
    /// replaced — a JVMTI redefine or retransform, or a `defineClass` over an
    /// already-loaded name.
    ///
    /// `ineligible`, both fail counters, `c2_bailout`, the trap counts and any
    /// OSR denial were all learned from the OLD bytecode, and none of them is
    /// evidence about the new one. The method keeps its invocation count, so
    /// it re-admits at its next stride instead of re-warming from zero. Queued
    /// requests are left to the install-epoch gate, which the redefinition has
    /// already moved.
    ///
    /// Keyed like [`Self::invalidate_class`]: by identity when both the key and
    /// `class_id` carry one, so a same-named class in another loader keeps its
    /// verdicts; by name when either side has none (`ClassId(0)`), which errs
    /// towards resetting too much and costs at most a recompile.
    pub fn on_class_redefined(&self, class_id: ClassId, class_name: &str) {
        {
            let mut methods = self.core.methods.lock();
            for (key, state) in methods.iter_mut() {
                if !key.belongs_to(class_id, class_name) {
                    continue;
                }
                state.current_tier = CompilationTier::Interpreter;
                state.tier_fail_count = 0;
                state.osr_fail_count = 0;
                state.waiting_on_class_epoch = None;
                state.waiting_on_class_offers = 0;
                state.waiting_on_class_parks = 0;
                state.ineligible = false;
                state.entry_ineligible = false;
                state.c2_bailout = false;
                state.c2_budget_overruns = 0;
                state.deopt_count = 0;
                state.c2_withdrawals = 0;
                state.trap_counts.clear();
                // JIT r22 wave 3 (lane inline; item 1 of
                // `r22-review-w3-intrinsics-on-inline-FIXED-20261020.md`): an armed
                // acceptance re-offer judged the OLD bytecode; the new one
                // starts its own budget.
                state.acceptance_reoffer_bar = 0;
                state.acceptance_reoffers = 0;
                // JIT round 21 lane tiermgr (INDYCONST21-1): an owed retry was
                // about the old bytecode's body.
                state.retry_owed = None;
                // j27 lane SEEDOWE: so was an owed seed retry; the new
                // bytecode starts its own bound.
                state.ic_seed_owed = None;
                state.ic_seed_owes = 0;
                // t1 wave 11 lane JTIER (review): the optimizing OSR body that
                // proved a hot loop ran the OLD bytecode; under
                // `CRATONVM_JIT_C2_AFTER_OPTIMIZING_OSR` (opt-in) it promoted
                // the new bytecode's first C1 body straight to C2. Inert with
                // that switch off (the only reader).
                state.optimizing_osr_published = false;
            }
        }
        self.core
            .osr_denied
            .write()
            .retain(|key, _| !key.belongs_to(class_id, class_name));
        // The compile verdicts were measured on the old bytecode too. A JVMTI
        // redefinition also moves the registry's epochs
        // (`JitVerdictRegistry::note_redefinition`), but a `defineClass` over
        // a loaded name does not, so this is what retires them there; it also
        // releases the memory.
        self.core.verdicts.forget_for_class(class_id, class_name);
        // Same for the runtime de-speculation verdicts (N3). A speculation that
        // failed against the old bytecode is not evidence about the new body.
        self.forget_runtime_despec_for_class(class_id, class_name);
        self.core.bump_settled_generation();
    }

    /// Create a new manager with the default policy.
    pub fn with_default_policy() -> Self {
        Self::new(CompilationPolicy::default())
    }

    /// wire-tiered-manager Step 6: create a manager whose policy honors the
    /// `CRATONVM_TIER_*` environment overrides (see [`CompilationPolicy::from_env`]).
    /// An unset environment is identical to [`with_default_policy`].
    /// `SharedVm::new` uses [`Self::with_env_policy_and_vm_epochs`].
    pub fn with_env_policy() -> Self {
        Self::new(CompilationPolicy::from_env())
    }

    /// [`Self::with_env_policy`], stamping OSR denials and queued requests
    /// with `source` — the owning VM's code-state epoch, which that VM
    /// advances on its own redefinitions and code-cache flushes — instead of
    /// the process-wide install epoch (interpreter round i1 wave 14). What
    /// `SharedVm::new` builds.
    pub fn with_env_policy_and_install_epoch_source(source: Arc<AtomicU64>) -> Self {
        Self::with_install_epoch_source(CompilationPolicy::from_env(), Some(source))
    }

    /// [`Self::with_env_policy_and_install_epoch_source`], also sharing the
    /// owning VM's redefine epoch with its verdict registry
    /// ([`Self::with_epoch_sources`]). What `SharedVm::new` builds since
    /// interpreter round i1 wave 17. `runtime_despec` is the same VM's runtime
    /// de-speculation registry, the only one this manager's class unloads and
    /// redefinitions forget verdicts in (interpreter round i1 wave 18).
    pub fn with_env_policy_and_vm_epochs(
        code_state_epoch: Arc<AtomicU64>,
        redefine_epoch: Arc<AtomicU32>,
        runtime_despec: Arc<crate::compile_gate::RuntimeDespecRegistry>,
    ) -> Self {
        let mut manager = Self::with_epoch_sources(
            CompilationPolicy::from_env(),
            Some(code_state_epoch),
            Some(redefine_epoch),
        );
        manager.runtime_despec = Some(runtime_despec);
        manager
    }

    // ── Invocation hooks ─────────────────────────────────────────────────

    /// Called on each method invocation from the interpreter.
    /// Increments the counter and checks if compilation should be triggered.
    /// Returns the target tier if compilation was enqueued.
    pub fn on_method_invocation(&self, key: &MethodKey) -> Option<CompilationTier> {
        self.on_method_invocation_observed(key, 0)
    }

    /// [`Self::on_method_invocation`], but fast-forwarded to an externally
    /// observed invocation count.
    ///
    /// The interpreter counts every invocation in its own profile store but
    /// consults the tiered manager only at stride boundaries (the
    /// `JIT_RETRY_STRIDE = 64` schedule past the warmup threshold). With the
    /// plain `+= 1` counting, the manager's view of "hotness" was therefore
    /// 64x DEFLATED: a method needed `c1_threshold × 64` real invocations past
    /// warmup before the manager recommended its first C1 compile (observed
    /// live, when `c1_threshold` was 200: `tiered-enqueue … invoc_count=13236`
    /// for a `CRATONVM_JIT_THRESHOLD=500` run). Passing the interpreter's real
    /// per-method count lets the recommendation fire at the intended
    /// thresholds. `observed_count == 0` (or a stale/smaller value) degrades
    /// to the historical `+= 1` behaviour.
    pub fn on_method_invocation_observed(
        &self,
        key: &MethodKey,
        observed_count: u64,
    ) -> Option<CompilationTier> {
        self.on_method_invocation_settling(key, observed_count)
            .recommended
    }

    /// [`Self::on_method_invocation_observed`], also reporting whether the
    /// method's tiering is settled — see [`InvocationVerdict`].
    ///
    /// This is the door the interpreter's hooks use. A method that will never
    /// compile used to take this global mutex and build a key at every stride
    /// for the life of the process; the settled stamp lets the call site stop
    /// asking until something changes.
    pub fn on_method_invocation_settling(
        &self,
        key: &MethodKey,
        observed_count: u64,
    ) -> InvocationVerdict {
        self.invocation_verdict(key, observed_count, true, None)
    }

    /// [`Self::on_method_invocation_settling`] for a caller that is REPORTING
    /// invocations counted elsewhere, not making one: the same fast-forward to
    /// `observed_count` and the same recommendation/enqueue, but without the
    /// `+= 1` the invocation hook charges for the call itself.
    ///
    /// The VM's compiled-prologue entry-counter drain
    /// (`drain_jit_entry_counters`) is the caller: a drain is not an
    /// invocation, and through the settling door every drain of every counted
    /// method was one phantom invocation. `observed_count == 0` (or a value
    /// not above the manager's own count) leaves the count exactly as it was.
    ///
    /// One recommendation differs from the hook's (round 9 wave 3): a method
    /// the manager still has at `Interpreter` is never recommended for C1
    /// here, only straight for C2 once it crosses the C2 bar. A drained
    /// method has a published counted body by construction, so a C1 request
    /// would recompile the body it already runs.
    ///
    /// Test-only since JIT round 22 wave 7 (lane review): the VM's drain calls
    /// [`Self::on_observed_invocations_with_back_edges`].
    #[cfg(test)]
    pub fn on_observed_invocations(
        &self,
        key: &MethodKey,
        observed_count: u64,
    ) -> InvocationVerdict {
        self.invocation_verdict(key, observed_count, false, None)
    }

    /// [`Self::on_observed_invocations`], also handed the body's RAW back-edge
    /// word as the drain read it (HANDLER8-2, JIT round 19 wave 9, lane
    /// handler9). Recorded on the method's state before anything else, so the
    /// `CRATONVM_DBG_TIER_ENQUEUE` line of an enqueue this report causes (or a
    /// later one) prints it as `drained-be=`. Diagnostic only: no decision
    /// reads it.
    pub fn on_observed_invocations_with_back_edges(
        &self,
        key: &MethodKey,
        observed_count: u64,
        back_edges: u64,
    ) -> InvocationVerdict {
        self.invocation_verdict(key, observed_count, false, Some(back_edges))
    }

    fn invocation_verdict(
        &self,
        key: &MethodKey,
        observed_count: u64,
        count_this_call: bool,
        drained_back_edges: Option<u64>,
    ) -> InvocationVerdict {
        // Read BEFORE deciding, so a concurrent bump (a deopt landing while
        // this runs) expires the stamp this call hands out.
        let generation = self.core.settled_generation.load(Ordering::Acquire);
        let mut methods = self.core.methods.lock();
        let state = methods
            .entry(key.clone())
            .or_insert_with(|| MethodState::new(key.clone()));
        if drained_back_edges.is_some() {
            state.last_drained_back_edges = drained_back_edges;
        }
        if count_this_call {
            state.invocation_count = state.invocation_count.saturating_add(1);
            // HANDLER8-1 census: a candidate crossing the bar by stride.
            self.core.note_outside_candidate_count(state);
        } else if observed_count <= state.invocation_count {
            // r11-tier: a report that says nothing new. Since round 11 wave 2
            // the drain reports only a body whose counter MOVED since the last
            // drain (`CompiledMethod::entry_count_if_moved`), but a moved
            // counter can still sit at or below this count: the compiled
            // counter starts at zero on publication, while the manager's count
            // already holds the method's interpreted history (and a recompile
            // restarts it again). Such a report, whose verdict the drain
            // discards, could only re-derive an answer this count already
            // produced, so it skips the policy lock and `should_compile_inner`.
            // A count that exceeds the manager's, and every interpreter
            // stride, still take them.
            return InvocationVerdict {
                recommended: None,
                settled_generation: 0,
            };
        }
        if observed_count > state.invocation_count {
            state.invocation_count = observed_count;
            // HANDLER8-1 census: a candidate crossing the bar by the drain.
            self.core.note_outside_candidate_count(state);
        }

        if state.queued_for_compilation {
            return InvocationVerdict {
                recommended: None,
                settled_generation: 0,
            };
        }

        // JIT r22 wave 2, lane inline (INLINE22-1): an armed acceptance
        // re-offer whose bar the live count has reached. Under the `methods`
        // lock, so it is `request_c2_retry`'s body inline (that function takes
        // the lock itself).
        if state.acceptance_reoffer_bar != 0
            && state.invocation_count >= state.acceptance_reoffer_bar
            && !self.core.tiering_is_disabled()
        {
            state.acceptance_reoffer_bar = 0;
            if let Some(verdict) = self.fire_acceptance_reoffer(state) {
                return verdict;
            }
        }

        // j27 lane SEEDOWE (`CRATONVM_JIT_IR_IC_SEED_OWED_RETRY`): an owed
        // inline-cache seed retry whose refusal lifted. One `Option` test when
        // nothing is owed.
        if let Some(verdict) = ic_seed_owed::pay(&self.core, state) {
            return verdict;
        }

        // JIT r25 wave 4 (`CRATONVM_JIT_OUTSIDE_C1_DRAIN_C2`, default ON since j27): a C1 body
        // published outside the manager is promoted once its DRAINED entry
        // count reaches HotSpot's `Tier4InvocationThreshold`, instead of the
        // 20 000-invocation bar the drain otherwise needs.
        if !count_this_call
            && state.outside_c1_published
            && matches!(
                state.current_tier,
                CompilationTier::C1 | CompilationTier::C1WithProfiling
            )
            && state.invocation_count >= OUTSIDE_C1_DRAIN_C2_BAR
            && !self.core.tiering_is_disabled()
            && outside_c1_drain_c2_enabled()
            && self.core.promote_outside_c1_by_drain(state)
        {
            if cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_JITC") {
                eprintln!(
                    "[cratonvm-jitc] outside-c1-drain-c2 ENQUEUED {}.{}{}: drained invocations={}",
                    key.class_name, key.method_name, key.descriptor, state.invocation_count,
                );
            }
            return InvocationVerdict {
                recommended: Some(CompilationTier::C2),
                settled_generation: 0,
            };
        }

        let policy = self.policy.lock();
        if !policy.tiered_enabled {
            // Settled until `set_policy` moves the generation.
            return InvocationVerdict {
                recommended: None,
                settled_generation: generation,
            };
        }

        // r9w3-tiering3: a drain reports a method that HAS a published,
        // counted (single-pass) body -- the counter lives in that body's
        // prologue. If the manager still has it at `Interpreter`, the body
        // came through a door that bypasses the manager (the synchronous
        // compile path, the eager first-call door), and a C1 request for it
        // is a recompile of the body it already runs. Measured on javac
        // compiling 137 files with `CRATONVM_JIT_ENTRY_COUNTER=1`: the manager
        // tracked 3 618 methods against 758, ran `c1=2254` compiles against
        // `733`, and its c1 lane waited 6 203 ms in total against 154. The
        // drain door may take such a method only straight to C2 (the
        // `Interpreter` arm's own skip-to-C2 clause), which is the hotness
        // promotion the counter exists for. The invocation hook
        // (`count_this_call`) is untouched: an interpreted method is exactly
        // what the C1 arm is for. Since round 11 wave 18 the callee compiler,
        // the eager chain and the synchronized callee compile record their
        // publish (`note_published_outside_manager`), so their methods reach
        // the C1 arm instead; this rule still covers the doors that do not.
        if !count_this_call
            && state.current_tier == CompilationTier::Interpreter
            && matches!(
                self.should_compile(state, &policy),
                Some(CompilationTier::C1 | CompilationTier::C1WithProfiling)
            )
        {
            return InvocationVerdict {
                recommended: None,
                settled_generation: 0,
            };
        }
        let recommended = self.should_compile_inner(state, &policy);
        // nio21 wave 15 (lane L3): why a drained, counted body that has
        // crossed the C2 bar is NOT promoted. `TreeMap.addEntry` and
        // `fixAfterInsertion` stayed single-pass for a whole `tmapcmp` run with
        // nothing on stderr to say which refusal held them (`addEntry` carries
        // a `new`, so the structural C1->C2 door, `c2_upgrade_would_engage`,
        // refuses it by policy and this hotness door is its only way up).
        // Report-only; reads nothing the decision did not already read.
        if !count_this_call
            && recommended.is_none()
            && cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_TIER_PROMOTE")
        {
            let bar = u64::from(policy.c2_threshold).max(u64::from(policy.c2_min_invocations));
            if state.invocation_count >= bar && state.current_tier < CompilationTier::C2 {
                eprintln!(
                    "[cratonvm-tier] drain NOT promoted {}.{}{} tier={:?} invocations={} bar={} \
                     queued={} c2_bailout={} ineligible={} entry_ineligible={} tier_fail={} \
                     deferred_at_cap={}",
                    key.class_name,
                    key.method_name,
                    key.descriptor,
                    state.current_tier,
                    state.invocation_count,
                    bar,
                    state.queued_for_compilation,
                    state.c2_bailout,
                    state.ineligible,
                    state.entry_ineligible,
                    state.tier_fail_count,
                    state.deferred_at_cap,
                );
            }
        }
        // `deferred_at_cap` settles the method until room may have come back,
        // which moves the generation this stamp is checked against.
        let settled =
            recommended.is_none() && (state.tiering_is_settled() || state.deferred_at_cap);
        InvocationVerdict {
            recommended,
            settled_generation: if settled { generation } else { 0 },
        }
    }

    /// Whether a stamp from [`InvocationVerdict::settled_generation`] still
    /// holds. One atomic load; `0` is never settled.
    #[inline]
    pub fn tiering_settled(&self, stamp: u32) -> bool {
        stamp != 0 && stamp == self.core.settled_generation.load(Ordering::Acquire)
    }

    /// Ask for the ONE extra optimizing attempt a method is owed after its IR
    /// build bailed on a `new` whose class had not loaded yet.
    ///
    /// The worker loop reaches the same request through `CompileOutcome`, but
    /// the worker is not the only compile door: the eager first-call door in
    /// `jit_bridge` reaches the backend directly and produces no
    /// `CompileOutcome` at all, so a method compiled there would never see its
    /// memo spent. This is that door's route in.
    ///
    /// The one-shot memo (`cratonvm_jit::take_deferred_new_retry`) is what the
    /// CALLER must consume before calling this, so both doors together can
    /// still only produce one extra compile per method.
    ///
    /// JIT round 21 lane indyconst (`CRATONVM_JIT_ONE_SHOT_RETRY_OWED`):
    /// returns the retry door's answer, so a door whose memo is already spent
    /// can owe an `AlreadyQueued` retry to the task holding the slot.
    pub fn request_deferred_new_retry(&self, key: &MethodKey) -> C2RetryAnswer {
        self.core.request_deferred_new_retry(key)
    }

    /// JIT r22 wave 2, lane inline (INLINE22-1, [`acceptance_reoffer_enabled`]):
    /// arm one C2 re-offer for a method whose C2 task just published its
    /// single-pass fall-through after the acceptance gate refused the
    /// optimizing body. The bar is [`ACCEPTANCE_REOFFER_GROWTH`] times the
    /// method's invocation count now (at least [`ACCEPTANCE_REOFFER_MIN_BAR`]);
    /// the drain fires it (`fire_acceptance_reoffer`). At most
    /// [`MAX_ACCEPTANCE_REOFFERS`] arms per method; a no-op with the switch off.
    /// Returns whether a re-offer was armed.
    pub fn arm_acceptance_reoffer(&self, key: &MethodKey) -> bool {
        if !acceptance_reoffer_enabled() {
            return false;
        }
        let mut methods = self.core.methods.lock();
        let Some(state) = methods.get_mut(key) else {
            return false;
        };
        if state.acceptance_reoffers >= MAX_ACCEPTANCE_REOFFERS || state.c2_bailout {
            return false;
        }
        state.acceptance_reoffers += 1;
        // JIT r22 wave 3 (item 2 of `r22-review-w3-intrinsics-on-inline-FIXED-20261020.md`):
        // the bar is compared with `invocation_count`, which the drain moves
        // only once the fall-through body's OWN entry counter (zero at its
        // publication) passes the manager's history. So the bar is in units
        // of NEW calls on that body: `(GROWTH - 1) x N` new calls make the
        // method's total `GROWTH x N`, the growth the constant names. A body
        // built without the entry counter (`CRATONVM_JIT_ENTRY_COUNTER=0`)
        // reports nothing, and its re-offer never fires.
        state.acceptance_reoffer_bar = state
            .invocation_count
            .max(1)
            .saturating_mul(ACCEPTANCE_REOFFER_GROWTH.saturating_sub(1).max(1))
            .max(ACCEPTANCE_REOFFER_MIN_BAR);
        // Item 1 of the same page: the bytecode this re-offer judged.
        state.acceptance_reoffer_epoch = self.core.verdicts.redefine_epoch.load();
        if cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_JITC") {
            eprintln!(
                "[cratonvm-jitc] acceptance-reoffer ARMED {}.{}{}: invocations={} bar={} (#{})",
                key.class_name,
                key.method_name,
                key.descriptor,
                state.invocation_count,
                state.acceptance_reoffer_bar,
                state.acceptance_reoffers,
            );
        }
        true
    }

    /// The armed re-offer's request, made under the `methods` lock by the
    /// drain (`invocation_verdict`): the gates of
    /// `CompilerCore::request_c2_retry` (queued is checked by the caller), the
    /// IR refusal memo's one re-judgement, then `admit`. `Some` when a C2 task
    /// was queued.
    fn fire_acceptance_reoffer(&self, state: &mut MethodState) -> Option<InvocationVerdict> {
        let dbg = cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_JITC");
        let key = state.method_key.clone();
        // JIT r22 wave 3 (item 1 of `r22-review-w3-intrinsics-on-inline-FIXED-20261020.md`):
        // only for the method still at C2 on the bytecode the refusal judged.
        // A redefinition clears the bar (`on_class_redefined`); this is the
        // fire-side twin, which also covers a tier reset by any other path
        // (a withdrawal demote) that leaves the bar armed.
        if state.current_tier < CompilationTier::C2
            || state.acceptance_reoffer_epoch != self.core.verdicts.redefine_epoch.load()
        {
            if dbg {
                eprintln!(
                    "[cratonvm-jitc] acceptance-reoffer DROPPED {}.{}{}: tier {:?} or the bytecode changed since the arm",
                    key.class_name, key.method_name, key.descriptor, state.current_tier
                );
            }
            return None;
        }
        if state.c2_bailout
            || state.ineligible
            || state.entry_ineligible
            || state.entry_out_of_retries()
        {
            if dbg {
                eprintln!(
                    "[cratonvm-jitc] acceptance-reoffer REFUSED {}.{}{}: c2_bailout / ineligible / retries",
                    key.class_name, key.method_name, key.descriptor
                );
            }
            return None;
        }
        let granted = self.core.verdicts.grant_ir_reoffer(
            &key.class_name,
            &key.method_name,
            &key.descriptor,
            key.class_id,
        );
        let queued = self.core.admit_one_shot_c2(state);
        if dbg {
            eprintln!(
                "[cratonvm-jitc] acceptance-reoffer {} {}.{}{}: invocations={} memo-regranted={}",
                if queued { "ENQUEUED" } else { "REFUSED (admit)" },
                key.class_name,
                key.method_name,
                key.descriptor,
                state.invocation_count,
                granted,
            );
        }
        queued.then_some(InvocationVerdict {
            recommended: Some(CompilationTier::C2),
            settled_generation: 0,
        })
    }

    /// Round 15 wave 4 (lane sync3, SY2-2): one more optimizing compile of a
    /// `static synchronized` method whose published body is wrapped because its
    /// compiles ran before any mutator minted its class's mirror slot. The VM
    /// caller bounds it (`jit_bridge` `request_static_self_lock_recompile`: at
    /// most 3 per method per redefinition epoch, one per wrapped body), not the
    /// deferred-`new` one-shot memo. Same C2 retry as
    /// [`Self::request_deferred_new_retry`], under its own `CRATONVM_DBG_JITC`
    /// name (round 15 wave 5, lane sync4).
    pub fn request_self_lock_recompile(&self, key: &MethodKey) {
        self.core.request_c2_retry(key, "static self-lock");
    }

    /// JIT round 16 wave 10 (lane indyir, IN16-8): one more optimizing compile
    /// of a method one of whose `invokedynamic` sites has just bound a
    /// constant call site's direct target, which its optimizing body may now
    /// compile as a plain call (`CRATONVM_JIT_INDY_CONSTANT_INVOKE`). The C1 ->
    /// C2 upgrade is queued as the C1 body publishes, before that body has run
    /// the bridge that learns the target. The VM caller bounds it (one request
    /// per bridge site, ever). The same C2 retry as
    /// [`Self::request_deferred_new_retry`], under its own `CRATONVM_DBG_JITC`
    /// name.
    ///
    /// JIT round 21 lane indyconst (`CRATONVM_JIT_INDY_CONSTANT_BRIDGE_RECOMPILE`):
    /// returns the retry door's answer. The VM's first-bind request only says
    /// it; the constant bridge's bounded re-asks (powers of two of the bridged
    /// calls, a few queued per site) count only `Enqueued`, so an
    /// `AlreadyQueued` answer is asked again.
    pub fn request_indy_constant_recompile(&self, key: &MethodKey) -> C2RetryAnswer {
        self.core.request_c2_retry(key, "indy-constant")
    }

    /// JIT round 21 lane reflect (`CRATONVM_JIT_INDY_MUTABLE_GUARD_BRIDGE_RECOMPILE`):
    /// one more optimizing compile of a method whose `MutableCallSite` /
    /// `VolatileCallSite` site keeps reaching its bridge for the very target
    /// the site's newest guard cell plans, i.e. no published body carries that
    /// guard. The VM caller bounds it (a few queued requests per site, asked at
    /// powers of two of the bridged calls) and re-asks after `AlreadyQueued`,
    /// hence the answer. The same C2 retry as [`Self::request_deferred_new_retry`],
    /// under its own `CRATONVM_DBG_JITC` name.
    pub fn request_indy_mutable_guard_recompile(&self, key: &MethodKey) -> C2RetryAnswer {
        self.core.request_c2_retry(key, "indy-mutable-guard")
    }

    /// JIT round 16 wave 14 (lane throw4, TP16-20): one more optimizing compile
    /// of a method whose published body left a statically bound call on the
    /// dispatch helper because the callee's class was not loaded (or, for a
    /// static callee, not initialized) when it was compiled -- a compile-order
    /// accident, not a policy refusal -- now that the dispatch helper has served
    /// that callee from a compiled body, so the recompile binds it directly.
    /// The VM caller bounds it (`CRATONVM_JIT_LATE_BIND_RECOMPILE`, one request
    /// per recorded refusal). The same C2 retry as
    /// [`Self::request_deferred_new_retry`], under its own `CRATONVM_DBG_JITC`
    /// name.
    ///
    /// JIT round 20 (lane stash, EAGER2-2): returns the retry door's answer, so
    /// the caller keeps its pending mark only when a compile will run.
    pub fn request_late_bind_recompile(&self, key: &MethodKey) -> C2RetryAnswer {
        self.core.request_c2_retry(key, "late-bind")
    }

    /// nio21 wave 16 L4 proposal P1 (built JIT r22 wave 2, lane inline): the
    /// inline-cache profile seed's one recompile (`note_ic_profile_seed` in
    /// `vm/src/jit/helpers.rs`), the same C2 retry as
    /// [`Self::request_late_bind_recompile`] under its own name, so the
    /// `CRATONVM_DBG_JITC` line reads `ic-profile-seed retry ..` instead of
    /// `late-bind retry ..`. No behaviour change.
    pub fn request_ic_seed_recompile(&self, key: &MethodKey) -> C2RetryAnswer {
        self.core.request_c2_retry(key, "ic-profile-seed")
    }

    /// JIT r25 wave 2 (`CRATONVM_JIT_TRAP_RECOMPILE_KEEPS_C2`, default ON since j27; see
    /// [`trap_recompile_keeps_c2_enabled`]): the eager re-queue of a method
    /// whose OPTIMIZING body a trap just evicted asks for C2 again, through
    /// the same gated retry door as the other one-shot recompiles (refused
    /// under `c2_bailout`, an ineligible method, a spent retry budget or the
    /// hotness gate). The caller falls back to its C1 re-queue on a refusal.
    pub fn request_trap_recompile_c2(&self, key: &MethodKey) -> C2RetryAnswer {
        self.core.request_c2_retry(key, "trap-recompile-c2")
    }

    /// Round 15 wave 5 (lane osrtier, OT-1 of `jit-r15w5-osrtier-proposals-RETIRED-20261003.md`):
    /// a METHOD-ENTRY optimizing compile of a method whose loop got an accepted
    /// optimizing OSR body the door cannot enter (no entry stub at its header),
    /// so the method's next activations run an optimizing body instead of
    /// re-entering the single-pass OSR body every call. The VM caller gates it
    /// (`CRATONVM_JIT_OSR_UNENTERABLE_PROMOTE`, default off) and bounds it (the
    /// door's refusal memo builds each body once per redefinition epoch; no
    /// request while an optimizing method-entry body is published). The same C2
    /// retry as [`Self::request_deferred_new_retry`], under its own
    /// `CRATONVM_DBG_JITC` name.
    pub fn request_osr_unenterable_promotion(&self, key: &MethodKey) {
        self.core.request_c2_retry(key, "osr-unenterable");
    }

    /// wire-tiered-manager Step 5 (precise background OSR): request an OSR
    /// compilation for a method whose loop the *caller* has already judged hot.
    ///
    /// This enqueues **immediately** — the interpreter's per-frame back-edge
    /// schedule (`Frame::should_try_osr`) is the throttle. (A counting
    /// `on_backedge` twin that fired at `policy.osr_threshold` existed beside
    /// this with no production caller and an unreachable branch; it was
    /// deleted.) It is idempotent: a no-op (returns `None`) if an OSR request
    /// for the method is already in flight (counted in
    /// `CompilationStats::osr_refused_in_flight`), it has bailed out of C2, or
    /// its OSR is denied at the current install epoch. Neither method-entry C2
    /// nor a method-entry request still in flight suppresses this request:
    /// OSR bodies live in an independent cache and hold their own in-flight
    /// slot. The enqueued task
    /// carries `osr_bci` so the background worker compiles an OSR-enterable
    /// artifact; the mutator enters it once published.
    pub fn request_osr(&self, key: &MethodKey, bci: u32) -> Option<CompilationTask> {
        match self.request_osr_classified(key, bci) {
            OsrRequest::Queued(task) => Some(task),
            OsrRequest::Pending | OsrRequest::Refused => None,
        }
    }

    /// [`Self::request_osr`], saying WHY nothing was queued. The two `None`s
    /// of `request_osr` call for opposite reactions from the back edge: a held
    /// OSR slot (or a full code cache) means "wait, re-probe later", while a
    /// method this manager has given up on -- denied at the current install
    /// epoch, `c2_bailout`, `ineligible`, either retry budget spent, or tiered
    /// compilation off -- will refuse every re-offer until a redefinition or
    /// an install-epoch move resets it. The interpreter's background OSR door
    /// retires the loop header on `Refused` instead of re-offering it every
    /// `osr_threshold` back edges for the life of the process
    /// (`interpreter-L7-bg-osr-offers-continue-after-the-worker-gives-up-FIXED`).
    pub fn request_osr_classified(&self, key: &MethodKey, bci: u32) -> OsrRequest {
        if self.core.is_osr_denied(key) {
            return OsrRequest::Refused;
        }
        let mut methods = self.core.methods.lock();
        let state = methods
            .entry(key.clone())
            .or_insert_with(|| MethodState::new(key.clone()));
        state.backedge_count = state.backedge_count.saturating_add(1);

        let policy = self.policy.lock();
        if !policy.tiered_enabled {
            return OsrRequest::Refused;
        }
        if state.osr_in_flight {
            // The method's OSR slot is held -- its own OSR request is queued or
            // compiling. Counted, because this refusal used to be invisible:
            // with one slot per method it also fired for the whole of the
            // method's C1 and C2-supersede pipeline. A method-entry request no
            // longer refuses this one; see `MethodState::queued_for_compilation`.
            self.core
                .stats
                .osr_refused_in_flight
                .fetch_add(1, Ordering::Relaxed);
            return OsrRequest::Pending;
        }
        if state.c2_bailout
            // Same "give up after repeated failures" convention as
            // `should_compile`/`request_c2_upgrade`: without this, a method
            // whose OSR artifact compile keeps returning `published=false`
            // (e.g. an uninlinable callee) has `current_tier` permanently
            // stuck below C2 and its slot released by `finish` after each
            // failure — so the very next hot back-edge re-enqueues an OSR
            // task, forever, with no diagnostic. This is the OSR-request twin
            // of the plain background-compile bail-listing gap (see the
            // `try_compile` ldc/ldc2_w fix): observed as a silent hang where
            // the same method (`TestResponsePerformance.doHomebrew`) kept
            // getting "bg-compile ... osr_bci=..." re-attempted every stride
            // while never completing even one 1M-iteration measurement pass.
            // A TRANSIENT OSR compile failure is retried through exactly this
            // budget rather than denied outright.
            //
            // `osr_fail_count` is the OSR pipeline's own budget since the two
            // were split; `tier_fail_count` is still asked so the coupling this
            // gate always had is kept. `CompilerCore::admit` asks the same pair
            // (this early check exists to avoid BUILDING the task).
            || state.ineligible
            // JIT r22 (lane inline): the same budget `admit` asks, through
            // the one spelling (`CRATONVM_JIT_OSR_OWN_FAIL_BUDGET`).
            || osr_out_of_retries(state, self.core.osr_own_fail_budget, self.core.osr_budget_trace)
        {
            return OsrRequest::Refused;
        }

        let task = CompilationTask {
            method_key: key.clone(),
            target_tier: CompilationTier::C2,
            priority: CompilationPriority::High,
            enqueue_time_ms: 0,
            osr_bci: Some(bci),
        };
        if !self.core.admit(state, task.clone()) {
            // With `methods` held and the slot and give-up gates asked above,
            // `admit` can only have refused at the code-cache cap, which a
            // sweep lifts: a deferral, not a verdict.
            return OsrRequest::Pending;
        }
        // `osr_compilations` is counted by `CompilerCore::finish` when the
        // artifact PUBLISHES, not here when it is merely queued.
        OsrRequest::Queued(task)
    }

    /// Whether OSR is denied for this method at the current install epoch.
    ///
    /// A denial is a verdict about the code that was loaded when it was made,
    /// so it expires when the JIT install epoch moves (a redefinition or a
    /// code-cache flush) and is forgotten outright by [`Self::on_class_redefined`]
    /// and [`Self::invalidate_class`]. Before this it was a process-global,
    /// name-keyed set that one failed background compile populated for good.
    ///
    /// History, because it explains why nothing is denied statically any
    /// more: `java/util/DualPivotQuicksort.sort` was denied here for one day
    /// (05f6930e) as a mitigation for a garbage-index corruption whose real
    /// cause (a8c5825d, the IR lowerer's unallocated-slot miscompile) was found
    /// the same day; with that fixed the deny was pure cost on a per-back-edge
    /// path and was removed. Only the dynamic, per-manager denials remain.
    pub fn is_osr_denied(&self, key: &MethodKey) -> bool {
        self.core.is_osr_denied(key)
    }

    /// Deny OSR for this method until the install epoch moves. Reserve it for
    /// verdicts the current bytecode cannot change; a transient failure should
    /// be reported as a failed compile, which is retried.
    pub fn mark_osr_denied(&self, key: MethodKey) {
        self.core.mark_osr_denied(key);
    }

    /// Drop every OSR denial this manager holds.
    pub fn clear_osr_denials(&self) {
        self.core.osr_denied.write().clear();
    }

    // ── Compilation queue ────────────────────────────────────────────────

    /// Enqueue a compilation task. Returns whether it was queued.
    ///
    /// Refused when the method already holds the in-flight slot this task
    /// needs (method-entry or OSR, by `task.osr_bci`), is
    /// `ineligible`, or has exhausted [`MAX_TIER_FAIL_RETRIES`] — all three now
    /// asked by [`CompilerCore::admit`] itself, so this door (the VM's eager
    /// deopt re-queue) can no longer re-enqueue a method the other doors have
    /// given up on.
    pub fn enqueue_compilation(&self, mut task: CompilationTask) -> bool {
        let mut methods = self.core.methods.lock();
        // A by-name door (the VM's deopt re-queue) may hand in a key without
        // class identity; admitting it verbatim created a second, id-0 state
        // beside the real one. See `identify_unidentified_key`.
        if let Some(real) = identify_unidentified_key(&methods, &task.method_key) {
            task.method_key = real;
        }
        let state = methods
            .entry(task.method_key.clone())
            .or_insert_with(|| MethodState::new(task.method_key.clone()));
        self.core.admit(state, task)
    }

    /// Dequeue the next compilation task (highest priority first, across both
    /// lanes), ignoring install-epoch stamps.
    pub fn dequeue_compilation(&self) -> Option<CompilationTask> {
        self.core.pop_across_lanes(None, &mut Vec::new())
    }

    /// Notify that compilation completed successfully at `tier`. A thin
    /// synchronous wrapper over `CompilerCore::complete_task` (always
    /// reports `success = true` — this API has no failure-reporting caller
    /// today; production code goes through the background workers'
    /// `compiler_loop`, which threads a real success/failure bool through).
    pub fn compilation_complete(
        &self,
        key: &MethodKey,
        tier: CompilationTier,
        compile_time_ms: u64,
    ) {
        self.core
            .complete_task(key, tier, compile_time_ms, true, false, false);
    }

    // ── Deoptimization ───────────────────────────────────────────────────

    /// Record a deoptimization of `key` at `bci`, with the action the deopt log
    /// recommended for it.
    ///
    /// HotSpot-style trap accounting:
    ///
    ///  * Every deopt is counted in `stats.deoptimizations`.
    ///  * A deopt that evicted the method-entry body
    ///    ([`deopt_evicts_method_body`]) drops `current_tier` to the
    ///    interpreter, because nothing is compiled any more.
    ///  * Only a counted trap ([`deopt_is_counted_trap`]) is charged to the
    ///    method. It drops any request still queued for it — formed against
    ///    the profile that just proved wrong, and leaving it queued is how a
    ///    second request got admitted beside it — and the method stops being
    ///    offered to C2 (`c2_bailout`) once one `(reason, bci)` reaches
    ///    [`PER_BCI_TRAP_LIMIT`] (not under
    ///    `CRATONVM_JIT_PER_BCI_TRAP_LIMIT_KEEPS_C2`) or the whole method
    ///    reaches [`PER_METHOD_TRAP_CUTOFF`] ([`counted_trap_bails_c2`]).
    ///    Both counts halve over time ([`TRAP_DECAY_INVOCATIONS`],
    ///    [`TRAP_DECAY_MS`]), so traps spread thinly over a long run never
    ///    reach either bar. Once set, though, `c2_bailout` stays for the life
    ///    of the method: decay does not clear it, only a redefinition
    ///    ([`Self::on_class_redefined`]) does.
    ///  * `MakeNotCompilable` also records the method as ineligible FOR METHOD
    ///    ENTRY (`MethodState::entry_ineligible`): the VM has just put it on
    ///    its skip list, which the OSR door deliberately does not ask.
    ///
    /// Every deopt used to be charged, whatever its action, and three of them
    /// set `c2_bailout` for good — which also blocks `request_osr`. Soft OSR
    /// loop exits alone were enough to take a hot loop's OSR away.
    pub fn on_deoptimization(
        &self,
        key: &MethodKey,
        reason: DeoptReason,
        bci: u32,
        action: DeoptAction,
    ) {
        self.core
            .stats
            .deoptimizations
            .fetch_add(1, Ordering::Relaxed);
        let counted = deopt_is_counted_trap(reason, action);
        let evicted = deopt_evicts_method_body(reason, action);
        // Read outside the `methods` lock, once per counted trap.
        let site_limit_keeps_c2 = counted && per_bci_trap_limit_keeps_c2_enabled();
        if !counted {
            self.core
                .stats
                .soft_deoptimizations
                .fetch_add(1, Ordering::Relaxed);
        }
        if !counted && !evicted {
            return;
        }
        let now_ms = process_uptime_ms();
        let mut dropped = 0u64;
        let mut unsettled = false;
        {
            let mut methods = self.core.methods.lock();
            // A key without class identity (`class_id` 0: the VM's by-name
            // resolution failed, e.g. two loaders define the name) matches no
            // state the invocation hooks built, so the trap was charged to
            // nobody. Resolve it to the ONE tracked state with that name; with
            // two or more the owner is unknowable here and nothing is charged,
            // as before. r11-tier-deopt-tiering-key-drops-the-class-id.
            let resolved = identify_unidentified_key(&methods, key);
            let key = resolved.as_ref().unwrap_or(key);
            if let Some(state) = methods.get_mut(key) {
                // The same test `invocation_verdict` stamps with.
                let was_settled = state.tiering_is_settled() || state.deferred_at_cap;
                if evicted {
                    state.current_tier = CompilationTier::Interpreter;
                }
                if counted {
                    decay_traps(state, now_ms);
                    let at_site = {
                        let n = state.trap_counts.entry((reason, bci)).or_insert(0);
                        *n = n.saturating_add(1);
                        *n
                    };
                    state.deopt_count = state.deopt_count.saturating_add(1);
                    let removed = self.core.remove_queued_for(key);
                    for (task, ticket) in &removed {
                        self.core
                            .release_slot_for(state, task.target_tier, task.osr_bci, *ticket);
                    }
                    dropped = removed.len() as u64;
                    if action == DeoptAction::MakeNotCompilable {
                        // Method entry only: see `MethodState::entry_ineligible`.
                        state.entry_ineligible = true;
                    }
                    if counted_trap_bails_c2(at_site, state.deopt_count, site_limit_keeps_c2)
                        && !state.c2_bailout
                    {
                        state.c2_bailout = true;
                        self.core.stats.c2_bailouts.fetch_add(1, Ordering::Relaxed);
                    }
                }
                unsettled = was_settled && !(state.tiering_is_settled() || state.deferred_at_cap);
            }
        }
        if dropped > 0 {
            crate::metrics::record_scheduling_events(crate::metrics::SCHEDULING_EVENTS[5], dropped);
            self.core.dropped.fetch_add(dropped, Ordering::Release);
        }
        // Expire the settled stamps only when THIS trap unsettled the method.
        // A live non-zero stamp exists only for a method that is settled now,
        // so a trap that leaves its method settled, or that hits a method that
        // was not settled, invalidates no stamp. The bump expires EVERY
        // method's stamp, and each call through an expired stamp takes the
        // global `methods` mutex again. It used to run on every trap, so a trap
        // storm (a caller frame stuck on a retired callee body, which re-traps
        // on each call: `R11TierSyncResume`) pushed every interpreted method
        // in the process back onto that mutex, once per trap.
        if unsettled {
            self.core.bump_settled_generation();
        }
    }

    /// Notify that C2 compilation bailed out (method too complex, etc.).
    /// Drops any request still queued for the method, as a counted trap does.
    ///
    /// `c2_bailouts` counts METHODS that became bailed out, as the trap limits
    /// and the compile-budget guard already do: it moves only on the
    /// `false -> true` transition of a method this manager tracks. It used to
    /// move on every call, so a repeated report for an already-bailed method
    /// (or one for a key the manager had never seen) inflated the exit dump's
    /// `c2_bailouts=` beside counts that meant something else.
    pub fn on_c2_bailout(&self, key: &MethodKey) {
        let mut dropped = 0u64;
        let mut newly_bailed = false;
        {
            let mut methods = self.core.methods.lock();
            if let Some(state) = methods.get_mut(key) {
                newly_bailed = !state.c2_bailout;
                state.c2_bailout = true;
                let removed = self.core.remove_queued_for(key);
                for (task, ticket) in &removed {
                    self.core
                        .release_slot_for(state, task.target_tier, task.osr_bci, *ticket);
                }
                dropped = removed.len() as u64;
            }
        }
        if dropped > 0 {
            crate::metrics::record_scheduling_events(crate::metrics::SCHEDULING_EVENTS[5], dropped);
            self.core.dropped.fetch_add(dropped, Ordering::Release);
        }
        if newly_bailed {
            self.core.stats.c2_bailouts.fetch_add(1, Ordering::Relaxed);
        }
        self.core.bump_settled_generation();
    }

    // ── Queries ──────────────────────────────────────────────────────────

    /// Get the current tier for a method.
    pub fn current_tier(&self, key: &MethodKey) -> CompilationTier {
        self.core
            .methods
            .lock()
            .get(key)
            .map(|s| s.current_tier)
            .unwrap_or(CompilationTier::Interpreter)
    }

    /// The interpreter invocation count recorded for `key`
    /// ([`MethodState::invocation_count`]), 0 for a method the manager has not
    /// seen. JIT round 23 wave 4, lane L3: the hotness of an INTERPRETED frame
    /// for the C2-intrinsic stack-trace rule (vm
    /// `stackwalker::drop_c2_intrinsic_native_leaf`), for a method this VM
    /// keeps in the interpreter where HotSpot would run C2 code. One lock and
    /// one lookup; asked on a rare throw path only.
    pub fn invocation_count_of(&self, key: &MethodKey) -> u64 {
        self.core
            .methods
            .lock()
            .get(key)
            .map_or(0, |s| s.invocation_count)
    }

    /// Get compilation statistics.
    pub fn stats(&self) -> &CompilationStats {
        &self.core.stats
    }

    /// Get a snapshot of the compilation policy.
    pub fn policy(&self) -> CompilationPolicy {
        let p = self.policy.lock();
        CompilationPolicy {
            c1_threshold: p.c1_threshold,
            c2_threshold: p.c2_threshold,
            osr_threshold: p.osr_threshold,
            tiered_enabled: p.tiered_enabled,
            c2_min_invocations: p.c2_min_invocations,
            c1_profiling: p.c1_profiling,
        }
    }

    /// Update the compilation policy. Expires every settled stamp, since a new
    /// threshold or a re-enabled tiering can unsettle any method.
    pub fn set_policy(&self, policy: CompilationPolicy) {
        self.core
            .c2_upgrade_min_invocations
            .store(c2_upgrade_gate(&policy), Ordering::Relaxed);
        self.core
            .c2_hotness_bar
            .store(c2_hotness_bar(&policy), Ordering::Relaxed);
        *self.policy.lock() = policy;
        self.core.bump_settled_generation();
    }

    /// Check if both compilation queues are empty.
    pub fn queue_empty(&self) -> bool {
        self.core.queue_len() == 0
    }

    /// Get the number of tasks queued across both lanes.
    pub fn queue_size(&self) -> usize {
        self.core.queue_len()
    }

    /// Pop the next request that is still current at the install epoch,
    /// dropping (and retiring) any stale ones ahead of it.
    ///
    /// The same rule the workers apply, across both lanes. Exposed so the
    /// dispatch rule can be exercised — and asserted on — without a thread, a
    /// backend, or a clock: enqueue, bump the epoch, call this, read
    /// [`Self::dropped_requests`].
    ///
    /// `None` means nothing fresh was queued. It does **not** mean nothing was
    /// there: stale entries ahead of an empty tail are consumed and counted.
    pub fn next_fresh_task(&self) -> Option<CompilationTask> {
        let mut stale: Vec<StaleRequest> = Vec::new();
        let current = self.core.current_install_epoch();
        let task = self.core.pop_across_lanes(Some(current), &mut stale);
        self.core.retire_stale(&stale);
        task
    }

    /// The install epoch this manager currently considers current.
    ///
    /// The owning VM's code-state epoch in a VM, [`crate::jit_install_epoch`]
    /// for a manager built with no source. The accessor exists so a diagnostic
    /// can print the number the dispatch gate is actually comparing against,
    /// rather than a number that is usually the same one.
    pub fn install_epoch(&self) -> u64 {
        self.core.current_install_epoch()
    }

    /// Install epoch of the OLDEST compile running right now, or `0` when
    /// every worker is idle.
    ///
    /// "Oldest" is the useful end: the question is whether anything still
    /// running was dispatched before the world moved. See
    /// `CompilerCore::inflight_epochs` for why this used to be a single word
    /// and what that word got wrong with more than one worker.
    pub fn inflight_install_epoch(&self) -> u64 {
        self.core.inflight_oldest_epoch()
    }

    /// Compiles running across all workers right now.
    ///
    /// The companion of [`Self::inflight_install_epoch`]: an epoch of `0` means
    /// "idle" only when this is also `0`, because `0` is a legal install epoch.
    pub fn inflight_compiles(&self) -> u64 {
        self.core.inflight_count.load(Ordering::Acquire)
    }

    /// Dispatches that found no free slot to record their epoch in.
    ///
    /// Structurally impossible — see `CompilerCore::inflight_slot_exhausted`.
    /// A non-zero reading means [`INFLIGHT_EPOCH_SLOTS`] and the worker clamp
    /// have drifted apart.
    pub fn inflight_slot_exhausted(&self) -> u64 {
        self.core.inflight_slot_exhausted.load(Ordering::Acquire)
    }

    /// Compilation requests this manager discarded without compiling them.
    ///
    /// The per-manager mirror of
    /// [`crate::metrics::scheduling_dropped_total`], which is process-wide.
    /// A steadily climbing value with a flat
    /// [`Self::completed_compilations`] is the signature of a queue that is
    /// being invalidated faster than it is being drained.
    pub fn dropped_requests(&self) -> u64 {
        self.core.dropped.load(Ordering::Acquire)
    }

    /// zs s9/rs2 (`CRATONVM_TIER_C2_STANDBY`): this manager's C2 standby
    /// workers started and promoted, over all its starts. Diagnostic
    /// ([`c2_standby_line`]).
    pub fn c2_standby_counts(&self) -> (u64, u64) {
        let c2 = &self.core.lanes[C2_LANE];
        (
            c2.standby_started_total.load(Ordering::Relaxed),
            c2.standby_promoted_total.load(Ordering::Relaxed),
        )
    }

    /// Requests refused because their method already held the in-flight slot.
    pub fn deduplicated_requests(&self) -> u64 {
        self.core.deduplicated.load(Ordering::Acquire)
    }

    /// Requests refused by [`CompilerCore::admit`] because the method is
    /// ineligible or has exhausted its compile retries.
    pub fn ineligible_refusals(&self) -> u64 {
        self.core.refused_ineligible.load(Ordering::Acquire)
    }

    /// Requests refused by [`CompilerCore::admit`] because the code cache was
    /// at its cap, and charged to nothing.
    pub fn cap_refusals(&self) -> u64 {
        self.core.refused_at_cap.load(Ordering::Acquire)
    }

    /// Unpublished compiles parked waiting on a class instead of charged to
    /// the method's failure budget (interpreter round i6 wave 116, lane L2).
    pub fn waiting_on_class_parks(&self) -> u64 {
        self.core.parked_waiting_on_class.load(Ordering::Acquire)
    }

    /// Install the VM's class-definition epoch reader, which the
    /// waiting-on-class park compares against ([`note_compile_waiting_on_class`]).
    /// Once per manager; `false` when one was already installed. Without it
    /// no completion parks and every unpublished compile is charged as
    /// before. Interpreter round i6 wave 116, lane L2 (page `i107-L2`).
    pub fn set_class_epoch_source(&self, source: ClassEpochSource) -> bool {
        self.core.class_epoch_source.set(source).is_ok()
    }

    /// Report the code cache's pressure. When it has just dropped below the
    /// cap, expire every settled stamp so the methods [`CompilerCore::admit`]
    /// deferred at the cap ask again.
    ///
    /// The VM calls this from its entry-counter drain, which runs whether or
    /// not a sweep does. It is what covers room coming back by a route other
    /// than a withdrawal this manager hears about: the retirement queue
    /// releasing a body some time after the sweep that withdrew it, or an
    /// invalidation.
    ///
    /// Returns whether it expired the stamps.
    pub fn note_code_cache_pressure(&self, at_cap: bool) -> bool {
        let was = self.core.cap_pressure.swap(at_cap, Ordering::AcqRel);
        if was && !at_cap {
            self.core.bump_settled_generation();
            return true;
        }
        false
    }

    /// Compiles whose callback panicked and was contained.
    pub fn worker_panics(&self) -> u64 {
        self.core.worker_panics.load(Ordering::Acquire)
    }

    /// Branch-profile windows this manager opened minus those it closed. Zero
    /// whenever no C2 nomination is outstanding; anything else left at exit is
    /// a window pinned open.
    pub fn branch_window_balance(&self) -> i64 {
        self.core.branch_window_balance.load(Ordering::Acquire)
    }

    /// JIT round 22 wave 11, lane runtime (reporting only; the
    /// `code_cache_sweep_phases` failure message): one line of the tiering
    /// state of every method of `class_name` whose name `method_names` accepts --
    /// tier, invocation count, whether a compile is queued, whether the last
    /// request was deferred at the code-cache cap, the compile-failure budget
    /// spent, and the ineligibility bits. Takes `methods` once.
    pub fn cap_report_for(
        &self,
        class_name: &str,
        method_names: &dyn Fn(&str) -> bool,
    ) -> Vec<String> {
        self.core
            .methods
            .lock()
            .values()
            .filter(|s| {
                &*s.method_key.class_name == class_name && method_names(&s.method_key.method_name)
            })
            .map(|s| {
                format!(
                    "{}{} tier={:?} inv={} queued={} deferred_at_cap={} fails={} ineligible={} \
                     entry_ineligible={} nominated_ms={} first_body_ms={}",
                    s.method_key.method_name,
                    s.method_key.descriptor,
                    s.current_tier,
                    s.invocation_count,
                    s.queued_for_compilation,
                    s.deferred_at_cap,
                    s.tier_fail_count,
                    s.ineligible,
                    s.entry_ineligible,
                    s.first_nominated_ms,
                    s.first_body_ms,
                )
            })
            .collect()
    }

    /// Get all method states: (key, current_tier, invocation_count).
    pub fn method_states(&self) -> Vec<(MethodKey, CompilationTier, u64)> {
        self.core
            .methods
            .lock()
            .values()
            .map(|s| (s.method_key.clone(), s.current_tier, s.invocation_count))
            .collect()
    }

    /// Whether this manager's compile workers are running.
    pub fn compiler_active(&self) -> bool {
        self.core.active.load(Ordering::Relaxed)
    }

    /// Set whether the background compiler is active.
    ///
    /// Retained for compatibility / tests that only assert the flag. The real
    /// workers are started via [`Self::start_background_compiler`] and flip
    /// this flag themselves.
    pub fn set_compiler_active(&self, active: bool) {
        self.core.active.store(active, Ordering::Relaxed);
    }

    /// Number of tasks the workers have finished compiling.
    pub fn completed_compilations(&self) -> u64 {
        self.core.completed.load(Ordering::Relaxed)
    }

    /// Install the work an idle C1-lane worker runs every [`IDLE_TICK`] while
    /// its lane is empty. The first installation wins until the workers are
    /// shut down, which clears it; later calls are ignored. A worker already
    /// parked picks the tick up after its next task.
    ///
    /// Why the compile workers need a clock (round 11 wave 4): the VM's
    /// compiled-entry counter is carried to this manager only by a drain, and
    /// the drain had two clocks, the interpreter's tier-up stride and the start
    /// of a background compile. A process whose hot code is all compiled, a
    /// benchmark whose `main` loop runs in an OSR body, stops both. A method
    /// the C1 publish did not promote (one containing `new`, while
    /// `CRATONVM_JIT_C2_ALLOC_UPGRADE` is off) then never reaches the C2
    /// hotness bar however hot it gets.
    pub fn set_idle_tick(&self, tick: IdleTickFn) {
        let mut slot = self.core.idle_tick.lock();
        if slot.is_none() {
            *slot = Some(Arc::from(tick));
        }
    }

    /// The bar a C1 body's drained count must reach for the C1 -> C2 hotness
    /// arm (`max(c2_threshold, c2_min_invocations)`), or `u64::MAX` while
    /// tiering is disabled. j27 TIER: the VM arms a published C1 body's entry
    /// trip at this number.
    pub fn c2_hotness_bar(&self) -> u64 {
        self.core.c2_hotness_bar.load(Ordering::Relaxed)
    }

    /// This manager's entry-trip sink (j27 wave 1, lane TIER;
    /// `CRATONVM_JIT_ENTRY_TRIP`; page
    /// `jr21-conf-compiled-bodies-reach-c2-only-at-a-drain-clock-FIXED-20261029.md`).
    /// A trip wakes an idle C1-lane worker to run the idle tick (the VM's
    /// entry-counter drain) now. Built once per manager; the wake holds a
    /// `Weak` of the core, so an armed body outliving the manager trips into
    /// nothing.
    pub fn entry_trip_sink(&self) -> Arc<crate::entry_counter::EntryTripSink> {
        Arc::clone(self.core.entry_trip_sink.get_or_init(|| {
            let weak = Arc::downgrade(&self.core);
            Arc::new(crate::entry_counter::EntryTripSink::new(Box::new(move || {
                if let Some(core) = weak.upgrade() {
                    core.request_idle_tick();
                }
            })))
        }))
    }

    /// Install the VM's note of admitted compiles (CONF21-1, JIT round 21
    /// lane conf; page `jr21-conf-string-identity-differs-only-from-hotspot-c2-FIXED-20261027.md`):
    /// every queued request calls it with the method's key and whether the
    /// request is optimizing (the optimizing tier, or any OSR request), on
    /// the thread that queued it (a mutator, or a worker's C1 -> C2 supersede)
    /// and under the manager's `methods` lock, so it must only record. The VM
    /// resolves String constants from the record on a mutator, as HotSpot
    /// does when a compile is requested. The first installation wins; answers
    /// whether this call installed it.
    pub fn set_compile_admission_sink(&self, sink: CompileAdmissionSink) -> bool {
        self.core.compile_admission_sink.set(sink).is_ok()
    }

    /// Install the VM's reader of its code cache (tier proposal W4-1, round
    /// 12 wave 5). After a method-entry compile reports success, `finish`
    /// asks it which backend's body is live under the key and sets
    /// `current_tier` from that rather than inferring it from the tier the
    /// method already had: a single-pass completion keeps an optimizing tier
    /// only over a live optimizing body, and raises the tier to C2 when one
    /// is live that the tier does not name. A `None` answer keeps the
    /// inference. The first installation wins; answers whether this call
    /// installed it. `CRATONVM_JIT_TIER_FROM_LIVE_BODY=0` (default on)
    /// installs nothing.
    pub fn set_live_entry_probe(&self, probe: LiveEntryProbe) -> bool {
        if !cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_TIER_FROM_LIVE_BODY") {
            return false;
        }
        self.core.live_entry_probe.set(probe).is_ok()
    }

    /// Start this manager's workers once and keep their handle on the manager.
    /// See the free [`ensure_background_compiler`].
    pub fn ensure_background_compiler<F>(&self, make_compile_fn: F)
    where
        F: FnOnce() -> CompileFn,
    {
        if self.core.active.load(Ordering::Acquire) {
            return;
        }
        let mut slot = self.background.lock();
        if slot.is_some() || self.core.active.load(Ordering::Acquire) {
            return;
        }
        *slot = self.start_background_compiler(make_compile_fn());
    }

    /// Stop the workers started by [`Self::ensure_background_compiler`] (drains
    /// and joins). Also happens when the manager is dropped.
    pub fn shutdown_background_compiler(&self) {
        let handle = self.background.lock().take();
        if let Some(mut handle) = handle {
            handle.shutdown();
        }
    }

    /// Spawn the compile workers (idempotent).
    ///
    /// Each lane gets its own workers ([`compiler_thread_counts`]). A worker
    /// loops: block on its lane's condvar until a task is available (or
    /// shutdown is requested), drain the highest-priority task, run
    /// `compile_fn` for it **off the mutator thread**, then record completion
    /// on the shared core. The returned [`BackgroundCompiler`] owns the join
    /// handles; dropping it (e.g. at VM teardown) signals shutdown and joins,
    /// so no compile thread outlives the VM.
    ///
    /// Takes `&self` (not `Arc<Self>`): the workers only need the
    /// `Arc<CompilerCore>`, so this works even though `SharedVm` owns the
    /// manager by value.
    ///
    /// Returns `None` if workers are already active, or if a lane could not
    /// get a single worker (every thread that did start is stopped again, and
    /// the slot released so a retry can succeed).
    pub fn start_background_compiler(&self, compile_fn: CompileFn) -> Option<BackgroundCompiler> {
        // Phase 5 fast boot (WP3): a boot image's capture run starts no
        // compiler thread before its snapshot point. Refused before the slot
        // is claimed, so the first call after the restore starts the workers.
        if cratonvm_types::image_layout::capturing() {
            return None;
        }
        // Atomically claim the worker slot.
        if self
            .core
            .active
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return None;
        }
        self.core.shutdown.store(false, Ordering::Release);

        let compile_fn: Arc<CompileFn> = Arc::new(compile_fn);
        let (c1_workers, c2_workers) = compiler_thread_counts();
        let mut background = BackgroundCompiler {
            core: Arc::clone(&self.core),
            handles: Vec::new(),
        };
        // zs s9/rs2: C2 workers after the first start parked
        // (`CRATONVM_TIER_C2_STANDBY`, opt-in); 0 = every worker live.
        let standby_depth = c2_standby_depth();
        let standby_epoch = {
            let c2 = &self.core.lanes[C2_LANE];
            // A restart after a shutdown parks its own standby workers anew.
            let mut promoted = c2.standby_promoted.lock();
            *promoted = 0;
            c2.standby_parked.store(0, Ordering::Release);
            c2.standby_depth.store(standby_depth, Ordering::Relaxed);
            c2.standby_scale.store(standby_depth > 0 && c2_standby_scale(), Ordering::Relaxed);
            // zt t2/fp: a new epoch, under the lock the parked workers wait
            // on; the notify below sends any worker an earlier start left
            // parked (a shutdown that detached its handles) out of the pool.
            let epoch = c2.standby_epoch.fetch_add(1, Ordering::AcqRel).wrapping_add(1);
            drop(promoted);
            c2.standby_wake.notify_all();
            epoch
        };
        for (lane, workers) in [(C1_LANE, c1_workers), (C2_LANE, c2_workers)] {
            let mut spawned = 0usize;
            for index in 0..workers {
                let core = Arc::clone(&self.core);
                let compile_fn = Arc::clone(&compile_fn);
                let standby = (lane == C2_LANE && standby_depth > 0 && index > 0).then_some(index);
                // The default Rust thread stack (~2 MiB) has no margin for
                // compiling methods with deep IR (heavy inlining, long
                // expression chains from generated/framework code such as
                // Quarkus/JUnit5 test-framework classes) — unlike the main-vm
                // interpreter thread, which was bumped to 128 MiB after
                // binaryTrees(18)-style recursion overflowed 64 MiB (see
                // vm-cli/src/main.rs). Give each compiler thread the same
                // 16 MiB headroom already used for other native-recursion-heavy
                // worker threads (see libcratonvm's foreign-attach threads); the
                // cost is virtual-address-space only (no commit until touched).
                let spawn = std::thread::Builder::new()
                    .name("cratonvm-jit-compiler".to_string())
                    .stack_size(16 * 1024 * 1024)
                    .spawn(move || {
                        let _ = ON_COMPILE_WORKER.try_with(|w| w.set(true));
                        // zs s9/rs2: a standby worker allocates nothing until
                        // promoted, so it owns no allocator pages.
                        if let Some(k) = standby {
                            if !core.lanes[lane].park_standby(
                                &core.shutdown,
                                k,
                                standby_epoch,
                            ) {
                                return;
                            }
                        }
                        Self::compiler_loop(&core, lane, &compile_fn);
                    });
                match spawn {
                    Ok(handle) => {
                        background.handles.push(handle);
                        spawned += 1;
                        if standby.is_some() {
                            self.core.lanes[lane]
                                .standby_parked
                                .fetch_add(1, Ordering::AcqRel);
                            self.core.lanes[lane]
                                .standby_started_total
                                .fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    Err(_) => break,
                }
            }
            if spawned == 0 {
                // A lane with no worker would leave its requests queued
                // forever. Stop what did start; `shutdown` releases `active`.
                background.shutdown();
                return None;
            }
        }
        Some(background)
    }

    /// Body of a compile worker, serving one lane.
    ///
    /// ## GC-STW-safety invariant (wire-tiered-manager increment 3)
    ///
    /// This loop runs on an unregistered, GC-neutral `cratonvm-jit-compiler`
    /// daemon (see [`Self::start_background_compiler`]). The queue wait MUST
    /// block on the lane's OWN [`Condvar`] while holding ONLY this crate's lane
    /// queue mutex — never any VM lock. `tiered.rs` is in the `cratonvm-jit`
    /// crate and cannot even name `SharedVm`'s locks, so the wait here is
    /// structurally VM-lock-free: the lanes and `core.methods` are
    /// jit-crate-private. `parking_lot::Condvar::wait` releases the queue lock
    /// while the worker is parked and re-acquires it on wake.
    ///
    /// The VM-side codegen + publish runs in `compile_fn` with NO lock of any
    /// kind held by this frame (the queue guard is dropped at the end of the
    /// inner scope before `compile_fn` is called). The VM closure
    /// (`background_compile_task`) is responsible for bounding its own VM-lock
    /// scopes; this loop guarantees it is entered lock-free. The net effect:
    /// while a worker idles, it holds no lock a mutator could need, so a
    /// mutator never stalls behind it and a concurrent STW completes promptly.
    ///
    /// ## Stale-request drop (install epoch)
    ///
    /// A request is dispatched only if the JIT install epoch is still the one
    /// it was queued at. If a JVMTI redefinition or a code-cache flush moved
    /// the epoch in between, the request describes a world that no longer
    /// exists and is dropped HERE, before the backend is entered, rather than
    /// after — the per-cache flush barrier in `JitCache::put` would refuse the
    /// resulting body anyway, so compiling it is pure waste. The drop is
    /// explicit: counted under `crate::metrics::SCHEDULING_EVENTS[0]`, the
    /// method's in-flight slot released, no retry spent. See
    /// [`CompilerCore::retire_stale`] and `docs/jit/broker-install-epoch.md`.
    ///
    /// ## Panic containment and the compile budget
    ///
    /// `compile_fn` runs under [`contain_compile_panic`]. A panic there is
    /// logged once, counted as `worker_panic`, recorded as a permanent decline
    /// for that method, and the loop carries on: the in-flight epoch is reset
    /// and the slot released exactly as for any other outcome. The compile is
    /// also bracketed by [`current_thread_cpu_time`], which is what the C2
    /// budget is charged.
    fn compiler_loop(core: &Arc<CompilerCore>, lane: usize, compile_fn: &CompileFn) {
        let lane_ref = &core.lanes[lane];
        loop {
            // Pop one FRESH task while holding ONLY this lane's queue lock;
            // block on the lane's condvar when empty so the worker idles
            // instead of spinning. No VM lock is — or can be — held across
            // this wait.
            //
            // Stale requests found on the way are collected, not acted on:
            // retiring one takes `core.methods`, and taking `methods` while
            // holding a queue would invert this file's `methods` → `queue`
            // order (see `retire_stale`). So the queue guard is released
            // first, `retire_stale` runs, and the loop re-enters — which is
            // also why `stale` is re-created per iteration.
            let (task, ticket, dispatch_epoch) = loop {
                let mut stale: Vec<StaleRequest> = Vec::new();
                let mut shutting_down = false;
                let mut idle_due = false;
                // Only the cheap lane ticks, so an idle process pays one wake
                // per `IDLE_TICK` per C1 worker and nothing on the C2 lane.
                let ticks = lane == C1_LANE && core.idle_tick.lock().is_some();
                let popped = {
                    let mut q = lane_ref.queue.lock();
                    loop {
                        if core.shutdown.load(Ordering::Acquire) {
                            shutting_down = true;
                            break None;
                        }
                        let current = core.current_install_epoch();
                        match q.dequeue_fresh(current, &mut stale) {
                            // The epoch the freshness gate just passed the
                            // request at travels with it: that is the world
                            // the compile is formed against.
                            Some((task, ticket)) => break Some((task, ticket, current)),
                            // Nothing fresh AND something to retire: give up
                            // the queue lock so the slots can be released
                            // under `methods`, then come back around.
                            None if !stale.is_empty() => break None,
                            // An idle C1 lane parks for at most `IDLE_TICK`,
                            // then leaves the lock: the tick runs below with
                            // no lock of this loop held.
                            None if ticks => {
                                // j27 TIER: an entry trip asked for the tick
                                // now (`CompilerCore::request_idle_tick`).
                                if core.idle_tick_requested.swap(false, Ordering::AcqRel) {
                                    idle_due = true;
                                    break None;
                                }
                                if lane_ref.wake.wait_for(&mut q, IDLE_TICK).timed_out() {
                                    idle_due = true;
                                    break None;
                                }
                            }
                            // `parking_lot::Condvar::wait` releases `q` while parked and
                            // re-acquires on wake; spurious wakeups re-check the loop.
                            None => lane_ref.wake.wait(&mut q),
                        }
                    }
                };
                // Guard released. Retire first — these requests are already
                // out of the queue, so returning without retiring them (the
                // shutdown path included) would lose them with no counter and
                // leave their methods marked in-flight forever. Anything still
                // IN the queue is `BackgroundCompiler::shutdown`'s to drain.
                core.retire_stale(&stale);
                if shutting_down {
                    return;
                }
                if let Some(dispatched) = popped {
                    break dispatched;
                }
                if idle_due {
                    // Cloned out, so the slot's lock is not held while the
                    // tick runs.
                    let tick = core.idle_tick.lock().clone();
                    if let Some(tick) = tick {
                        // Contained like a compile: a panicking tick must not
                        // take the lane's only worker with it.
                        let run: &(dyn Fn() + Send + Sync) = &*tick;
                        let _ = contain_compile_panic(run);
                    }
                    // j27 w6 T1TIERMGR (TIERMGR21-4): time one gate-refused
                    // body, if the VM installed a timer and queued one.
                    let _ = contain_compile_panic(|| core.run_accept_timing_tick());
                }
            };

            // Compile off the mutator thread with NO lock held by this frame,
            // then publish completion. `compile_fn` bounds its own VM-lock
            // scopes internally.
            //
            // `dispatch_epoch` is the in-flight stamp: the dispatch gate above
            // proved the epoch had not moved *yet*, and this is what lets the
            // completion below notice that it moved *during* the compile. That
            // window cannot be closed here — the artifact is already built —
            // and it is not this loop's to close: `JitCache::put`/`put_osr`
            // refuse a body stamped below the owning cache's flush barrier.
            // Recording it makes the residual visible instead of invisible.
            //
            // `dispatch_epoch` is the epoch the freshness gate compared
            // against under the lane lock, NOT a second read taken here. A
            // re-read let an epoch move landing between the gate and this
            // line become the "dispatch" epoch itself, so the compile of a
            // request formed against the old world was recorded as current
            // and the move was never seen.
            // gcj pcp: a compile does not START inside the VM's collection
            // pause. The workers are GC-neutral and took as many CPU samples
            // as every GC thread together in a JIT-warm javac pause; on an
            // oversubscribed host that is CPU the pause's workers wait for.
            // No lock of this loop is held here.
            core.wait_out_gc_pause();
            core.note_queue_wait(lane, &task, process_uptime_ms());
            // Tier proposal R12-4: a single-pass request that an optimizing
            // body published outside the manager has overtaken is not
            // compiled, so its publish cannot replace that body.
            if core.retire_overtaken(&task, ticket) {
                continue;
            }
            let inflight_slot = core.note_dispatch(dispatch_epoch);
            // JIT round 21 lane tiermgr (INDYCONST21-1): the retry generation
            // this compile starts at. `SeqCst` pairs with the stamp's
            // `fetch_add` in `request_c2_retry`: a request whose stamp this
            // load does not see came after this compile began reading.
            let dispatched_at = core.retry_generation.load(Ordering::SeqCst);
            let cpu_before = current_thread_cpu_time();
            let wall_before = std::time::Instant::now();
            // Wave 116, lane L2 (page `i107-L2`): the class-definition epoch
            // BEFORE the compile, so a class defined while it ran unparks it
            // at the next offer; the waiting-on-class note is this compile's
            // alone.
            let class_epoch_before = core.class_definition_epoch();
            let _ = take_compile_waiting_on_class();
            let _ = take_compile_cap_refused();
            let cap_refusals_before = crate::jit_code_cache_cap_refusals();
            let result = contain_compile_panic(|| compile_fn(&task));
            // JIT round 22 wave 12 (lane review,
            // `CRATONVM_JIT_CAP_REFUSAL_WITNESS_PER_THREAD`): THIS compile's
            // refusal, not any compile's in the process.
            let refused_here = take_compile_cap_refused();
            let cap_refusals_moved = if cap_refusal_witness_per_thread_enabled() {
                refused_here
            } else {
                crate::jit_code_cache_cap_refusals() != cap_refusals_before
            };
            let wall_ns = u64::try_from(wall_before.elapsed().as_nanos()).unwrap_or(u64::MAX);
            let waiting_on_class = take_compile_waiting_on_class();
            let cpu_ms = match (cpu_before, current_thread_cpu_time()) {
                (Some(before), Some(after)) => {
                    Some(after.saturating_sub(before).as_millis() as u64)
                }
                _ => None,
            };
            core.note_dispatch_done(inflight_slot);
            let epoch_moved = core.current_install_epoch() != dispatch_epoch;
            if epoch_moved {
                crate::metrics::record_scheduling_event(crate::metrics::SCHEDULING_EVENTS[2]);
            }
            let outcome = match result {
                Ok(outcome) => outcome,
                Err(payload) => {
                    core.note_worker_panic(&task, &*payload);
                    CompileOutcome::declined(0)
                }
            };
            let owed_retry = core.finish_owed(
                &task.method_key,
                Completion {
                    tier: task.target_tier,
                    osr_bci: task.osr_bci,
                    osr: task.osr_bci.is_some(),
                    compile_time_ms: outcome.compile_time_ms,
                    compile_time_ns: wall_ns,
                    budget_ms: cpu_ms.unwrap_or(outcome.compile_time_ms),
                    success: outcome.published,
                    declined_permanently: outcome.declined_permanently,
                    epoch_moved,
                    waiting_on_class: class_epoch_before.filter(|_| {
                        waiting_on_class && !outcome.published && !outcome.declined_permanently
                    }),
                    cap_refused: cap_refusals_moved
                        && !outcome.published
                        && !outcome.declined_permanently
                        && cap_refusal_witnessed_enabled(),
                    ticket,
                },
                dispatched_at,
            );
            // JIT round 21 lane tiermgr (review): a compile that finished
            // after shutdown was requested queues nothing more. A worker
            // detached by a shutdown that ran ON a worker
            // (`BackgroundCompiler::shutdown`'s `on_worker` arm) finishes
            // after the drain, so a request queued here outlived the drain:
            // a held slot and an open branch window (process-wide) with no
            // worker left. The loop head sees the flag and returns.
            if core.shutdown.load(Ordering::Acquire) {
                continue;
            }
            // C1→C2 supersede: a freshly-published C1-family body whose
            // method the VM judged IR-eligible gets a Low-priority C2
            // recompile. Enqueued AFTER `finish` so the C1 task's slot has
            // been released (otherwise the idempotence gate would drop the
            // upgrade). OSR tasks are excluded (their artifacts serve loop
            // entry; the invocation path re-tiers separately), as are tasks
            // already at an optimized tier.
            //
            // JIT round 21 lane tiermgr (INDYCONST21-1): `queued_c2` records
            // whether one of these post-completion requests queued an
            // optimizing compile. Admitted after the owed stamp, it is
            // dispatched after it too, so it answers an owed retry.
            let mut queued_c2 = false;
            if outcome.published
                && outcome.c2_upgrade_candidate
                && task.osr_bci.is_none()
                && !tier_uses_optimized_backend(task.target_tier)
            {
                queued_c2 |= core.request_c2_upgrade(&task.method_key);
            }
            // The deferred-`new` retry, which deliberately does NOT carry the
            // "not already optimized" clause above: the bail it answers happens
            // inside a C2 task that then fell through to the single-pass
            // backend, so by the time we get here the method is recorded as
            // done with C2 and `request_c2_upgrade` would refuse it for exactly
            // that reason. Bounded by the one-shot memo in `lib.rs` — a method
            // is armed once, and a class that never loads cannot re-arm it.
            if outcome.published && outcome.deferred_new_retry && task.osr_bci.is_none() {
                queued_c2 |=
                    core.request_deferred_new_retry(&task.method_key) == C2RetryAnswer::Enqueued;
            }
            // A late-bind retry refused `already queued` while this task held
            // the slot (`late_bind_retry_owed_enabled`): asked now, published
            // or not, since the body this task built may predate the callee.
            // j27 w6 T1TIERMGR (`CRATONVM_JIT_ONE_OWED_RETRY`): an optimizing
            // compile queued above answers it (`ask_late_bind_owed`).
            if task.osr_bci.is_none() {
                queued_c2 |= core.ask_late_bind_owed(&task.method_key, queued_c2);
            }
            // After `finish`, like the two above: it released this task's slot
            // and charged the unpublished attempt, which is what bounds this.
            if !outcome.published
                && outcome.optimizing_publication_refused
                && task.osr_bci.is_none()
                && tier_uses_optimized_backend(task.target_tier)
            {
                // JIT r23 wave 9, lane L6: a refused publication is a race, so
                // its charge is handed back (bounded); see
                // `c2_publication_refusal_refund_enabled`.
                core.refund_refused_publication(&task.method_key);
                queued_c2 |= core.request_c2_retry(&task.method_key, "refused-publication")
                    == C2RetryAnswer::Enqueued;
            }
            // JIT round 21 lane tiermgr (INDYCONST21-1): an owed retry this
            // task did not answer, asked LAST, and only when none of the
            // requests above queued an optimizing compile (that compile
            // answers it; asking here could race its dispatch into a second
            // compile).
            if let Some(door) = owed_retry {
                if !queued_c2 {
                    core.ask_owed_retry(&task.method_key, door);
                }
            }
        }
    }

    // ── Internal ─────────────────────────────────────────────────────────

    /// Core compilation-decision logic. Must be called while `methods` is locked.
    fn should_compile_inner(
        &self,
        state: &mut MethodState,
        policy: &CompilationPolicy,
    ) -> Option<CompilationTier> {
        let target = self.should_compile(state, policy)?;

        let priority = match target {
            CompilationTier::C2 => CompilationPriority::High,
            CompilationTier::C1 | CompilationTier::C1WithProfiling => CompilationPriority::Normal,
            _ => CompilationPriority::Low,
        };

        let task = CompilationTask {
            method_key: state.method_key.clone(),
            target_tier: target,
            priority,
            enqueue_time_ms: 0,
            osr_bci: None,
        };

        if cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_TIER_ENQUEUE") {
            eprintln!(
                "[cratonvm-tier] enqueue {}.{}{} tier={:?} invocations={} drained-be={} elapsed_ms={}",
                state.method_key.class_name,
                state.method_key.method_name,
                state.method_key.descriptor,
                target,
                state.invocation_count,
                // HANDLER8-2: the raw back-edge word the last drain read, `-`
                // when no drain ever reported this method.
                state
                    .last_drained_back_edges
                    .map_or_else(|| "-".to_string(), |be| be.to_string()),
                process_uptime_ms(),
            );
        }
        // `admit` takes a lane's queue lock (distinct from `methods`, which
        // the caller still holds) and wakes one of its workers.
        if !self.core.admit(state, task) {
            return None;
        }
        Some(target)
    }

    /// Pure policy check: determine if the method should be compiled (and at what tier).
    fn should_compile(
        &self,
        state: &MethodState,
        policy: &CompilationPolicy,
    ) -> Option<CompilationTier> {
        // Give up after repeated compile-attempt failures (the attempt ran
        // but never published a body — see `finish`). Without this, a method
        // whose compile step keeps failing for a reason outside the (fast)
        // permanent bail-list — e.g. an in-flight class redefine — would be
        // re-recommended and re-enqueued on every
        // single invocation forever, since a failed attempt no longer
        // advances `current_tier`. The code-cache cap is NOT one of these any
        // more: `CompilerCore::admit` refuses it before a worker runs, and it
        // is never charged here -- see `MethodState::deferred_at_cap`. A
        // policy decline is permanent by
        // construction, so one is enough — unlike `tier_fail_count`, which
        // deliberately allows retries.
        if state.ineligible || state.entry_ineligible {
            return None;
        }
        if state.entry_out_of_retries() {
            return None;
        }
        match state.current_tier {
            CompilationTier::Interpreter => {
                // Can we skip straight to C2?
                if !state.c2_bailout
                    && state.invocation_count >= policy.c2_threshold as u64
                    && state.invocation_count >= policy.c2_min_invocations as u64
                {
                    return Some(CompilationTier::C2);
                }
                // Otherwise go to C1
                if state.invocation_count >= policy.c1_threshold as u64 {
                    return Some(CompilationTier::C1);
                }
                None
            }
            CompilationTier::C1 | CompilationTier::C1WithProfiling => {
                if state.c2_bailout {
                    return None;
                }
                if state.invocation_count >= policy.c2_threshold as u64
                    && state.invocation_count >= policy.c2_min_invocations as u64
                {
                    return Some(CompilationTier::C2);
                }
                // MON11-4 (JIT round 19 wave 12, lane mon12;
                // `CRATONVM_JIT_TIER4_I_PLUS_B`, opt-in): HotSpot's Tier4
                // `i + b` predicate on the drained back edges.
                if tier4_i_plus_b_enabled()
                    && state
                        .last_drained_back_edges
                        .is_some_and(|b| tier4_i_plus_b_due(state.invocation_count, b))
                {
                    return Some(CompilationTier::C2);
                }
                None
            }
            // Already at C2 or FullProfile — nothing to do.
            _ => None,
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    /// JIT round 23 wave 3: under both arms of
    /// `CRATONVM_JIT_PANIC_SCOPE_ONE_TLS` the scope depth the crash hook reads
    /// is raised inside every (nested) scope, back to zero after, and a panic
    /// inside is contained with the depth restored.
    #[test]
    fn panic_scope_depth_is_kept_under_both_arms() {
        for arm in ["1", "0"] {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_JIT_PANIC_SCOPE_ONE_TLS", Some(arm))],
                || {
                    assert!(!compile_panic_is_contained(), "arm {arm}");
                    let nested = contain_compile_panic(|| {
                        let inner = contain_compile_panic(compile_panic_is_contained);
                        (compile_panic_is_contained(), inner.ok())
                    });
                    assert_eq!(nested.ok(), Some((true, Some(true))), "arm {arm}");
                    assert!(!compile_panic_is_contained(), "arm {arm}");
                    let caught = contain_compile_panic(|| -> u32 { panic!("contained") });
                    assert!(caught.is_err(), "arm {arm}");
                    assert!(!compile_panic_is_contained(), "arm {arm}: depth restored");
                },
            );
        }
    }

    fn test_key() -> MethodKey {
        MethodKey::new("java/lang/String", "hashCode", "()I")
    }

    /// A key used ONLY by the deny-list test.
    ///
    /// OSR denials used to live in a PROCESS-GLOBAL set, and marking the
    /// shared `test_key()` denied OSR for every other test in the binary —
    /// which is how `stats_osr_compilations` once intermittently observed 0
    /// OSR compilations instead of 1. They are per manager now; the key stays
    /// distinct so a test that reads as "denied" names a method nothing else
    /// uses.
    fn osr_deny_only_key() -> MethodKey {
        MethodKey::new("craton/test/OsrDenyOnly", "denied", "()V")
    }

    fn test_key2() -> MethodKey {
        MethodKey::new(
            "java/util/HashMap",
            "get",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
        )
    }

    #[test]
    #[ignore = "BigInteger JIT deny removed 2026-07-31 with the last static ban mirrors (docs/known-issues/jit-bans/jit-bans-all-disabled-20260731.md); the assertion is kept as the record of what the ban covered"]
    fn hibernate_biginteger_divide_cluster_is_never_background_enqueued() {
        let policy = CompilationPolicy {
            c1_threshold: 1,
            ..CompilationPolicy::default()
        };
        let mgr = TieredCompilationManager::new(policy);
        let key = MethodKey::new(
            "java/math/MutableBigInteger",
            "divideMagnitude",
            "(Ljava/math/MutableBigInteger;Ljava/math/MutableBigInteger;)Ljava/math/MutableBigInteger;",
        );

        assert_eq!(mgr.on_method_invocation(&key), None);
        assert!(
            mgr.dequeue_compilation().is_none(),
            "quarantined MutableBigInteger must not enter the background queue"
        );
        assert_eq!(mgr.current_tier(&key), CompilationTier::Interpreter);
    }

    // ── wire-tiered-manager Step 6: CRATONVM_TIER_* policy overrides ─────

    #[test]
    fn step6_policy_overrides_apply_and_clamp() {
        use std::collections::HashMap;
        let env: HashMap<&str, &str> = [
            ("CRATONVM_TIER_C1_THRESHOLD", "50"),
            ("CRATONVM_TIER_C2_THRESHOLD", "9000"),
            ("CRATONVM_TIER_OSR_THRESHOLD", "0"), // clamps to 1
            ("CRATONVM_TIER_C2_MIN_INVOCATIONS", "garbage"), // ignored → default
            ("CRATONVM_TIER_ENABLED", "0"),
        ]
        .into_iter()
        .collect();
        let p = CompilationPolicy::with_overrides(|k| env.get(k).map(|s| s.to_string()));
        assert_eq!(p.c1_threshold, 50);
        assert_eq!(p.c2_threshold, 9000);
        assert_eq!(p.osr_threshold, 1, "0 must clamp to 1, not disable warmup");
        assert_eq!(
            p.c2_min_invocations, 1_000,
            "unparseable value keeps the default"
        );
        assert!(
            !p.tiered_enabled,
            "CRATONVM_TIER_ENABLED=0 disables tiering"
        );
    }

    #[test]
    fn step6_policy_overrides_empty_env_is_default() {
        let p = CompilationPolicy::with_overrides(|_| None);
        let d = CompilationPolicy::default();
        assert_eq!(p.c1_threshold, d.c1_threshold);
        assert_eq!(p.c2_threshold, d.c2_threshold);
        assert_eq!(p.osr_threshold, d.osr_threshold);
        assert_eq!(p.c2_min_invocations, d.c2_min_invocations);
        assert_eq!(p.tiered_enabled, d.tiered_enabled);
    }

    // ── observed-count fast-forward (stride-boundary deflation fix) ──────

    #[test]
    fn observed_count_fast_forwards_hotness() {
        let mgr = TieredCompilationManager::with_default_policy();

        // Plain counting: a single visit is far below c1_threshold → None.
        let cold = test_key();
        assert_eq!(mgr.on_method_invocation(&cold), None);

        // Observed-count fast-forward: the interpreter has REALLY seen 5,000
        // invocations of this method but only consults the manager at stride
        // boundaries — the recommendation must fire on this single visit
        // instead of after another c1_threshold visits (64x deflation).
        let hot = test_key2();
        let rec = mgr.on_method_invocation_observed(&hot, 5_000);
        assert!(
            rec.is_some(),
            "observed=5000 must produce a tier recommendation on the first visit"
        );

        // A stale/smaller observed value never rewinds the counter.
        let state_count = mgr
            .method_states()
            .into_iter()
            .find(|(k, _, _)| *k == hot)
            .map(|(_, _, c)| c)
            .expect("state for hot key");
        assert!(state_count >= 5_000);
        let _ = mgr.on_method_invocation_observed(&hot, 3);
        let state_count_after = mgr
            .method_states()
            .into_iter()
            .find(|(k, _, _)| *k == hot)
            .map(|(_, _, c)| c)
            .expect("state for hot key");
        assert!(
            state_count_after > state_count.saturating_sub(1),
            "smaller observed count must not rewind the counter"
        );
    }

    // ── wire-tiered-manager Step 5: request_osr ──────────────────────────

    #[test]
    fn step5_request_osr_enqueues_osr_task_immediately() {
        let mgr = TieredCompilationManager::with_default_policy();
        let key = test_key();
        // The first call enqueues immediately: the interpreter's per-frame
        // back-edge schedule is the throttle, not a count kept here.
        let task = mgr
            .request_osr(&key, 42)
            .expect("first request should enqueue");
        assert_eq!(task.osr_bci, Some(42));
        assert_eq!(task.priority, CompilationPriority::High);
        assert_eq!(task.target_tier, CompilationTier::C2);
        // Idempotent while queued: a second request is a no-op (no double compile).
        assert!(mgr.request_osr(&key, 42).is_none());
        // The task really is on the queue. The OSR stat counts PUBLISHED
        // artifacts, so a queued request is not one yet.
        let dq = mgr
            .dequeue_compilation()
            .expect("an OSR task should be queued");
        assert_eq!(dq.osr_bci, Some(42));
        assert_eq!(mgr.stats().osr_compilations.load(Ordering::Relaxed), 0);
        mgr.core
            .complete_task(&key, CompilationTier::C2, 1, true, true, false);
        assert_eq!(mgr.stats().osr_compilations.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn step5_request_osr_honors_osr_deny_list() {
        // Pinned epoch: a denial expires when the install epoch moves, and the
        // process-wide one moves whenever any test flushes a cache.
        let mgr = worker_manager(CompilationPolicy::default());
        let key = osr_deny_only_key();
        mgr.mark_osr_denied(key.clone());
        assert!(mgr.is_osr_denied(&key));
        assert!(
            mgr.request_osr(&key, 42).is_none(),
            "OSR-denied method must not enqueue an OSR task"
        );
        assert!(mgr.dequeue_compilation().is_none());
        // The denial belongs to this manager, not to the process.
        let other = worker_manager(CompilationPolicy::default());
        assert!(
            other.request_osr(&key, 42).is_some(),
            "another VM's manager must not inherit this one's OSR denials"
        );
    }

    #[test]
    fn step5_request_osr_is_independent_of_method_entry_c2_but_honors_bailout() {
        // A method-entry C2 body does not provide an OSR entry and therefore
        // must not suppress the separately cached OSR artifact.
        let mgr = TieredCompilationManager::with_default_policy();
        let key = test_key();
        mgr.on_method_invocation(&key);
        mgr.compilation_complete(&key, CompilationTier::C2, 1);
        let task = mgr
            .request_osr(&key, 7)
            .expect("method-entry C2 must still allow an OSR artifact");
        assert_eq!(task.osr_bci, Some(7));

        // C2-bailed method → no OSR enqueue.
        let mgr2 = TieredCompilationManager::with_default_policy();
        let key2 = test_key2();
        mgr2.on_method_invocation(&key2);
        mgr2.on_c2_bailout(&key2);
        assert!(
            mgr2.request_osr(&key2, 7).is_none(),
            "bailed method: no OSR enqueue"
        );
    }

    /// A denial is a verdict about the code that was loaded when it was made,
    /// so it expires when the install epoch moves, and a redefinition of the
    /// class forgets it outright. It used to be a process-global set that one
    /// failed background compile populated for good.
    #[test]
    fn an_osr_denial_expires_when_the_install_epoch_moves() {
        let epoch = Arc::new(AtomicU64::new(1));
        let mgr = epoch_driven_manager(&epoch);
        let key = MethodKey::new("craton/test/OsrExpiry", "loop", "()V");
        mgr.mark_osr_denied(key.clone());
        assert!(mgr.request_osr(&key, 3).is_none());
        epoch.store(2, Ordering::Release);
        assert!(
            !mgr.is_osr_denied(&key),
            "a denial stamped at epoch 1 has expired at epoch 2"
        );
        assert!(mgr.request_osr(&key, 3).is_some());

        let redefined = MethodKey::new("craton/test/OsrRedefined", "loop", "()V");
        mgr.mark_osr_denied(redefined.clone());
        mgr.on_class_redefined(ClassId::new(0), "craton/test/OsrRedefined");
        assert!(
            !mgr.is_osr_denied(&redefined),
            "a redefinition forgets the class's denials"
        );
    }

    /// A failed OSR compile is a failed compile, not a denial: it spends one
    /// retry and the next hot back-edge asks again, up to the fail limit.
    #[test]
    fn a_failed_osr_compile_is_retried_up_to_the_fail_limit() {
        let mgr = epoch_driven_manager(&Arc::new(AtomicU64::new(1)));
        let key = MethodKey::new("craton/test/OsrRetry", "loop", "()V");
        for attempt in 0..MAX_TIER_FAIL_RETRIES {
            let task = mgr.request_osr(&key, 9).unwrap_or_else(|| {
                panic!("attempt {attempt}: a failed OSR compile must be retried")
            });
            assert_task_eq(mgr.next_fresh_task(), Some(task));
            mgr.core
                .complete_task(&key, CompilationTier::C2, 1, false, true, false);
            assert!(
                !mgr.is_osr_denied(&key),
                "a failed compile must not deny OSR"
            );
        }
        assert!(
            mgr.request_osr(&key, 9).is_none(),
            "the retry budget still bounds an OSR compile that keeps failing"
        );
    }

    /// The background OSR door reacts to the two `None`s of `request_osr` in
    /// opposite ways (wait vs retire the loop header), so the classified twin
    /// must tell a held slot from a give-up -- and a spent retry budget, which
    /// is NOT an OSR denial, must still read as a give-up.
    #[test]
    fn a_classified_osr_request_tells_waiting_from_giving_up() {
        let mgr = epoch_driven_manager(&Arc::new(AtomicU64::new(1)));
        let key = MethodKey::new("craton/test/OsrClassified", "loop", "()V");
        let task = match mgr.request_osr_classified(&key, 9) {
            OsrRequest::Queued(task) => task,
            other => panic!("a fresh method must queue, got {other:?}"),
        };
        assert_eq!(
            mgr.request_osr_classified(&key, 9),
            OsrRequest::Pending,
            "a held OSR slot is a wait, not a refusal"
        );
        assert_task_eq(mgr.next_fresh_task(), Some(task));
        for _ in 0..MAX_TIER_FAIL_RETRIES {
            mgr.core
                .complete_task(&key, CompilationTier::C2, 1, false, true, false);
            if let OsrRequest::Queued(next) = mgr.request_osr_classified(&key, 9) {
                assert_task_eq(mgr.next_fresh_task(), Some(next));
            }
        }
        assert!(!mgr.is_osr_denied(&key), "a spent budget is not a denial");
        assert_eq!(
            mgr.request_osr_classified(&key, 9),
            OsrRequest::Refused,
            "a spent OSR retry budget refuses every re-offer"
        );
        assert!(mgr.request_osr(&key, 9).is_none());

        let denied = MethodKey::new("craton/test/OsrClassifiedDenied", "loop", "()V");
        mgr.mark_osr_denied(denied.clone());
        assert_eq!(mgr.request_osr_classified(&denied, 3), OsrRequest::Refused);

        let bailed = MethodKey::new("craton/test/OsrClassifiedBailed", "loop", "()V");
        mgr.on_method_invocation(&bailed);
        mgr.on_c2_bailout(&bailed);
        assert_eq!(mgr.request_osr_classified(&bailed, 3), OsrRequest::Refused);
    }

    /// r11-tier: an OSR decline (the VM's OSR-denial / bail-listed arms, both
    /// of which expire with the install epoch) must not set the lifetime,
    /// whole-method `ineligible` flag, which also closed method entry.
    #[test]
    fn an_osr_decline_spends_an_osr_retry_and_leaves_method_entry_open() {
        let mgr = epoch_driven_manager(&Arc::new(AtomicU64::new(1)));
        let key = MethodKey::new("craton/test/OsrDecline", "loop", "()V");
        let task = mgr.request_osr(&key, 9);
        assert!(task.is_some(), "the first OSR request must be queued");
        assert_task_eq(mgr.next_fresh_task(), task);
        mgr.core
            .complete_task(&key, CompilationTier::C2, 1, false, true, true);
        {
            let methods = mgr.core.methods.lock();
            let state = &methods[&key];
            assert!(
                !state.ineligible,
                "an OSR decline must not close method entry"
            );
            assert_eq!(state.osr_fail_count, 1);
            assert_eq!(state.tier_fail_count, 0);
        }
        let entry = MethodKey::new("craton/test/OsrDecline", "loop", "()V");
        let mut recommended = None;
        for _ in 0..10_000 {
            recommended = recommended.or(mgr.on_method_invocation(&entry));
        }
        assert!(
            recommended.is_some(),
            "method entry must still tier after an OSR decline"
        );
    }

    // ── Policy defaults ──────────────────────────────────────────────────

    #[test]
    fn default_policy_values() {
        let p = CompilationPolicy::default();
        assert_eq!(p.c1_threshold, 500);
        assert_eq!(p.c2_threshold, 20_000);
        assert_eq!(p.osr_threshold, 10_000);
        assert!(p.tiered_enabled);
        assert_eq!(p.c2_min_invocations, 1_000);
        assert!(p.c1_profiling);
    }

    // ── Initial state ────────────────────────────────────────────────────

    #[test]
    fn method_starts_at_interpreter() {
        let mgr = TieredCompilationManager::with_default_policy();
        assert_eq!(mgr.current_tier(&test_key()), CompilationTier::Interpreter);
    }

    // ── Invocation counting ──────────────────────────────────────────────

    #[test]
    fn invocation_count_tracking() {
        let mgr = TieredCompilationManager::with_default_policy();
        let key = test_key();
        for _ in 0..50 {
            mgr.on_method_invocation(&key);
        }
        let states = mgr.method_states();
        let (_, _, count) = states.iter().find(|(k, _, _)| k == &key).unwrap();
        assert_eq!(*count, 50);
    }

    // ── C1 trigger ───────────────────────────────────────────────────────

    #[test]
    fn c1_triggered_after_threshold() {
        let mgr = TieredCompilationManager::with_default_policy();
        let key = test_key();
        // Drive exactly `c1_threshold` invocations rather than a hardcoded
        // literal, so this test stays correct regardless of the default
        // policy's threshold value (currently 500; see
        // the rationale comment on `CompilationPolicy::default`).
        let c1_threshold = mgr.policy().c1_threshold;
        let mut triggered = None;
        for _ in 0..c1_threshold {
            if let Some(tier) = mgr.on_method_invocation(&key) {
                triggered = Some(tier);
            }
        }
        assert_eq!(triggered, Some(CompilationTier::C1));
    }

    // ── C2 trigger ───────────────────────────────────────────────────────

    #[test]
    fn c2_triggered_after_threshold_with_profile() {
        let policy = CompilationPolicy {
            c1_threshold: 10,
            c2_threshold: 100,
            c2_min_invocations: 50,
            ..CompilationPolicy::default()
        };
        let mgr = TieredCompilationManager::new(policy);
        let key = test_key();

        // Run up to C1 compilation.
        for _ in 0..10 {
            mgr.on_method_invocation(&key);
        }
        // Drain the C1 task.
        let task = mgr.dequeue_compilation().unwrap();
        assert_eq!(task.target_tier, CompilationTier::C1);
        mgr.compilation_complete(&key, CompilationTier::C1, 5);

        // Keep invoking until C2 threshold.
        let mut c2_triggered = false;
        for _ in 10..100 {
            if let Some(tier) = mgr.on_method_invocation(&key) {
                if tier == CompilationTier::C2 {
                    c2_triggered = true;
                }
            }
        }
        assert!(c2_triggered);
    }

    // ── C1 → C2 on the publish, because nothing asks afterwards ──────────
    //
    // `c2_triggered_after_threshold_with_profile`, directly above, is the test
    // that let this bug live: it keeps calling `on_method_invocation` after the
    // C1 publish, and the interpreter does not. Every VM call site that reaches
    // the tiered manager is guarded by "this method has no compiled body"
    // (`dispatch_static.rs`'s `jit_cache` probe rewrites the site to
    // `CachedInvokeTarget::Jit` and returns before the counting block;
    // `dispatch_virtual.rs` counts only in the probe's `None` arm;
    // `invoke_fast.rs`'s door declines when `callee_has_compiled_body`), and
    // `execute_jit_call` counts nothing. `current_tier == C1` means a body WAS
    // published, so the hook that would evaluate `should_compile`'s C1→C2 arm
    // is never called again and the arm was unreachable in steady state.
    //
    // These cover the door that replaces it: the same predicate, asked once, on
    // the publish itself. See `CompilerCore::promote_hot_c1_to_c2`.

    /// The policy these tests share: a C1 bar low enough to cross by hand, and
    /// a C2 bar (`max(c2_threshold, c2_min_invocations)` = 50) that a method
    /// can reach WHILE its C1 compile is in flight — which is exactly the shape
    /// the 100 000-invocation probe produced, where the manager's count froze
    /// at ~5 000 with `c2_threshold` lowered to 1 000 and nothing happened.
    fn hot_c1_policy() -> CompilationPolicy {
        CompilationPolicy {
            c1_threshold: 5,
            c2_threshold: 50,
            c2_min_invocations: 20,
            ..CompilationPolicy::default()
        }
    }

    /// Drive `key` to `count` invocations, crossing the C1 bar on the way, and
    /// drain the C1 task the crossing enqueues.
    ///
    /// The invocations after the crossing return `None` (the request holds the
    /// method's in-flight slot, so `on_method_invocation_settling` returns
    /// early) but STILL advance `invocation_count` — which is what makes the
    /// count at publish time the interesting number.
    fn warm_to_c1_in_flight(mgr: &TieredCompilationManager, key: &MethodKey, count: u32) {
        let mut saw_c1 = false;
        for _ in 0..count {
            if mgr.on_method_invocation(key) == Some(CompilationTier::C1) {
                saw_c1 = true;
            }
        }
        assert!(saw_c1, "crossing c1_threshold must enqueue a C1 task");
        let task = mgr
            .dequeue_compilation()
            .expect("the C1 task should be queued");
        assert_eq!(task.target_tier, CompilationTier::C1);
    }

    #[test]
    fn a_c1_publish_promotes_a_hot_method_without_another_invocation() {
        let mgr = TieredCompilationManager::new(hot_c1_policy());
        let key = test_key();
        // 60 > the C2 bar of 50, all of it accumulated before the publish.
        warm_to_c1_in_flight(&mgr, &key, 60);

        // The publish. NOTHING is invoked after this point — that is the whole
        // claim: in the VM there is no "after this point" for a compiled
        // method's tiering.
        mgr.compilation_complete(&key, CompilationTier::C1, 1);
        assert_eq!(mgr.current_tier(&key), CompilationTier::C1);

        let task = mgr
            .dequeue_compilation()
            .expect("a method over the C2 bar must be promoted on its C1 publish");
        assert_eq!(task.target_tier, CompilationTier::C2);
        assert_eq!(task.method_key, key);
        assert_eq!(
            task.osr_bci, None,
            "the promotion is a method-entry compile, not an OSR one"
        );
        assert_eq!(
            task.priority,
            CompilationPriority::Low,
            "same priority the C1→C2 supersede uses, so a method both doors \
             would take sees an identical task either way"
        );
    }

    /// JIT r25 wave 2 (`CRATONVM_JIT_TRAP_RECOMPILE_KEEPS_C2`): the trap
    /// re-queue's C2 door queues C2 for a hot method whose optimizing body a
    /// trap evicted, and refuses once the method bailed out of C2. The switch
    /// itself is read by the VM caller; default ON since j27 2026-10-28, `=0`
    /// kills.
    #[test]
    fn r25_a_trapped_optimizing_body_requeues_c2_until_bailout() {
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_TRAP_RECOMPILE_KEEPS_C2", None)],
            || assert!(trap_recompile_keeps_c2_enabled()),
        );
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_TRAP_RECOMPILE_KEEPS_C2", Some("0"))],
            || assert!(!trap_recompile_keeps_c2_enabled()),
        );
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_TRAP_RECOMPILE_KEEPS_C2", Some("1"))],
            || assert!(trap_recompile_keeps_c2_enabled()),
        );
        let mgr = TieredCompilationManager::new(hot_c1_policy());
        let key = test_key();
        warm_to_c1_in_flight(&mgr, &key, 60);
        mgr.compilation_complete(&key, CompilationTier::C1, 1);
        let promoted = mgr
            .dequeue_compilation()
            .expect("a hot method is promoted on its C1 publish");
        assert_eq!(promoted.target_tier, CompilationTier::C2);
        mgr.compilation_complete(&key, CompilationTier::C2, 1);
        mgr.on_deoptimization(
            &key,
            DeoptReason::UnstableIf,
            4,
            DeoptAction::RecompileAndReinterpret,
        );
        assert!(matches!(
            mgr.request_trap_recompile_c2(&key),
            C2RetryAnswer::Enqueued
        ));
        let task = mgr.dequeue_compilation().expect("the C2 re-queue");
        assert_eq!(task.target_tier, CompilationTier::C2);
        mgr.compilation_complete(&key, CompilationTier::C2, 1);
        mgr.on_c2_bailout(&key);
        assert!(matches!(
            mgr.request_trap_recompile_c2(&key),
            C2RetryAnswer::Refused
        ));
    }

    #[test]
    fn a_c1_publish_below_the_c2_bar_is_not_promoted() {
        let mgr = TieredCompilationManager::new(hot_c1_policy());
        let key = test_key();
        // 30 is past `c2_min_invocations` (20) but short of `c2_threshold`
        // (50); the arm needs BOTH, and so does the bar that folds them.
        warm_to_c1_in_flight(&mgr, &key, 30);

        mgr.compilation_complete(&key, CompilationTier::C1, 1);
        assert_eq!(
            mgr.dequeue_compilation(),
            None,
            "a method that never crossed the C2 bar must not be promoted"
        );
    }

    /// JIT r23 wave 11 (`CRATONVM_JIT_C2_AFTER_OPTIMIZING_OSR`, default ON
    /// since the churn page's close): a C1 method below the C2 bar whose
    /// optimizing OSR body is published is promoted to a method-entry C2
    /// compile, in either order of the two publishes; with `=0`, nothing is
    /// queued.
    #[test]
    fn r23w11_an_optimizing_osr_publish_promotes_the_c1_entry_body() {
        const KEY: &str = "CRATONVM_JIT_C2_AFTER_OPTIMIZING_OSR";
        // Order 1: the C1 entry body, then the OSR body.
        for on in [false, true] {
            let mgr = TieredCompilationManager::new(hot_c1_policy());
            let key = test_key();
            warm_to_c1_in_flight(&mgr, &key, 30); // below the C2 bar of 50
            let v = if on { None } else { Some("0") };
            cratonvm_types::flags::with_thread_overrides(&[(KEY, v)], || {
                mgr.compilation_complete(&key, CompilationTier::C1, 1);
                assert_eq!(mgr.dequeue_compilation(), None, "below the bar");
                mgr.request_osr(&key, 5).expect("an OSR task");
                let osr = mgr.dequeue_compilation().expect("the OSR task");
                assert_eq!(osr.osr_bci, Some(5));
                mgr.core
                    .complete_task(&key, CompilationTier::C2, 1, true, true, false);
                let next = mgr.dequeue_compilation();
                if on {
                    let task = next.expect("the C1 entry body promoted on the OSR evidence");
                    assert_eq!(task.target_tier, CompilationTier::C2);
                    assert_eq!(task.osr_bci, None, "a method-entry compile");
                    assert_eq!(task.priority, CompilationPriority::Low);
                } else {
                    assert_eq!(next, None, "off: the old wait for the drain clock");
                }
            });
        }
        // Order 2: the OSR body lands while the C1 compile is in flight; the
        // C1 publish then promotes.
        let mgr = TieredCompilationManager::new(hot_c1_policy());
        let key = test_key();
        warm_to_c1_in_flight(&mgr, &key, 30);
        cratonvm_types::flags::with_thread_overrides(&[(KEY, Some("1"))], || {
            // The OSR slot is independent of the in-flight entry compile.
            mgr.request_osr(&key, 5)
                .expect("an OSR task beside the in-flight C1 compile");
            let _ = mgr.dequeue_compilation().expect("the OSR task");
            mgr.core
                .complete_task(&key, CompilationTier::C2, 1, true, true, false);
            assert_eq!(
                mgr.dequeue_compilation(),
                None,
                "no C1 entry body yet: nothing to promote"
            );
            mgr.compilation_complete(&key, CompilationTier::C1, 1);
            let task = mgr
                .dequeue_compilation()
                .expect("the C1 publish after the OSR body is promoted");
            assert_eq!(task.target_tier, CompilationTier::C2);
            assert_eq!(task.osr_bci, None);
        });
    }

    #[test]
    fn the_c1_publish_promotion_honors_the_c2_bailout() {
        let mgr = TieredCompilationManager::new(hot_c1_policy());
        let key = test_key();
        warm_to_c1_in_flight(&mgr, &key, 60);
        // A method that has already proved it does not survive C2 stays at C1
        // however hot it is — the same refusal `should_compile`'s C1 arm and
        // `request_c2_upgrade` both apply.
        mgr.core.methods.lock().get_mut(&key).unwrap().c2_bailout = true;

        mgr.compilation_complete(&key, CompilationTier::C1, 1);
        assert_eq!(mgr.dequeue_compilation(), None);
    }

    #[test]
    fn the_c1_publish_promotion_honors_ineligibility() {
        let mgr = TieredCompilationManager::new(hot_c1_policy());
        let key = test_key();
        warm_to_c1_in_flight(&mgr, &key, 60);
        mgr.core.methods.lock().get_mut(&key).unwrap().ineligible = true;

        mgr.compilation_complete(&key, CompilationTier::C1, 1);
        assert_eq!(mgr.dequeue_compilation(), None);
    }

    #[test]
    fn a_failed_c1_attempt_does_not_promote() {
        let mgr = TieredCompilationManager::new(hot_c1_policy());
        let key = test_key();
        warm_to_c1_in_flight(&mgr, &key, 60);

        // No body was published, so nothing stops the interpreter asking again
        // and `current_tier` is still `Interpreter`. Promoting here would queue
        // an optimizing compile for a method whose cheap tier never succeeded.
        mgr.core
            .complete_task(&key, CompilationTier::C1, 1, false, false, false);
        assert_eq!(mgr.current_tier(&key), CompilationTier::Interpreter);
        assert_eq!(mgr.dequeue_compilation(), None);
    }

    #[test]
    fn an_osr_completion_does_not_take_the_c1_publish_door() {
        let mgr = TieredCompilationManager::new(hot_c1_policy());
        let key = test_key();
        warm_to_c1_in_flight(&mgr, &key, 60);
        // An OSR artifact is a loop-entry body in a separate cache; it neither
        // advances `current_tier` nor says anything about method-entry tiering.
        mgr.core
            .complete_task(&key, CompilationTier::C1, 1, true, true, false);
        assert_eq!(mgr.dequeue_compilation(), None);
    }

    #[test]
    fn tiering_disabled_closes_the_c1_publish_door() {
        let policy = CompilationPolicy {
            tiered_enabled: false,
            ..hot_c1_policy()
        };
        let mgr = TieredCompilationManager::new(policy);
        let key = test_key();
        // The invocation hook returns before `should_compile` when tiering is
        // off, so warm the state by hand rather than through the C1 crossing.
        for _ in 0..60 {
            mgr.on_method_invocation(&key);
        }
        assert_eq!(mgr.dequeue_compilation(), None, "tiering is off");
        mgr.compilation_complete(&key, CompilationTier::C1, 1);
        assert_eq!(
            mgr.dequeue_compilation(),
            None,
            "a disabled tiering must not reach C2 through the publish door \
             either"
        );
    }

    #[test]
    fn the_c1_publish_promotion_opens_and_closes_exactly_one_branch_window() {
        let mgr = TieredCompilationManager::new(hot_c1_policy());
        let key = test_key();
        warm_to_c1_in_flight(&mgr, &key, 60);
        assert_eq!(mgr.branch_window_balance(), 0);

        mgr.compilation_complete(&key, CompilationTier::C1, 1);
        assert_eq!(
            mgr.branch_window_balance(),
            1,
            "the C2 nomination arms branch profiling"
        );
        let task = mgr.dequeue_compilation().expect("the promotion dispatches");
        mgr.core
            .complete_task(&key, task.target_tier, 1, true, false, false);
        assert_eq!(
            mgr.branch_window_balance(),
            0,
            "and the completion disarms it exactly once"
        );
    }

    /// The C1→C2 supersede finds the slot already taken and is a no-op, rather
    /// than queueing a second optimizing compile of the same method — the
    /// duplicate-install shape `admit`'s one-slot-per-method rule exists to
    /// prevent. This is the ordering the worker loop actually runs: `finish`
    /// (which now carries the promotion) and then `request_c2_upgrade`.
    #[test]
    fn the_supersede_does_not_double_queue_behind_the_publish_promotion() {
        let mgr = TieredCompilationManager::new(hot_c1_policy());
        let key = test_key();
        warm_to_c1_in_flight(&mgr, &key, 60);

        mgr.compilation_complete(&key, CompilationTier::C1, 1);
        mgr.core.request_c2_upgrade(&key);

        let first = mgr.dequeue_compilation().expect("one C2 task");
        assert_eq!(first.target_tier, CompilationTier::C2);
        assert_eq!(
            mgr.dequeue_compilation(),
            None,
            "and only one — the method holds a single method-entry slot"
        );
        assert_eq!(mgr.branch_window_balance(), 1, "one window, not two");
    }

    /// A method the other doors have given up on cannot be re-enqueued through
    /// the VM's eager deopt re-queue: `admit` asks the eligibility gates
    /// itself, so there is no door left that skips them.
    #[test]
    fn an_ineligible_method_is_refused_by_the_eager_requeue_door() {
        let mgr = TieredCompilationManager::with_default_policy();
        let key = test_key();
        mgr.core
            .methods
            .lock()
            .entry(key.clone())
            .or_insert_with(|| MethodState::new(key.clone()))
            .ineligible = true;

        assert!(
            !mgr.enqueue_compilation(CompilationTask {
                method_key: key.clone(),
                target_tier: CompilationTier::C1,
                priority: CompilationPriority::Normal,
                enqueue_time_ms: 0,
                osr_bci: None,
            }),
            "an ineligible method must not be queued by any door"
        );
        assert_eq!(mgr.dequeue_compilation(), None);
        assert_eq!(mgr.ineligible_refusals(), 1);
    }

    /// The same for a method whose compiles have failed their whole retry
    /// budget — the shape that let the deopt path re-queue forever.
    #[test]
    fn a_retry_exhausted_method_is_refused_by_the_eager_requeue_door() {
        let mgr = TieredCompilationManager::with_default_policy();
        let key = test_key();
        mgr.core
            .methods
            .lock()
            .entry(key.clone())
            .or_insert_with(|| MethodState::new(key.clone()))
            .tier_fail_count = MAX_TIER_FAIL_RETRIES;

        assert!(!mgr.enqueue_compilation(CompilationTask {
            method_key: key.clone(),
            target_tier: CompilationTier::C2,
            priority: CompilationPriority::Low,
            enqueue_time_ms: 0,
            osr_bci: None,
        }));
        assert_eq!(mgr.dequeue_compilation(), None);
        assert_eq!(mgr.ineligible_refusals(), 1);
    }

    /// An ineligible refusal reaches the process-wide scheduling counters, not
    /// only the per-manager accessor.
    ///
    /// `ineligible_refusals()` has no reader outside this module's own tests,
    /// so before the producer landed the only way to see a refusal was to hold
    /// the manager that made it. `metrics::summary()` and its JSON are what a
    /// sink reads, and a row nobody increments cannot distinguish "never
    /// happened" from "never wired" — which is the exact claim the row's doc
    /// comment makes about a healthy run staying at zero.
    ///
    /// Per thread (`SchedulingCapture`), for the reason
    /// `drops_reach_the_process_wide_scheduling_counters` gives: `admit` runs
    /// on the caller's thread, and the global table is everyone's.
    #[test]
    fn an_ineligible_refusal_reaches_the_process_wide_scheduling_counters() {
        let counts = crate::metrics::SchedulingCapture::start();

        let mgr = TieredCompilationManager::with_default_policy();
        let key = test_key();
        mgr.core
            .methods
            .lock()
            .entry(key.clone())
            .or_insert_with(|| MethodState::new(key.clone()))
            .ineligible = true;

        assert!(!mgr.enqueue_compilation(CompilationTask {
            method_key: key.clone(),
            target_tier: CompilationTier::C1,
            priority: CompilationPriority::Normal,
            enqueue_time_ms: 0,
            osr_bci: None,
        }));

        assert_eq!(
            counts.count("queue_refused_ineligible"),
            1,
            "the refusal must be visible in the metrics idiom, not only on the manager"
        );
        assert_eq!(mgr.ineligible_refusals(), 1, "and on the manager too");
        // A refusal is NOT a drop: nothing was queued, so the drop total must
        // not have moved. This is the property that keeps index 7 out of
        // `scheduling_dropped_total`, exactly like `queue_deduplicated`.
        assert_eq!(
            counts.count("queue_dropped_stale_install_epoch")
                + counts.count("queue_dropped_class_invalidated")
                + counts.count("queue_shutdown_abandoned"),
            0,
            "an ineligible refusal must not read as a dropped request"
        );
    }

    /// Bands still come out strictly by priority when every request is the same
    /// age, which is every burst that arrives inside one aging window.
    #[test]
    fn same_age_requests_still_leave_in_strict_priority_order() {
        let mut q = CompilationQueue::new();
        let key = test_key();
        let at = |priority| CompilationTask {
            method_key: key.clone(),
            target_tier: CompilationTier::C2,
            priority,
            enqueue_time_ms: 1_000,
            osr_bci: None,
        };
        q.enqueue(at(CompilationPriority::Low), 0);
        q.enqueue(at(CompilationPriority::Normal), 0);
        q.enqueue(at(CompilationPriority::High), 0);

        assert_eq!(
            q.dequeue().unwrap().task.priority,
            CompilationPriority::High
        );
        assert_eq!(
            q.dequeue().unwrap().task.priority,
            CompilationPriority::Normal
        );
        assert_eq!(q.dequeue().unwrap().task.priority, CompilationPriority::Low);
    }

    /// A `Low` C1→C2 upgrade that has waited two bands' worth of time overtakes
    /// a `High` OSR request that has just arrived. Without this, a warm-up
    /// phase that keeps the High band non-empty means no C1→C2 upgrade ever
    /// runs — the two kinds of request share the C2 lane.
    #[test]
    fn a_long_waiting_low_request_overtakes_a_fresh_high_one() {
        let mut q = CompilationQueue::new();
        let key = test_key();
        // The wait is SYNTHESISED, not slept for: `process_uptime_ms` is
        // monotonic, so a stamp far enough in the past is indistinguishable
        // from having really waited, and the test does not pay the time. The
        // sleep runs only in the rare case where the process is younger than
        // the wait being synthesised (this test running first in a fresh
        // binary).
        let wait = 2 * QUEUE_AGING_MS + 20;
        let now = process_uptime_ms();
        if now <= wait {
            std::thread::sleep(std::time::Duration::from_millis(wait - now + 1));
        }
        q.enqueue(
            CompilationTask {
                method_key: key.clone(),
                target_tier: CompilationTier::C2,
                priority: CompilationPriority::Low,
                // Never 0: `0` is the unset sentinel `enqueue` re-stamps.
                enqueue_time_ms: (process_uptime_ms() - wait).max(1),
                osr_bci: None,
            },
            0,
        );
        q.enqueue(
            CompilationTask {
                method_key: key.clone(),
                target_tier: CompilationTier::C2,
                priority: CompilationPriority::High,
                enqueue_time_ms: 0,
                osr_bci: Some(7),
            },
            0,
        );

        assert_eq!(
            q.dequeue().unwrap().task.priority,
            CompilationPriority::Low,
            "a starving Low request must eventually outrank a fresh High one"
        );
    }

    /// `enqueue` stamps the arrival time a door left unset, so the aging term
    /// has something to read and the queue-latency field stops being dead.
    #[test]
    fn enqueue_stamps_an_unset_arrival_time_and_keeps_a_supplied_one() {
        let mut q = CompilationQueue::new();
        let key = test_key();
        q.enqueue(
            CompilationTask {
                method_key: key.clone(),
                target_tier: CompilationTier::C1,
                priority: CompilationPriority::Normal,
                enqueue_time_ms: 0,
                osr_bci: None,
            },
            0,
        );
        assert_ne!(
            q.dequeue().unwrap().task.enqueue_time_ms,
            0,
            "an unset arrival time is stamped on the way into the queue"
        );
        q.enqueue(
            CompilationTask {
                method_key: key,
                target_tier: CompilationTier::C1,
                priority: CompilationPriority::Normal,
                enqueue_time_ms: 42,
                osr_bci: None,
            },
            0,
        );
        assert_eq!(
            q.dequeue().unwrap().task.enqueue_time_ms,
            42,
            "a door that supplied one keeps it"
        );
        // r11-tier: a wall-clock stamp (the deopt re-queue's) is restamped.
        q.enqueue(
            CompilationTask {
                method_key: test_key(),
                target_tier: CompilationTier::C1,
                priority: CompilationPriority::High,
                enqueue_time_ms: 1_758_000_000_000,
                osr_bci: None,
            },
            0,
        );
        let stamped = q.dequeue().map_or(0, |r| r.task.enqueue_time_ms);
        assert!(
            stamped != 0 && stamped < 1_758_000_000_000,
            "a stamp on another clock must be restamped, got {stamped}"
        );
    }

    /// `set_policy` has to re-mirror the bar onto the core, or an operator who
    /// lowers `c2_threshold` at runtime gets the old bar on the publish door —
    /// which is the same class of inert-knob bug this whole door exists to fix.
    #[test]
    fn lowering_the_c2_threshold_at_runtime_reaches_the_publish_door() {
        let policy = CompilationPolicy {
            c1_threshold: 5,
            c2_threshold: 1_000_000,
            c2_min_invocations: 1_000_000,
            ..CompilationPolicy::default()
        };
        let mgr = TieredCompilationManager::new(policy);
        let key = test_key();
        warm_to_c1_in_flight(&mgr, &key, 60);

        mgr.set_policy(hot_c1_policy());
        mgr.compilation_complete(&key, CompilationTier::C1, 1);
        let task = mgr
            .dequeue_compilation()
            .expect("the new bar must be the one the publish door asks");
        assert_eq!(task.target_tier, CompilationTier::C2);
    }

    // ── Queue priority ordering ──────────────────────────────────────────

    #[test]
    fn queue_priority_ordering() {
        let mgr = TieredCompilationManager::with_default_policy();
        let k1 = MethodKey::new("A", "a", "()V");
        let k2 = MethodKey::new("B", "b", "()V");
        let k3 = MethodKey::new("C", "c", "()V");

        mgr.enqueue_compilation(CompilationTask {
            method_key: k2.clone(),
            target_tier: CompilationTier::C1,
            priority: CompilationPriority::Normal,
            enqueue_time_ms: 0,
            osr_bci: None,
        });
        mgr.enqueue_compilation(CompilationTask {
            method_key: k3.clone(),
            target_tier: CompilationTier::C1,
            priority: CompilationPriority::Low,
            enqueue_time_ms: 0,
            osr_bci: None,
        });
        mgr.enqueue_compilation(CompilationTask {
            method_key: k1.clone(),
            target_tier: CompilationTier::C2,
            priority: CompilationPriority::High,
            enqueue_time_ms: 0,
            osr_bci: None,
        });

        // The three sit on two lanes (C1 and C2); the manual drain still
        // hands them out in priority order across both.
        let t1 = mgr.dequeue_compilation().unwrap();
        assert_eq!(t1.priority, CompilationPriority::High);
        let t2 = mgr.dequeue_compilation().unwrap();
        assert_eq!(t2.priority, CompilationPriority::Normal);
        let t3 = mgr.dequeue_compilation().unwrap();
        assert_eq!(t3.priority, CompilationPriority::Low);
    }

    // ── Enqueue / dequeue roundtrip ──────────────────────────────────────

    #[test]
    fn enqueue_dequeue_roundtrip() {
        let mgr = TieredCompilationManager::with_default_policy();
        let key = test_key();
        mgr.enqueue_compilation(CompilationTask {
            method_key: key.clone(),
            target_tier: CompilationTier::C1,
            priority: CompilationPriority::Normal,
            enqueue_time_ms: 42,
            osr_bci: None,
        });
        let task = mgr.dequeue_compilation().unwrap();
        assert_eq!(task.method_key, key);
        assert_eq!(task.enqueue_time_ms, 42);
    }

    // ── Compilation complete updates tier ────────────────────────────────

    #[test]
    fn compilation_complete_updates_tier() {
        let mgr = TieredCompilationManager::with_default_policy();
        let key = test_key();
        mgr.on_method_invocation(&key); // create state
        mgr.compilation_complete(&key, CompilationTier::C1, 10);
        assert_eq!(mgr.current_tier(&key), CompilationTier::C1);
    }

    // ── Failed compile attempts do not fake "compiled" and DO retry ──────
    //
    // Regression coverage for the bg-compile-no-publish bug: a
    // `compile_fn` that runs but bails (skip-listed method, resolver miss,
    // code-cache cap, ...) must not be recorded as having reached `tier` —
    // `current_tier` has to stay put so a later invocation gets another
    // shot, and the per-tier compilation stat must not count a body that
    // was never published.

    #[test]
    fn failed_compile_does_not_advance_tier_or_stats() {
        let mgr = TieredCompilationManager::with_default_policy();
        let key = test_key();
        mgr.on_method_invocation(&key); // create state
        mgr.core
            .complete_task(&key, CompilationTier::C1, 10, false, false, false);
        assert_eq!(
            mgr.current_tier(&key),
            CompilationTier::Interpreter,
            "a failed attempt must not advance current_tier"
        );
        assert_eq!(
            mgr.stats().c1_compilations.load(Ordering::Relaxed),
            0,
            "a failed attempt must not count as a C1 compilation"
        );
    }

    // ── Policy declines are recorded once, not charged to the retry budget ──
    //
    // A method the VM refuses on policy grounds (skip list, OSR denial) used
    // to report `success=false` exactly like a failed compile, so it was
    // enqueued and declined three times before `tier_fail_count` saturated.
    // Two of those round-trips were waste, and the resulting
    // `tier_fail_count=3` was indistinguishable from genuinely broken codegen
    // in `hot_but_stuck_in_interpreter`.

    #[test]
    fn permanent_decline_bans_on_first_attempt_without_spending_retries() {
        let policy = CompilationPolicy {
            c1_threshold: 1,
            ..CompilationPolicy::default()
        };
        let mgr = TieredCompilationManager::new(policy);
        let key = test_key();

        assert_eq!(mgr.on_method_invocation(&key), Some(CompilationTier::C1));
        mgr.core
            .complete_task(&key, CompilationTier::C1, 0, false, false, true);

        {
            let methods = mgr.core.methods.lock();
            let state = &methods[&key];
            assert!(state.ineligible, "a policy decline must be recorded");
            assert_eq!(
                state.tier_fail_count, 0,
                "a policy decline must NOT spend the compile-failure retry budget"
            );
        }

        // ONE decline is enough: no further invocation may re-enqueue it.
        for _ in 0..10 {
            assert_eq!(
                mgr.on_method_invocation(&key),
                None,
                "an ineligible method must never be re-enqueued"
            );
        }
    }

    /// Interpreter round i6 wave 116, lane L2 (page `i107-L2`): the
    /// waiting-on-class park, driven as the worker drives it (a completion
    /// carrying the epoch read before the compile). Target independent: the
    /// only producer today is the aarch64 block, the policy is here.
    fn i116_park(mgr: &TieredCompilationManager, key: &MethodKey, epoch: u64) {
        mgr.core.finish(
            key,
            Completion {
                tier: CompilationTier::C1,
                osr_bci: None,
                osr: false,
                compile_time_ms: 0,
                compile_time_ns: 0,
                budget_ms: 0,
                success: false,
                declined_permanently: false,
                epoch_moved: false,
                waiting_on_class: Some(epoch),
                cap_refused: false,
                ticket: 0,
            },
        );
    }

    fn i116_manager(epoch: &Arc<AtomicU64>) -> TieredCompilationManager {
        let mgr = TieredCompilationManager::new(CompilationPolicy {
            c1_threshold: 1,
            ..CompilationPolicy::default()
        });
        let source = Arc::clone(epoch);
        assert!(mgr.set_class_epoch_source(Box::new(move || source.load(Ordering::Acquire))));
        mgr
    }

    #[test]
    fn i116_l2_waiting_on_class_is_not_charged_and_waits_for_a_definition() {
        let epoch = Arc::new(AtomicU64::new(7));
        let mgr = i116_manager(&epoch);
        let key = test_key();
        // More waits than the failure budget would ever allow.
        for round in 0..(MAX_TIER_FAIL_RETRIES + 3) {
            assert_eq!(
                mgr.on_method_invocation(&key),
                Some(CompilationTier::C1),
                "round {round}: a parked method is offered again once a class is defined"
            );
            i116_park(&mgr, &key, epoch.load(Ordering::Acquire));
            assert_eq!(
                mgr.on_method_invocation(&key),
                None,
                "round {round}: no class defined, no new compile"
            );
            epoch.fetch_add(1, Ordering::AcqRel);
        }
        let methods = mgr.core.methods.lock();
        assert_eq!(methods[&key].tier_fail_count, 0);
        assert_eq!(methods[&key].waiting_on_class_parks, MAX_TIER_FAIL_RETRIES + 3);
        drop(methods);
        assert_eq!(mgr.waiting_on_class_parks(), u64::from(MAX_TIER_FAIL_RETRIES + 3));
    }

    #[test]
    fn i116_l2_a_parked_method_is_let_through_on_the_release_stride() {
        let epoch = Arc::new(AtomicU64::new(1));
        let mgr = i116_manager(&epoch);
        let key = test_key();
        assert_eq!(mgr.on_method_invocation(&key), Some(CompilationTier::C1));
        i116_park(&mgr, &key, 1);
        for i in 1..WAITING_ON_CLASS_RELEASE_OFFERS {
            assert_eq!(mgr.on_method_invocation(&key), None, "offer {i}");
        }
        // The epoch never moved (a loaded owner whose `<clinit>` ran meanwhile).
        assert_eq!(mgr.on_method_invocation(&key), Some(CompilationTier::C1));
    }

    #[test]
    fn i116_l2_waits_past_the_bound_are_charged_and_a_publish_ends_the_wait() {
        let epoch = Arc::new(AtomicU64::new(1));
        let mgr = i116_manager(&epoch);
        let key = test_key();
        assert_eq!(mgr.on_method_invocation(&key), Some(CompilationTier::C1));
        mgr.core.methods.lock().get_mut(&key).unwrap().waiting_on_class_parks =
            MAX_WAITING_ON_CLASS_PARKS;
        i116_park(&mgr, &key, 1);
        {
            let methods = mgr.core.methods.lock();
            assert_eq!(methods[&key].tier_fail_count, 1, "past the bound: charged");
            assert_eq!(methods[&key].waiting_on_class_epoch, None);
        }
        // Without an epoch source nothing parks: the worker sends `None`.
        let other = MethodKey::new("Other", "m", "()V");
        let plain = TieredCompilationManager::new(CompilationPolicy {
            c1_threshold: 1,
            ..CompilationPolicy::default()
        });
        assert_eq!(plain.on_method_invocation(&other), Some(CompilationTier::C1));
        i116_park(&plain, &other, 5);
        // A park recorded with no source is released at the next offer.
        assert_eq!(plain.on_method_invocation(&other), Some(CompilationTier::C1));
        // A publish ends a wait.
        let key2 = test_key2();
        assert_eq!(mgr.on_method_invocation(&key2), Some(CompilationTier::C1));
        i116_park(&mgr, &key2, 1);
        mgr.core
            .complete_task(&key2, CompilationTier::C1, 0, true, false, false);
        assert_eq!(mgr.core.methods.lock()[&key2].waiting_on_class_epoch, None);
    }

    /// Interpreter round i6 wave 117, lane L9 (review; page
    /// `i117-L9-a-one-shot-c2-retry-is-eaten-by-the-waiting-on-class-park`):
    /// a late-bind retry (a deferred site linked, e.g. a static owner whose
    /// `<clinit>` ran, which moves no class-definition epoch) is not eaten by
    /// the park: it ends the park and is queued.
    #[test]
    fn i117_l9_a_late_bind_retry_ends_the_waiting_on_class_park() {
        let epoch = Arc::new(AtomicU64::new(3));
        let mgr = i116_manager(&epoch);
        let key = test_key();
        assert_eq!(mgr.on_method_invocation(&key), Some(CompilationTier::C1));
        i116_park(&mgr, &key, 3);
        assert_eq!(mgr.on_method_invocation(&key), None, "parked");
        assert_eq!(
            mgr.request_late_bind_recompile(&key),
            C2RetryAnswer::Enqueued,
            "the one-shot retry is queued, not refused as a parked offer"
        );
        assert_eq!(mgr.core.methods.lock()[&key].waiting_on_class_epoch, None);
    }

    /// `CRATONVM_JIT_LATE_BIND_RETRY_OWED` (page
    /// `jr21-osr-early-reentry-request-rows-FIXED-20261026.md`): a late-bind retry
    /// refused `already queued` is owed once, to be asked after the task
    /// holding the slot finishes; other doors owe nothing, nor does `=0`.
    #[test]
    fn a_late_bind_retry_refused_already_queued_is_owed_once() {
        const OWED: &str = "CRATONVM_JIT_LATE_BIND_RETRY_OWED";
        let epoch = Arc::new(AtomicU64::new(1));
        let mgr = epoch_driven_manager(&epoch);
        let key = epoch_key("lateBindOwed");
        cratonvm_types::flags::with_thread_overrides(&[(OWED, None)], || {
            assert_eq!(
                mgr.request_late_bind_recompile(&key),
                C2RetryAnswer::Enqueued
            );
            assert!(
                !mgr.core.take_late_bind_retry_owed(&key),
                "nothing owed yet"
            );
            mgr.request_self_lock_recompile(&key);
            assert!(
                !mgr.core.take_late_bind_retry_owed(&key),
                "another door owes nothing"
            );
            assert_eq!(
                mgr.request_late_bind_recompile(&key),
                C2RetryAnswer::AlreadyQueued
            );
            assert!(mgr.core.take_late_bind_retry_owed(&key), "owed");
            assert!(!mgr.core.take_late_bind_retry_owed(&key), "taken once");
        });
        cratonvm_types::flags::with_thread_overrides(&[(OWED, Some("0"))], || {
            assert_eq!(
                mgr.request_late_bind_recompile(&key),
                C2RetryAnswer::AlreadyQueued
            );
            assert!(!mgr.core.take_late_bind_retry_owed(&key), "kill word");
        });
    }

    #[test]
    fn permanent_decline_does_not_ban_unrelated_methods() {
        let policy = CompilationPolicy {
            c1_threshold: 1,
            ..CompilationPolicy::default()
        };
        let mgr = TieredCompilationManager::new(policy);
        let declined = test_key();
        let healthy = MethodKey::new("Other", "m", "()V");

        assert_eq!(
            mgr.on_method_invocation(&declined),
            Some(CompilationTier::C1)
        );
        mgr.core
            .complete_task(&declined, CompilationTier::C1, 0, false, false, true);

        // The other method is untouched and still compiles normally.
        assert_eq!(
            mgr.on_method_invocation(&healthy),
            Some(CompilationTier::C1)
        );
        mgr.core
            .complete_task(&healthy, CompilationTier::C1, 1, true, false, false);
        assert_eq!(mgr.current_tier(&healthy), CompilationTier::C1);
    }

    #[test]
    fn failed_compile_is_retried_up_to_the_fail_limit() {
        let policy = CompilationPolicy {
            c1_threshold: 1,
            ..CompilationPolicy::default()
        };
        let mgr = TieredCompilationManager::new(policy);
        let key = test_key();

        // 1st invocation crosses c1_threshold=1 and enqueues C1.
        assert_eq!(mgr.on_method_invocation(&key), Some(CompilationTier::C1));
        // Fail it MAX_TIER_FAIL_RETRIES - 1 times; each failure must still
        // leave the method eligible for another attempt (slot released,
        // current_tier untouched).
        for i in 0..(MAX_TIER_FAIL_RETRIES - 1) {
            mgr.core
                .complete_task(&key, CompilationTier::C1, 1, false, false, false);
            assert_eq!(
                mgr.on_method_invocation(&key),
                Some(CompilationTier::C1),
                "attempt {i}: should still be retried below the fail limit"
            );
        }
        // One more failure reaches MAX_TIER_FAIL_RETRIES — should_compile
        // must now give up permanently.
        mgr.core
            .complete_task(&key, CompilationTier::C1, 1, false, false, false);
        assert_eq!(
            mgr.on_method_invocation(&key),
            None,
            "should stop recommending compilation after the fail limit"
        );
        assert_eq!(mgr.current_tier(&key), CompilationTier::Interpreter);
    }

    #[test]
    fn successful_compile_after_a_failure_resets_the_fail_count() {
        let policy = CompilationPolicy {
            c1_threshold: 1,
            ..CompilationPolicy::default()
        };
        let mgr = TieredCompilationManager::new(policy);
        let key = test_key();
        mgr.on_method_invocation(&key);
        mgr.core
            .complete_task(&key, CompilationTier::C1, 1, false, false, false);
        mgr.core
            .complete_task(&key, CompilationTier::C1, 5, true, false, false);
        assert_eq!(mgr.current_tier(&key), CompilationTier::C1);
        let methods = mgr.core.methods.lock();
        assert_eq!(
            methods[&key].tier_fail_count, 0,
            "a later success should reset the fail streak"
        );
    }

    // Regression coverage for the OSR-starves-method-entry bug: a successful
    // OSR compile publishes into the SEPARATE OSR artifact cache, so it must
    // not stamp `current_tier = C2` — that made `should_compile` refuse every
    // later method-entry recommendation while the method-entry cache was
    // still empty, so each fresh invocation of a loop-heavy method (e.g.
    // QuickBench sieve) re-entered the interpreter and re-OSR'd forever.
    #[test]
    fn osr_completion_does_not_suppress_method_entry_tiering() {
        let policy = CompilationPolicy {
            c1_threshold: 1,
            ..CompilationPolicy::default()
        };
        let mgr = TieredCompilationManager::new(policy);
        let key = test_key();

        // A hot back-edge enqueues an OSR task (target tier C2) before the
        // invocation counter has recommended anything.
        let task = mgr.request_osr(&key, 42).expect("OSR task should enqueue");
        assert_eq!(task.osr_bci, Some(42));
        // The worker completes it successfully — artifact goes to the OSR
        // cache, `osr = true`.
        mgr.core
            .complete_task(&key, task.target_tier, 3, true, true, false);
        assert_eq!(
            mgr.current_tier(&key),
            CompilationTier::Interpreter,
            "an OSR publish must not advance the method-entry tier"
        );
        // The invocation counter must still be able to recommend the
        // method-entry compile.
        assert_eq!(
            mgr.on_method_invocation(&key),
            Some(CompilationTier::C1),
            "method-entry compilation must still be recommended after an OSR publish"
        );
        // And a successful method-entry completion advances the tier as usual.
        mgr.core
            .complete_task(&key, CompilationTier::C1, 2, true, false, false);
        assert_eq!(mgr.current_tier(&key), CompilationTier::C1);
    }

    /// RT-8. A swept body demotes the method so it can be compiled again.
    ///
    /// The failure this catches is invisible rather than loud: without
    /// `note_body_withdrawn` the method sits at C1 with no body, so
    /// `on_method_invocation` — the only door the interpreter ever knocks on —
    /// answers `None` forever, and the method is interpreted for the life of
    /// the process. Nothing counts a method that is simply never offered, which
    /// is why this is asserted here rather than left to a benchmark.
    #[test]
    fn a_withdrawn_body_demotes_the_method_so_it_can_be_compiled_again() {
        let mgr = TieredCompilationManager::with_default_policy();
        let key = test_key();
        mgr.on_method_invocation(&key);
        mgr.compilation_complete(&key, CompilationTier::C1, 50);
        assert_eq!(mgr.current_tier(&key), CompilationTier::C1);
        // At C1, the method-entry door is shut: `should_compile`'s C1 arm is
        // the C2 one, and it wants ~20 000 invocations from a counter that has
        // no writer once a C1 body exists.
        assert_eq!(mgr.on_method_invocation(&key), None);

        mgr.note_body_withdrawn(&key);
        assert_eq!(mgr.current_tier(&key), CompilationTier::Interpreter);

        // The invocation count is deliberately KEPT, so the method re-admits at
        // its next stride instead of re-warming from zero — a method cold
        // enough to sweep would take a very long time to rebuild a count.
        let count = {
            let methods = mgr.core.methods.lock();
            methods[&key].invocation_count
        };
        assert!(
            count > 0,
            "note_body_withdrawn must not reset the invocation count"
        );
    }

    /// Round 11 wave 18, P1 (`r11-orch-callee-compiled-bodies-are-never-promoted`):
    /// a body published by a door the manager did not schedule raises the tier,
    /// so the drained count is judged by the C1 arm, and C2 is queued at the bar.
    #[test]
    fn a_body_published_outside_the_manager_is_promoted_by_its_drained_count() {
        if !crate::entry_counter::callee_promotion_enabled() {
            return; // the kill switch is set in this environment
        }
        let mgr = TieredCompilationManager::with_default_policy();
        let key = test_key();
        mgr.note_published_outside_manager(&key, CompilationTier::C1);
        assert_eq!(mgr.current_tier(&key), CompilationTier::C1);
        assert_eq!(
            mgr.on_observed_invocations(&key, 600).recommended,
            None,
            "below the C2 bar a C1 body is left alone"
        );
        assert_eq!(
            mgr.on_observed_invocations(&key, 20_000).recommended,
            Some(CompilationTier::C2),
            "the drained count crosses the C2 bar through the C1 arm"
        );
        // Never lowers: a second single-pass publish leaves C1 (the queued C2
        // task is still in flight) and an `Interpreter` note is ignored.
        mgr.note_published_outside_manager(&key, CompilationTier::Interpreter);
        assert_eq!(mgr.current_tier(&key), CompilationTier::C1);
    }

    /// JIT r25 wave 4 (`CRATONVM_JIT_OUTSIDE_C1_DRAIN_C2`, default ON since
    /// j27 2026-10-28, `=0` kills): an outside C1 body is promoted at the
    /// HotSpot `Tier4InvocationThreshold` of drained entries under the switch,
    /// and only at the full bar at `=0`.
    #[test]
    fn r25_an_outside_c1_body_is_promoted_at_the_tier4_bar_under_the_switch() {
        if !crate::entry_counter::callee_promotion_enabled() {
            return;
        }
        const KEY: &str = "CRATONVM_JIT_OUTSIDE_C1_DRAIN_C2";
        cratonvm_types::flags::with_thread_overrides(&[(KEY, None)], || {
            assert!(outside_c1_drain_c2_enabled(), "unset is ON since j27");
        });
        cratonvm_types::flags::with_thread_overrides(&[(KEY, Some("0"))], || {
            assert!(!outside_c1_drain_c2_enabled());
            let mgr = TieredCompilationManager::with_default_policy();
            let key = test_key();
            mgr.note_published_outside_manager(&key, CompilationTier::C1);
            assert_eq!(
                mgr.on_observed_invocations(&key, OUTSIDE_C1_DRAIN_C2_BAR).recommended,
                None,
                "off: the full bar"
            );
        });
        cratonvm_types::flags::with_thread_overrides(&[(KEY, Some("1"))], || {
            assert!(outside_c1_drain_c2_enabled());
            let mgr = TieredCompilationManager::with_default_policy();
            let key = test_key();
            mgr.note_published_outside_manager(&key, CompilationTier::C1);
            assert_eq!(
                mgr.on_observed_invocations(&key, OUTSIDE_C1_DRAIN_C2_BAR - 1).recommended,
                None
            );
            assert_eq!(
                mgr.on_observed_invocations(&key, OUTSIDE_C1_DRAIN_C2_BAR).recommended,
                Some(CompilationTier::C2)
            );
            let task = mgr.dequeue_compilation().expect("the C2 task");
            assert_eq!(task.target_tier, CompilationTier::C2);
            // A manager-scheduled C1 publish is not this rule's.
            let other = test_key2();
            let _ = mgr.on_observed_invocations(&other, 1);
            assert_eq!(
                mgr.on_observed_invocations(&other, OUTSIDE_C1_DRAIN_C2_BAR).recommended,
                None
            );
        });
    }

    /// HANDLER8-1 census (JIT round 19 wave 9, lane handler9): an outside C1
    /// publish that passes the supersede predicate is a candidate ONCE, and
    /// its count's later crossing of the C2 bar is counted; a non-candidate
    /// publish counts only as a publish. No switch is read on this path.
    #[test]
    fn the_outside_manager_census_counts_candidates_and_their_bar_crossing() {
        if !crate::entry_counter::callee_promotion_enabled() {
            return;
        }
        let mgr = TieredCompilationManager::with_default_policy();
        let key = test_key();
        let other = test_key2();
        mgr.note_published_outside_manager_with_c2_candidate(&key, CompilationTier::C1, true, true);
        mgr.note_published_outside_manager_with_c2_candidate(&key, CompilationTier::C1, true, true);
        mgr.note_published_outside_manager_with_c2_candidate(
            &other,
            CompilationTier::C1,
            false,
            true,
        );
        let census = &mgr.core.outside_c2;
        assert_eq!(census.c1_publishes.load(Ordering::Relaxed), 3);
        assert_eq!(
            census.candidates.load(Ordering::Relaxed),
            1,
            "once per method"
        );
        let _ = mgr.on_observed_invocations(&key, 600);
        assert_eq!(census.crossed_bar.load(Ordering::Relaxed), 0);
        let _ = mgr.on_observed_invocations(&key, 20_000);
        assert_eq!(census.crossed_bar.load(Ordering::Relaxed), 1);
        let _ = mgr.on_observed_invocations(&key, 40_000);
        assert_eq!(
            census.crossed_bar.load(Ordering::Relaxed),
            1,
            "counted once"
        );
        let _ = mgr.on_observed_invocations(&other, 30_000);
        assert_eq!(
            census.crossed_bar.load(Ordering::Relaxed),
            1,
            "not a candidate"
        );
        let line = mgr.outside_manager_c2_census_line();
        assert!(line.contains("c1-publishes=3 candidates=1"), "{line}");
        assert!(line.contains("crossed-bar=1 never-crossed=0"), "{line}");
    }

    /// HANDLER11-6 (lane handler12): a warm candidate whose C1 body predicts a
    /// refused forward is counted and not superseded; one that predicts a
    /// forward is admitted as before.
    #[test]
    fn an_outside_candidate_without_a_predicted_forward_is_not_superseded() {
        if !crate::entry_counter::callee_promotion_enabled() {
            return;
        }
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_OUTSIDE_MANAGER_C2_UPGRADE", Some("1"))],
            || {
                let mgr = TieredCompilationManager::with_default_policy();
                let key = test_key();
                let other = test_key2();
                let c1 = CompilationTier::C1;
                mgr.note_published_outside_manager_gated(&key, c1, true, true, false);
                let census = &mgr.core.outside_c2;
                assert_eq!(census.forward_refused.load(Ordering::Relaxed), 1);
                assert_eq!(census.upgrades_admitted.load(Ordering::Relaxed), 0);
                assert!(!mgr.core.methods.lock()[&key].queued_for_compilation);
                mgr.note_published_outside_manager_gated(&other, c1, true, true, true);
                assert_eq!(census.forward_refused.load(Ordering::Relaxed), 1);
                assert!(
                    mgr.outside_manager_c2_census_line()
                        .contains("forward-refused=1"),
                    "{}",
                    mgr.outside_manager_c2_census_line()
                );
            },
        );
    }

    /// HANDLER9-2 (lane handler11): a C2 publication over a census candidate's
    /// C1 body is split by whether the C1 body was forwarded; a method that was
    /// never a candidate is not counted.
    #[test]
    fn the_outside_census_splits_c2_publications_by_the_c1_forward() {
        if !crate::entry_counter::callee_promotion_enabled() {
            return;
        }
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_DBG_OUTSIDE_MANAGER_C2_CENSUS", Some("1"))],
            || {
                let mgr = TieredCompilationManager::with_default_policy();
                let key = test_key();
                let other = test_key2();
                mgr.note_published_outside_manager_with_c2_candidate(
                    &key,
                    CompilationTier::C1,
                    true,
                    true,
                );
                mgr.note_published_outside_manager(&other, CompilationTier::C1);
                mgr.note_c2_supersede_of_outside_candidate(&key, true);
                mgr.note_c2_supersede_of_outside_candidate(&key, false);
                mgr.note_c2_supersede_of_outside_candidate(&other, true);
                let line = mgr.outside_manager_c2_census_line();
                assert!(
                    line.contains("c2-published=2 c1-forwarded=1 c1-kept-entrant=1"),
                    "{line}"
                );
            },
        );
    }

    /// HANDLER8-1 supersede door: admits through `request_c2_upgrade` under a
    /// per-VM budget; a refusal by the door's own gates does not spend it.
    #[test]
    fn the_outside_manager_supersede_is_bounded_by_its_budget() {
        if !crate::entry_counter::callee_promotion_enabled() {
            return;
        }
        let mgr = TieredCompilationManager::with_default_policy();
        let key = test_key();
        let other = test_key2();
        mgr.note_published_outside_manager(&key, CompilationTier::C1);
        mgr.note_published_outside_manager(&other, CompilationTier::C1);
        let census = &mgr.core.outside_c2;
        census.budget_left.store(1, Ordering::Relaxed);
        assert!(mgr.core.request_outside_manager_c2_upgrade(&key));
        assert!(mgr.core.methods.lock()[&key].queued_for_compilation);
        assert!(
            !mgr.core.request_outside_manager_c2_upgrade(&other),
            "the budget is spent"
        );
        assert_eq!(census.budget_refused.load(Ordering::Relaxed), 1);
        assert!(!mgr.core.methods.lock()[&other].queued_for_compilation);
        // A gate refusal (already queued) refunds its unit.
        census.budget_left.store(1, Ordering::Relaxed);
        assert!(!mgr.core.request_outside_manager_c2_upgrade(&key));
        assert_eq!(census.upgrades_refused.load(Ordering::Relaxed), 1);
        assert_eq!(census.budget_left.load(Ordering::Relaxed), 1);
        assert_eq!(census.upgrades_admitted.load(Ordering::Relaxed), 1);
    }

    /// Wave-9 follow-up: a candidate whose profile is thinner than the manager
    /// path's is still a census candidate, and with the supersede off nothing
    /// is counted as refused (the gate is asked only of an armed supersede).
    #[test]
    fn a_cold_profile_candidate_is_counted_but_not_refused_while_unarmed() {
        if !crate::entry_counter::callee_promotion_enabled() || outside_manager_c2_upgrade_enabled()
        {
            return;
        }
        let mgr = TieredCompilationManager::with_default_policy();
        let key = test_key();
        mgr.note_published_outside_manager_with_c2_candidate(
            &key,
            CompilationTier::C1,
            true,
            false,
        );
        let census = &mgr.core.outside_c2;
        assert_eq!(census.candidates.load(Ordering::Relaxed), 1);
        assert_eq!(census.profile_refused.load(Ordering::Relaxed), 0);
        assert!(!mgr.core.methods.lock()[&key].queued_for_compilation);
    }

    /// MON11-4: HotSpot's Tier4 numbers, with the drain's 8:1 share taken
    /// back off the count.
    #[test]
    fn the_tier4_i_plus_b_predicate_is_hotspots() {
        // 715 calls of a 20-iteration loop: 715 + 14 300 >= 15 000.
        let b = 715 * 20;
        assert!(tier4_i_plus_b_due(715 + (b >> 3), b));
        assert!(!tier4_i_plus_b_due(700 + ((700 * 20) >> 3), 700 * 20));
        // Never below 600 entries, however many back edges.
        assert!(!tier4_i_plus_b_due(599 + (1_000_000 >> 3), 1_000_000));
        assert!(tier4_i_plus_b_due(600 + (1_000_000 >> 3), 1_000_000));
        assert!(tier4_i_plus_b_due(u64::MAX, u64::MAX), "saturates");
    }

    /// MON11-4: a C1 method's drain that the 8:1 weight leaves far below the
    /// 20 000 bar is promoted to C2 only under `CRATONVM_JIT_TIER4_I_PLUS_B`
    /// (default ON; `=0` is the off arm).
    #[test]
    fn a_short_loop_c1_drain_is_promoted_only_under_the_switch() {
        if !crate::entry_counter::callee_promotion_enabled() {
            return;
        }
        let b: u64 = 715 * 20;
        let count = 715 + (b >> 3);
        let run = |on: bool| {
            let mgr = TieredCompilationManager::with_default_policy();
            let key = test_key();
            mgr.note_published_outside_manager(&key, CompilationTier::C1);
            if mgr.core.methods.lock()[&key].current_tier != CompilationTier::C1 {
                return None;
            }
            let edits: &[(&str, Option<&str>)] = if on {
                &[("CRATONVM_JIT_TIER4_I_PLUS_B", Some("1"))]
            } else {
                &[("CRATONVM_JIT_TIER4_I_PLUS_B", Some("0"))]
            };
            Some(cratonvm_types::flags::with_thread_overrides(edits, || {
                mgr.on_observed_invocations_with_back_edges(&key, count, b)
                    .recommended
            }))
        };
        let (Some(off), Some(on)) = (run(false), run(true)) else {
            return;
        };
        assert_eq!(off, None, "2 502 weighted is far below the 20 000 bar");
        assert_eq!(on, Some(CompilationTier::C2));
    }

    /// HANDLER8-2: the drain's raw back-edge word is kept for the enqueue
    /// line, also on a report that says nothing new about the count.
    #[test]
    fn the_drained_back_edge_word_is_recorded_for_the_enqueue_line() {
        let mgr = TieredCompilationManager::with_default_policy();
        let key = test_key();
        let _ = mgr.on_observed_invocations_with_back_edges(&key, 5, 77);
        assert_eq!(
            mgr.core.methods.lock()[&key].last_drained_back_edges,
            Some(77)
        );
        let _ = mgr.on_observed_invocations_with_back_edges(&key, 5, 90);
        assert_eq!(
            mgr.core.methods.lock()[&key].last_drained_back_edges,
            Some(90)
        );
        let _ = mgr.on_observed_invocations(&key, 6);
        assert_eq!(
            mgr.core.methods.lock()[&key].last_drained_back_edges,
            Some(90),
            "a report without the word keeps the last one"
        );
    }

    /// P1: an OPTIMIZING body published outside the manager settles the method
    /// instead of earning a C2 recompile of itself, and a later single-pass
    /// note cannot lower it.
    #[test]
    fn an_optimizing_body_published_outside_the_manager_settles_it() {
        if !crate::entry_counter::callee_promotion_enabled() {
            return;
        }
        let mgr = TieredCompilationManager::with_default_policy();
        let key = test_key2();
        mgr.note_published_outside_manager(&key, CompilationTier::C2);
        mgr.note_published_outside_manager(&key, CompilationTier::C1);
        assert_eq!(mgr.current_tier(&key), CompilationTier::C2);
        let verdict = mgr.on_observed_invocations(&key, 50_000);
        assert_eq!(verdict.recommended, None);
        assert_ne!(verdict.settled_generation, 0, "a C2 method is settled");
        // A withdrawal still demotes it.
        mgr.note_body_withdrawn(&key);
        assert_eq!(mgr.current_tier(&key), CompilationTier::Interpreter);
    }

    /// **RT-8 soak, defect 3.** A method at C2 is SETTLED, and its call sites
    /// hold that stamp and skip this manager until the generation moves. A
    /// withdrawal that demoted the state without moving the generation was
    /// therefore never read: the method stayed interpreted for good. Remove
    /// the bump from `note_body_withdrawn` and the second assertion fails.
    #[test]
    fn a_withdrawal_expires_the_settled_stamp_so_the_demotion_is_read() {
        let mgr = TieredCompilationManager::with_default_policy();
        let key = test_key();
        mgr.on_method_invocation(&key);
        mgr.compilation_complete(&key, CompilationTier::C2, 50);
        let stamp = mgr
            .on_method_invocation_settling(&key, 0)
            .settled_generation;
        assert!(
            mgr.tiering_settled(stamp),
            "a method at C2 is settled; the precondition of this test"
        );
        mgr.note_body_withdrawn(&key);
        assert!(
            !mgr.tiering_settled(stamp),
            "after a withdrawal the call site must ask again, or it never \
             learns the method was demoted"
        );
    }

    /// r11 wave 3 (lane tier): a trap expires the settled stamps only when it
    /// unsettles its own method. Every trap used to bump the generation, so a
    /// trap storm sent every settled method in the process back to the
    /// `methods` mutex once per trap.
    #[test]
    fn a_trap_expires_the_settled_stamps_only_when_it_unsettles_its_method() {
        let mgr = TieredCompilationManager::with_default_policy();
        let settled = test_key();
        mgr.on_method_invocation(&settled);
        mgr.compilation_complete(&settled, CompilationTier::C2, 50);
        let stamp = mgr
            .on_method_invocation_settling(&settled, 0)
            .settled_generation;
        assert!(
            mgr.tiering_settled(stamp),
            "a method at C2 is settled; the precondition of this test"
        );

        // Traps on another, interpreted method change nothing this stamp
        // depends on.
        let other = MethodKey::new("com/example/Storm", "run", "()V");
        mgr.on_method_invocation(&other);
        for _ in 0..3 {
            mgr.on_deoptimization(
                &other,
                DeoptReason::ClassCheck,
                7,
                DeoptAction::RecompileAndReinterpret,
            );
        }
        assert!(
            mgr.tiering_settled(stamp),
            "a trap that unsettles no method expires no stamp"
        );

        // Evicting the settled method's own body demotes it, so its call
        // sites must ask again.
        mgr.on_deoptimization(
            &settled,
            DeoptReason::ClassCheck,
            7,
            DeoptAction::RecompileAndReinterpret,
        );
        assert!(!mgr.tiering_settled(stamp), "the demotion is read");
    }

    /// The pressure note expires stamps on the falling edge only: every drain
    /// calls it, so a bump on every call at the cap would unsettle the whole
    /// process each stride, and a bump on nothing would leave the methods
    /// deferred at the cap waiting for good.
    #[test]
    fn only_the_cap_clearing_expires_the_settled_stamps() {
        let mgr = TieredCompilationManager::with_default_policy();
        let key = test_key();
        mgr.on_method_invocation(&key);
        mgr.compilation_complete(&key, CompilationTier::C2, 50);
        let stamp = mgr
            .on_method_invocation_settling(&key, 0)
            .settled_generation;

        assert!(
            !mgr.note_code_cache_pressure(false),
            "no edge: never at the cap"
        );
        assert!(
            !mgr.note_code_cache_pressure(true),
            "rising edge: nothing to release"
        );
        assert!(!mgr.note_code_cache_pressure(true), "still at the cap");
        assert!(
            mgr.tiering_settled(stamp),
            "none of those may expire a stamp"
        );

        assert!(mgr.note_code_cache_pressure(false), "the falling edge");
        assert!(!mgr.tiering_settled(stamp), "and it expires the stamps");
        assert!(!mgr.note_code_cache_pressure(false), "once");
    }

    /// A key the manager has no state for — an eagerly compiled first call
    /// never reaches `on_method_invocation` — must be ignored rather than
    /// create an entry. A sweeper runs over the whole cache, so it will hand
    /// this function such keys routinely.
    #[test]
    fn withdrawing_a_body_for_an_unknown_method_does_nothing() {
        let mgr = TieredCompilationManager::with_default_policy();
        let key = test_key();
        mgr.note_body_withdrawn(&key);
        assert!(
            mgr.core.methods.lock().get(&key).is_none(),
            "an unknown key must not be inserted; the map is walked elsewhere"
        );
    }

    // ── Deoptimization and trap accounting ───────────────────────────────

    #[test]
    fn deoptimization_drops_to_interpreter() {
        let mgr = TieredCompilationManager::with_default_policy();
        let key = test_key();
        mgr.on_method_invocation(&key);
        mgr.compilation_complete(&key, CompilationTier::C2, 50);
        assert_eq!(mgr.current_tier(&key), CompilationTier::C2);

        mgr.on_deoptimization(
            &key,
            DeoptReason::ClassCheck,
            7,
            DeoptAction::RecompileAndReinterpret,
        );
        assert_eq!(mgr.current_tier(&key), CompilationTier::Interpreter);
    }

    /// The same speculation failing again and again is a property of the code:
    /// `PER_BCI_TRAP_LIMIT` counted traps at one site end the method's C2 career.
    #[test]
    fn repeated_traps_at_one_bci_trigger_c2_bailout() {
        let mgr = TieredCompilationManager::with_default_policy();
        let key = test_key();
        mgr.on_method_invocation(&key);
        for i in 0..PER_BCI_TRAP_LIMIT {
            assert!(
                !mgr.core.methods.lock()[&key].c2_bailout,
                "trap {i} is still below the per-bci limit"
            );
            mgr.compilation_complete(&key, CompilationTier::C2, 10);
            mgr.on_deoptimization(
                &key,
                DeoptReason::NullCheck,
                12,
                DeoptAction::RecompileAndReinterpret,
            );
        }
        assert!(mgr.core.methods.lock()[&key].c2_bailout);
    }

    /// Traps spread over different sites are charged to the method, which has
    /// a larger allowance than any one site.
    #[test]
    fn traps_across_sites_use_the_per_method_cutoff() {
        let mgr = TieredCompilationManager::with_default_policy();
        let key = test_key();
        mgr.on_method_invocation(&key);
        for bci in 0..PER_METHOD_TRAP_CUTOFF - 1 {
            mgr.on_deoptimization(
                &key,
                DeoptReason::BoundsCheck,
                bci,
                DeoptAction::RecompileAndReinterpret,
            );
        }
        assert!(
            !mgr.core.methods.lock()[&key].c2_bailout,
            "one trap per site, and still below the per-method cutoff"
        );
        mgr.on_deoptimization(
            &key,
            DeoptReason::BoundsCheck,
            9_999,
            DeoptAction::RecompileAndReinterpret,
        );
        assert!(mgr.core.methods.lock()[&key].c2_bailout);
    }

    /// Soft deopts are not charged, and — since nothing charges them — nothing
    /// throws their body away either. A soft OSR exit, a debugger transfer and
    /// a pending exception's handler hand-off all keep the compiled body and
    /// leave the tier where it was.
    #[test]
    fn soft_deopts_do_not_set_c2_bailout() {
        let mgr = TieredCompilationManager::with_default_policy();
        let key = test_key();
        mgr.on_method_invocation(&key);
        mgr.compilation_complete(&key, CompilationTier::C2, 10);
        for _ in 0..50 {
            mgr.on_deoptimization(&key, DeoptReason::OsrExit, 30, DeoptAction::Reinterpret);
        }
        {
            let methods = mgr.core.methods.lock();
            let state = &methods[&key];
            assert!(!state.c2_bailout, "soft OSR exits must not ban C2");
            assert_eq!(state.deopt_count, 0);
            assert_eq!(state.current_tier, CompilationTier::C2, "the body was kept");
        }
        for _ in 0..50 {
            mgr.on_deoptimization(
                &key,
                DeoptReason::TransferToInterpreter,
                30,
                DeoptAction::RecompileAndReinterpret,
            );
            mgr.on_deoptimization(
                &key,
                DeoptReason::PendingException,
                30,
                DeoptAction::MakeNotEntrant,
            );
        }
        {
            let methods = mgr.core.methods.lock();
            let state = &methods[&key];
            assert!(!state.c2_bailout);
            assert_eq!(state.deopt_count, 0);
            assert_eq!(
                state.current_tier,
                CompilationTier::C2,
                "a reason nothing charges must not evict the body either: \
                 evicting without charging is the unbounded recompile thrash"
            );
        }
        assert_eq!(
            mgr.stats().soft_deoptimizations.load(Ordering::Relaxed),
            150
        );
        assert!(
            mgr.request_osr(&key, 30).is_some(),
            "OSR stays available: nothing here was a counted trap"
        );
    }

    /// The two trap predicates agree by construction: a reason nothing charges
    /// is a reason nothing evicts. The reverse used to hold for exactly one
    /// pair (a soft `OsrExit`), which is what let a hot `try`/`catch` evict and
    /// recompile forever with nothing charged and nothing backing off.
    #[test]
    fn an_uncharged_reason_never_evicts_the_body() {
        for action in [
            DeoptAction::Reinterpret,
            DeoptAction::RecompileAndReinterpret,
            DeoptAction::MakeNotEntrant,
            DeoptAction::MakeNotCompilable,
        ] {
            for reason in [
                DeoptReason::PendingException,
                DeoptReason::TransferToInterpreter,
            ] {
                assert!(!deopt_reason_is_charged(reason));
                assert!(!deopt_is_counted_trap(reason, action));
                assert!(
                    !deopt_evicts_method_body(reason, action),
                    "{reason:?}/{action:?} may not throw a body away that nothing charges for"
                );
            }
        }
        // The soft-OSR-exit exemption is unchanged, and a charged reason still
        // evicts so the next call recompiles against a profile that saw it.
        assert!(!deopt_evicts_method_body(
            DeoptReason::OsrExit,
            DeoptAction::Reinterpret
        ));
        assert!(deopt_evicts_method_body(
            DeoptReason::OsrExit,
            DeoptAction::MakeNotCompilable
        ));
        assert!(deopt_evicts_method_body(
            DeoptReason::UncommonTrap,
            DeoptAction::Reinterpret
        ));
    }

    /// Trap counts decay, so a burst early in a long run does not ban a method
    /// for the rest of it.
    #[test]
    fn trap_counts_decay_with_invocations() {
        let mgr = TieredCompilationManager::with_default_policy();
        let key = test_key();
        mgr.on_method_invocation(&key);
        for _ in 0..PER_BCI_TRAP_LIMIT - 1 {
            mgr.on_deoptimization(
                &key,
                DeoptReason::ClassCheck,
                5,
                DeoptAction::RecompileAndReinterpret,
            );
        }
        // The method keeps running long enough for its counts to decay...
        let seen = mgr
            .method_states()
            .into_iter()
            .find(|(k, _, _)| *k == key)
            .map(|(_, _, count)| count)
            .expect("state for key");
        let _ = mgr.on_method_invocation_observed(&key, seen + TRAP_DECAY_INVOCATIONS);
        // ...so one more trap at the same site is not the one that reaches the
        // limit.
        mgr.on_deoptimization(
            &key,
            DeoptReason::ClassCheck,
            5,
            DeoptAction::RecompileAndReinterpret,
        );
        let methods = mgr.core.methods.lock();
        let state = &methods[&key];
        assert!(
            !state.c2_bailout,
            "decayed counts must not reach the per-bci limit"
        );
        assert_eq!(
            state.trap_counts[&(DeoptReason::ClassCheck, 5)],
            (PER_BCI_TRAP_LIMIT - 1) / 2 + 1
        );
    }

    #[test]
    fn deopt_resets_queued_state() {
        let mgr = TieredCompilationManager::with_default_policy();
        let key = test_key();
        mgr.enqueue_compilation(CompilationTask {
            method_key: key.clone(),
            target_tier: CompilationTier::C2,
            priority: CompilationPriority::High,
            enqueue_time_ms: 0,
            osr_bci: None,
        });
        mgr.on_deoptimization(
            &key,
            DeoptReason::ReceiverTypeChanged,
            0,
            DeoptAction::RecompileAndReinterpret,
        );

        let methods = mgr.core.methods.lock();
        let state = &methods[&key];
        assert!(!state.queued_for_compilation);
        assert!(state.queued_tier.is_none());
        drop(methods);
        assert!(
            mgr.queue_empty(),
            "the queued request is dropped, not left behind"
        );
    }

    // ── Tier-4 compile-time guard (jit-inlining-and-ir-calls) ────────────

    /// A C2 compile that stays inside `MAX_C2_COMPILE_TIME_MS` must leave the
    /// method eligible for C2. Repeated compiles that exceed it demote the
    /// method to C1 for the rest of the process, through the SAME `c2_bailout`
    /// flag the trap limits use (so every existing degradation path honours it
    /// with no additional wiring). One overrun is not enough.
    #[test]
    fn slow_c2_compile_demotes_to_c1() {
        let mgr = TieredCompilationManager::with_default_policy();
        let key = test_key();
        mgr.on_method_invocation(&key);

        // Inside the budget → still a C2 method.
        mgr.compilation_complete(&key, CompilationTier::C2, MAX_C2_COMPILE_TIME_MS);
        assert!(
            !mgr.core.methods.lock()[&key].c2_bailout,
            "a C2 compile at exactly the budget must not demote"
        );

        // One overrun is not evidence.
        mgr.compilation_complete(&key, CompilationTier::C2, MAX_C2_COMPILE_TIME_MS + 1);
        assert!(
            !mgr.core.methods.lock()[&key].c2_bailout,
            "a single over-budget compile must not demote"
        );

        // Repeated overruns are, and the statistic counts the demotion
        // alongside the trap-driven ones.
        let before = mgr.stats().c2_bailouts.load(Ordering::Relaxed);
        for _ in 1..C2_BUDGET_OVERRUNS_BEFORE_DEMOTION {
            mgr.compilation_complete(&key, CompilationTier::C2, MAX_C2_COMPILE_TIME_MS + 1);
        }
        assert!(
            mgr.core.methods.lock()[&key].c2_bailout,
            "repeated C2 compiles over MAX_C2_COMPILE_TIME_MS must demote the method to C1"
        );
        assert_eq!(
            mgr.stats().c2_bailouts.load(Ordering::Relaxed),
            before + 1,
            "the compile-time demotion must be counted as a c2 bailout"
        );
    }

    /// zt t18/pl8: the perf-data `sun.ci.*` statistics follow HotSpot's
    /// `CompileBroker::collect_statistics`: every attempt's time in the total,
    /// the per-kind times of the published compiles only, a bailout for a
    /// compile that ran and published nothing (an epoch-moved install refusal
    /// included), none for a permanent decline.
    #[test]
    fn zt18pl8_bailouts_and_per_kind_times_follow_collect_statistics() {
        let mgr = TieredCompilationManager::with_default_policy();
        let key = MethodKey::new("craton/test/Pl8CompileStats", "m", "()V");
        mgr.core
            .complete_task(&key, CompilationTier::C1, 2, true, false, false);
        mgr.core
            .complete_task(&key, CompilationTier::C2, 3, true, true, false);
        mgr.core
            .complete_task(&key, CompilationTier::C2, 5, false, false, false);
        mgr.core
            .complete_task(&key, CompilationTier::C1, 7, false, false, true);
        let stats = mgr.stats();
        let load = |cell: &AtomicU64| cell.load(Ordering::Relaxed);
        assert_eq!(load(&stats.standard_compile_time_ns), 2_000_000);
        assert_eq!(load(&stats.osr_compile_time_ns), 3_000_000);
        assert_eq!(load(&stats.total_compile_time_ns), 17_000_000);
        assert_eq!(load(&stats.total_bailouts), 1, "the decline is no bailout");
        let failed = stats.last_failed.lock().clone().expect("a bailout");
        assert_eq!(
            (&*failed.class_name, &*failed.method_name, failed.osr),
            ("craton/test/Pl8CompileStats", "m", false)
        );
        mgr.core.finish(
            &key,
            Completion {
                tier: CompilationTier::C2,
                osr_bci: Some(4),
                osr: true,
                compile_time_ms: 0,
                compile_time_ns: 400,
                budget_ms: 0,
                success: false,
                declined_permanently: false,
                epoch_moved: true,
                waiting_on_class: None,
                cap_refused: false,
                ticket: 0,
            },
        );
        assert_eq!(load(&stats.total_bailouts), 2, "a refused install bails");
        assert!(stats.last_failed.lock().as_ref().is_some_and(|f| f.osr));
        assert_eq!(load(&stats.osr_compile_time_ns), 3_000_000);
        assert_eq!(load(&stats.total_compile_time_ns), 17_000_400);
    }

    /// Only a successful method-entry compile is charged. A slow failure is
    /// already bounded by `tier_fail_count`, and an OSR compile builds a
    /// different artifact.
    #[test]
    fn failed_and_osr_c2_compiles_are_not_charged_to_the_budget() {
        let mgr = TieredCompilationManager::with_default_policy();
        let key = test_key();
        mgr.on_method_invocation(&key);
        for _ in 0..(C2_BUDGET_OVERRUNS_BEFORE_DEMOTION * 3) {
            mgr.core.complete_task(
                &key,
                CompilationTier::C2,
                MAX_C2_COMPILE_TIME_MS * 10,
                false,
                false,
                false,
            );
            mgr.core.complete_task(
                &key,
                CompilationTier::C2,
                MAX_C2_COMPILE_TIME_MS * 10,
                true,
                true,
                false,
            );
        }
        let methods = mgr.core.methods.lock();
        assert_eq!(methods[&key].c2_budget_overruns, 0);
        assert!(!methods[&key].c2_bailout);
    }

    /// The budget is charged `Completion::budget_ms` — compile-thread CPU time
    /// from the worker — not the wall-clock time the statistics record. A
    /// descheduled compile thread is not a slow compile.
    #[test]
    fn the_c2_budget_is_charged_cpu_time_not_wall_time() {
        let mgr = TieredCompilationManager::with_default_policy();
        let key = test_key();
        mgr.on_method_invocation(&key);
        for _ in 0..(C2_BUDGET_OVERRUNS_BEFORE_DEMOTION * 2) {
            mgr.core.finish(
                &key,
                Completion {
                    tier: CompilationTier::C2,
                    osr_bci: None,
                    osr: false,
                    compile_time_ms: MAX_C2_COMPILE_TIME_MS * 20,
                    compile_time_ns: 0,
                    budget_ms: 1,
                    success: true,
                    declined_permanently: false,
                    epoch_moved: false,
                    waiting_on_class: None,
                    cap_refused: false,
                    ticket: 0,
                },
            );
        }
        assert!(!mgr.core.methods.lock()[&key].c2_bailout);
    }

    /// The supersede gate arms only for a C2-threshold value the policy
    /// actually applies. An empty or unparseable value is ignored by
    /// `with_overrides`, and must be ignored by the gate too -- or it arms at
    /// the default 20 000 and closes the supersede door for nearly everything.
    #[test]
    fn the_supersede_gate_ignores_a_c2_threshold_the_policy_ignores() {
        assert!(!c2_threshold_knob_applies(None));
        assert!(!c2_threshold_knob_applies(Some("")));
        assert!(!c2_threshold_knob_applies(Some("   ")));
        assert!(!c2_threshold_knob_applies(Some("20k")));
        assert!(!c2_threshold_knob_applies(Some("-5")));
        assert!(c2_threshold_knob_applies(Some("20000")));
        assert!(c2_threshold_knob_applies(Some(" 750 ")));
        // And the policy side agrees on the same inputs.
        let get = |v: &'static str| {
            move |name: &str| (name == "CRATONVM_TIER_C2_THRESHOLD").then(|| v.to_string())
        };
        assert_eq!(
            CompilationPolicy::with_overrides(get("")).c2_threshold,
            CompilationPolicy::default().c2_threshold
        );
        assert_eq!(
            CompilationPolicy::with_overrides(get(" 750 ")).c2_threshold,
            750
        );
    }

    /// Queue latency is charged per lane, from the request's own stamp, and a
    /// stamp from another clock (or no stamp) counts the dispatch but no wait.
    #[test]
    fn queue_wait_is_charged_per_lane_from_the_enqueue_stamp() {
        let mgr = TieredCompilationManager::with_default_policy();
        let task = |stamp: u64| CompilationTask {
            method_key: test_key(),
            target_tier: CompilationTier::C1,
            priority: CompilationPriority::Normal,
            enqueue_time_ms: stamp,
            osr_bci: None,
        };
        mgr.core.note_queue_wait(C1_LANE, &task(100), 130);
        mgr.core.note_queue_wait(C1_LANE, &task(100), 110);
        // Another clock's stamp (ahead of "now") and the unset sentinel.
        mgr.core.note_queue_wait(C2_LANE, &task(5_000), 130);
        mgr.core.note_queue_wait(C2_LANE, &task(0), 130);
        let s = mgr.stats();
        assert_eq!(s.dispatched[C1_LANE].load(Ordering::Relaxed), 2);
        assert_eq!(s.queue_wait_ms_total[C1_LANE].load(Ordering::Relaxed), 40);
        assert_eq!(s.queue_wait_ms_max[C1_LANE].load(Ordering::Relaxed), 30);
        assert_eq!(s.dispatched[C2_LANE].load(Ordering::Relaxed), 2);
        assert_eq!(s.queue_wait_ms_total[C2_LANE].load(Ordering::Relaxed), 0);
        assert_eq!(s.queue_wait_ms_max[C2_LANE].load(Ordering::Relaxed), 0);
    }

    /// An unpublished compile during which the install epoch moved is the
    /// flush barrier refusing a stale body, not a compiler failure, and must
    /// not spend the method's retry budget -- `retire_stale`'s rule for a
    /// request dropped before dispatch, applied to one caught mid-compile.
    /// Charged, it took `MAX_TIER_FAIL_RETRIES` epoch moves during warmup to
    /// settle a healthy method for the life of the process.
    #[test]
    fn a_compile_overtaken_by_an_epoch_move_spends_no_retry() {
        let mgr = epoch_driven_manager(&Arc::new(AtomicU64::new(1)));
        let key = MethodKey::new("craton/test/EpochMovedMidCompile", "m", "()V");
        for _ in 0..(MAX_TIER_FAIL_RETRIES * 2) {
            let task = mgr
                .request_osr(&key, 3)
                .expect("an uncharged outcome leaves the method admissible");
            assert_task_eq(mgr.next_fresh_task(), Some(task));
            mgr.core.finish(
                &key,
                Completion {
                    tier: CompilationTier::C2,
                    osr_bci: Some(3),
                    osr: true,
                    compile_time_ms: 1,
                    compile_time_ns: 0,
                    budget_ms: 1,
                    success: false,
                    declined_permanently: false,
                    epoch_moved: true,
                    waiting_on_class: None,
                    cap_refused: false,
                    ticket: 0,
                },
            );
            let methods = mgr.core.methods.lock();
            assert_eq!(methods[&key].tier_fail_count, 0);
            assert!(!methods[&key].osr_in_flight, "OSR slot released");
        }
        // The same outcome WITHOUT the epoch move is still a failure.
        let _ = mgr.request_osr(&key, 3).expect("admissible");
        let _ = mgr.next_fresh_task();
        mgr.core.finish(
            &key,
            Completion {
                tier: CompilationTier::C2,
                osr_bci: Some(3),
                osr: true,
                compile_time_ms: 1,
                compile_time_ns: 0,
                budget_ms: 1,
                success: false,
                declined_permanently: false,
                epoch_moved: false,
                waiting_on_class: None,
                cap_refused: false,
                ticket: 0,
            },
        );
        // r10: this charge lands on `osr_fail_count`, not `tier_fail_count`. The
        // completion being charged is an OSR one (`request_osr`, `osr: true`), and
        // OSR failures no longer spend the method's ordinary C1->C2 retries -- an
        // OSR compile that cannot be built is not evidence that a whole-method
        // recompile cannot be either. Asserting BOTH counters here is the point:
        // it pins the split rather than merely following it, and it still holds
        // this test's own claim, that the epoch-moved outcomes above spent
        // nothing while this genuine failure spends exactly one.
        let methods = mgr.core.methods.lock();
        assert_eq!(
            methods[&key].osr_fail_count, 1,
            "the OSR failure is charged"
        );
        assert_eq!(
            methods[&key].tier_fail_count, 0,
            "and it does not spend a whole-method retry"
        );
    }

    /// JIT round 22 wave 11, lane runtime (`Completion::cap_refused`): a
    /// method-entry compile the cap gate refused is deferred at the cap and
    /// never charged, even when the cap no longer reads full by the time
    /// `finish` asks (a drain released room in between, the race that settled
    /// `code_cache_sweep_phases`'s phase-3 methods). No cap is configured in
    /// this test process, so `jit_code_cache_at_capacity` answers `false`
    /// throughout: only the witness can defer.
    #[test]
    fn r22w11_a_witnessed_cap_refusal_is_deferred_not_charged() {
        let mgr = epoch_driven_manager(&Arc::new(AtomicU64::new(1)));
        let key = MethodKey::new("craton/test/CapRefusedMidCompile", "m", "()V");
        for _ in 0..(MAX_TIER_FAIL_RETRIES * 2) {
            {
                let mut methods = mgr.core.methods.lock();
                let state = methods
                    .entry(key.clone())
                    .or_insert_with(|| MethodState::new(key.clone()));
                state.queued_for_compilation = true;
                state.queued_tier = Some(CompilationTier::C1);
            }
            mgr.core.finish(
                &key,
                Completion {
                    tier: CompilationTier::C1,
                    osr_bci: None,
                    osr: false,
                    compile_time_ms: 1,
                    budget_ms: 1,
                    success: false,
                    declined_permanently: false,
                    epoch_moved: false,
                    waiting_on_class: None,
                    cap_refused: true,
                    ticket: 0,
                    compile_time_ns: 0,
                },
            );
            let methods = mgr.core.methods.lock();
            assert_eq!(methods[&key].tier_fail_count, 0, "a cap refusal is never charged");
            assert!(methods[&key].deferred_at_cap, "it is deferred at the cap");
        }
    }

    /// JIT round 22 wave 12, lane review (`CRATONVM_JIT_CAP_REFUSAL_WITNESS_PER_THREAD`):
    /// the cap-refused witness is per thread. A refusal noted on another
    /// thread (another worker, a mutator's eager door, another VM) leaves this
    /// thread's bit clear, and the bit does not disturb the waiting-on-class
    /// flag that shares its cell.
    #[test]
    fn r22w12_the_cap_refusal_witness_is_this_threads_own() {
        let _ = take_compile_cap_refused();
        let _ = take_compile_waiting_on_class();
        std::thread::spawn(note_compile_cap_refused_on_this_thread)
            .join()
            .expect("the other thread");
        assert!(!take_compile_cap_refused(), "another thread's refusal is not ours");
        note_compile_cap_refused_on_this_thread();
        note_compile_waiting_on_class();
        assert!(take_compile_cap_refused());
        assert!(!take_compile_cap_refused(), "taken once");
        assert!(take_compile_waiting_on_class(), "the other bit survives");
        assert!(!take_compile_waiting_on_class());
        const KEY: &str = "CRATONVM_JIT_CAP_REFUSAL_WITNESS_PER_THREAD";
        cratonvm_types::flags::with_thread_overrides(&[(KEY, None)], || {
            assert!(cap_refusal_witness_per_thread_enabled());
        });
        cratonvm_types::flags::with_thread_overrides(&[(KEY, Some("0"))], || {
            assert!(!cap_refusal_witness_per_thread_enabled());
        });
    }

    /// The compile-time guard is C2-only: a slow C1 compile is not a reason to
    /// refuse the optimizing tier (C1 and C2 use different backends, and the
    /// single-pass backend is the fallback the bailout demotes *to*).
    #[test]
    fn slow_c1_compile_does_not_demote() {
        let mgr = TieredCompilationManager::with_default_policy();
        let key = test_key();
        mgr.on_method_invocation(&key);
        mgr.compilation_complete(&key, CompilationTier::C1, MAX_C2_COMPILE_TIME_MS * 10);
        assert!(!mgr.core.methods.lock()[&key].c2_bailout);
    }

    #[test]
    fn the_thread_cpu_clock_is_monotonic_where_it_is_wired() {
        // Platforms without a per-thread clock return `None`, and the budget
        // falls back to wall time; nothing to assert there.
        let Some(before) = current_thread_cpu_time() else {
            return;
        };
        let mut x = 0u64;
        for i in 0..2_000_000u64 {
            x = x.wrapping_mul(31).wrapping_add(i);
        }
        std::hint::black_box(x);
        let after = current_thread_cpu_time().expect("a clock that answered once answers again");
        assert!(after >= before);
    }

    #[test]
    fn zs9rs2_c2_standby_switch_parses_opt_in_and_depth() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| -> Option<String> {
                pairs
                    .iter()
                    .find(|(k, _)| *k == name)
                    .map(|(_, v)| v.to_string())
            }
        };
        assert_eq!(c2_standby_depth_with(env(&[])), 0, "unset is off");
        assert_eq!(
            c2_standby_depth_with(env(&[("CRATONVM_TIER_C2_STANDBY", "0")])),
            0
        );
        assert_eq!(
            c2_standby_depth_with(env(&[("CRATONVM_TIER_C2_STANDBY", "off")])),
            0
        );
        assert_eq!(
            c2_standby_depth_with(env(&[("CRATONVM_TIER_C2_STANDBY", "")])),
            0
        );
        assert_eq!(
            c2_standby_depth_with(env(&[("CRATONVM_TIER_C2_STANDBY", "1")])),
            4
        );
        assert_eq!(
            c2_standby_depth_with(env(&[
                ("CRATONVM_TIER_C2_STANDBY", "1"),
                ("CRATONVM_TIER_C2_STANDBY_DEPTH", "9")
            ])),
            9
        );
        assert_eq!(
            c2_standby_depth_with(env(&[
                ("CRATONVM_TIER_C2_STANDBY", "on"),
                ("CRATONVM_TIER_C2_STANDBY_DEPTH", "0")
            ])),
            1,
            "depth floors at 1"
        );
        // The depth alone does not turn it on.
        assert_eq!(
            c2_standby_depth_with(env(&[("CRATONVM_TIER_C2_STANDBY_DEPTH", "2")])),
            0
        );
    }

    /// zs s9/rs2: a standby worker stays parked below the depth, is promoted
    /// by a backlog, and a parked one leaves on shutdown.
    #[test]
    fn zs9rs2_standby_worker_parks_until_a_backlog_or_shutdown() {
        let lane = Arc::new(CompileLane::new());
        let shutdown = Arc::new(AtomicBool::new(false));
        lane.standby_depth.store(2, Ordering::Relaxed);
        let spawn = |k: usize| {
            let (lane, shutdown) = (Arc::clone(&lane), Arc::clone(&shutdown));
            lane.standby_parked.fetch_add(1, Ordering::AcqRel);
            std::thread::spawn(move || lane.park_standby(&shutdown, k, 0))
        };
        let w1 = spawn(1);
        let w2 = spawn(2);
        // Below the depth: nobody is promoted.
        lane.maybe_promote_standby(1);
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(!w1.is_finished() && !w2.is_finished());
        assert_eq!(*lane.standby_promoted.lock(), 0);
        // A backlog promotes exactly one (worker 1).
        lane.maybe_promote_standby(2);
        assert!(w1.join().unwrap(), "worker 1 promoted");
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(!w2.is_finished(), "worker 2 still parked");
        assert_eq!(lane.standby_parked.load(Ordering::Acquire), 1);
        // Shutdown releases the parked one with false.
        shutdown.store(true, Ordering::Release);
        drop(lane.standby_promoted.lock());
        lane.standby_wake.notify_all();
        assert!(!w2.join().unwrap(), "worker 2 left on shutdown");
        // With no parked worker a backlog promotes nothing.
        lane.standby_parked.store(0, Ordering::Release);
        lane.maybe_promote_standby(100);
        assert_eq!(*lane.standby_promoted.lock(), 1);
    }

    /// JIT r23 wave 11 (page `c2-standby-counters-are-process-wide`): the
    /// promotion count lives on the lane that promoted, so one VM's
    /// promotions never show in another VM's line; without an owner the line
    /// reports zeros.
    #[test]
    fn r23w11_standby_promotions_are_counted_per_lane_not_per_process() {
        let a = CompileLane::new();
        let b = CompileLane::new();
        a.standby_depth.store(1, Ordering::Relaxed);
        a.standby_parked.store(2, Ordering::Release);
        a.maybe_promote_standby(1);
        a.maybe_promote_standby(1);
        assert_eq!(a.standby_promoted_total.load(Ordering::Relaxed), 2);
        assert_eq!(b.standby_promoted_total.load(Ordering::Relaxed), 0, "lane b saw none");
        assert_eq!(b.standby_started_total.load(Ordering::Relaxed), 0);
        let line = c2_standby_line(None);
        assert!(line.ends_with("started=0 promoted=0"), "{line}");
    }

    /// zt t2/fp: the scale switch parses as an opt-in word.
    #[test]
    fn zt2fp_c2_standby_scale_parses_opt_in() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| -> Option<String> {
                pairs
                    .iter()
                    .find(|(k, _)| *k == name)
                    .map(|(_, v)| v.to_string())
            }
        };
        assert!(!c2_standby_scale_with(env(&[])), "unset is off");
        assert!(!c2_standby_scale_with(env(&[(
            "CRATONVM_TIER_C2_STANDBY_SCALE",
            "0"
        )])));
        assert!(!c2_standby_scale_with(env(&[(
            "CRATONVM_TIER_C2_STANDBY_SCALE",
            "off"
        )])));
        assert!(!c2_standby_scale_with(env(&[(
            "CRATONVM_TIER_C2_STANDBY_SCALE",
            " "
        )])));
        assert!(c2_standby_scale_with(env(&[(
            "CRATONVM_TIER_C2_STANDBY_SCALE",
            "1"
        )])));
        assert!(c2_standby_scale_with(env(&[(
            "CRATONVM_TIER_C2_STANDBY_SCALE",
            "on"
        )])));
    }

    /// zt t2/fp: under the scale switch the k-th promotion waits for a
    /// backlog of `depth x live workers`; without it every promotion waits
    /// for `depth` alone (the positive control).
    #[test]
    fn zt2fp_scaled_standby_promotion_waits_for_depth_times_live() {
        for scale in [false, true] {
            let lane = CompileLane::new();
            lane.standby_depth.store(2, Ordering::Relaxed);
            lane.standby_scale.store(scale, Ordering::Relaxed);
            lane.standby_parked.store(3, Ordering::Release);
            lane.maybe_promote_standby(2);
            assert_eq!(*lane.standby_promoted.lock(), 1, "first at depth");
            lane.maybe_promote_standby(3);
            let second = *lane.standby_promoted.lock();
            assert_eq!(second, if scale { 1 } else { 2 }, "scale={scale}");
            lane.maybe_promote_standby(4);
            let third = *lane.standby_promoted.lock();
            assert_eq!(third, if scale { 2 } else { 3 }, "scale={scale}");
            lane.maybe_promote_standby(6);
            assert_eq!(*lane.standby_promoted.lock(), 3, "scale={scale}");
            assert_eq!(lane.standby_parked.load(Ordering::Acquire), 0);
        }
    }

    /// zt t2/fp: a worker parked by an earlier compiler start leaves when a
    /// later start bumps the lane's epoch, even with the shutdown flag
    /// cleared again by that start; a worker of the current epoch stays.
    #[test]
    fn zt2fp_a_new_start_releases_an_earlier_starts_parked_worker() {
        let lane = Arc::new(CompileLane::new());
        let shutdown = Arc::new(AtomicBool::new(false));
        let park = |epoch: usize| {
            let (lane, shutdown) = (Arc::clone(&lane), Arc::clone(&shutdown));
            std::thread::spawn(move || lane.park_standby(&shutdown, 1, epoch))
        };
        let old = park(0);
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(!old.is_finished(), "parked in its own epoch");
        {
            let _promoted = lane.standby_promoted.lock();
            lane.standby_epoch.fetch_add(1, Ordering::AcqRel);
        }
        lane.standby_wake.notify_all();
        assert!(!old.join().unwrap(), "the old epoch's worker left");
        let current = park(1);
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(!current.is_finished(), "the current epoch's worker stays");
        lane.standby_parked.store(1, Ordering::Release);
        lane.standby_depth.store(1, Ordering::Relaxed);
        lane.maybe_promote_standby(1);
        assert!(current.join().unwrap(), "promoted");
    }

    #[test]
    fn compiler_thread_counts_follow_log2_cpus_and_clamp_overrides() {
        let unset = |_: &str| -> Option<String> { None };
        assert_eq!(compiler_thread_counts_with(unset, 1), (1, 1));
        assert_eq!(compiler_thread_counts_with(unset, 2), (1, 1));
        assert_eq!(compiler_thread_counts_with(unset, 8), (1, 3));
        assert_eq!(compiler_thread_counts_with(unset, 64), (1, 6));
        let set = |name: &str| -> Option<String> {
            match name {
                "CRATONVM_TIER_C1_THREADS" => Some("0".to_string()),
                "CRATONVM_TIER_C2_THREADS" => Some("1000".to_string()),
                _ => None,
            }
        };
        assert_eq!(
            compiler_thread_counts_with(set, 8),
            (1, 64),
            "0 clamps up to one worker, and an absurd count clamps down"
        );
    }

    // ── C2 bailout stays at C1 ───────────────────────────────────────────

    #[test]
    fn c2_bailout_stays_at_c1() {
        let policy = CompilationPolicy {
            c1_threshold: 5,
            c2_threshold: 20,
            c2_min_invocations: 10,
            ..CompilationPolicy::default()
        };
        let mgr = TieredCompilationManager::new(policy);
        let key = test_key();

        // Trigger C1.
        for _ in 0..5 {
            mgr.on_method_invocation(&key);
        }
        mgr.dequeue_compilation();
        mgr.compilation_complete(&key, CompilationTier::C1, 5);

        // Force bailout.
        mgr.on_c2_bailout(&key);

        // Keep invoking past C2 threshold — should NOT trigger C2.
        for _ in 5..30 {
            let tier = mgr.on_method_invocation(&key);
            assert_ne!(tier, Some(CompilationTier::C2));
        }
    }

    // ── Queue size ───────────────────────────────────────────────────────

    #[test]
    fn queue_size_tracking() {
        let mgr = TieredCompilationManager::with_default_policy();
        assert_eq!(mgr.queue_size(), 0);

        let key = test_key();
        mgr.enqueue_compilation(CompilationTask {
            method_key: key.clone(),
            target_tier: CompilationTier::C1,
            priority: CompilationPriority::Normal,
            enqueue_time_ms: 0,
            osr_bci: None,
        });
        assert_eq!(mgr.queue_size(), 1);

        mgr.dequeue_compilation();
        assert_eq!(mgr.queue_size(), 0);
    }

    // ── Stats counting ───────────────────────────────────────────────────

    #[test]
    fn stats_c1_compilations() {
        let mgr = TieredCompilationManager::with_default_policy();
        let key = test_key();
        mgr.on_method_invocation(&key);
        mgr.compilation_complete(&key, CompilationTier::C1, 10);
        assert_eq!(mgr.stats().c1_compilations.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn stats_c2_compilations() {
        let mgr = TieredCompilationManager::with_default_policy();
        let key = test_key();
        mgr.on_method_invocation(&key);
        mgr.compilation_complete(&key, CompilationTier::C2, 50);
        assert_eq!(mgr.stats().c2_compilations.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn stats_osr_compilations() {
        let mgr = TieredCompilationManager::with_default_policy();
        let key = test_key();
        assert!(mgr.request_osr(&key, 0).is_some());
        assert!(mgr.request_osr(&key, 0).is_none(), "already queued");
        // Queued is not compiled: the stat sits beside c1/c2, which count
        // published bodies, and must mean the same thing.
        assert_eq!(mgr.stats().osr_compilations.load(Ordering::Relaxed), 0);
        // A failed OSR compile is not an OSR compilation either.
        mgr.core
            .complete_task(&key, CompilationTier::C2, 1, false, true, false);
        assert_eq!(mgr.stats().osr_compilations.load(Ordering::Relaxed), 0);
        assert!(
            mgr.request_osr(&key, 0).is_some(),
            "retried after the failure"
        );
        mgr.core
            .complete_task(&key, CompilationTier::C2, 1, true, true, false);
        assert_eq!(mgr.stats().osr_compilations.load(Ordering::Relaxed), 1);
        // And it is not ALSO a method-entry C2 compile.
        assert_eq!(mgr.stats().c2_compilations.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn stats_deoptimizations() {
        let mgr = TieredCompilationManager::with_default_policy();
        let key = test_key();
        mgr.on_method_invocation(&key);
        mgr.compilation_complete(&key, CompilationTier::C2, 10);
        mgr.on_deoptimization(&key, DeoptReason::UncommonTrap, 1, DeoptAction::Reinterpret);
        assert_eq!(mgr.stats().deoptimizations.load(Ordering::Relaxed), 1);
        assert_eq!(mgr.stats().soft_deoptimizations.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn stats_total_compile_time() {
        let mgr = TieredCompilationManager::with_default_policy();
        let key = test_key();
        mgr.on_method_invocation(&key);
        mgr.compilation_complete(&key, CompilationTier::C1, 15);
        mgr.compilation_complete(&key, CompilationTier::C2, 85);
        assert_eq!(
            mgr.stats().total_compile_time_ms.load(Ordering::Relaxed),
            100
        );
    }

    // ── Method states listing ────────────────────────────────────────────

    #[test]
    fn method_states_listing() {
        let mgr = TieredCompilationManager::with_default_policy();
        let k1 = test_key();
        let k2 = test_key2();
        mgr.on_method_invocation(&k1);
        mgr.on_method_invocation(&k2);
        mgr.on_method_invocation(&k2);

        let states = mgr.method_states();
        assert_eq!(states.len(), 2);
    }

    // ── Policy update ────────────────────────────────────────────────────

    #[test]
    fn policy_update() {
        let mgr = TieredCompilationManager::with_default_policy();
        let mut p = mgr.policy();
        assert_eq!(p.c1_threshold, 500);
        p.c1_threshold = 500;
        mgr.set_policy(p);
        assert_eq!(mgr.policy().c1_threshold, 500);
    }

    // ── Empty queue returns None ─────────────────────────────────────────

    #[test]
    fn empty_queue_returns_none() {
        let mgr = TieredCompilationManager::with_default_policy();
        assert!(mgr.dequeue_compilation().is_none());
        assert!(mgr.queue_empty());
    }

    // ── High priority first ──────────────────────────────────────────────

    #[test]
    fn queue_processes_high_priority_first() {
        let mgr = TieredCompilationManager::with_default_policy();
        mgr.enqueue_compilation(CompilationTask {
            method_key: test_key(),
            target_tier: CompilationTier::C1,
            priority: CompilationPriority::Low,
            enqueue_time_ms: 0,
            osr_bci: None,
        });
        mgr.enqueue_compilation(CompilationTask {
            method_key: test_key2(),
            target_tier: CompilationTier::C2,
            priority: CompilationPriority::High,
            enqueue_time_ms: 0,
            osr_bci: None,
        });

        let first = mgr.dequeue_compilation().unwrap();
        assert_eq!(first.method_key, test_key2());
        assert_eq!(first.priority, CompilationPriority::High);
    }

    // ── OSR compilation has bci set ──────────────────────────────────────

    #[test]
    fn osr_compilation_has_bci() {
        let mgr = TieredCompilationManager::with_default_policy();
        let key = test_key();
        let task = mgr.request_osr(&key, 77).unwrap();
        assert_eq!(task.osr_bci, Some(77));
    }

    // ── Tiered disabled ──────────────────────────────────────────────────

    #[test]
    fn no_compilation_when_tiered_disabled() {
        let policy = CompilationPolicy {
            c1_threshold: 1,
            tiered_enabled: false,
            ..CompilationPolicy::default()
        };
        let mgr = TieredCompilationManager::new(policy);
        let key = test_key();
        for _ in 0..100 {
            assert!(mgr.on_method_invocation(&key).is_none());
        }
    }

    // ── Current tier query ───────────────────────────────────────────────

    #[test]
    fn current_tier_returns_correct_value() {
        let mgr = TieredCompilationManager::with_default_policy();
        let key = test_key();
        mgr.on_method_invocation(&key);
        mgr.compilation_complete(&key, CompilationTier::C1WithProfiling, 5);
        assert_eq!(mgr.current_tier(&key), CompilationTier::C1WithProfiling);
    }

    // ── Compilation task priorities ──────────────────────────────────────

    #[test]
    fn compilation_task_priorities() {
        assert_eq!(CompilationPriority::High, CompilationPriority::High);
        assert_ne!(CompilationPriority::High, CompilationPriority::Low);
    }

    // ── Queue total_processed ────────────────────────────────────────────

    #[test]
    fn queue_total_processed_counter() {
        let mgr = TieredCompilationManager::with_default_policy();
        // Two methods: one method holds one in-flight slot, so a second request
        // for the same key would be refused rather than queued.
        mgr.enqueue_compilation(CompilationTask {
            method_key: test_key(),
            target_tier: CompilationTier::C1,
            priority: CompilationPriority::Normal,
            enqueue_time_ms: 0,
            osr_bci: None,
        });
        mgr.enqueue_compilation(CompilationTask {
            method_key: test_key2(),
            target_tier: CompilationTier::C2,
            priority: CompilationPriority::High,
            enqueue_time_ms: 0,
            osr_bci: None,
        });
        mgr.dequeue_compilation();
        mgr.dequeue_compilation();

        assert_eq!(mgr.core.total_processed(), 2);
    }

    // ── Custom policy thresholds ─────────────────────────────────────────

    #[test]
    fn custom_policy_thresholds() {
        let policy = CompilationPolicy {
            c1_threshold: 10,
            c2_threshold: 50,
            osr_threshold: 100,
            tiered_enabled: true,
            c2_min_invocations: 25,
            c1_profiling: false,
        };
        let mgr = TieredCompilationManager::new(policy);
        let key = test_key();

        // Should trigger C1 at invocation 10.
        let mut triggered_at = None;
        for i in 1..=15 {
            if let Some(CompilationTier::C1) = mgr.on_method_invocation(&key) {
                triggered_at = Some(i);
                break;
            }
        }
        assert_eq!(triggered_at, Some(10));
    }

    // ── Compiler active flag ─────────────────────────────────────────────

    #[test]
    fn compiler_active_flag() {
        let mgr = TieredCompilationManager::with_default_policy();
        assert!(!mgr.compiler_active());
        mgr.set_compiler_active(true);
        assert!(mgr.compiler_active());
    }

    // ── Tier ordering ────────────────────────────────────────────────────

    #[test]
    fn tier_ordering() {
        assert!(CompilationTier::Interpreter < CompilationTier::C1);
        assert!(CompilationTier::C1 < CompilationTier::C1WithProfiling);
        assert!(CompilationTier::C1WithProfiling < CompilationTier::FullProfile);
        assert!(CompilationTier::FullProfile < CompilationTier::C2);
    }

    // ── C2 bailout stat incremented ──────────────────────────────────────

    #[test]
    fn c2_bailout_stat() {
        let mgr = TieredCompilationManager::with_default_policy();
        let key = test_key();
        mgr.on_method_invocation(&key);
        mgr.on_c2_bailout(&key);
        assert_eq!(mgr.stats().c2_bailouts.load(Ordering::Relaxed), 1);
    }

    // ── Split in-flight slots: method entry and OSR ──────────────────────

    /// A hot loop in a still-interpreted activation may ask for OSR while
    /// the method's own method-entry compile waits or runs. With one slot per
    /// method it was refused, uncounted, for the whole C1 and C2 pipeline.
    #[test]
    fn osr_is_admitted_while_the_methods_own_entry_compile_is_in_flight() {
        let policy = CompilationPolicy {
            c1_threshold: 1,
            ..CompilationPolicy::default()
        };
        let mgr = TieredCompilationManager::new(policy);
        let key = test_key();
        assert_eq!(mgr.on_method_invocation(&key), Some(CompilationTier::C1));

        let task = mgr
            .request_osr(&key, 5)
            .expect("the method-entry request does not hold the OSR slot");
        assert_eq!(task.osr_bci, Some(5));
        assert_eq!(mgr.queue_size(), 2);
        assert_eq!(mgr.stats().osr_refused_in_flight.load(Ordering::Relaxed), 0);

        // One OSR request per method, whatever the bci -- and counted.
        assert!(mgr.request_osr(&key, 5).is_none());
        assert!(mgr.request_osr(&key, 9).is_none());
        assert_eq!(mgr.stats().osr_refused_in_flight.load(Ordering::Relaxed), 2);
        // And one method-entry request.
        assert_eq!(mgr.on_method_invocation(&key), None);

        // The OSR completion releases the OSR slot only.
        mgr.core
            .complete_task(&key, CompilationTier::C2, 1, true, true, false);
        {
            let methods = mgr.core.methods.lock();
            let state = &methods[&key];
            assert!(!state.osr_in_flight);
            assert_eq!(state.queued_osr_bci, None);
            assert!(
                state.queued_for_compilation,
                "an OSR completion must not release the method-entry slot"
            );
            assert_eq!(state.queued_tier, Some(CompilationTier::C1));
            assert_eq!(
                state.current_tier,
                CompilationTier::Interpreter,
                "an OSR publish is not a method-entry tier"
            );
        }
        assert!(mgr.request_osr(&key, 9).is_some(), "OSR slot free again");

        // The method-entry completion releases the method-entry slot only.
        // With `CRATONVM_JIT_C2_AFTER_OPTIMIZING_OSR` (default ON since the
        // churn page's close) a C1 publish after the C2 OSR publish above
        // would promote the entry body at once and re-take the slot; this
        // test is about the slots, so the promotion is pinned off.
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_C2_AFTER_OPTIMIZING_OSR", Some("0"))],
            || mgr.compilation_complete(&key, CompilationTier::C1, 1),
        );
        let methods = mgr.core.methods.lock();
        let state = &methods[&key];
        assert!(!state.queued_for_compilation);
        assert!(
            state.osr_in_flight,
            "a method-entry completion must not release the OSR slot"
        );
        assert_eq!(state.queued_osr_bci, Some(9));
    }

    /// The branch-profile window belongs to the method-entry slot: an OSR
    /// request beside a C2 supersede neither closes it nor opens another.
    #[test]
    fn an_osr_request_beside_a_supersede_leaves_its_window_alone() {
        let mgr = TieredCompilationManager::with_default_policy();
        let key = MethodKey::new("craton/test/SplitSlotWindow", "loop", "()V");
        mgr.on_method_invocation(&key);
        mgr.compilation_complete(&key, CompilationTier::C1, 1);
        mgr.core.request_c2_upgrade(&key);
        assert_eq!(mgr.branch_window_balance(), 1);

        assert!(mgr.request_osr(&key, 4).is_some());
        mgr.core
            .complete_task(&key, CompilationTier::C2, 1, true, true, false);
        assert_eq!(
            mgr.branch_window_balance(),
            1,
            "the OSR completion closes nothing it did not open"
        );
        mgr.core
            .complete_task(&key, CompilationTier::C2, 1, true, false, false);
        assert_eq!(mgr.branch_window_balance(), 0);
    }

    /// Time-to-tier: the first method-entry body is timed from the first
    /// admission, the first C2 body from the first body, each once per method;
    /// an OSR publish is neither.
    #[test]
    fn time_to_tier_is_charged_once_per_method_and_not_by_osr() {
        let policy = CompilationPolicy {
            c1_threshold: 1,
            ..CompilationPolicy::default()
        };
        let mgr = TieredCompilationManager::new(policy);
        let key = MethodKey::new("craton/test/TimeToTier", "hot", "()V");
        assert_eq!(mgr.on_method_invocation(&key), Some(CompilationTier::C1));
        {
            let mut methods = mgr.core.methods.lock();
            let state = methods.get_mut(&key).expect("tracked");
            assert_ne!(state.first_nominated_ms, 0, "stamped at admission");
            // Backdate so the charged wait is visibly non-zero.
            state.first_nominated_ms = 1;
        }
        let s = mgr.stats();
        assert!(mgr.request_osr(&key, 2).is_some());
        mgr.core
            .complete_task(&key, CompilationTier::C2, 1, true, true, false);
        assert_eq!(s.nominate_to_first_body.load(Ordering::Relaxed), 0);
        assert_eq!(s.first_body_to_c2.load(Ordering::Relaxed), 0);

        mgr.compilation_complete(&key, CompilationTier::C1, 1);
        assert_eq!(s.nominate_to_first_body.load(Ordering::Relaxed), 1);
        let charged = s.nominate_to_first_body_ms_total.load(Ordering::Relaxed);
        assert_eq!(
            s.nominate_to_first_body_ms_max.load(Ordering::Relaxed),
            charged
        );
        assert_eq!(s.first_body_to_c2.load(Ordering::Relaxed), 0);

        mgr.compilation_complete(&key, CompilationTier::C2, 1);
        mgr.compilation_complete(&key, CompilationTier::C2, 1);
        assert_eq!(s.nominate_to_first_body.load(Ordering::Relaxed), 1);
        assert_eq!(s.first_body_to_c2.load(Ordering::Relaxed), 1, "once");
    }

    /// A counted trap drops every queued request for the method and releases
    /// both slots.
    #[test]
    fn a_counted_trap_releases_both_slots() {
        let policy = CompilationPolicy {
            c1_threshold: 1,
            ..CompilationPolicy::default()
        };
        let mgr = TieredCompilationManager::new(policy);
        let key = MethodKey::new("craton/test/SplitSlotTrap", "loop", "()V");
        assert_eq!(mgr.on_method_invocation(&key), Some(CompilationTier::C1));
        assert!(mgr.request_osr(&key, 3).is_some());
        let removed = mgr.core.remove_queued_for(&key);
        assert_eq!(removed.len(), 2);
        {
            let mut methods = mgr.core.methods.lock();
            let state = methods.get_mut(&key).expect("tracked");
            for (task, ticket) in &removed {
                mgr.core
                    .release_slot_for(state, task.target_tier, task.osr_bci, *ticket);
            }
            assert!(!state.queued_for_compilation);
            assert!(!state.osr_in_flight);
        }
        assert!(mgr.request_osr(&key, 3).is_some());
        assert_eq!(mgr.on_method_invocation(&key), Some(CompilationTier::C1));
    }

    // ── OSR requests disabled when tiered off ────────────────────────────

    #[test]
    fn osr_request_disabled_when_tiered_off() {
        let policy = CompilationPolicy {
            tiered_enabled: false,
            ..CompilationPolicy::default()
        };
        let mgr = TieredCompilationManager::new(policy);
        let key = test_key();
        assert!(mgr.request_osr(&key, 0).is_none());
    }

    // ── Background worker drains an enqueued task off-thread ──────────────

    /// How long a test waits for the background compile thread to reach a
    /// rendezvous before giving up.
    ///
    /// A **liveness** bound, not a latency assertion. None of these tests
    /// claims anything about how fast a compile is; the budget exists so a
    /// worker that never runs fails the suite instead of hanging it.
    ///
    /// Worth knowing what it is NOT for. All four worker tests below failed on
    /// this timeout about 1 run in 13 at `--test-threads=32`, which looks
    /// exactly like an oversubscribed box starving a spawned thread — each test
    /// starts its own `cratonvm-jit-compiler` (16 MiB stack), so a 32-way run
    /// has dozens of them. Raising the budget from 5 seconds to 60 changed
    /// nothing: still 6 failures in 60 runs. The task was not late, it was
    /// GONE — see [`worker_manager`]. Tuning a timeout is what you do after a
    /// measurement says it is a timing problem, not instead of measuring.
    const WORKER_RENDEZVOUS: std::time::Duration = std::time::Duration::from_secs(30);

    /// A manager for a test that starts a background worker: the same thing
    /// `TieredCompilationManager::new` builds, with the install epoch PINNED.
    ///
    /// A queued task is stamped with the install epoch it was enqueued at, and
    /// `take_fresh` drops it if the epoch has moved since. That is the
    /// redefinition rule and it is correct. But `TieredCompilationManager::new`
    /// reads the epoch from `crate::jit_install_epoch()`, which is
    /// **process-global**, and every `JitCache::clear_all` anywhere in this test
    /// binary advances it. So a sibling test clearing its own cache silently
    /// invalidated this test's queued task; the worker dropped it instead of
    /// compiling it, and the rendezvous channel never received. The symptom is
    /// a timeout, which reads as "the worker never ran".
    ///
    /// The section below already knew the hazard — "bumping the real
    /// `crate::JIT_INSTALL_EPOCH` would be non-deterministic (every
    /// `JitCache::clear_all` anywhere in this test binary advances it)" — and
    /// injects an epoch source for the tests that are ABOUT epochs. These four
    /// are not about epochs, which is exactly why nobody pinned theirs.
    fn worker_manager(policy: CompilationPolicy) -> TieredCompilationManager {
        TieredCompilationManager::with_install_epoch_source(
            policy,
            Some(Arc::new(AtomicU64::new(1))),
        )
    }

    /// wire-tiered-manager increment 1: crossing the C1 threshold via
    /// `on_method_invocation` enqueues a task, and the background compile thread
    /// dequeues + "compiles" it on a *different* thread, then publishes the tier.
    ///
    /// Deterministic: the test blocks on an `mpsc` recv (a synchronization
    /// handle) rather than sleeping, so it never races on timing.
    /// gcj pcp: while a GC pause scope lives, a queued compile does not
    /// start; it runs once the scope is dropped, and the wait is counted.
    #[test]
    fn gcj_pcp_a_compile_queued_during_a_gc_pause_starts_after_it() {
        use std::sync::mpsc;
        let policy = CompilationPolicy {
            c1_threshold: 2,
            c2_threshold: u32::MAX,
            c2_min_invocations: u32::MAX,
            osr_threshold: u32::MAX,
            tiered_enabled: true,
            c1_profiling: true,
        };
        let mgr = worker_manager(policy);
        let key = test_key();
        let (tx, rx) = mpsc::channel::<()>();
        let _bg = mgr
            .start_background_compiler(Box::new(move |_: &CompilationTask| -> CompileOutcome {
                tx.send(()).unwrap();
                CompileOutcome::declined(0)
            }))
            .expect("worker should start");
        let pause = mgr.gc_pause_scope();
        let _ = mgr.on_method_invocation(&key);
        assert_eq!(mgr.on_method_invocation(&key), Some(CompilationTier::C1));
        assert!(
            rx.recv_timeout(std::time::Duration::from_millis(300)).is_err(),
            "no compile starts inside the pause"
        );
        drop(pause);
        rx.recv_timeout(WORKER_RENDEZVOUS)
            .expect("the compile runs once the pause ends");
        assert_eq!(mgr.gc_pause_waits(), 1);
    }

    #[test]
    fn background_worker_drains_enqueued_task_off_thread() {
        use std::sync::mpsc;

        // Low C1 threshold so a couple of invocations cross it. Disable the
        // straight-to-C2 path by keeping c2 thresholds high.
        let policy = CompilationPolicy {
            c1_threshold: 2,
            c2_threshold: u32::MAX,
            c2_min_invocations: u32::MAX,
            osr_threshold: u32::MAX,
            tiered_enabled: true,
            c1_profiling: true,
        };
        let mgr = worker_manager(policy);
        let key = test_key();

        // The compile closure reports (task tier, the thread it ran on) back to
        // the test thread, proving the work happened off the "mutator".
        let (tx, rx) = mpsc::channel::<(CompilationTier, std::thread::ThreadId)>();
        let bg = mgr
            .start_background_compiler(Box::new(move |task: &CompilationTask| -> CompileOutcome {
                tx.send((task.target_tier, std::thread::current().id()))
                    .unwrap();
                // pretend the compile took 7ms and published
                CompileOutcome {
                    compile_time_ms: 7,
                    published: true,
                    c2_upgrade_candidate: false,
                    deferred_new_retry: false,
                    declined_permanently: false,
                    optimizing_publication_refused: false,
                }
            }))
            .expect("worker should start");

        let mutator_thread = std::thread::current().id();

        // Drive invocations on *this* (mutator) thread until the threshold
        // crossing enqueues a C1 task.
        assert!(
            mgr.on_method_invocation(&key).is_none(),
            "1st invocation: below threshold"
        );
        let rec = mgr.on_method_invocation(&key);
        assert_eq!(
            rec,
            Some(CompilationTier::C1),
            "threshold crossing enqueues C1"
        );

        // The worker should pick it up off-thread. Block on the channel (no sleep).
        let (compiled_tier, worker_thread) = rx
            .recv_timeout(WORKER_RENDEZVOUS)
            .expect("worker must drain the task");
        assert_eq!(compiled_tier, CompilationTier::C1);
        assert_ne!(
            worker_thread, mutator_thread,
            "compilation must run OFF the mutator thread"
        );

        // After completion the worker must publish the tier and clear the queue.
        // Spin briefly on the completion counter (bounded, no fixed sleep).
        let deadline = std::time::Instant::now() + WORKER_RENDEZVOUS;
        while mgr.completed_compilations() == 0 && std::time::Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert_eq!(mgr.completed_compilations(), 1, "one task completed");
        assert!(mgr.queue_empty(), "queue drained");
        assert_eq!(
            mgr.current_tier(&key),
            CompilationTier::C1,
            "tier published by worker"
        );
        {
            let methods = mgr.core.methods.lock();
            assert!(
                !methods[&key].queued_for_compilation,
                "queued flag cleared on completion"
            );
            assert_eq!(methods[&key].last_compile_time_ms, 7);
        }

        // Clean shutdown joins the worker thread.
        drop(bg);
        assert!(!mgr.compiler_active(), "worker stopped after shutdown");
    }

    /// Round 11 wave 4: an installed idle tick runs on an idle compile worker
    /// with no task ever queued, off the installing thread, and keeps running
    /// (it is a clock, not a one-shot). Shutdown still joins the worker.
    #[test]
    fn an_idle_worker_runs_the_installed_idle_tick() {
        use std::sync::mpsc;
        let mgr = worker_manager(CompilationPolicy::default());
        let (tx, rx) = mpsc::channel::<std::thread::ThreadId>();
        let tx = Mutex::new(tx);
        mgr.set_idle_tick(Box::new(move || {
            let _ = tx.lock().send(std::thread::current().id());
        }));
        let bg = mgr
            .start_background_compiler(Box::new(|_: &CompilationTask| CompileOutcome::declined(0)))
            .expect("worker should start");
        let first = rx
            .recv_timeout(WORKER_RENDEZVOUS)
            .expect("an idle worker must run the tick");
        assert_ne!(
            first,
            std::thread::current().id(),
            "the tick runs on a worker"
        );
        rx.recv_timeout(WORKER_RENDEZVOUS)
            .expect("and must keep running it");
        drop(bg);
        assert!(!mgr.compiler_active(), "worker stopped after shutdown");
    }

    /// j27 TIER (`CRATONVM_JIT_ENTRY_TRIP`): an armed box's trip reaches this
    /// manager's sink and raises the idle-tick request; the one sink is
    /// shared; an idle C1 worker consumes the request by running the tick.
    #[test]
    fn j27_an_entry_trip_requests_the_idle_tick() {
        use std::sync::mpsc;
        let mgr = worker_manager(CompilationPolicy::default());
        let sink = mgr.entry_trip_sink();
        assert!(Arc::ptr_eq(&sink, &mgr.entry_trip_sink()), "one sink per manager");
        assert!(!mgr.core.idle_tick_requested.load(Ordering::Acquire));
        let counter = crate::entry_counter::JitEntryCounter::new();
        counter.arm_trip(0, &sink);
        crate::entry_counter::jit_entry_counter_tripped(&counter);
        assert_eq!(sink.trips(), 1);
        assert!(
            mgr.core.idle_tick_requested.load(Ordering::Acquire),
            "the trip raised the request"
        );
        let (tx, rx) = mpsc::channel::<()>();
        let tx = Mutex::new(tx);
        mgr.set_idle_tick(Box::new(move || {
            let _ = tx.lock().send(());
        }));
        let bg = mgr
            .start_background_compiler(Box::new(|_: &CompilationTask| CompileOutcome::declined(0)))
            .expect("worker should start");
        rx.recv_timeout(WORKER_RENDEZVOUS)
            .expect("an idle worker must run the requested tick");
        drop(bg);
        assert!(
            !mgr.core.idle_tick_requested.load(Ordering::Acquire),
            "the worker consumed the request"
        );
    }

    // ── Tier → backend routing (Step 3) ──────────────────────────────────

    #[test]
    fn tier_routing_selects_optimized_backend_for_c2() {
        // C1 tiers route to the fast single-pass (no-opt) backend; C2 (and the
        // FullProfile collection tier, which only reaches codegen as a C2
        // promotion) route to the optimizing pipeline.
        assert!(!tier_uses_optimized_backend(CompilationTier::Interpreter));
        assert!(!tier_uses_optimized_backend(CompilationTier::C1));
        assert!(!tier_uses_optimized_backend(
            CompilationTier::C1WithProfiling
        ));
        assert!(tier_uses_optimized_backend(CompilationTier::FullProfile));
        assert!(tier_uses_optimized_backend(CompilationTier::C2));
    }

    // ── C1→C2 supersede: candidate C1 publish auto-enqueues a C2 recompile ──

    #[test]
    fn c1_publish_with_upgrade_candidate_enqueues_c2_supersede() {
        use std::sync::mpsc;
        let policy = CompilationPolicy {
            c1_threshold: 1,
            c2_threshold: u32::MAX,
            c2_min_invocations: u32::MAX,
            osr_threshold: u32::MAX,
            tiered_enabled: true,
            c1_profiling: true,
        };
        let mgr = worker_manager(policy);
        let key = test_key();

        let (tx, rx) = mpsc::channel::<CompilationTier>();
        let bg = mgr
            .start_background_compiler(Box::new(move |task: &CompilationTask| -> CompileOutcome {
                tx.send(task.target_tier).unwrap();
                CompileOutcome {
                    compile_time_ms: 1,
                    published: true,
                    // Models the VM-side predicate: judged IR-eligible on the
                    // C1 pass; a C2 task never re-seeds an upgrade.
                    c2_upgrade_candidate: !tier_uses_optimized_backend(task.target_tier),
                    deferred_new_retry: false,
                    declined_permanently: false,
                    optimizing_publication_refused: false,
                }
            }))
            .expect("worker should start");

        // Cross c1_threshold=1 → C1 task enqueued.
        assert_eq!(mgr.on_method_invocation(&key), Some(CompilationTier::C1));

        // Worker compiles C1, then the loop auto-enqueues + compiles the C2
        // supersede (Low priority). Deterministic via the channel.
        let first = rx
            .recv_timeout(WORKER_RENDEZVOUS)
            .expect("C1 compile must run");
        assert_eq!(first, CompilationTier::C1);
        let second = rx
            .recv_timeout(WORKER_RENDEZVOUS)
            .expect("C2 supersede compile must follow a candidate C1 publish");
        assert_eq!(second, CompilationTier::C2);

        // Both completions recorded; tier settles at C2; nothing re-queued
        // (request_c2_upgrade is idempotent and gated on current_tier < C2).
        let deadline = std::time::Instant::now() + WORKER_RENDEZVOUS;
        while mgr.completed_compilations() < 2 && std::time::Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert_eq!(mgr.completed_compilations(), 2);
        assert_eq!(mgr.current_tier(&key), CompilationTier::C2);
        assert!(mgr.queue_empty(), "no repeat upgrade churn");
        drop(bg);
    }

    /// str17: a C2 body whose publication the cache refused (`put` dropped it,
    /// the C1 body stayed live) leaves the method at C1 and is asked again; the
    /// second C2 attempt publishes. Before, the refused attempt was reported as a
    /// publish, the tier read C2, and the method ran its C1 body for good.
    #[test]
    fn a_refused_c2_publication_is_retried_and_does_not_record_c2() {
        use std::sync::mpsc;
        let policy = CompilationPolicy {
            c1_threshold: 1,
            c2_threshold: u32::MAX,
            c2_min_invocations: u32::MAX,
            osr_threshold: u32::MAX,
            tiered_enabled: true,
            c1_profiling: true,
        };
        let mgr = worker_manager(policy);
        let key = test_key();
        let c2_attempts = Arc::new(AtomicU32::new(0));
        let attempts = Arc::clone(&c2_attempts);

        let (tx, rx) = mpsc::channel::<CompilationTier>();
        let bg = mgr
            .start_background_compiler(Box::new(move |task: &CompilationTask| -> CompileOutcome {
                let optimized = tier_uses_optimized_backend(task.target_tier);
                // The first C2 attempt is refused at publication.
                let refused = optimized && attempts.fetch_add(1, Ordering::SeqCst) == 0;
                tx.send(task.target_tier).unwrap();
                CompileOutcome {
                    compile_time_ms: 1,
                    published: !refused,
                    c2_upgrade_candidate: !optimized,
                    deferred_new_retry: false,
                    declined_permanently: false,
                    optimizing_publication_refused: refused,
                }
            }))
            .expect("worker should start");

        assert_eq!(mgr.on_method_invocation(&key), Some(CompilationTier::C1));
        let tiers: Vec<CompilationTier> = (0..3)
            .map(|_| {
                rx.recv_timeout(WORKER_RENDEZVOUS)
                    .expect("C1, the refused C2 and its retry all run")
            })
            .collect();
        assert_eq!(
            tiers,
            [
                CompilationTier::C1,
                CompilationTier::C2,
                CompilationTier::C2
            ]
        );
        let deadline = std::time::Instant::now() + WORKER_RENDEZVOUS;
        while mgr.completed_compilations() < 3 && std::time::Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert_eq!(mgr.current_tier(&key), CompilationTier::C2);
        assert!(
            mgr.queue_empty(),
            "the published retry asks for nothing more"
        );
        drop(bg);
    }

    /// The retry is bounded by the tier-failure budget the unpublished attempts
    /// spend: a C2 body the cache refuses every time is asked
    /// `MAX_TIER_FAIL_RETRIES` times, and the method stays at C1.
    #[test]
    fn an_always_refused_c2_publication_stops_at_the_failure_budget() {
        // The budget arm: the refund (default ON since j27, `WARNING
        // j27-user-flip-20261028`) gives refused publications their attempts
        // back, so this test pins it off; the worker thread reads it, hence a
        // process override.
        cratonvm_types::flags::with_process_overrides(
            &[("CRATONVM_JIT_C2_PUBLICATION_REFUSAL_REFUND", Some("0"))],
            an_always_refused_c2_publication_stops_at_the_failure_budget_body,
        );
    }

    fn an_always_refused_c2_publication_stops_at_the_failure_budget_body() {
        let policy = CompilationPolicy {
            c1_threshold: 1,
            c2_threshold: u32::MAX,
            c2_min_invocations: u32::MAX,
            osr_threshold: u32::MAX,
            tiered_enabled: true,
            c1_profiling: true,
        };
        let mgr = worker_manager(policy);
        let key = test_key();
        let c2_attempts = Arc::new(AtomicU32::new(0));
        let attempts = Arc::clone(&c2_attempts);

        let bg = mgr
            .start_background_compiler(Box::new(move |task: &CompilationTask| -> CompileOutcome {
                let optimized = tier_uses_optimized_backend(task.target_tier);
                if optimized {
                    attempts.fetch_add(1, Ordering::SeqCst);
                }
                CompileOutcome {
                    compile_time_ms: 1,
                    published: !optimized,
                    c2_upgrade_candidate: !optimized,
                    deferred_new_retry: false,
                    declined_permanently: false,
                    optimizing_publication_refused: optimized,
                }
            }))
            .expect("worker should start");

        assert_eq!(mgr.on_method_invocation(&key), Some(CompilationTier::C1));
        let want = 1 + u64::from(MAX_TIER_FAIL_RETRIES);
        let deadline = std::time::Instant::now() + WORKER_RENDEZVOUS;
        while (mgr.completed_compilations() < want || !mgr.queue_empty())
            && std::time::Instant::now() < deadline
        {
            std::thread::yield_now();
        }
        assert_eq!(c2_attempts.load(Ordering::SeqCst), MAX_TIER_FAIL_RETRIES);
        assert_eq!(mgr.current_tier(&key), CompilationTier::C1);
        assert!(mgr.queue_empty(), "no C2 request after the budget is spent");
        drop(bg);
    }

    // ── Increment 2: flag-gated off-thread compile publishes the Jit target ──

    /// wire-tiered-manager increment 2: with background compilation enabled, a
    /// crossed threshold enqueues a task that the worker compiles OFF the
    /// mutator thread; the worker then "publishes" the compiled entry (here a
    /// shared `Jit`-target map standing in for `SharedVm::jit_cache`, which is
    /// VM-crate-only) and the manager's tier is flipped to the compiled tier —
    /// the jit-crate analogue of the invoke cache being updated to the `Jit`
    /// target. Deterministic: blocks on an `mpsc` recv, never sleeps.
    #[test]
    fn flag_on_threshold_compiles_off_thread_and_publishes_jit_target() {
        use std::sync::mpsc;

        // A stand-in for the VM's `jit_cache`: the compile closure inserts the
        // method key here to model "the Jit target is now installed/published".
        let published: Arc<Mutex<Vec<MethodKey>>> = Arc::new(Mutex::new(Vec::new()));

        // Low C2 threshold so a couple of invocations route STRAIGHT to C2
        // (the optimized backend), exercising the Step-3 routing decision.
        let policy = CompilationPolicy {
            c1_threshold: u32::MAX, // skip the C1 step
            c2_threshold: 2,
            c2_min_invocations: 2,
            osr_threshold: u32::MAX,
            tiered_enabled: true,
            c1_profiling: true,
        };
        let mgr = worker_manager(policy);
        let key = test_key();

        let mutator_thread = std::thread::current().id();
        let (tx, rx) = mpsc::channel::<(CompilationTier, bool, std::thread::ThreadId)>();
        let published_w = Arc::clone(&published);
        let bg = mgr
            .start_background_compiler(Box::new(move |task: &CompilationTask| -> CompileOutcome {
                // Real compile_fn shape: pick the backend by tier (Step 3),
                // "publish" the Jit target, and report back off-thread.
                let optimized = tier_uses_optimized_backend(task.target_tier);
                published_w.lock().push(task.method_key.clone());
                tx.send((task.target_tier, optimized, std::thread::current().id()))
                    .unwrap();
                CompileOutcome {
                    compile_time_ms: 3,
                    published: true,
                    c2_upgrade_candidate: false,
                    deferred_new_retry: false,
                    declined_permanently: false,
                    optimizing_publication_refused: false,
                }
            }))
            .expect("worker should start");

        // Drive invocations on the mutator thread until C2 is recommended.
        assert!(
            mgr.on_method_invocation(&key).is_none(),
            "1st invocation: below threshold"
        );
        let rec = mgr.on_method_invocation(&key);
        assert_eq!(
            rec,
            Some(CompilationTier::C2),
            "threshold crossing enqueues a C2 task (straight-to-C2 path)"
        );

        // Worker drains + compiles off-thread; block on the channel (no sleep).
        let (compiled_tier, optimized, worker_thread) = rx
            .recv_timeout(WORKER_RENDEZVOUS)
            .expect("worker must drain the task");
        assert_eq!(compiled_tier, CompilationTier::C2);
        assert!(
            optimized,
            "C2 must route to the optimized backend (Step 3 routing)"
        );
        assert_ne!(
            worker_thread, mutator_thread,
            "compilation must run OFF the mutator thread"
        );

        // After completion the worker publishes the tier (invoke-cache analogue)
        // and clears the queue. Bounded spin on the completion counter.
        let deadline = std::time::Instant::now() + WORKER_RENDEZVOUS;
        while mgr.completed_compilations() == 0 && std::time::Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert_eq!(mgr.completed_compilations(), 1, "one task completed");
        assert!(mgr.queue_empty(), "queue drained");
        assert_eq!(
            mgr.current_tier(&key),
            CompilationTier::C2,
            "Jit target (tier) published by the worker"
        );
        assert_eq!(
            *published.lock(),
            vec![key.clone()],
            "the compiled method's Jit target was published off-thread"
        );
        assert_eq!(mgr.stats().c2_compilations.load(Ordering::Relaxed), 1);

        drop(bg);
        assert!(!mgr.compiler_active(), "worker stopped after shutdown");
    }

    // ── Increment 3: GC-STW-safety — no VM-equivalent lock held across waits ──

    /// wire-tiered-manager increment 3 (GC-STW-safety): the background worker
    /// must hold NO VM lock across (a) its queue wait and (b) an in-flight
    /// compile's own blocking. This is the load-bearing invariant that keeps a
    /// STW prompt: a mutator wanting an exclusive VM lock (modelled here by
    /// `vm_lock`, standing in for `SharedVm::class_manager.write()`) must be
    /// able to acquire it WHILE a compile task is in-flight, because the worker
    /// only ever takes that lock for a bounded scope and drops it before doing
    /// anything blocking.
    ///
    /// The test drives a compile task whose `compile_fn` mirrors the real
    /// `background_compile_task` lock shape: briefly take `vm_lock` (read out
    /// what it needs), DROP it, then block (here on a barrier standing in for
    /// the long codegen / a `load_class_concurrent` condvar wait). While the
    /// worker is blocked mid-compile, a competing "STW initiator" thread must
    /// acquire `vm_lock` PROMPTLY. If the worker wrongly held a VM lock across
    /// its blocking wait, this acquisition would deadlock and the bounded
    /// `recv_timeout` would fire. Deterministic: every rendezvous is a channel
    /// recv or a barrier, never a sleep.
    #[test]
    fn worker_holds_no_vm_lock_across_blocking_compile() {
        use std::sync::mpsc;
        use std::sync::{Arc as StdArc, Barrier};

        // Stand-in for `SharedVm::class_manager` (the lock a class-defining
        // mutator / STW path contends for). The worker takes it only briefly.
        let vm_lock: StdArc<Mutex<u64>> = StdArc::new(Mutex::new(0));

        let policy = CompilationPolicy {
            c1_threshold: 1,
            c2_threshold: u32::MAX,
            c2_min_invocations: u32::MAX,
            osr_threshold: u32::MAX,
            tiered_enabled: true,
            c1_profiling: true,
        };
        let mgr = worker_manager(policy);
        let key = test_key();

        // Rendezvous: worker -> test when it has ENTERED the compile and is
        // about to block; a two-party barrier the worker waits on to model the
        // long in-flight compile; and a channel the worker uses to report the
        // VM-lock value it read during its bounded critical section.
        let (entered_tx, entered_rx) = mpsc::channel::<u64>();
        let release = StdArc::new(Barrier::new(2));

        let vm_lock_w = StdArc::clone(&vm_lock);
        let release_w = StdArc::clone(&release);
        let bg = mgr
            .start_background_compiler(Box::new(move |_task: &CompilationTask| -> CompileOutcome {
                // (1) Bounded VM-lock scope: acquire, read, DROP — exactly the
                // shape `try_jit_compile_callee_slow` uses for class_manager /
                // jit_cache. The guard must NOT survive into the blocking wait.
                let seen = {
                    let g = vm_lock_w.lock();
                    *g
                }; // <-- guard dropped here, BEFORE blocking below.
                entered_tx.send(seen).unwrap();
                // (2) Blocking wait with NO VM lock held — models long codegen
                // or a `load_class_concurrent` condvar wait. If a VM lock were
                // still held here, the STW thread below would deadlock.
                release_w.wait();
                CompileOutcome {
                    compile_time_ms: 4,
                    published: true,
                    c2_upgrade_candidate: false,
                    deferred_new_retry: false,
                    declined_permanently: false,
                    optimizing_publication_refused: false,
                }
            }))
            .expect("worker should start");

        // Enqueue one task (crossing c1_threshold=1 on the 1st invocation) ->
        // worker picks it up.
        assert_eq!(
            mgr.on_method_invocation(&key),
            Some(CompilationTier::C1),
            "1st invocation crosses c1_threshold=1 and enqueues a C1 task"
        );
        // The worker has entered the compile and finished its bounded VM-lock
        // critical section; block on the channel (no sleep).
        let seen = entered_rx
            .recv_timeout(WORKER_RENDEZVOUS)
            .expect("worker must enter compile and release the VM lock");
        assert_eq!(seen, 0, "worker read the VM-lock-protected state");

        // The worker is now blocked mid-compile (on `release`). A competing STW
        // initiator MUST be able to grab the VM lock promptly — proving the
        // worker holds no VM lock across its blocking wait. Do it on a separate
        // thread with a bounded join so a regression deadlocks the test thread's
        // timeout rather than hanging forever.
        let vm_lock_stw = StdArc::clone(&vm_lock);
        let (stw_tx, stw_rx) = mpsc::channel::<()>();
        let stw = std::thread::spawn(move || {
            let mut g = vm_lock_stw.lock();
            *g += 1; // mutate while the worker is mid-compile
            stw_tx.send(()).unwrap();
        });
        stw_rx
            .recv_timeout(WORKER_RENDEZVOUS)
            .expect("STW initiator must acquire the VM lock while a compile is in-flight");
        stw.join().unwrap();
        assert_eq!(*vm_lock.lock(), 1, "STW path mutated the VM-locked state");

        // The worker is also not holding its OWN queue lock while mid-compile:
        // `queue_size()` takes `core.queue.lock()` and returns without blocking,
        // confirming the worker dropped the queue lock before running compile_fn
        // (the queue was drained when the task was dequeued).
        assert_eq!(mgr.queue_size(), 0, "queue drained while compile in-flight");

        // Let the in-flight compile finish.
        release.wait();
        let deadline = std::time::Instant::now() + WORKER_RENDEZVOUS;
        while mgr.completed_compilations() == 0 && std::time::Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert_eq!(mgr.completed_compilations(), 1, "compile completed");

        drop(bg);
        assert!(!mgr.compiler_active(), "worker stopped after shutdown");
    }

    // ── Install-epoch stamping and the stale-request drop ────────────────
    //
    // Every test here drives a manager whose install epoch comes from an
    // INJECTED `AtomicU64`, so a "redefinition" is one `store` and the
    // scheduling rule is exercised with no thread, no backend, no clock and no
    // sleep. Bumping the real `crate::JIT_INSTALL_EPOCH` would be both
    // non-deterministic (every `JitCache::clear_all` anywhere in this test
    // binary advances it) and untestable in the other direction — there is no
    // way to hold it still.
    //
    // The scheduling counters are process-wide, so the tests that assert exact
    // counts use `crate::metrics::SchedulingCapture` — a per-thread view of the
    // same recorder. They used to take `METRICS_TEST_LOCK` and reset the table
    // instead, which does not work: the lock serialises the tests that ASSERT
    // and the other tests here are the PRODUCERS. See the macro's own doc.

    /// A manager reading its install epoch from `epoch` instead of the global.
    fn epoch_driven_manager(epoch: &Arc<AtomicU64>) -> TieredCompilationManager {
        TieredCompilationManager::with_install_epoch_source(
            CompilationPolicy::default(),
            Some(Arc::clone(epoch)),
        )
    }

    fn epoch_key(name: &str) -> MethodKey {
        MethodKey::new("craton/test/EpochSubject", name, "()V")
    }

    fn c1_task(key: &MethodKey) -> CompilationTask {
        CompilationTask {
            method_key: key.clone(),
            target_tier: CompilationTier::C1,
            priority: CompilationPriority::Normal,
            enqueue_time_ms: 0,
            osr_bci: None,
        }
    }

    /// Compare a dequeued task against an expected one while ignoring
    /// `enqueue_time_ms`.
    ///
    /// `TaskQueue::enqueue` stamps that field from the process clock whenever a
    /// door left it at the `0` "unset" sentinel, which is what makes request
    /// aging possible at all. Every test below that builds an expected
    /// `CompilationTask` by hand leaves it `0`, so full struct equality would
    /// compare a constant against a wall-clock reading and fail whenever the
    /// process happened to be more than a millisecond old. These tests are
    /// about epochs, deduplication, retry limits and window accounting — not
    /// about queue latency — so they compare everything else. The two tests
    /// that ARE about the stamp assert on `enqueue_time_ms` directly.
    #[track_caller]
    fn assert_task_eq(got: Option<CompilationTask>, want: Option<CompilationTask>) {
        let strip = |t: Option<CompilationTask>| {
            t.map(|mut t| {
                t.enqueue_time_ms = 0;
                t
            })
        };
        assert_eq!(strip(got), strip(want));
    }

    #[test]
    fn a_request_queued_at_the_current_epoch_dispatches() {
        let epoch = Arc::new(AtomicU64::new(7));
        let mgr = epoch_driven_manager(&epoch);
        let key = epoch_key("stillCurrent");
        mgr.enqueue_compilation(c1_task(&key));

        assert_eq!(mgr.install_epoch(), 7);
        assert_task_eq(mgr.next_fresh_task(), Some(c1_task(&key)));
        assert_eq!(mgr.dropped_requests(), 0);
        assert_eq!(mgr.queue_size(), 0);
    }

    /// Tier proposal R12-4: a C1 request dequeued after an optimizing body was
    /// published for its method outside the manager is dropped at dispatch,
    /// with its slot released and the tier left at C2; a request its method
    /// has not outgrown is kept.
    #[test]
    fn a_single_pass_request_overtaken_by_an_optimizing_publish_is_not_compiled() {
        if !crate::entry_counter::callee_promotion_enabled() {
            return; // the outside-publish note is switched off here
        }
        let epoch = Arc::new(AtomicU64::new(3));
        let mgr = epoch_driven_manager(&epoch);
        let key = epoch_key("overtaken");
        assert!(mgr.enqueue_compilation(c1_task(&key)));
        mgr.note_published_outside_manager(&key, CompilationTier::C2);
        let task = mgr
            .next_fresh_task()
            .expect("the C1 request is still current");
        assert!(mgr.core.retire_overtaken(&task, 0));
        assert_eq!(mgr.current_tier(&key), CompilationTier::C2);
        assert_eq!(mgr.dropped_requests(), 1);
        {
            let methods = mgr.core.methods.lock();
            let state = methods.get(&key).expect("tracked");
            assert!(!state.queued_for_compilation, "the slot is released");
        }

        let kept = epoch_key("notOvertaken");
        assert!(mgr.enqueue_compilation(c1_task(&kept)));
        let task = mgr.next_fresh_task().expect("queued");
        assert!(!mgr.core.retire_overtaken(&task, 0));
        assert_eq!(mgr.dropped_requests(), 1);
    }

    /// Tier proposal W4-1 (round 12 wave 5): with the VM's cache probe
    /// installed, a successful single-pass completion takes its tier from the
    /// body live under the key. C1 over a single-pass body although the tier
    /// read C2 (inferred, it stayed C2 and the method was never promoted
    /// again); C2 over a live optimizing body the tier no longer named.
    #[test]
    fn a_completion_takes_its_tier_from_the_live_body() {
        let mgr = TieredCompilationManager::new(CompilationPolicy::default());
        let live = Arc::new(Mutex::new(LiveEntryBody::SinglePass));
        let answer = Arc::clone(&live);
        if !mgr.set_live_entry_probe(Box::new(move |_: &MethodKey| Some(*answer.lock()))) {
            return; // `CRATONVM_JIT_TIER_FROM_LIVE_BODY=0` in this process
        }
        let key = MethodKey::new("craton/test/LiveBody", "run", "()V");
        mgr.on_method_invocation(&key);
        mgr.compilation_complete(&key, CompilationTier::C2, 1);
        assert_eq!(mgr.current_tier(&key), CompilationTier::C2);

        // The optimizing body went and a single-pass one was published.
        mgr.compilation_complete(&key, CompilationTier::C1, 1);
        assert_eq!(mgr.current_tier(&key), CompilationTier::C1);

        // A demotion raced an optimizing republication: the single-pass
        // publish kept that body, and the tier names it again.
        mgr.note_body_withdrawn(&key);
        *live.lock() = LiveEntryBody::Optimizing;
        mgr.compilation_complete(&key, CompilationTier::C1, 1);
        assert_eq!(mgr.current_tier(&key), CompilationTier::C2);
    }

    #[test]
    fn a_request_queued_before_an_epoch_bump_is_dropped_before_it_is_compiled() {
        let epoch = Arc::new(AtomicU64::new(1));
        let mgr = epoch_driven_manager(&epoch);
        let key = epoch_key("queuedThenRedefined");
        mgr.enqueue_compilation(c1_task(&key));
        assert!(
            mgr.method_states().iter().any(|(k, _, _)| *k == key),
            "enqueue tracks the method"
        );

        // A JVMTI redefinition / code-cache flush lands while the request sits
        // in the queue.
        epoch.store(2, Ordering::Release);

        assert_eq!(
            mgr.next_fresh_task(),
            None,
            "the stale request must not reach the backend"
        );
        assert_eq!(mgr.dropped_requests(), 1, "and must be counted, not lost");
        assert_eq!(mgr.queue_size(), 0);
    }

    #[test]
    fn a_dropped_request_releases_the_slot_without_spending_a_retry() {
        let epoch = Arc::new(AtomicU64::new(1));
        let mgr = epoch_driven_manager(&epoch);
        let key = epoch_key("reAdmitted");
        mgr.enqueue_compilation(c1_task(&key));
        epoch.store(2, Ordering::Release);
        assert_task_eq(mgr.next_fresh_task(), None);

        // THE property. A stale drop is not a compile failure: the method's
        // in-flight flag is cleared so the next invocation can re-admit it,
        // and none of `current_tier` / `tier_fail_count` / `ineligible` moved.
        // Routing the drop through `complete_task(success = false)` would burn
        // one of MAX_TIER_FAIL_RETRIES, so three redefinitions during warmup
        // would leave a hot method permanently interpreted.
        {
            let methods = mgr.core.methods.lock();
            let state = methods.get(&key).expect("state survives a stale drop");
            assert!(
                !state.queued_for_compilation,
                "the in-flight slot must be released so the method can re-admit"
            );
            assert_eq!(state.queued_tier, None);
            assert_eq!(state.tier_fail_count, 0, "a stale drop is not a failure");
            assert!(!state.ineligible, "a stale drop is not a policy decline");
            assert_eq!(state.current_tier, CompilationTier::Interpreter);
        }

        // Re-admission works, at the new epoch, and now dispatches.
        mgr.enqueue_compilation(c1_task(&key));
        assert_task_eq(mgr.next_fresh_task(), Some(c1_task(&key)));
        assert_eq!(
            mgr.dropped_requests(),
            1,
            "the re-admitted one was not dropped"
        );
    }

    #[test]
    fn stale_requests_do_not_starve_a_fresh_one_behind_them() {
        let epoch = Arc::new(AtomicU64::new(1));
        let mgr = epoch_driven_manager(&epoch);
        let stale_a = epoch_key("staleA");
        let stale_b = epoch_key("staleB");
        let fresh = epoch_key("fresh");
        // Same band, so the order in the queue is exactly the enqueue order and
        // the fresh request really is behind both stale ones.
        mgr.enqueue_compilation(c1_task(&stale_a));
        mgr.enqueue_compilation(c1_task(&stale_b));
        epoch.store(2, Ordering::Release);
        mgr.enqueue_compilation(c1_task(&fresh));

        assert_task_eq(mgr.next_fresh_task(), Some(c1_task(&fresh)));
        assert_eq!(mgr.dropped_requests(), 2);
        assert_eq!(mgr.queue_size(), 0);
    }

    #[test]
    fn priority_order_survives_the_epoch_gate() {
        let epoch = Arc::new(AtomicU64::new(1));
        let mgr = epoch_driven_manager(&epoch);
        let low = epoch_key("low");
        let high = epoch_key("high");
        mgr.enqueue_compilation(CompilationTask {
            priority: CompilationPriority::Low,
            ..c1_task(&low)
        });
        mgr.enqueue_compilation(CompilationTask {
            target_tier: CompilationTier::C2,
            priority: CompilationPriority::High,
            ..c1_task(&high)
        });

        // The gate filters; it does not reorder.
        let first = mgr.next_fresh_task().expect("a fresh task");
        assert_eq!(first.method_key, high);
        let second = mgr.next_fresh_task().expect("a fresh task");
        assert_eq!(second.method_key, low);
        assert_eq!(mgr.dropped_requests(), 0);
    }

    #[test]
    fn an_empty_queue_is_not_a_drop() {
        let epoch = Arc::new(AtomicU64::new(1));
        let mgr = epoch_driven_manager(&epoch);
        assert_task_eq(mgr.next_fresh_task(), None);
        epoch.store(9, Ordering::Release);
        assert_task_eq(mgr.next_fresh_task(), None);
        assert_eq!(mgr.dropped_requests(), 0);
    }

    #[test]
    fn every_enqueue_door_stamps_the_epoch() {
        // The stamp is applied by the queue, not by the caller, so a request
        // that entered through the policy path (`on_method_invocation`) or the
        // OSR path is gated identically to one the VM pushed directly. A
        // per-caller stamp is what would eventually be forgotten on one door.
        let policy = CompilationPolicy {
            c1_threshold: 1,
            c2_threshold: 1_000_000,
            osr_threshold: 1,
            tiered_enabled: true,
            c2_min_invocations: 1_000_000,
            c1_profiling: false,
        };
        let epoch = Arc::new(AtomicU64::new(1));
        let mgr =
            TieredCompilationManager::with_install_epoch_source(policy, Some(Arc::clone(&epoch)));

        let invoked = epoch_key("viaInvocationHook");
        assert_eq!(
            mgr.on_method_invocation(&invoked),
            Some(CompilationTier::C1),
            "crossing c1_threshold=1 enqueues"
        );
        epoch.store(2, Ordering::Release);
        assert_eq!(mgr.next_fresh_task(), None, "policy-path request is gated");
        assert_eq!(mgr.dropped_requests(), 1);

        let osr = epoch_key("viaOsrRequest");
        assert!(mgr.request_osr(&osr, 12).is_some(), "OSR request enqueues");
        epoch.store(3, Ordering::Release);
        assert_eq!(mgr.next_fresh_task(), None, "OSR request is gated too");
        assert_eq!(mgr.dropped_requests(), 2);
    }

    #[test]
    fn drops_reach_the_process_wide_scheduling_counters() {
        // Counted PER THREAD. `METRICS_TEST_LOCK` + a reset used to stand here
        // and did not work: the lock serialises the tests that ASSERT, while
        // every other `tiered` test that drops a request is a PRODUCER holding
        // nothing. It failed about 1 run in 20 at `--test-threads=32`,
        // reporting `Some(5)` for a count of 1. `next_fresh_task` records on
        // its caller's thread, which is this one.
        let counts = crate::metrics::SchedulingCapture::start();

        let epoch = Arc::new(AtomicU64::new(1));
        let mgr = epoch_driven_manager(&epoch);
        let key = epoch_key("counted");
        mgr.enqueue_compilation(c1_task(&key));
        epoch.store(2, Ordering::Release);
        assert_task_eq(mgr.next_fresh_task(), None);

        assert_eq!(
            counts.count("queue_dropped_stale_install_epoch"),
            1,
            "the drop must be visible in the metrics idiom, not only on the manager"
        );
        // …and it reaches the GLOBAL table too, which is what a sink reads.
        // Asserted as a floor, because that table is everyone's.
        assert!(crate::metrics::scheduling_dropped_total() >= 1);
    }

    #[test]
    fn invalidate_class_counts_the_requests_it_discards() {
        // Per thread; see the test above.
        let counts = crate::metrics::SchedulingCapture::start();

        let epoch = Arc::new(AtomicU64::new(1));
        let mgr = epoch_driven_manager(&epoch);
        let a = epoch_key("unloadedA");
        let b = epoch_key("unloadedB");
        let survivor = MethodKey::new("craton/test/OtherClass", "kept", "()V");
        mgr.enqueue_compilation(c1_task(&a));
        mgr.enqueue_compilation(CompilationTask {
            priority: CompilationPriority::High,
            ..c1_task(&b)
        });
        mgr.enqueue_compilation(c1_task(&survivor));

        mgr.invalidate_class(ClassId::new(0), "craton/test/EpochSubject");

        assert_eq!(mgr.queue_size(), 1, "only the unrelated class survives");
        assert_eq!(counts.count("queue_dropped_class_invalidated"), 2);
        assert_eq!(mgr.dropped_requests(), 2);
        // Still dispatchable: invalidating one class must not gate another.
        assert_task_eq(mgr.next_fresh_task(), Some(c1_task(&survivor)));
    }

    /// gc-common w36-c: the batch form leaves exactly what a loop of the
    /// single form leaves -- tiered states, queued requests, OSR denials,
    /// bail-list verdicts and the drop count -- for the empty set, an absent
    /// class, one class, and a mixed set with an id-less member. Covers the
    /// `belongs_to` rule both ways: another loader's same-named class keeps its
    /// rows against an identified member, and an id-less key (or member)
    /// matches by name.
    #[test]
    fn invalidate_classes_matches_a_loop_of_invalidate_class() {
        let a = ClassId::new(0x7A36_C001);
        let a2 = ClassId::new(0x7A36_C002);
        let b = ClassId::new(0x7A36_C003);
        let c = ClassId::new(0x7A36_C004);
        let keys = [
            MethodKey::with_class_id(a, "w36c/A", "m", "()V"),
            MethodKey::with_class_id(a2, "w36c/A", "m", "()V"),
            MethodKey::with_class_id(b, "w36c/B", "m", "()V"),
            MethodKey::new("w36c/B", "idless", "()V"),
            MethodKey::with_class_id(c, "w36c/C", "m", "()V"),
            MethodKey::new("w36c/C", "idless", "()V"),
        ];
        let fill = || {
            let mgr = epoch_driven_manager(&Arc::new(AtomicU64::new(1)));
            for key in &keys {
                mgr.core
                    .methods
                    .lock()
                    .insert(key.clone(), MethodState::new(key.clone()));
                mgr.core.mark_osr_denied(key.clone());
                mgr.verdicts().mark_bail_listed(
                    key.class_id,
                    &key.class_name,
                    &key.method_name,
                    &key.descriptor,
                );
            }
            for key in &keys {
                assert!(mgr.enqueue_compilation(c1_task(key)));
            }
            mgr
        };
        type Row = (u32, String, String);
        let row = |key: &MethodKey| -> Row {
            (
                key.class_id.as_u32(),
                key.class_name.to_string(),
                key.method_name.to_string(),
            )
        };
        let state = |mgr: &TieredCompilationManager| {
            let mut methods: Vec<Row> = mgr.core.methods.lock().keys().map(row).collect();
            methods.sort();
            let mut denied: Vec<Row> = mgr.core.osr_denied.read().keys().map(row).collect();
            denied.sort();
            let mut queued: Vec<Row> = Vec::new();
            while let Some(task) = mgr.dequeue_compilation() {
                queued.push(row(&task.method_key));
            }
            queued.sort();
            let bail_listed: Vec<bool> = keys
                .iter()
                .map(|key| {
                    mgr.verdicts().is_bail_listed(
                        key.class_id,
                        &key.class_name,
                        &key.method_name,
                        &key.descriptor,
                    )
                })
                .collect();
            (methods, denied, queued, bail_listed, mgr.dropped_requests())
        };
        let sets: [&[(ClassId, &str)]; 4] = [
            &[],
            &[(ClassId::new(0x7A36_C0F8), "w36c/Absent")],
            &[(a, "w36c/A")],
            &[(a, "w36c/A"), (b, "w36c/B"), (ClassId::new(0), "w36c/C")],
        ];
        for set in sets {
            let looped = fill();
            for &(id, name) in set {
                looped.invalidate_class(id, name);
            }
            let batched = fill();
            batched.invalidate_classes(&crate::UnloadedClassSet::new(set.iter().copied()));
            let (l, bt) = (state(&looped), state(&batched));
            assert_eq!(l, bt, "set {set:?}");
        }
        // The mixed set leaves exactly the other loader's `w36c/A`.
        let batched = fill();
        batched.invalidate_classes(&crate::UnloadedClassSet::new(sets[3].iter().copied()));
        let (methods, denied, queued, bail_listed, dropped) = state(&batched);
        let survivor = vec![row(&keys[1])];
        assert_eq!(methods, survivor);
        assert_eq!(denied, survivor);
        assert_eq!(queued, survivor);
        assert_eq!(bail_listed, [false, true, false, false, false, false]);
        assert_eq!(dropped, 5, "five queued requests were discarded");
    }

    #[test]
    fn shutdown_counts_the_requests_it_abandons() {
        // Per thread; see `drops_reach_the_process_wide_scheduling_counters`.
        // `shutdown` drains on the caller's thread — there is no worker here.
        let counts = crate::metrics::SchedulingCapture::start();

        let epoch = Arc::new(AtomicU64::new(1));
        let mgr = epoch_driven_manager(&epoch);
        // No worker is started, so nothing drains: the queue is exactly what
        // teardown finds. Deterministic, and no thread to race.
        mgr.enqueue_compilation(c1_task(&epoch_key("abandonedA")));
        mgr.enqueue_compilation(c1_task(&epoch_key("abandonedB")));

        let mut bg = BackgroundCompiler {
            core: Arc::clone(&mgr.core),
            handles: Vec::new(),
        };
        bg.shutdown();

        assert_eq!(mgr.queue_size(), 0, "shutdown drains rather than leaves");
        assert_eq!(
            counts.count("queue_shutdown_abandoned"),
            2,
            "abandoning work at teardown is correct, but it is still countable"
        );
        assert_eq!(mgr.dropped_requests(), 2);
    }

    // ── Deduplication: one method-entry slot per method ──────────────────

    #[test]
    fn an_identical_request_is_deduplicated_not_queued_twice() {
        let counts = crate::metrics::SchedulingCapture::start();
        let mgr = epoch_driven_manager(&Arc::new(AtomicU64::new(1)));
        let key = epoch_key("dedup");
        let c2 = CompilationTask {
            target_tier: CompilationTier::C2,
            priority: CompilationPriority::High,
            ..c1_task(&key)
        };

        assert!(mgr.enqueue_compilation(c1_task(&key)));
        assert!(
            !mgr.enqueue_compilation(c1_task(&key)),
            "an identical request is refused"
        );
        assert!(
            !mgr.enqueue_compilation(c2.clone()),
            "so is a different request while the slot is held: one compile per method at a time"
        );
        assert_eq!(mgr.queue_size(), 1);
        assert_eq!(mgr.deduplicated_requests(), 2);
        assert_eq!(counts.count("queue_deduplicated"), 2);

        // A dispatched request still holds the slot until it completes.
        assert_task_eq(mgr.next_fresh_task(), Some(c1_task(&key)));
        assert!(!mgr.enqueue_compilation(c1_task(&key)));
        mgr.core
            .complete_task(&key, CompilationTier::C1, 1, true, false, false);
        assert!(mgr.enqueue_compilation(c2), "the slot frees on completion");
    }

    /// r11-tier: a runtime `MakeNotCompilable` closes METHOD ENTRY only. The
    /// OSR door deliberately does not ask the runtime de-spec verdict (one cold
    /// `assert` trap must not take a method's hot loops their OSR entry), and
    /// recording the verdict as `ineligible` refused `request_osr` anyway.
    #[test]
    fn make_not_compilable_leaves_background_osr_open() {
        let policy = CompilationPolicy {
            c1_threshold: 1,
            ..CompilationPolicy::default()
        };
        let mgr = TieredCompilationManager::new(policy);
        let key = test_key();
        let _ = mgr.on_method_invocation(&key);
        mgr.on_deoptimization(
            &key,
            DeoptReason::UnreachedCode,
            12,
            DeoptAction::MakeNotCompilable,
        );
        {
            let methods = mgr.core.methods.lock();
            let state = &methods[&key];
            assert!(state.entry_ineligible);
            assert!(!state.ineligible, "the OSR pipeline keeps its eligibility");
        }
        for _ in 0..5 {
            assert_eq!(
                mgr.on_method_invocation(&key),
                None,
                "method entry stays closed"
            );
        }
        assert!(
            mgr.request_osr(&key, 40).is_some(),
            "a hot loop in the method may still be OSR-compiled"
        );
    }

    /// A counted trap drops the request still queued for its method, so the
    /// VM's eager re-queue gets the slot instead of a second task beside it.
    #[test]
    fn a_counted_trap_drops_the_queued_request() {
        let counts = crate::metrics::SchedulingCapture::start();
        let mgr = epoch_driven_manager(&Arc::new(AtomicU64::new(1)));
        let key = epoch_key("trappedWhileQueued");
        assert!(mgr.enqueue_compilation(CompilationTask {
            target_tier: CompilationTier::C2,
            priority: CompilationPriority::High,
            ..c1_task(&key)
        }));

        mgr.on_deoptimization(
            &key,
            DeoptReason::ClassCheck,
            3,
            DeoptAction::RecompileAndReinterpret,
        );
        assert_eq!(mgr.queue_size(), 0, "the stale C2 request is gone");
        assert_eq!(mgr.dropped_requests(), 1);
        assert_eq!(counts.count("queue_dropped_deoptimized"), 1);

        assert!(mgr.enqueue_compilation(CompilationTask {
            priority: CompilationPriority::High,
            ..c1_task(&key)
        }));
        assert_eq!(mgr.queue_size(), 1, "exactly one request, not two");
    }

    // ── Loader-aware method keys ─────────────────────────────────────────

    #[test]
    fn same_named_classes_in_different_loaders_tier_independently() {
        let policy = CompilationPolicy {
            c1_threshold: 1,
            ..CompilationPolicy::default()
        };
        let mgr = TieredCompilationManager::with_install_epoch_source(
            policy,
            Some(Arc::new(AtomicU64::new(1))),
        );
        let app = MethodKey::with_class_id(ClassId::new(7), "com/example/Foo", "run", "()V");
        let plugin = MethodKey::with_class_id(ClassId::new(9), "com/example/Foo", "run", "()V");
        assert_ne!(app, plugin);

        assert_eq!(mgr.on_method_invocation(&app), Some(CompilationTier::C1));
        mgr.core
            .complete_task(&app, CompilationTier::C1, 0, false, false, true);
        mgr.mark_osr_denied(app.clone());

        assert_eq!(
            mgr.on_method_invocation(&plugin),
            Some(CompilationTier::C1),
            "one loader's ineligible verdict must not apply to the other's class"
        );
        assert!(!mgr.is_osr_denied(&plugin));

        // Unloading one loader's class leaves the other's state and queue alone.
        mgr.invalidate_class(ClassId::new(7), "com/example/Foo");
        let states = mgr.method_states();
        assert!(states.iter().all(|(k, _, _)| *k != app));
        assert!(states.iter().any(|(k, _, _)| *k == plugin));
        assert!(
            !mgr.is_osr_denied(&app),
            "unload forgets the class's denials"
        );
        // The app's C1 request (never dispatched here) went with its class.
        assert_eq!(mgr.queue_size(), 1, "the plugin's queued request survives");
    }

    /// A redefinition resets the redefined class's state by identity: a
    /// same-named class in another loader keeps its OSR denials.
    #[test]
    fn redefining_one_loaders_class_leaves_a_same_named_class_alone() {
        let epoch = Arc::new(AtomicU64::new(1));
        let mgr = epoch_driven_manager(&epoch);
        let app = MethodKey::with_class_id(ClassId::new(7), "com/example/Bar", "run", "()V");
        let plugin = MethodKey::with_class_id(ClassId::new(9), "com/example/Bar", "run", "()V");
        mgr.mark_osr_denied(app.clone());
        mgr.mark_osr_denied(plugin.clone());
        assert!(mgr.is_osr_denied(&app));
        assert!(mgr.is_osr_denied(&plugin));

        mgr.on_class_redefined(ClassId::new(7), "com/example/Bar");
        assert!(
            !mgr.is_osr_denied(&app),
            "the redefined class forgets its denials"
        );
        assert!(
            mgr.is_osr_denied(&plugin),
            "the other loader's same-named class keeps its denials"
        );

        // With no identity to go on, a redefinition falls back to the name.
        mgr.on_class_redefined(ClassId::new(0), "com/example/Bar");
        assert!(!mgr.is_osr_denied(&plugin));
    }

    // ── Branch-profile windows: armed once, closed once ──────────────────

    #[test]
    fn every_branch_window_is_closed_exactly_once_whichever_way_its_request_leaves() {
        let epoch = Arc::new(AtomicU64::new(1));
        let mgr = epoch_driven_manager(&epoch);

        // 1. A C1→C2 supersede dropped as stale.
        let stale = epoch_key("windowStale");
        mgr.on_method_invocation(&stale);
        mgr.compilation_complete(&stale, CompilationTier::C1, 1);
        mgr.core.request_c2_upgrade(&stale);
        assert_eq!(mgr.branch_window_balance(), 1);
        epoch.store(2, Ordering::Release);
        assert_task_eq(mgr.next_fresh_task(), None);
        // r11 wave 2: the method runs its C1 body, so nothing else would ever
        // re-form the supersede; the drop closes the dropped request's window
        // and the re-admission opens exactly one of its own.
        assert_eq!(
            mgr.branch_window_balance(),
            1,
            "a stale drop closes its window; the re-admitted request holds one"
        );
        let readmitted = mgr
            .next_fresh_task()
            .expect("the dropped supersede is re-admitted at the new epoch");
        assert_eq!(readmitted.method_key, stale);
        assert_eq!(readmitted.target_tier, CompilationTier::C2);
        mgr.core
            .complete_task(&stale, CompilationTier::C2, 1, true, false, false);
        assert_eq!(mgr.branch_window_balance(), 0);

        // 2. An OSR C2 completion never opened one and must not close one.
        let osr = epoch_key("windowOsr");
        let task = mgr.request_osr(&osr, 4).expect("OSR request");
        assert_task_eq(mgr.next_fresh_task(), Some(task));
        mgr.core
            .complete_task(&osr, CompilationTier::C2, 1, true, true, false);
        assert_eq!(
            mgr.branch_window_balance(),
            0,
            "an OSR completion closes nothing it did not open"
        );

        // 3. A deferred-new retry still queued when its class is invalidated.
        let unloaded = epoch_key("windowUnloaded");
        mgr.request_deferred_new_retry(&unloaded);
        assert_eq!(mgr.branch_window_balance(), 1);
        mgr.invalidate_class(ClassId::new(0), "craton/test/EpochSubject");
        assert_eq!(
            mgr.branch_window_balance(),
            0,
            "invalidate_class closes the window of the request it drops"
        );

        // 4. The ordinary exit: a supersede that compiles.
        let compiled = epoch_key("windowCompiled");
        mgr.on_method_invocation(&compiled);
        mgr.compilation_complete(&compiled, CompilationTier::C1, 1);
        mgr.core.request_c2_upgrade(&compiled);
        let task = mgr.next_fresh_task().expect("the supersede dispatches");
        mgr.core
            .complete_task(&compiled, task.target_tier, 1, true, false, false);
        assert_eq!(mgr.branch_window_balance(), 0);
    }

    /// Round 15 wave 5 (lane sync4): the static self-lock recompile door
    /// (`r15w4-sync3-tiered-self-lock-recompile-door-patch-FIXED-20260930.md`) is the
    /// deferred-`new` retry's C2 task under its own name: one C2 task, refused
    /// while it is queued.
    #[test]
    fn the_self_lock_recompile_door_enqueues_one_c2_retry() {
        let epoch = Arc::new(AtomicU64::new(1));
        let mgr = epoch_driven_manager(&epoch);
        let key = epoch_key("selfLockRecompile");
        mgr.request_self_lock_recompile(&key);
        assert_eq!(mgr.queue_size(), 1);
        mgr.request_self_lock_recompile(&key);
        assert_eq!(mgr.queue_size(), 1, "already queued");
        let task = mgr.next_fresh_task().expect("the retry dispatches");
        assert_eq!(task.method_key, key);
        assert_eq!(task.target_tier, CompilationTier::C2);
    }

    /// Round 15 wave 5 (lane osrtier, OT-1): the unenterable-OSR promotion is
    /// a METHOD-ENTRY C2 task (no `osr_bci`), refused while it is queued.
    #[test]
    fn the_osr_unenterable_promotion_enqueues_one_method_entry_c2_task() {
        let epoch = Arc::new(AtomicU64::new(1));
        let mgr = epoch_driven_manager(&epoch);
        let key = epoch_key("osrUnenterablePromotion");
        mgr.request_osr_unenterable_promotion(&key);
        assert_eq!(mgr.queue_size(), 1);
        mgr.request_osr_unenterable_promotion(&key);
        assert_eq!(mgr.queue_size(), 1, "already queued");
        let task = mgr.next_fresh_task().expect("the promotion dispatches");
        assert_eq!(task.method_key, key);
        assert_eq!(task.target_tier, CompilationTier::C2);
        assert_eq!(task.osr_bci, None);
    }

    /// r11-tier-stale-epoch-drop-loses-c2-upgrade-requests: a C1->C2 supersede
    /// dropped as stale is re-admitted at the new epoch, because the method
    /// runs its C1 body and no invocation hook will ever form it again. A C1
    /// request (formed by the hook) is still only dropped, and an OSR one too.
    #[test]
    fn a_stale_c2_supersede_is_readmitted_at_the_new_epoch() {
        let epoch = Arc::new(AtomicU64::new(1));
        let mgr = epoch_driven_manager(&epoch);
        let key = epoch_key("supersedeOvertaken");
        mgr.on_method_invocation(&key);
        mgr.compilation_complete(&key, CompilationTier::C1, 1);
        mgr.core.request_c2_upgrade(&key);
        assert_eq!(mgr.queue_size(), 1);

        epoch.store(2, Ordering::Release);
        assert_task_eq(mgr.next_fresh_task(), None);
        assert_eq!(
            mgr.dropped_requests(),
            1,
            "the stale request is still dropped"
        );
        assert_eq!(mgr.queue_size(), 1, "and a fresh one replaces it");
        let fresh = mgr
            .next_fresh_task()
            .expect("dispatchable at the new epoch");
        assert_eq!(fresh.method_key, key);
        assert_eq!(fresh.target_tier, CompilationTier::C2);
        assert_eq!(fresh.osr_bci, None);
        {
            let methods = mgr.core.methods.lock();
            assert_eq!(methods[&key].tier_fail_count, 0, "a drop is not a failure");
        }

        // A compile the epoch overtakes is re-admitted too, uncharged.
        epoch.store(3, Ordering::Release);
        mgr.core.finish(
            &key,
            Completion {
                tier: CompilationTier::C2,
                osr_bci: None,
                osr: false,
                compile_time_ms: 1,
                compile_time_ns: 0,
                budget_ms: 1,
                success: false,
                declined_permanently: false,
                epoch_moved: true,
                waiting_on_class: None,
                cap_refused: false,
                ticket: 0,
            },
        );
        let again = mgr
            .next_fresh_task()
            .expect("re-admitted after the overtaken compile");
        assert_eq!(again.target_tier, CompilationTier::C2);
        mgr.core
            .complete_task(&key, CompilationTier::C2, 1, true, false, false);
        assert_eq!(mgr.branch_window_balance(), 0, "every window closed once");

        // A hook-formed C1 request of an interpreted method is only dropped.
        let interp = epoch_key("interpretedC1");
        mgr.enqueue_compilation(c1_task(&interp));
        epoch.store(4, Ordering::Release);
        assert_task_eq(mgr.next_fresh_task(), None);
        assert_eq!(mgr.queue_size(), 0);
    }

    /// r11-tier-deopt-tiering-key-drops-the-class-id (tiered half): a deopt
    /// reported under an identity-less key reaches the ONE state tracked under
    /// that name, and a by-name re-queue does not mint a second, id-0 state.
    /// With two states of that name nothing is charged (owner unknowable).
    #[test]
    fn an_identity_less_deopt_key_resolves_to_the_unique_named_state() {
        let mgr = TieredCompilationManager::new(CompilationPolicy::default());
        let real = MethodKey::with_class_id(ClassId::new(7), "com/example/Only", "run", "()V");
        let by_name = MethodKey::new("com/example/Only", "run", "()V");
        mgr.on_method_invocation(&real);
        mgr.compilation_complete(&real, CompilationTier::C1, 1);
        assert_eq!(mgr.current_tier(&real), CompilationTier::C1);

        mgr.on_deoptimization(
            &by_name,
            DeoptReason::ClassCheck,
            3,
            DeoptAction::RecompileAndReinterpret,
        );
        assert_eq!(
            mgr.current_tier(&real),
            CompilationTier::Interpreter,
            "the evicted body's real state drops to the interpreter"
        );
        assert!(mgr.enqueue_compilation(CompilationTask {
            method_key: by_name.clone(),
            target_tier: CompilationTier::C1,
            priority: CompilationPriority::High,
            enqueue_time_ms: 0,
            osr_bci: None,
        }));
        assert!(
            !mgr.method_states().iter().any(|(k, _, _)| *k == by_name),
            "no id-0 twin state"
        );
        let queued = mgr
            .dequeue_compilation()
            .expect("queued under the real key");
        assert_eq!(queued.method_key.class_id, ClassId::new(7));

        // Two loaders: ambiguous, so the by-name deopt charges neither.
        let a = MethodKey::with_class_id(ClassId::new(8), "com/example/Twin", "run", "()V");
        let b = MethodKey::with_class_id(ClassId::new(9), "com/example/Twin", "run", "()V");
        for k in [&a, &b] {
            mgr.on_method_invocation(k);
            mgr.compilation_complete(k, CompilationTier::C1, 1);
        }
        mgr.on_deoptimization(
            &MethodKey::new("com/example/Twin", "run", "()V"),
            DeoptReason::ClassCheck,
            3,
            DeoptAction::RecompileAndReinterpret,
        );
        assert_eq!(mgr.current_tier(&a), CompilationTier::C1);
        assert_eq!(mgr.current_tier(&b), CompilationTier::C1);
    }

    // ── Settled tiering: the interpreter stops asking ────────────────────

    #[test]
    fn a_method_that_can_never_compile_reports_settled_until_something_changes() {
        let policy = CompilationPolicy {
            c1_threshold: 1,
            ..CompilationPolicy::default()
        };
        let mgr = TieredCompilationManager::new(policy);
        let key = test_key();

        let first = mgr.on_method_invocation_settling(&key, 0);
        assert_eq!(first.recommended, Some(CompilationTier::C1));
        assert_eq!(
            first.settled_generation, 0,
            "a method with a request in flight is not settled"
        );

        mgr.core
            .complete_task(&key, CompilationTier::C1, 0, false, false, true);
        let banned = mgr.on_method_invocation_settling(&key, 0);
        assert_eq!(banned.recommended, None);
        assert!(
            mgr.tiering_settled(banned.settled_generation),
            "an ineligible method is settled"
        );

        // Anything that could unsettle a method expires every stamp.
        mgr.on_class_redefined(ClassId::new(0), "java/lang/String");
        assert!(!mgr.tiering_settled(banned.settled_generation));
        assert_eq!(
            mgr.on_method_invocation_settling(&key, 0).recommended,
            Some(CompilationTier::C1),
            "the redefinition cleared the verdict"
        );
        assert!(!mgr.tiering_settled(0), "a zero stamp is never settled");
    }

    #[test]
    fn observed_invocations_fast_forward_without_counting_the_report() {
        // Thresholds far above anything here, so no request takes the slot.
        let mgr = TieredCompilationManager::new(CompilationPolicy::default());
        let key = test_key();
        let count = |mgr: &TieredCompilationManager| {
            mgr.method_states()
                .into_iter()
                .find(|(k, _, _)| *k == key)
                .map(|(_, _, n)| n)
        };

        let _ = mgr.on_observed_invocations(&key, 0);
        assert_eq!(count(&mgr), Some(0), "a report of nothing counts nothing");
        let _ = mgr.on_observed_invocations(&key, 7);
        assert_eq!(count(&mgr), Some(7), "fast-forwarded, no +1");
        let _ = mgr.on_observed_invocations(&key, 7);
        let _ = mgr.on_observed_invocations(&key, 3);
        assert_eq!(
            count(&mgr),
            Some(7),
            "a repeated or stale report is a no-op"
        );
        // The invocation hook still charges the call itself.
        let _ = mgr.on_method_invocation_settling(&key, 7);
        assert_eq!(count(&mgr), Some(8));
    }

    /// The drain door (round 9 wave 3): a drained method has a published,
    /// counted body by construction, so a method the manager still has at
    /// `Interpreter` (its body came through a door that bypasses the manager)
    /// is NOT recommended for C1 -- that would recompile the body it runs --
    /// but IS taken straight to C2 once it crosses the C2 bar. The invocation
    /// hook, which reports a genuinely interpreted call, still recommends C1.
    #[test]
    fn observed_invocations_never_recompile_a_counted_body_at_c1() {
        let policy = CompilationPolicy {
            c1_threshold: 10,
            c2_threshold: 100,
            c2_min_invocations: 50,
            ..CompilationPolicy::default()
        };
        let mgr = TieredCompilationManager::new(policy);
        let key = test_key();
        assert_eq!(mgr.on_observed_invocations(&key, 9).recommended, None);
        let v = mgr.on_observed_invocations(&key, 10);
        assert_eq!(v.recommended, None, "no C1 recompile of a counted body");
        assert_eq!(v.settled_generation, 0, "not settled: C2 is still ahead");
        assert_eq!(mgr.queue_size(), 0);
        assert_eq!(mgr.on_observed_invocations(&key, 99).recommended, None);
        let v = mgr.on_observed_invocations(&key, 100);
        assert_eq!(
            v.recommended,
            Some(CompilationTier::C2),
            "past the C2 bar the drain takes it straight to the optimizing tier"
        );
        assert_eq!(mgr.queue_size(), 1);

        // The invocation hook is unchanged.
        let hooked = test_key2();
        let v = mgr.on_method_invocation_settling(&hooked, 10);
        assert_eq!(v.recommended, Some(CompilationTier::C1));
    }

    /// The drain door's reason to exist: a method whose C1 body has landed
    /// (so the invocation hook never asks again) is promoted to C2 by the
    /// drained compiled-entry count alone.
    #[test]
    fn observed_invocations_promote_a_c1_method_past_the_c2_bar() {
        let policy = CompilationPolicy {
            c1_threshold: 10,
            c2_threshold: 100,
            c2_min_invocations: 50,
            ..CompilationPolicy::default()
        };
        let mgr = TieredCompilationManager::new(policy);
        let key = test_key();
        for _ in 0..10 {
            mgr.on_method_invocation(&key);
        }
        let task = mgr.dequeue_compilation().expect("the C1 request");
        assert_eq!(task.target_tier, CompilationTier::C1);
        mgr.compilation_complete(&key, CompilationTier::C1, 1);
        assert_eq!(mgr.current_tier(&key), CompilationTier::C1);

        assert_eq!(mgr.on_observed_invocations(&key, 99).recommended, None);
        assert_eq!(
            mgr.on_observed_invocations(&key, 100).recommended,
            Some(CompilationTier::C2)
        );
        let task = mgr.dequeue_compilation().expect("the C2 request");
        assert_eq!(task.target_tier, CompilationTier::C2);
        assert_eq!(task.osr_bci, None);
    }

    // ── Panic containment ────────────────────────────────────────────────

    #[test]
    fn a_contained_panic_is_visible_to_the_panic_hook_as_contained() {
        assert!(!compile_panic_is_contained());
        let inside = contain_compile_panic(compile_panic_is_contained).expect("no panic");
        assert!(
            inside,
            "the crash handler must see the scope while the hook runs"
        );
        let caught =
            contain_compile_panic(|| -> () { panic!("deliberate contained panic (test)") });
        assert!(caught.is_err());
        assert!(
            !compile_panic_is_contained(),
            "the scope closes again after the unwind"
        );
    }

    #[test]
    fn the_worker_survives_a_panicking_compile_fn() {
        use std::sync::mpsc;
        let policy = CompilationPolicy {
            c1_threshold: 1,
            c2_threshold: u32::MAX,
            c2_min_invocations: u32::MAX,
            osr_threshold: u32::MAX,
            tiered_enabled: true,
            c1_profiling: true,
        };
        let mgr = worker_manager(policy);
        let doomed = MethodKey::new("craton/test/Panics", "boom", "()V");
        let healthy = MethodKey::new("craton/test/Panics", "fine", "()V");

        let (tx, rx) = mpsc::channel::<MethodKey>();
        let bg = mgr
            .start_background_compiler(Box::new(move |task: &CompilationTask| -> CompileOutcome {
                if &*task.method_key.method_name == "boom" {
                    panic!("deliberate compile panic (test)");
                }
                tx.send(task.method_key.clone()).unwrap();
                CompileOutcome {
                    compile_time_ms: 1,
                    published: true,
                    c2_upgrade_candidate: false,
                    deferred_new_retry: false,
                    declined_permanently: false,
                    optimizing_publication_refused: false,
                }
            }))
            .expect("worker should start");

        assert_eq!(mgr.on_method_invocation(&doomed), Some(CompilationTier::C1));
        let deadline = std::time::Instant::now() + WORKER_RENDEZVOUS;
        while mgr.completed_compilations() == 0 && std::time::Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert_eq!(
            mgr.worker_panics(),
            1,
            "the panic was contained and counted"
        );

        // The same lane keeps serving: the next C1 compile still runs.
        assert_eq!(
            mgr.on_method_invocation(&healthy),
            Some(CompilationTier::C1)
        );
        assert_eq!(
            rx.recv_timeout(WORKER_RENDEZVOUS)
                .expect("the worker must survive the panic"),
            healthy
        );
        let deadline = std::time::Instant::now() + WORKER_RENDEZVOUS;
        while mgr.completed_compilations() < 2 && std::time::Instant::now() < deadline {
            std::thread::yield_now();
        }
        {
            let methods = mgr.core.methods.lock();
            assert!(
                methods[&doomed].ineligible,
                "a panicking compile is a permanent decline"
            );
            assert!(
                !methods[&doomed].queued_for_compilation,
                "and its slot is released"
            );
        }
        assert_eq!(
            mgr.inflight_install_epoch(),
            0,
            "the in-flight epoch is reset"
        );
        assert_eq!(
            mgr.inflight_compiles(),
            0,
            "and the count agrees — `0` is a legal epoch, so the epoch alone \
             cannot distinguish idle from 'running at epoch 0'"
        );
        assert_eq!(
            mgr.inflight_slot_exhausted(),
            0,
            "every dispatch found a slot"
        );
        assert!(mgr.compiler_active());
        drop(bg);
    }

    /// The slot array covers every worker configuration the knobs admit.
    ///
    /// `INFLIGHT_EPOCH_SLOTS` and the `clamp(1, MAX_COMPILER_THREADS_PER_LANE)`
    /// in `compiler_thread_counts_with` are two statements of one fact. Raise
    /// the clamp without raising the array and a dispatch silently stops
    /// recording its epoch — which is exactly the class of quiet wrongness the
    /// array replaced.
    #[test]
    fn dispatch_slot_count_covers_the_worker_clamp() {
        assert!(
            INFLIGHT_EPOCH_SLOTS >= 2 * MAX_COMPILER_THREADS_PER_LANE,
            "two lanes of at most {MAX_COMPILER_THREADS_PER_LANE} workers each \
             need {} slots, and there are {INFLIGHT_EPOCH_SLOTS}",
            2 * MAX_COMPILER_THREADS_PER_LANE
        );
        // And the clamp really is the number this is derived from.
        let (c1, c2) = compiler_thread_counts_with(
            |name| match name {
                "CRATONVM_TIER_C1_THREADS" => Some("100000".to_string()),
                "CRATONVM_TIER_C2_THREADS" => Some("100000".to_string()),
                _ => None,
            },
            8,
        );
        assert_eq!(
            (c1, c2),
            (MAX_COMPILER_THREADS_PER_LANE, MAX_COMPILER_THREADS_PER_LANE),
            "the knobs clamp to MAX_COMPILER_THREADS_PER_LANE"
        );
    }

    /// Concurrent dispatches each keep their OWN epoch, and the reader reports
    /// the oldest.
    ///
    /// The single-word record this replaced could not do either: the last
    /// store won whatever order the dispatches happened in, and a worker
    /// finishing out of order left a stale value standing. Both halves are
    /// checked here with dispatches interleaved deliberately.
    #[test]
    fn concurrent_dispatches_keep_their_own_epochs() {
        let core = CompilerCore::with_install_epoch_source(None);

        let a = core.note_dispatch(7);
        let b = core.note_dispatch(9);
        assert_eq!(
            core.inflight_oldest_epoch(),
            7,
            "the OLDEST in-flight dispatch is the answer, not the most recent \
             store — with one word, `b` would have overwritten `a`"
        );
        assert_eq!(core.inflight_count.load(Ordering::Acquire), 2);

        // Finish out of order: the newer compile completes first.
        core.note_dispatch_done(b);
        assert_eq!(
            core.inflight_oldest_epoch(),
            7,
            "`a` is still running at epoch 7"
        );

        core.note_dispatch_done(a);
        assert_eq!(
            core.inflight_oldest_epoch(),
            0,
            "idle once every dispatch is done"
        );
        assert_eq!(core.inflight_count.load(Ordering::Acquire), 0);
        assert_eq!(core.inflight_slot_exhausted.load(Ordering::Acquire), 0);
    }

    /// Install epoch `0` is a legal state, and the record does not confuse it
    /// with "idle".
    ///
    /// `0` is where `with_install_epoch_source`'s counter starts, so a
    /// sentinel that collides with it would report every compile dispatched at
    /// the first epoch as "no compile running". The slots hold `epoch + 1` for
    /// exactly this reason.
    #[test]
    fn epoch_zero_is_a_dispatch_not_an_idle_worker() {
        let core = CompilerCore::with_install_epoch_source(None);
        assert_eq!(core.inflight_count.load(Ordering::Acquire), 0);

        let slot = core.note_dispatch(0);
        assert_eq!(core.inflight_oldest_epoch(), 0, "the epoch really is 0");
        assert_eq!(
            core.inflight_count.load(Ordering::Acquire),
            1,
            "and the COUNT is what distinguishes this from idle"
        );

        core.note_dispatch_done(slot);
        assert_eq!(core.inflight_count.load(Ordering::Acquire), 0);
    }

    /// Running out of slots is counted, not silently ignored, and the count
    /// still balances.
    #[test]
    fn a_dispatch_with_no_free_slot_is_counted_and_still_balances() {
        let core = CompilerCore::with_install_epoch_source(None);
        let claimed: Vec<InflightSlot> = (0..INFLIGHT_EPOCH_SLOTS)
            .map(|i| core.note_dispatch(i as u64))
            .collect();
        assert_eq!(core.inflight_slot_exhausted.load(Ordering::Acquire), 0);

        let overflow = core.note_dispatch(9999);
        assert_eq!(overflow, InflightSlot::NONE);
        assert_eq!(
            core.inflight_slot_exhausted.load(Ordering::Acquire),
            1,
            "a dispatch that could not record its epoch is reported"
        );
        assert_eq!(
            core.inflight_count.load(Ordering::Acquire),
            INFLIGHT_EPOCH_SLOTS as u64 + 1,
            "and it is still counted as running"
        );

        core.note_dispatch_done(overflow);
        for slot in claimed {
            core.note_dispatch_done(slot);
        }
        assert_eq!(
            core.inflight_count.load(Ordering::Acquire),
            0,
            "handing back InflightSlot::NONE clears nothing but still balances \
             the count"
        );
        assert_eq!(core.inflight_oldest_epoch(), 0);
    }

    // ── Per-manager workers ──────────────────────────────────────────────

    #[test]
    fn a_second_manager_in_one_process_drains_its_own_queue() {
        use std::sync::mpsc;
        let policy = || CompilationPolicy {
            c1_threshold: 1,
            c2_threshold: u32::MAX,
            c2_min_invocations: u32::MAX,
            osr_threshold: u32::MAX,
            tiered_enabled: true,
            c1_profiling: true,
        };
        let compile_into = |tx: mpsc::Sender<MethodKey>| -> CompileFn {
            Box::new(move |task: &CompilationTask| -> CompileOutcome {
                tx.send(task.method_key.clone()).unwrap();
                CompileOutcome {
                    compile_time_ms: 1,
                    published: true,
                    c2_upgrade_candidate: false,
                    deferred_new_retry: false,
                    declined_permanently: false,
                    optimizing_publication_refused: false,
                }
            })
        };
        let first = worker_manager(policy());
        let second = worker_manager(policy());
        let (tx1, rx1) = mpsc::channel::<MethodKey>();
        let (tx2, rx2) = mpsc::channel::<MethodKey>();
        // The production door, for both: it used to be a process-wide `Once`,
        // so the second call started nothing.
        ensure_background_compiler(&first, || compile_into(tx1));
        ensure_background_compiler(&second, || compile_into(tx2));
        assert!(first.compiler_active() && second.compiler_active());

        let k1 = MethodKey::new("craton/test/VmOne", "m", "()V");
        let k2 = MethodKey::new("craton/test/VmTwo", "m", "()V");
        assert_eq!(second.on_method_invocation(&k2), Some(CompilationTier::C1));
        assert_eq!(
            rx2.recv_timeout(WORKER_RENDEZVOUS)
                .expect("the second VM's queue is drained"),
            k2
        );
        assert_eq!(first.on_method_invocation(&k1), Some(CompilationTier::C1));
        assert_eq!(
            rx1.recv_timeout(WORKER_RENDEZVOUS)
                .expect("the first VM's queue is drained"),
            k1
        );
        assert!(
            rx1.try_recv().is_err() && rx2.try_recv().is_err(),
            "neither manager compiled the other's request"
        );

        second.shutdown_background_compiler();
        assert!(!second.compiler_active());
        assert!(
            first.compiler_active(),
            "stopping one VM's workers leaves the other's running"
        );
    }

    /// r9 wave 7 (review7a): a trap ledger idle for several decay periods owes
    /// one halving PER period, as the deopt log's ledger applies them -- not
    /// the single halving the next trap used to apply however long it had been.
    #[test]
    fn trap_decay_applies_every_period_it_owes() {
        let mut state = MethodState::new(test_key());
        state.trap_decay_ms = 1_000;
        state.trap_decay_invocations = 0;
        state.invocation_count = 0;
        state.deopt_count = 8;
        state.trap_counts.insert((DeoptReason::NullCheck, 3), 8);
        state.trap_counts.insert((DeoptReason::ClassCheck, 9), 1);

        // Less than one period: nothing moves.
        decay_traps(&mut state, 1_000 + TRAP_DECAY_MS - 1);
        assert_eq!(state.deopt_count, 8);
        assert_eq!(state.trap_decay_ms, 1_000);

        // Three whole periods and a bit: three halvings, and the stamp moves
        // by the whole periods only, keeping the remainder owed.
        decay_traps(&mut state, 1_000 + 3 * TRAP_DECAY_MS + 7);
        assert_eq!(state.deopt_count, 1, "8 >> 3");
        assert_eq!(
            state.trap_counts.get(&(DeoptReason::NullCheck, 3)),
            Some(&1)
        );
        assert!(
            !state
                .trap_counts
                .contains_key(&(DeoptReason::ClassCheck, 9)),
            "a site halved to zero is dropped"
        );
        assert_eq!(state.trap_decay_ms, 1_000 + 3 * TRAP_DECAY_MS);

        // Invocation periods count too, and the larger debt wins.
        state.deopt_count = 16;
        state.invocation_count = 2 * TRAP_DECAY_INVOCATIONS;
        decay_traps(&mut state, 1_000 + 3 * TRAP_DECAY_MS + 8);
        assert_eq!(state.deopt_count, 4, "16 >> 2 invocation periods");
        assert_eq!(state.trap_decay_invocations, 2 * TRAP_DECAY_INVOCATIONS);

        // An enormous idle gap saturates at zero without an overflowing shift.
        state.deopt_count = u32::MAX;
        decay_traps(&mut state, u64::MAX);
        assert_eq!(state.deopt_count, 0);
    }
}

/// Interpreter round i1 wave 15, lane L4: the method-stats dump holds the
/// first core weakly and keeps its report when the core drops first
/// (`interpreter-L3-diag-core-pins-the-first-vms-compiler-core-FIXED-20260925.md`).
#[cfg(test)]
mod i15_l4_diag_core_tests {
    use super::{
        CompilationPolicy, CompilerCore, DiagCore, MethodKey, MethodState, TieredCompilationManager,
    };
    use std::sync::Arc;

    /// The slot neither keeps its core alive nor loses what the core counted:
    /// the report read from the live core and the one retired as it dropped
    /// say the same thing, and the retired one is handed out once.
    #[test]
    fn the_diagnostic_slot_releases_its_core_and_keeps_its_report() {
        let mgr = TieredCompilationManager::new(CompilationPolicy::default());
        let key = MethodKey::new("i15/l4/Counted", "run", "()V");
        let mut state = MethodState::new(key.clone());
        state.invocation_count = 5;
        mgr.core.methods.lock().insert(key, state);
        let slot = DiagCore::new(&mgr.core, 3);
        assert!(slot.is_for(&mgr.core));
        let other = CompilerCore::with_install_epoch_source(None);
        assert!(!slot.is_for(&other));

        let live = slot.report().expect("the core lives");
        assert_eq!(live.snap.distinct_methods, 1);
        assert_eq!(live.snap.total_invocations, 5);
        assert_eq!(live.hot_but_stuck.len(), 1, "5 invocations cross 3");
        assert!(slot.retired.lock().is_none(), "a live read retires nothing");

        // What `CompilerCore::drop` does for the reported core when the
        // report is armed (it asks `DIAG_CORE`, not this local slot).
        slot.retire(&mgr.core);
        drop(mgr);
        assert!(
            slot.core.upgrade().is_none(),
            "the slot does not keep the dropped VM's core alive"
        );
        let retired = slot.report().expect("the retired report");
        assert_eq!(retired.c1_threshold, 3);
        assert_eq!(retired.snap.distinct_methods, 1);
        assert_eq!(retired.snap.total_invocations, 5);
        assert_eq!(retired.hot_but_stuck.len(), 1);
        assert_eq!(retired.verdict_fields, live.verdict_fields);
        assert!(slot.report().is_none(), "handed out once");
    }

    /// No manager's core is held strongly by the process-wide slot: whichever
    /// manager claimed it, the slot's strong count is only what live managers
    /// and workers hold.
    #[test]
    fn the_process_slot_holds_no_strong_reference() {
        let mgr = TieredCompilationManager::new(CompilationPolicy::default());
        let diag = super::DIAG_CORE.get().expect("a manager was built");
        if diag.is_for(&mgr.core) {
            assert_eq!(Arc::strong_count(&mgr.core), 1);
            drop(mgr);
            assert!(diag.core.upgrade().is_none());
        }
    }
}

/// Interpreter round i1 wave 18, lane L5: a VM's tiered manager forgets a
/// class's runtime de-speculation verdicts in its own VM's registry only.
#[cfg(test)]
mod i18_l5_runtime_despec_scope_tests {
    use super::{AtomicU32, AtomicU64, ClassId, TieredCompilationManager};
    use crate::compile_gate::RuntimeDespecRegistry;
    use std::sync::Arc;

    /// Another VM's class with the same `ClassId` keeps its verdict when this
    /// VM unloads or redefines its own class under that id.
    #[test]
    fn a_class_unload_forgets_only_in_the_managers_own_vm() {
        let flush = Arc::new(AtomicU64::new(1));
        let redefine = Arc::new(AtomicU32::new(0));
        let own = RuntimeDespecRegistry::new_registered_with_vm_epochs(
            Arc::clone(&flush),
            Arc::clone(&redefine),
        );
        let other = RuntimeDespecRegistry::new_registered();
        let mgr = TieredCompilationManager::with_env_policy_and_vm_epochs(
            flush,
            redefine,
            Arc::clone(&own),
        );
        let (id, name) = (ClassId::new(0x1805), "i18l5/SameId");
        own.mark(id, name, "m", "()V");
        other.mark(id, name, "m", "()V");

        mgr.invalidate_class(id, name);
        assert!(
            !own.is_not_compilable(id, name, "m", "()V"),
            "own VM forgets"
        );
        assert!(
            other.is_not_compilable(id, name, "m", "()V"),
            "another VM's class with this id is a different class"
        );

        own.mark(id, name, "m", "()V");
        mgr.on_class_redefined(id, name);
        assert!(!own.is_not_compilable(id, name, "m", "()V"));
        assert!(other.is_not_compilable(id, name, "m", "()V"));
    }
}

#[cfg(test)]
mod read_path_census_tests {
    use super::*;

    /// i73-L5 (lane zip10): the read path's classes match, nested classes of
    /// an exact-class prefix match, a longer sibling name does not.
    #[test]
    fn read_path_classes_match_by_package_or_exact_class() {
        for c in [
            "java/util/zip/ZipFile",
            "java/util/zip/ZipFile$Source",
            "java/util/zip/ZipUtils",
            "java/util/jar/JarFile",
            "jdk/internal/loader/URLClassPath$JarLoader",
            "java/net/URL",
            "java/net/URLClassLoader",
            "sun/net/www/ParseUtil",
            "jdk/internal/util/Preconditions",
        ] {
            assert!(read_path_census_class(c), "{c}");
        }
        for c in [
            "java/net/URLConnection",
            "java/net/URI",
            "java/util/HashMap",
            "sun/net/www/ParseUtils",
            "java/util/zipper/X",
        ] {
            assert!(!read_path_census_class(c), "{c}");
        }
    }

    /// The census lines: one summary, the top rows by invocations, fields as
    /// `key=value` and the name last; nothing for no rows.
    #[test]
    fn read_path_census_lines_rank_by_invocations() {
        assert!(read_path_census_lines(Vec::new()).is_empty());
        let row = |n: u64, tier, deopts, fails, inelig, name: &str, reason: &str| -> ReadPathRow {
            (
                n,
                tier,
                deopts,
                fails,
                inelig,
                false,
                name.to_string(),
                reason.to_string(),
            )
        };
        let lines = read_path_census_lines(vec![
            row(
                10,
                CompilationTier::C2,
                1,
                0,
                false,
                "java/util/zip/ZipFile.getEntry(Ljava/lang/String;)Ljava/util/zip/ZipEntry;",
                "-",
            ),
            row(
                900,
                CompilationTier::Interpreter,
                0,
                2,
                false,
                "java/util/zip/ZipUtils.get16([BI)I",
                "monitor here",
            ),
            row(
                50,
                CompilationTier::C1,
                0,
                0,
                false,
                "java/net/URL.<init>(Ljava/net/URL;Ljava/lang/String;)V",
                "-",
            ),
        ]);
        assert_eq!(lines.len(), 4);
        assert_eq!(
            lines[0],
            "[cratonvm] jit-read-path-census summary methods=3 interpreted=1 compiled=2 \
             refused=1 deopted=1 interpreted-invocations=900 total-invocations=960"
        );
        assert_eq!(
            lines[1],
            "[cratonvm] jit-read-path-census rank=1 inv=900 tier=Interpreter deopts=0 fails=2 \
             ineligible=false queued=false reason=monitor_here java/util/zip/ZipUtils.get16([BI)I"
        );
        assert!(lines[2].contains("rank=2 inv=50 tier=C1 "), "{}", lines[2]);
        assert!(lines[3].ends_with(
            "java/util/zip/ZipFile.getEntry(Ljava/lang/String;)Ljava/util/zip/ZipEntry;"
        ));
    }
}

/// JIT round 21 wave 4 (lane codecache): HotSpot's
/// `PerMethodRecompilationCutoff` for invalidated optimizing bodies.
#[cfg(test)]
mod r21w4_codecache_withdrawal_cutoff_tests {
    use super::*;
    use crate::JitBodyWithdrawalSink as _;

    fn withdrawn_key(key: &MethodKey) -> crate::WithdrawnMethodKey {
        (
            Arc::clone(&key.class_name),
            Arc::clone(&key.method_name),
            Arc::clone(&key.descriptor),
            key.class_id,
        )
    }

    /// Publish a C2 body for `key` and have it invalidated (the sink's charge,
    /// then the withdrawal's demotion), `rounds` times; answer whether the
    /// method is bailed out of C2 afterwards.
    fn invalidated_rounds(mgr: &TieredCompilationManager, key: &MethodKey, rounds: u32) -> bool {
        mgr.on_method_invocation(key);
        let sink = mgr.withdrawal_sink().upgrade().expect("the manager is alive");
        let wkey = withdrawn_key(key);
        for _ in 0..rounds {
            mgr.compilation_complete(key, CompilationTier::C2, 10);
            sink.optimizing_bodies_invalidated(std::slice::from_ref(&wkey));
            sink.bodies_withdrawn(std::slice::from_ref(&wkey));
        }
        mgr.core.methods.lock()[key].c2_bailout
    }

    #[test]
    fn a_c2_body_invalidated_past_the_cutoff_stays_c1() {
        let mgr = TieredCompilationManager::with_default_policy();
        let key = MethodKey::new("r21w4/Storm", "hot", "(I)I");
        assert!(
            !invalidated_rounds(&mgr, &key, WITHDRAWAL_RECOMPILE_CUTOFF - 1),
            "one invalidation below the cutoff still recompiles at C2"
        );
        assert!(
            invalidated_rounds(&mgr, &key, 1),
            "the cutoff-th invalidation bails out of C2"
        );
        assert_eq!(
            mgr.core.methods.lock()[&key].c2_withdrawals,
            WITHDRAWAL_RECOMPILE_CUTOFF
        );
    }

    /// rev-x64 item 8: a withdrawal that is not a decompilation (a flush, an
    /// eviction, an unload sweep, the sweeper's `note_body_withdrawn`) is not
    /// charged, however many times it repeats.
    #[test]
    fn a_plain_withdrawal_is_not_charged() {
        let mgr = TieredCompilationManager::with_default_policy();
        let key = MethodKey::new("r21w4/Storm", "reloaded", "(I)I");
        mgr.on_method_invocation(&key);
        let sink = mgr.withdrawal_sink().upgrade().expect("the manager is alive");
        let wkey = withdrawn_key(&key);
        for _ in 0..WITHDRAWAL_RECOMPILE_CUTOFF + 10 {
            mgr.compilation_complete(&key, CompilationTier::C2, 10);
            sink.bodies_withdrawn(std::slice::from_ref(&wkey));
            mgr.compilation_complete(&key, CompilationTier::C2, 10);
            mgr.note_body_withdrawn(&key);
        }
        let methods = mgr.core.methods.lock();
        assert_eq!(methods[&key].c2_withdrawals, 0);
        assert!(!methods[&key].c2_bailout);
    }

    #[test]
    fn the_kill_switch_keeps_recompiling_at_c2() {
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_WITHDRAWAL_RECOMPILE_CUTOFF", Some("0"))],
            || {
                let mgr = TieredCompilationManager::with_default_policy();
                let key = MethodKey::new("r21w4/Storm", "killed", "(I)I");
                assert!(!invalidated_rounds(&mgr, &key, WITHDRAWAL_RECOMPILE_CUTOFF + 5));
            },
        );
    }

    /// A redefinition is new bytecode: the count starts again.
    #[test]
    fn a_redefinition_resets_the_withdrawal_count() {
        let mgr = TieredCompilationManager::with_default_policy();
        let key = MethodKey::new("r21w4/Storm", "redefined", "(I)I");
        assert!(invalidated_rounds(&mgr, &key, WITHDRAWAL_RECOMPILE_CUTOFF));
        mgr.on_class_redefined(ClassId::new(0), "r21w4/Storm");
        let methods = mgr.core.methods.lock();
        assert_eq!(methods[&key].c2_withdrawals, 0);
        assert!(!methods[&key].c2_bailout);
    }
}

/// JIT round 21 wave 4 follow-up (lane codecache, CODECACHE21-4): optimizing
/// OSR bodies withdrawn are charged to the same recompile cutoff.
#[cfg(test)]
mod r21w4_codecache_osr_withdrawal_tests {
    use super::*;
    use crate::JitBodyWithdrawalSink as _;

    fn osr_withdrawn(mgr: &TieredCompilationManager, key: &MethodKey, rounds: u32) -> bool {
        mgr.on_method_invocation(key);
        let sink = mgr.withdrawal_sink().upgrade().expect("the manager is alive");
        let wkey: crate::WithdrawnMethodKey = (
            Arc::clone(&key.class_name),
            Arc::clone(&key.method_name),
            Arc::clone(&key.descriptor),
            key.class_id,
        );
        for _ in 0..rounds {
            sink.optimizing_bodies_invalidated(std::slice::from_ref(&wkey));
        }
        let methods = mgr.core.methods.lock();
        let state = &methods[key];
        assert_eq!(state.current_tier, CompilationTier::Interpreter, "no tier change");
        state.c2_bailout
    }

    #[test]
    fn an_osr_body_withdrawn_past_the_cutoff_bails_out_of_c2() {
        let mgr = TieredCompilationManager::with_default_policy();
        let key = MethodKey::new("r21w4/OsrStorm", "loop", "()V");
        assert!(!osr_withdrawn(&mgr, &key, WITHDRAWAL_RECOMPILE_CUTOFF - 1));
        assert!(osr_withdrawn(&mgr, &key, 1));
    }

    #[test]
    fn the_cutoff_kill_switch_also_spares_osr_withdrawals() {
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_WITHDRAWAL_RECOMPILE_CUTOFF", Some("0"))],
            || {
                let mgr = TieredCompilationManager::with_default_policy();
                let key = MethodKey::new("r21w4/OsrStorm", "killed", "()V");
                assert!(!osr_withdrawn(&mgr, &key, WITHDRAWAL_RECOMPILE_CUTOFF + 1));
            },
        );
    }
}

/// JIT r22 wave 1 (lane inline, `jit-r10-tier2-proposals.md` section 1):
/// `CRATONVM_JIT_OSR_OWN_FAIL_BUDGET` lets the OSR pipeline answer to its own
/// retry budget only; the default keeps the method-entry coupling.
#[cfg(test)]
mod r22_inline_osr_own_fail_budget_tests {
    use super::*;

    fn offer(
        mgr: &TieredCompilationManager,
        key: &MethodKey,
        tier_fails: u32,
        osr_fails: u32,
    ) -> OsrRequest {
        {
            let mut methods = mgr.core.methods.lock();
            let state = methods
                .entry(key.clone())
                .or_insert_with(|| MethodState::new(key.clone()));
            state.tier_fail_count = tier_fails;
            state.osr_fail_count = osr_fails;
        }
        mgr.request_osr_classified(key, 7)
    }

    #[test]
    fn by_default_a_spent_method_entry_budget_refuses_the_osr_request() {
        let mgr = TieredCompilationManager::with_default_policy();
        let key = MethodKey::new("r22/OsrBudget", "coupled", "()V");
        assert_eq!(offer(&mgr, &key, MAX_TIER_FAIL_RETRIES, 0), OsrRequest::Refused);
    }

    #[test]
    fn the_switch_admits_an_osr_request_the_entry_budget_alone_refused() {
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_OSR_OWN_FAIL_BUDGET", Some("1"))],
            || {
                let mgr = TieredCompilationManager::with_default_policy();
                let key = MethodKey::new("r22/OsrBudget", "own", "()V");
                assert_ne!(offer(&mgr, &key, MAX_TIER_FAIL_RETRIES, 0), OsrRequest::Refused);
                // Its own budget still bounds it.
                let spent = MethodKey::new("r22/OsrBudget", "spent", "()V");
                assert_eq!(
                    offer(&mgr, &spent, 0, MAX_TIER_FAIL_RETRIES),
                    OsrRequest::Refused
                );
            },
        );
    }
}

#[cfg(test)]
mod r22_sync_withdrawal_census_tests {
    use super::{withdrawal_census_line, WITHDRAWAL_RECOMPILE_CUTOFF};

    /// JIT round 22 (lane sync, CODECACHE21-3): nothing when no method was
    /// charged; otherwise the top five by count, ties by name, bailed marked.
    #[test]
    fn the_withdrawal_census_names_the_top_five() {
        assert_eq!(withdrawal_census_line(Vec::new()), None);
        let rows = vec![
            (3, false, "a.m()V".to_string()),
            (WITHDRAWAL_RECOMPILE_CUTOFF, true, "z.storm()V".to_string()),
            (7, false, "b.m()V".to_string()),
            (7, false, "a.n()V".to_string()),
            (1, false, "c.m()V".to_string()),
            (2, false, "d.m()V".to_string()),
        ];
        let line = withdrawal_census_line(rows).expect("rows were charged");
        assert_eq!(
            line,
            format!(
                "[cratonvm] JIT withdrawal census: methods=6 bailed=1 cutoff={WITHDRAWAL_RECOMPILE_CUTOFF} \
                 | top: z.storm()V={WITHDRAWAL_RECOMPILE_CUTOFF}(bailed) a.n()V=7 b.m()V=7 a.m()V=3 d.m()V=2"
            )
        );
    }
}

/// JIT r22 wave 2 (lane inline, INLINE22-1): the acceptance re-offer is armed
/// only under `CRATONVM_JIT_C2_ACCEPTANCE_REOFFER`, fires once the drained
/// count reaches its bar, and is bounded per method.
#[cfg(test)]
mod r22_inline_acceptance_reoffer_tests {
    use super::*;

    const KEY: &str = "CRATONVM_JIT_C2_ACCEPTANCE_REOFFER";

    fn at_c2(mgr: &TieredCompilationManager, key: &MethodKey, invocations: u64) {
        let mut methods = mgr.core.methods.lock();
        let state = methods
            .entry(key.clone())
            .or_insert_with(|| MethodState::new(key.clone()));
        state.current_tier = CompilationTier::C2;
        state.invocation_count = invocations;
    }

    #[test]
    fn off_by_default_nothing_is_armed() {
        cratonvm_types::flags::with_thread_overrides(&[(KEY, None)], || {
            let mgr = TieredCompilationManager::with_default_policy();
            let key = MethodKey::new("r22/Reoffer", "off", "()V");
            at_c2(&mgr, &key, 100);
            assert!(!mgr.arm_acceptance_reoffer(&key));
            assert_eq!(mgr.core.methods.lock()[&key].acceptance_reoffer_bar, 0);
        });
    }

    #[test]
    fn an_armed_reoffer_fires_at_its_bar_and_is_bounded() {
        cratonvm_types::flags::with_thread_overrides(&[(KEY, Some("1"))], || {
            let mgr = TieredCompilationManager::with_default_policy();
            let key = MethodKey::new("r22/Reoffer", "on", "()V");
            at_c2(&mgr, &key, 10_000);
            assert!(mgr.arm_acceptance_reoffer(&key));
            // Wave 3 (item 2): in new calls on the fall-through body.
            let bar = 10_000 * (ACCEPTANCE_REOFFER_GROWTH - 1);
            assert_eq!(mgr.core.methods.lock()[&key].acceptance_reoffer_bar, bar);
            // Below the bar: nothing.
            let v = mgr.on_observed_invocations(&key, bar - 1);
            assert_eq!(v.recommended, None);
            // At the bar: one C2 task, and the bar is spent.
            let v = mgr.on_observed_invocations(&key, bar);
            assert_eq!(v.recommended, Some(CompilationTier::C2));
            {
                let methods = mgr.core.methods.lock();
                assert!(methods[&key].queued_for_compilation);
                assert_eq!(methods[&key].acceptance_reoffer_bar, 0);
            }
            // Bounded: MAX_ACCEPTANCE_REOFFERS arms in all.
            for _ in 1..MAX_ACCEPTANCE_REOFFERS {
                assert!(mgr.arm_acceptance_reoffer(&key));
            }
            assert!(!mgr.arm_acceptance_reoffer(&key));
        });
    }

    /// Wave 3 (item 1 of `r22-review-w3-intrinsics-on-inline-FIXED-20261020.md`): a
    /// redefinition clears an armed re-offer, and a method no longer at C2
    /// never fires one.
    #[test]
    fn a_redefinition_or_a_lower_tier_drops_the_reoffer() {
        cratonvm_types::flags::with_thread_overrides(&[(KEY, Some("1"))], || {
            let mgr = TieredCompilationManager::with_default_policy();
            let key = MethodKey::new("r22/Reoffer", "redef", "()V");
            at_c2(&mgr, &key, 10_000);
            assert!(mgr.arm_acceptance_reoffer(&key));
            mgr.on_class_redefined(key.class_id, &key.class_name);
            {
                let methods = mgr.core.methods.lock();
                if let Some(state) = methods.get(&key) {
                    assert_eq!(state.acceptance_reoffer_bar, 0);
                    assert_eq!(state.acceptance_reoffers, 0);
                }
            }
            // Armed again at C2, then demoted: the drain past the bar queues nothing.
            let key2 = MethodKey::new("r22/Reoffer", "demoted", "()V");
            at_c2(&mgr, &key2, 10_000);
            assert!(mgr.arm_acceptance_reoffer(&key2));
            mgr.core.methods.lock().get_mut(&key2).unwrap().current_tier =
                CompilationTier::Interpreter;
            let bar = 10_000 * (ACCEPTANCE_REOFFER_GROWTH - 1);
            // (The ordinary drain policy may still promote an interpreted
            // method; what matters is that the re-offer is spent, not fired.)
            let _ = mgr.on_observed_invocations(&key2, bar);
            assert_eq!(mgr.core.methods.lock()[&key2].acceptance_reoffer_bar, 0);
        });
    }
}

/// JIT round 23 wave 1, lane L1 (THROWS22-1): the headers an OSR profit exit
/// must find an optimizing entry at.
#[cfg(test)]
mod r23_l1_osr_profit_exit_tests {
    use super::osr_profit_exit_headers;

    /// Two nested `goto` loops and a bottom-tested (`if_icmplt`) loop: every
    /// back-edge target once, sorted; a method without a loop has none.
    #[test]
    fn r23_l1_profit_exit_headers_are_every_back_edge_target() {
        let code: Vec<u8> = vec![
            0x03, // 0: iconst_0
            0x3c, // 1: istore_1
            0x84, 0x01, 0x01, // 2: iinc 1 1        <- outer header
            0x84, 0x02, 0x01, // 5: iinc 2 1        <- inner header
            0xa7, 0xff, 0xfd, // 8: goto 5          (inner back edge)
            0xa7, 0xff, 0xf7, // 11: goto 2         (outer back edge)
            0x84, 0x03, 0x01, // 14: iinc 3 1       <- third header
            0x1b, // 17: iload_1
            0x1c, // 18: iload_2
            0xa1, 0xff, 0xfb, // 19: if_icmplt 14   (bottom-tested back edge)
            0xa7, 0xff, 0xeb, // 22: goto 1         (an edge back to pc 1)
            0xb1, // 25: return
        ];
        assert_eq!(osr_profit_exit_headers(&code), vec![1, 2, 5, 14]);
        // Two edges to one header count once.
        let twice: Vec<u8> = vec![
            0x84, 0x01, 0x01, // 0: iinc 1 1
            0x1b, // 3: iload_1
            0x99, 0xff, 0xfc, // 4: ifeq 0
            0xa7, 0xff, 0xf9, // 7: goto 0
            0xb1, // 10: return
        ];
        assert_eq!(osr_profit_exit_headers(&twice), vec![0]);
        assert!(osr_profit_exit_headers(&[0x03, 0xac]).is_empty());
        assert!(osr_profit_exit_headers(&[]).is_empty());
    }
}

/// JIT round 21 lane tiermgr: INDYCONST21-1 (`CRATONVM_JIT_TIER_RETRY_OWED`),
/// the owed C2 retry kept in the manager for every retry door; INDYLATE21-1's
/// tier-side half (every retry moves the method's acceptance-memo input
/// stamp); and the shutdown drain no longer re-admitting dropped C2 requests.
#[cfg(test)]
mod jr21_tiermgr_retry_owed_tests {
    use super::*;

    const OWED: &str = "CRATONVM_JIT_TIER_RETRY_OWED";
    const STAMP: &str = "CRATONVM_JIT_IR_MEMO_METHOD_INPUT_STAMP";

    fn manager(epoch: &Arc<AtomicU64>) -> TieredCompilationManager {
        TieredCompilationManager::with_install_epoch_source(
            CompilationPolicy::default(),
            Some(Arc::clone(epoch)),
        )
    }

    fn key(name: &str) -> MethodKey {
        MethodKey::new("craton/test/Jr21Owed", name, "()V")
    }

    fn completion(tier: CompilationTier) -> Completion {
        Completion {
            tier,
            osr_bci: None,
            osr: false,
            compile_time_ms: 1,
            budget_ms: 1,
            success: true,
            declined_permanently: false,
            epoch_moved: false,
            compile_time_ns: 1_000_000,
            waiting_on_class: None,
            cap_refused: false,
            ticket: 0,
        }
    }

    fn owed_of(mgr: &TieredCompilationManager, key: &MethodKey) -> Option<(u64, &'static str)> {
        mgr.core.methods.lock()[key].retry_owed
    }

    /// The EAGER19-3 race, for any door: the optimizing compile holding the
    /// slot was dispatched BEFORE the request, so it read the old state; the
    /// request is owed and asked again once that compile finishes.
    #[test]
    fn a_retry_refused_by_an_older_compile_is_asked_again_after_it() {
        cratonvm_types::flags::with_thread_overrides(&[(OWED, None)], || {
            let epoch = Arc::new(AtomicU64::new(1));
            let mgr = manager(&epoch);
            let k = key("older");
            assert_eq!(mgr.request_late_bind_recompile(&k), C2RetryAnswer::Enqueued);
            let task = mgr.next_fresh_task().expect("the first retry dispatches");
            let dispatched_at = mgr.core.retry_generation.load(Ordering::SeqCst);
            assert_eq!(
                mgr.request_indy_constant_recompile(&k),
                C2RetryAnswer::AlreadyQueued
            );
            assert_eq!(owed_of(&mgr, &k).map(|o| o.1), Some("indy-constant"));
            let owed = mgr
                .core
                .finish_owed(&k, completion(task.target_tier), dispatched_at);
            assert_eq!(
                owed,
                Some("indy-constant"),
                "the older compile did not answer it"
            );
            assert_eq!(owed_of(&mgr, &k), None, "taken");
            assert_eq!(
                mgr.core.ask_owed_retry(&k, "indy-constant"),
                C2RetryAnswer::Enqueued
            );
            assert_eq!(mgr.queue_size(), 1);
        });
    }

    /// A request answered `AlreadyQueued` while the holder was still QUEUED is
    /// answered by that compile: it is dispatched after the stamp. No second
    /// compile.
    #[test]
    fn a_retry_owed_to_a_compile_not_yet_dispatched_costs_nothing() {
        cratonvm_types::flags::with_thread_overrides(&[(OWED, None)], || {
            let epoch = Arc::new(AtomicU64::new(1));
            let mgr = manager(&epoch);
            let k = key("queued");
            assert_eq!(mgr.request_late_bind_recompile(&k), C2RetryAnswer::Enqueued);
            assert_eq!(
                mgr.request_deferred_new_retry(&k),
                C2RetryAnswer::AlreadyQueued
            );
            assert!(owed_of(&mgr, &k).is_some());
            let task = mgr.next_fresh_task().expect("dispatches");
            let dispatched_at = mgr.core.retry_generation.load(Ordering::SeqCst);
            let owed = mgr
                .core
                .finish_owed(&k, completion(task.target_tier), dispatched_at);
            assert_eq!(
                owed, None,
                "the holder read the state the request was about"
            );
            assert_eq!(owed_of(&mgr, &k), None);
            assert_eq!(mgr.queue_size(), 0);
        });
    }

    /// A single-pass compile holding the slot never answers a C2 retry,
    /// whenever it was dispatched.
    #[test]
    fn a_single_pass_holder_does_not_answer_an_owed_c2_retry() {
        cratonvm_types::flags::with_thread_overrides(&[(OWED, None)], || {
            let epoch = Arc::new(AtomicU64::new(1));
            let mgr = manager(&epoch);
            let k = key("singlePass");
            assert!(mgr.enqueue_compilation(CompilationTask {
                method_key: k.clone(),
                target_tier: CompilationTier::C1,
                priority: CompilationPriority::Normal,
                enqueue_time_ms: 0,
                osr_bci: None,
            }));
            // The `late-bind` doors owe through dev's own latch
            // (`late_bind_retry_owed`); this module asks another door.
            assert_eq!(
                mgr.request_indy_constant_recompile(&k),
                C2RetryAnswer::AlreadyQueued
            );
            let task = mgr.next_fresh_task().expect("the C1 task dispatches");
            let dispatched_at = mgr.core.retry_generation.load(Ordering::SeqCst);
            let owed = mgr
                .core
                .finish_owed(&k, completion(task.target_tier), dispatched_at);
            assert_eq!(owed, Some("indy-constant"));
        });
    }

    /// Off, an `AlreadyQueued` answer leaves nothing behind (the pre-round-21
    /// behaviour; the VM's own owed paths still apply).
    #[test]
    fn the_switch_off_owes_nothing() {
        cratonvm_types::flags::with_thread_overrides(&[(OWED, Some("0"))], || {
            let epoch = Arc::new(AtomicU64::new(1));
            let mgr = manager(&epoch);
            let k = key("off");
            assert_eq!(mgr.request_late_bind_recompile(&k), C2RetryAnswer::Enqueued);
            assert_eq!(
                mgr.request_late_bind_recompile(&k),
                C2RetryAnswer::AlreadyQueued
            );
            assert_eq!(owed_of(&mgr, &k), None);
        });
    }

    /// A holder dropped as stale (an install-epoch move) compiled nothing: the
    /// owed retry is asked again at the new epoch.
    #[test]
    fn a_stale_drop_of_the_holder_asks_the_owed_retry() {
        cratonvm_types::flags::with_thread_overrides(&[(OWED, None)], || {
            let epoch = Arc::new(AtomicU64::new(1));
            let mgr = manager(&epoch);
            let k = key("stale");
            assert_eq!(
                mgr.core.request_c2_retry(&k, "static self-lock"),
                C2RetryAnswer::Enqueued
            );
            // The `late-bind` doors owe through dev's own latch
            // (`late_bind_retry_owed`); this module asks another door.
            assert_eq!(
                mgr.request_indy_constant_recompile(&k),
                C2RetryAnswer::AlreadyQueued
            );
            epoch.store(2, Ordering::Release);
            assert!(
                mgr.next_fresh_task().is_none(),
                "the stale request is dropped"
            );
            assert_eq!(mgr.dropped_requests(), 1);
            assert_eq!(mgr.queue_size(), 1, "the owed retry is queued again");
            assert_eq!(owed_of(&mgr, &k), None);
        });
    }

    /// Review fix: the shutdown drain must leave the queues empty and every
    /// branch window closed; it used to re-admit a dropped C2 supersede.
    #[test]
    fn the_shutdown_drain_does_not_readmit_a_dropped_c2_request() {
        let epoch = Arc::new(AtomicU64::new(1));
        let mgr = manager(&epoch);
        let k = key("shutdown");
        mgr.on_method_invocation(&k);
        mgr.compilation_complete(&k, CompilationTier::C1, 1);
        assert!(mgr.core.request_c2_upgrade(&k));
        assert_eq!(mgr.branch_window_balance(), 1);
        let mut bg = BackgroundCompiler {
            core: Arc::clone(&mgr.core),
            handles: Vec::new(),
        };
        bg.shutdown();
        assert_eq!(mgr.queue_size(), 0, "nothing re-admitted at shutdown");
        assert_eq!(mgr.branch_window_balance(), 0, "the window is closed");
        assert!(!mgr.core.methods.lock()[&k].queued_for_compilation);
    }

    /// INDYLATE21-1: every retry door moves the method's acceptance-memo input
    /// stamp before it admits anything, so a refusal recorded under the old
    /// stamp no longer skips the build the retry asks for.
    #[test]
    fn a_retry_moves_the_methods_acceptance_memo_input_stamp() {
        let memo_key = |mgr: &TieredCompilationManager, id: ClassId, name: &str| {
            mgr.core.verdicts.ir_refusal_key(
                crate::ir_method_memo_hash("craton/test/Jr21Stamp", name, "(I)I"),
                id,
            )
        };
        cratonvm_types::flags::with_thread_overrides(&[(STAMP, None)], || {
            let epoch = Arc::new(AtomicU64::new(1));
            let mgr = manager(&epoch);
            let id = ClassId::new(0x2115);
            let k = MethodKey::with_class_id(id, "craton/test/Jr21Stamp", "k", "(I)I");
            let mine = memo_key(&mgr, id, "k");
            let other = memo_key(&mgr, id, "j");
            let memo = mgr.core.verdicts.ir_refusals();
            assert_eq!(memo.method_inputs_of(mine), 0);
            mgr.request_late_bind_recompile(&k);
            assert_eq!(memo.method_inputs_of(mine), 1, "the retry moved the stamp");
            // `AlreadyQueued` moves it too: the build in flight read the old one.
            mgr.request_late_bind_recompile(&k);
            assert_eq!(memo.method_inputs_of(mine), 2);
            assert_eq!(
                memo.method_inputs_of(other),
                0,
                "another method's is untouched"
            );
        });
        cratonvm_types::flags::with_thread_overrides(&[(STAMP, Some("0"))], || {
            let epoch = Arc::new(AtomicU64::new(1));
            let mgr = manager(&epoch);
            let id = ClassId::new(0x2116);
            let k = MethodKey::with_class_id(id, "craton/test/Jr21Stamp", "k", "(I)I");
            let mine = memo_key(&mgr, id, "k");
            mgr.request_late_bind_recompile(&k);
            assert_eq!(mgr.core.verdicts.ir_refusals().method_inputs_of(mine), 0);
        });
    }
}

/// t1 wave 11 lane JTIER: TIERMGR21-2 of
/// `docs/internal/proposals/jit/tier1/jit-r21-tiermgr-proposals.md`, the
/// per-admission slot ticket (`CRATONVM_JIT_TIER_SLOT_TICKET`).
#[cfg(test)]
mod t1_w11_jtier_slot_ticket_tests {
    use super::*;

    const SWITCH: &str = "CRATONVM_JIT_TIER_SLOT_TICKET";
    const CLASS: &str = "craton/test/T1W11JtierTicket";

    fn manager() -> TieredCompilationManager {
        TieredCompilationManager::new(CompilationPolicy {
            c1_threshold: 1,
            ..CompilationPolicy::default()
        })
    }

    fn completion(ticket: u64) -> Completion {
        Completion {
            tier: CompilationTier::C1,
            osr_bci: None,
            osr: false,
            compile_time_ms: 1,
            compile_time_ns: 1_000_000,
            budget_ms: 1,
            success: true,
            declined_permanently: false,
            epoch_moved: false,
            waiting_on_class: None,
            cap_refused: false,
            ticket,
        }
    }

    /// Admit a C1 request for the identity-less `key`, dequeue it as a worker
    /// does, unload the class while it "compiles", and re-admit the method
    /// under the re-created state. Returns the old request's ticket.
    fn dispatch_then_invalidate_and_readmit(
        mgr: &TieredCompilationManager,
        key: &MethodKey,
    ) -> u64 {
        assert_eq!(mgr.on_method_invocation(key), Some(CompilationTier::C1));
        let current = mgr.core.current_install_epoch();
        let mut stale = Vec::new();
        let (task, ticket) = mgr.core.lanes[C1_LANE]
            .queue
            .lock()
            .dequeue_fresh(current, &mut stale)
            .expect("the C1 request is queued");
        assert!(stale.is_empty());
        assert_eq!(task.method_key, *key);
        mgr.invalidate_class(ClassId::new(0), CLASS);
        assert_eq!(mgr.on_method_invocation(key), Some(CompilationTier::C1));
        assert_eq!(mgr.queue_size(), 1, "the new state's request is queued");
        ticket
    }

    /// The duplicate admission the item describes: the OLD compile finishing
    /// must not release the slot the NEW state granted, nor apply its outcome
    /// to the new state; a further invocation is deduplicated.
    #[test]
    fn t1_w11_jtier_an_orphaned_completion_keeps_the_new_states_slot() {
        cratonvm_types::flags::with_thread_overrides(&[(SWITCH, Some("1"))], || {
            let mgr = manager();
            assert!(mgr.core.slot_ticket);
            let key = MethodKey::new(CLASS, "orphan", "()V");
            let old = dispatch_then_invalidate_and_readmit(&mgr, &key);
            assert_ne!(old, 0, "an admission draws a ticket");
            {
                let methods = mgr.core.methods.lock();
                let state = &methods[&key];
                assert!(state.queued_for_compilation);
                assert_ne!(state.entry_slot_ticket, old, "the new request has its own ticket");
            }
            mgr.core.finish(&key, completion(old));
            {
                let methods = mgr.core.methods.lock();
                let state = &methods[&key];
                assert!(state.queued_for_compilation, "the new request still holds the slot");
                assert_eq!(
                    state.current_tier,
                    CompilationTier::Interpreter,
                    "the old class's publish is not the new state's"
                );
            }
            assert_eq!(mgr.on_method_invocation(&key), None, "deduplicated");
            assert_eq!(mgr.queue_size(), 1, "no duplicate request");
            // The new request's own completion releases the slot and counts.
            let current = mgr.core.current_install_epoch();
            let (_, new) = mgr.core.lanes[C1_LANE]
                .queue
                .lock()
                .dequeue_fresh(current, &mut Vec::new())
                .expect("queued");
            mgr.core.finish(&key, completion(new));
            let methods = mgr.core.methods.lock();
            let state = &methods[&key];
            assert!(!state.queued_for_compilation);
            assert_eq!(state.current_tier, CompilationTier::C1);
        });
    }

    /// `=0` (and a ticketless completion) keeps the `(tier, bci)` match: the
    /// old compile releases the new state's same-tier slot, the defect the
    /// ticket closes.
    #[test]
    fn t1_w11_jtier_switch_off_keeps_the_tier_match() {
        cratonvm_types::flags::with_thread_overrides(&[(SWITCH, Some("0"))], || {
            let mgr = manager();
            assert!(!mgr.core.slot_ticket);
            let key = MethodKey::new(CLASS, "legacy", "()V");
            let old = dispatch_then_invalidate_and_readmit(&mgr, &key);
            assert_eq!(old, 0, "no ticket with the switch off");
            mgr.core.finish(&key, completion(old));
            let methods = mgr.core.methods.lock();
            assert!(
                !methods[&key].queued_for_compilation,
                "the tier match released the new request's slot"
            );
        });
    }

    /// `release_slot_for` with a ticket is exact: a same-tier request under
    /// another ticket does not release the slot; the holder's ticket does.
    #[test]
    fn t1_w11_jtier_release_matches_the_ticket_exactly() {
        cratonvm_types::flags::with_thread_overrides(&[(SWITCH, Some("1"))], || {
            let mgr = manager();
            let key = MethodKey::new(CLASS, "exact", "()V");
            assert_eq!(mgr.on_method_invocation(&key), Some(CompilationTier::C1));
            let mut methods = mgr.core.methods.lock();
            let state = methods.get_mut(&key).expect("tracked");
            let held = state.entry_slot_ticket;
            mgr.core.release_slot_for(state, CompilationTier::C1, None, held + 1);
            assert!(state.queued_for_compilation, "another ticket does not release");
            mgr.core.release_slot_for(state, CompilationTier::C1, None, held);
            assert!(!state.queued_for_compilation, "the holder's ticket releases");
        });
    }

    /// Review fix (`CRATONVM_JIT_LATE_BIND_RETRY_OWED_ON_DROP`): a late-bind
    /// retry owed to a request that is then dropped stale (never compiled) is
    /// asked again once the slot is free; `=0` leaves the bit set and nothing
    /// queued, the loss the fix closes.
    #[test]
    fn t1_w11_jtier_a_stale_drop_hands_on_the_late_bind_owed_bit() {
        for (on_drop, requeued) in [("1", true), ("0", false)] {
            cratonvm_types::flags::with_thread_overrides(
                &[
                    ("CRATONVM_JIT_LATE_BIND_RETRY_OWED_ON_DROP", Some(on_drop)),
                    ("CRATONVM_JIT_LATE_BIND_RETRY_OWED", Some("1")),
                    ("CRATONVM_JIT_TIER_RETRY_OWED", Some("0")),
                ],
                || {
                    let epoch = Arc::new(AtomicU64::new(1));
                    let mgr = TieredCompilationManager::with_install_epoch_source(
                        CompilationPolicy::default(),
                        Some(Arc::clone(&epoch)),
                    );
                    let key = MethodKey::new(CLASS, "lateBindDrop", "()V");
                    assert_eq!(mgr.request_late_bind_recompile(&key), C2RetryAnswer::Enqueued);
                    assert_eq!(
                        mgr.request_late_bind_recompile(&key),
                        C2RetryAnswer::AlreadyQueued
                    );
                    assert!(mgr.core.methods.lock()[&key].late_bind_retry_owed);
                    epoch.store(2, Ordering::Release);
                    assert!(mgr.next_fresh_task().is_none(), "the stale request is dropped");
                    assert_eq!(mgr.dropped_requests(), 1);
                    let owed_left = mgr.core.methods.lock()[&key].late_bind_retry_owed;
                    if requeued {
                        assert_eq!(mgr.queue_size(), 1, "the owed retry is asked again");
                        assert!(!owed_left, "the bit is spent");
                    } else {
                        assert_eq!(mgr.queue_size(), 0);
                        assert!(owed_left, "=0: the bit waits for a compile that never comes");
                    }
                },
            );
        }
    }
}

/// CONF21-1 (JIT round 21 lane conf proposal; page
/// `jr21-conf-string-identity-differs-only-from-hotspot-c2-FIXED-20261027.md`): the admission
/// sink sees every queued request once, an optimizing method-entry task and
/// every OSR task marked optimizing, and no refused one.
#[cfg(test)]
mod conf21_compile_admission_sink_tests {
    use super::*;

    fn task(name: &str, tier: CompilationTier, osr_bci: Option<u32>) -> CompilationTask {
        CompilationTask {
            method_key: MethodKey::new("craton/test/Conf21Subject", name, "()V"),
            target_tier: tier,
            priority: CompilationPriority::Normal,
            enqueue_time_ms: 0,
            osr_bci,
        }
    }

    #[test]
    fn the_sink_sees_each_admission_and_marks_the_optimizing_ones() {
        let mgr = TieredCompilationManager::new(CompilationPolicy::default());
        let seen: Arc<Mutex<Vec<(String, bool)>>> = Arc::new(Mutex::new(Vec::new()));
        let sink_seen = Arc::clone(&seen);
        assert!(mgr.set_compile_admission_sink(Box::new(move |key: &MethodKey, optimizing| {
            sink_seen.lock().push((key.method_name.to_string(), optimizing));
        })));
        // A second installation is refused; the first stays.
        assert!(!mgr.set_compile_admission_sink(Box::new(|_: &MethodKey, _| {})));
        assert!(mgr.enqueue_compilation(task("single", CompilationTier::C1, None)));
        assert!(mgr.enqueue_compilation(task("optimizing", CompilationTier::C2, None)));
        assert!(mgr.enqueue_compilation(task("loop", CompilationTier::C1, Some(7))));
        // A refused duplicate is not an admission.
        assert!(!mgr.enqueue_compilation(task("optimizing", CompilationTier::C2, None)));
        assert_eq!(
            *seen.lock(),
            vec![
                ("single".to_string(), false),
                ("optimizing".to_string(), true),
                ("loop".to_string(), true),
            ]
        );
    }
}

#[cfg(test)]
mod j27_trapprof_per_bci_limit_keeps_c2_tests {
    //! j27 wave 5 (lane TRAPPROF; page
    //! `j27-per-bci-trap-limit-bans-c2-for-the-life-of-the-method-FIXED-20261029.md`):
    //! under `CRATONVM_JIT_PER_BCI_TRAP_LIMIT_KEEPS_C2` the per-bci limit no
    //! longer ends the method's C2 career (HotSpot: `PerBytecodeTrapLimit` only
    //! stops the speculation there, which the VM's de-spec registry does at the
    //! same count); the per-method cutoff still does, so it stays bounded.
    use super::*;

    const SWITCH: &str = "CRATONVM_JIT_PER_BCI_TRAP_LIMIT_KEEPS_C2";

    fn key() -> MethodKey {
        MethodKey::new("craton/test/J27TrapLimit", "f", "(I)I")
    }

    /// A manager that tracks `key()` (a trap on an untracked key charges
    /// nobody).
    fn manager() -> (TieredCompilationManager, MethodKey) {
        let mgr = TieredCompilationManager::with_default_policy();
        let k = key();
        mgr.on_method_invocation(&k);
        (mgr, k)
    }

    fn trap(mgr: &TieredCompilationManager, k: &MethodKey, bci: u32) {
        mgr.on_deoptimization(k, DeoptReason::NullCheck, bci, DeoptAction::RecompileAndReinterpret);
    }

    fn bailed(mgr: &TieredCompilationManager, k: &MethodKey) -> bool {
        mgr.core.methods.lock()[k].c2_bailout
    }

    /// `PER_BCI_TRAP_LIMIT` traps at one bci, each followed by the trap's own
    /// C2 recompile; answers whether the method was still offered C2 after
    /// the last one.
    fn offered_c2_after_limit_at_one_bci(on: bool) -> bool {
        cratonvm_types::flags::with_thread_overrides(&[(SWITCH, on.then_some("1"))], || {
            let (mgr, k) = manager();
            for _ in 0..PER_BCI_TRAP_LIMIT {
                trap(&mgr, &k, 12);
                if mgr.request_trap_recompile_c2(&k) != C2RetryAnswer::Enqueued {
                    return false;
                }
                mgr.dequeue_compilation().expect("the C2 re-queue");
                mgr.compilation_complete(&k, CompilationTier::C2, 1);
            }
            !bailed(&mgr, &k)
        })
    }

    #[test]
    fn four_traps_at_one_bci_keep_c2_for_the_rest_of_the_method() {
        assert!(offered_c2_after_limit_at_one_bci(true));
        assert!(
            !offered_c2_after_limit_at_one_bci(false),
            "switch off: the per-bci limit sets c2_bailout, as before"
        );
    }

    #[test]
    fn a_method_trapping_at_every_bci_still_bails() {
        cratonvm_types::flags::with_thread_overrides(&[(SWITCH, Some("1"))], || {
            let (mgr, k) = manager();
            for bci in 0..PER_METHOD_TRAP_CUTOFF - 1 {
                trap(&mgr, &k, bci);
            }
            assert!(!bailed(&mgr, &k), "below the per-method cutoff");
            trap(&mgr, &k, 9_999);
            assert!(bailed(&mgr, &k));
            assert_eq!(mgr.request_trap_recompile_c2(&k), C2RetryAnswer::Refused);
        });
    }

    #[test]
    fn one_bci_that_keeps_trapping_still_bails_at_the_method_cutoff() {
        cratonvm_types::flags::with_thread_overrides(&[(SWITCH, Some("1"))], || {
            let (mgr, k) = manager();
            for _ in 0..PER_METHOD_TRAP_CUTOFF - 1 {
                trap(&mgr, &k, 12);
            }
            assert!(!bailed(&mgr, &k));
            trap(&mgr, &k, 12);
            assert!(bailed(&mgr, &k), "a de-spec the planner never honours stays bounded");
        });
    }
}
