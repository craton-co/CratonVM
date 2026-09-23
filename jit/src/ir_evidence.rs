// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! What the optimizing tier actually DID to a method, and whether that is
//! enough to justify replacing the baseline body.
//!
//! # The problem this answers
//!
//! `docs/JIT_OPTIMIZATION.md` has recorded for months that "nothing compares a
//! C2 body against the C1 body it replaces", and that the obvious static
//! metrics misjudge the good cases -- a bigger body is usually inlining or
//! unrolling, and more call sites can be a callee's own calls after its frame
//! was inlined away.
//!
//! Measured on the H2 JDBC workload on 2026-09-06: 161 supersedes, C2 bodies
//! **6% larger in aggregate** than the C1 bodies they replaced (81 bigger, 66
//! smaller, 14 the same), 58 ms of extra background compile, and switching
//! supersede off was the fastest arm in two independent interleaved runs. So
//! the tier was replacing bodies it had no reason to believe were better.
//!
//! # What is recorded, and what it is NOT
//!
//! A [`CompileRecord`]: a bitset of transforms the optimizing tier applied to
//! THIS compile, plus the per-execution costs it introduced.
//!
//! The bitset was the whole record until 2026-09-10, on the argument that "a
//! compile-time predicate cannot know whether the emitted body is faster". That
//! is true of the body as a whole and it was doing more work than it can bear:
//! a predicate cannot know the total, but it CAN know the price of the specific
//! trades this tier makes, because those prices are measured.
//!
//! The gap was not hypothetical. Splicing `Objects.checkIndex` into
//! `ArrayList.get` sets `Inlined`, which the list accepts. The call that splice
//! left behind is native-shadowed, so it lowered to a name resolution -- ~175 ns
//! on every execution against a direct `CALL`'s ~4 -- and the published body ran
//! its probe in 897 ms against the single-pass body's 338. **The transform was
//! the evidence that got a 3x regression published: the harmful change paid for
//! its own admission.** (`c2-splice-checkcast-and-instanceof-20260909.md`.)
//!
//! So evidence is now NECESSARY BUT NOT SUFFICIENT. The list still answers "did
//! this tier do something the baseline has no equivalent for"; the counts
//! answer "and what did that cost", and a body whose net per-execution cost
//! went up is refused however much it transformed. Refusing costs the
//! single-pass body. Accepting a regression costs every execution of it, and
//! nothing downstream measures that -- which is why the asymmetry is
//! deliberate.
//!
//! What the price does NOT model: site execution frequency. A resolution on a
//! cold branch is charged like one in a loop. That errs toward refusing, which
//! is the safe direction here, and it is the first thing to fix if this is ever
//! measured refusing bodies that were in fact better.
//!
//! When the transform list answers "no" the C2 body is a differently-emitted
//! version of the same computation carrying this tier's weaker register model,
//! which is the case there is no argument for publishing.
//!
//! The membership of [`is_worth_publishing`]'s list is a JUDGMENT, stated as
//! one so it can be argued with. Two entries are firm:
//!
//! * **Scalar replacement** deletes an allocation. Nothing the baseline tier
//!   emits recovers that.
//! * **Array guard elision** removes work from every iteration of a loop the
//!   baseline tier pays in full, because the baseline's bounds-check
//!   elimination is not reachable from this tier and this one's is new.
//!
//! Two are on the list because the transform has no baseline counterpart at the
//! same strength (**IR-tier splicing** nests six deep against the baseline's
//! three, and **late sinking** takes work out of a loop the baseline leaves in),
//! and they are the ones to revisit first if this gate is ever measured as
//! refusing bodies that were in fact better.
//!
//! # Thread model
//!
//! A thread-local, because one compile runs on one thread and the decision is
//! taken in `try_compile_inner`, on that same thread, immediately after
//! `ir_lower::lower_inner` returns. There is no cross-thread hand-off to get
//! wrong. [`begin_compile`] arms the slot so [`take`] can tell "this compile
//! did nothing" (`Some(0)`) from "no compile was recorded here" (`None`) -- the
//! distinction that keeps a plumbing mistake from silently disabling the whole
//! tier.

/// One transform. Values are bit positions, not a count.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transform {
    /// An allocation was scalar-replaced (`escape_analysis`).
    ScalarReplacement,
    /// A callee body was spliced into the graph (`CRATONVM_JIT_IR_INLINE`).
    Inlined,
    /// At least one array null or bounds check was proven unnecessary.
    GuardElided,
    /// A pure node was moved to a shallower loop nesting (`IR_SINK_LATE`).
    SunkLate,
    /// A counted loop was fully unrolled.
    Unrolled,
    /// Loop-invariant code was hoisted.
    Licm,
    /// A call-site intrinsic was lowered as arithmetic.
    ScalarIntrinsic,
    /// `ir_optimize`'s fixpoint loop actually removed nodes — constant folding,
    /// algebraic simplification, GVN or dead-store/dead-node elimination did
    /// something.
    ///
    /// DIAGNOSTIC ONLY, and deliberately absent from [`is_worth_publishing`].
    /// It exists to answer the question the refusal count raises rather than
    /// settles: the gate refused 587 bodies on one H2 run, and the evidence
    /// list omits exactly these passes, so "the gate is refusing bodies that
    /// did real work" and "the gate is refusing bodies that did nothing" are
    /// indistinguishable without it. `refused_but_simplified` in the census is
    /// the split.
    ///
    /// It WAS diagnostic-only when it landed, on the argument that the
    /// single-pass backend folds constants too. The measurement it produced
    /// refuted that argument the same day and promoted it to evidence — see
    /// [`is_worth_publishing`].
    Simplified,
}

impl Transform {
    const fn bit(self) -> u32 {
        1 << (self as u32)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Transform::ScalarReplacement => "scalar-replacement",
            Transform::Inlined => "inlined",
            Transform::GuardElided => "guard-elided",
            Transform::SunkLate => "sunk-late",
            Transform::Unrolled => "unrolled",
            Transform::Licm => "licm",
            Transform::ScalarIntrinsic => "scalar-intrinsic",
            Transform::Simplified => "simplified",
        }
    }

    pub const ALL: [Transform; 8] = [
        Transform::ScalarReplacement,
        Transform::Inlined,
        Transform::GuardElided,
        Transform::SunkLate,
        Transform::Unrolled,
        Transform::Licm,
        Transform::ScalarIntrinsic,
        Transform::Simplified,
    ];
}

/// What one compile did, as both the transform bitset and the per-execution
/// costs it introduced.
///
/// The bitset alone was the whole record until 2026-09-10, and the gap that
/// closed is in [`is_worth_publishing`]: a bit says a transform HAPPENED, and a
/// transform that happened can still have made the body slower. The counts
/// below are the two sides of the one trade this tier is known to lose.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CompileRecord {
    /// Transforms applied, as [`Transform`] bit positions.
    pub bits: u32,
    /// Callee frames removed by splicing in this compile. Each saves roughly
    /// one direct `CALL` per execution.
    pub spliced_frames: u32,
    /// Calls this body lowered to `jit_invoke_dispatch` -- a name resolution on
    /// EVERY execution -- at sites that exist only because a callee body was
    /// spliced in. `own_code` blind dispatches are deliberately not counted:
    /// the method's own megamorphic or unbindable sites resolve by name in
    /// either tier, so they are not something publishing this body causes.
    pub blind_dispatches_in_splice: u32,
}

