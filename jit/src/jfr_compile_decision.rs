//! The `cratonvm.JitCompileDecision` JFR event producer.
//!
//! Split out of `lib.rs` on 2026-09-16. `lib.rs` had grown past 47,000 lines,
//! which is the size at which unrelated concerns share a file only because
//! nobody drew a boundary. This section already had one: it is the whole
//! producer half of a single JFR event, it owns its own thread-local, and
//! nothing outside it needs that thread-local.
//!
//! Glob-re-exported from the crate root, so every path a caller used before
//! the split still resolves -- the code moved, the API did not.

use crate::{compile_gate, compute_jit_key_hash};

// ---------------------------------------------------------------------------
// The `cratonvm.JitCompileDecision` producer (JFR)
// ---------------------------------------------------------------------------
//
// `cratonvm-jit` has no `FlightRecorder`. The recorder is
// `SharedVm::debug.flight_recorder` in `cratonvm-vm`, and a `jit -> vm` edge
// would cycle, so `cratonvm_jfr::jit_decision` holds an installed sink that
// `Vm::new` fills in at boot — the same shape as
// `cratonvm_gc::install_gc_start_hook`. Everything below is the producer half:
// it decides WHAT to say, and pays nothing at all when nobody is listening.
//
// ONE EVENT PER COMPILE, EMITTED AT THE FUNNEL — the judgement call this wiring
// had to make, written down because the alternative reads as an oversight.
// The admission verdict is built in `try_compile_inner` BEFORE the optimizing
// pipeline runs, and a method the chain admitted can still fall back to the
// single-pass backend inside that pipeline. So the verdict is a PREDICTION of
// which backend will run; the FACT is `CompiledMethod::used_ir_backend`, and
// that is known only at the completion funnel in
// `try_compile_with_invokespecial_resolver`. Emitting at both places would put
// two events with contradicting `outcome` values in the dump for every admitted
// method, and a reader asking `jfr print` "which backend actually compiled this
// method?" would have to know to join them and to prefer the second. So the
// verdict is CARRIED FORWARD to the funnel in the thread-local below — exactly
// the way `JIT_BAIL_SITE` above already carries a refusal reason across the
// same boundary — and the single event the funnel emits pairs the authoritative
// `outcome` with the verdict that explains it.
//
// The OSR door is the one exception, and it is not a second event for the same
// compile: `JitVerdictRegistry::mark_bail_listed_with_site` is a DIFFERENT compile, at a door
// that reaches the backend directly and never passes through that funnel.

thread_local! {
    /// The admission verdict of each compile currently in flight on this
    /// thread, innermost last.
    ///
    /// A stack rather than the plain `Cell` [`JIT_BAIL_SITE`] uses, because
    /// compiles NEST: `callee_compiler` re-enters
    /// `try_compile_with_invokespecial_resolver` on this same thread for an
    /// inlining candidate, which is the whole reason `JitCompileStackGuard`
    /// exists. With one slot the callee's verdict would overwrite the caller's
    /// and then be consumed by the callee's own funnel, leaving the caller —
    /// the method the operator actually asked about — reporting nothing.
    ///
    /// Only ever pushed while `jit_decision_enabled()`, so a default run never
    /// allocates this `Vec` at all.
    static JIT_ADMISSION_VERDICT: std::cell::RefCell<Vec<Option<String>>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// One frame of [`JIT_ADMISSION_VERDICT`], popped on every exit of the compile
/// that pushed it.
///
/// RAII rather than a take at the funnel, because
/// `try_compile_with_invokespecial_resolver` has early `return None` exits that
/// never reach the funnel, and a frame left behind would be read by the NEXT
/// compile on this thread — the same stale-reason failure mode that
/// `take_jit_pipeline_stage`'s reset-even-on-success exists to prevent.
pub(crate) struct JitDecisionFrame {
    /// Whether this frame actually pushed. Recorded rather than re-derived on
    /// drop: the gate is process-global and another thread may flip it
    /// mid-compile, and a push/pop pair decided by two separate reads of it
    /// would unbalance the stack.
    pushed: bool,
}

impl JitDecisionFrame {
    pub(crate) fn enter() -> Self {
        let pushed = cratonvm_jfr::jit_decision::jit_decision_enabled();
        if pushed {
            JIT_ADMISSION_VERDICT.with(|v| v.borrow_mut().push(None));
        }
        Self { pushed }
    }
}

impl Drop for JitDecisionFrame {
    fn drop(&mut self) {
        if self.pushed {
            JIT_ADMISSION_VERDICT.with(|v| {
                let _ = v.borrow_mut().pop();
            });
        }
    }
}

/// Hand the admission verdict to the compile's own frame.
///
/// A no-op when the event is not armed (no frame was pushed), which is what
/// makes it safe to call from the verdict site without a second gate check.
pub(crate) fn note_jit_admission_verdict(verdict: &str) {
    JIT_ADMISSION_VERDICT.with(|v| {
        if let Some(top) = v.borrow_mut().last_mut() {
            *top = Some(verdict.to_owned());
        }
    });
}

/// The verdict recorded for the compile currently innermost on this thread.
pub(crate) fn current_jit_admission_verdict() -> Option<String> {
    JIT_ADMISSION_VERDICT.with(|v| v.borrow().last().cloned().flatten())
}

/// Nanos since the UNIX epoch, for the decision event's `start_time`.
///
/// Deliberately the same expression the `emit_compilation_event_arc` call sites
/// in `vm/src/runtime/interpreter.rs` and
/// `vm/src/runtime/interpreter/jit_bridge.rs` use, rather than a second clock:
/// a `cratonvm.JitCompileDecision` and the `jdk.Compilation` for the same
/// method have to be joinable in one dump, and two clocks disagreeing by even a
/// boot offset would make that join silently wrong instead of visibly absent.
pub(crate) fn jit_decision_now_nanos() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64 // Cast: duration to u64 nanoseconds
}

