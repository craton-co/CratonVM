// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Whole-run phase accounting — where a benchmark's wall clock actually went.
//!
//! ## Why this exists
//!
//! The C2 review (`feature-designs/c2/deep-research-vm-c2.md`) has a P0 lane
//! "Separate startup, compilation, execution, and GC time", whose acceptance
//! criterion is: *"Every benchmark's wall time reconciles to named categories
//! within 2%."* Nothing in the tree could answer that. `jit::metrics` measures
//! the inside of one compilation, `gc::gc_metrics` counts cards and names the
//! collector decision, and `regression-suite/perf/` measures the wall clock —
//! but no artefact connects the three, and none of them can say what fraction
//! of a run is *unaccounted for*.
//!
//! [`report`] is the structured answer: a partition of each thread's timeline
//! into [`Category`] buckets plus an explicit **unattributed** remainder, with
//! a JSON sink for `regression-suite/perf/` and a JFR sink for JMC.
//!
//! ## The reconciliation identity
//!
//! For every thread, and by construction rather than by arithmetic afterwards:
//!
//! ```text
//! thread_wall_ns = Σ category_self_ns + open_ns + unattributed_ns
//! ```
//!
//! `unattributed_ns` is a **measurement**, not a residual that gets quietly
//! folded into "execution". A run where 30% of the clock is in no category is
//! a run this module reports as 30% unattributed — which is the finding. The
//! mirror-image failure (categories summing to *more* than the wall clock)
//! cannot be silently clamped either: it surfaces as
//! [`ThreadPhases::over_attributed_ns`], and any non-zero value there is a bug
//! in the wiring, not a property of the workload.
//!
//! ## Why double-counting is structurally impossible
//!
//! Phases nest: a class load happens inside interpretation, a GC happens
//! inside a class load, a deoptimization happens inside compiled execution.
//! Charging each span its full elapsed time would count the same nanoseconds
//! two or three times, and the totals would exceed the wall clock without
//! anything being wrong.
//!
//! This module charges **self time** only. Each thread keeps a stack of open
//! spans. When a span closes it hands its *whole* elapsed time to its parent's
//! `child_ns` accumulator and charges `elapsed - child_ns` to its own
//! category. Therefore, at any instant, a nanosecond on a thread is charged to
//! exactly one category — the innermost span open at that instant — or to
//! nothing at all, in which case it lands in `unattributed_ns`. There is no
//! code path that adds a duration to two categories, because there is no code
//! path that adds a duration to anything other than the frame being popped.
//!
//! The stack discipline is enforced, not assumed. Each frame carries a token;
//! [`PhaseSpan::drop`] finds its own frame by token. Dropping a span out of
//! order closes every frame above it (so their time is still charged exactly
//! once, to the right categories) and increments
//! [`Anomalies::out_of_order_closes`] so the report says the wiring is wrong.
//! A span that is never dropped charges nothing, and its time appears as
//! unattributed — visible, not absorbed.
//!
//! ## Compilation is delegated, not re-timed
//!
//! [`Category::Compilation`] is a single span around the compiler's entry
//! point. This module does **not** re-time `scan` / `build` / `optimize` /
//! `escape_analysis` / `verify` / `schedule` / `lower` / `single_pass` —
//! `jit::metrics::Phase` already does, and a second set of timers around the
//! same code would disagree with the first one and there would be no way to
//! say which was right. Instead the caller pushes `jit::metrics::summary()`'s
//! `phase_totals_ns` in through [`set_compilation_breakdown`], and the report
//! carries it as a **breakdown of** the `compilation` bucket together with
//! [`PhaseReport::compilation_delta_ns`], the disagreement between the two
//! measurements. That keeps `cratonvm-jfr` free of a `cratonvm-jit`
//! dependency and keeps one owner per number.
//!
//! ## Levels
//!
//! Splitting *interpreted* from *compiled* execution needs a span at every
//! method entry, which is exactly the kind of instrumentation that perturbs
//! the number it reports. So there are two levels:
//!
//! | Level | Executor spans | Cost |
//! |---|---|---|
//! | [`Level::Coarse`] | one [`Category::JavaExecution`] span around the whole Java run | negligible |
//! | [`Level::Fine`] | [`Category::Interpretation`] and [`Category::JitExecution`] per entry | two clock reads per method entry |
//!
//! At `Coarse`, `interpretation` and `jit_execution` are always `0` and the
//! combined figure is `java_execution`. At `Fine`, `java_execution` is always
//! `0` and dispatch overhead *between* an interpretation span and a JIT span
//! becomes unattributed — which is honest, and is why the two levels are
//! reported separately rather than blended.
//!
//! ## Cost when disabled
//!
//! Off unless the enable flag is set. [`enter`] then performs one relaxed
//! atomic load and returns a [`PhaseSpan`] whose `token` is `None`: no
//! allocation, no clock read, no thread-local touch, no registry lock. Its
//! `Drop` is an `Option` test that returns. This is the same shape as
//! `jit::metrics::CompileRecorder::begin`.
//!
//! ## Flags
//!
//! | Flag | Meaning |
//! |---|---|
//! | [`FLAG_ENABLE`]`=1`/`coarse` | enable at [`Level::Coarse`] (default off) |
//! | [`FLAG_ENABLE`]`=fine` | enable at [`Level::Fine`] |
//! | [`FLAG_JSON_OUT`]`=<path>` | write the JSON report there at [`emit_configured_sinks`] |
//! | [`FLAG_JFR_OUT`]`=<path>` | write a standalone JFR chunk there |
//!
//! Read through [`cratonvm_types::flags::runtime_var_os`], matching
//! `jit::metrics`. **These three names must be declared in
//! `types/src/flag_groups.rs` and `types/tests/flag-surface.txt`** — see
//! `docs/observability/phase-accounting.md` for the exact edit; the workspace
//! has a `flag_declaration_guard` test that fails on an undeclared
//! `CRATONVM_*` literal.
//!
//! ## Relationship to the rest of this crate
//!
//! This module shares no machinery with the JFR event rings. It takes no
//! per-thread event ring, does not consult [`crate::is_enabled`], and is not
//! registered by [`crate::create_flight_recorder`]. It lives here for the same
//! reason [`crate::jdk_only`] does: this crate is where "measurements of a run
//! that an operator may ask to have written out" belong. Its JFR sink is
//! standalone — [`write_jfr_report`] builds its own registry and calls
//! [`crate::dump::dump_to_file`] directly — so a phase report can be produced
//! by a run that never started a flight recording. [`register_phase_events`]
//! is public for the case where an operator *has* one and wants the phase
//! events in the same chunk.

use std::cell::RefCell;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use parking_lot::Mutex;

use crate::dump::JfrDumpError;
use crate::event::{
    EventField, EventFields, EventInstance, EventPeriod, EventType, EventTypeId, EventTypeRegistry,
    EventValue,
};
use crate::repository::EventRepository;

// ── Flags ────────────────────────────────────────────────────────────

/// Collection gate. `1`/`true`/`on`/`coarse` selects [`Level::Coarse`];
/// `fine` selects [`Level::Fine`]; anything else is off.
pub const FLAG_ENABLE: &str = "CRATONVM_PHASE_ACCOUNTING";

/// Path for the JSON report written by [`emit_configured_sinks`].
pub const FLAG_JSON_OUT: &str = "CRATONVM_PHASE_ACCOUNTING_OUT";

/// Path for the standalone JFR chunk written by [`emit_configured_sinks`].
pub const FLAG_JFR_OUT: &str = "CRATONVM_PHASE_ACCOUNTING_JFR";

/// Schema version of the JSON document produced by [`PhaseReport::to_json`].
///
/// Bumped whenever a key is removed or its meaning changes. Adding a key does
/// not bump it — consumers are expected to ignore unknown keys.
pub const PHASE_ACCOUNTING_SCHEMA_VERSION: u32 = 1;

/// The review's acceptance threshold: at most 2% of the measured wall clock
/// may be unattributed (or over-attributed) for a run to count as reconciled.
pub const DEFAULT_RECONCILIATION_TOLERANCE: f64 = 0.02;

/// Upper bound on tracked threads.
///
/// A workload that churns threads (a servlet container, a forked test runner)
/// would otherwise grow the registry without bound, and a diagnostic that
/// leaks is worse than a diagnostic that stops. Threads past the cap record
/// nothing and are counted in [`Anomalies::threads_dropped`], so their absence
/// is visible rather than silently shrinking the totals.
pub const MAX_TRACKED_THREADS: usize = 4096;

// ── Level ────────────────────────────────────────────────────────────

/// How much of the timeline is instrumented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// Nothing is recorded; every entry point is a load and a branch.
    Off,
    /// Everything except the per-method-entry executor split.
    Coarse,
    /// Also splits interpreted from compiled execution, at the cost of two
    /// clock reads per method entry.
    Fine,
}

impl Level {
    /// Stable metrics key.
    pub fn name(self) -> &'static str {
        match self {
            Level::Off => "off",
            Level::Coarse => "coarse",
            Level::Fine => "fine",
        }
    }
}

/// Tri-plus-state cache of the level flag: `0` unresolved, `1` off, `2`
/// coarse, `3` fine. An `AtomicU8` rather than a `OnceLock<Level>` so the test
/// helper can flip it; the read is a single relaxed load either way.
static LEVEL: AtomicU8 = AtomicU8::new(0);

fn resolve_level() -> Level {
    let Some(raw) = cratonvm_types::flags::runtime_var_os(FLAG_ENABLE) else {
        return Level::Off;
    };
    let Some(text) = raw.to_str() else {
        return Level::Off;
    };
    match text.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" | "coarse" => Level::Coarse,
        "2" | "fine" => Level::Fine,
        _ => Level::Off,
    }
}

fn level_code(level: Level) -> u8 {
    match level {
        Level::Off => 1,
        Level::Coarse => 2,
        Level::Fine => 3,
    }
}

/// The collection level. Latched on first read.
#[inline]
pub fn level() -> Level {
    match LEVEL.load(Ordering::Relaxed) {
        1 => Level::Off,
        2 => Level::Coarse,
        3 => Level::Fine,
        _ => {
            let resolved = resolve_level();
            LEVEL.store(level_code(resolved), Ordering::Relaxed);
            resolved
        }
    }
}

/// Whether anything is being recorded.
#[inline]
pub fn enabled() -> bool {
    !matches!(level(), Level::Off)
}