impl CompileRecord {
    /// Estimated per-execution cost this compile ADDED, in nanoseconds.
    /// Negative is an improvement.
    ///
    /// Both constants are measured and already load-bearing elsewhere in this
    /// crate: a `jit_invoke_dispatch` resolves its callee by name at ~175 ns
    /// against a direct `CALL`'s ~4 (see `ir_lower`'s blind-dispatch arm, and
    /// `InlineInvokeTarget::direct_entry`, which states the same trade as
    /// "removing a ~4 ns frame does not pay for a ~175 ns downgrade").
    ///
    /// The model weights every site equally, which is its main limitation: a
    /// blind dispatch on a cold branch is charged the same as one in a loop.
    /// It errs toward refusing, and the asymmetry is deliberate -- the refused
    /// body is merely the single-pass one, while the accepted body is a
    /// regression nothing downstream measures.
    pub fn added_ns_per_execution(self) -> i64 {
        const BLIND_DISPATCH_NS: i64 = 175;
        const DIRECT_CALL_NS: i64 = 4;
        i64::from(self.blind_dispatches_in_splice) * (BLIND_DISPATCH_NS - DIRECT_CALL_NS)
            - i64::from(self.spliced_frames) * DIRECT_CALL_NS
    }
}

thread_local! {
    /// A STACK of armed compiles, innermost last. Empty means nothing is armed.
    ///
    /// A stack rather than one cell, and the reason is measured: with IR-tier
    /// inlining on, the H2 JDBC workload reported `unjudged=190` -- 190 gate
    /// decisions taken with no evidence recorded -- because a compile can enter
    /// this function again on the same thread before the outer one finishes.
    /// A single cell made the inner `take` disarm the OUTER compile, which then
    /// read `None` and was waved through unjudged.
    ///
    /// With a stack, an inner compile's transforms are attributed to the inner
    /// compile and the outer one keeps its own bits. The `unjudged` counter is
    /// what found this; it is the reason the fail-open case is counted rather
    /// than silently accepted.
    static STACK: std::cell::RefCell<Vec<CompileRecord>> = const {
        std::cell::RefCell::new(Vec::new())
    };
}

/// Arm a new optimizing compile on this thread. Nests.
pub fn begin_compile() {
    STACK.with(|s| s.borrow_mut().push(CompileRecord::default()));
}

/// An armed compile that disarms itself if it is left without reaching
/// [`take`].
///
/// [`begin_compile`] alone leaked: every bail between arming and the
/// acceptance `take()` left its entry on the stack, and the next OUTER compile
/// on the thread then popped that stale inner entry as its own, and was judged
/// — and possibly memoised as refused — on another method's evidence. The
/// scope records the stack depth it armed at and truncates back to it on drop;
/// after a normal `take()` the stack is already there, so the drop does nothing.
#[must_use = "dropping the scope immediately disarms the compile"]
pub struct CompileScope {
    depth: usize,
}

/// [`begin_compile`], returning the scope that disarms it on every exit.
pub fn begin_compile_scope() -> CompileScope {
    let depth = STACK.with(|s| {
        let mut v = s.borrow_mut();
        let depth = v.len();
        v.push(CompileRecord::default());
        depth
    });
    CompileScope { depth }
}

impl Drop for CompileScope {
    fn drop(&mut self) {
        // `try_with`: a scope dropped during thread teardown must not panic.
        let _ = STACK.try_with(|s| {
            if let Ok(mut v) = s.try_borrow_mut() {
                if v.len() > self.depth {
                    v.truncate(self.depth);
                }
            }
        });
    }
}

/// Record that `t` was applied to the compile running on this thread.
///
/// A call with the slot un-armed is IGNORED rather than armed implicitly: the
/// transforms below also run from unit tests and from the single-pass tier's
/// own passes, and letting either arm the slot would attribute their work to
/// whatever compile came next on the same thread.
#[inline]
pub fn note(t: Transform) {
    // Process-wide FIRST, and unconditionally — before the armed-slot test.
    //
    // # Why this counter exists, and why it is not the per-compile record
    //
    // `CompileRecord` answers "did THIS compile do anything worth publishing",
    // which is the acceptance gate's question. Nobody could ask the other one:
    // *how often does transform X fire across a run*. That gap has a cost on
    // the record. The 2026-08-27 scalar-deopt gauntlet soak
    // (`internal/performance/scalar-deopt-gauntlet-soak-20260827.md`) found
    // `CRATONVM_SCALAR_DEOPT` "green and inert" — 30,392 allocation-bearing IR
    // compiles across netty and hibernate producing ZERO scalar replacements —
    // and reaching that reading needed a purpose-built instrumented run,
    // because the tree had no counter that could say it. A flag whose soak
    // cannot distinguish "ran and was neutral" from "never ran" cannot be
    // flipped or retired, and `CRATONVM_SCALAR_DEOPT` has been stuck in
    // exactly that state since.
    //
    // Deliberately outside the `last_mut()` guard: `note`'s own doc explains
    // that an un-armed call is IGNORED so a unit test cannot attribute its work
    // to the next real compile, and that is right for ATTRIBUTION. It is wrong
    // for a census, whose question is "did this fire at all". Tests move the
    // census; that is the intended reading and this comment is the warning not
    // to "fix" it by moving the line inside.
    TRANSFORM_CENSUS[t as usize].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    STACK.with(|s| {
        if let Ok(mut v) = s.try_borrow_mut() {
            if let Some(rec) = v.last_mut() {
                rec.bits |= t.bit();
                if matches!(t, Transform::Inlined) {
                    rec.spliced_frames = rec.spliced_frames.saturating_add(1);
                }
            }
        }
    });
}

/// How many times each [`Transform`] has been noted in this process, indexed by
/// the variant's discriminant (`t as usize`, which is also its position in
/// [`Transform::ALL`]).
static TRANSFORM_CENSUS: [std::sync::atomic::AtomicU64; Transform::ALL.len()] =
    [const { std::sync::atomic::AtomicU64::new(0) }; Transform::ALL.len()];

/// `(name, count)` for every transform, in [`Transform::ALL`] order.
///
/// The instrument the scalar-deopt soak needed and did not have. A ZERO here is
/// the meaningful reading — it says the transform never fired, which is what
/// separates "this optimization is neutral on this workload" from "this
/// optimization did not run". `release_max_level_info` compiles `debug!` and
/// `trace!` away, so a counter whose only reader is a log line cannot answer
/// this on a release binary; this is a reader.
pub fn transform_census() -> Vec<(&'static str, u64)> {
    Transform::ALL
        .iter()
        .map(|t| {
            (
                t.as_str(),
                TRANSFORM_CENSUS[*t as usize].load(std::sync::atomic::Ordering::Relaxed),
            )
        })
        .collect()
}