/// Map the compile gate's door onto the JFR event's structural mirror of it.
///
/// `cratonvm-jfr` cannot depend on `cratonvm-jit`, so `CompileDoor` is
/// redeclared there with identical spellings; this is the one place the two
/// meet, and it is an exhaustive `match` on purpose. `compile_gate` exists
/// *because* patching one door and shipping was a repeated failure mode here,
/// so a fourth door has to be a compile error at this line rather than a silent
/// `MethodEntry`.
pub(crate) fn jfr_compile_door(
    door: compile_gate::CompileDoor,
) -> cratonvm_jfr::jit_decision::CompileDoor {
    match door {
        compile_gate::CompileDoor::MethodEntry => {
            cratonvm_jfr::jit_decision::CompileDoor::MethodEntry
        }
        compile_gate::CompileDoor::EagerFirstCall => {
            cratonvm_jfr::jit_decision::CompileDoor::EagerFirstCall
        }
        compile_gate::CompileDoor::Osr => cratonvm_jfr::jit_decision::CompileDoor::Osr,
    }
}

thread_local! {
    /// The furthest stage of the compile pipeline this thread has entered for
    /// the compile currently running.
    ///
    /// # Why this exists
    ///
    /// `jit-method-stats` reported `reason=unrecorded` for **26 of the top 30**
    /// hot-but-permanently-interpreted methods on a Spring Boot context-startup
    /// workload — the compiler was asked, refused, bail-listed the method for
    /// the life of the process, and recorded nothing about why. Every bail that
    /// goes through `jitc_bail!` / `jitc_permanent_bail!` / the backend's own
    /// `note_jit_bail_site*` calls is named; the unnamed ones come from
    /// somewhere those macros do not cover, and reading the ~40 `None` exits of
    /// `try_compile_inner` did not find it.
    ///
    /// So instead of auditing exits, bound the answer: each pipeline stage
    /// stamps its own name on entry, and a `None` with no explicit site reports
    /// the last stage reached. That cannot miss a path, present or future — a
    /// new exit added tomorrow is still attributed to whichever stage was
    /// running. The site name is deliberately shaped `no-site-after-<stage>` so
    /// it is obvious in a report that this is the FALLBACK, not a diagnosis:
    /// it localises the refusal to one stage, and the stage's owner then names
    /// the specific exit.
    pub(crate) static JIT_PIPELINE_STAGE: std::cell::Cell<&'static str> =
        const { std::cell::Cell::new(JIT_STAGE_ENTRY) };
}

/// Stage names. `&'static str` because a bail site is one, so a stage can be
/// reported as a site without formatting.
pub const JIT_STAGE_ENTRY: &str = "no-site-after-entry";
pub const JIT_STAGE_SCAN: &str = "no-site-after-scan";
pub const JIT_STAGE_BUILD: &str = "no-site-after-ir-build";
pub const JIT_STAGE_OPTIMIZE: &str = "no-site-after-ir-optimize";
pub const JIT_STAGE_EA: &str = "no-site-after-escape-analysis";
pub const JIT_STAGE_SCHEDULE: &str = "no-site-after-schedule";
pub const JIT_STAGE_LOWER: &str = "no-site-after-lower";
pub const JIT_STAGE_SINGLE_PASS: &str = "no-site-after-single-pass-backend";

/// Stamp the stage the compile pipeline is entering. One `Cell` store.
#[inline]
pub fn note_jit_pipeline_stage(stage: &'static str) {
    JIT_PIPELINE_STAGE.with(|c| c.set(stage));
}

/// The last stage stamped, resetting to `entry` for the next compile.
pub fn take_jit_pipeline_stage() -> &'static str {
    JIT_PIPELINE_STAGE.with(|c| c.replace(JIT_STAGE_ENTRY))
}

/// Census of methods sealed out of JIT compilation before any compile was
/// attempted, by reason.
///
/// Separate from the compile-failure table because the two populations are
/// different sizes and want different fixes: a Spring Boot context startup
/// seals **856** methods here against **69** whose compile was attempted and
/// refused. Until 2026-08-10 every one of the 856 was labelled
/// `static-policy-or-native-shadow`, which cannot distinguish a policy-table
/// entry (a list somebody can shorten) from a native-shadow scan hit (a scan
/// that may be over-matching) from a `ForkJoinTask` subclass from a `<clinit>`.
static JIT_SKIP_SEAL_REASONS: std::sync::OnceLock<
    parking_lot::RwLock<rustc_hash::FxHashMap<&'static str, u64>>,
> = std::sync::OnceLock::new();

fn jit_skip_seal_reasons() -> &'static parking_lot::RwLock<rustc_hash::FxHashMap<&'static str, u64>>
{
    JIT_SKIP_SEAL_REASONS.get_or_init(|| parking_lot::RwLock::new(rustc_hash::FxHashMap::default()))
}