/// Whether the per-method-entry executor split is on.
///
/// Call sites on the method-entry hot path should test this *before* building
/// any argument to [`enter`], not only rely on [`enter`] returning a disabled
/// span.
#[inline]
pub fn fine_enabled() -> bool {
    matches!(level(), Level::Fine)
}

// ── Categories ───────────────────────────────────────────────────────

/// Number of [`Category`] variants. The width of every per-thread counter
/// array.
pub const CATEGORY_COUNT: usize = 16;

/// A named bucket of wall time.
///
/// The set is closed and the names are an external contract: they key the
/// JSON, the JFR event payloads, and the stderr summary line. Renaming a
/// variant must not rename [`Category::name`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Category {
    /// VM construction: heap reservation, bootstrap class loading, native
    /// registration, module graph — everything before the application's
    /// `main` is entered.
    VmStartup,
    /// Reading, parsing, defining and linking a class, plus running its
    /// `<clinit>`. Time spent *inside* `<clinit>` bytecode is charged to the
    /// executor categories, because the interpreter's own spans nest inside
    /// this one.
    ClassLoad,
    /// Bytecode verification (`classloading::verifier`). Split out of
    /// [`Category::ClassLoad`] because it is the part an operator can turn
    /// off.
    Verification,
    /// Interpreting bytecode. [`Level::Fine`] only.
    Interpretation,
    /// Executing JIT-compiled code. [`Level::Fine`] only.
    JitExecution,
    /// Interpreted and compiled execution, undivided. [`Level::Coarse`] only.
    JavaExecution,
    /// A mutator blocked waiting for a compilation it requested. Zero on a
    /// run whose compilation is entirely asynchronous — the background
    /// compiler's idle time is [`Category::Idle`] on the compiler's own
    /// thread, and is not mutator wall time.
    CompileQueueWait,
    /// Running the compiler. The per-phase breakdown comes from
    /// `jit::metrics` via [`set_compilation_breakdown`]; this bucket is not a
    /// second measurement of the same phases.
    Compilation,
    /// Publishing a compiled body: code-cache allocation, relocation, entry
    /// patching, inline-cache and vtable updates.
    CodeInstall,
    /// Deoptimization: frame reconstruction and the transfer back to the
    /// interpreter.
    Deoptimization,
    /// Stop-the-world collection pauses, including the time mutators spend
    /// blocked in them.
    GcPause,
    /// Concurrent collector work that runs alongside mutators (marking,
    /// refinement, sweeping).
    GcConcurrent,
    /// Time at a safepoint for something other than a GC (deopt storms,
    /// class redefinition, stack walks, jcmd).
    Safepoint,
    /// Inside a native method or a foreign downcall — the transition and the
    /// callee both.
    NativeCall,
    /// Parked with no work: the background compiler waiting on its condvar, a
    /// GC worker between cycles, a pooled thread between tasks. Real thread
    /// wall time that is deliberately not benchmark work.
    Idle,
    /// VM teardown after the application's `main` returns: shutdown hooks,
    /// finalization, report emission.
    VmShutdown,
}

impl Category {
    /// Every category, in report order. A category's position here is its
    /// index into every per-thread counter array.
    pub const ALL: [Category; CATEGORY_COUNT] = [
        Category::VmStartup,
        Category::ClassLoad,
        Category::Verification,
        Category::Interpretation,
        Category::JitExecution,
        Category::JavaExecution,
        Category::CompileQueueWait,
        Category::Compilation,
        Category::CodeInstall,
        Category::Deoptimization,
        Category::GcPause,
        Category::GcConcurrent,
        Category::Safepoint,
        Category::NativeCall,
        Category::Idle,
        Category::VmShutdown,
    ];

    /// Stable metrics key.
    pub fn name(self) -> &'static str {
        match self {
            Category::VmStartup => "vm_startup",
            Category::ClassLoad => "class_load",
            Category::Verification => "verification",
            Category::Interpretation => "interpretation",
            Category::JitExecution => "jit_execution",
            Category::JavaExecution => "java_execution",
            Category::CompileQueueWait => "compile_queue_wait",
            Category::Compilation => "compilation",
            Category::CodeInstall => "code_install",
            Category::Deoptimization => "deoptimization",
            Category::GcPause => "gc_pause",
            Category::GcConcurrent => "gc_concurrent",
            Category::Safepoint => "safepoint",
            Category::NativeCall => "native_call",
            Category::Idle => "idle",
            Category::VmShutdown => "vm_shutdown",
        }
    }

    /// Index into [`Category::ALL`] and into the per-thread counter arrays.
    pub fn index(self) -> usize {
        match self {
            Category::VmStartup => 0,
            Category::ClassLoad => 1,
            Category::Verification => 2,
            Category::Interpretation => 3,
            Category::JitExecution => 4,
            Category::JavaExecution => 5,
            Category::CompileQueueWait => 6,
            Category::Compilation => 7,
            Category::CodeInstall => 8,
            Category::Deoptimization => 9,
            Category::GcPause => 10,
            Category::GcConcurrent => 11,
            Category::Safepoint => 12,
            Category::NativeCall => 13,
            Category::Idle => 14,
            Category::VmShutdown => 15,
        }
    }

    /// Resolve a category from its stable key.
    pub fn from_name(name: &str) -> Option<Category> {
        Category::ALL.iter().copied().find(|c| c.name() == name)
    }

    /// Whether this category is recorded at `level`.
    ///
    /// The executor split is level-dependent and mutually exclusive: at
    /// [`Level::Coarse`] only [`Category::JavaExecution`] is live, at
    /// [`Level::Fine`] only [`Category::Interpretation`] and
    /// [`Category::JitExecution`] are. A call site may therefore open all
    /// three unconditionally; at most the level-appropriate one records.
    pub fn active_at(self, level: Level) -> bool {
        match level {
            Level::Off => false,
            Level::Coarse => !matches!(self, Category::Interpretation | Category::JitExecution),
            Level::Fine => !matches!(self, Category::JavaExecution),
        }
    }
}

// ── Process epoch ────────────────────────────────────────────────────

fn epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

fn since_epoch_ns(at: Instant) -> u64 {
    let nanos = at.saturating_duration_since(epoch()).as_nanos();
    nanos.min(u64::MAX as u128) as u64
}

/// Seed the process epoch and claim the calling thread as the reconciliation
/// basis.
///
/// Call this once, as early in `main` as the flags allow. Without it the
/// epoch is seeded by the first recorded span, so `process_wall_ns` measures
/// "since the first phase" rather than "since process start" and the
/// reconciliation basis is whichever thread happened to record first.
///
/// A no-op when collection is off — deliberately, so the disabled path stays
/// free of a clock read.
pub fn mark_process_start() {
    if !enabled() {
        return;
    }
    let _ = epoch();
    let _ = with_or_init_state(|state| {
        let id = state.account.thread_id;
        let primary = &registry().primary_thread_id;
        let _ = primary.compare_exchange(0, id, Ordering::Relaxed, Ordering::Relaxed);
    });
}

/// Move the reconciliation basis onto the calling thread, keeping the epoch
/// [`mark_process_start`] already seeded.
///
/// Needed because "the thread that is earliest" and "the thread that runs the
/// VM" are not the same thread in every launcher. `vm-cli` is the case that
/// forces it: the flags only become readable on the launcher thread (after
/// `flag_groups::expand_process_env`), but the whole VM then runs on a spawned
/// `main-vm` thread with a 128 MB stack while the launcher blocks in `join`
/// for the rest of the process. Leaving the basis on the launcher would make
/// `reconciles` a verdict about a thread that does nothing but wait — it would
/// read as ~100% unattributed no matter how well the VM itself was
/// instrumented, and the per-category values on the summary line (§7 of
/// `docs/observability/phase-accounting.md`) would all be zero.
///
/// Unlike [`mark_process_start`] this **overwrites** an existing claim, and it
/// deliberately does not touch the epoch: `process_wall_ns` keeps measuring
/// from the earlier call, so the launcher prologue is still inside the
/// process wall even though it is outside `basis_wall_ns`. The two are
/// reported separately for exactly this reason.
///
/// Last caller wins. Call it once, from the thread whose wall clock the
/// benchmark's wall clock is meant to be.
pub fn claim_reconciliation_basis() {
    if !enabled() {
        return;
    }
    let _ = with_or_init_state(|state| {
        let id = state.account.thread_id;
        registry().primary_thread_id.store(id, Ordering::Relaxed);
    });
}

// ── Per-thread accounting ────────────────────────────────────────────

/// Sentinel in [`ThreadAccount::end_ns`] meaning "this thread is still
/// running", so its wall clock is measured against report time.
const STILL_RUNNING: u64 = u64::MAX;

/// One thread's totals. Written only by the owning thread (relaxed adds);
/// read by [`report`] from any thread, which is why the fields are atomics
/// even though there is a single writer.
struct ThreadAccount {
    thread_id: u64,
    name: String,
    /// Epoch-relative registration time.
    start_ns: u64,
    /// Epoch-relative thread-exit time, or [`STILL_RUNNING`].
    end_ns: AtomicU64,
    /// Self time per category.
    totals: [AtomicU64; CATEGORY_COUNT],
    /// Closed-span count per category.
    entries: [AtomicU64; CATEGORY_COUNT],
    /// Running sum of `totals`, maintained alongside them so
    /// [`ThreadAccount::open_ns`] can subtract "charged since the outermost
    /// span opened" without re-summing the array under a race.
    attributed_ns: AtomicU64,
    /// Epoch-relative start of the outermost currently-open span, **biased by
    /// one** so `0` can mean "no span open" without excluding a genuine start
    /// at epoch-relative zero.
    open_root_start_ns_biased: AtomicU64,
    /// `attributed_ns` sampled when the outermost span opened.
    attributed_at_root_open: AtomicU64,
    /// Depth of the open-span stack.
    open_depth: AtomicU64,
    /// Spans dropped out of stack order.
    out_of_order: AtomicU64,
}

impl ThreadAccount {
    fn new(thread_id: u64, name: String, start_ns: u64) -> Self {
        ThreadAccount {
            thread_id,
            name,
            start_ns,
            end_ns: AtomicU64::new(STILL_RUNNING),
            totals: std::array::from_fn(|_| AtomicU64::new(0)),
            entries: std::array::from_fn(|_| AtomicU64::new(0)),
            attributed_ns: AtomicU64::new(0),
            open_root_start_ns_biased: AtomicU64::new(0),
            attributed_at_root_open: AtomicU64::new(0),
            open_depth: AtomicU64::new(0),
            out_of_order: AtomicU64::new(0),
        }
    }