/// The count for one transform, for a test or a targeted probe.
pub fn transform_count(t: Transform) -> u64 {
    TRANSFORM_CENSUS[t as usize].load(std::sync::atomic::Ordering::Relaxed)
}

/// Print the transform census on the VM's shutdown path, under
/// `CRATONVM_JIT_TRANSFORM_CENSUS=1`.
///
/// Without this, [`transform_census`] is a counter with no reader — the very
/// shape this census exists to expose elsewhere in the JIT. Called from BOTH
/// exit arms in `vm-cli`, because a run that ends in an error is at least as
/// interesting as one that returns `Ok`, and from `maybe_dump_shutdown_reports`,
/// because a JUnit runner leaves through `System.exit` and reaches neither.
/// `Once`-guarded so that is safe.
///
/// Also prints the admission funnel ([`admission_funnel_line`]): the
/// population the transforms were applied to.
///
/// Every transform is printed, including the zeroes: a zero is the reading
/// that distinguishes "this pass ran and changed nothing" from "this pass
/// never ran", and that distinction is the whole reason the counter exists.
pub fn exit_summary() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        if !cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_TRANSFORM_CENSUS") {
            return;
        }
        let census = transform_census();
        let total: u64 = census.iter().map(|(_, n)| *n).sum();
        eprintln!(
            "[jit-transform-census] TOTALS applied={total} over {} transforms",
            census.len()
        );
        // The population the transforms were applied TO. A transform count
        // read without it cannot say whether the tier saw most of the
        // workload or a sliver of it.
        eprintln!(
            "[jit-transform-census] admission funnel: {}",
            admission_funnel_line()
        );
        // The scalar-replacement funnel, beside the transform it feeds: the
        // `scalar-replacement` row counts allocations ELIDED, and a zero there
        // cannot on its own say whether escape analysis offered nothing or the
        // planner refused everything it offered.
        let (candidates, plans) = crate::scalar_replacement_census();
        eprintln!(
            "[jit-transform-census] scalar-replacement funnel: \
             candidates={candidates} planned={plans}"
        );
        for (name, n) in census {
            eprintln!("[jit-transform-census]   {name} = {n}");
        }
        // WHY scalar replacement refused, which is the question `candidates=0`
        // raises and cannot answer. Zero rows are printed too: the shape of the
        // distribution is the finding, and a reason that never fires is as
        // informative as one that always does.
        let (analyses, allocations) = crate::escape_analysis::scalar_scope_census();
        eprintln!(
            "[jit-transform-census] scalar-replacement scope: analyses={analyses} \
             allocation-nodes-seen={allocations}"
        );
        eprintln!("[jit-transform-census] scalar-replacement refusals by reason:");
        for (name, n) in crate::escape_analysis::scalar_refusal_census() {
            eprintln!("[jit-transform-census]   refused[{name}] = {n}");
        }
    });
}

/// Record that this compile lowered a call inside a SPLICED body to the blind
/// dispatch helper. Ignored when nothing is armed, for `note`'s reason.
///
/// This is the cost side of the splice trade, and it is counted here rather
/// than read from `ir_lower`'s process-wide census because the gate judges ONE
/// compile and that census is cumulative across all of them.
#[inline]
pub fn note_blind_dispatch_in_splice() {
    STACK.with(|s| {
        if let Ok(mut v) = s.try_borrow_mut() {
            if let Some(rec) = v.last_mut() {
                rec.blind_dispatches_in_splice = rec.blind_dispatches_in_splice.saturating_add(1);
            }
        }
    });
}

/// Pop the innermost armed compile. `None` means none was armed on this
/// thread, which a caller must treat as "cannot judge" rather than as "did
/// nothing".
pub fn take() -> Option<CompileRecord> {
    STACK.with(|s| s.borrow_mut().pop())
}

/// Render a bitset for a diagnostic line.
pub fn describe(bits: u32) -> String {
    let names: Vec<&str> = Transform::ALL
        .iter()
        .filter(|t| bits & t.bit() != 0)
        .map(|t| t.as_str())
        .collect();
    if names.is_empty() {
        "none".to_string()
    } else {
        names.join(",")
    }
}

/// Is a body carrying this evidence worth replacing the baseline body with?
///
/// See the module header for why this list is a judgment and which two entries
/// to revisit first. `Unrolled` and `Licm` are deliberately ABSENT: the
/// single-pass backend has both (`x64/loop_unroll_admission.rs` unrolls 4x,
/// `x64/licm.rs` hoists), so their presence in a C2 body is not evidence the C2
/// body is the better one.
pub fn is_worth_publishing(rec: CompileRecord) -> bool {
    // Evidence is NECESSARY but no longer SUFFICIENT (2026-09-10).
    //
    // The list below answers "did this tier do something the baseline has no
    // equivalent for". It cannot answer "and did that make the body faster",
    // and the difference is not hypothetical. `Objects.checkIndex` spliced into
    // `ArrayList.get` set `Inlined`, which this list accepts; the call that
    // splice left behind was native-shadowed, so it lowered to a name
    // resolution, and the published body ran the probe in 897 ms against the
    // single-pass body's 338. The transform was the evidence that got a 3x
    // regression published -- the harmful change paid for its own admission.
    // See `c2-splice-checkcast-and-instanceof-20260909.md`.
    //
    // So the trade is now priced, with the same measured constants the splice
    // path already reasons in, and a body whose net per-execution cost went UP
    // is refused however much it transformed. Refusing costs the single-pass
    // body; accepting a regression costs every execution, and nothing
    // downstream measures it.
    if rec.added_ns_per_execution() > 0 {
        return false;
    }
    is_worth_publishing_bits(rec.bits)
}

/// The transform-list half of the judgment, split out so it can be tested and
/// argued with on its own. Callers want [`is_worth_publishing`].
pub fn is_worth_publishing_bits(bits: u32) -> bool {
    const WORTH: u32 = Transform::ScalarReplacement.bit()
        | Transform::Inlined.bit()
        | Transform::GuardElided.bit()
        | Transform::SunkLate.bit()
        | Transform::ScalarIntrinsic.bit()
        // Added 2026-09-06 BY MEASUREMENT, against the argument written here
        // when `Simplified` landed as a diagnostic.
        //
        // That argument was: "the single-pass backend folds constants too, so a
        // graph getting smaller says the optimizer ran, not that its output
        // beats C1's." It is a reasonable argument and it is wrong. The split
        // this bit was added to produce came back 296 simplified against 284
        // inert on H2 -- half the refusals had `ir_optimize` genuinely remove
        // nodes -- and an A/B of the gate against `CRATONVM_C2_ACCEPT=always`
        // then measured the gate LOSING: 2.315 s against 2.237 s of wall clock,
        // 3.4%, with `always` winning 15 of 21 position-balanced rounds on both
        // wall and CPU, against a control pair agreeing to 0.4% on the means.
        //
        // So the bodies the narrow list refused were better, and the list was
        // the thing costing throughput. Admitting `Simplified` keeps the half
        // that is provably inert -- 284 methods where the optimizer removed
        // NOTHING -- refused, so the abandon path still saves their epoch
        // bumps, while stopping the gate throwing away the half that did work.
        | Transform::Simplified.bit();
    bits & WORTH != 0
}

