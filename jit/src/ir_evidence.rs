// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
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
    /// A loop's accumulator cells/statics/elements were promoted into registers
    /// or retired to closed form (`loop_field_forward`, `loop_retire`).
    LoopPromoted,
    /// A runtime-bound loop became a main loop that runs `factor` copies of
    /// its body per test, plus the original as the post loop
    /// (`ir_optimize::main_post_unroll_loop`, round 17). EVIDENCE, unlike
    /// [`Self::Unrolled`]: the single-pass backend's unroller keeps the exit
    /// test in every copy, so this shape -- one test per `factor` iterations,
    /// no early exits -- is one the baseline tier does not have.
    MainPostUnrolled,
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
            Transform::LoopPromoted => "loop-promoted",
            Transform::MainPostUnrolled => "main-post-unrolled",
        }
    }

    pub const ALL: [Transform; 10] = [
        Transform::ScalarReplacement,
        Transform::Inlined,
        Transform::GuardElided,
        Transform::SunkLate,
        Transform::Unrolled,
        Transform::Licm,
        Transform::ScalarIntrinsic,
        Transform::Simplified,
        Transform::LoopPromoted,
        Transform::MainPostUnrolled,
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
    /// JIT round 17 wave 2 (lane monitor3): loops whose bytecode
    /// `synchronized` block `ir_optimize::coarsen_loop_block` rewrote in this
    /// compile (either shape, any monitor kind). Non-zero only under
    /// `CRATONVM_JIT_IR_LOOP_LOCK_COARSEN_BLOCKS` /
    /// `CRATONVM_JIT_IR_LOOP_LOCK_COARSEN_INSTANCE_BLOCKS`, so the default
    /// gate is unchanged. Kept apart from [`Self::bits`] so the transform
    /// census and [`describe`] stay byte-identical with those switches off.
    /// See [`note_loop_block_coarsened`].
    pub loop_blocks_coarsened: u32,
    /// JIT round 17 wave 2 (lane monitor3): loops whose synchronized SPLICE
    /// window the window pass (`ir_optimize::coarsen_loop_sync_windows`,
    /// default ON) coarsened in this compile. Counted always, read only under
    /// `CRATONVM_JIT_IR_LOOP_LOCK_COARSEN_WINDOW_EVIDENCE`
    /// ([`loop_window_coarsening_is_evidence`]), so the default gate is
    /// unchanged. See [`note_loop_window_coarsened`].
    pub loop_windows_coarsened: u32,
    /// JIT round 17 wave 2 (lane irfeed, MON3R17-1): `ir_optimize::
    /// optimize_passes` found the graph smaller than it started BEFORE the
    /// loop rounds, i.e. before any pass that may grow it (partial unroll,
    /// window and block coarsening) ran. [`Transform::Simplified`] is the net
    /// count over the whole pipeline, which those passes' copies mask.
    /// Recorded always, read as evidence only under
    /// `CRATONVM_JIT_IR_EVIDENCE_SIMPLIFIED_BEFORE_GROWTH`
    /// ([`simplified_before_growth_is_evidence`]), so the default gate is
    /// unchanged. See [`note_simplified_before_growth`].
    pub simplified_before_growth: bool,
    /// JIT round 17 wave 3 (lane irlower, IRFEED17-2): counted loops the
    /// unroller FULLY unrolled in this compile (`ir_optimize::unroll`'s
    /// full-unroll emitter, not the partial one). [`Transform::Unrolled`] is
    /// noted for both and is not evidence, because the single-pass backend
    /// unrolls 4x too; a full unroll instead retires the loop (no back edge, no
    /// poll, constant induction values folded through the body), which that
    /// backend has no equivalent for. Recorded always, read as evidence unless
    /// `CRATONVM_JIT_IR_EVIDENCE_FULL_UNROLL=0`
    /// ([`full_unroll_is_evidence`]). Kept
    /// outside the bitset so `describe` and the transform census are
    /// byte-identical.
    pub fully_unrolled: u32,
    /// nio20 wave 6, lane L3: `Op::Call` sites whose lowering emitted an
    /// inline call PREFIX (`ir_lower`'s String / StringBuilder / getClass /
    /// ArrayList / box / `Unsafe` / identity-hash prefixes: the common case
    /// answered inline, the call kept as the decline edge). Recorded always,
    /// read as evidence unless `CRATONVM_JIT_IR_EVIDENCE_CALL_PREFIX=0`
    /// ([`call_prefix_is_evidence`], default ON since wave 7b; it was opt-in
    /// because most
    /// of these prefixes have a single-pass twin (the intrinsic the baseline
    /// body already inlines), so a fired prefix alone is not proof the body
    /// beats the baseline). Kept outside the bitset so `describe` and the
    /// transform census are byte-identical. See [`note_call_prefix`].
    pub call_prefixes: u32,
    /// JIT round 21 (lane accept, `79fbc8a88c`; re-landed by t1 wave 8 lane
    /// ACCEPT after merge `624fae0b49` dropped it): call sites the IR BUILDER
    /// replaced with inline IR that the single-pass body has no counterpart
    /// for -- the `ThreadLocal.get()` hit path
    /// (`try_thread_local_get_intrinsic`; the single-pass body calls the thin
    /// helper), a mutable call site's guarded direct call
    /// (`try_indy_mutable_guarded_call`), a published lambda singleton read
    /// (`try_indy_lambda_word`) and a linked capturing lambda's inline `new`
    /// (`try_indy_capture_new`; the single-pass body calls the bridge for all
    /// three), plus (ACCEPT21-2, opt-in [`scoped_aligned_fold_is_evidence`])
    /// an ALIGNED `ScopedMemoryAccess` fold. Recorded always, read as evidence
    /// under `CRATONVM_JIT_IR_EVIDENCE_BUILDER_EXPANSIONS` (default ON, `=0`
    /// kill; [`builder_expansion_is_evidence`]). Without it a kernel whose
    /// only gain is one of these reads "evidence: none" and runs the
    /// single-pass body. Kept outside the bitset so `describe` and the
    /// transform census are byte-identical. See [`note_builder_expansion`] and
    /// [`BUILDER_EXPANSION_PARITY`].
    pub builder_expansions: u32,
    /// t1 wave 8 lane ACCEPT (review of the refusal memo): the
    /// [`inputs_generation`] when the compile was ARMED
    /// ([`begin_compile_scope`]; `None` for a bare [`begin_compile`]). The
    /// gate's refusal judged the inputs the builder read at that point, so the
    /// memo stamps it with this generation, not the one current when the
    /// refusal is written ([`refusal_stamp_at_arm_enabled`]).
    pub inputs_generation_at_arm: Option<u64>,
    /// JIT round 23 wave 8 (lane W8-L2, CF-W8-3;
    /// [`cold_blind_dispatch_weight_enabled`]): combined-pc ranges `(start,
    /// end, permille)` of spliced code that only a RARELY taken branch arm
    /// reaches, by the callee's own profile, installed per compile by
    /// [`set_splice_cold_ranges`]. At most [`COLD_RANGE_ROWS`]; `cold_range_len`
    /// are in use. A fixed array so the record stays `Copy`.
    pub cold_ranges: [(u32, u32, u16); COLD_RANGE_ROWS],
    /// How many rows of [`Self::cold_ranges`] are in use.
    pub cold_range_len: u8,
    /// The per-execution charge (permille of one blind dispatch) taken OFF
    /// [`Self::blind_dispatches_in_splice`] for sites inside a cold range
    /// ([`note_blind_dispatch_weight_at`]): `1000 - weight` per such site.
    pub blind_cold_discount_permille: u32,
    /// [`Self::blind_cold_discount_permille`] when [`blind_dispatch_charge`]
    /// was read, so a retried lowering rolls the discount back with the count.
    pub blind_cold_discount_mark: u32,
}

/// CF-W8-3: the most cold ranges one compile records.
pub const COLD_RANGE_ROWS: usize = 16;

/// CF-W8-3: the least weight (permille of one execution per method entry) a
/// cold arm's blind dispatch is charged, however rare the profile says the arm
/// is: a profile is a sample, and an arm that ran once may run again.
pub const COLD_ARM_WEIGHT_FLOOR_PERMILLE: u16 = 31;