/// Count one method sealed out of compilation for `reason`.
///
/// Called once per method (the seal is permanent and the caller checks the
/// skip-set first), so the write lock is taken once per sealed method for the
/// life of the process, not once per invocation.
pub fn note_jit_skip_seal_reason(reason: &'static str) {
    *jit_skip_seal_reasons().write().entry(reason).or_insert(0) += 1;
}

/// Census of native-shadow seal decisions by which arm fired.
static JIT_NATIVE_SHADOW_CAUSES: std::sync::OnceLock<
    parking_lot::RwLock<rustc_hash::FxHashMap<&'static str, u64>>,
> = std::sync::OnceLock::new();

fn jit_native_shadow_causes(
) -> &'static parking_lot::RwLock<rustc_hash::FxHashMap<&'static str, u64>> {
    JIT_NATIVE_SHADOW_CAUSES
        .get_or_init(|| parking_lot::RwLock::new(rustc_hash::FxHashMap::default()))
}

/// Count one native-shadow verdict, by arm (`direct` / `inherited` /
/// `interface-blind`). Called from the caller-scan, which runs once per method
/// before it is sealed — not per invocation.
pub fn note_jit_native_shadow_cause(cause: &'static str) {
    *jit_native_shadow_causes().write().entry(cause).or_insert(0) += 1;
}