/// How the C1→C2 acceptance gate behaves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AcceptPolicy {
    /// Publish every C2 body that lowers. The behaviour before 2026-09-06.
    Always,
    /// Publish only a body carrying evidence [`is_worth_publishing`] accepts.
    Evidence,
    /// Publish none. Equivalent to `CRATONVM_C2_SUPERSEDE=0` at this door, and
    /// present so the three arms can be measured from one binary.
    Never,
}

/// One exemption exists and it is a failure of the gate's PREMISE rather than a
/// special case: "if C2 applied nothing C1 lacks, C1's body is at least as
/// good" assumes there is a C1 body. `promote_scalar_selfrec_to_ir` reaches the
/// optimizing tier with no predecessor -- deliberately, because compiling the
/// narrow `static int f(int)` self-recursion shape as C1 first strands
/// recursive frames in the slower body -- and `fib` is pure arithmetic, so it
/// produces no evidence at all. `try_compile_inner` asks
/// `scalar_selfrec_ir_would_engage`, the same predicate the VM used to reach
/// that door, and skips the gate for it.
///
/// `CRATONVM_C2_ACCEPT=always|evidence|never`, default `evidence`.
///
/// **This is a policy, and the default is a judgment made on one workload.**
/// The H2 measurement that motivates it is in the module header. It should be
/// re-taken on a second real application before this default is treated as
/// settled, and `=always` is exactly the arm to take it against.
pub fn accept_policy() -> AcceptPolicy {
    #[cfg(test)]
    {
        if let Some(forced) = ACCEPT_FORCE.with(|c| c.get()) {
            return forced;
        }
    }
    if PROCESS_FORCE_ALWAYS.load(std::sync::atomic::Ordering::Relaxed) {
        return AcceptPolicy::Always;
    }
    use std::sync::OnceLock;
    static P: OnceLock<AcceptPolicy> = OnceLock::new();
    *P.get_or_init(|| {
        match cratonvm_types::flags::runtime_var("CRATONVM_C2_ACCEPT").as_deref() {
            Ok("always") => AcceptPolicy::Always,
            Ok("never") => AcceptPolicy::Never,
            // An unrecognised value takes the default rather than failing the
            // VM: this is a diagnostic lever, and a typo in it should not be
            // the reason a program does not start.
            _ => AcceptPolicy::Evidence,
        }
    })
}

/// Bodies accepted and refused by the gate, and the refusals' reason.
static ACCEPTED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static REFUSED_NO_EVIDENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static REFUSED_POLICY: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static UNJUDGED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Apply the gate to a finished optimizing compile. `true` keeps the body.
///
/// `evidence` is [`take`]'s answer. A `None` — no compile recorded on this
/// thread — ACCEPTS and counts `unjudged`, because a plumbing mistake must
/// cost an unfiltered publish, never a silently disabled tier. A non-zero
/// `unjudged` in the census is the signal that the recording is broken.
pub fn accept(evidence: Option<CompileRecord>) -> bool {
    use std::sync::atomic::Ordering::Relaxed;
    match accept_policy() {
        AcceptPolicy::Always => {
            ACCEPTED.fetch_add(1, Relaxed);
            true
        }
        AcceptPolicy::Never => {
            REFUSED_POLICY.fetch_add(1, Relaxed);
            false
        }
        AcceptPolicy::Evidence => match evidence {
            None => {
                UNJUDGED.fetch_add(1, Relaxed);
                ACCEPTED.fetch_add(1, Relaxed);
                true
            }
            // Priced as a regression: refused whatever it transformed, and
            // counted apart from the no-evidence refusals because the two mean
            // opposite things about the tier. A no-evidence refusal says the
            // tier did nothing; this one says it did something harmful.
            Some(rec) if rec.added_ns_per_execution() > 0 => {
                REFUSED_COST_REGRESSION.fetch_add(1, Relaxed);
                REGRESSION_NS_REFUSED.fetch_add(
                    u64::try_from(rec.added_ns_per_execution()).unwrap_or(0),
                    Relaxed,
                );
                false
            }
            Some(rec) if is_worth_publishing(rec) => {
                ACCEPTED.fetch_add(1, Relaxed);
                true
            }
            Some(rec) => {
                let bits = rec.bits;
                REFUSED_NO_EVIDENCE.fetch_add(1, Relaxed);
                // Split the refusals by whether the optimizer did ANYTHING.
                // See `Transform::Simplified` for why this is the number the
                // refusal count needs beside it.
                if bits & Transform::Simplified.bit() != 0 {
                    REFUSED_BUT_SIMPLIFIED.fetch_add(1, Relaxed);
                } else {
                    REFUSED_AND_INERT.fetch_add(1, Relaxed);
                }
                false
            }
        },
    }
}

/// Bodies refused because their priced per-execution cost went UP, and the
/// total nanoseconds-per-execution those refusals declined to publish.
static REFUSED_COST_REGRESSION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static REGRESSION_NS_REFUSED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// `(bodies refused as a cost regression, ns/execution they would have added)`.
///
/// A non-zero left-hand number is the gate doing the job the transform list
/// alone could not: refusing a body that DID transform and was slower for it.
pub fn cost_regression_census() -> (u64, u64) {
    use std::sync::atomic::Ordering::Relaxed;
    (
        REFUSED_COST_REGRESSION.load(Relaxed),
        REGRESSION_NS_REFUSED.load(Relaxed),
    )
}

static REFUSED_BUT_SIMPLIFIED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static REFUSED_AND_INERT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// `(refused_but_the_optimizer_simplified, refused_and_the_optimizer_did_nothing)`.
///
/// The two together are `refused_no_evidence`. A large left-hand number means
/// the evidence list is too narrow and the gate is refusing bodies the
/// optimizer genuinely worked on; a large right-hand number means the tier
/// really is inert on those methods and the gate is right.
pub fn refusal_split() -> (u64, u64) {
    use std::sync::atomic::Ordering::Relaxed;
    (
        REFUSED_BUT_SIMPLIFIED.load(Relaxed),
        REFUSED_AND_INERT.load(Relaxed),
    )
}

/// `(accepted, refused_no_evidence, refused_by_policy, unjudged)`.
pub fn census() -> (u64, u64, u64, u64) {
    use std::sync::atomic::Ordering::Relaxed;
    (
        ACCEPTED.load(Relaxed),
        REFUSED_NO_EVIDENCE.load(Relaxed),
        REFUSED_POLICY.load(Relaxed),
        UNJUDGED.load(Relaxed),
    )
}

// ── The admission funnel ─────────────────────────────────────────────