    fn charge(&self, category: Category, self_ns: u64) {
        let idx = category.index();
        self.totals[idx].fetch_add(self_ns, Ordering::Relaxed);
        self.entries[idx].fetch_add(1, Ordering::Relaxed);
        self.attributed_ns.fetch_add(self_ns, Ordering::Relaxed);
    }

    /// Elapsed-but-not-yet-charged time inside the currently-open span tree.
    ///
    /// Without this the whole of an in-flight span would read as
    /// unattributed, which would make a report taken from inside a
    /// `vm_shutdown` span look like a 100%-unaccounted run.
    fn open_ns(&self, now_rel: u64, attributed: u64) -> u64 {
        let biased = self.open_root_start_ns_biased.load(Ordering::Relaxed);
        if biased == 0 {
            return 0;
        }
        let root_start = biased - 1;
        let elapsed = now_rel.saturating_sub(root_start);
        let at_open = self.attributed_at_root_open.load(Ordering::Relaxed);
        let charged_since = attributed.saturating_sub(at_open);
        elapsed.saturating_sub(charged_since)
    }
}

/// One open span on the owning thread's stack.
struct Frame {
    category: Category,
    token: u64,
    start: Instant,
    /// Whole elapsed time of every child span that has already closed. What
    /// makes the parent's charge *self* time.
    child_ns: u64,
}

/// The owning thread's private view. No locks and no atomics on the stack:
/// only this thread touches it.
struct ThreadState {
    account: Arc<ThreadAccount>,
    stack: Vec<Frame>,
    next_token: u64,
}

impl Drop for ThreadState {
    fn drop(&mut self) {
        // Whatever is still open at thread exit is charged to nothing; its
        // time becomes unattributed, which is the honest reading of "this
        // thread went away mid-phase".
        let now_rel = since_epoch_ns(Instant::now());
        self.account.end_ns.store(now_rel, Ordering::Relaxed);
    }
}

thread_local! {
    static STATE: RefCell<Option<ThreadState>> = const { RefCell::new(None) };
}

// ── Registry ─────────────────────────────────────────────────────────

struct Registry {
    accounts: Mutex<Vec<Arc<ThreadAccount>>>,
    next_thread_id: AtomicU64,
    /// `0` until [`mark_process_start`] (or the first registering thread)
    /// claims it.
    primary_thread_id: AtomicU64,
    threads_dropped: AtomicU64,
}

fn registry() -> &'static Registry {
    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    REGISTRY.get_or_init(|| Registry {
        accounts: Mutex::new(Vec::new()),
        next_thread_id: AtomicU64::new(1),
        primary_thread_id: AtomicU64::new(0),
        threads_dropped: AtomicU64::new(0),
    })
}

fn new_thread_state() -> Option<ThreadState> {
    let reg = registry();
    let now = Instant::now();
    let start_ns = since_epoch_ns(now);
    let thread = std::thread::current();
    let name = thread.name().unwrap_or("unnamed").to_string();
    let mut accounts = reg.accounts.lock();
    if accounts.len() >= MAX_TRACKED_THREADS {
        reg.threads_dropped.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let thread_id = reg.next_thread_id.fetch_add(1, Ordering::Relaxed);
    let account = Arc::new(ThreadAccount::new(thread_id, name, start_ns));
    accounts.push(Arc::clone(&account));
    drop(accounts);
    // First thread to record is the basis unless `mark_process_start` already
    // claimed one.
    let primary = &reg.primary_thread_id;
    let _ = primary.compare_exchange(0, thread_id, Ordering::Relaxed, Ordering::Relaxed);
    Some(ThreadState {
        account,
        stack: Vec::with_capacity(8),
        next_token: 1,
    })
}

/// Run `f` against this thread's state, creating it if necessary.
///
/// `try_with` rather than `with`, and `try_borrow_mut` rather than
/// `borrow_mut`: both of these run from `Drop` paths that can execute during
/// thread teardown, and a diagnostic that panics in a destructor is worse
/// than a diagnostic that loses a sample.
fn with_or_init_state<R>(f: impl FnOnce(&mut ThreadState) -> R) -> Option<R> {
    STATE
        .try_with(|cell| {
            let mut slot = cell.try_borrow_mut().ok()?;
            if slot.is_none() {
                *slot = new_thread_state();
            }
            slot.as_mut().map(f)
        })
        .ok()
        .flatten()
}

/// Run `f` against this thread's state only if it already exists.
fn with_existing_state<R>(f: impl FnOnce(&mut ThreadState) -> R) -> Option<R> {
    STATE
        .try_with(|cell| {
            let mut slot = cell.try_borrow_mut().ok()?;
            slot.as_mut().map(f)
        })
        .ok()
        .flatten()
}

// ── Spans ────────────────────────────────────────────────────────────

/// A scope guard that charges its own self time to one [`Category`].
///
/// Instrumenting a call site is one line:
///
/// ```ignore
/// let _phase = cratonvm_jfr::phase::enter(cratonvm_jfr::phase::Category::ClassLoad);
/// ```
///
/// When collection is off the guard holds no state and never reads the clock.
#[must_use = "a PhaseSpan charges its time on drop; binding it to `_` closes it immediately"]
pub struct PhaseSpan {
    /// `None` when this span records nothing.
    token: Option<u64>,
    category: Category,
}

impl PhaseSpan {
    /// A span that measures nothing. Cheap enough to construct
    /// unconditionally.
    pub fn disabled() -> PhaseSpan {
        PhaseSpan {
            token: None,
            category: Category::Idle,
        }
    }

    /// Which category this span charges.
    pub fn category(&self) -> Category {
        self.category
    }

    /// Whether this span is actually measuring.
    pub fn is_measuring(&self) -> bool {
        self.token.is_some()
    }

    /// Close the span now instead of at end of scope. Equivalent to `drop`,
    /// but reads better at a call site that must close before a `return`.
    pub fn end(self) {
        drop(self);
    }
}

impl Drop for PhaseSpan {
    fn drop(&mut self) {
        let Some(token) = self.token.take() else {
            return;
        };
        close_span(token);
    }
}

/// Open a span charging `category`.
///
/// Returns a non-recording span when collection is off, or when `category` is
/// not live at the current [`Level`] (see [`Category::active_at`]).
#[inline]
pub fn enter(category: Category) -> PhaseSpan {
    let level = level();
    if !category.active_at(level) {
        return PhaseSpan {
            token: None,
            category,
        };
    }
    open_span(category)
}

fn open_span(category: Category) -> PhaseSpan {
    let token = with_or_init_state(|state| {
        let token = state.next_token;
        state.next_token = state.next_token.wrapping_add(1);
        let now = Instant::now();
        if state.stack.is_empty() {
            let rel = since_epoch_ns(now).saturating_add(1);
            let account = &state.account;
            let charged = account.attributed_ns.load(Ordering::Relaxed);
            account
                .attributed_at_root_open
                .store(charged, Ordering::Relaxed);
            account
                .open_root_start_ns_biased
                .store(rel, Ordering::Relaxed);
        }
        state.account.open_depth.fetch_add(1, Ordering::Relaxed);
        state.stack.push(Frame {
            category,
            token,
            start: now,
            child_ns: 0,
        });
        token
    });
    PhaseSpan { token, category }
}

fn close_span(token: u64) {
    let now = Instant::now();
    let _ = with_existing_state(|state| {
        let Some(pos) = state.stack.iter().rposition(|f| f.token == token) else {
            // Already closed by an out-of-order sibling's unwind, or the
            // thread state was reset underneath us. Charging nothing is the
            // only choice that cannot double-count.
            return;
        };
        if pos + 1 != state.stack.len() {
            state.account.out_of_order.fetch_add(1, Ordering::Relaxed);
        }
        // Close every frame from the top down to and including `pos`. Frames
        // above `pos` are `pos`'s descendants, so popping top-down keeps the
        // child_ns hand-off correct and each frame is charged exactly once.
        while state.stack.len() > pos {
            let Some(frame) = state.stack.pop() else {
                break;
            };
            let total = now
                .saturating_duration_since(frame.start)
                .as_nanos()
                .min(u64::MAX as u128) as u64;
            let self_ns = total.saturating_sub(frame.child_ns);
            state.account.charge(frame.category, self_ns);
            state.account.open_depth.fetch_sub(1, Ordering::Relaxed);
            if let Some(parent) = state.stack.last_mut() {
                parent.child_ns = parent.child_ns.saturating_add(total);
            }
        }
        if state.stack.is_empty() {
            state
                .account
                .open_root_start_ns_biased
                .store(0, Ordering::Relaxed);
        }
    });
}

// ── Delegated compilation breakdown ──────────────────────────────────

fn compilation_breakdown_slot() -> &'static Mutex<Vec<(String, u64)>> {
    static SLOT: OnceLock<Mutex<Vec<(String, u64)>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(Vec::new()))
}

/// Publish `jit::metrics`' per-phase compile totals for the report to carry.
///
/// The caller is expected to pass `jit::metrics::summary().phase_totals_ns`
/// verbatim. This module deliberately does not depend on `cratonvm-jit` and
/// does not re-time those phases; see the module docs.
///
/// Replaces any previous breakdown, so calling it twice at two report points
/// yields two correct readings rather than one doubled one.
pub fn set_compilation_breakdown(phases: &[(&str, u64)]) {
    let mut slot = compilation_breakdown_slot().lock();
    slot.clear();
    slot.extend(phases.iter().map(|(name, ns)| ((*name).to_string(), *ns)));
}

/// The breakdown last published by [`set_compilation_breakdown`].
pub fn compilation_breakdown() -> Vec<(String, u64)> {
    compilation_breakdown_slot().lock().clone()
}

// ── Report ───────────────────────────────────────────────────────────

/// Wiring defects the report must not hide.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Anomalies {
    /// Spans dropped out of stack order, summed over threads. Non-zero means
    /// a call site holds a span past its logical scope; the time is still
    /// charged exactly once, but to a shorter interval than intended.
    pub out_of_order_closes: u64,
    /// Spans still open at report time, summed over threads. Expected to be
    /// small and non-zero (the report is usually taken inside a
    /// `vm_shutdown` span); their elapsed time is in `open_ns`, not in
    /// `unattributed_ns`.
    pub open_spans: u64,
    /// Threads that hit [`MAX_TRACKED_THREADS`] and recorded nothing.
    pub threads_dropped: u64,
}

