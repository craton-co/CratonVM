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
//! A bitset of transforms the optimizing tier applied to THIS compile. It is
//! deliberately not a quality score and cannot be one: a compile-time predicate
//! cannot know whether the emitted body is faster. What it can know is whether
//! the tier did anything at all that the baseline tier has no equivalent for --
//! and when the answer is no, the C2 body is a differently-emitted version of
//! the same computation carrying this tier's weaker register model, which is
//! the case there is no argument for publishing.
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
    static STACK: std::cell::RefCell<Vec<u32>> = const {
        std::cell::RefCell::new(Vec::new())
    };
}

/// Arm a new optimizing compile on this thread. Nests.
pub fn begin_compile() {
    STACK.with(|s| s.borrow_mut().push(0));
}

/// Record that `t` was applied to the compile running on this thread.
///
/// A call with the slot un-armed is IGNORED rather than armed implicitly: the
/// transforms below also run from unit tests and from the single-pass tier's
/// own passes, and letting either arm the slot would attribute their work to
/// whatever compile came next on the same thread.
#[inline]
pub fn note(t: Transform) {
    STACK.with(|s| {
        if let Ok(mut v) = s.try_borrow_mut() {
            if let Some(bits) = v.last_mut() {
                *bits |= t.bit();
            }
        }
    });
}

/// Pop the innermost armed compile. `None` means none was armed on this
/// thread, which a caller must treat as "cannot judge" rather than as "did
/// nothing".
pub fn take() -> Option<u32> {
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
pub fn is_worth_publishing(bits: u32) -> bool {
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
pub fn accept(evidence: Option<u32>) -> bool {
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
            Some(bits) if is_worth_publishing(bits) => {
                ACCEPTED.fetch_add(1, Relaxed);
                true
            }
            Some(bits) => {
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

static REFUSED_BUT_SIMPLIFIED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
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

#[cfg(test)]
mod tests {
    use super::*;

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
            Some(0),
            "an armed compile that did nothing reads as Some(0), not None",
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
            describe(inner),
            "guard-elided",
            "the inner compile must see ONLY its own transforms",
        );
        let outer = take().expect("the outer compile must still be armed");
        assert_eq!(
            describe(outer),
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
        let bits = take().expect("armed");
        assert!(is_worth_publishing(bits));
        let d = describe(bits);
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
        let bits = take().expect("armed");
        assert!(
            !is_worth_publishing(bits),
            "the baseline tier unrolls 4x and hoists; doing the same is not a \
             reason to replace its body",
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
// A memo is only ever a REFUSAL. An accepted method is never recorded, so the
// memo can cost an optimization only by being consulted for a method whose
// evidence would now differ -- which needs the passes themselves to change,
// i.e. a new build.

fn refused_methods() -> &'static parking_lot::RwLock<rustc_hash::FxHashSet<u64>> {
    static SET: std::sync::OnceLock<parking_lot::RwLock<rustc_hash::FxHashSet<u64>>> =
        std::sync::OnceLock::new();
    SET.get_or_init(|| parking_lot::RwLock::new(rustc_hash::FxHashSet::default()))
}

/// How many methods may be remembered as refused. Bounded for the same reason
/// every other memo here is: an unbounded set keyed by method is a leak on a
/// program that generates classes.
const MAX_REFUSED_MEMOS: usize = 8192;

/// Record that this method's optimizing body carried no evidence.
pub fn note_method_refused(hash: u64) {
    let mut set = refused_methods().write();
    if set.len() < MAX_REFUSED_MEMOS {
        set.insert(hash);
    }
}

/// Has this method already been refused for want of evidence?
///
/// Consulted BEFORE the IR pipeline runs, which is the whole point: the first
/// refusal pays for a discarded build, and no later one does.
pub fn method_already_refused(hash: u64) -> bool {
    if accept_policy() != AcceptPolicy::Evidence {
        return false;
    }
    if !refused_methods_memo_enabled() {
        return false;
    }
    refused_methods().read().contains(&hash)
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
static SUPERSEDES_ABANDONED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

pub fn note_supersede_abandoned() {
    SUPERSEDES_ABANDONED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// How many epoch bumps and republishes the verdict avoided.
pub fn supersedes_abandoned() -> u64 {
    SUPERSEDES_ABANDONED.load(std::sync::atomic::Ordering::Relaxed)
}