/// Where one optimizing (C2) request left the IR tier.
///
/// [`census`] counts only the LAST of the four independent ways a method is
/// sent to the single-pass emitter instead of the tier it was offered to. The
/// other three -- `ir_compatible_sized`'s caps and the rest of the admission
/// conjunction, `IR_MAX_GRAPH_NODES`, and a decline anywhere in
/// build/optimize/verify/schedule/lower -- produce the identical observable (a
/// single-pass body), so "how much of this workload does the optimizing tier
/// actually see?" had no answer. That is how a regression test for an IR-tier
/// miscompile came to pass on three methods that never reached the tier; see
/// `docs/internal/fixed-bugs/jit/ir-tier-admission-hides-its-own-defects-FIXED-20260918.md`.
///
/// Counted exactly once per optimizing request that got past the refusal memo,
/// at the driver in `try_compile_inner`, so the rows sum to those requests.
/// Memo skips are [`memo_skips`] and are printed beside them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdmissionExit {
    /// `ir_compatible_sized` refused: a per-method cap (invokes, field ops,
    /// `new` sites, array allocations, bytecode bytes) or an opcode the builder
    /// has no arm for.
    IrCompatible,
    /// Another conjunct of the admission gate refused: moving young, a
    /// single-pass-only lowering, the String-intrinsic pin, the exception-table
    /// and precise-frame terms, or the value shape.
    OtherGate,
    /// The graph was built and exceeded `IR_MAX_GRAPH_NODES`.
    NodeCap,
    /// Declined inside the pipeline: the builder, the optimizer, the verifier,
    /// the scheduler or the lowerer.
    Pipeline,
    /// Lowered, then refused by [`accept`] -- the quality judgement, not a
    /// capability one, and so the refusal that hides small leaf methods.
    Acceptance,
    /// Published: the optimizing tier's body is the one installed.
    Published,
}

impl AdmissionExit {
    /// Every exit, in funnel order.
    pub const ALL: [AdmissionExit; 6] = [
        AdmissionExit::IrCompatible,
        AdmissionExit::OtherGate,
        AdmissionExit::NodeCap,
        AdmissionExit::Pipeline,
        AdmissionExit::Acceptance,
        AdmissionExit::Published,
    ];

    /// Stable census key.
    pub fn as_str(self) -> &'static str {
        match self {
            AdmissionExit::IrCompatible => "refused-ir-compatible",
            AdmissionExit::OtherGate => "refused-other-gate",
            AdmissionExit::NodeCap => "refused-node-cap",
            AdmissionExit::Pipeline => "declined-in-pipeline",
            AdmissionExit::Acceptance => "refused-acceptance",
            AdmissionExit::Published => "published",
        }
    }
}

static ADMISSION_EXITS: [std::sync::atomic::AtomicU64; AdmissionExit::ALL.len()] =
    [const { std::sync::atomic::AtomicU64::new(0) }; AdmissionExit::ALL.len()];

thread_local! {
    /// The stage that declined the IR attempt now running on this thread.
    /// `ir_tier` returns a bare `None` from dozens of sites, so the two stages
    /// that can be named without touching them -- the node cap and the
    /// acceptance gate -- overwrite the default `Pipeline` in place, and the
    /// driver reads the mark back once `ir_tier` returns. Saved and restored
    /// around each attempt ([`arm_ir_exit_mark`] / [`take_ir_exit_mark`]),
    /// because a nested compile on the same thread would otherwise leave ITS
    /// verdict behind for the outer one.
    static IR_EXIT_MARK: std::cell::Cell<AdmissionExit> =
        const { std::cell::Cell::new(AdmissionExit::Pipeline) };
}

