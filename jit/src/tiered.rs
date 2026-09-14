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
//!       └──► C1 (per-bci trap limit or per-method trap cutoff → c2_bailout, stays C1)
//! ```

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use cratonvm_types::ClassId;
use parking_lot::{Condvar, Mutex, RwLock};
use rustc_hash::FxHashMap;

use crate::deopt::{DeoptAction, DeoptReason};

/// Consecutive background-compile attempts allowed to fail (run but not
/// publish a body) at a given tier before `should_compile` gives up on that
/// method entirely. See `CompilerCore::finish` / `should_compile`.
const MAX_TIER_FAIL_RETRIES: u32 = 3;

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
fn process_uptime_ms() -> u64 {
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
struct CompilePanicScope;

impl CompilePanicScope {
    fn enter() -> Self {
        let _ = COMPILE_PANIC_SCOPES.try_with(|depth| depth.set(depth.get().saturating_add(1)));
        CompilePanicScope
    }
}

impl Drop for CompilePanicScope {
    fn drop(&mut self) {
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
            .map(|n| n.clamp(1, 64))
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
            p.tiered_enabled = s != "0" && !s.eq_ignore_ascii_case("false");
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
    /// Whether a request for this method holds the in-flight slot: queued, or
    /// dispatched and still compiling. See [`CompilerCore::admit`].
    pub queued_for_compilation: bool,
    /// Tier of the request holding the slot.
    pub queued_tier: Option<CompilationTier>,
    /// OSR bci of the request holding the slot (`None` for method entry).
    /// With `queued_tier`, the identity a leaving request is matched against
    /// before it may release the slot -- see [`CompilerCore::release_slot_for`].
    pub queued_osr_bci: Option<u32>,
    /// The request holding the slot opened a branch-profile window
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
    pub tier_fail_count: u32,
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
            queued_osr_bci: None,
            branch_window_armed: false,
            deopt_count: 0,
            trap_counts: FxHashMap::default(),
            trap_decay_invocations: 0,
            trap_decay_ms: process_uptime_ms(),
            c2_budget_overruns: 0,
            last_compile_time_ms: 0,
            c2_bailout: false,
            tier_fail_count: 0,
            ineligible: false,
        }
    }

    /// Whether nothing the invocation hook can do will change this method's
    /// tiering until something invalidates the state.
    ///
    /// Deliberately narrow. A C1 method that has not bailed out of C2 is NOT
    /// settled: it can still be recommended for C2 once it is hot enough.
    fn tiering_is_settled(&self) -> bool {
        self.ineligible
            || self.tier_fail_count >= MAX_TIER_FAIL_RETRIES
            || self.current_tier >= CompilationTier::C2
            || (self.c2_bailout && self.current_tier >= CompilationTier::C1)
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
/// to C2. HotSpot's `PerBytecodeTrapLimit` is 4 for the same reason: one
/// speculation that keeps failing is a property of the code, not noise.
pub const PER_BCI_TRAP_LIMIT: u32 = 4;

/// Counted traps across the whole method, however they are spread over sites,
/// before it stops being offered to C2 -- the analogue of HotSpot's per-method
/// trap limit and recompilation cutoff.
pub const PER_METHOD_TRAP_CUTOFF: u32 = 16;

/// Trap counts halve once the method has run this many further invocations...
pub const TRAP_DECAY_INVOCATIONS: u64 = 20_000;

/// ...or once this much process time has passed, whichever comes first.
///
/// The time arm exists because a method whose body is recompiled immediately
/// after each trap (the eager `RecompileAndReinterpret` re-queue in
/// `DeoptimizationController::deoptimize`) stops reaching the interpreter's
/// invocation hook, so its invocation count stops moving.
pub const TRAP_DECAY_MS: u64 = 60_000;

/// Whether a deopt with this recommended action is a trap the method is
/// charged for.
///
/// Only actions that throw the body away count. `Reinterpret` is a soft exit:
/// the frame continues in the interpreter and the body is still good. Charging
/// those spent a method's whole C2 allowance on routine OSR loop exits -- three
/// of them banned C2 *and* OSR for the rest of the process. A debugger's
/// `TransferToInterpreter` and a pending exception's handler hand-off are not
/// speculation failures at all, whatever action the log recommends for them.
pub fn deopt_is_counted_trap(reason: DeoptReason, action: DeoptAction) -> bool {
    if matches!(
        reason,
        DeoptReason::TransferToInterpreter | DeoptReason::PendingException
    ) {
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
/// again and stays interpreted forever. A soft OSR exit keeps its body;
/// everything else is evicted.
pub fn deopt_evicts_method_body(reason: DeoptReason, action: DeoptAction) -> bool {
    !(reason == DeoptReason::OsrExit && action == DeoptAction::Reinterpret)
}

/// Halve a method's trap counts when enough invocations or time have passed
/// since the last decay.
fn decay_traps(state: &mut MethodState, now_ms: u64) {
    let by_invocations = state
        .invocation_count
        .saturating_sub(state.trap_decay_invocations)
        >= TRAP_DECAY_INVOCATIONS;
    let by_time = now_ms.saturating_sub(state.trap_decay_ms) >= TRAP_DECAY_MS;
    if !(by_invocations || by_time) {
        return;
    }
    state.deopt_count /= 2;
    state.trap_counts.retain(|_, n| {
        *n /= 2;
        *n > 0
    });
    state.trap_decay_invocations = state.invocation_count;
    state.trap_decay_ms = now_ms;
}

// ───────────────────────────────────────────────────────────────────────────────
// CompilationQueue
// ───────────────────────────────────────────────────────────────────────────────

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
    /// [`crate::jit_install_epoch`] as of the moment this request entered the
    /// queue.
    install_epoch: u64,
    task: CompilationTask,
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

    /// Push `task`, stamped with `install_epoch`.
    fn enqueue(&mut self, task: CompilationTask, install_epoch: u64) {
        let entry = QueuedRequest {
            install_epoch,
            task,
        };
        match entry.task.priority {
            CompilationPriority::High => self.high.push_back(entry),
            CompilationPriority::Normal => self.normal.push_back(entry),
            CompilationPriority::Low => self.low.push_back(entry),
        }
    }

    /// Pop the highest-priority request **without** consulting its stamp.
    ///
    /// The raw form, kept for [`TieredCompilationManager::dequeue_compilation`]
    /// — a manual/diagnostic drain that is not the compile pipeline. A caller
    /// that is about to *compile* the result must use [`Self::dequeue_fresh`].
    fn dequeue(&mut self) -> Option<QueuedRequest> {
        let entry = self
            .high
            .pop_front()
            .or_else(|| self.normal.pop_front())
            .or_else(|| self.low.pop_front());
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
    ) -> Option<CompilationTask> {
        while let Some(entry) = self.dequeue() {
            if entry.install_epoch >= current {
                return Some(entry.task);
            }
            stale.push(StaleRequest {
                task: entry.task,
                event: crate::metrics::SCHEDULING_EVENTS[0],
                queued_epoch: entry.install_epoch,
                current_epoch: current,
            });
        }
        None
    }

    fn len(&self) -> usize {
        self.high.len() + self.normal.len() + self.low.len()
    }

    /// Rank of the request [`Self::dequeue`] would hand out next: 2 for High,
    /// 1 for Normal, 0 for Low, `None` when empty.
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

    /// Remove and return every queued task `pred` selects, keeping the order
    /// of what remains.
    fn remove_matching(&mut self, pred: impl Fn(&CompilationTask) -> bool) -> Vec<CompilationTask> {
        let mut removed = Vec::new();
        for band in [&mut self.high, &mut self.normal, &mut self.low] {
            band.retain(|entry| {
                if pred(&entry.task) {
                    removed.push(entry.task.clone());
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
    fn drain_all(&mut self) -> Vec<CompilationTask> {
        self.high
            .drain(..)
            .chain(self.normal.drain(..))
            .chain(self.low.drain(..))
            .map(|entry| entry.task)
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
}

impl CompileLane {
    fn new() -> Self {
        Self {
            queue: Mutex::new(CompilationQueue::new()),
            wake: Condvar::new(),
        }
    }
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
    /// What the C2 budget is charged: compile-thread CPU time where the
    /// platform reports it, wall time otherwise.
    budget_ms: u64,
    success: bool,
    declined_permanently: bool,
}

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
    /// Compiles whose `compile_fn` panicked and was contained.
    worker_panics: AtomicU64,
    /// Whether the first contained panic has been logged (later ones are only
    /// counted).
    panic_logged: AtomicBool,
    /// Install epoch of the most recently dispatched compile still running, or
    /// `0` when every worker is idle.
    ///
    /// The dispatch-time gate can only refuse a request whose epoch had
    /// *already* moved. This records the epoch a running compile was
    /// dispatched at, so a worker can tell afterwards whether the world moved
    /// underneath it — the window that only the per-cache flush barrier in
    /// `JitCache::put` can close, and the one this field makes visible. With
    /// several workers it is a sample, not a per-compile record.
    inflight_epoch: AtomicU64,
    /// Compiles currently running across all workers.
    inflight_count: AtomicU64,
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
    /// Test seam: when set, the source of "the current install epoch" instead
    /// of the process-wide [`crate::jit_install_epoch`].
    ///
    /// The scheduling rule under test is "a request queued before an
    /// invalidation is not compiled after it", and that is a statement about
    /// epoch *ordering*, not about wall-clock time or about any real
    /// redefinition. Driving it from an injected counter makes the test
    /// deterministic and hermetic; driving it from the global would make it
    /// depend on whatever every other test in the process happened to flush.
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
    /// Aggregate compilation statistics (shared so the workers can update them).
    stats: CompilationStats,
}

/// The invocation count the C1→C2 supersede should require, or `0` for "no
/// gate" (the default, and what every build did before this existed).
///
/// Returns non-zero only when the operator actually asked, which is the whole
/// point: see [`CompilerCore::c2_upgrade_min_invocations`]. The env var is read
/// here rather than inferred from `policy.c2_threshold`, because a policy that
/// happens to equal the default is indistinguishable from one nobody set — and
/// "the operator set this knob" is exactly the distinction being made.
fn c2_upgrade_gate(policy: &CompilationPolicy) -> u64 {
    if cratonvm_types::flags::runtime_var_os("CRATONVM_TIER_C2_THRESHOLD").is_some() {
        u64::from(policy.c2_threshold)
    } else {
        0
    }
}

impl CompilerCore {
    fn with_install_epoch_source(install_epoch_source: Option<Arc<AtomicU64>>) -> Self {
        Self {
            methods: Mutex::new(FxHashMap::default()),
            lanes: [CompileLane::new(), CompileLane::new()],
            active: AtomicBool::new(false),
            shutdown: AtomicBool::new(false),
            completed: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            deduplicated: AtomicU64::new(0),
            worker_panics: AtomicU64::new(0),
            panic_logged: AtomicBool::new(false),
            inflight_epoch: AtomicU64::new(0),
            inflight_count: AtomicU64::new(0),
            settled_generation: AtomicU32::new(1),
            branch_window_balance: AtomicI64::new(0),
            osr_denied: RwLock::new(FxHashMap::default()),
            install_epoch_source,
            // 0 = no gate. Raised by the manager's constructor and by
            // `set_policy` only when the operator set the knob explicitly.
            c2_upgrade_min_invocations: AtomicU64::new(0),
            stats: CompilationStats::default(),
        }
    }

    /// The install epoch as of right now.
    ///
    /// One atomic load on the default path — the same load
    /// `crate::jit_install_epoch` performs, which is already on every
    /// compilation's entry path.
    fn current_install_epoch(&self) -> u64 {
        match &self.install_epoch_source {
            Some(cell) => cell.load(Ordering::Acquire),
            None => crate::jit_install_epoch(),
        }
    }

    /// Push a task onto its lane, stamped with the install epoch it was queued
    /// at, and wake one of that lane's workers.
    ///
    /// Every enqueue funnels through here (via [`Self::admit`]), so no door
    /// into either queue can produce an unstamped request.
    fn enqueue(&self, task: CompilationTask) {
        let epoch = self.current_install_epoch();
        let lane = &self.lanes[lane_of(&task)];
        lane.queue.lock().enqueue(task, epoch);
        lane.wake.notify_one();
    }

    /// Give `task` the method's in-flight slot and queue it, or refuse it.
    /// Returns whether it was queued.
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
    fn admit(&self, state: &mut MethodState, task: CompilationTask) -> bool {
        if state.queued_for_compilation {
            self.deduplicated.fetch_add(1, Ordering::Relaxed);
            crate::metrics::record_scheduling_event(crate::metrics::SCHEDULING_EVENTS[4]);
            return false;
        }
        state.queued_for_compilation = true;
        state.queued_tier = Some(task.target_tier);
        state.queued_osr_bci = task.osr_bci;
        self.enqueue(task);
        true
    }

    /// Release the in-flight slot for a request leaving by any exit, closing
    /// the branch-profile window it opened.
    ///
    /// A request that no longer holds the slot does not release it. That
    /// happens when a deopt dropped a queued request, the slot was re-granted,
    /// and the OLD request is only now reaching completion or retirement:
    /// clearing the flag then would admit a duplicate beside the new request.
    fn release_slot_for(
        &self,
        state: &mut MethodState,
        tier: CompilationTier,
        osr_bci: Option<u32>,
    ) {
        if state.queued_for_compilation
            && (state.queued_tier != Some(tier) || state.queued_osr_bci != osr_bci)
        {
            return;
        }
        state.queued_for_compilation = false;
        state.queued_tier = None;
        state.queued_osr_bci = None;
        if std::mem::take(&mut state.branch_window_armed) {
            self.close_branch_window();
        }
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
        {
            let mut methods = self.methods.lock();
            for request in stale {
                if let Some(state) = methods.get_mut(&request.task.method_key) {
                    self.release_slot_for(state, request.task.target_tier, request.task.osr_bci);
                }
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
                }),
                _ => return Some(entry.task),
            }
        }
    }

    /// Remove every queued (not yet dispatched) request for `key` from both
    /// lanes. Call with `methods` held.
    fn remove_queued_for(&self, key: &MethodKey) -> Vec<CompilationTask> {
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

    fn note_dispatch(&self, epoch: u64) {
        self.inflight_count.fetch_add(1, Ordering::AcqRel);
        self.inflight_epoch.store(epoch, Ordering::Release);
    }

    fn note_dispatch_done(&self) {
        if self.inflight_count.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.inflight_epoch.store(0, Ordering::Release);
        }
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
    pub(crate) fn request_deferred_new_retry(&self, key: &MethodKey) {
        let dbg = cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_JITC");
        macro_rules! refuse {
            ($why:expr) => {{
                if dbg {
                    eprintln!(
                        "[cratonvm-jitc] deferred-new retry REFUSED {}.{}{}: {}",
                        key.class_name, key.method_name, key.descriptor, $why
                    );
                }
                return;
            }};
        }
        let mut methods = self.methods.lock();
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
            refuse!("already queued");
        }
        if state.c2_bailout {
            refuse!("c2_bailout");
        }
        if state.ineligible {
            refuse!("ineligible");
        }
        if state.tier_fail_count >= MAX_TIER_FAIL_RETRIES {
            refuse!("tier_fail_count exhausted");
        }
        let gate = self.c2_upgrade_min_invocations.load(Ordering::Relaxed);
        if gate != 0 && state.invocation_count < gate {
            refuse!("below the c2-upgrade hotness gate");
        }
        if dbg {
            eprintln!(
                "[cratonvm-jitc] deferred-new retry ENQUEUED {}.{}{}",
                key.class_name, key.method_name, key.descriptor
            );
        }
        // Open the branch-profile window: there is now an optimizing compile
        // pending that will read the counts. Closed by whichever exit the
        // request takes -- see `MethodState::branch_window_armed`.
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
        );
    }

    /// C1→C2 supersede: enqueue a Low-priority C2 recompile for a method
    /// whose C1 body just published. Idempotent: skipped when the method is
    /// already queued, already at C2, has bailed out of C2, or has exhausted
    /// its compile retries. Called by the worker loop AFTER [`Self::finish`]
    /// released the C1 task's slot.
    fn request_c2_upgrade(&self, key: &MethodKey) {
        let mut methods = self.methods.lock();
        let Some(state) = methods.get_mut(key) else {
            return;
        };
        if state.queued_for_compilation
            || state.current_tier >= CompilationTier::C2
            || state.c2_bailout
            || state.ineligible
            || state.tier_fail_count >= MAX_TIER_FAIL_RETRIES
        {
            return;
        }
        // Hotness gate, off unless the operator set
        // `CRATONVM_TIER_C2_THRESHOLD`. Every other door to C2 asks for
        // `invocation_count >= c2_threshold`; this one asked for nothing,
        // which is what made that knob inert for the path that produces
        // nearly all C2 compiles. See the field for why the default stays
        // ungated. A method held back here is not held back forever:
        // `should_compile`'s own C2 arm admits it once it really is that hot.
        let gate = self.c2_upgrade_min_invocations.load(Ordering::Relaxed);
        if gate != 0 && state.invocation_count < gate {
            return;
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
        );
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
                budget_ms: compile_time_ms,
                success,
                declined_permanently,
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
    /// tiering must not be suppressed by an OSR artifact. Slots and
    /// fail counters still clear/advance normally, so both pipelines share
    /// the single in-flight slot.
    fn finish(&self, key: &MethodKey, c: Completion) {
        {
            let mut methods = self.methods.lock();
            if let Some(state) = methods.get_mut(key) {
                if c.success {
                    if !c.osr {
                        state.current_tier = c.tier;
                    }
                    state.tier_fail_count = 0;
                } else if c.declined_permanently {
                    // Policy verdict, not a compile failure: the skip list, the
                    // OSR-denial set and a contained panic are verdicts that
                    // re-asking cannot change. Record it once and stop; do NOT
                    // spend `tier_fail_count`, which exists to bound genuinely
                    // failing codegen. See `MethodState::ineligible`.
                    state.ineligible = true;
                } else {
                    state.tier_fail_count = state.tier_fail_count.saturating_add(1);
                }
                self.release_slot_for(state, c.tier, c.osr_bci);
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
            }
        }
        if c.success {
            match c.tier {
                CompilationTier::C1 | CompilationTier::C1WithProfiling => {
                    self.stats.c1_compilations.fetch_add(1, Ordering::Relaxed);
                }
                CompilationTier::C2 => {
                    self.stats.c2_compilations.fetch_add(1, Ordering::Relaxed);
                }
                _ => {}
            }
        }
        self.stats
            .total_compile_time_ms
            .fetch_add(c.compile_time_ms, Ordering::Relaxed);
        self.completed.fetch_add(1, Ordering::Release);
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
        }
    }
}

/// A compile callback invoked on a worker thread for each drained task.
/// The actual codegen is supplied by the VM at startup; `tiered.rs` only owns
/// the scheduling. `Sync` because every worker of both lanes shares one.
pub type CompileFn = Box<dyn Fn(&CompilationTask) -> CompileOutcome + Send + Sync + 'static>;

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
        let abandoned: Vec<CompilationTask> = self
            .core
            .lanes
            .iter()
            .flat_map(|lane| lane.queue.lock().drain_all())
            .collect();
        if !abandoned.is_empty() {
            let stale: Vec<StaleRequest> = abandoned
                .into_iter()
                .map(|task| StaleRequest {
                    task,
                    event: crate::metrics::SCHEDULING_EVENTS[3],
                    queued_epoch: 0,
                    current_epoch: 0,
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
static DIAG_CORE: std::sync::OnceLock<Arc<CompilerCore>> = std::sync::OnceLock::new();
/// Diagnostic-only snapshot of the active policy's `c1_threshold`, captured
/// at [`TieredCompilationManager::new`] — used by
/// [`dump_method_stats_to_stderr`] to flag methods that crossed the
/// promotion threshold but never actually got promoted.
static DIAG_C1_THRESHOLD: AtomicU64 = AtomicU64::new(0);

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
    //   * `osr-contract-violations` / `osr-coordinate-mismatches` — both MUST
    //     be 0. Non-zero means an artifact's OSR metadata contradicted itself
    //     and was dropped, so the method silently lost OSR service.
    {
        use crate::compile_gate::{admissions, refusals, ungated_backend_entries, CompileDoor};
        let doors: Vec<String> = CompileDoor::ALL
            .iter()
            .map(|d| {
                format!(
                    "{}: admitted={} refused={}",
                    d.label(),
                    admissions(*d),
                    refusals(*d)
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
            "[cratonvm] compiled-frame lines: {} | inline-map-at-return={:?}              miss-edge-poison={:?}",
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
    let Some(core) = DIAG_CORE.get() else {
        return;
    };
    let c1_threshold = DIAG_C1_THRESHOLD.load(Ordering::Relaxed);
    let mut snap = MethodPromotionSnapshot::default();
    // (invocation_count, queued_for_compilation, tier_fail_count, ineligible,
    // name) for every Interpreter-tier method whose invocation_count already
    // crossed c1_threshold. Splitting `ineligible` out matters: a method the
    // skip list refuses is stuck BY DESIGN and is not evidence of anything,
    // whereas one with a non-zero `tier_fail_count` is a compiler failure.
    // Reporting both as `tier_fail_count=3` is what made an earlier
    // "1531 of 1642 hot methods never compile" reading unactionable.
    // (invocations, queued, tier_fail_count, ineligible, display name, the
    // refusal site the compiler recorded for it — see
    // crate::jit_bail_reason_for).
    let mut hot_but_stuck: Vec<(u64, bool, u32, bool, String, String)> = Vec::new();
    let mut ineligible_by_policy: u64 = 0;
    {
        let methods = core.methods.lock();
        for state in methods.values() {
            snap.distinct_methods += 1;
            snap.total_invocations += state.invocation_count;
            if state.invocation_count > 0 {
                snap.methods_ever_invoked += 1;
            }
            match state.current_tier {
                CompilationTier::Interpreter => {
                    snap.methods_still_interpreted += 1;
                    if state.invocation_count >= c1_threshold {
                        if state.ineligible {
                            ineligible_by_policy += 1;
                        }
                        hot_but_stuck.push((
                            state.invocation_count,
                            state.queued_for_compilation,
                            state.tier_fail_count,
                            state.ineligible,
                            format!(
                                "{}.{}{}",
                                state.method_key.class_name,
                                state.method_key.method_name,
                                state.method_key.descriptor
                            ),
                            crate::jit_bail_reason_for(
                                state.method_key.class_id,
                                &state.method_key.class_name,
                                &state.method_key.method_name,
                                &state.method_key.descriptor,
                            )
                            .unwrap_or_else(|| "unrecorded".to_string()),
                        ));
                    }
                }
                CompilationTier::C1 | CompilationTier::C1WithProfiling => snap.methods_at_c1 += 1,
                CompilationTier::FullProfile => snap.methods_at_full_profile += 1,
                CompilationTier::C2 => snap.methods_at_c2 += 1,
            }
        }
    }
    let stats = &core.stats;
    eprintln!(
        "[cratonvm] JIT method stats: {} distinct methods tracked, {} ever invoked, {} total invocations \
         | still-interpreted={} c1={} full-profile={} c2={} \
         | compiles: c1={} c2={} osr={} deopts={} c2_bailouts={} total_compile_time_ms={} \
         | code_buffer_bails={} (discarded_compile_ms={}) \
         | inline_live_slot_clamps={} inline_locals_floor_bumps={} \
         | c1_threshold={} hot_but_stuck_in_interpreter={} (of which ineligible-by-policy={}, compile-failures={})",
        snap.distinct_methods,
        snap.methods_ever_invoked,
        snap.total_invocations,
        snap.methods_still_interpreted,
        snap.methods_at_c1,
        snap.methods_at_full_profile,
        snap.methods_at_c2,
        stats.c1_compilations.load(Ordering::Relaxed),
        stats.c2_compilations.load(Ordering::Relaxed),
        stats.osr_compilations.load(Ordering::Relaxed),
        stats.deoptimizations.load(Ordering::Relaxed),
        stats.c2_bailouts.load(Ordering::Relaxed),
        stats.total_compile_time_ms.load(Ordering::Relaxed),
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
    );
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
            "[cratonvm] JIT invokevirtual pinned non-virtual: private={private_pinned}              final={final_pinned}",
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

impl TieredCompilationManager {
    /// Create a new manager with the given policy.
    pub fn new(policy: CompilationPolicy) -> Self {
        Self::with_install_epoch_source(policy, None)
    }

    /// [`Self::new`], but reading "the current install epoch" from `source`
    /// instead of the process-wide [`crate::jit_install_epoch`].
    ///
    /// The seam that makes the stale-request drop testable without a clock,
    /// without a sleep, and without a real class redefinition: the rule under
    /// test is an ordering statement about epochs, so a test bumps the
    /// injected counter exactly where a redefinition would have bumped the
    /// global one and asserts on what the queue then does. Driving it from the
    /// real global would make the test depend on every other test in the
    /// process that happens to flush a `JitCache`.
    ///
    /// `None` is the production configuration and is what [`Self::new`] passes.
    #[doc(hidden)]
    pub fn with_install_epoch_source(
        policy: CompilationPolicy,
        install_epoch_source: Option<Arc<AtomicU64>>,
    ) -> Self {
        // Seed the process-start timestamp as early as possible (this
        // manager is constructed during VM init) so `process_uptime_ms`
        // reports genuine process age, not "time since first compile".
        process_start();
        let core = Arc::new(CompilerCore::with_install_epoch_source(
            install_epoch_source,
        ));
        // Diagnostic-only: the first manager in the process is the one
        // `dump_method_stats_to_stderr` reports. See `DIAG_CORE`.
        let _ = DIAG_CORE.set(core.clone());
        DIAG_C1_THRESHOLD.store(policy.c1_threshold as u64, Ordering::Relaxed);
        core.c2_upgrade_min_invocations
            .store(c2_upgrade_gate(&policy), Ordering::Relaxed);
        Self {
            core,
            policy: Mutex::new(policy),
            background: Mutex::new(None),
        }
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
        // And the process-wide compile verdicts about the class (bail list,
        // refusal reasons, OSR entry rejects): a class that is gone must not
        // leave refusals behind for the next class loaded under its id or name.
        // By identity, so a same-named class in another loader keeps its own.
        crate::forget_jit_verdicts_for_class(class_id, class_name);
        for _ in 0..windows_to_close {
            self.core.close_branch_window();
        }
        if dropped > 0 {
            crate::metrics::record_scheduling_events(crate::metrics::SCHEDULING_EVENTS[1], dropped);
            self.core.dropped.fetch_add(dropped, Ordering::Release);
        }
        self.core.bump_settled_generation();
    }

    /// Forget the verdicts recorded against a class whose bytecode was just
    /// replaced — a JVMTI redefine or retransform, or a `defineClass` over an
    /// already-loaded name.
    ///
    /// `ineligible`, `tier_fail_count`, `c2_bailout`, the trap counts and any
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
                state.ineligible = false;
                state.c2_bailout = false;
                state.c2_budget_overruns = 0;
                state.deopt_count = 0;
                state.trap_counts.clear();
            }
        }
        self.core
            .osr_denied
            .write()
            .retain(|key, _| !key.belongs_to(class_id, class_name));
        // The compile verdicts were measured on the old bytecode too. They
        // already stop counting once the redefine epoch moves; dropping them
        // also releases the memory.
        crate::forget_jit_verdicts_for_class(class_id, class_name);
        self.core.bump_settled_generation();
    }

    /// Create a new manager with the default policy.
    pub fn with_default_policy() -> Self {
        Self::new(CompilationPolicy::default())
    }

    /// wire-tiered-manager Step 6: create a manager whose policy honors the
    /// `CRATONVM_TIER_*` environment overrides (see [`CompilationPolicy::from_env`]).
    /// Used by `SharedVm::new`; an unset environment is identical to
    /// [`with_default_policy`].
    pub fn with_env_policy() -> Self {
        Self::new(CompilationPolicy::from_env())
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
        // Read BEFORE deciding, so a concurrent bump (a deopt landing while
        // this runs) expires the stamp this call hands out.
        let generation = self.core.settled_generation.load(Ordering::Acquire);
        let mut methods = self.core.methods.lock();
        let state = methods
            .entry(key.clone())
            .or_insert_with(|| MethodState::new(key.clone()));
        state.invocation_count = state.invocation_count.saturating_add(1);
        if observed_count > state.invocation_count {
            state.invocation_count = observed_count;
        }

        if state.queued_for_compilation {
            return InvocationVerdict {
                recommended: None,
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

        let recommended = self.should_compile_inner(state, &policy);
        let settled = recommended.is_none() && state.tiering_is_settled();
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
    pub fn request_deferred_new_retry(&self, key: &MethodKey) {
        self.core.request_deferred_new_retry(key);
    }

    /// wire-tiered-manager Step 5 (precise background OSR): request an OSR
    /// compilation for a method whose loop the *caller* has already judged hot.
    ///
    /// This enqueues **immediately** — the interpreter's per-frame back-edge
    /// schedule (`Frame::should_try_osr`) is the throttle. (A counting
    /// `on_backedge` twin that fired at `policy.osr_threshold` existed beside
    /// this with no production caller and an unreachable branch; it was
    /// deleted.) It is idempotent: a no-op (returns `None`) if the method is
    /// already queued, has bailed out of C2, or its OSR is denied at the
    /// current install epoch. Method-entry C2 does not suppress this request
    /// because OSR bodies live in an independent cache. The enqueued task
    /// carries `osr_bci` so the background worker compiles an OSR-enterable
    /// artifact; the mutator enters it once published.
    pub fn request_osr(&self, key: &MethodKey, bci: u32) -> Option<CompilationTask> {
        if self.core.is_osr_denied(key) {
            return None;
        }
        let mut methods = self.core.methods.lock();
        let state = methods
            .entry(key.clone())
            .or_insert_with(|| MethodState::new(key.clone()));
        state.backedge_count = state.backedge_count.saturating_add(1);

        let policy = self.policy.lock();
        if !policy.tiered_enabled {
            return None;
        }
        if state.queued_for_compilation
            || state.c2_bailout
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
            || state.ineligible
            || state.tier_fail_count >= MAX_TIER_FAIL_RETRIES
        {
            return None;
        }

        let task = CompilationTask {
            method_key: key.clone(),
            target_tier: CompilationTier::C2,
            priority: CompilationPriority::High,
            enqueue_time_ms: 0,
            osr_bci: Some(bci),
        };
        if !self.core.admit(state, task.clone()) {
            return None;
        }
        self.core
            .stats
            .osr_compilations
            .fetch_add(1, Ordering::Relaxed);
        Some(task)
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

    /// Enqueue a compilation task. Returns whether it was queued: a method that
    /// already holds the in-flight slot is refused (see `CompilerCore::admit`).
    pub fn enqueue_compilation(&self, task: CompilationTask) -> bool {
        let mut methods = self.core.methods.lock();
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
    ///    offered to C2 once one `(reason, bci)` reaches
    ///    [`PER_BCI_TRAP_LIMIT`] or the whole method reaches
    ///    [`PER_METHOD_TRAP_CUTOFF`]. Both counts halve over time
    ///    ([`TRAP_DECAY_INVOCATIONS`], [`TRAP_DECAY_MS`]), so a burst early
    ///    in a long run does not ban the method for the rest of it.
    ///  * `MakeNotCompilable` also records the method as ineligible: the VM
    ///    has just put it on its skip list.
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
        {
            let mut methods = self.core.methods.lock();
            if let Some(state) = methods.get_mut(key) {
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
                    for task in &removed {
                        self.core
                            .release_slot_for(state, task.target_tier, task.osr_bci);
                    }
                    dropped = removed.len() as u64;
                    if action == DeoptAction::MakeNotCompilable {
                        state.ineligible = true;
                    }
                    if (at_site >= PER_BCI_TRAP_LIMIT
                        || state.deopt_count >= PER_METHOD_TRAP_CUTOFF)
                        && !state.c2_bailout
                    {
                        state.c2_bailout = true;
                        self.core.stats.c2_bailouts.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }
        if dropped > 0 {
            crate::metrics::record_scheduling_events(crate::metrics::SCHEDULING_EVENTS[5], dropped);
            self.core.dropped.fetch_add(dropped, Ordering::Release);
        }
        self.core.bump_settled_generation();
    }

    /// Notify that C2 compilation bailed out (method too complex, etc.).
    /// Drops any request still queued for the method, as a counted trap does.
    pub fn on_c2_bailout(&self, key: &MethodKey) {
        let mut dropped = 0u64;
        {
            let mut methods = self.core.methods.lock();
            if let Some(state) = methods.get_mut(key) {
                state.c2_bailout = true;
                let removed = self.core.remove_queued_for(key);
                for task in &removed {
                    self.core
                        .release_slot_for(state, task.target_tier, task.osr_bci);
                }
                dropped = removed.len() as u64;
            }
        }
        if dropped > 0 {
            crate::metrics::record_scheduling_events(crate::metrics::SCHEDULING_EVENTS[5], dropped);
            self.core.dropped.fetch_add(dropped, Ordering::Release);
        }
        self.core.stats.c2_bailouts.fetch_add(1, Ordering::Relaxed);
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
    /// Equal to [`crate::jit_install_epoch`] in production. The accessor
    /// exists so a diagnostic can print the number the dispatch gate is
    /// actually comparing against, rather than a number that is usually the
    /// same one.
    pub fn install_epoch(&self) -> u64 {
        self.core.current_install_epoch()
    }

    /// Install epoch of a compile running right now, or `0` when idle.
    pub fn inflight_install_epoch(&self) -> u64 {
        self.core.inflight_epoch.load(Ordering::Acquire)
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

    /// Requests refused because their method already held the in-flight slot.
    pub fn deduplicated_requests(&self) -> u64 {
        self.core.deduplicated.load(Ordering::Acquire)
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
        for (lane, workers) in [(C1_LANE, c1_workers), (C2_LANE, c2_workers)] {
            let mut spawned = 0usize;
            for _ in 0..workers {
                let core = Arc::clone(&self.core);
                let compile_fn = Arc::clone(&compile_fn);
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
                        Self::compiler_loop(&core, lane, &compile_fn);
                    });
                match spawn {
                    Ok(handle) => {
                        background.handles.push(handle);
                        spawned += 1;
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
            let task = loop {
                let mut stale: Vec<StaleRequest> = Vec::new();
                let mut shutting_down = false;
                let popped = {
                    let mut q = lane_ref.queue.lock();
                    loop {
                        if core.shutdown.load(Ordering::Acquire) {
                            shutting_down = true;
                            break None;
                        }
                        let current = core.current_install_epoch();
                        match q.dequeue_fresh(current, &mut stale) {
                            Some(task) => break Some(task),
                            // Nothing fresh AND something to retire: give up
                            // the queue lock so the slots can be released
                            // under `methods`, then come back around.
                            None if !stale.is_empty() => break None,
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
                if let Some(task) = popped {
                    break task;
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
            let dispatch_epoch = core.current_install_epoch();
            core.note_dispatch(dispatch_epoch);
            let cpu_before = current_thread_cpu_time();
            let result = contain_compile_panic(|| compile_fn(&task));
            let cpu_ms = match (cpu_before, current_thread_cpu_time()) {
                (Some(before), Some(after)) => {
                    Some(after.saturating_sub(before).as_millis() as u64)
                }
                _ => None,
            };
            core.note_dispatch_done();
            if core.current_install_epoch() != dispatch_epoch {
                crate::metrics::record_scheduling_event(crate::metrics::SCHEDULING_EVENTS[2]);
            }
            let outcome = match result {
                Ok(outcome) => outcome,
                Err(payload) => {
                    core.note_worker_panic(&task, &*payload);
                    CompileOutcome::declined(0)
                }
            };
            core.finish(
                &task.method_key,
                Completion {
                    tier: task.target_tier,
                    osr_bci: task.osr_bci,
                    osr: task.osr_bci.is_some(),
                    compile_time_ms: outcome.compile_time_ms,
                    budget_ms: cpu_ms.unwrap_or(outcome.compile_time_ms),
                    success: outcome.published,
                    declined_permanently: outcome.declined_permanently,
                },
            );
            // C1→C2 supersede: a freshly-published C1-family body whose
            // method the VM judged IR-eligible gets a Low-priority C2
            // recompile. Enqueued AFTER `finish` so the C1 task's slot has
            // been released (otherwise the idempotence gate would drop the
            // upgrade). OSR tasks are excluded (their artifacts serve loop
            // entry; the invocation path re-tiers separately), as are tasks
            // already at an optimized tier.
            if outcome.published
                && outcome.c2_upgrade_candidate
                && task.osr_bci.is_none()
                && !tier_uses_optimized_backend(task.target_tier)
            {
                core.request_c2_upgrade(&task.method_key);
            }
            // The deferred-`new` retry, which deliberately does NOT carry the
            // "not already optimized" clause above: the bail it answers happens
            // inside a C2 task that then fell through to the single-pass
            // backend, so by the time we get here the method is recorded as
            // done with C2 and `request_c2_upgrade` would refuse it for exactly
            // that reason. Bounded by the one-shot memo in `lib.rs` — a method
            // is armed once, and a class that never loads cannot re-arm it.
            if outcome.published && outcome.deferred_new_retry && task.osr_bci.is_none() {
                core.request_deferred_new_retry(&task.method_key);
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
                "[cratonvm-tier] enqueue {}.{}{} tier={:?} invocations={} elapsed_ms={}",
                state.method_key.class_name,
                state.method_key.method_name,
                state.method_key.descriptor,
                target,
                state.invocation_count,
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
        // permanent bail-list — e.g. a transient code-cache-cap or in-flight
        // class redefine — would be re-recommended and re-enqueued on every
        // single invocation forever, since a failed attempt no longer
        // advances `current_tier`. A policy decline is permanent by
        // construction, so one is enough — unlike `tier_fail_count`, which
        // deliberately allows retries.
        if state.ineligible {
            return None;
        }
        if state.tier_fail_count >= MAX_TIER_FAIL_RETRIES {
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
        // The task really is on the queue, and the OSR stat counted exactly once.
        let dq = mgr
            .dequeue_compilation()
            .expect("an OSR task should be queued");
        assert_eq!(dq.osr_bci, Some(42));
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
            assert_eq!(mgr.next_fresh_task(), Some(task));
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

    /// Soft deopts are not charged. A soft OSR exit keeps its body, so it does
    /// not move the tier either; a debugger transfer or a pending exception's
    /// hand-off evicts the body but is not a speculation failure.
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
                CompilationTier::Interpreter,
                "an evicted body drops the tier even when the deopt is not charged"
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
                    budget_ms: 1,
                    success: true,
                    declined_permanently: false,
                },
            );
        }
        assert!(!mgr.core.methods.lock()[&key].c2_bailout);
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
        assert_eq!(mgr.stats().osr_compilations.load(Ordering::Relaxed), 1);
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

    #[test]
    fn a_request_queued_at_the_current_epoch_dispatches() {
        let epoch = Arc::new(AtomicU64::new(7));
        let mgr = epoch_driven_manager(&epoch);
        let key = epoch_key("stillCurrent");
        mgr.enqueue_compilation(c1_task(&key));

        assert_eq!(mgr.install_epoch(), 7);
        assert_eq!(mgr.next_fresh_task(), Some(c1_task(&key)));
        assert_eq!(mgr.dropped_requests(), 0);
        assert_eq!(mgr.queue_size(), 0);
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
        assert_eq!(mgr.next_fresh_task(), None);

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
        assert_eq!(mgr.next_fresh_task(), Some(c1_task(&key)));
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

        assert_eq!(
            mgr.next_fresh_task(),
            Some(c1_task(&fresh)),
            "the drop loop must skip past stale entries, not stop at the first one"
        );
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
        assert_eq!(mgr.next_fresh_task(), None);
        epoch.store(9, Ordering::Release);
        assert_eq!(mgr.next_fresh_task(), None);
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
        assert_eq!(mgr.next_fresh_task(), None);

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
        assert_eq!(mgr.next_fresh_task(), Some(c1_task(&survivor)));
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

    // ── Deduplication: one in-flight slot per method ─────────────────────

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
        assert_eq!(mgr.next_fresh_task(), Some(c1_task(&key)));
        assert!(!mgr.enqueue_compilation(c1_task(&key)));
        mgr.core
            .complete_task(&key, CompilationTier::C1, 1, true, false, false);
        assert!(mgr.enqueue_compilation(c2), "the slot frees on completion");
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
        assert_eq!(mgr.next_fresh_task(), None);
        assert_eq!(
            mgr.branch_window_balance(),
            0,
            "a stale drop closes the window"
        );

        // 2. An OSR C2 completion never opened one and must not close one.
        let osr = epoch_key("windowOsr");
        let task = mgr.request_osr(&osr, 4).expect("OSR request");
        assert_eq!(mgr.next_fresh_task(), Some(task));
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
        assert!(mgr.compiler_active());
        drop(bg);
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
}