/// `CRATONVM_JIT_IR_EVIDENCE_COLD_ARM_WEIGHT` (JIT round 23 wave 8, lane
/// W8-L2, CF-W8-3; perf, default ON, `=0` charges every in-splice blind
/// dispatch at full cost as before): the acceptance gate's splice trade
/// ([`CompileRecord::added_ns_per_execution`]) weights a blind dispatch that
/// sits in spliced code only a rarely taken branch arm reaches by that arm's
/// profiled frequency, floored at [`COLD_ARM_WEIGHT_FLOOR_PERMILLE`]. Read
/// live (no `static`: `process_global_statics_ratchet`), at most a few times
/// per gate decision and once per resolved splice body.
pub fn cold_blind_dispatch_weight_enabled() -> bool {
    true
}

/// CF-W8-3: install this compile's cold ranges (combined pcs) in the innermost
/// armed record, replacing any earlier set (a re-planned body re-installs).
/// Rows past [`COLD_RANGE_ROWS`] are dropped: their sites are then charged in
/// full, the old rule. A no-op when nothing is armed or the switch is off.
pub fn set_splice_cold_ranges(rows: &[(usize, usize, u16)]) {
    if !cold_blind_dispatch_weight_enabled() {
        return;
    }
    STACK.with(|s| {
        if let Ok(mut v) = s.try_borrow_mut() {
            if let Some(rec) = v.last_mut() {
                rec.cold_ranges = [(0, 0, 0); COLD_RANGE_ROWS];
                let mut n = 0usize;
                for &(start, end, w) in rows {
                    if n == COLD_RANGE_ROWS {
                        break;
                    }
                    let (Ok(start), Ok(end)) = (u32::try_from(start), u32::try_from(end)) else {
                        continue;
                    };
                    rec.cold_ranges[n] =
                        (start, end, w.clamp(COLD_ARM_WEIGHT_FLOOR_PERMILLE, 1000));
                    n += 1;
                }
                rec.cold_range_len = u8::try_from(n).unwrap_or(0);
            }
        }
    });
}

/// CF-W8-3: the weight (permille) of a blind dispatch at combined pc `pc` in
/// `rec`: the least weight of the cold ranges holding it, or 1000.
fn cold_weight_at(rec: &CompileRecord, pc: usize) -> u16 {
    let Ok(pc) = u32::try_from(pc) else {
        return 1000;
    };
    rec.cold_ranges[..usize::from(rec.cold_range_len).min(COLD_RANGE_ROWS)]
        .iter()
        .filter(|&&(s, e, _)| s <= pc && pc < e)
        .map(|&(_, _, w)| w)
        .min()
        .unwrap_or(1000)
}

/// CF-W8-3: the in-splice blind dispatch just charged by
/// [`note_blind_dispatch_in_splice`] sits at combined pc `pc`; discount it to
/// its cold range's weight. `None` (no pc) and a pc in no cold range leave the
/// full charge. Ignored when nothing is armed or the switch is off.
pub fn note_blind_dispatch_weight_at(pc: Option<usize>) {
    let Some(pc) = pc else {
        return;
    };
    if !cold_blind_dispatch_weight_enabled() {
        return;
    }
    STACK.with(|s| {
        if let Ok(mut v) = s.try_borrow_mut() {
            if let Some(rec) = v.last_mut() {
                let w = cold_weight_at(rec, pc);
                if w < 1000 {
                    rec.blind_cold_discount_permille = rec
                        .blind_cold_discount_permille
                        .saturating_add(u32::from(1000 - w));
                }
            }
        }
    });
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
        // CF-W8-3 (`CRATONVM_JIT_IR_EVIDENCE_COLD_ARM_WEIGHT`): a site in a
        // rarely taken arm is charged at that arm's weight. The discount is
        // never more than the full charge it was taken from.
        let full_permille = i64::from(self.blind_dispatches_in_splice) * 1000;
        let charged_permille = if cold_blind_dispatch_weight_enabled() {
            (full_permille - i64::from(self.blind_cold_discount_permille)).max(0)
        } else {
            full_permille
        };
        charged_permille * (BLIND_DISPATCH_NS - DIRECT_CALL_NS) / 1000
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
        v.push(CompileRecord {
            inputs_generation_at_arm: Some(inputs_generation()),
            ..CompileRecord::default()
        });
        depth
    });
    CompileScope { depth }
}