/// Count one optimizing request's exit. Called by the driver only.
pub fn note_admission_exit(exit: AdmissionExit) {
    ADMISSION_EXITS[exit as usize].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// Name the stage declining the IR attempt in progress on this thread.
pub fn mark_ir_exit(exit: AdmissionExit) {
    IR_EXIT_MARK.with(|c| c.set(exit));
}

/// Arm the mark for an IR attempt. Returns the enclosing attempt's mark, which
/// the caller hands back to [`take_ir_exit_mark`].
pub fn arm_ir_exit_mark() -> AdmissionExit {
    IR_EXIT_MARK.with(|c| c.replace(AdmissionExit::Pipeline))
}

/// Read this attempt's mark and restore the enclosing one.
pub fn take_ir_exit_mark(enclosing: AdmissionExit) -> AdmissionExit {
    IR_EXIT_MARK.with(|c| c.replace(enclosing))
}

/// `(exit, count)` for every [`AdmissionExit`], in funnel order, zeroes
/// included: a zero row is what separates "this gate never fires" from "this
/// gate is not counted".
pub fn admission_funnel() -> Vec<(&'static str, u64)> {
    AdmissionExit::ALL
        .iter()
        .map(|e| {
            (
                e.as_str(),
                ADMISSION_EXITS[*e as usize].load(std::sync::atomic::Ordering::Relaxed),
            )
        })
        .collect()
}

/// The funnel as one line, for the two exit printers.
pub fn admission_funnel_line() -> String {
    let rows = admission_funnel();
    let requests: u64 = rows.iter().map(|(_, n)| *n).sum();
    let detail = rows
        .iter()
        .map(|(name, n)| format!("{name}={n}"))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "optimizing requests={requests} memo_skips={} {detail}",
        memo_skips()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A nested IR attempt on the same thread must not leave its exit for the
    /// outer one. The outer attempt below declines in the pipeline AFTER a
    /// nested attempt was refused by the acceptance gate; without the
    /// save/restore the outer one would read `Acceptance` and the funnel would
    /// file a pipeline decline as a quality refusal.
    #[test]
    fn a_nested_attempt_does_not_leak_its_exit_into_the_outer_one() {
        let outer = arm_ir_exit_mark();
        {
            let inner = arm_ir_exit_mark();
            mark_ir_exit(AdmissionExit::Acceptance);
            assert_eq!(take_ir_exit_mark(inner), AdmissionExit::Acceptance);
        }
        assert_eq!(take_ir_exit_mark(outer), AdmissionExit::Pipeline);

        let outer = arm_ir_exit_mark();
        {
            let inner = arm_ir_exit_mark();
            assert_eq!(take_ir_exit_mark(inner), AdmissionExit::Pipeline);
        }
        mark_ir_exit(AdmissionExit::NodeCap);
        assert_eq!(take_ir_exit_mark(outer), AdmissionExit::NodeCap);
    }

    /// Every exit has a row, zeroes included, in funnel order.
    #[test]
    fn the_funnel_reports_every_exit() {
        let names: Vec<&str> = admission_funnel().into_iter().map(|(n, _)| n).collect();
        assert_eq!(
            names,
            AdmissionExit::ALL
                .iter()
                .map(|e| e.as_str())
                .collect::<Vec<_>>()
        );
        assert!(admission_funnel_line().starts_with("optimizing requests="));
    }

    /// Drain any residue a sibling test left on this thread.
    fn drain() {
        while take().is_some() {}
    }

    /// The armed/un-armed distinction is the whole safety argument, so it is
    /// the first thing tested: an un-armed `note` must not leak into the next
    /// compile on the same thread.
    #[test]
    fn an_unarmed_note_is_ignored_and_does_not_leak() {
        drain();
        note(Transform::Inlined);
        assert_eq!(take(), None, "a note with no armed compile records nothing");
        begin_compile();
        assert_eq!(
            take(),
            Some(CompileRecord::default()),
            "an armed compile that did nothing reads as an empty record, not None",
        );
    }

    /// A compile that begins INSIDE another must not disarm it. With one cell
    /// instead of a stack, the H2 JDBC workload reported `unjudged=190` -- the
    /// outer compile read `None` and the gate waved it through without judging.
    #[test]
    fn a_nested_compile_does_not_disarm_the_outer_one() {
        drain();
        begin_compile();
        note(Transform::Inlined);
        // ...an inner compile starts and finishes...
        begin_compile();
        note(Transform::GuardElided);
        let inner = take().expect("the inner compile is armed");
        assert_eq!(
            describe(inner.bits),
            "guard-elided",
            "the inner compile must see ONLY its own transforms",
        );
        let outer = take().expect("the outer compile must still be armed");
        assert_eq!(
            describe(outer.bits),
            "inlined",
            "the outer compile must keep its own bits across a nested one",
        );
        assert_eq!(take(), None, "and the stack is empty again");
    }

    #[test]
    fn bits_accumulate_and_describe() {
        drain();
        begin_compile();
        note(Transform::Inlined);
        note(Transform::GuardElided);
        let rec = take().expect("armed");
        assert!(is_worth_publishing(rec));
        let d = describe(rec.bits);
        assert!(d.contains("inlined") && d.contains("guard-elided"), "{d}");
    }

    /// `Unrolled` and `Licm` alone are NOT evidence, because the single-pass
    /// backend has both. This is the judgment the module header states, pinned
    /// so that changing it is a deliberate edit rather than a drift.
    #[test]
    fn a_transform_the_baseline_tier_also_has_is_not_evidence() {
        drain();
        begin_compile();
        note(Transform::Unrolled);
        note(Transform::Licm);
        let rec = take().expect("armed");
        assert!(
            !is_worth_publishing(rec),
            "the baseline tier unrolls 4x and hoists; doing the same is not a \
             reason to replace its body",
        );
    }

    /// THE REGRESSION THIS GATE MISSED. `Inlined` is on the evidence list, so
    /// the bitset alone accepts this body -- and the body is slower, because
    /// the splice left behind a call that resolves by name on every execution.
    /// Measured shape: `Objects.checkIndex` spliced into `ArrayList.get`,
    /// 897 ms against a single-pass 338 ms.
    #[test]
    fn a_transform_that_made_the_body_slower_is_refused() {
        drain();
        begin_compile();
        note(Transform::Inlined);
        note_blind_dispatch_in_splice();
        let rec = take().expect("armed");
        assert!(
            is_worth_publishing_bits(rec.bits),
            "the transform list alone still accepts it -- that is the bug",
        );
        assert!(
            rec.added_ns_per_execution() > 0,
            "one name resolution against one saved frame is a regression",
        );
        assert!(
            !is_worth_publishing(rec),
            "the priced judgment must refuse a body its own transform made slower",
        );
    }

    /// The refusal is a PRICE, not a veto on the opcode: enough saved frames
    /// pay for a resolution, and the gate must let that through rather than
    /// treating any blind dispatch as fatal.
    #[test]
    fn enough_saved_frames_pay_for_a_resolution() {
        drain();
        begin_compile();
        note(Transform::Inlined);
        note_blind_dispatch_in_splice();
        // 171 ns of regression against 4 ns a frame: 43 frames is the turn.
        for _ in 0..50 {
            note(Transform::Inlined);
        }
        let rec = take().expect("armed");
        assert!(rec.added_ns_per_execution() <= 0, "{rec:?}");
        assert!(
            is_worth_publishing(rec),
            "a paid-for trade is still publishable"
        );
    }

    /// A blind dispatch in the method's OWN code is not this compile's doing --
    /// an unbindable or megamorphic site resolves by name in either tier -- so
    /// it must not be charged here. Only `note_blind_dispatch_in_splice` counts.
    #[test]
    fn only_the_splices_own_resolutions_are_charged() {
        drain();
        begin_compile();
        note(Transform::Inlined);
        let rec = take().expect("armed");
        assert_eq!(rec.blind_dispatches_in_splice, 0);
        assert!(
            rec.added_ns_per_execution() < 0,
            "a splice that left no resolution behind is a straight saving",
        );
        assert!(is_worth_publishing(rec));
    }

    /// `TRANSFORM_CENSUS` is an array of `ALL.len()` counters indexed by the
    /// discriminant, from `note` — which runs on every compile thread. A
    /// variant added to the enum but not to `ALL` would index past the end and
    /// PANIC there, and one inserted out of order would count under another
    /// transform's name. Pin both.
    #[test]
    fn every_transform_indexes_its_own_census_row() {
        for (i, t) in Transform::ALL.iter().enumerate() {
            assert_eq!(*t as usize, i, "{t:?} is out of place in Transform::ALL");
        }
        assert_eq!(
            Transform::Simplified as usize + 1,
            Transform::ALL.len(),
            "the last variant must be the last entry of ALL",
        );
    }

    /// A plumbing mistake must cost an unfiltered publish, not a disabled tier.
    #[test]
    fn an_unrecorded_compile_is_accepted_and_counted() {
        drain();
        let before = census().3;
        // Only meaningful under the default policy; another test may have
        // latched a different one into the `OnceLock`.
        if accept_policy() == AcceptPolicy::Evidence {
            assert!(accept(None), "an unjudged compile must be accepted");
            assert_eq!(census().3, before + 1, "and counted as unjudged");
        }
    }

    /// Round 9 wave 2, `ir-refusal-memo-outlives-the-inputs-it-judged`: the
    /// gate's refusal is judged against the inputs of its generation, a sticky
    /// refusal is not, and an input-stamped one becomes final after
    /// `MAX_INPUT_REFUSALS` generations.
    ///
    /// Reads the entries and `memo_still_refuses` directly rather than
    /// `method_already_refused`, which also depends on the process-wide
    /// policy latch; other tests may bump the generation concurrently, which
    /// only widens the gaps this test relies on.
    #[test]
    fn an_input_stamped_refusal_expires_when_the_inputs_move_and_a_sticky_one_does_not() {
        let stamped = 0x5EED_0009_0002_0001u64;
        let sticky = 0x5EED_0009_0002_0002u64;
        let entry = |k: u64| {
            refused_methods()
                .read()
                .get(&k)
                .copied()
                .expect("the refusal was recorded")
        };

        note_method_refused_for_inputs(stamped);
        note_method_refused(sticky);
        let g0 = entry(stamped).generation;
        assert!(
            memo_still_refuses(&entry(stamped), g0),
            "refused under its own inputs"
        );
        assert!(
            !memo_still_refuses(&entry(stamped), g0.wrapping_add(1)),
            "offered again once the inputs moved",
        );
        assert!(
            memo_still_refuses(&entry(sticky), g0.wrapping_add(1)),
            "a sticky (site-trap) refusal never expires",
        );

        // A repeat under the SAME generation is not a second refusal...
        let before = entry(stamped).refusals;
        note_method_refused_for_inputs(stamped);
        if inputs_generation() == g0 {
            assert_eq!(entry(stamped).refusals, before);
        }
        // ...and refusals under MAX_INPUT_REFUSALS distinct generations are
        // final.
        while entry(stamped).refusals < MAX_INPUT_REFUSALS {
            bump_inputs_generation();
            note_method_refused_for_inputs(stamped);
        }
        let last = entry(stamped).generation;
        assert!(
            memo_still_refuses(&entry(stamped), last.wrapping_add(7)),
            "refused under {MAX_INPUT_REFUSALS} generations: final",
        );

        // An input-stamped note never downgrades a sticky entry.
        bump_inputs_generation();
        note_method_refused_for_inputs(sticky);
        assert!(entry(sticky).sticky);
    }
}

#[cfg(test)]
thread_local! {
    /// Test-only override of [`accept_policy`] on this thread.
    ///
    /// The policy is read through a process-wide `OnceLock`, so a
    /// `with_thread_overrides` on the env var races with whichever test
    /// initialises it first. A thread-local force is the same shape
    /// `ir_lower`'s `LS_FORCE` and `OSR_ENTRY_FORCE` use, and for the same
    /// reason.
    ///
    /// It exists because the ROUTING tests in `lib.rs` -- "does an int
    /// `invokestatic` go through the IR pipeline" -- are about routing, and
    /// the acceptance gate is a policy layered on top of it. A routing test
    /// that also measures the policy is a test of two things.
    pub(crate) static ACCEPT_FORCE: std::cell::Cell<Option<AcceptPolicy>> =
        const { std::cell::Cell::new(None) };
}

/// Test-only RAII override: publish every body that lowers, so a routing test
/// measures routing.
#[cfg(test)]
pub(crate) struct AcceptAlways;

#[cfg(test)]
impl AcceptAlways {
    pub(crate) fn on() -> Self {
        ACCEPT_FORCE.with(|c| c.set(Some(AcceptPolicy::Always)));
        AcceptAlways
    }
}

#[cfg(test)]
impl Drop for AcceptAlways {
    fn drop(&mut self) {
        ACCEPT_FORCE.with(|c| c.set(None));
    }
}

// ── Refusal memo ────────────────────────────────────────────────────
//
// A refusal is decided AFTER the IR body is built, because the evidence is an
// output of building it. That is unavoidable the first time and pure waste
// every time after: on the H2 JDBC workload the gate refused 577 bodies and
// drove `fell_through_to_single_pass` from 12 compiles (44 ms) to 150
// (586 ms), because each refusal discards the IR artifact and the single-pass
// backend recompiles the method from scratch.
//
// The verdict is a property of the METHOD, not of the attempt: the same
// bytecode, put through the same passes, produces the same evidence. So record
// it once and skip the IR attempt on every later compile of that method.
//
// A memo is only ever a REFUSAL. An accepted method is never recorded.
//
// ## The premise above is false for the gate's own refusals (r9 wave 2)
//
// "The same bytecode, put through the same passes, produces the same
// evidence" holds only while everything ELSE the builder reads holds too: the
// inline plans (a callee is spliced only once its class is loaded), CHA's
// unique implementor, `new`/`checkcast` resolution, the published String
// layout, the branch profile. The first optimizing compile of a hot method
// typically runs during start-up, before most of those exist, and a refusal
// memoised then excluded the method from every optimizing transform for the
// life of the process (`ir-refusal-memo-outlives-the-inputs-it-judged`).
//
// So there are two kinds of entry:
//
// * a STICKY refusal ([`note_method_refused`]) — a decision that is not about
//   the inputs, such as the site-trap policy evicting a trapping body. It
//   never expires; re-offering such a method would re-plant the trap that
//   `ir::claim_site_trap_decision` will not act on a second time.
// * an INPUT-STAMPED refusal ([`note_method_refused_for_inputs`]) — the gate's
//   "no evidence" / cost verdict. It is stamped with [`inputs_generation`] and
//   answers "refused" only while that generation has not moved. Once it has,
//   the method is offered again, and a fresh refusal re-stamps it. A method
//   refused under [`MAX_INPUT_REFUSALS`] different generations stays refused,
//   so a class-loading storm costs each method a bounded number of discarded
//   builds rather than defeating the memo.
//
// The generation is bumped by whatever changes those inputs process-wide:
// `ir::publish_string_layout` here, and class initialization in the VM (see
// [`bump_inputs_generation`]).

/// One memo entry. See the section comment for the two kinds.
#[derive(Clone, Copy, Debug)]
struct RefusalMemo {
    /// [`inputs_generation`] at the most recent refusal.
    generation: u64,
    /// How many input-stamped refusals this method has collected, each under a
    /// different generation (a repeat under the same one is not re-counted,
    /// because the method is not offered again until the generation moves).
    refusals: u8,
    /// A sticky refusal never expires.
    sticky: bool,
}

fn refused_methods() -> &'static parking_lot::RwLock<rustc_hash::FxHashMap<u64, RefusalMemo>> {
    static SET: std::sync::OnceLock<parking_lot::RwLock<rustc_hash::FxHashMap<u64, RefusalMemo>>> =
        std::sync::OnceLock::new();
    SET.get_or_init(|| parking_lot::RwLock::new(rustc_hash::FxHashMap::default()))
}