/// One thread's partition of its own wall clock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadPhases {
    /// Registration-order id, `1`-based.
    pub thread_id: u64,
    /// `std::thread::current().name()` at registration, or `"unnamed"`.
    pub name: String,
    /// Whether this is the reconciliation basis (see [`mark_process_start`]).
    pub primary: bool,
    /// Epoch-relative registration time.
    pub start_ns: u64,
    /// Epoch-relative exit time; `None` while the thread is still running.
    pub end_ns: Option<u64>,
    /// `end_ns` (or report time) minus `start_ns` — the interval this
    /// thread's categories must partition.
    pub wall_ns: u64,
    /// Self time per category, in [`Category::ALL`] order.
    pub categories: Vec<(&'static str, u64)>,
    /// Closed-span count per category, in [`Category::ALL`] order.
    pub entries: Vec<(&'static str, u64)>,
    /// Σ `categories`.
    pub attributed_ns: u64,
    /// Elapsed time inside spans that were still open at report time.
    pub open_ns: u64,
    /// `wall_ns - attributed_ns - open_ns`. The measurement the 2% criterion
    /// is about.
    pub unattributed_ns: u64,
    /// `attributed_ns + open_ns - wall_ns` when that is positive. Always `0`
    /// on correct wiring; a non-zero value is a defect, never a workload
    /// property.
    pub over_attributed_ns: u64,
    /// Spans still open at report time.
    pub open_spans: u64,
    /// Spans dropped out of stack order on this thread.
    pub out_of_order_closes: u64,
}

impl ThreadPhases {
    /// Self time for one category.
    pub fn category_ns(&self, category: Category) -> u64 {
        lookup_ns(&self.categories, category)
    }

    /// `(unattributed + over_attributed) / wall`, or `0.0` for a zero-length
    /// thread.
    pub fn residual_fraction(&self) -> f64 {
        if self.wall_ns == 0 {
            return 0.0;
        }
        let residual = self.unattributed_ns.saturating_add(self.over_attributed_ns);
        residual as f64 / self.wall_ns as f64
    }
}

/// A whole run's phase accounting.
#[derive(Debug, Clone, PartialEq)]
pub struct PhaseReport {
    /// [`PHASE_ACCOUNTING_SCHEMA_VERSION`] at the time of writing.
    pub schema_version: u32,
    /// The level collection ran at.
    pub level: Level,
    /// Report time minus the process epoch.
    pub process_wall_ns: u64,
    /// Every tracked thread, registration order.
    pub threads: Vec<ThreadPhases>,
    /// The reconciliation basis: the thread claimed by [`mark_process_start`],
    /// or the first thread to record. `None` when nothing recorded.
    pub primary: Option<ThreadPhases>,
    /// Per-category self time summed over every thread, in [`Category::ALL`]
    /// order. **This is thread time, not wall time** — on a multi-threaded run
    /// it legitimately exceeds `process_wall_ns`.
    pub totals: Vec<(&'static str, u64)>,
    /// Per-category closed-span counts summed over every thread.
    pub entries: Vec<(&'static str, u64)>,
    /// Σ every thread's `wall_ns`.
    pub thread_wall_ns: u64,
    /// Σ `totals`.
    pub attributed_ns: u64,
    /// Σ every thread's `open_ns`.
    pub open_ns: u64,
    /// Σ every thread's `unattributed_ns`.
    pub unattributed_ns: u64,
    /// Σ every thread's `over_attributed_ns`.
    pub over_attributed_ns: u64,
    /// `jit::metrics` per-phase compile totals, as published by
    /// [`set_compilation_breakdown`]. A breakdown *of* the `compilation`
    /// bucket, never added to it.
    pub compilation_breakdown: Vec<(String, u64)>,
    /// `Σ compilation_breakdown - totals["compilation"]`, signed. Non-zero is
    /// expected and informative: the compile span brackets queue handoff and
    /// publication that `jit::metrics` does not time, and `jit::metrics`'
    /// ring may have evicted reports the span still counted.
    pub compilation_delta_ns: i128,
    /// Wiring defects.
    pub anomalies: Anomalies,
    /// The fraction of the basis wall clock that may be unattributed for
    /// [`PhaseReport::reconciles`] to hold.
    pub tolerance: f64,
}

impl PhaseReport {
    /// Wall clock the reconciliation is measured against: the basis thread's,
    /// falling back to the process wall when nothing registered.
    pub fn basis_wall_ns(&self) -> u64 {
        match &self.primary {
            Some(t) => t.wall_ns,
            None => self.process_wall_ns,
        }
    }

    /// Unattributed plus over-attributed time on the basis thread.
    pub fn residual_ns(&self) -> u64 {
        match &self.primary {
            Some(t) => t.unattributed_ns.saturating_add(t.over_attributed_ns),
            None => self.process_wall_ns,
        }
    }

    /// `residual_ns / basis_wall_ns`.
    pub fn residual_fraction(&self) -> f64 {
        let wall = self.basis_wall_ns();
        if wall == 0 {
            return 0.0;
        }
        self.residual_ns() as f64 / wall as f64
    }

    /// Whether this run meets the review's acceptance criterion.
    pub fn reconciles(&self) -> bool {
        self.residual_fraction() <= self.tolerance
    }

    /// Per-category self time summed over threads.
    pub fn total_ns(&self, category: Category) -> u64 {
        lookup_ns(&self.totals, category)
    }

    /// One JSON object. Hand-rolled: this crate has no serialization
    /// dependency and adding one for a diagnostic would be a poor trade — the
    /// same call `jit::metrics` made.
    pub fn to_json(&self) -> String {
        let mut s = String::with_capacity(2048);
        s.push('{');
        let _ = write!(
            s,
            "\"schema_version\":{},\"level\":\"{}\",\"process_wall_ns\":{}",
            self.schema_version,
            self.level.name(),
            self.process_wall_ns,
        );
        let _ = write!(
            s,
            ",\"tolerance\":{},\"reconciles\":{},\"residual_ppm\":{}",
            self.tolerance,
            self.reconciles(),
            residual_ppm(self.residual_ns(), self.basis_wall_ns()),
        );
        let _ = write!(
            s,
            ",\"basis_wall_ns\":{},\"residual_ns\":{}",
            self.basis_wall_ns(),
            self.residual_ns(),
        );
        let _ = write!(
            s,
            ",\"thread_wall_ns\":{},\"attributed_ns\":{},\"open_ns\":{}",
            self.thread_wall_ns, self.attributed_ns, self.open_ns,
        );
        let _ = write!(
            s,
            ",\"unattributed_ns\":{},\"over_attributed_ns\":{}",
            self.unattributed_ns, self.over_attributed_ns,
        );
        let _ = write!(s, ",\"totals\":{}", json_pairs(&self.totals));
        let _ = write!(s, ",\"entries\":{}", json_pairs(&self.entries));
        s.push_str(",\"primary\":");
        match &self.primary {
            Some(t) => s.push_str(&thread_json(t)),
            None => s.push_str("null"),
        }
        s.push_str(",\"threads\":[");
        for (i, t) in self.threads.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push_str(&thread_json(t));
        }
        s.push(']');
        s.push_str(",\"compilation_breakdown\":{");
        for (i, (name, ns)) in self.compilation_breakdown.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            let _ = write!(s, "\"{}\":{}", json_escape(name), ns);
        }
        s.push('}');
        let _ = write!(s, ",\"compilation_delta_ns\":{}", self.compilation_delta_ns);
        let _ = write!(
            s,
            ",\"anomalies\":{{\"out_of_order_closes\":{},\"open_spans\":{},\
             \"threads_dropped\":{}}}",
            self.anomalies.out_of_order_closes,
            self.anomalies.open_spans,
            self.anomalies.threads_dropped,
        );
        s.push('}');
        s
    }

    /// One line for stderr, in the `key=<digits>` shape
    /// `regression-suite/perf/run-cratonbench-gate.sh` already scrapes.
    ///
    /// Every value is an integer for exactly that reason: the gate's
    /// extraction anchors on `[0-9]+$`, and a float would split the token.
    /// `residual_ppm` is parts per million rather than a percentage for the
    /// same reason.
    pub fn summary_line(&self) -> String {
        // Fall back to the cross-thread aggregate when nothing claimed the
        // basis, so a report is never silently all-zeroes.
        let attributed = match &self.primary {
            Some(t) => t.attributed_ns,
            None => self.attributed_ns,
        };
        let open = match &self.primary {
            Some(t) => t.open_ns,
            None => self.open_ns,
        };
        let unattributed = match &self.primary {
            Some(t) => t.unattributed_ns,
            None => self.unattributed_ns,
        };
        let over = match &self.primary {
            Some(t) => t.over_attributed_ns,
            None => self.over_attributed_ns,
        };
        let ppm = residual_ppm(self.residual_ns(), self.basis_wall_ns());
        let mut s = String::with_capacity(512);
        let _ = write!(
            s,
            "[PHASE-ACCOUNTING] level={} schema={} basis_wall_ns={} attributed_ns={} \
             open_ns={} unattributed_ns={} over_attributed_ns={} residual_ppm={} \
             reconciles={} threads={} out_of_order={} threads_dropped={}",
            self.level.name(),
            self.schema_version,
            self.basis_wall_ns(),
            attributed,
            open,
            unattributed,
            over,
            ppm,
            u8::from(self.reconciles()),
            self.threads.len(),
            self.anomalies.out_of_order_closes,
            self.anomalies.threads_dropped,
        );
        for category in Category::ALL {
            let ns = match &self.primary {
                Some(t) => t.category_ns(category),
                None => self.total_ns(category),
            };
            let _ = write!(s, " {}_ns={}", category.name(), ns);
        }
        s
    }
}

/// Residual as parts per million, saturating. Integer so the value survives
/// the benchmark harness's `grep -oE 'key=[0-9]+'` extraction.
fn residual_ppm(residual_ns: u64, wall_ns: u64) -> u64 {
    if wall_ns == 0 {
        return 0;
    }
    let scaled = (residual_ns as u128).saturating_mul(1_000_000) / wall_ns as u128;
    scaled.min(u64::MAX as u128) as u64
}

/// Read one category's value out of a `Category::ALL`-ordered pair list.
fn lookup_ns(pairs: &[(&'static str, u64)], category: Category) -> u64 {
    match pairs.get(category.index()) {
        Some((_, ns)) => *ns,
        None => 0,
    }
}

/// Sum one field over every thread, saturating.
fn sum_threads(threads: &[ThreadPhases], pick: impl Fn(&ThreadPhases) -> u64) -> u64 {
    let mut total: u64 = 0;
    for t in threads {
        total = total.saturating_add(pick(t));
    }
    total
}

fn json_pairs(list: &[(&'static str, u64)]) -> String {
    let mut s = String::with_capacity(list.len() * 24 + 2);
    s.push('{');
    for (i, (name, value)) in list.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        let _ = write!(s, "\"{}\":{}", json_escape(name), value);
    }
    s.push('}');
    s
}

fn thread_json(t: &ThreadPhases) -> String {
    let mut s = String::with_capacity(512);
    s.push('{');
    let _ = write!(
        s,
        "\"thread_id\":{},\"name\":\"{}\",\"primary\":{}",
        t.thread_id,
        json_escape(&t.name),
        t.primary,
    );
    let _ = write!(s, ",\"start_ns\":{}", t.start_ns);
    match t.end_ns {
        Some(end) => {
            let _ = write!(s, ",\"end_ns\":{}", end);
        }
        None => s.push_str(",\"end_ns\":null"),
    }
    let _ = write!(
        s,
        ",\"wall_ns\":{},\"attributed_ns\":{},\"open_ns\":{}",
        t.wall_ns, t.attributed_ns, t.open_ns,
    );
    let _ = write!(
        s,
        ",\"unattributed_ns\":{},\"over_attributed_ns\":{}",
        t.unattributed_ns, t.over_attributed_ns,
    );
    let _ = write!(
        s,
        ",\"open_spans\":{},\"out_of_order_closes\":{}",
        t.open_spans, t.out_of_order_closes,
    );
    let _ = write!(s, ",\"categories\":{}", json_pairs(&t.categories));
    let _ = write!(s, ",\"entries\":{}", json_pairs(&t.entries));
    s.push('}');
    s
}

/// Escape a string for a JSON double-quoted scalar.
///
/// Thread names are supplied by whoever spawned the thread, so they are not
/// guaranteed to be free of quotes or control characters, and one unescaped
/// byte would make the whole report unparseable.
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

/// Snapshot the accounting for every tracked thread.
///
/// Safe to call at any time and from any thread, including more than once:
/// every number is run-cumulative, so two snapshots are two correct readings
/// rather than one doubled one.
pub fn report() -> PhaseReport {
    let reg = registry();
    let now_rel = since_epoch_ns(Instant::now());
    let primary_id = reg.primary_thread_id.load(Ordering::Relaxed);

    let accounts: Vec<Arc<ThreadAccount>> = reg.accounts.lock().clone();

    let mut threads: Vec<ThreadPhases> = Vec::with_capacity(accounts.len());
    let mut totals: Vec<(&'static str, u64)> =
        Category::ALL.iter().map(|c| (c.name(), 0u64)).collect();
    let mut entries: Vec<(&'static str, u64)> =
        Category::ALL.iter().map(|c| (c.name(), 0u64)).collect();
    let mut anomalies = Anomalies {
        threads_dropped: reg.threads_dropped.load(Ordering::Relaxed),
        ..Anomalies::default()
    };

    for account in &accounts {
        let raw_end = account.end_ns.load(Ordering::Relaxed);
        let end_ns = if raw_end == STILL_RUNNING {
            None
        } else {
            Some(raw_end)
        };
        let effective_end = end_ns.unwrap_or(now_rel).max(account.start_ns);
        let wall_ns = effective_end.saturating_sub(account.start_ns);

        let mut categories: Vec<(&'static str, u64)> = Vec::with_capacity(CATEGORY_COUNT);
        let mut thread_entries: Vec<(&'static str, u64)> = Vec::with_capacity(CATEGORY_COUNT);
        let mut attributed_ns: u64 = 0;
        for category in Category::ALL {
            let idx = category.index();
            let ns = account.totals[idx].load(Ordering::Relaxed);
            let count = account.entries[idx].load(Ordering::Relaxed);
            attributed_ns = attributed_ns.saturating_add(ns);
            categories.push((category.name(), ns));
            thread_entries.push((category.name(), count));
            if let Some(slot) = totals.get_mut(idx) {
                slot.1 = slot.1.saturating_add(ns);
            }
            if let Some(slot) = entries.get_mut(idx) {
                slot.1 = slot.1.saturating_add(count);
            }
        }

        // An open span's elapsed time is neither charged nor unattributed —
        // it is in flight. Clamp it to what is left of the thread's wall
        // clock so the three parts can never sum past it.
        let open_ns = account
            .open_ns(now_rel, attributed_ns)
            .min(wall_ns.saturating_sub(attributed_ns.min(wall_ns)));
        let charged = attributed_ns.saturating_add(open_ns);
        let unattributed_ns = wall_ns.saturating_sub(charged);
        let over_attributed_ns = charged.saturating_sub(wall_ns);
        let open_spans = account.open_depth.load(Ordering::Relaxed);
        let out_of_order_closes = account.out_of_order.load(Ordering::Relaxed);

        anomalies.open_spans = anomalies.open_spans.saturating_add(open_spans);
        anomalies.out_of_order_closes = anomalies
            .out_of_order_closes
            .saturating_add(out_of_order_closes);

        threads.push(ThreadPhases {
            thread_id: account.thread_id,
            name: account.name.clone(),
            primary: account.thread_id == primary_id,
            start_ns: account.start_ns,
            end_ns,
            wall_ns,
            categories,
            entries: thread_entries,
            attributed_ns,
            open_ns,
            unattributed_ns,
            over_attributed_ns,
            open_spans,
            out_of_order_closes,
        });
    }

    let thread_wall_ns = sum_threads(&threads, |t| t.wall_ns);
    let attributed_ns = sum_threads(&threads, |t| t.attributed_ns);
    let open_ns = sum_threads(&threads, |t| t.open_ns);
    let unattributed_ns = sum_threads(&threads, |t| t.unattributed_ns);
    let over_attributed_ns = sum_threads(&threads, |t| t.over_attributed_ns);
    let primary = threads.iter().find(|t| t.primary).cloned();

    let compilation_breakdown = compilation_breakdown();
    let mut breakdown_total: u64 = 0;
    for (_, ns) in &compilation_breakdown {
        breakdown_total = breakdown_total.saturating_add(*ns);
    }
    let compilation_total = lookup_ns(&totals, Category::Compilation);
    let compilation_delta_ns = breakdown_total as i128 - compilation_total as i128;

    PhaseReport {
        schema_version: PHASE_ACCOUNTING_SCHEMA_VERSION,
        level: level(),
        process_wall_ns: now_rel,
        threads,
        primary,
        totals,
        entries,
        thread_wall_ns,
        attributed_ns,
        open_ns,
        unattributed_ns,
        over_attributed_ns,
        compilation_breakdown,
        compilation_delta_ns,
        anomalies,
        tolerance: DEFAULT_RECONCILIATION_TOLERANCE,
    }
}

// ── Sinks ────────────────────────────────────────────────────────────

fn flag_path(name: &str) -> Option<PathBuf> {
    let raw = cratonvm_types::flags::runtime_var_os(name)?;
    if raw.is_empty() {
        return None;
    }
    Some(PathBuf::from(raw))
}

/// The JSON sink path, or `None` when [`FLAG_JSON_OUT`] is unset or empty.
pub fn json_out_path() -> Option<PathBuf> {
    flag_path(FLAG_JSON_OUT)
}

/// The JFR sink path, or `None` when [`FLAG_JFR_OUT`] is unset or empty.
pub fn jfr_out_path() -> Option<PathBuf> {
    flag_path(FLAG_JFR_OUT)
}

/// Write `report` to `path` as one JSON document, replacing any existing file.
///
/// One document rather than JSON-lines: unlike `jit::metrics`, which appends a
/// record per compilation, a phase report is a whole-run artefact and the
/// benchmark harness wants to `json.load` it.
pub fn write_json_report(path: &Path, report: &PhaseReport) -> std::io::Result<()> {
    let mut body = report.to_json();
    body.push('\n');
    std::fs::write(path, body)
}

/// Register the phase-accounting event types into `registry`.
///
/// Public so an operator who already has a live [`crate::FlightRecorder`] can
/// have the phase events land in the same chunk. Deliberately **not** called
/// by [`crate::create_flight_recorder`]: these are not `jdk.*` built-ins, and
/// silently growing the built-in registry would change every existing
/// recording's metadata section.
pub fn register_phase_events(registry: &mut EventTypeRegistry) {
    let stub = EventTypeId(0);
    registry.register(EventType {
        id: stub,
        name: PHASE_CATEGORY_EVENT.into(),
        category: vec!["CratonVM".into(), "Phase Accounting".into()],
        description: "Self time charged to one phase category on one thread".into(),
        fields: vec![
            EventField::new("category", "string", "Phase category"),
            EventField::new("threadName", "string", "Thread name"),
            EventField::new("selfTime", "long", "Self time (ns)"),
            EventField::new("count", "long", "Closed spans"),
            EventField::new("primary", "boolean", "Reconciliation basis thread"),
        ],
        has_thread: true,
        has_stacktrace: false,
        period: EventPeriod::None,
        threshold: None,
    });
    registry.register(EventType {
        id: stub,
        name: PHASE_THREAD_EVENT.into(),
        category: vec!["CratonVM".into(), "Phase Accounting".into()],
        description: "One thread's wall clock and its attribution residual".into(),
        fields: vec![
            EventField::new("threadName", "string", "Thread name"),
            EventField::new("wallTime", "long", "Thread wall time (ns)"),
            EventField::new("attributedTime", "long", "Attributed time (ns)"),
            EventField::new("openTime", "long", "In-flight span time (ns)"),
            EventField::new("unattributedTime", "long", "Unattributed time (ns)"),
            EventField::new("overAttributedTime", "long", "Over-attributed time (ns)"),
            EventField::new("primary", "boolean", "Reconciliation basis thread"),
        ],
        has_thread: true,
        has_stacktrace: false,
        period: EventPeriod::BeginEnd,
        threshold: None,
    });
    registry.register(EventType {
        id: stub,
        name: PHASE_SUMMARY_EVENT.into(),
        category: vec!["CratonVM".into(), "Phase Accounting".into()],
        description: "Whole-run phase accounting summary".into(),
        fields: vec![
            EventField::new("level", "string", "Collection level"),
            EventField::new("wallTime", "long", "Basis wall time (ns)"),
            EventField::new("attributedTime", "long", "Attributed thread time (ns)"),
            EventField::new("openTime", "long", "In-flight span time (ns)"),
            EventField::new("unattributedTime", "long", "Unattributed time (ns)"),
            EventField::new("overAttributedTime", "long", "Over-attributed time (ns)"),
            EventField::new("residualPpm", "long", "Residual, parts per million"),
            EventField::new("reconciles", "boolean", "Within tolerance"),
        ],
        has_thread: true,
        has_stacktrace: false,
        period: EventPeriod::None,
        threshold: None,
    });
}

/// Event type name: one (thread, category) self-time record.
pub const PHASE_CATEGORY_EVENT: &str = "cratonvm.PhaseAccountingCategory";
/// Event type name: one thread's wall clock and residual.
pub const PHASE_THREAD_EVENT: &str = "cratonvm.PhaseAccountingThread";
/// Event type name: the whole-run summary.
pub const PHASE_SUMMARY_EVENT: &str = "cratonvm.PhaseAccountingSummary";

/// Convert `report` into JFR event instances against `registry`.
///
/// `chunk_start_ns` is the absolute (epoch-nanos) time corresponding to
/// process-relative zero, so a consumer can place the events on the same
/// timeline as ordinary JFR events. A category event's duration is its *self
/// time*, not a real contiguous interval — a category is by construction the
/// union of many disjoint intervals, and JFR has no shape for that. The
/// duration is the number the report is about; the position is nominal.
///
/// Returns an empty vector when the registry does not know the phase event
/// types (call [`register_phase_events`] first).
pub fn report_to_events(
    report: &PhaseReport,
    registry: &EventTypeRegistry,
    chunk_start_ns: u64,
) -> Vec<EventInstance> {
    let mut events = Vec::new();
    let category_id = registry.find_by_name(PHASE_CATEGORY_EVENT);
    let thread_id_type = registry.find_by_name(PHASE_THREAD_EVENT);
    let summary_id = registry.find_by_name(PHASE_SUMMARY_EVENT);

    for thread in &report.threads {
        let base = chunk_start_ns.saturating_add(thread.start_ns);
        if let Some(type_id) = category_id {
            for (name, ns) in &thread.categories {
                if *ns == 0 {
                    continue;
                }
                let count = thread
                    .entries
                    .iter()
                    .find(|(n, _)| *n == *name)
                    .map(|(_, c)| *c)
                    .unwrap_or(0);
                let mut fields = EventFields::with_capacity(5);
                fields.push(EventValue::Str(*name));
                fields.push(EventValue::String(Arc::from(thread.name.as_str())));
                fields.push(EventValue::Long(clamp_i64(*ns)));
                fields.push(EventValue::Long(clamp_i64(count)));
                fields.push(EventValue::Boolean(thread.primary));
                events.push(EventInstance {
                    type_id,
                    start_time: base,
                    end_time: base.saturating_add(*ns),
                    thread_id: thread.thread_id,
                    fields,
                });
            }
        }
        if let Some(type_id) = thread_id_type {
            let mut fields = EventFields::with_capacity(7);
            fields.push(EventValue::String(Arc::from(thread.name.as_str())));
            fields.push(EventValue::Long(clamp_i64(thread.wall_ns)));
            fields.push(EventValue::Long(clamp_i64(thread.attributed_ns)));
            fields.push(EventValue::Long(clamp_i64(thread.open_ns)));
            fields.push(EventValue::Long(clamp_i64(thread.unattributed_ns)));
            fields.push(EventValue::Long(clamp_i64(thread.over_attributed_ns)));
            fields.push(EventValue::Boolean(thread.primary));
            events.push(EventInstance {
                type_id,
                start_time: base,
                end_time: base.saturating_add(thread.wall_ns),
                thread_id: thread.thread_id,
                fields,
            });
        }
    }

    if let Some(type_id) = summary_id {
        let ppm = residual_ppm(report.residual_ns(), report.basis_wall_ns());
        let basis_thread = match &report.primary {
            Some(t) => t.thread_id,
            None => 0,
        };
        let mut fields = EventFields::with_capacity(8);
        fields.push(EventValue::Str(report.level.name()));
        fields.push(EventValue::Long(clamp_i64(report.basis_wall_ns())));
        fields.push(EventValue::Long(clamp_i64(report.attributed_ns)));
        fields.push(EventValue::Long(clamp_i64(report.open_ns)));
        fields.push(EventValue::Long(clamp_i64(report.unattributed_ns)));
        fields.push(EventValue::Long(clamp_i64(report.over_attributed_ns)));
        fields.push(EventValue::Long(clamp_i64(ppm)));
        fields.push(EventValue::Boolean(report.reconciles()));
        events.push(EventInstance {
            type_id,
            start_time: chunk_start_ns,
            end_time: chunk_start_ns.saturating_add(report.process_wall_ns),
            thread_id: basis_thread,
            fields,
        });
    }

    events
}

fn clamp_i64(v: u64) -> i64 {
    v.min(i64::MAX as u64) as i64
}

/// Write `report` to `path` as a standalone JFR chunk.
///
/// Builds its own registry, so this works on a run that never started a
/// flight recording — which is every run today (see the LIVENESS block in
/// [`crate`]'s module docs). Returns the byte count written.
///
/// The bytes are the **JDK's own** chunk format ([`crate::jdk_chunk`]). This
/// file is handed to an operator who opens it in JMC or runs `jfr print` on it —
/// the timeline caveat below is written for exactly that reader — and until this
/// used the JDK format that reader got
/// `IOException: Unknown string encoding 17` instead of a timeline.
pub fn write_jfr_report(path: &Path, report: &PhaseReport) -> Result<u64, JfrDumpError> {
    let mut registry = EventTypeRegistry::new();
    register_phase_events(&mut registry);
    let now_epoch_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .min(u64::MAX as u128) as u64;
    let chunk_start_ns = now_epoch_ns.saturating_sub(report.process_wall_ns);
    let events = report_to_events(report, &registry, chunk_start_ns);
    let repository = EventRepository::new(1);
    crate::jdk_chunk::dump_to_file(
        path,
        &repository,
        &registry,
        chunk_start_ns,
        report.process_wall_ns,
        events,
        true,
    )
}

/// Outcome of [`emit_configured_sinks`].
#[derive(Debug)]
pub struct SinkOutcome {
    /// The report that was written (or would have been).
    pub report: PhaseReport,
    /// `Some(path)` when the JSON sink was configured, with the write result.
    pub json: Option<(PathBuf, std::io::Result<()>)>,
    /// `Some(path)` when the JFR sink was configured, with the write result.
    pub jfr: Option<(PathBuf, Result<u64, JfrDumpError>)>,
}

/// Build a report and write whichever sinks the flags configured.
///
/// The single call a shutdown path needs. Returns the report so the caller can
/// also print [`PhaseReport::summary_line`] to stderr, which is what the
/// benchmark gate scrapes. A sink whose path cannot be opened is reported in
/// the outcome rather than raised: a diagnostic that fails a JVM shutdown
/// because a directory is read-only would be a worse defect than the missing
/// artefact.
pub fn emit_configured_sinks() -> Option<SinkOutcome> {
    if !enabled() {
        return None;
    }
    let report = report();
    let json = json_out_path().map(|p| {
        let r = write_json_report(&p, &report);
        (p, r)
    });
    let jfr = jfr_out_path().map(|p| {
        let r = write_jfr_report(&p, &report);
        (p, r)
    });
    Some(SinkOutcome { report, json, jfr })
}

// ── Test helpers ─────────────────────────────────────────────────────

#[cfg(test)]
fn set_level_for_test(level: Level) {
    LEVEL.store(level_code(level), Ordering::Relaxed);
}

/// Drop every registered thread account and this thread's local state.
///
/// Test-only: the registry is process-wide and the cargo harness runs tests in
/// parallel threads of one process, so every test that observes totals must
/// start from a known state *and* hold `TEST_LOCK`.
#[cfg(test)]
fn reset_for_test() {
    registry().accounts.lock().clear();
    registry().primary_thread_id.store(0, Ordering::Relaxed);
    registry().threads_dropped.store(0, Ordering::Relaxed);
    compilation_breakdown_slot().lock().clear();
    let _ = STATE.try_with(|cell| {
        if let Ok(mut slot) = cell.try_borrow_mut() {
            *slot = None;
        }
    });
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// The level flag, the registry and the compilation breakdown are all
    /// process-wide. Every test here takes this first.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn spin_ns(ns: u64) {
        let start = Instant::now();
        while start.elapsed() < Duration::from_nanos(ns) {
            std::hint::spin_loop();
        }
    }

    /// Extract an integer field from a flat JSON object body. Enough to prove
    /// the document parses back to the numbers that went in without pulling a
    /// serde dependency into this crate for a test.
    fn json_u64(doc: &str, key: &str) -> Option<u64> {
        let needle = format!("\"{key}\":");
        let at = doc.find(&needle)? + needle.len();
        let tail = &doc[at..];
        let end = tail
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(tail.len());
        tail[..end].parse().ok()
    }

    fn json_str<'a>(doc: &'a str, key: &str) -> Option<&'a str> {
        let needle = format!("\"{key}\":\"");
        let at = doc.find(&needle)? + needle.len();
        let tail = &doc[at..];
        let end = tail.find('"')?;
        Some(&tail[..end])
    }

    // --- Category / level vocabulary -------------------------------------

    #[test]
    fn category_names_and_indices_agree_with_all() {
        for (i, c) in Category::ALL.iter().enumerate() {
            assert_eq!(c.index(), i, "{} has the wrong index", c.name());
            assert_eq!(
                Category::from_name(c.name()),
                Some(*c),
                "{} does not round-trip through from_name",
                c.name()
            );
        }
        let mut names: Vec<&str> = Category::ALL.iter().map(|c| c.name()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), CATEGORY_COUNT, "duplicate category name");
    }

    #[test]
    fn the_executor_split_is_mutually_exclusive_across_levels() {
        assert!(Category::JavaExecution.active_at(Level::Coarse));
        assert!(!Category::Interpretation.active_at(Level::Coarse));
        assert!(!Category::JitExecution.active_at(Level::Coarse));

        assert!(!Category::JavaExecution.active_at(Level::Fine));
        assert!(Category::Interpretation.active_at(Level::Fine));
        assert!(Category::JitExecution.active_at(Level::Fine));

        for c in Category::ALL {
            assert!(!c.active_at(Level::Off), "{} records when off", c.name());
        }
    }

    // --- Disabled path ---------------------------------------------------

    #[test]
    fn disabled_records_nothing() {
        let _g = TEST_LOCK.lock();
        set_level_for_test(Level::Off);
        reset_for_test();

        {
            let span = enter(Category::ClassLoad);
            assert!(!span.is_measuring(), "a disabled span must not measure");
            spin_ns(50_000);
        }
        let report = report();
        assert!(
            report.threads.is_empty(),
            "the disabled path registered a thread account"
        );
        assert_eq!(report.attributed_ns, 0);
        assert_eq!(report.level, Level::Off);
        set_level_for_test(Level::Off);
        reset_for_test();
    }

    #[test]
    fn a_category_inactive_at_this_level_records_nothing() {
        let _g = TEST_LOCK.lock();
        set_level_for_test(Level::Coarse);
        reset_for_test();

        {
            let span = enter(Category::Interpretation);
            assert!(!span.is_measuring());
            spin_ns(50_000);
        }
        let report = report();
        assert_eq!(report.total_ns(Category::Interpretation), 0);

        set_level_for_test(Level::Off);
        reset_for_test();
    }

    // --- Disjointness ----------------------------------------------------

    #[test]
    fn nested_phases_charge_disjoint_time() {
        let _g = TEST_LOCK.lock();
        set_level_for_test(Level::Coarse);
        reset_for_test();

        // Register the thread account first: its one-time cost (registry lock,
        // Arc allocation, thread-name copy) would otherwise land between
        // `outer_start` and the first frame's own clock read and read as lost
        // time.
        mark_process_start();
        let outer_start = Instant::now();
        {
            let _outer = enter(Category::JavaExecution);
            spin_ns(200_000);
            {
                let _middle = enter(Category::ClassLoad);
                spin_ns(200_000);
                {
                    let _inner = enter(Category::Verification);
                    spin_ns(200_000);
                }
                spin_ns(200_000);
            }
            spin_ns(200_000);
        }
        let outer_total = outer_start.elapsed().as_nanos() as u64;

        let report = report();
        let java = report.total_ns(Category::JavaExecution);
        let load = report.total_ns(Category::ClassLoad);
        let verify = report.total_ns(Category::Verification);

        // Each of the three saw its own spins only.
        for (name, ns) in [
            ("java_execution", java),
            ("class_load", load),
            ("verification", verify),
        ] {
            assert!(ns > 0, "{name} recorded nothing");
        }
        // The decisive assertion: the parts sum to at most the whole. Under
        // full double-counting this sum would be ~3x the outer span.
        let sum = java + load + verify;
        assert!(
            sum <= outer_total,
            "nested phases double-counted: parts {sum} ns exceed the outer span's {outer_total} ns",
        );
        // ...and they account for essentially all of it (the only slack is
        // the instrumentation's own clock reads).
        assert!(
            sum * 100 >= outer_total * 90,
            "nested phases lost time: parts {sum} ns cover less than 90% of {outer_total} ns",
        );

        set_level_for_test(Level::Off);
        reset_for_test();
    }

    #[test]
    fn out_of_order_close_is_counted_and_still_charges_once() {
        let _g = TEST_LOCK.lock();
        set_level_for_test(Level::Coarse);
        reset_for_test();

        let outer = enter(Category::JavaExecution);
        spin_ns(100_000);
        let inner = enter(Category::ClassLoad);
        spin_ns(100_000);
        // Deliberately wrong order: close the outer span first.
        drop(outer);
        spin_ns(100_000);
        drop(inner);

        let report = report();
        assert_eq!(
            report.anomalies.out_of_order_closes, 1,
            "an out-of-order close must be reported, not silently accepted"
        );
        // The inner span was unwound by the outer's close, so its later drop
        // charged nothing extra.
        assert_eq!(
            report.entries[Category::ClassLoad.index()].1,
            1,
            "the inner span was charged more than once"
        );
        let thread = report.primary.expect("a thread registered");
        assert_eq!(
            thread.over_attributed_ns, 0,
            "an out-of-order close must not push attribution past the wall clock"
        );

        set_level_for_test(Level::Off);
        reset_for_test();
    }

    // --- Reconciliation --------------------------------------------------

    #[test]
    fn categories_plus_unattributed_sum_to_the_measured_wall_time() {
        let _g = TEST_LOCK.lock();
        set_level_for_test(Level::Coarse);
        reset_for_test();

        mark_process_start();
        {
            let _s = enter(Category::VmStartup);
            spin_ns(300_000);
        }
        {
            let _s = enter(Category::JavaExecution);
            spin_ns(300_000);
        }

        let report = report();
        let thread = report.primary.clone().expect("primary thread registered");
        assert_eq!(
            thread.attributed_ns + thread.open_ns + thread.unattributed_ns,
            thread.wall_ns,
            "the reconciliation identity does not hold on the basis thread",
        );
        assert_eq!(
            thread.over_attributed_ns, 0,
            "nothing over-attributed on a well-formed run"
        );
        let summed: u64 = thread.categories.iter().map(|(_, ns)| *ns).sum();
        assert_eq!(summed, thread.attributed_ns);

        set_level_for_test(Level::Off);
        reset_for_test();
    }

    #[test]
    fn a_deliberate_gap_lands_in_unattributed_not_in_a_category() {
        let _g = TEST_LOCK.lock();
        set_level_for_test(Level::Coarse);
        reset_for_test();

        mark_process_start();
        {
            let _s = enter(Category::JavaExecution);
            spin_ns(200_000);
        }
        // Instrumented by nothing on purpose.
        spin_ns(2_000_000);
        {
            let _s = enter(Category::VmShutdown);
            spin_ns(200_000);
        }

        let report = report();
        let thread = report.primary.clone().expect("primary thread registered");
        assert!(
            thread.unattributed_ns >= 1_500_000,
            "the 2 ms gap was absorbed into a category: unattributed is only {} ns",
            thread.unattributed_ns
        );
        assert!(
            !report.reconciles(),
            "a run that is majority-unattributed must not report as reconciled \
             (residual {:.4})",
            report.residual_fraction()
        );
        assert!(report.residual_fraction() > 0.02);

        set_level_for_test(Level::Off);
        reset_for_test();
    }

    #[test]
    fn an_open_span_is_reported_as_open_not_as_unattributed() {
        let _g = TEST_LOCK.lock();
        set_level_for_test(Level::Coarse);
        reset_for_test();

        mark_process_start();
        let held = enter(Category::VmShutdown);
        spin_ns(1_000_000);
        let report = report();
        let thread = report.primary.clone().expect("primary thread registered");
        assert_eq!(thread.open_spans, 1);
        assert!(
            thread.open_ns >= 500_000,
            "an in-flight span's elapsed time must show as open_ns, got {}",
            thread.open_ns
        );
        assert_eq!(
            thread.attributed_ns + thread.open_ns + thread.unattributed_ns,
            thread.wall_ns,
        );
        drop(held);

        set_level_for_test(Level::Off);
        reset_for_test();
    }

    // --- Concurrency -----------------------------------------------------

    #[test]
    fn concurrent_threads_neither_lose_nor_double_count() {
        let _g = TEST_LOCK.lock();
        set_level_for_test(Level::Coarse);
        reset_for_test();

        const THREADS: usize = 8;
        const SPANS: usize = 25;

        fn worker() {
            for _ in 0..SPANS {
                let _outer = enter(Category::JavaExecution);
                spin_ns(20_000);
                let _inner = enter(Category::GcPause);
                spin_ns(20_000);
            }
        }

        let mut handles = Vec::new();
        for i in 0..THREADS {
            let name = format!("phase-test-{i}");
            let builder = std::thread::Builder::new().name(name);
            handles.push(builder.spawn(worker).expect("spawn"));
        }
        for h in handles {
            h.join().expect("join");
        }

        let report = report();
        let expected = (THREADS * SPANS) as u64;
        assert_eq!(
            report.entries[Category::JavaExecution.index()].1,
            expected,
            "outer spans lost or duplicated across threads"
        );
        assert_eq!(
            report.entries[Category::GcPause.index()].1,
            expected,
            "inner spans lost or duplicated across threads"
        );
        assert_eq!(
            report.anomalies.out_of_order_closes, 0,
            "correctly-nested concurrent spans reported an ordering anomaly"
        );
        // Every worker thread is its own account, and each one's parts sum to
        // its own wall clock.
        let mut workers = 0usize;
        for t in &report.threads {
            if !t.name.starts_with("phase-test-") {
                continue;
            }
            workers += 1;
            let parts = t.attributed_ns + t.open_ns + t.unattributed_ns;
            assert_eq!(parts, t.wall_ns, "thread {} does not reconcile", t.name);
            assert_eq!(t.over_attributed_ns, 0);
            assert!(
                t.attributed_ns <= t.wall_ns,
                "thread {} attributed {} ns of a {} ns life",
                t.name,
                t.attributed_ns,
                t.wall_ns
            );
        }
        assert_eq!(workers, THREADS);

        set_level_for_test(Level::Off);
        reset_for_test();
    }

    // --- Reconciliation basis --------------------------------------------

    /// The `vm-cli` shape: the thread that can first read the flags is not the
    /// thread that runs the VM, and the first one spends the run blocked in
    /// `join`. Without [`claim_reconciliation_basis`] the basis stays on the
    /// waiter and `reconciles()` is a verdict about a thread that did nothing.
    #[test]
    fn the_basis_moves_to_the_thread_that_claims_it_last() {
        let _g = TEST_LOCK.lock();
        set_level_for_test(Level::Coarse);
        reset_for_test();

        // The launcher: seeds the epoch, claims the basis, then only waits.
        mark_process_start();
        let launcher_id = with_or_init_state(|s| s.account.thread_id).expect("launcher account");
        assert_eq!(
            registry().primary_thread_id.load(Ordering::Relaxed),
            launcher_id,
            "mark_process_start did not claim the calling thread"
        );

        let builder = std::thread::Builder::new().name("phase-basis-vm".into());
        let worker = builder
            .spawn(|| {
                claim_reconciliation_basis();
                let _p = enter(Category::VmStartup);
                spin_ns(200_000);
            })
            .expect("spawn");
        worker.join().expect("join");

        let report = report();
        let primary = report.primary.as_ref().expect("a basis thread");
        assert_eq!(
            primary.name, "phase-basis-vm",
            "the basis stayed on the launcher thread"
        );
        assert!(
            primary.categories[Category::VmStartup.index()].1 > 0,
            "the basis thread recorded no vm_startup time"
        );
        // The launcher is still tracked — moving the basis must not drop its
        // account, only its verdict-carrying role.
        assert!(
            report.threads.iter().any(|t| t.thread_id == launcher_id),
            "the launcher account disappeared when the basis moved"
        );
        // And the epoch is still the launcher's: the process wall covers at
        // least the worker's whole life.
        assert!(
            report.process_wall_ns >= primary.wall_ns,
            "claim_reconciliation_basis re-seeded the epoch"
        );

        set_level_for_test(Level::Off);
        reset_for_test();
    }

    /// Same guarantee as [`mark_process_start`]: when collection is off,
    /// claiming the basis must not register an account or read the clock.
    #[test]
    fn claiming_the_basis_while_disabled_registers_nothing() {
        let _g = TEST_LOCK.lock();
        set_level_for_test(Level::Off);
        reset_for_test();

        mark_process_start();
        claim_reconciliation_basis();

        let report = report();
        assert!(
            report.threads.is_empty(),
            "the disabled path registered a thread account"
        );
        assert_eq!(registry().primary_thread_id.load(Ordering::Relaxed), 0);

        reset_for_test();
    }

    /// The `vm-cli` span nesting, end to end: `vm_startup` then
    /// `java_execution` then `vm_shutdown`, on one thread, charging disjoint
    /// time that sums to that thread's wall clock.
    #[test]
    fn the_launcher_span_sequence_charges_disjoint_time() {
        let _g = TEST_LOCK.lock();
        set_level_for_test(Level::Coarse);
        reset_for_test();

        let builder = std::thread::Builder::new().name("phase-launcher-seq".into());
        let worker = builder
            .spawn(|| {
                claim_reconciliation_basis();
                let startup = enter(Category::VmStartup);
                spin_ns(300_000);
                // A class load nests inside startup and subtracts from it.
                {
                    let _load = enter(Category::ClassLoad);
                    spin_ns(300_000);
                }
                startup.end();

                let exec = enter(Category::JavaExecution);
                spin_ns(300_000);
                exec.end();

                let _shutdown = enter(Category::VmShutdown);
                spin_ns(300_000);
            })
            .expect("spawn");
        worker.join().expect("join");

        let report = report();
        let primary = report.primary.as_ref().expect("a basis thread");
        let cat = |c: Category| primary.categories[c.index()].1;

        let charged = [
            Category::VmStartup,
            Category::ClassLoad,
            Category::JavaExecution,
            Category::VmShutdown,
        ];
        for c in charged {
            assert!(
                cat(c) > 0,
                "{} charged nothing in the launcher sequence",
                c.name()
            );
        }
        // Disjoint, not overlapping: these four are the only spans opened, so
        // they account for all of the thread's attributed time exactly. A
        // nested `class_load` that was *added* to `vm_startup` rather than
        // carved out of it would push this sum past `attributed_ns`.
        let sum: u64 = charged.iter().map(|c| cat(*c)).sum();
        assert_eq!(
            sum, primary.attributed_ns,
            "the four launcher categories do not partition the attributed time"
        );
        assert_eq!(primary.over_attributed_ns, 0, "over-attribution");
        assert_eq!(primary.out_of_order_closes, 0, "an out-of-order close");
        assert_eq!(
            primary.attributed_ns + primary.open_ns + primary.unattributed_ns,
            primary.wall_ns,
            "the launcher sequence does not reconcile"
        );

        set_level_for_test(Level::Off);
        reset_for_test();
    }

    // --- Serialization ---------------------------------------------------

    #[test]
    fn json_round_trips_the_numbers_and_the_shape() {
        let _g = TEST_LOCK.lock();
        set_level_for_test(Level::Fine);
        reset_for_test();

        mark_process_start();
        {
            let _s = enter(Category::Interpretation);
            spin_ns(200_000);
        }
        set_compilation_breakdown(&[("scan", 11), ("build", 22), ("lower", 33)]);

        let report = report();
        let doc = report.to_json();

        let want_schema = PHASE_ACCOUNTING_SCHEMA_VERSION as u64;
        assert_eq!(json_u64(&doc, "schema_version"), Some(want_schema));
        assert_eq!(json_str(&doc, "level"), Some("fine"));
        let want_wall = report.process_wall_ns;
        assert_eq!(json_u64(&doc, "process_wall_ns"), Some(want_wall));
        assert_eq!(json_u64(&doc, "attributed_ns"), Some(report.attributed_ns));
        let want_unattributed = report.unattributed_ns;
        assert_eq!(json_u64(&doc, "unattributed_ns"), Some(want_unattributed));
        let want_thread_wall = report.thread_wall_ns;
        assert_eq!(json_u64(&doc, "thread_wall_ns"), Some(want_thread_wall));
        // Every category key is present even at zero, so two runs diff cleanly.
        for c in Category::ALL {
            assert!(
                doc.contains(&format!("\"{}\":", c.name())),
                "category {} missing from the JSON",
                c.name()
            );
        }
        // The delegated breakdown survives verbatim.
        assert!(doc.contains("\"scan\":11"));
        assert!(doc.contains("\"build\":22"));
        assert!(doc.contains("\"lower\":33"));
        let compiled_ns = report.total_ns(Category::Compilation) as i128;
        assert_eq!(report.compilation_delta_ns, 66 - compiled_ns);
        // Balanced braces: a hand-rolled encoder's most likely failure.
        assert_eq!(
            doc.chars().filter(|c| *c == '{').count(),
            doc.chars().filter(|c| *c == '}').count(),
            "unbalanced braces in the JSON report"
        );

        set_level_for_test(Level::Off);
        reset_for_test();
    }

    #[test]
    fn the_summary_line_is_integer_only_so_the_gate_can_scrape_it() {
        let _g = TEST_LOCK.lock();
        set_level_for_test(Level::Coarse);
        reset_for_test();

        mark_process_start();
        {
            let _s = enter(Category::GcPause);
            spin_ns(100_000);
        }
        let line = report().summary_line();
        assert!(line.starts_with("[PHASE-ACCOUNTING] "));
        for token in line.split_whitespace().skip(1) {
            let Some((key, value)) = token.split_once('=') else {
                panic!("summary token {token} is not key=value");
            };
            assert!(!key.is_empty());
            if key == "level" {
                continue;
            }
            assert!(
                value.chars().all(|c| c.is_ascii_digit()),
                "summary value for {key} is not an integer: {value}"
            );
        }
        for c in Category::ALL {
            assert!(
                line.contains(&format!(" {}_ns=", c.name())),
                "summary line omits {}",
                c.name()
            );
        }

        set_level_for_test(Level::Off);
        reset_for_test();
    }

    #[test]
    fn thread_names_with_quotes_do_not_break_the_json() {
        let _g = TEST_LOCK.lock();
        set_level_for_test(Level::Coarse);
        reset_for_test();

        let name = "we\"ird\n\tname".to_string();
        let builder = std::thread::Builder::new().name(name);
        let handle = builder.spawn(|| {
            let _s = enter(Category::NativeCall);
            spin_ns(10_000);
        });
        handle.expect("spawn").join().expect("join");

        let doc = report().to_json();
        assert!(
            doc.contains("we\\\"ird\\n\\tname"),
            "thread name not escaped"
        );
        assert!(!doc.contains("we\"ird"), "raw quote leaked into the JSON");

        set_level_for_test(Level::Off);
        reset_for_test();
    }

    // --- JFR sink --------------------------------------------------------

    #[test]
    fn the_jfr_sink_writes_a_readable_chunk() {
        let _g = TEST_LOCK.lock();
        set_level_for_test(Level::Coarse);
        reset_for_test();

        mark_process_start();
        {
            let _s = enter(Category::Compilation);
            spin_ns(150_000);
        }
        {
            let _s = enter(Category::GcPause);
            spin_ns(150_000);
        }

        let report = report();
        let name = format!("cratonvm-phase-accounting-{}.jfr", std::process::id());
        let path = std::env::temp_dir().join(name);
        let bytes = write_jfr_report(&path, &report).expect("jfr write");
        assert!(bytes > 0);

        let chunk = crate::jdk_chunk::read_chunk(&path).expect("chunk");
        let mut registry = EventTypeRegistry::new();
        register_phase_events(&mut registry);
        let events = &chunk.events;
        assert!(
            !events.is_empty(),
            "the phase report produced no readable events"
        );
        for e in events {
            assert!(
                registry.find_by_name(&e.type_name).is_some(),
                "event of unregistered type {} survived the round trip",
                e.type_name
            );
        }
        assert_eq!(
            events
                .iter()
                .filter(|e| e.type_name == PHASE_SUMMARY_EVENT)
                .count(),
            1,
            "expected exactly one summary event"
        );

        let _ = std::fs::remove_file(&path);
        set_level_for_test(Level::Off);
        reset_for_test();
    }

    #[test]
    fn the_json_sink_writes_a_whole_document() {
        let _g = TEST_LOCK.lock();
        set_level_for_test(Level::Coarse);
        reset_for_test();

        mark_process_start();
        {
            let _s = enter(Category::ClassLoad);
            spin_ns(100_000);
        }
        let report = report();
        let name = format!("cratonvm-phase-accounting-{}.json", std::process::id());
        let path = std::env::temp_dir().join(name);
        write_json_report(&path, &report).expect("json write");
        let body = std::fs::read_to_string(&path).expect("read back");
        assert!(body.trim_end().starts_with('{'));
        assert!(body.trim_end().ends_with('}'));
        assert_eq!(json_u64(&body, "attributed_ns"), Some(report.attributed_ns));

        let _ = std::fs::remove_file(&path);
        set_level_for_test(Level::Off);
        reset_for_test();
    }

    // --- Delegation ------------------------------------------------------

    #[test]
    fn the_compilation_breakdown_is_replaced_not_accumulated() {
        let _g = TEST_LOCK.lock();
        set_level_for_test(Level::Coarse);
        reset_for_test();

        set_compilation_breakdown(&[("scan", 5)]);
        set_compilation_breakdown(&[("scan", 5), ("build", 7)]);
        let breakdown = compilation_breakdown();
        assert_eq!(breakdown.len(), 2);
        assert_eq!(breakdown[0], ("scan".to_string(), 5));
        assert_eq!(breakdown[1], ("build".to_string(), 7));

        set_level_for_test(Level::Off);
        reset_for_test();
    }
}