/// The native-shadow arm census, highest count first.
pub fn jit_native_shadow_cause_census() -> Vec<(&'static str, u64)> {
    let mut v: Vec<(&'static str, u64)> = jit_native_shadow_causes()
        .read()
        .iter()
        .map(|(k, v)| (*k, *v))
        .collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    v
}

/// The seal census, highest count first, for the stats dump.
pub fn jit_skip_seal_census() -> Vec<(&'static str, u64)> {
    let mut v: Vec<(&'static str, u64)> = jit_skip_seal_reasons()
        .read()
        .iter()
        .map(|(k, v)| (*k, *v))
        .collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    v
}

/// The one bail site that is a MEASUREMENT rather than a verdict: the
/// single-pass backend emitted past its code-buffer estimate.
///
/// Named as a constant because two places have to agree on it — the backend
/// records it, and `try_compile` exempts it from the permanent bail-list so the
/// next attempt can re-run at the measured size. A string literal in both
/// places would silently stop matching the day either is reworded, and the
/// symptom (methods quietly never compiling again) is the exact failure this
/// exemption exists to prevent.
pub const CODE_BUFFER_TOO_SMALL_SITE: &str = "code-buffer-estimate-too-small";

/// Compiles thrown away because the buffer estimate was short, and the
/// wall-clock nanoseconds those attempts cost.
///
/// The retry is deferred to the NEXT compile request, so every shortfall is a
/// full lowering done twice. Whether that matters is an arithmetic question and
/// it had never been answered: `bug-two-pqc-classes-exceed-900s-20260821-RESOLVED.md`
/// listed "14 wasted compiles of ~120 KB methods" as a lead and said, correctly,
/// "worth sizing before assuming it matters". These two counters size it. They
/// are printed beside `total_compile_time_ms`, which is the denominator that
/// makes the number mean something.
static CODE_BUFFER_BAIL_NANOS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static CODE_BUFFER_BAIL_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Record one discarded compile and what it cost.
pub fn note_code_buffer_bail_cost(elapsed: std::time::Duration) {
    CODE_BUFFER_BAIL_NANOS.fetch_add(
        elapsed.as_nanos().min(u64::MAX as u128) as u64,
        std::sync::atomic::Ordering::Relaxed,
    );
    CODE_BUFFER_BAIL_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// `(discarded compiles, milliseconds they cost)`.
pub fn code_buffer_bail_cost() -> (u64, u64) {
    (
        CODE_BUFFER_BAIL_COUNT.load(std::sync::atomic::Ordering::Relaxed),
        CODE_BUFFER_BAIL_NANOS.load(std::sync::atomic::Ordering::Relaxed) / 1_000_000,
    )
}

/// Per-method code-buffer shortfalls measured by a previous compile attempt.
///
/// Keyed by the same `"<class>.<method>:<descriptor>"` string the backend
/// receives as `method_key`. Small and write-once-per-overflow: only methods
/// that actually overflowed ever appear, which on the workloads measured here
/// is none at all.
/// `(measured, attempts)` per overflowing method.
type CodeBufferShortfall = (usize, u32);

static CODE_BUFFER_SHORTFALLS: std::sync::OnceLock<
    parking_lot::RwLock<rustc_hash::FxHashMap<String, CodeBufferShortfall>>,
> = std::sync::OnceLock::new();

fn code_buffer_shortfalls(
) -> &'static parking_lot::RwLock<rustc_hash::FxHashMap<String, CodeBufferShortfall>> {
    CODE_BUFFER_SHORTFALLS
        .get_or_init(|| parking_lot::RwLock::new(rustc_hash::FxHashMap::default()))
}

/// How many times one method may be re-lowered to chase a bigger code buffer
/// before the refusal becomes permanent.
///
/// Each doubling is a full single-pass lowering plus an `mmap`/`munmap` of the
/// estimate, paid on the calling thread at the warmup gate. Three doublings take
/// the buffer to 8x the first failing capacity; a method that still does not fit
/// is not going to, and retrying it forever is the failure mode this cap exists
/// to bound (see [`ExecutableBuffer::mark_codegen_unencodable`], which removed
/// the OTHER way this loop used to become infinite).
pub const MAX_CODE_BUFFER_RETRIES: u32 = 3;

/// Record that compiling `method_key` wanted `wanted` bytes and failed at a
/// buffer of `failed_capacity` bytes.
///
/// Keeps the LARGEST observation: a later attempt can take a shorter path
/// through the same method (a callee that has since become inlinable, a guard
/// that de-speculated), and sizing the next buffer from that smaller number
/// would overflow again.
///
/// `failed_capacity` is part of that maximum, and it is what makes the retry
/// CONVERGE. `wanted` alone does not: an overflow can be recorded with `wanted`
/// well below the capacity that failed (`try_patch_*` overruns add nothing to
/// it), and `code_buffer_hint`'s doubling of such a `wanted` produces a size the
/// heuristic already beat — so `estimated_size.max(hint)` re-allocated exactly
/// the capacity that had just failed, and the next attempt failed identically.
/// Taking `failed_capacity` into the maximum guarantees each attempt allocates
/// strictly more than the last.
pub fn note_code_buffer_shortfall(method_key: &str, wanted: usize, failed_capacity: usize) {
    if method_key.is_empty() {
        return;
    }
    let measured = wanted.max(failed_capacity);
    if measured == 0 {
        return;
    }
    let mut map = code_buffer_shortfalls().write();
    let slot = map.entry(method_key.to_string()).or_insert((0, 0));
    slot.0 = slot.0.max(measured);
    slot.1 = slot.1.saturating_add(1);
}

/// The measured buffer size to use for `method_key`, if a previous attempt
/// overflowed.
///
/// Doubled, because the recorded measurement UNDER-reports: `wanted` accumulates
/// the bytes `emit` asked for, and an out-of-bounds `try_patch_*` adds nothing to
/// it — so the true requirement is at least the measurement and possibly more.
/// Doubling converges in one step for every shape seen so far instead of burning
/// a second `tier_fail_count` retry to discover the same thing again.
pub fn code_buffer_hint(method_key: &str) -> Option<usize> {
    if method_key.is_empty() {
        return None;
    }
    code_buffer_shortfalls()
        .read()
        .get(method_key)
        .map(|(measured, _)| measured.saturating_mul(2))
}

/// Methods whose IR build bailed on a `new` whose class was not loaded YET.
///
/// Keyed like [`jit_bail_list`]. Bounded by [`MAX_DEFERRED_NEW_RETRIES`]
/// entries so a pathological run cannot grow it without limit.
///
/// `state` is what makes the grant ONE-SHOT rather than a loop: `0` means "one
/// retry is owed", `1` means "already granted".
///
/// `sites` are the `new` sites that were `Deferred` when the build bailed, as
/// `(holder_class_id, cp_idx)` -- the same pair the resolver takes. They are
/// recorded because the retry used to be spent BLIND: it flipped `0` to `1` on
/// the next supersede attempt whether or not the class that caused the bail had
/// loaded. Measured on CratonBench, that lost every retry it granted -- five
/// `java/util/regex/Pattern` methods each bailed TWICE and then had no retry
/// left for the moment the class did load. `sites` is what lets the grant wait
/// for the condition it is retrying on.
struct DeferredNewRetry {
    state: u8,
    /// How many times the re-offer sweep has asked this memo's sites to resolve
    /// and been told no.
    ///
    /// The sweep runs on every class definition, so an armed memo whose class
    /// never loads is re-resolved once per definition for the life of the
    /// process. Measured on the H2 JDBC workload: **8,131 site resolutions over
    /// 101 distinct sites in one run**, one of them 3,905 times -- a
    /// `new java/nio/charset/MalformedInputException` on a decoding error path
    /// inside `java/lang/String`, for a class the program never loads *because*
    /// that path never runs. Each of those took the class-manager read lock.
    ///
    /// A memo that has been asked [`MAX_DEFERRED_NEW_LOOKS`] times and answered
    /// no every time is retired, which returns [`DEFERRED_NEW_ARMED`] toward
    /// zero and with it the sweep's own fast path. Bounding the LOOKS rather
    /// than the wall-clock or the definition count is deliberate: it is a
    /// budget on the thing that costs, and it cannot silence a memo whose class
    /// loads promptly, because such a memo is granted on its first or second
    /// look and never spends the budget at all.
    looks: u32,
    sites: Vec<(u32, u16)>,
    /// The method this memo belongs to, so a HELD retry can be re-offered.
    ///
    /// The map is keyed by a hash, which is enough to answer "does this method
    /// have a retry" at the door the method itself walks through, and useless
    /// for the opposite direction: after holding a retry, something has to go
    /// looking for it once the class loads, and a hash names no method.
    key: (
        std::sync::Arc<str>,
        std::sync::Arc<str>,
        std::sync::Arc<str>,
    ),
    /// The declaring class the recorder compiled (`ClassId(0)` when it had
    /// none), so the re-offer sweep can name the method's loader-faithful
    /// identity instead of re-resolving the class by name. Not part of the
    /// key: the memo is keyed by names, as it always was.
    class_id: cratonvm_types::ClassId,
}

/// The deferred-`new` retry memo (see [`DeferredNewRetry`]).
///
/// One per VM (interpreter round i1 wave 10,
/// `i1-L7-process-global-jit-bridge-state-not-vm-scoped`): the VM owns one
/// (`JitBridgeState`) and hands it to its compiles through
/// [`crate::CompileRequest::deferred_new_retries`], the way the de-speculation
/// registries are handed in. Keyed by names, so it used to be one
/// process-global map shared by every VM: VM B's sweep could spend VM A's
/// one-shot retry, and it asked VM B's class manager about VM A's `(holder,
/// cp_idx)` site ids. A compile with no VM in scope names the process-wide
/// instance explicitly (`crate::CompileRealm::process`; the request field is
/// required since interpreter round i1 wave 18), and the free functions below
/// use it, exactly the old behaviour.
#[derive(Default)]
pub struct DeferredNewRetries {
    set: parking_lot::RwLock<rustc_hash::FxHashMap<u64, DeferredNewRetry>>,
    /// Memos armed and not yet spent or retired, in THIS registry.
    armed: std::sync::atomic::AtomicU64,
}

/// The registry of compiles with no VM in scope, and of the free functions.
pub(crate) fn process_deferred_new_retries() -> &'static DeferredNewRetries {
    static SET: std::sync::OnceLock<DeferredNewRetries> = std::sync::OnceLock::new();
    SET.get_or_init(DeferredNewRetries::default)
}

impl Drop for DeferredNewRetries {
    /// Take this registry's armed memos out of the process-wide sum, so a VM
    /// that goes away does not leave [`held_deferred_new_count`] non-zero and
    /// the class-definition sweep off its fast path for good.
    fn drop(&mut self) {
        let armed = *self.armed.get_mut();
        if armed != 0 {
            DEFERRED_NEW_ARMED.fetch_sub(armed, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

/// How many methods may be remembered for a deferred-`new` retry at once.
const MAX_DEFERRED_NEW_RETRIES: usize = 4096;

/// How many times the re-offer sweep may ask one held memo's sites to resolve
/// before retiring it. See [`DeferredNewRetry::looks`].
///
/// Sixteen, because the event the memo waits for is a class DEFINITION and the
/// classes that resolve at all resolve within a handful of them -- the fixture
/// this path was built on (`bench/DeferredNewReoffer.java`) re-offers on the
/// first definition after the `touch()`. Sixteen leaves an order of magnitude
/// of headroom over that and still bounds the pathological case at 16 rather
/// than at the number of classes the program loads, which is unbounded.
///
/// `CRATONVM_JIT_DEFERRED_NEW_LOOKS=0` is the kill switch: it means UNBOUNDED
/// and restores the pre-2026-09-06 behaviour exactly. Any other number sets the
/// budget, which is what a bisect wants when the question is "how many looks
/// did this method need", not merely "is the budget the cause".
///
/// Declared as a JIT inventory row rather than a `SCALARS` entry: the scalar
/// list is the surface a USER has to learn (a path, a heap size, an encoding),
/// and `flag_surface.rs` guards its size on purpose. This is a compiler tuning
/// knob reached during a bisect, so it belongs in the group where the rest of
/// the JIT's levers are.
pub(crate) const MAX_DEFERRED_NEW_LOOKS: u32 = 16;

fn read_deferred_new_look_budget() -> u32 {
    cratonvm_types::flags::runtime_var("CRATONVM_JIT_DEFERRED_NEW_LOOKS")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(MAX_DEFERRED_NEW_LOOKS)
}

/// The look budget in force, cached in production. `0` => unbounded.
///
/// # There is no process-wide memo in a TEST binary, deliberately
///
/// The `OnceLock` is right in production -- the flag cannot change and this is
/// read per compile -- and wrong under `cfg(test)`, because
/// `flags::runtime_var` honours `flags::with_thread_overrides`, which is
/// THREAD-scoped. Memoizing a thread-scoped answer in a process-wide cell means
/// whichever test thread reads it first decides the budget for every other test
/// in the binary.
///
/// That had both of the consequences it has everywhere:
/// `the_zero_budget_restores_the_unbounded_behaviour` carried an early return
/// for the case where another test won the race, so it asserted NOTHING on most
/// runs; and on the runs where it won instead, it latched an unbounded budget
/// for the whole binary.
///
/// This is the same defect `ir_check_elim::enabled` was fixed for on
/// 2026-09-07, found by the same scan, and the rule it breaks is already
/// written down at `x64::osr::osr_empty_stack_entry_enabled`: "a process-wide
/// latch would also put this out of reach of `flags::with_thread_overrides`,
/// which is how a declared flag is arranged in a test."
pub(crate) fn deferred_new_look_budget() -> u32 {
    #[cfg(test)]
    {
        return read_deferred_new_look_budget();
    }
    #[cfg(not(test))]
    {
        use std::sync::OnceLock;
        static N: OnceLock<u32> = OnceLock::new();
        *N.get_or_init(read_deferred_new_look_budget)
    }
}

/// Record that this method's IR build bailed on a `new` site whose class was
/// not loaded at the time — a TRANSIENT refusal, not a property of the class
/// file.
///
/// `resolve_jit_new_site` deliberately never runs a user `ClassLoader.loadClass`
/// from inside a compile, so a `new` of a class nothing has touched yet reports
/// `JitNewSite::Deferred`, gets no `new_info` row, and the IR builder's `0xbb`
/// arm bails the whole method. The class then loads moments later — often on
/// the very next line of the compile log, because the constructor is the next
/// thing to run — and nothing ever looked again:
///
/// ```text
/// [ir] admission RJitGc.make(II)LRJitGc$Tree;: admitted to the optimizing pipeline
/// [ir] new-site DEFERRED: RJitGc$Tree ... loaded_anywhere=false
/// [ir] IrBuilder::build returned None for RJitGc.make(II)LRJitGc$Tree; — no IR body
/// ...
/// [ir] admission RJitGc$Tree.<init>(I)V: admitted to the optimizing pipeline   <- it is loaded now
/// ```
///
/// `make` never appears again: one attempt, single-pass for the life of the
/// process. That is the same shape as the code-buffer shortfall two functions
/// up — a measurement-like refusal that the next attempt would not repeat — and
/// it gets the same treatment.
///
/// The memo is consumed by [`take_deferred_new_retry`], so it grants exactly
/// ONE extra attempt: a class that is still not loaded on the retry bails
/// again, records nothing, and the method settles on single-pass as before.
///
/// The process-wide registry's entry point ([`DeferredNewRetries::note_bail`]
/// is the per-VM one). Test-only since interpreter round i1 wave 18, like the
/// free verdict functions since wave 12: no production caller remained, and
/// one would record into a memo no VM sweeps.
#[cfg(test)]
pub fn note_deferred_new_bail(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    deferred_sites: &[(u32, u16)],
) {
    process_deferred_new_retries().note_bail(
        cratonvm_types::ClassId::new(0),
        class_name,
        method_name,
        descriptor,
        deferred_sites,
    );
}

impl DeferredNewRetries {
    /// `note_deferred_new_bail` in this registry, remembering `class_id`
    /// (the compiled method's declaring class) for the re-offer sweep.
    pub fn note_bail(
        &self,
        class_id: cratonvm_types::ClassId,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        deferred_sites: &[(u32, u16)],
    ) {
        let h = compute_jit_key_hash(
            class_name,
            method_name,
            descriptor,
            cratonvm_types::ClassId::new(0),
        );
        let mut set = self.set.write();
        // `or_insert` and not `insert`: a method whose retry was already
        // granted stays at `1` and is never re-armed.
        // r11-tier: the cap counts memos HELD now, as its doc says ("at
        // once"). `set.len()` also counted every spent and retired memo, which
        // are never removed (a removal would let the compile door re-arm
        // them), so after 4096 methods had EVER bailed this way no later one
        // could be armed for the rest of the process. The map itself is still
        // bounded, at a larger multiple, and a spent or retired memo drops its
        // `sites`, keeping only its key.
        let held = self.held_count();
        if set.contains_key(&h)
            || (held < MAX_DEFERRED_NEW_RETRIES as u64
                && set.len() < MAX_DEFERRED_NEW_RETRIES.saturating_mul(16))
        {
            let armed = !set.contains_key(&h);
            set.entry(h).or_insert_with(|| DeferredNewRetry {
                state: 0,
                looks: 0,
                sites: deferred_sites.to_vec(),
                key: (
                    std::sync::Arc::from(class_name),
                    std::sync::Arc::from(method_name),
                    std::sync::Arc::from(descriptor),
                ),
                class_id,
            });
            if armed {
                self.arm_one();
                if cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_JITC") {
                    eprintln!(
                        "[cratonvm-jitc] deferred-new ARMED {class_name}.{method_name}{descriptor} ({} unresolved new site(s))",
                        deferred_sites.len(),
                    );
                }
            }
        }
    }

    /// Count one memo armed, here and in the process-wide sum.
    fn arm_one(&self) {
        self.armed
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        DEFERRED_NEW_ARMED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Count one memo spent or retired, here and in the process-wide sum.
    fn disarm_one(&self) {
        self.armed
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        DEFERRED_NEW_ARMED.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Number of retries armed and not yet spent in this registry. One
    /// relaxed load.
    pub fn held_count(&self) -> u64 {
        self.armed.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Every method whose retry is still HELD in this registry, with the
    /// declaring class its recorder named (`ClassId(0)` for none).
    #[allow(clippy::type_complexity)]
    pub fn held_methods(
        &self,
    ) -> Vec<(
        cratonvm_types::ClassId,
        std::sync::Arc<str>,
        std::sync::Arc<str>,
        std::sync::Arc<str>,
    )> {
        self.set
            .read()
            .values()
            .filter(|e| e.state == 0)
            .map(|e| {
                (
                    e.class_id,
                    e.key.0.clone(),
                    e.key.1.clone(),
                    e.key.2.clone(),
                )
            })
            .collect()
    }
}

/// Restore the historical BLIND deferred-`new` retry grant.
///
/// The grant used to flip its one-shot memo whether or not the class that
/// caused the bail had loaded, which on CratonBench lost every retry it granted
/// -- five `java/util/regex/Pattern` methods bailed twice each and then had no
/// retry left. `CRATONVM_JIT_DEFERRED_NEW_RETRY_BLIND=1` puts that back, as the
/// bisection lever for anything that looks like a method no longer reaching the
/// optimizing tier.
fn deferred_new_retry_blind() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    // `runtime_flag_on`, the shared truth table: the hand-rolled
    // `v != "0" && v != "false"` turned the lever ON for `off`, `no` and an
    // empty value (r11-tier).
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_DEFERRED_NEW_RETRY_BLIND")
    })
}

/// Take (clear) this method's one deferred-`new` retry, if it has one, in the
/// process-wide registry ([`DeferredNewRetries::take_retry`] is the per-VM
/// one).
///
/// The C1->C2 supersede door asks this: a method here is admitted to one more
/// optimizing attempt even though its bytecode contains a `new`, which
/// [`c2_upgrade_would_engage`] otherwise refuses without
/// `CRATONVM_JIT_C2_ALLOC_UPGRADE`. That refusal exists to avoid trading a
/// cheap inline TLAB bump for a more optimized body on a method whose
/// allocations escape anyway; it is not the right answer for a method that was
/// never given a chance to have its allocation looked at.
///
/// Test-only since interpreter round i1 wave 18 (see `note_deferred_new_bail`).
#[cfg(test)]
pub fn take_deferred_new_retry(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    site_resolves_now: &dyn Fn(u32, u16) -> bool,
) -> bool {
    process_deferred_new_retries().take_retry(
        class_name,
        method_name,
        descriptor,
        site_resolves_now,
    )
}

impl DeferredNewRetries {
    /// `take_deferred_new_retry` in this registry.
    pub fn take_retry(
        &self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        site_resolves_now: &dyn Fn(u32, u16) -> bool,
    ) -> bool {
        let h = compute_jit_key_hash(
            class_name,
            method_name,
            descriptor,
            cratonvm_types::ClassId::new(0),
        );
        // r11-tier: every published background compile asks, and almost none
        // is owed anything. `armed` counts exactly the `state == 0` memos and
        // is bumped under the write lock that inserts one, so zero here means
        // there is nothing this call could take — answered without the write
        // lock every compiler thread otherwise serialised on.
        if self.held_count() == 0 {
            return false;
        }
        let mut set = self.set.write();
        let Some(entry) = set.get_mut(&h) else {
            return false;
        };
        if entry.state != 0 {
            return false;
        }
        // The retry is for a TRANSIENT condition -- a `new` whose class had not
        // been loaded yet -- so spending it while that condition still holds
        // throws it away on an attempt guaranteed to bail exactly as the first
        // one did, and leaves nothing for the moment the class actually loads.
        // Ask first.
        //
        // An empty `sites` list is treated as "cannot tell" and grants, which
        // is the historical behaviour.
        let ready = deferred_new_retry_blind()
            || entry.sites.is_empty()
            || entry
                .sites
                .iter()
                .all(|&(holder, cp_idx)| site_resolves_now(holder, cp_idx));
        if !ready {
            // Charge the look, and retire the memo once the budget is gone. A
            // memo that is retired here is one whose class has failed to load
            // across sixteen class definitions; leaving it armed costs a
            // class-manager read lock per site on EVERY later definition and
            // buys a retry the evidence says will not be granted. `state = 2`
            // rather than a removal so `note_bail`'s `or_insert` still refuses
            // to re-arm it -- a retired memo must not come back through the
            // compile door and start the budget over.
            entry.looks = entry.looks.saturating_add(1);
            let budget = deferred_new_look_budget();
            if budget != 0 && entry.looks >= budget {
                entry.state = 2;
                // Nothing reads a settled memo's sites (both readers ask
                // `state` first); keep only the key that stops it being
                // re-armed.
                entry.sites = Vec::new();
                self.disarm_one();
                DEFERRED_NEW_RETIRED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_JITC") {
                    eprintln!(
                        "[cratonvm-jitc] deferred-new RETIRED {class_name}.{method_name}{descriptor} -- {} looks, class never loaded",
                        entry.looks,
                    );
                }
                return false;
            }
            if cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_JITC") {
                eprintln!(
                    "[cratonvm-jitc] deferred-new HELD {class_name}.{method_name}{descriptor} -- deferred class still unloaded; retry kept (look {}/{budget})",
                    entry.looks,
                );
            }
            DEFERRED_NEW_HELD.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return false;
        }
        entry.state = 1;
        entry.sites = Vec::new();
        self.disarm_one();
        if cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_JITC") {
            eprintln!("[cratonvm-jitc] deferred-new SPENT {class_name}.{method_name}{descriptor}");
        }
        DEFERRED_NEW_SPENT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        true
    }
}

/// Retries withheld because the deferred class was still not loaded, and
/// retries actually spent. A gate that never holds, or never grants, is a gate
/// that is not doing what it says.
static DEFERRED_NEW_HELD: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static DEFERRED_NEW_SPENT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Memos abandoned because their class did not load within the look budget.
///
/// Printed beside `held` and `spent` always, including as a zero: a zero here
/// cannot be told from "the budget is unbounded" or "nothing was ever armed"
/// unless it is on the line.
static DEFERRED_NEW_RETIRED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// `(held, spent, retired)` deferred-`new` retry decisions for this process.
pub fn deferred_new_retry_census() -> (u64, u64, u64) {
    use std::sync::atomic::Ordering::Relaxed;
    (
        DEFERRED_NEW_HELD.load(Relaxed),
        DEFERRED_NEW_SPENT.load(Relaxed),
        DEFERRED_NEW_RETIRED.load(Relaxed),
    )
}

/// How many deferred-`new` retries are currently HELD, as one relaxed load.
///
/// The sweep that re-offers them runs on every class definition, so its fast
/// path must not take the memo lock: during startup a lock per definition is a
/// cost paid by every program, to answer "nothing to do" for almost all of them.
static DEFERRED_NEW_ARMED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Number of retries armed and not yet spent, over EVERY registry in the
/// process (each VM's and the process-wide one). Cheap enough for a hot path:
/// the class-definition sweep asks it before it visits any VM.
pub fn held_deferred_new_count() -> u64 {
    DEFERRED_NEW_ARMED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Every method whose deferred-`new` retry is still HELD in the process-wide
/// registry, newest-first order unspecified. A VM asks its own
/// ([`DeferredNewRetries::held_methods`]).
///
/// The sweep that re-offers them runs on a class definition, not on the
/// method's own next compile: a method that bailed on an unloaded class has no
/// reason to be compiled again, so waiting for it to come back is waiting for
/// something that does not happen.
///
/// Test-only since interpreter round i1 wave 10: the VM's re-sweep reads its
/// own registry, so no production caller remains.
#[cfg(test)]
pub(crate) fn held_deferred_new_methods() -> Vec<(
    std::sync::Arc<str>,
    std::sync::Arc<str>,
    std::sync::Arc<str>,
)> {
    process_deferred_new_retries()
        .held_methods()
        .into_iter()
        .map(|(_, class_name, method_name, descriptor)| (class_name, method_name, descriptor))
        .collect()
}

/// Has `method_key` used up its [`MAX_CODE_BUFFER_RETRIES`] re-lowerings?
///
/// `try_compile` exempts the code-buffer bail from the permanent bail list so
/// the next attempt can run at the measured size. That exemption is only sound
/// while the retries are bounded — otherwise a method that can never fit is
/// re-lowered on every warmup-gate re-attempt for the life of the process.
pub fn code_buffer_retries_exhausted(method_key: &str) -> bool {
    if method_key.is_empty() {
        return false;
    }
    code_buffer_shortfalls()
        .read()
        .get(method_key)
        .is_some_and(|(_, attempts)| *attempts >= MAX_CODE_BUFFER_RETRIES)
}

/// Render a taken bail site for a diagnostic line.
pub(crate) fn format_jit_bail_site(site: Option<(&'static str, u32, u32)>) -> String {
    match site {
        Some((s, 0, 0)) => s.to_string(),
        Some((s, pc, op)) => format!("{s}(pc={pc},op=0x{op:02x})"),
        None => "unrecorded".to_string(),
    }
}

/// Interpreter round i1 wave 10, lane L5: the deferred-`new` memo is one per
/// registry (one per VM), and the process-wide armed count is their sum.
#[cfg(test)]
mod deferred_new_registry_tests {
    use super::{held_deferred_new_count, DeferredNewRetries};
    use cratonvm_types::ClassId;

    const NAMES: (&str, &str, &str) = ("w10/DeferredNewHolder", "make", "()V");

    #[test]
    fn a_retry_armed_in_one_registry_is_not_anothers() {
        let a = DeferredNewRetries::default();
        let b = DeferredNewRetries::default();
        a.note_bail(ClassId::new(7), NAMES.0, NAMES.1, NAMES.2, &[(7, 3)]);
        assert_eq!(a.held_count(), 1);
        assert_eq!(b.held_count(), 0, "another registry holds nothing");
        assert!(!b.take_retry(NAMES.0, NAMES.1, NAMES.2, &|_, _| true));
        let held = a.held_methods();
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].0, ClassId::new(7), "the recorder's class is kept");
        assert_eq!(&*held[0].2, NAMES.1);
        // Its own sites are asked, and a site that does not resolve holds it.
        assert!(!a.take_retry(NAMES.0, NAMES.1, NAMES.2, &|holder, cp_idx| {
            assert_eq!((holder, cp_idx), (7, 3));
            false
        }));
        assert!(a.take_retry(NAMES.0, NAMES.1, NAMES.2, &|_, _| true));
        assert_eq!(a.held_count(), 0);
        assert!(
            !a.take_retry(NAMES.0, NAMES.1, NAMES.2, &|_, _| true),
            "one-shot"
        );
    }

    /// A dropped registry takes its armed memos out of the process-wide sum,
    /// which the class-definition sweep's fast path reads. Other tests arm
    /// memos concurrently, so the sum is compared on this registry's share:
    /// it must be at least the armed count while alive, and a registry that
    /// armed and was dropped must not leave its count behind (checked by a
    /// second registry arming and dropping the same way, which would double
    /// the residue if the first had leaked).
    #[test]
    fn a_dropped_registry_leaves_nothing_in_the_process_wide_count() {
        let r = DeferredNewRetries::default();
        r.note_bail(ClassId::new(0), "w10/A", "m", "()V", &[]);
        r.note_bail(ClassId::new(0), "w10/B", "m", "()V", &[]);
        assert_eq!(r.held_count(), 2);
        assert!(held_deferred_new_count() >= 2);
        // `Drop` subtracts exactly what the registry still held.
        let mut r = r;
        assert_eq!(*r.armed.get_mut(), 2);
        drop(r);
    }
}