/// How many methods may be remembered as refused. Bounded for the same reason
/// every other memo here is: an unbounded set keyed by method is a leak on a
/// program that generates classes. An entry already present may always be
/// updated; only NEW methods are turned away at the bound.
const MAX_REFUSED_MEMOS: usize = 8192;

/// An input-stamped refusal collected under this many different
/// [`inputs_generation`]s is final: the method is not offered again.
///
/// Three: the start-up attempt, one after the classes it needed arrived, and
/// one more for a profile or a CHA answer that arrived later still. Past that
/// the refusal is a property of the method, which is what the memo assumed of
/// every refusal.
pub const MAX_INPUT_REFUSALS: u8 = 3;

/// Process-wide generation of the builder's non-bytecode inputs.
static INPUTS_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The current generation of the optimizing tier's non-bytecode inputs (class
/// loading/initialization, the published String layout). Only its CHANGES
/// mean anything.
pub fn inputs_generation() -> u64 {
    INPUTS_GENERATION.load(std::sync::atomic::Ordering::Acquire)
}

/// Record that something a build reads besides the bytecode may now answer
/// differently — a class was initialized (an inline plan, a `new`/`checkcast`
/// site or a CHA query can now resolve), or the String layout was published.
///
/// Every input-stamped refusal recorded before this call stops being
/// consulted; see the "Refusal memo" section. Cheap (one atomic add), so a
/// caller on the class-initialization path need not batch it.
pub fn bump_inputs_generation() {
    INPUTS_GENERATION.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
}