/// `CRATONVM_JIT_IR_REFUSAL_STAMP_AT_ARM` (t1 wave 8 lane ACCEPT; memo
/// correctness, **default ON**, `=0` restores the stamp taken at write time):
/// the acceptance gate's input-stamped refusal
/// ([`IrRefusalMemo::note_refused_for_inputs_at`]) carries the
/// [`inputs_generation`] read when the compile was armed
/// ([`CompileRecord::inputs_generation_at_arm`]). A class initialization
/// during the compile (frequent at start-up, when the first optimizing
/// compiles run) used to stamp the refusal with the NEWER generation, so the
/// method stayed refused under inputs no build had judged until the next
/// bump. Read once per refusal.
pub fn refusal_stamp_at_arm_enabled() -> bool {
    true
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
        if !false {
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

/// Record that this compile coarsened a loop's bytecode `synchronized` block
/// (`ir_optimize::coarsen_loop_block`). Ignored when nothing is armed, for
/// `note`'s reason.
///
/// JIT round 17 wave 2 (lane monitor3;
/// `r17-monitor-forked-block-coarsening-is-not-the-body-that-runs-FIXED-20261005.md`):
/// the coarsened `R16MonitorCrowd.shortCs` body was lowered and then DISCARDED
/// here with "evidence: none". The one bit that body used to earn,
/// `Simplified`, is a NET node-count test (`ir_optimize::optimize_passes`:
/// `live_count() < nodes_before`), and the coarsening's K copies of the block
/// add more nodes than the cleanup removed, so a body accepted uncoarsened was
/// refused coarsened. A coarsened block removes K-1 of every K monitor pairs
/// -- two locked RMWs each -- which the single-pass backend has no equivalent
/// for, so it is evidence in its own right ([`is_worth_publishing`]).
pub fn note_loop_block_coarsened() {
    STACK.with(|s| {
        if let Ok(mut v) = s.try_borrow_mut() {
            if let Some(rec) = v.last_mut() {
                rec.loop_blocks_coarsened = rec.loop_blocks_coarsened.saturating_add(1);
            }
        }
    });
}

/// Record that this compile coarsened a loop's synchronized SPLICE window
/// (`ir_optimize::coarsen_loop_sync_windows`'s window pass, default ON).
/// Ignored when nothing is armed. The count changes no verdict unless
/// [`loop_window_coarsening_is_evidence`] is on: the window pass runs by
/// default, and counting it unconditionally would change which bodies the
/// default gate publishes.
pub fn note_loop_window_coarsened() {
    STACK.with(|s| {
        if let Ok(mut v) = s.try_borrow_mut() {
            if let Some(rec) = v.last_mut() {
                rec.loop_windows_coarsened = rec.loop_windows_coarsened.saturating_add(1);
            }
        }
    });
}

/// Has the innermost armed compile on this thread coarsened a loop block
/// ([`note_loop_block_coarsened`])? `false` when nothing is armed.
///
/// JIT round 17 wave 2 (lane irfeed, MON3R17-2): the per-artifact half of
/// `osr_exit`'s lock-holding guard rule. The lowering asks it while it builds
/// the OSR guard-exit fact (`osr_exit::guard_exit_fact`), inside the compile
/// whose optimizer made (or did not make) the coarsened block.
pub(crate) fn armed_compile_coarsened_a_loop_block() -> bool {
    STACK.with(|s| {
        s.try_borrow()
            .ok()
            .and_then(|v| v.last().map(|rec| rec.loop_blocks_coarsened > 0))
            .unwrap_or(false)
    })
}

/// Record that this compile's removal-only passes shrank the graph before any
/// growing pass ran ([`CompileRecord::simplified_before_growth`]). Ignored
/// when nothing is armed, for `note`'s reason.
///
/// JIT round 17 wave 2 (lane irfeed, MON3R17-1;
/// `r17-acceptance-gate-simplified-is-a-net-node-count-CLOSED-20261006.md`): a body
/// that folded nodes and was then partially unrolled (or loop-coarsened)
/// reached the gate with no `Simplified` bit, because the copies outnumber the
/// folds, and was refused as "evidence: none".
pub fn note_simplified_before_growth() {
    STACK.with(|s| {
        if let Ok(mut v) = s.try_borrow_mut() {
            if let Some(rec) = v.last_mut() {
                rec.simplified_before_growth = true;
            }
        }
    });
}

/// Record that this compile fully unrolled a counted loop
/// ([`CompileRecord::fully_unrolled`]). Ignored when nothing is armed, for
/// `note`'s reason. JIT round 17 wave 3 (lane irlower, IRFEED17-2).
pub fn note_full_unroll() {
    STACK.with(|s| {
        if let Ok(mut v) = s.try_borrow_mut() {
            if let Some(rec) = v.last_mut() {
                rec.fully_unrolled = rec.fully_unrolled.saturating_add(1);
            }
        }
    });
}

/// Record that this compile's lowering emitted one inline call prefix
/// ([`CompileRecord::call_prefixes`]). Ignored when nothing is armed, for
/// `note`'s reason. nio20 wave 6, lane L3.
pub fn note_call_prefix() {
    STACK.with(|s| {
        if let Ok(mut v) = s.try_borrow_mut() {
            if let Some(rec) = v.last_mut() {
                rec.call_prefixes = rec.call_prefixes.saturating_add(1);
            }
        }
    });
}

/// Record that the IR builder expanded one call site inline
/// ([`CompileRecord::builder_expansions`]). Ignored when nothing is armed, for
/// `note`'s reason. JIT round 21, lane accept.
pub fn note_builder_expansion() {
    STACK.with(|s| {
        if let Ok(mut v) = s.try_borrow_mut() {
            if let Some(rec) = v.last_mut() {
                rec.builder_expansions = rec.builder_expansions.saturating_add(1);
            }
        }
    });
}

/// `CRATONVM_JIT_IR_EVIDENCE_BUILDER_EXPANSIONS` (perf, **default ON**, `=0`
/// restores the gate's old answer; JIT round 21, lane accept): does a builder
/// expansion ([`note_builder_expansion`]) count as acceptance evidence? Each
/// one replaces a call the single-pass body keeps (a helper or a bridge) with
/// straight-line IR, the rule [`Transform::ScalarIntrinsic`] already states
/// for call-site intrinsics. Read live, at most once per gate decision (no
/// `static`: `process_global_statics_ratchet`).
pub fn builder_expansion_is_evidence() -> bool {
    true
}

/// `CRATONVM_JIT_IR_EVIDENCE_SCOPED_ALIGNED` (perf,
/// **default ON since j27 2026-10-28**, `=0` kills; t1 wave 8
/// lane ACCEPT, ACCEPT21-2): an ALIGNED native-order `ScopedMemoryAccess` fold
/// (`UnboxOp::ScopedGet/ScopedPut { ordered: false }`, zt t31
/// `CRATONVM_JIT_IR_SCOPED_FOLD_ALIGNED`) notes a builder expansion. It is the
/// one builder expansion of [`BUILDER_EXPANSION_PARITY`] with no single-pass
/// twin: the single-pass scoped prefix serves only the sixteen `*Unaligned`
/// cells (`x64::op_invoke::sp_scoped_access_of`), so a single-pass body calls
/// `DirectCharBufferU.get`'s aligned accessor. It was opt-in until j27 because
/// it changes which bodies the default gate publishes; the A/B is `zt31` direct `CharBuffer`
/// rows against `CRATONVM_C2_ACCEPT=always`. Read once per folded site.
pub fn scoped_aligned_fold_is_evidence() -> bool {
    // WARNING j27-user-flip-20261028 (default ON; =0 restores the old default)
    true
}

/// ACCEPT21-2 (t1 wave 8, lane ACCEPT): the IR builder's call-site expansions
/// and the single-pass arm that answers the same call, `(builder site, single-pass
/// twin or "none", evidence)`. An expansion with a twin is NOT evidence (the
/// baseline body does the same work inline); one without notes
/// [`note_builder_expansion`]. A row changes only with the code it names; the
/// unit test pins the "none" rows to the ones that note.
pub const BUILDER_EXPANSION_PARITY: &[(&str, &str, &str)] = &[
    (
        "try_thread_local_get_intrinsic",
        "none (thin-helper call)",
        "builder expansion (default ON)",
    ),
    (
        "try_indy_mutable_guarded_call",
        "none (bridge call)",
        "builder expansion (default ON)",
    ),
    (
        "try_indy_lambda_word",
        "none (bridge call)",
        "builder expansion (default ON)",
    ),
    (
        "try_indy_capture_new",
        "none (bridge call)",
        "builder expansion (default ON)",
    ),
    (
        "try_emit_vh_field_read",
        "x64 try_emit_sp_vh_const_{thin,dispatch}_prefix (nio21 w11 L5)",
        "no",
    ),
    (
        "try_emit_scoped_raw (ordered: *Unaligned)",
        "x64 try_emit_sp_scoped_access_prefix (CRATONVM_JIT_SP_SCOPED_ACCESS_PREFIX)",
        "no",
    ),
    (
        "try_emit_scoped_raw (aligned, zt t31)",
        "none (sp_scoped_access_of serves *Unaligned only)",
        "builder expansion (default ON since j27, =0 kills: CRATONVM_JIT_IR_EVIDENCE_SCOPED_ALIGNED)",
    ),
    (
        "try_string_access_intrinsic",
        "x64 String call-site intrinsics (driver.rs length/charAt/isEmpty)",
        "no",
    ),
    (
        "try_emit_unbox_intrinsic (Long/Integer value, Atomic*)",
        "x64 op_invoke box/unbox + atomic intrinsic arms (same Atomic*FieldLayout)",
        "no",
    ),
];

/// `CRATONVM_JIT_IR_EVIDENCE_CALL_PREFIX` (nio20 wave 6, lane L3; **default
/// ON** since wave 7b, `=0` kills): does a fired inline call prefix
/// ([`note_call_prefix`]) count as evidence?
/// It was opt-in because it changes which bodies the default gate publishes, and
/// because the prefixes mostly mirror an intrinsic the single-pass body has
/// (the A/B is against `CRATONVM_C2_ACCEPT=always`, which measured
/// `Nio20StringIr sbappendstr` WORSE on w5c). Default ON because a switch
/// dispatcher whose only optimizing-tier gain is its inline `String.hashCode`
/// / `String.equals` prefixes (its looping arms now stay calls,
/// `ir_splice_order::switch_tail_loop_callee_refused`) read "evidence: none"
/// and ran the single-pass body. It still counts only for a body whose priced
/// trade did not get worse: [`is_worth_publishing`] refuses any record with
/// `added_ns_per_execution() > 0` before it looks at evidence. Read live, at
/// most once per gate decision (no `static`: `process_global_statics_ratchet`).
pub fn call_prefix_is_evidence() -> bool {
    true
}

/// `CRATONVM_JIT_IR_EVIDENCE_FULL_UNROLL` (**default ON** since JIT r27 LOOP27-4,
/// `=0` kills; JIT round 17 wave 3, lane irlower, IRFEED17-2): does a FULL unroll
/// ([`note_full_unroll`]) count as evidence? A partial unroll still does not.
/// Round 17 kept it off on `R17IrLowerCopies` (within noise); r27 found the row it
/// decides: `Nio21MaskedFence` `own` / `param` fully unroll under the named-frame
/// rule (LOOPOPT20-6), the gate then discarded the unrolled body for the
/// single-pass one (`acceptance ...: REFUSED (evidence: unrolled,licm; ... fully
/// unrolled 1)`), so the rule that unrolled them never ran. The A/B is against
/// `CRATONVM_C2_ACCEPT=always`. Read live, at most once per gate decision (no
/// `static`: `process_global_statics_ratchet`).
pub fn full_unroll_is_evidence() -> bool {
    true
}

/// `CRATONVM_JIT_IR_EVIDENCE_SIMPLIFIED_BEFORE_GROWTH=1` (opt-in; JIT round 17
/// wave 2, lane irfeed, MON3R17-1): does a shrink measured before the growing
/// passes ([`note_simplified_before_growth`]) count as the `Simplified`
/// evidence the net count hides? Opt-in because it changes which bodies the
/// default gate publishes; the A/B is against `CRATONVM_C2_ACCEPT=always` on
/// H2. Read live, at most once per gate decision (no `static`).
pub fn simplified_before_growth_is_evidence() -> bool {
    false
}

/// `CRATONVM_JIT_IR_LOOP_LOCK_COARSEN_WINDOW_EVIDENCE=1` (opt-in; JIT round
/// 17 wave 2, lane monitor3): does a coarsened splice window
/// ([`note_loop_window_coarsened`]) count as evidence? The same blind spot as
/// a coarsened block's (`Simplified` is a NET node count, and the window
/// pass's copies grow the graph), for the default-on pass. Read live, at most
/// once per gate decision (no `static`: `process_global_statics_ratchet`).
pub fn loop_window_coarsening_is_evidence() -> bool {
    false
}

/// The innermost armed compile's blind-dispatch charge so far, or `None` when
/// nothing is armed. Read by `ir_lower::lower_inner_with_array_list` before
/// its first lowering attempt so a RETRIED lowering can be rolled back to it.
///
/// Round 11 (irlower): the lowerer re-lowers the SAME graph once after a
/// code-buffer overflow, and every blind dispatch the discarded first attempt
/// emitted had already been charged here -- so a large spliced body (the kind
/// that overflows the estimate) was priced at twice its real cost and could be
/// refused by [`accept`] as a regression it is not. The process-wide census in
/// `ir_lower` double-counts too, and that skew is documented and accepted;
/// this record is a gate INPUT, not a diagnostic, so it is restored instead.
pub(crate) fn blind_dispatch_charge() -> Option<u32> {
    STACK.with(|s| {
        s.try_borrow_mut().ok().and_then(|mut v| {
            v.last_mut().map(|rec| {
                // CF-W8-3: the cold discount rolls back with the count.
                rec.blind_cold_discount_mark = rec.blind_cold_discount_permille;
                rec.blind_dispatches_in_splice
            })
        })
    })
}

/// Reset the innermost armed compile's blind-dispatch charge to `charge`, a
/// value [`blind_dispatch_charge`] returned for the same compile. A no-op when
/// nothing is armed.
pub(crate) fn restore_blind_dispatch_charge(charge: u32) {
    STACK.with(|s| {
        if let Ok(mut v) = s.try_borrow_mut() {
            if let Some(rec) = v.last_mut() {
                rec.blind_dispatches_in_splice = charge;
                rec.blind_cold_discount_permille = rec.blind_cold_discount_mark;
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

/// Render a whole record for the gate's per-method diagnostic lines: the
/// bitset ([`describe`]) followed by the evidence kept outside it, each only
/// when non-zero, so a record with none of them renders exactly as
/// `describe(rec.bits)` did.
///
/// JIT round 17 wave 3 (lane irlower, IRFEED17-1 / MON3R17-3): the refused
/// line named the bitset alone, so a body whose `Simplified` a growing pass
/// masked read `evidence: unrolled` with nothing beside it, and an accepted
/// body printed nothing at all. `None` (nothing armed) renders as `unjudged`.
pub fn describe_record(rec: Option<CompileRecord>) -> String {
    let Some(rec) = rec else {
        return "unjudged".to_string();
    };
    let mut out = describe(rec.bits);
    if rec.loop_blocks_coarsened > 0 {
        out.push_str(&format!(
            "; loop blocks coarsened {}",
            rec.loop_blocks_coarsened
        ));
    }
    if rec.loop_windows_coarsened > 0 {
        out.push_str(&format!(
            "; loop windows coarsened {}",
            rec.loop_windows_coarsened
        ));
    }
    if rec.simplified_before_growth {
        out.push_str("; simplified before growth");
    }
    if rec.fully_unrolled > 0 {
        out.push_str(&format!("; fully unrolled {}", rec.fully_unrolled));
    }
    if rec.call_prefixes > 0 {
        out.push_str(&format!("; call prefixes {}", rec.call_prefixes));
    }
    if rec.builder_expansions > 0 {
        out.push_str(&format!("; builder expansions {}", rec.builder_expansions));
    }
    out
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
    // JIT round 17 wave 2 (lane monitor3): a coarsened loop block
    // ([`note_loop_block_coarsened`]) is evidence the bitset cannot carry
    // (see there for why `Simplified` misses it). Zero unless an opt-in
    // block-coarsening switch is on.
    rec.loop_blocks_coarsened > 0
        || is_worth_publishing_bits(rec.bits)
        || (rec.loop_windows_coarsened > 0 && loop_window_coarsening_is_evidence())
        // JIT round 17 wave 2 (lane irfeed, MON3R17-1): the `Simplified` a
        // growing pass masked. Opt-in.
        || (rec.simplified_before_growth && simplified_before_growth_is_evidence())
        // JIT round 17 wave 3 (lane irlower, IRFEED17-2): a loop the unroller
        // retired. Opt-in.
        || (rec.fully_unrolled > 0 && full_unroll_is_evidence())
        // nio20 wave 6, lane L3: a fired inline call prefix. Default ON
        // since wave 7b (behind the priced-trade refusal above).
        || (rec.call_prefixes > 0 && call_prefix_is_evidence())
        // JIT round 21 (lane accept): a call the builder expanded inline.
        // Default ON.
        || (rec.builder_expansions > 0 && builder_expansion_is_evidence())
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
        | Transform::Simplified.bit()
        | Transform::LoopPromoted.bit()
        | Transform::MainPostUnrolled.bit();
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
    // t1 wave 11 (lane JACC): read per call (once per lowered body, a cold
    // compile path), not latched in a process `OnceLock` -- the first VM's (or
    // the first thread override's) answer used to stick for every later VM in
    // the process, the defect t1 wave 2 removed from `x64/bce.rs`'s switches.
    match Err::<String, std::env::VarError>(std::env::VarError::NotPresent).as_deref() {
        Ok("always") => AcceptPolicy::Always,
        Ok("never") => AcceptPolicy::Never,
        // An unrecognised value takes the default rather than failing the
        // VM: this is a diagnostic lever, and a typo in it should not be
        // the reason a program does not start.
        _ => AcceptPolicy::Evidence,
    }
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

/// `CRATONVM_DBG_ACCEPT_CENSUS` (JIT round 21 lane tiermgr, ACCEPT21-1 first
/// step of `jit-proposals/jit-r21-accept-proposals-RETIRED-20261101.md`; diagnostic, default off): one
/// `[accept-census]` line per optimizing body that LOWERED, naming what the
/// evidence gate WOULD answer ([`evidence_gate_word`]) under every
/// `CRATONVM_C2_ACCEPT` policy, its priced estimate
/// ([`CompileRecord::added_ns_per_execution`]) and whether the body was kept.
/// No behaviour change. Read once per lowered body (cold).
///
/// The measurement it enables: run a row probe (or a suite) under
/// `CRATONVM_C2_ACCEPT=always` with the census on -- every lowered body
/// publishes, and the lines with `gate=refuse-*` name the bodies the default
/// gate would have thrown away -- then per-symbol profile both arms (the perf
/// map names each body): a `refuse-*` body that is faster than the default
/// arm's single-pass body is a gate error, and its `est_added_ns` / `evidence`
/// say which part of the estimate was wrong.
pub fn accept_census_enabled() -> bool {
    false
}

/// What the `evidence` policy of [`accept`] answers for `evidence`, whatever
/// policy is in force, without counting anything: `accept`, `accept-unjudged`
/// (nothing recorded), `refuse-cost` (priced as a regression) or
/// `refuse-no-evidence`. JIT round 21 lane tiermgr (ACCEPT21-1).
pub fn evidence_gate_word(evidence: Option<CompileRecord>) -> &'static str {
    match evidence {
        None => "accept-unjudged",
        Some(rec) if rec.added_ns_per_execution() > 0 => "refuse-cost",
        Some(rec) if is_worth_publishing(rec) => "accept",
        Some(_) => "refuse-no-evidence",
    }
}

/// The `[accept-census]` line ([`accept_census_enabled`]) for one lowered
/// optimizing body of `method` (`Class.name(desc)`). `kept`: the body went on
/// to publication (the gate, or the self-recursion exemption, accepted it);
/// `ir_bytes`: its code size; `osr_route`: built by the OSR optimizing route.
pub fn accept_census_line(
    method: &str,
    evidence: Option<CompileRecord>,
    kept: bool,
    selfrec_exempt: bool,
    ir_bytes: usize,
    osr_route: bool,
) -> String {
    let policy = match accept_policy() {
        AcceptPolicy::Always => "always",
        AcceptPolicy::Evidence => "evidence",
        AcceptPolicy::Never => "never",
    };
    format!(
        "[accept-census] {method} policy={policy} gate={} kept={kept} selfrec-exempt={selfrec_exempt} \
         est-added-ns={} ir-bytes={ir_bytes} route={} evidence={}",
        evidence_gate_word(evidence),
        evidence.map_or(0, |rec| rec.added_ns_per_execution()),
        if osr_route { "osr" } else { "entry" },
        describe_record(evidence),
    )
}

/// ACCEPT21-5 first step (t1 wave 11, lane JACC; diagnostic only, no gate
/// change): what the deopt guards INSIDE LOOPS of one scheduled optimizing
/// body keep alive. A guard's frame state names every local and stack value
/// the interpreter frame needs at its bci, so each such value stays live (a
/// register or a spill) across the whole loop even when the body itself has
/// stopped reading it -- the spill pressure `added_ns_per_execution` does not
/// price. Printed beside the `[ir] acceptance ...` lines
/// ([`GuardFrameCensus::describe`]), so a row like `R21AcceptGetClassFold`
/// can be read against the live set before any of it becomes a price.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GuardFrameCensus {
    /// `Op::Guard`s scheduled into a block of loop depth >= 1.
    pub in_loop_guards: u32,
    /// The largest number of DISTINCT non-constant values one of those guards'
    /// frame states names (locals, stack and monitors; a constant rebuilds
    /// from the metadata and keeps nothing live).
    pub max_live: u32,
    /// The bci of the guard that set [`Self::max_live`] (first one wins).
    pub max_live_bci: Option<usize>,
    /// In-loop guards whose frame state could not be found (no snapshot at
    /// their bci): counted, never guessed at.
    pub unresolved: u32,
    /// ACCEPT21-5's other half (j27 wave 7, lane T1ACCEPT; diagnostic): the
    /// Java calls (`Op::Call`, `Op::IndyBridge`) still scheduled inside a loop.
    /// Each is a safepoint and a caller-saved clobber the single-pass body
    /// pays too; a body that inlined them away shows a smaller number than
    /// the loop's invoke count, the saving the gate does not price.
    pub in_loop_calls: u32,
}

impl GuardFrameCensus {
    /// `guard-frame in-loop=<n> max-live=<m>[@<bci>][ unresolved=<u>]`.
    pub fn describe(&self) -> String {
        let mut s = format!(
            "guard-frame in-loop={} max-live={}",
            self.in_loop_guards, self.max_live
        );
        if let Some(bci) = self.max_live_bci {
            s.push_str(&format!("@{bci}"));
        }
        if self.unresolved > 0 {
            s.push_str(&format!(" unresolved={}", self.unresolved));
        }
        if self.in_loop_calls > 0 {
            s.push_str(&format!(" calls-in-loop={}", self.in_loop_calls));
        }
        s
    }
}

/// [`GuardFrameCensus`] for `graph` as `schedule` placed it. Loop depth is
/// `ir_schedule::loop_depths` (the dominator-based model the allocator
/// weights by); a schedule whose dominators do not match its block list (a
/// hand-built one) yields the empty census rather than a guess.
pub fn guard_frame_census(
    graph: &crate::ir::Graph,
    schedule: &crate::ir_schedule::Schedule,
) -> GuardFrameCensus {
    if schedule.dom.len() != schedule.blocks.len() {
        return GuardFrameCensus::default();
    }
    let depths = crate::ir_schedule::loop_depths(&schedule.blocks, &schedule.dom);
    guard_frame_census_over(
        graph,
        schedule
            .blocks
            .iter()
            .enumerate()
            .map(|(b, blk)| (depths.get(b).copied().unwrap_or(0), blk.nodes.as_slice())),
    )
}

/// The block walk of [`guard_frame_census`], split out so a test can hand it
/// `(loop depth, scheduled nodes)` pairs without building a schedule.
pub(crate) fn guard_frame_census_over<'a>(
    graph: &crate::ir::Graph,
    blocks: impl Iterator<Item = (u32, &'a [crate::ir::NodeId])>,
) -> GuardFrameCensus {
    use crate::ir::{NodeId, Op, NO_NODE};
    let mut out = GuardFrameCensus::default();
    for (depth, nodes) in blocks {
        if depth == 0 {
            continue;
        }
        for &id in nodes {
            let Some(node) = graph.nodes.get(id as usize) else {
                continue;
            };
            if matches!(node.op, Op::Call { .. } | Op::IndyBridge { .. }) {
                out.in_loop_calls += 1;
                continue;
            }
            let Op::Guard { bci } = node.op else {
                continue;
            };
            out.in_loop_guards += 1;
            // The node's own snapshot (an unrolled copy) first, then the
            // method's flat frame at the bci, then a splice chain at it -- the
            // order `ir_lower` resolves a guard's frame in.
            let sp = node
                .frame_snapshot
                .and_then(|i| graph.safepoints.get(i as usize))
                .or_else(|| {
                    graph
                        .safepoints
                        .iter()
                        .find(|s| s.bci == bci && !s.is_splice_state())
                })
                .or_else(|| {
                    graph
                        .safepoints
                        .iter()
                        .find(|s| s.bci == bci && s.is_guard_splice_state())
                });
            let Some(sp) = sp else {
                out.unresolved += 1;
                continue;
            };
            let mut live: Vec<NodeId> = sp
                .locals
                .iter()
                .chain(sp.stack.iter())
                .chain(sp.monitors.iter())
                .copied()
                .filter(|&v| v != NO_NODE)
                .filter(|&v| {
                    graph
                        .nodes
                        .get(v as usize)
                        .is_some_and(|n| !matches!(n.op, Op::Const(_) | Op::ConstF(_)))
                })
                .collect();
            live.sort_unstable();
            live.dedup();
            let n = u32::try_from(live.len()).unwrap_or(u32::MAX);
            if out.max_live_bci.is_none() || n > out.max_live {
                out.max_live = n;
                out.max_live_bci = Some(bci);
            }
        }
    }
    out
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
/// `docs/feature-designs/jit-ir-optimizations.md`.
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
mod tests;

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
// * a STICKY refusal ([`IrRefusalMemo::note_refused`]) — a decision that is not about
//   the inputs, such as the site-trap policy evicting a trapping body. It
//   never expires; re-offering such a method would re-plant the trap that
//   `JitVerdictRegistry::claim_site_trap_decision` will not act on a second time.
// * an INPUT-STAMPED refusal ([`IrRefusalMemo::note_refused_for_inputs`]) — the gate's
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
    /// JIT round 21 lane tiermgr (INDYLATE21-1,
    /// `CRATONVM_JIT_IR_MEMO_METHOD_INPUT_STAMP`): the method's own input
    /// stamp ([`IrRefusalMemo::note_method_inputs_changed`]) the refused build
    /// read its inputs under. `0` with the switch off.
    method_inputs: u32,
}

/// JIT round 21 lane tiermgr (INDYLATE21-1): the stamps one optimizing build
/// read its inputs under, taken at the compile door BEFORE the memo is asked
/// ([`IrRefusalMemo::ticket`]) and handed to the gate's refusal
/// ([`IrRefusalMemo::note_refused_for_ticket`]).
///
/// Why at the door and not at the refusal: an input that changes WHILE the
/// build runs (an `invokedynamic` site binds, a late-bind callee publishes, a
/// class initializes) was not read by that build. A refusal stamped at its own
/// end swallowed the change: the retry the change asked for (answered
/// `AlreadyQueued` while the build ran and owed, or `Enqueued` right after)
/// found the memo stamped with the new value and was SKIPPED, so the request
/// was lost exactly as the memo's section comment says a stale verdict must
/// not be.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct IrRefusalTicket {
    /// The memo key ([`crate::JitVerdictRegistry::ir_refusal_key`]).
    pub(crate) key: u64,
    /// [`inputs_generation`] at the door.
    generation: u64,
    /// The method's own input stamp at the door.
    method_inputs: u32,
    /// The switch was on at the door. Off, the refusal is stamped as before.
    at_door: bool,
}

impl IrRefusalTicket {
    /// Whether the stamps were taken at the door (the switch was on). The OSR
    /// route's own key (ACCEPT21-3) records without them: the method stamps
    /// are kept under the shared key.
    pub(crate) fn at_door(self) -> bool {
        self.at_door
    }
}

/// `CRATONVM_JIT_IR_MEMO_METHOD_INPUT_STAMP` (JIT round 21 lane tiermgr,
/// INDYLATE21-1; **default ON**, `=0` off): the acceptance gate's
/// input-stamped refusal also carries a per-method input stamp that every C2
/// retry door bumps (`tiered.rs` `CompilerCore::request_c2_retry`: a retry is
/// by definition "something this method's build reads has changed"), and the
/// stamps recorded are the ones the refused build READ (taken at the compile
/// door), not the ones current at its end. Off: the process-wide generation
/// alone ([`refusal_stamp_at_arm_enabled`] decides which). Read per compile
/// door and per retry (cold).
pub fn ir_memo_method_input_stamp_enabled() -> bool {
    true
}

/// One VM's IR refusal memo, owned by its [`crate::JitVerdictRegistry`]
/// (`ir_refusals`), keyed by `ir_refusal_memo_key(method, declaring class id,
/// the registry's redefine epoch)` ([`crate::JitVerdictRegistry::ir_refusal_key`]).
///
/// Interpreter round i1 wave 23, lane L5
/// (`i22-L5-ir-site-trap-decisions-are-process-wide-by-method-name`): until
/// then this was a process static keyed with the PROCESS redefine epoch. Two
/// VMs that load the same JDK in the same order give its classes the same ids,
/// so VM A's sticky refusal of `(Foo.bar, id 42)` refused VM B's `Foo.bar` the
/// optimizing tier though VM B never trapped, and any VM's redefinition
/// expired every VM's sticky refusals.
///
/// [`inputs_generation`] stays process-wide: a class initialization in one VM
/// re-offers another VM's input-stamped refusals, which costs a rebuild, never
/// a wrong answer.
#[derive(Default)]
pub(crate) struct IrRefusalMemo {
    entries: parking_lot::RwLock<rustc_hash::FxHashMap<u64, RefusalMemo>>,
    /// JIT round 21 lane tiermgr (INDYLATE21-1): each method's own input
    /// stamp, keyed like `entries`, moved by
    /// [`IrRefusalMemo::note_method_inputs_changed`]. Bounded by
    /// [`MAX_REFUSED_MEMOS`] like `entries`; a method absent reads `0`.
    method_inputs: parking_lot::RwLock<rustc_hash::FxHashMap<u64, u32>>,
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

impl IrRefusalMemo {
    /// Record a STICKY refusal under `key`: this method is not offered to the
    /// optimizing tier again, whatever loads later. For a decision that is not
    /// about the build's inputs (the site-trap policy). The gate's own verdicts
    /// should use [`IrRefusalMemo::note_refused_for_inputs`].
    pub(crate) fn note_refused(&self, key: u64) {
        let mut map = self.entries.write();
        let generation = inputs_generation();
        if let Some(e) = map.get_mut(&key) {
            e.sticky = true;
            e.generation = generation;
            return;
        }
        if map.len() < MAX_REFUSED_MEMOS {
            map.insert(
                key,
                RefusalMemo {
                    generation,
                    refusals: 1,
                    sticky: true,
                    method_inputs: 0,
                },
            );
        }
    }

    /// Record the acceptance gate's refusal of this method's optimizing body,
    /// stamped with the current [`inputs_generation`]: it is consulted only
    /// until the generation moves, at most [`MAX_INPUT_REFUSALS`] times over.
    pub(crate) fn note_refused_for_inputs(&self, key: u64) {
        self.note_refused_for_inputs_at(key, inputs_generation());
    }

    /// [`IrRefusalMemo::note_refused_for_inputs`], stamped with `generation`:
    /// the [`inputs_generation`] the refused build judged (t1 wave 8 lane
    /// ACCEPT, [`refusal_stamp_at_arm_enabled`]). An older generation than the
    /// current one makes the entry stop answering "refused" at once, so the
    /// next compile judges the inputs that arrived during this one.
    pub(crate) fn note_refused_for_inputs_at(&self, key: u64, generation: u64) {
        let mut map = self.entries.write();
        if let Some(e) = map.get_mut(&key) {
            // An older verdict (a build armed before another one's refusal
            // was written) never replaces a newer stamp; `u64::MAX` is a
            // re-offer's restamp (`grant_reoffer`), not a generation.
            if e.generation != u64::MAX && generation < e.generation {
                return;
            }
            if e.generation != generation {
                e.refusals = e.refusals.saturating_add(1);
                e.generation = generation;
            }
            return;
        }
        if map.len() < MAX_REFUSED_MEMOS {
            map.insert(
                key,
                RefusalMemo {
                    generation,
                    refusals: 1,
                    sticky: false,
                    method_inputs: 0,
                },
            );
        }
    }

    /// Has this method already been refused for want of evidence?
    ///
    /// Consulted BEFORE the IR pipeline runs, which is the whole point: the
    /// first refusal pays for a discarded build, and no later one does — until
    /// the inputs it was judged on change (see the "Refusal memo" section).
    ///
    /// JIT round 21 (lane lambdatrap,
    /// `jr21-tlocal-lambda-word-optimizing-body-traps-20261014`): a STICKY
    /// entry is not a gate verdict but the site-trap policy's IR ban
    /// ([`IrRefusalMemo::note_refused`], `helpers::despeculate_trapped_method`),
    /// and is honoured under EVERY acceptance policy and with the gate's memo
    /// switched off. Before, `CRATONVM_C2_ACCEPT=always` (or `=never`, or
    /// `CRATONVM_C2_ACCEPT_MEMO=0`) answered `false` for it: the recompile the
    /// policy asked for rebuilt the same trapping optimizing body, the policy
    /// was already claimed for the method (decided once), and the trap fired
    /// on every call for the rest of the run (`lambda-word-kernel` 8664 ns vs
    /// 100 single-pass). Unconditional: a correctness-of-routing fix.
    ///
    /// JIT round 21 lane tiermgr (INDYLATE21-1,
    /// `CRATONVM_JIT_IR_MEMO_METHOD_INPUT_STAMP`): an input-stamped entry also
    /// stops refusing once the method's own input stamp moved past the one
    /// its build read.
    pub(crate) fn already_refused(&self, key: u64) -> bool {
        let gate_memo_read =
            accept_policy() == AcceptPolicy::Evidence && refused_methods_memo_enabled();
        let now = inputs_generation();
        let Some(e) = self.entries.read().get(&key).copied() else {
            return false;
        };
        if e.sticky {
            return true;
        }
        if !gate_memo_read {
            return false;
        }
        if ir_memo_method_input_stamp_enabled() {
            memo_still_refuses_at(&e, now, self.method_inputs_of(key))
        } else {
            memo_still_refuses(&e, now)
        }
    }

    /// The method's own input stamp (`0` when never moved).
    pub(crate) fn method_inputs_of(&self, key: u64) -> u32 {
        self.method_inputs.read().get(&key).copied().unwrap_or(0)
    }

    /// JIT round 21 lane tiermgr (INDYLATE21-1): the stamps a build starting
    /// NOW reads its inputs under. Take it BEFORE [`Self::already_refused`];
    /// see [`IrRefusalTicket`].
    pub(crate) fn ticket(&self, key: u64) -> IrRefusalTicket {
        if !ir_memo_method_input_stamp_enabled() {
            return IrRefusalTicket {
                key,
                generation: 0,
                method_inputs: 0,
                at_door: false,
            };
        }
        // Method stamp first: a move between the two reads leaves the ticket
        // older than the inputs, which costs one rebuild, never a skip.
        let method_inputs = self.method_inputs_of(key);
        IrRefusalTicket {
            key,
            generation: inputs_generation(),
            method_inputs,
            at_door: true,
        }
    }

    /// [`Self::note_refused_for_inputs`] stamped with the stamps the refused
    /// build read ([`Self::ticket`]). A ticket older than the entry already
    /// recorded (a slower concurrent build of the same method) leaves the
    /// newer entry as it is.
    pub(crate) fn note_refused_for_ticket(&self, ticket: IrRefusalTicket) {
        if !ticket.at_door {
            self.note_refused_for_inputs(ticket.key);
            return;
        }
        let (generation, method_inputs) = (ticket.generation, ticket.method_inputs);
        let mut map = self.entries.write();
        if let Some(e) = map.get_mut(&ticket.key) {
            let older = e.generation != u64::MAX
                && (generation < e.generation
                    || (generation == e.generation && method_inputs < e.method_inputs));
            if older {
                return;
            }
            if e.generation != generation || e.method_inputs != method_inputs {
                e.refusals = e.refusals.saturating_add(1);
                e.generation = generation;
                e.method_inputs = method_inputs;
            }
            return;
        }
        if map.len() < MAX_REFUSED_MEMOS {
            map.insert(
                ticket.key,
                RefusalMemo {
                    generation,
                    refusals: 1,
                    sticky: false,
                    method_inputs,
                },
            );
        }
    }

    /// JIT round 21 lane tiermgr (INDYLATE21-1): something this method's
    /// build reads besides the bytecode may now answer differently (a C2
    /// retry door asked for it). Moves the method's own input stamp, so an
    /// input-stamped refusal recorded under the old stamp -- including the
    /// one a build in flight right now records when it ends -- is judged
    /// again. At the table's bound a method not yet in it gets its
    /// input-stamped entry dropped instead ([`Self::forget_input_refusal`]),
    /// which covers a refusal already recorded but not one a build in flight
    /// records later. Returns whether the stamp moved.
    pub(crate) fn note_method_inputs_changed(&self, key: u64) -> bool {
        {
            let mut map = self.method_inputs.write();
            if let Some(n) = map.get_mut(&key) {
                *n = n.wrapping_add(1);
                return true;
            }
            if map.len() < MAX_REFUSED_MEMOS {
                map.insert(key, 1);
                return true;
            }
        }
        self.forget_input_refusal(key);
        false
    }

    /// Does `key` hold the site-trap policy's STICKY ban (JIT round 21, lane
    /// lambdatrap)? `false` when the entry could not be recorded (the memo is
    /// at [`MAX_REFUSED_MEMOS`]), which is the case the policy's repeat path
    /// re-decides for.
    pub(crate) fn holds_sticky_ban(&self, key: u64) -> bool {
        self.entries.read().get(&key).is_some_and(|e| e.sticky)
    }

    /// JIT round 21 (lane indylate): drop the acceptance gate's INPUT-STAMPED
    /// refusal recorded under `key`, so the next optimizing compile of the
    /// method is built and judged again. A STICKY entry (the site-trap
    /// policy's IR ban) is kept. `true` when an entry was dropped.
    pub(crate) fn forget_input_refusal(&self, key: u64) -> bool {
        let mut map = self.entries.write();
        if map.get(&key).is_some_and(|e| !e.sticky) {
            map.remove(&key);
            return true;
        }
        false
    }

    /// See [`crate::JitVerdictRegistry::grant_ir_reoffer`]. The restamp is
    /// `u64::MAX`, a generation [`inputs_generation`] never reaches, so the
    /// entry stops answering "refused" until the next refusal re-stamps it
    /// (counted, since the generations differ).
    pub(crate) fn grant_reoffer(&self, key: u64) -> bool {
        let mut map = self.entries.write();
        match map.get_mut(&key) {
            Some(e) if !e.sticky && e.refusals < MAX_INPUT_REFUSALS => {
                e.generation = u64::MAX;
                true
            }
            _ => false,
        }
    }

    /// The entry recorded under `key`, if any (tests read it directly).
    #[cfg(test)]
    fn entry(&self, key: u64) -> Option<RefusalMemo> {
        self.entries.read().get(&key).copied()
    }

    /// Whether any entry is recorded under `key`, whatever the policy latch.
    pub(crate) fn is_recorded(&self, key: u64) -> bool {
        self.entries.read().contains_key(&key)
    }
}

/// Is a memo entry recorded at some generation still a refusal at `now`?
fn memo_still_refuses(e: &RefusalMemo, now: u64) -> bool {
    e.sticky || e.generation == now || e.refusals >= MAX_INPUT_REFUSALS
}

/// [`memo_still_refuses`] that also asks the method's own input stamp
/// (INDYLATE21-1): an input-stamped entry refuses only while BOTH stamps are
/// the ones its build read, or once it is final.
fn memo_still_refuses_at(e: &RefusalMemo, now: u64, method_now: u32) -> bool {
    e.sticky
        || e.refusals >= MAX_INPUT_REFUSALS
        || (e.generation == now && e.method_inputs == method_now)
}

impl crate::JitVerdictRegistry {
    /// The IR refusal memo key of the method `ir_method_hash`
    /// ([`crate::ir_method_memo_hash`]) of the loaded class `class_id`, under
    /// THIS registry's redefine epoch: what the compile door reads and every
    /// writer records under.
    pub(crate) fn ir_refusal_key(
        &self,
        ir_method_hash: u64,
        class_id: cratonvm_types::ClassId,
    ) -> u64 {
        crate::ir_refusal_memo_key(ir_method_hash, class_id, self.redefine_epoch.load())
    }

    /// This VM's IR refusal memo.
    pub(crate) fn ir_refusals(&self) -> &IrRefusalMemo {
        &self.ir_refusals
    }

    /// Mark a method refused in this VM's IR refusal memo (STICKY) under the
    /// key the compile door reads: [`Self::ir_refusal_key`] of the method hash
    /// and the declaring class id. The site-trap policy in
    /// `vm/src/jit/helpers.rs` calls it.
    ///
    /// A writer that passes the bare [`crate::ir_method_memo_hash`] marks an
    /// entry no reader ever asks for; the site-trap policy once did exactly
    /// that.
    pub fn note_ir_method_refused(
        &self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        declaring_class_id: cratonvm_types::ClassId,
    ) {
        let hash = crate::ir_method_memo_hash(class_name, method_name, descriptor);
        self.ir_refusals
            .note_refused(self.ir_refusal_key(hash, declaring_class_id));
    }

    /// JIT r22 wave 2, lane inline (INLINE22-1, opt-in
    /// `CRATONVM_JIT_C2_ACCEPTANCE_REOFFER`): grant the method one re-judgement
    /// by the acceptance gate although the build inputs' generation has not
    /// moved -- its profile has matured instead (the tiered manager's
    /// acceptance re-offer). An INPUT-STAMPED entry under
    /// [`MAX_INPUT_REFUSALS`] is restamped to a generation no compile runs
    /// under, so the next compile is offered to the IR pipeline; a fresh
    /// refusal then counts as one more refusal, so the memo's bound is
    /// unchanged. A sticky entry, a final one, or no entry: nothing to grant.
    /// Returns whether a re-judgement was granted.
    pub fn grant_ir_reoffer(
        &self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        declaring_class_id: cratonvm_types::ClassId,
    ) -> bool {
        let hash = crate::ir_method_memo_hash(class_name, method_name, descriptor);
        self.ir_refusals
            .grant_reoffer(self.ir_refusal_key(hash, declaring_class_id))
    }

    /// Is the site-trap policy's IR ban ([`Self::note_ir_method_refused`]) in
    /// effect for this method under this registry's redefine epoch? JIT round
    /// 21 (lane lambdatrap): the policy's repeat path asks it, so a ban that
    /// could not be recorded is re-decided instead of leaving a trapping body
    /// installed for good.
    pub fn ir_method_ban_holds(
        &self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        declaring_class_id: cratonvm_types::ClassId,
    ) -> bool {
        let hash = crate::ir_method_memo_hash(class_name, method_name, descriptor);
        self.ir_refusals
            .holds_sticky_ban(self.ir_refusal_key(hash, declaring_class_id))
    }

    /// JIT round 21 (lane indylate): forget the acceptance gate's
    /// input-stamped "no evidence" refusal of this method (never the
    /// site-trap policy's sticky ban), under the key the compile door reads.
    ///
    /// For a caller that is about to ask for the method's optimizing
    /// recompile because a build input the gate's stamp does not track has
    /// changed -- an `invokedynamic` site that has just bound a constant or
    /// guarded target the builder now plans as a plain call. Without it the
    /// retry door answers `Enqueued` and the compile door then SKIPS the build
    /// (`already_refused`: same inputs generation, no class initialized since),
    /// so the request is silently lost and the method keeps the single-pass
    /// body's bridge call for good. `true` when an entry was dropped.
    pub fn forget_ir_input_refusal(
        &self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        declaring_class_id: cratonvm_types::ClassId,
    ) -> bool {
        let hash = crate::ir_method_memo_hash(class_name, method_name, descriptor);
        self.ir_refusals
            .forget_input_refusal(self.ir_refusal_key(hash, declaring_class_id))
    }

    /// JIT round 21 lane tiermgr (INDYLATE21-1,
    /// `CRATONVM_JIT_IR_MEMO_METHOD_INPUT_STAMP`): something this method's
    /// optimizing build reads besides the bytecode may now answer differently.
    /// The tier manager's C2 retry door calls it for every request (late-bind
    /// callee served, indy target bound, deferred `new` class loaded,
    /// self-lock mirror minted, refused publication, ...). Moves the method's
    /// own input stamp under the key the compile door reads, so the gate's
    /// input-stamped refusal is judged again; the site-trap policy's sticky
    /// ban is untouched. `false` with the switch off, or when the stamp table
    /// is full (the method's recorded refusal is then dropped instead).
    pub fn note_ir_method_inputs_changed(
        &self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        declaring_class_id: cratonvm_types::ClassId,
    ) -> bool {
        if !ir_memo_method_input_stamp_enabled() {
            return false;
        }
        let hash = crate::ir_method_memo_hash(class_name, method_name, descriptor);
        self.ir_refusals
            .note_method_inputs_changed(self.ir_refusal_key(hash, declaring_class_id))
    }

    /// Whether this registry's refusal memo holds an entry for the method
    /// `ir_method_hash` of `class_id` under its current redefine epoch,
    /// whatever the acceptance policy latch says.
    #[cfg(test)]
    pub(crate) fn ir_refusal_is_recorded(
        &self,
        ir_method_hash: u64,
        class_id: cratonvm_types::ClassId,
    ) -> bool {
        self.ir_refusals
            .is_recorded(self.ir_refusal_key(ir_method_hash, class_id))
    }
}

/// `CRATONVM_C2_ACCEPT_MEMO=0` restores the un-memoized gate, which rebuilds
/// and re-discards on every compile of a refused method. Kept because it is
/// the arm that separates "the gate refuses the right methods" from "the memo
/// remembers the right verdict".
/// Read once per compile, no `static` (j27 LATCHES).
pub(crate) fn refused_methods_memo_enabled() -> bool {
    true
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
// path is the consumer that acts on it (the optimizing OSR door also reads it,
// for its `CRATONVM_DBG_JITC` line), on the same thread, immediately after
// `try_compile` returns: a REFUSED verdict on a method that already has a
// published body means "leave the C1 body alone" -- no publish, no epoch bump,
// no invalidation.
//
// A thread-local rather than a return value, because threading a 23rd
// parameter through `try_compile` to carry one bit is worse than a slot the
// caller reads at a known point. It is take-and-clear so a stale verdict from
// an earlier compile can never be read as this one's.
//
// Round 15 wave 1 (lane calls): take-and-clear alone did not guarantee that.
// A compile NESTED in another's resolvers (the eager callee chain, run from the
// outer method's single-pass fall-through after the outer gate published) took
// the outer verdict as its own, and a door that publishes but never takes (the
// optimizing door's in-place callee compile) left one for the next compile on
// the thread. The VM's reader now hands a pending verdict back across a nested
// compile and discards a stale one at a top-level entry
// (`jit_bridge.rs` `NestedVerdictHandOff`).
//
// Round 15 wave 2 (lane calls2, CR15-4): the optimizing door
// (`compile_optimizing_artifact`) takes its own verdict right after its compile
// and returns it as a value (`PinnedArtifact::verdict`), and its in-place callee
// compile hands the enclosing verdict back and discards its own, so neither
// leaves one in the slot. The slot is still the transport out of
// `try_compile_inner`: a `CompiledMethod` field would need the single-pass
// fall-through to carry the IR attempt's verdict, which is the next step
// (`jit-r15w2-calls2-proposals-RETIRED-20261003.md`, C2W2-3).

thread_local! {
    /// `.0`: the last optimizing compile's acceptance verdict
    /// ([`take_last_verdict`]). `.1`: a compile on this thread ended in an
    /// abandoned supersede ([`take_supersede_abandoned`]). One slot for both
    /// keeps the jit static count at the ratchet's baseline.
    static LAST_VERDICT: std::cell::Cell<(Option<bool>, bool)> = const {
        std::cell::Cell::new((None, false))
    };
}

/// Record this compile's acceptance verdict for the caller. `true` = accepted.
pub fn publish_verdict(accepted: bool) {
    LAST_VERDICT.with(|c| c.set((Some(accepted), c.get().1)));
}

/// Read and clear the verdict of the last optimizing compile on this thread.
///
/// `None` means no optimizing compile recorded one — the method never reached
/// the IR pipeline at all — which a caller must treat as "no opinion", not as
/// a refusal.
pub fn take_last_verdict() -> Option<bool> {
    LAST_VERDICT.with(|c| {
        let (verdict, abandoned) = c.get();
        c.set((None, abandoned));
        verdict
    })
}

/// Supersedes abandoned because the optimizing body carried no evidence and a
/// baseline body already existed.
static SUPERSEDES_ABANDONED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Also sets the abandoned-supersede flag in `LAST_VERDICT` on the thread that
/// ran the compile, read once by the background task wrapper
/// ([`take_supersede_abandoned`]).
pub fn note_supersede_abandoned() {
    SUPERSEDES_ABANDONED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    LAST_VERDICT.with(|c| c.set((c.get().0, true)));
}

/// Did the compile this thread just ran end in an abandoned supersede (the
/// optimizing body carried no evidence and a baseline body stayed published)?
/// Clears the flag, so call it once before a compile and once after.
pub fn take_supersede_abandoned() -> bool {
    LAST_VERDICT.with(|c| {
        let (verdict, abandoned) = c.get();
        c.set((verdict, false));
        abandoned
    })
}

/// How many epoch bumps and republishes the verdict avoided.
pub fn supersedes_abandoned() -> u64 {
    SUPERSEDES_ABANDONED.load(std::sync::atomic::Ordering::Relaxed)
}

/// JIT round 21 (lane accept): a builder expansion is recorded apart from the
/// bitset, named by `describe_record`, and evidence by default (`=0` kills).

/// JIT round 21 (lane lambdatrap,
/// `jr21-tlocal-lambda-word-optimizing-body-traps-20261014`): the site-trap
/// policy's STICKY ban is honoured under every acceptance policy; a gate
/// refusal (input-stamped) is still read only under `evidence`.
#[cfg(test)]
mod jr21_lambdatrap_sticky_ban_tests;

#[cfg(test)]
mod jr21_indylate_refusal_forget_tests;

/// JIT round 21 lane tiermgr (INDYLATE21-1,
/// `CRATONVM_JIT_IR_MEMO_METHOD_INPUT_STAMP`): the per-method input stamp and
/// the build-start ticket. Reads entries and `memo_still_refuses_at` directly,
/// not `already_refused` (the process-wide policy latch).
#[cfg(test)]
mod jr21_tiermgr_method_input_stamp_tests;

#[cfg(test)]
mod t1_w11_jacc_guard_frame_census_tests;