/// Record a STICKY refusal: this method is not offered to the optimizing tier
/// again, whatever loads later. For a decision that is not about the build's
/// inputs (the site-trap policy). The gate's own verdicts should use
/// [`note_method_refused_for_inputs`].
pub fn note_method_refused(hash: u64) {
    let mut map = refused_methods().write();
    let generation = inputs_generation();
    if let Some(e) = map.get_mut(&hash) {
        e.sticky = true;
        e.generation = generation;
        return;
    }
    if map.len() < MAX_REFUSED_MEMOS {
        map.insert(
            hash,
            RefusalMemo {
                generation,
                refusals: 1,
                sticky: true,
            },
        );
    }
}

/// Record the acceptance gate's refusal of this method's optimizing body,
/// stamped with the current [`inputs_generation`]: it is consulted only until
/// the generation moves, at most [`MAX_INPUT_REFUSALS`] times over.
pub fn note_method_refused_for_inputs(hash: u64) {
    let mut map = refused_methods().write();
    let generation = inputs_generation();
    if let Some(e) = map.get_mut(&hash) {
        if e.generation != generation {
            e.refusals = e.refusals.saturating_add(1);
            e.generation = generation;
        }
        return;
    }
    if map.len() < MAX_REFUSED_MEMOS {
        map.insert(
            hash,
            RefusalMemo {
                generation,
                refusals: 1,
                sticky: false,
            },
        );
    }
}

/// Is a memo entry recorded at some generation still a refusal at `now`?
fn memo_still_refuses(e: &RefusalMemo, now: u64) -> bool {
    e.sticky || e.generation == now || e.refusals >= MAX_INPUT_REFUSALS
}

/// Has this method already been refused for want of evidence?
///
/// Consulted BEFORE the IR pipeline runs, which is the whole point: the first
/// refusal pays for a discarded build, and no later one does — until the
/// inputs it was judged on change (see the "Refusal memo" section).
pub fn method_already_refused(hash: u64) -> bool {
    if accept_policy() != AcceptPolicy::Evidence {
        return false;
    }
    if !refused_methods_memo_enabled() {
        return false;
    }
    let now = inputs_generation();
    refused_methods()
        .read()
        .get(&hash)
        .is_some_and(|e| memo_still_refuses(e, now))
}

/// `CRATONVM_C2_ACCEPT_MEMO=0` restores the un-memoized gate, which rebuilds
/// and re-discards on every compile of a refused method. Kept because it is
/// the arm that separates "the gate refuses the right methods" from "the memo
/// remembers the right verdict".
fn refused_methods_memo_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        !matches!(
            cratonvm_types::flags::runtime_var("CRATONVM_C2_ACCEPT_MEMO").as_deref(),
            Ok("0") | Ok("false") | Ok("off") | Ok("no")
        )
    })
}

/// Methods skipped because a previous compile of them was refused.
static MEMO_SKIPS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn note_memo_skip() {
    MEMO_SKIPS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// How many IR builds the memo avoided. A zero with a large
/// `refused_no_evidence` means the memo is not being consulted.
pub fn memo_skips() -> u64 {
    MEMO_SKIPS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Process-wide override forcing [`AcceptPolicy::Always`].
///
/// # Why a public function and not just the env var
///
/// The policy is latched in a `OnceLock`, so a test binary that sets
/// `CRATONVM_C2_ACCEPT` after the first compile has already lost. And an
/// INTEGRATION test lives in its own crate, where the `#[cfg(test)]`
/// thread-local force in this module is not visible.
///
/// The caller that needs this is `jit/tests/ir_vs_singlepass.rs`, whose whole
/// subject is ROUTING -- "does this bytecode reach the IR pipeline, and does it
/// answer what the single-pass backend answers". The acceptance gate is a
/// POLICY layered on top of routing, and its probes are three-bytecode methods
/// that by construction apply no transform the baseline tier lacks. Running
/// that harness under the gate measures the gate, not the backends: 17 of its
/// 20 failures on 2026-09-06 were exactly that.
///
/// Deliberately one-way. There is no `force_off`, because the only legitimate
/// use is a harness declaring "I am testing something the policy is not about",
/// and a switch that can turn the gate back ON mid-process would let one test
/// change another's meaning.
pub fn force_accept_always_for_this_process() {
    PROCESS_FORCE_ALWAYS.store(true, std::sync::atomic::Ordering::Relaxed);
}

static PROCESS_FORCE_ALWAYS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

// ── The verdict, for a caller that can act on it ────────────────────
//
// The gate discards the IR body, `try_compile_inner` falls through to the
// single-pass backend, and the caller publishes THAT. For a SUPERSEDE that is
// the worst of both: the method already had an equivalent C1 body, so the
// publish replaces it with an equal one and still bumps the process-wide
// supersede epoch, which stales every cached invoke target in every thread
// (1,558 evictions on one H2 run).
//
// Measured: with all seven items on and the gate refusing 571 bodies,
// `CRATONVM_C2_SUPERSEDE=0` was still ~5% faster in mean CPU on H2 against a
// control pair agreeing to 0.04%. A gate that refuses a body and then
// republishes an equivalent one has not refused anything the caller can feel.
//
// So the verdict is published for the caller to read. The VM's compile-task
// path is the only consumer, on the same thread, immediately after
// `try_compile` returns: a REFUSED verdict on a method that already has a
// published body means "leave the C1 body alone" -- no publish, no epoch bump,
// no invalidation.
//
// A thread-local rather than a return value, because threading a 23rd
// parameter through `try_compile` to carry one bit is worse than a slot the
// caller reads at a known point. It is take-and-clear so a stale verdict from
// an earlier compile can never be read as this one's.

thread_local! {
    static LAST_VERDICT: std::cell::Cell<Option<bool>> = const {
        std::cell::Cell::new(None)
    };
}

/// Record this compile's acceptance verdict for the caller. `true` = accepted.
pub fn publish_verdict(accepted: bool) {
    LAST_VERDICT.with(|c| c.set(Some(accepted)));
}

/// Read and clear the verdict of the last optimizing compile on this thread.
///
/// `None` means no optimizing compile recorded one — the method never reached
/// the IR pipeline at all — which a caller must treat as "no opinion", not as
/// a refusal.
pub fn take_last_verdict() -> Option<bool> {
    LAST_VERDICT.with(|c| c.replace(None))
}

/// Supersedes abandoned because the optimizing body carried no evidence and a
/// baseline body already existed.
static SUPERSEDES_ABANDONED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn note_supersede_abandoned() {
    SUPERSEDES_ABANDONED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// How many epoch bumps and republishes the verdict avoided.
pub fn supersedes_abandoned() -> u64 {
    SUPERSEDES_ABANDONED.load(std::sync::atomic::Ordering::Relaxed)
}
