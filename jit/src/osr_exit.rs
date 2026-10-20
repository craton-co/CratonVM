// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Where an OSR exit actually landed, and whether the bci it names has exactly
//! one resume image.
//!
//! `docs/feature-designs/jit-osr-exit-and-recompile.md` steps 3–4, and the
//! lane's "what to refuse":
//!
//! > An OSR exit whose resume bci has more than one possible native image, or
//! > none. Under a loop transform the reverse mapping is one-to-many inside the
//! > transformed region, and picking the wrong image is a wrong-code bug rather
//! > than a missed optimisation.
//!
//! # Two questions, deliberately kept apart
//!
//! **1. Where did the exit land?** [`classify_exit_site`] answers it against
//! `CompiledMethod::osr_exit_points` — the set the emitter recorded a
//! loop-boundary exit map for. The lane's step 4 is exactly this: the set
//! "exists and is populated under the bytecode transform, but nothing
//! cross-checks it against where exits are actually taken". It is a
//! **classification, not a verdict** — an exit off the loop boundary is a
//! legitimate, different exit path (an `invokedynamic` uncommon trap, a
//! speculative-BCE guard), and refusing one would be refusing a shape that
//! works. What was missing is that nobody could tell the three apart.
//!
//! **2. Is the resume bci unambiguous?** [`resume_image`] answers it against
//! the artifact's `deopt_points`. This one IS a verdict, and it belongs at
//! **admission** rather than at exit — see below.
//!
//! # Why the ambiguity check runs at admission, not at exit
//!
//! Refusing at exit is not free the way refusing at entry is. `resume_after_exit`
//! returning `Err` means the caller must propagate or unwind; the VM's in-place
//! transfer treats it as "cannot resume", and the historical fallback from there
//! is the safe reject — *"continue interpreting THIS frame from where it was"* —
//! which after a committed body **re-runs every iteration since entry**. That is
//! the exact defect the lane exists for, so a refusal added at exit would be a
//! way to cause it, not a way to prevent it.
//!
//! At admission nothing has run. `CompiledMethod::osr_exit_policy` already walks
//! every deopt point of the artifact before the entry is taken, for exactly this
//! reason (`docs/jit/on-stack-replacement.md` §4 item 4: *"Discovering that after
//! entering is useless: by then the only options are to replay the committed
//! iterations or to lose them"*). The ambiguity check joins it there, and the
//! check at exit stays as an assertion of something admission already
//! guaranteed — unreachable rather than merely rare, which is the same argument
//! the `MaterializationRequired` guard is landed under.
//!
//! # What "the same image" means — and what the measurement changed
//!
//! Two deopt points at one bci are interchangeable **for the resume** when they
//! agree on the one field the resume reads by bci:
//!
//! | Field | Read by | Consequence of picking the wrong copy |
//! |---|---|---|
//! | `semantics` | [`crate::OsrEntryPlan::resume_after_exit`] | parking the interpreter at a bci that already took effect — the same double-execution one bytecode down. **Wrong code.** |
//! | `reason` | the de-speculation step at the OSR-exit *reject* sink | the wrong recompile policy — `OsrExit`'s count-based retry where `UnreachedCode`'s "give up immediately" was meant (the recorded Groovy `IndyInterface` regression). **Wrong policy, on a path admission makes unreachable.** |
//!
//! The first draft of this module refused on `(semantics, reason)`, on the
//! reading that both are picked arbitrarily and both matter. Running it decided
//! the question the other way, and the measurement is why the predicate is what
//! it is:
//!
//! > `CratonBench`, 2026-08-04: **10 of 11** OSR entry refusals were
//! > `osr-entry-ambiguous-exit-image` on `CratonBench.matrixKernel(I)I`, at
//! > `bci 16`, where the loop-boundary map (`+0x37e`, `OsrExit`) and the
//! > speculative-BCE guard (`+0x3c1`, `BoundsCheck`) sit on the same bci. They
//! > **agree on `semantics`** — `ResumeSemantics::for_reason` answers
//! > `REEXECUTE` for everything but `PendingException`, so a reason
//! > disagreement never implies a semantics one — and the refusal is memoed as
//! > permanent, so it cost that kernel its OSR entirely, for the life of the
//! > process.
//!
//! A loop header carrying both its exit map and its range guard is the ordinary
//! shape of a compiled counted loop, not a defect. So:
//!
//! * **`semantics` disagreement refuses** the entry. That is the wrong-code
//!   half, and it is what the lane's "what to refuse" is about.
//! * **`reason`-only disagreement is counted, not refused**
//!   ([`ResumeImage::Unique::reason_ambiguous`]). Its only consumer is the
//!   de-speculation lookup at the OSR-exit *reject* sink, and an admitted entry
//!   cannot reach that sink — `osr_exit_policy` refuses at admission every
//!   artifact whose points could land there. Counting it keeps the shape
//!   visible instead of silently tolerated, which is the difference between a
//!   decision and an oversight.
//!
//! Which `semantics` disagreement is actually reachable is worth stating,
//! because it is not the obvious one. A `RESUME` point anywhere in an artifact
//! is refused wholesale by `osr_exit_policy`'s per-point rule (its successor
//! bci is not computable in this crate), so it never reaches this check. A
//! `RETHROW` point is explicitly allowed to exist — such points are stashed via
//! `take_exceptional_frame` and never routed to a resume — so the reachable
//! disagreement is `REEXECUTE` vs `RETHROW`: a `PendingException` point sharing
//! a bci with a speculative-dispatch guard or a loop-boundary exit map.
//!
//! **And that one must NOT refuse (corrected 2026-08-17).** The paragraph above
//! named the reachable case correctly and then drew the wrong conclusion from
//! it: it counted a `RETHROW` point as a candidate resume image. It is not one,
//! by the same sentence that admits it — never routed to a resume. So a bci
//! carrying one `REEXECUTE` point and one `RETHROW` point has exactly ONE
//! resume image, and the resume bci is not arbitrary at all.
//!
//! `resume_image` now skips `rethrow_exception` points for that reason. What
//! made the correction is a measurement, not the argument: after the RBC.6b
//! lift admitted `try`/`catch` methods to OSR, a `try { foo(x); } catch (...)`
//! loop puts a `ReceiverTypeChanged` guard and a `PendingException` frame on
//! the same invoke bci, which is the ordinary shape rather than an exotic one.
//! `probes/OsrExcTableProbe.java` reported `osr_entered=0
//! osr_entry_refused_ambiguous_image=15`: the artifact compiled, every
//! correctness arm passed, and NOTHING EVER ENTERED THE COMPILED BODY.
//!
//! Two points that both claim to be resume images and disagree are still
//! refused; that half of the rule is unchanged and is the wrong-code half.
//!
//! Everything else about two points at one bci is *supposed* to differ:
//! `native_offset` by construction (that IS what makes them two images), and
//! the per-slot machine locations with it. `frame_state`'s slot **types** are
//! re-verified against the live interpreter frame by the entry contract, so a
//! divergence there is not this check's business —
//! `x64::loop_rewrite::deopt_point_difference` already draws that same line and
//! argues it at length.

use crate::deopt::{DeoptReason, DeoptimizationPoint, ResumeSemantics};

/// `CRATONVM_DEOPT_CHAIN_POINTS_OFF_ROOT_BCIS` -- JIT round 19 proposal wave
/// 10, lane deopt10 (`i62-L2-proposal-single-pass-chain-points-leave-the-roots-bci-key-space`).
/// **Opt-in, default OFF.** A single-pass chain point (a guard of an
/// outermost splice, a post-call exit after a real call inside a splice) is
/// recorded in `CompiledMethod::deopt_points` with the spliced CALLEE's bci
/// and carries its caller chain ([`DeoptimizationPoint::names_a_root_bci`] is
/// `false`). On, the by-bci readers that ask about a ROOT bci skip it:
/// [`resume_image`] (and through it [`first_ambiguous_resume_bci`] /
/// [`has_reason_ambiguous_bci`], the OSR admission), [`trap_reason_at_bci`] /
/// [`deopt_reason_at_bci`] (the cause-less reason lookups) and the OSR entry
/// contract (`CompiledMethod::osr_entry_frame_state`). Off: today's scans,
/// where a chain point whose callee bci equals a root bci joins that bci's
/// candidates -- every consequence fail-closed (a refused OSR entry, an
/// `Ambiguous` reason charged as `UncommonTrap`). [`classify_exit_site`] (a
/// metric) keeps the raw scan: its caller hands it the frame's own bci, which
/// for a chain exit IS the callee bci the chain point records.
///
/// Read per call (cold: an OSR admission, a cause-less trap lookup, an exit);
/// not cached in a static (the `process_global_statics_ratchet`).
pub fn chain_points_off_root_bcis_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_DEOPT_CHAIN_POINTS_OFF_ROOT_BCIS")
}

/// Does `p` count as a candidate for a ROOT bci under `off_root`
/// ([`chain_points_off_root_bcis_enabled`])? Always with the switch off.
#[inline]
fn counts_at_root_bci(p: &DeoptimizationPoint, off_root: bool) -> bool {
    !off_root || p.names_a_root_bci()
}

/// Where an OSR exit landed, relative to the exit maps the emitter recorded.
///
/// Mostly a classification rather than a verdict — the first two are both
/// reachable in a correct VM — but the last two are disagreements between the
/// two sets, and both are expected to stay at zero.
///
/// # `osr_exit_points` is not what its name says
///
/// It is written by `Compiler::emit_osr_exit_map_at_reason`, which is shared by
/// **two** emitters: the loop-boundary exit map (reason `OsrExit`) and the
/// `invokedynamic` uncommon trap (reason `UnreachedCode`). So membership alone
/// does not distinguish "a whole number of iterations completed" from "the body
/// hit a site it can never execute" — and those are the two readings the lane
/// wants told apart, because the first is the transfer working and the second
/// is an artifact that bails on every trip.
///
/// Hence the classification consults the *reason* as well as the set, and
/// `LoopBoundary` means both.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OsrExitSite {
    /// The bci is one of `CompiledMethod::osr_exit_points` **and** its recorded
    /// point is `OsrExit` — a true loop-boundary exit map. This is the exit the
    /// in-place transfer was designed for: a whole number of iterations has
    /// completed and the header has not been re-entered.
    LoopBoundary,
    /// A recorded deopt point that is not a loop-boundary exit map: an
    /// `invokedynamic` uncommon trap (which shares the exit-map machinery, so
    /// it appears in `osr_exit_points` too) or a speculative-BCE guard (which
    /// does not). Legitimate — the point's own `semantics` is still what makes
    /// the resume exact — but the exit is not on an iteration boundary.
    OffLoopBoundary(DeoptReason),
    /// The two sets disagree: an `OsrExit`-reason point whose bci is **not** in
    /// `osr_exit_points`. They are written by the same function, so this is the
    /// cross-check the lane asks for and it must read zero. Non-zero means one
    /// of the two was rebuilt (the loop transform moves both between coordinate
    /// spaces) and the other was not.
    ExitMapMissing,
    /// An `OsrExit` point outside `osr_exit_points` that the artifact lists in
    /// `CompiledMethod::post_call_own_exit_bcis`: a single-pass post-call exit
    /// that bakes its own stub beside a guard of another frame (interpreter
    /// round i2 wave 56, lane L2). Off the loop boundary, legitimate, and kept
    /// apart from [`Self::ExitMapMissing`] so that cross-check still reads
    /// zero. Answered only by `CompiledMethod::classify_osr_exit_site`;
    /// [`classify_exit_site`] alone never sees the list. Since wave 62 also
    /// an `OsrExit` point with a caller chain (a post-call exit after a real
    /// call inside a single-pass splice, whose bci is the callee's).
    PostCallOwnPoint,
    /// Neither set names this bci. Nothing in this artifact can describe the
    /// frame, so the transfer must refuse. **Expected to stay at zero**;
    /// non-zero means a stash reached an artifact that cannot describe it.
    Unrecorded,
}

impl OsrExitSite {
    /// The [`crate::metrics::OSR_EVENTS`] row this site increments.
    pub fn metric(&self) -> &'static str {
        match self {
            OsrExitSite::LoopBoundary => "osr_exit_at_loop_boundary",
            OsrExitSite::OffLoopBoundary(_) => "osr_exit_off_loop_boundary",
            OsrExitSite::ExitMapMissing => "osr_exit_map_missing",
            OsrExitSite::PostCallOwnPoint => "osr_exit_at_post_call_own_point",
            OsrExitSite::Unrecorded => "osr_exit_bci_unrecorded",
        }
    }
}

/// Classify an exit taken at `bci` against the artifact's recorded exit maps.
///
/// Takes the two vectors rather than a `CompiledMethod`, so the classification
/// can be tested against synthetic metadata instead of against whatever a real
/// compile happened to produce — the same reason
/// [`crate::osr_contract::check`] takes vectors.
pub fn classify_exit_site(
    osr_exit_points: &[usize],
    deopt_points: &[DeoptimizationPoint],
    bci: u32,
) -> OsrExitSite {
    classify_exit_site_keyed(osr_exit_points, deopt_points, bci, None)
}

/// `CRATONVM_OSR_EXIT_SITE_FRAME_KEYED` -- JIT round 19 wave 11, lane tail11
/// (DEOPT10-3 of `jit-r19-deopt10-proposals-RETIRED-20261009.md`), opt-in: the OSR-exit sink
/// classifies an exit by the frame's KIND as well as its bci
/// ([`classify_exit_site_keyed`] with `Some(chain_exit)`), so a root exit
/// counts only root points and a chain exit only chain points. Off: the raw
/// scan, where a chain point at a callee bci equal to the root exit's bci (or
/// the reverse) joins the candidates. A metric only (`osr_exit_*` rows);
/// read per exit, not cached in a static.
pub fn osr_exit_site_frame_keyed_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_OSR_EXIT_SITE_FRAME_KEYED")
}

/// [`classify_exit_site`] keyed by the exit frame's kind. `None` is the raw
/// scan (every non-rethrow point at `bci`, the loop-boundary set asked by bci
/// alone). `Some(chain_exit)` counts only points of the same kind
/// ([`DeoptimizationPoint::names_a_root_bci`] `== !chain_exit`) and, for a
/// chain exit, never answers from `osr_exit_points`: that set names ROOT
/// loop headers, and a chain exit's bci is a spliced callee's.
pub fn classify_exit_site_keyed(
    osr_exit_points: &[usize],
    deopt_points: &[DeoptimizationPoint],
    bci: u32,
    chain_exit: Option<bool>,
) -> OsrExitSite {
    let same_kind = |p: &DeoptimizationPoint| chain_exit.is_none_or(|c| p.names_a_root_bci() != c);
    let in_set = chain_exit != Some(true) && osr_exit_points.iter().any(|p| *p == bci as usize);
    // The reason comes from the bci-lookup rule ([`deopt_reason_at_bci`])
    // WITHOUT its wave-49 `OsrExit` skip ([`raw_reason_at_bci_where`]): this names
    // the exit's site, and the tie-break below must still see the `OsrExit`
    // map beside a guard.
    let reason = match raw_reason_at_bci_where(deopt_points, bci, &same_kind) {
        ReasonAtBci::Unique(r) => r,
        // Several non-`RETHROW` points name different reasons, **only to NAME
        // the site**: this arm feeds a counter, not a decision, and dropping
        // the count would hide the exit entirely. Nothing that escalates a
        // method or de-specs a site may take this shortcut — that is precisely
        // what `ReasonAtBci::Ambiguous` exists to stop.
        //
        // `OsrExit` WINS the tie rather than list order deciding it, and that
        // is a correctness property of the classification rather than a
        // preference. `LoopBoundary`/`ExitMapMissing` are the two answers that
        // depend on the reason being `OsrExit`, and the ambiguous shape is the
        // ORDINARY one for a compiled counted loop: the loop-boundary exit map
        // and the speculative-BCE range guard sit on the same header bci (the
        // `CratonBench.matrixKernel` bci-16 measurement in the module note).
        // Taking whichever the emitter happened to record first made a true
        // loop-boundary exit read as `osr_exit_off_loop_boundary`, and — the
        // half that matters — made an `OsrExit` point MISSING from
        // `osr_exit_points` read as `OffLoopBoundary` instead of
        // `ExitMapMissing`, silencing the one cross-check this module exists
        // for on exactly the bci shape where the two sets are most likely to
        // have been rebuilt apart. The tie-break can only turn the answer INTO
        // one of the two `OsrExit` arms, never away from them.
        ReasonAtBci::Ambiguous => {
            let at_bci = |p: &&DeoptimizationPoint| {
                p.bci == bci && !p.semantics.rethrow_exception && same_kind(p)
            };
            let osr_exit = deopt_points
                .iter()
                .find(|p| at_bci(p) && p.reason == DeoptReason::OsrExit);
            match osr_exit.or_else(|| deopt_points.iter().find(at_bci)) {
                Some(p) => p.reason,
                None => DeoptReason::UnreachedCode,
            }
        }
        // No point at this bci can describe the frame at all (`RETHROW` points
        // are not resume images and do not count). Same predicate
        // `resume_image` answers `ResumeImage::None` for.
        ReasonAtBci::None => return OsrExitSite::Unrecorded,
    };
    match (in_set, reason) {
        (true, DeoptReason::OsrExit) => OsrExitSite::LoopBoundary,
        (false, DeoptReason::OsrExit) => OsrExitSite::ExitMapMissing,
        (_, other) => OsrExitSite::OffLoopBoundary(other),
    }
}

impl crate::CompiledMethod {
    /// [`Self::classify_osr_exit_site`] for the exit FRAME the sink holds:
    /// `chain_exit` is `!rframe.caller_frames.is_empty()`. JIT round 19 wave
    /// 11, lane tail11 (DEOPT10-3), behind
    /// [`osr_exit_site_frame_keyed_enabled`] (opt-in): on, a root exit counts
    /// only root points and a chain exit only chain points
    /// ([`classify_exit_site_keyed`]); off, exactly
    /// [`Self::classify_osr_exit_site`]. Either way, under `CRATONVM_DBG_DEOPT`
    /// an exit whose two classifications differ prints one census line
    /// (`frame-keyed-differs`): zero such lines on a suite says the raw metric
    /// is already exact there.
    pub fn classify_osr_exit_site_of_frame(&self, bci: u32, chain_exit: bool) -> OsrExitSite {
        let keyed_on = osr_exit_site_frame_keyed_enabled();
        let dbg = cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_DEOPT");
        if !keyed_on && !dbg {
            return self.classify_osr_exit_site(bci);
        }
        let raw = self.classify_osr_exit_site(bci);
        let keyed = match classify_exit_site_keyed(
            &self.osr_exit_points,
            &self.deopt_points,
            bci,
            Some(chain_exit),
        ) {
            // The post-call own-exit remap of `classify_osr_exit_site`, kept
            // to the same kind: a root exit by the root list, a chain exit by
            // a chain `OsrExit` point at its callee bci.
            OsrExitSite::ExitMapMissing
                if (!chain_exit
                    && self
                        .post_call_own_exit_bcis
                        .iter()
                        .any(|&b| u32::try_from(b) == Ok(bci)))
                    || (chain_exit
                        && self.deopt_points.iter().any(|p| {
                            p.bci == bci
                                && p.reason == DeoptReason::OsrExit
                                && !p.names_a_root_bci()
                        })) =>
            {
                OsrExitSite::PostCallOwnPoint
            }
            site => site,
        };
        if dbg && raw != keyed {
            eprintln!(
                "[cratonvm-deopt] osr-exit site classification frame-keyed-differs: bci={bci} \
                 chain={chain_exit} raw={raw:?} keyed={keyed:?} used={} \
                 (CRATONVM_OSR_EXIT_SITE_FRAME_KEYED)",
                if keyed_on { "keyed" } else { "raw" }
            );
        }
        if keyed_on {
            keyed
        } else {
            raw
        }
    }
}

/// How many resume images the artifact records at one bci.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResumeImage {
    /// No deopt point at this bci: nothing can describe the frame.
    None,
    /// Exactly one, or several that agree on `semantics` — the only field the
    /// resume reads by bci. `index` is the representative's position in
    /// `deopt_points`.
    Unique {
        index: usize,
        /// Several images that agree on `semantics` and differ on `reason`.
        ///
        /// The ordinary shape of a compiled counted loop: the loop-boundary
        /// exit map and the speculative-BCE range guard sit on the same header
        /// bci. Not a refusal — see the module note, and the CratonBench
        /// measurement that decided it — but counted, because "tolerated"
        /// should be visible rather than assumed.
        ///
        /// **`deopt_points[index].reason` is not authoritative and never was.**
        /// `index` names the FIRST image in list order, which is the right
        /// answer for `semantics` (they all agree, that is the predicate) and
        /// an arbitrary one for `reason`. Anything that acts on the reason —
        /// escalation, de-speculation, trap accounting — must ask
        /// [`deopt_reason_at_bci`], which refuses instead of picking.
        reason_ambiguous: bool,
    },
    /// Two or more that disagree on `semantics`, so the resume bci itself is
    /// arbitrary. Carries both indices so the diagnostic can name what differs.
    Ambiguous { first: usize, second: usize },
}

/// Which resume image `bci` names in `deopt_points`.
///
/// The agreement predicate is `semantics` alone; see the module note for why
/// `reason` is observed rather than refused, and why the per-slot state is not
/// this check's business at all.
///
/// **A `RETHROW` point is not a resume image and is skipped.** Its `bci` names
/// a THROWING instruction, and this module already says so: "`RETHROW` points
/// are fine to *have* — they are stashed separately (`take_exceptional_frame`)
/// and never routed to a resume". Counting one as a candidate image made a
/// protected invoke that ALSO carries a speculative-dispatch guard read as two
/// images that disagree on `semantics`, and `first_ambiguous_resume_bci` then
/// refused the whole OSR entry.
///
/// That is not a hypothetical shape, it is the ordinary one after the RBC.6b
/// lift (2026-08-17): a `try { foo(x); } catch (...)` loop puts a
/// `ReceiverTypeChanged` guard (`reexecute`) and a `PendingException` frame
/// (`rethrow`) on the same invoke bci. Measured on
/// `probes/OsrExcTableProbe.java`: `osr_entered=0
/// osr_entry_refused_ambiguous_image=15` — the artifact compiled and NOTHING
/// EVER ENTERED IT, while every correctness arm passed. The lift was a vacuous
/// green until this skip.
///
/// Skipping is not a relaxation of the rule the ambiguity check enforces. The
/// question that check asks is "if this body exits here and the VM has to park
/// the interpreter at `bci`, is the bci well defined?" — and a `RETHROW` point
/// is never an answer to it: `resume_after_exit` rejects non-`REEXECUTE`
/// semantics explicitly, and the exception path finds its own point by
/// `(bci, reason == PendingException)` rather than through this function. Two
/// points that both claim to be resume images and disagree are still refused.
///
/// Under [`chain_points_off_root_bcis_enabled`] a single-pass chain point (whose
/// bci is a spliced callee's) is not a candidate either: the exit frame this
/// answers for has no caller chain (`resume_after_exit` refuses one first).
pub fn resume_image(deopt_points: &[DeoptimizationPoint], bci: u32) -> ResumeImage {
    resume_image_keyed(deopt_points, bci, chain_points_off_root_bcis_enabled())
}

/// [`resume_image`] with the switch's answer in hand (`off_root`).
fn resume_image_keyed(
    deopt_points: &[DeoptimizationPoint],
    bci: u32,
    off_root: bool,
) -> ResumeImage {
    let mut first: Option<(usize, ResumeSemantics, DeoptReason)> = None;
    let mut reason_ambiguous = false;
    for (i, p) in deopt_points.iter().enumerate() {
        if p.bci != bci {
            continue;
        }
        if p.semantics.rethrow_exception || !counts_at_root_bci(p, off_root) {
            continue;
        }
        match first {
            // `resume_kind`: two images that differ only in the replay fact
            // `first_arrival` resume identically (round 12 wave 3).
            None => first = Some((i, p.semantics.resume_kind(), p.reason)),
            Some((j, sem, reason)) => {
                if sem != p.semantics.resume_kind() {
                    return ResumeImage::Ambiguous {
                        first: j,
                        second: i,
                    };
                }
                if reason != p.reason {
                    reason_ambiguous = true;
                }
            }
        }
    }
    match first {
        Some((i, _, _)) => ResumeImage::Unique {
            index: i,
            reason_ambiguous,
        },
        None => ResumeImage::None,
    }
}

/// What a bci lookup can say about the REASON a trap carried.
///
/// Separate from [`ResumeImage`] on purpose. That one answers "is the resume
/// bci well defined", and its verdict is measured: a `reason`-only
/// disagreement is *tolerated* there, because refusing it cost a hot kernel
/// its OSR entirely (see the module note). This one answers "which speculation
/// failed", whose consumers are the escalation policy and the de-spec registry
/// — and there, picking one of two candidates arbitrarily is not a tolerable
/// approximation, it is the wrong verdict applied to the wrong site.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReasonAtBci {
    /// No non-`RETHROW` point at this bci names a reason.
    None,
    /// Exactly one reason is named by the points that could have produced an
    /// ordinary (non-exceptional) stash at this bci.
    Unique(DeoptReason),
    /// Two or more points at this bci name different reasons and both could
    /// have produced the stash. There is no way to tell which fired from the
    /// bci alone.
    Ambiguous,
}

/// The deopt reason a trap at `bci` carried, or an explicit refusal.
///
/// # Why a bci lookup needs its own function
///
/// Four VM-side resume sinks recover the reason with
/// `deopt_points.iter().find(|dp| dp.bci == rframe.bci).map(|dp| dp.reason)`.
/// That is a *first-in-list-order* pick, and this module's own doc note says
/// two points at one bci is the ORDINARY shape after the RBC.6b lift: a
/// `try { foo(x); } catch (...)` loop puts a `ReceiverTypeChanged` guard
/// (`reexecute`) and a `PendingException` frame (`rethrow`) on the same invoke
/// bci. When the guard fires and the `PendingException` point happens to be
/// first, the sink reports `PendingException`, and every downstream policy then
/// reads the wrong answer at once:
///
///  * `recommend_action(PendingException)` is `Reinterpret`, so the failing
///    speculation is never de-spec'd and never escalates;
///  * `deopt_reason_is_charged(PendingException)` is `false`, so no trap is
///    charged and no limit is approached;
///  * the artifact is evicted anyway.
///
/// Evict, recompile with the SAME wrong speculation, trap, evict — forever,
/// with no counter anywhere saying so. Symmetrically, an exceptional frame at a
/// bci that also carries a guard is attributed to the guard and escalates a
/// method that never mis-speculated.
///
/// # The rule
///
/// `RETHROW` points are skipped, exactly as [`resume_image`] skips them and for
/// exactly the same reason: an ordinary stash (`take_last_deopt`) cannot have
/// come from one. Exceptional frames are published to a different stash
/// (`take_exceptional_frame`) by a different arm of the trampoline, so a caller
/// holding an ordinary frame already knows the point was not a rethrow.
///
/// What is left is then required to AGREE. If it does not, this answers
/// [`ReasonAtBci::Ambiguous`] rather than picking: a caller must fall back to a
/// reason that neither escalates the method nor de-specs a site, because it
/// cannot prove which of the two candidates fired.
///
/// # There is no more precise lookup to defer to (corrected 2026-09-22)
///
/// This paragraph used to end by recommending one. It described an offset-keyed
/// lookup (`CompiledMethod::find_deopt_point`) as naming exactly one point, and
/// as the better answer whenever a caller holds an offset. Both halves were
/// wrong, and a reader who acted on them would have replaced a refusal with a
/// silent guess on the escalation and de-speculation paths — the two this
/// function's whole doc note is about.
///
/// Paraphrased rather than quoted, deliberately: the retired sentence is the
/// subject of a textual ratchet in
/// `jit/tests/r10_offsetkey_deopt_point_identity.rs`, and reproducing it here
/// verbatim would make that ratchet fire on the correction itself. It also keeps
/// a reader grepping for the old claim from finding a hit and stopping there.
///
/// **`native_offset` is not unique.**
/// `x64/deopt_stubs.rs::build_and_record_deopt_point` takes it as
/// `self.buf.pos()` and *emits no machine code* ("Emits NO machine code — pure
/// metadata"), and the three recorder families that funnel into it keep their
/// idempotence in three maps that never consult each other
/// (`deopt_box_ptr_by_bci`, `osr_exit_box_ptr_by_bci`,
/// `exc_frame_box_ptr_by_bci`), two of them unguarded. Two metadata-only
/// records at one emitter pc therefore land on one offset, and the one lane
/// that could have forbidden it deliberately does not:
/// `deopt::DeoptMetadataError::DeoptPointsUnsorted` compares with `>` and not
/// `>=` precisely so that the equal pair stays legal.
///
/// **And an offset-keyed lookup would be strictly WORSE here, not better.**
/// Both members of a same-offset pair are stamped `bci: resume_bci_for(pc)`,
/// which is a function of the emitter pc alone (`orig_bci(pc)`, or `pc` inside
/// a splice), so such a pair necessarily AGREES on `bci` and can differ only in
/// `reason` — exactly the field this function refuses to guess.
/// `find_deopt_point` was a `binary_search_by_key` on the offset: handed that
/// pair it returned whichever of the two elements the search happened to land
/// on, an arbitrary one of the two reasons, and it had no `Ambiguous` to
/// return. It also had no production caller at all — the only calls in the tree
/// were two asserts in `ir_lower.rs`'s own tests — so it never had to be right
/// about this, and round 10 wave 9 deleted it rather than leave a search that a
/// reader could mistake for the precise lookup this paragraph declines to
/// recommend.
///
/// **The identity a reconstructing path uses is neither.** A frame-deopt stub
/// bakes the ADDRESS of its own `deopt_points` element as an imm64
/// (`x64/deopt_stubs.rs::resolve_baked_point`, which cross-checks the box's
/// scalar fields against the element's and fails the compile closed if they
/// disagree), and `deopt::x64_deopt_entry` receives that pointer in arg0. Such
/// a path therefore resolves its point by neither offset nor bci. Those maps
/// are keyed by EMITTER pc, so each unrolled copy of one bytecode gets its own
/// entry and its own baked pointer.
///
/// This function exists for the sinks that hold neither the address nor
/// anything but a bci — the VM-side reason lookups named above — and for them
/// [`ReasonAtBci::Ambiguous`] IS the most precise answer available. All three
/// call sites in the tree today handle it conservatively, which is what makes
/// the refusal usable rather than merely correct: `vm/src/jit/helpers.rs`'s
/// `deopt_reason_from_artifact` and `deopt_resume.rs`'s `trap_reason_for_frame`
/// both fall back to `UncommonTrap` — deliberately, because it IS charged, so
/// an attribution nobody could make still makes the trap rate visible without
/// blacklisting the method on the first occurrence the way `UnreachedCode`
/// would — and the per-bci de-spec insert falls back to
/// `deopt::SPECULATION_ID_ANY` rather than suppressing a speculation it cannot
/// identify.
///
/// All of the above is from reading the producer and the callers; **no compile
/// was observed emitting a duplicate offset.** The weaker claim is the one that
/// matters and is the one asserted: nothing in the tree establishes uniqueness,
/// so nothing may assume it. See
/// `docs/internal/fixed-bugs/r10-interner-native-offset-is-not-a-unique-deopt-point-key-RESOLVED-20260922.md`.
///
/// # An `OsrExit` map beside a guard (interpreter round i1 wave 49)
///
/// Under [`OSR_EXIT_MAPS_BESIDE_GUARDS`] an `OsrExit` point is skipped when a
/// non-rethrow point of ANOTHER reason is at the same bci: the answer is then
/// the guard's (or `Ambiguous` among several guards). An `OsrExit` map is a
/// mode exit, not a failed speculation, and its stub stashes its own
/// [`crate::deopt::DeoptCause`], which every production sink reads first
/// (`deopt_resume::trap_cause_at_frame`, and the callee service's frame-less
/// arm `helpers::deopt_reason_from_artifact` since wave 49); a by-bci answer
/// is asked only for a stash WITHOUT a cause, which a mode-exit stub never
/// produces. `trap_reason_at_frame`, the other cause-less lookup, has skipped
/// `OsrExit` maps since round 11 wave 2 for the same reason (the loop-header
/// map beside the speculative-BCE guard). [`classify_exit_site`] keeps the
/// raw lookup: it names the exit site, where the `OsrExit` map is the answer.
pub fn deopt_reason_at_bci(deopt_points: &[DeoptimizationPoint], bci: u32) -> ReasonAtBci {
    trap_reason_at_bci(deopt_points, bci, None)
}

/// The ONE cause-less trap-reason lookup (interpreter round i1 wave 50, lane
/// L1; `interpreter-L1-proposal-one-cause-less-trap-reason-lookup-FIXED`): the reason a
/// trap at `bci` carried, re-derived for a stash that holds no
/// [`crate::deopt::DeoptCause`]. [`deopt_reason_at_bci`] (the frame-less
/// sinks) is this with no frame; the VM's `deopt_resume::trap_reason_at_frame`
/// is this with the trapped frame. The two kept separate copies of the rule
/// until wave 50, which agreed only by convention.
///
/// 1. The non-`RETHROW` points at `bci` count. An `OsrExit` map is skipped
///    when a point of another reason is beside it (under
///    [`OSR_EXIT_MAPS_BESIDE_GUARDS`]): such a map is a mode exit, whose stub
///    stashes its own cause, not a failed speculation -- except under the
///    OSR-exit test triggers (`CRATONVM_OSR_EXIT_TEST`, `osr_exit_after`), where
///    an `OsrExit` map IS a trap source and counts like any point. (The
///    frame-less copy ignored the triggers; it now answers `Ambiguous` there
///    for a map beside a guard, as the frame copy always did.)
/// 2. When the points left disagree and are exactly the optimizing tier's
///    `NullCheck` + `BoundsCheck` pair at an array access (`ir_lower`'s
///    unelided `xaload` / `xastore`), `frame` -- the method's bytecode and
///    the re-executed frame's operand stack -- says which fired: the null
///    check precedes the bounds check, so a null array reference is
///    `NullCheck` and any other `BoundsCheck`. Without a frame (or for any
///    other disagreement) the answer is [`ReasonAtBci::Ambiguous`].
/// 3. A bci whose only point is an `OsrExit` map answers that.
///
/// Under [`chain_points_off_root_bcis_enabled`] (JIT round 19 wave 10, lane
/// deopt10) a single-pass chain point is not a candidate: `bci` is a ROOT bci
/// (the frame sinks ask for a single-scope frame, the frame-less arm for the
/// outermost scope's bci) and a chain point records its spliced callee's. Off,
/// a root trap at bci X and a chain guard of another reason at callee bci X
/// answer `Ambiguous`. Either way, under `CRATONVM_DBG_DEOPT` a lookup whose
/// bci carries a chain point prints one census line (`chain-candidates=`).
pub fn trap_reason_at_bci(
    deopt_points: &[DeoptimizationPoint],
    bci: u32,
    frame: Option<(&[u8], &[crate::deopt::FrameValue])>,
) -> ReasonAtBci {
    let off_root = chain_points_off_root_bcis_enabled();
    let answer = trap_reason_at_bci_keyed(deopt_points, bci, frame, off_root);
    let chain_candidates = deopt_points
        .iter()
        .filter(|p| p.bci == bci && !p.semantics.rethrow_exception && !p.names_a_root_bci())
        .count();
    if chain_candidates > 0 && cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_DEOPT") {
        eprintln!(
            "[cratonvm-deopt] cause-less reason lookup at root bci={bci}: \
             chain-candidates={chain_candidates} skipped={off_root} answer={answer:?} \
             (CRATONVM_DEOPT_CHAIN_POINTS_OFF_ROOT_BCIS)"
        );
    }
    answer
}

/// [`trap_reason_at_bci`] with the switch's answer in hand (`off_root`).
fn trap_reason_at_bci_keyed(
    deopt_points: &[DeoptimizationPoint],
    bci: u32,
    frame: Option<(&[u8], &[crate::deopt::FrameValue])>,
    off_root: bool,
) -> ReasonAtBci {
    let osr_exit_is_a_trap_source =
        crate::osr_exit_test_enabled() || crate::osr_exit_after().is_some();
    let skip_osr_exit = !osr_exit_is_a_trap_source
        && OSR_EXIT_MAPS_BESIDE_GUARDS
        && deopt_points.iter().any(|p| {
            p.bci == bci
                && !p.semantics.rethrow_exception
                && p.reason != DeoptReason::OsrExit
                && counts_at_root_bci(p, off_root)
        });
    let mut found: Option<DeoptReason> = None;
    let mut ambiguous = false;
    let mut only_array_guards = true;
    for p in deopt_points.iter() {
        if p.bci != bci
            || p.semantics.rethrow_exception
            || (skip_osr_exit && p.reason == DeoptReason::OsrExit)
            || !counts_at_root_bci(p, off_root)
        {
            continue;
        }
        if !matches!(p.reason, DeoptReason::NullCheck | DeoptReason::BoundsCheck) {
            only_array_guards = false;
        }
        match found {
            None => found = Some(p.reason),
            Some(r) if r == p.reason => {}
            Some(_) => ambiguous = true,
        }
    }
    if ambiguous {
        return match frame {
            Some((code, stack)) if only_array_guards => array_guard_that_fired(code, stack, bci)
                .map_or(ReasonAtBci::Ambiguous, ReasonAtBci::Unique),
            _ => ReasonAtBci::Ambiguous,
        };
    }
    match found {
        Some(r) => ReasonAtBci::Unique(r),
        None => ReasonAtBci::None,
    }
}

/// Rule 2 of [`trap_reason_at_bci`]: which of an array access's `NullCheck`
/// and `BoundsCheck` fired, read off the re-executed frame's operand stack.
fn array_guard_that_fired(
    code: &[u8],
    stack: &[crate::deopt::FrameValue],
    bci: u32,
) -> Option<DeoptReason> {
    use crate::deopt::FrameValue;
    let bci = usize::try_from(bci).ok()?;
    // Operand-stack depth of the array reference below the top: the
    // interpreter stack is compact, so a cat-2 stored value is one entry.
    let below_top = match *code.get(bci)? {
        0x2e..=0x35 => 2, // xaload: arrayref, index
        0x4f..=0x56 => 3, // xastore: arrayref, index, value
        _ => return None,
    };
    let at = stack.len().checked_sub(below_top)?;
    match stack.get(at)? {
        FrameValue::Object(0) => Some(DeoptReason::NullCheck),
        FrameValue::Object(_) | FrameValue::VirtualObject(_) | FrameValue::VirtualObjectRef(_) => {
            Some(DeoptReason::BoundsCheck)
        }
        _ => None,
    }
}

/// [`deopt_reason_at_bci`] without the wave-49 `OsrExit` skip: every
/// non-rethrow point at `bci` counts. [`classify_exit_site`]'s lookup, which
/// names an exit's SITE (an `OsrExit` map beside a guard is its answer), not
/// the speculation a trap failed.
///
/// Only the points `keep` admits count ([`classify_exit_site_keyed`]'s
/// frame-kind filter; everything for the raw scan).
fn raw_reason_at_bci_where(
    deopt_points: &[DeoptimizationPoint],
    bci: u32,
    keep: &dyn Fn(&DeoptimizationPoint) -> bool,
) -> ReasonAtBci {
    let mut found: Option<DeoptReason> = None;
    for p in deopt_points.iter() {
        if p.bci != bci || p.semantics.rethrow_exception || !keep(p) {
            continue;
        }
        match found {
            None => found = Some(p.reason),
            Some(r) if r == p.reason => {}
            Some(_) => return ReasonAtBci::Ambiguous,
        }
    }
    match found {
        Some(r) => ReasonAtBci::Unique(r),
        None => ReasonAtBci::None,
    }
}

/// The reason an EXCEPTIONAL (`RETHROW`) stash at `bci` carried.
///
/// The mirror of [`deopt_reason_at_bci`] for the other stash: a caller holding
/// a frame taken from `take_exceptional_frame` knows the point WAS a rethrow,
/// so the non-rethrow points at that bci are the ones that must be skipped.
///
/// # Its refusal arm is unreachable today, and that is a fact about `for_reason`
///
/// **It cannot answer [`ReasonAtBci::Ambiguous`] on any artifact this tree
/// produces.** `semantics.rethrow_exception` is set only by
/// `ResumeSemantics::for_reason`, and `for_reason` answers `RETHROW` for
/// `DeoptReason::PendingException` and `REEXECUTE` for every other reason. Every
/// production construction site of a `DeoptimizationPoint` sets `semantics`
/// either through `for_reason(reason)` or to an explicit `REEXECUTE` (read
/// 2026-09-22 across `ir_lower.rs`, `x64/deopt_stubs.rs`, `x64/inlining.rs` and
/// `x64/loop_unroll_admission.rs`; `ir_lower.rs` carries a source-scanning test
/// that counts those initialisers, so a new site cannot appear silently). So the
/// points this function iterates all carry one reason, the `Some(_)` arm below
/// is dead, and the answer is `Unique(PendingException)` or `None`.
///
/// This is recorded rather than left for the next reader to rediscover (round
/// 10, lane `offsetkey`): an unreachable refusal reads as a guard against a
/// hazard that is real and handled, and here it is evidence about nothing.
/// [`deopt_reason_at_bci`]'s ambiguity, by contrast, IS reachable and IS the
/// ordinary shape of a protected invoke after the RBC.6b lift — do not transfer
/// that argument onto this function. The invariant is pinned by
/// `tests::the_exceptional_lookup_has_no_reachable_ambiguity`, which fails the
/// moment `for_reason` gains a second `RETHROW` reason.
///
/// **Its production caller** is the OSR exception-exit transfer in
/// `vm/src/runtime/interpreter/deopt_resume.rs`, which asks it the provenance
/// question "did this artifact record a rethrow point at this bci?" and refuses
/// the transfer on [`ReasonAtBci::None`]. That sink ACCEPTS `Ambiguous`: two
/// rethrow points at one bci both answer yes to the question it is asking, which
/// is why the unreachable arm above costs nothing there even if it becomes
/// reachable. Until 2026-09-22 the sink asked the same question by an inline
/// `any(..)` and this function had no caller at all; see
/// `docs/internal/retired/r10-offsetkey-exceptional-reason-at-bci-is-unwired-and-cannot-refuse-20260921-RETIRED-20260922.md`.
pub fn exceptional_reason_at_bci(deopt_points: &[DeoptimizationPoint], bci: u32) -> ReasonAtBci {
    let mut found: Option<DeoptReason> = None;
    for p in deopt_points.iter() {
        if p.bci != bci || !p.semantics.rethrow_exception {
            continue;
        }
        match found {
            None => found = Some(p.reason),
            Some(r) if r == p.reason => {}
            Some(_) => return ReasonAtBci::Ambiguous,
        }
    }
    match found {
        Some(r) => ReasonAtBci::Unique(r),
        None => ReasonAtBci::None,
    }
}

/// The first bci of this artifact whose images disagree on `semantics`, if any
/// — the admission-time form of the lane's "what to refuse".
///
/// Walks the whole point list rather than only the OSR-exit maps: the entry is
/// being admitted for a body that can leave through **any** of its deopt
/// points, and an ambiguous one that is not a loop boundary is just as
/// arbitrary a pick.
///
/// Returns `(bci, first_index, second_index)`.
pub fn first_ambiguous_resume_bci(
    deopt_points: &[DeoptimizationPoint],
) -> Option<(u32, usize, usize)> {
    first_ambiguous_resume_bci_keyed(deopt_points, chain_points_off_root_bcis_enabled())
}

/// [`first_ambiguous_resume_bci`] with the switch's answer in hand.
fn first_ambiguous_resume_bci_keyed(
    deopt_points: &[DeoptimizationPoint],
    off_root: bool,
) -> Option<(u32, usize, usize)> {
    for (bci, image) in distinct_bci_images(deopt_points, off_root) {
        if let ResumeImage::Ambiguous { first, second } = image {
            return Some((bci, first, second));
        }
    }
    None
}

/// Does any bci of this artifact carry images that agree on `semantics` and
/// differ on `reason`? The observed-not-refused case; the caller counts it.
pub fn has_reason_ambiguous_bci(deopt_points: &[DeoptimizationPoint]) -> bool {
    has_reason_ambiguous_bci_keyed(deopt_points, chain_points_off_root_bcis_enabled())
}

/// [`has_reason_ambiguous_bci`] with the switch's answer in hand.
fn has_reason_ambiguous_bci_keyed(deopt_points: &[DeoptimizationPoint], off_root: bool) -> bool {
    distinct_bci_images(deopt_points, off_root).any(|(_, image)| {
        matches!(
            image,
            ResumeImage::Unique {
                reason_ambiguous: true,
                ..
            }
        )
    })
}

/// `(bci, image)` for each DISTINCT bci in `deopt_points`, in first-seen order.
///
/// Quadratic in the number of distinct bcis, because `resume_image` rescans;
/// artifacts carry tens of points and this runs once per OSR admission, not per
/// iteration. A map would be faster and would allocate on a path that today
/// does not.
///
/// Under `off_root` ([`chain_points_off_root_bcis_enabled`]) a chain point
/// neither contributes a bci nor counts at one: its bci is a spliced callee's.
fn distinct_bci_images(
    deopt_points: &[DeoptimizationPoint],
    off_root: bool,
) -> impl Iterator<Item = (u32, ResumeImage)> + '_ {
    let mut seen: Vec<u32> = Vec::new();
    deopt_points.iter().filter_map(move |p| {
        if seen.contains(&p.bci) || !counts_at_root_bci(p, off_root) {
            return None;
        }
        seen.push(p.bci);
        Some((p.bci, resume_image_keyed(deopt_points, p.bci, off_root)))
    })
}

// ── Tier proposal 27, phase 3: guard exits of an optimizing OSR body ──────
//
// Round 11 wave 12, lane irexc
// (`r11w9-tier-optimizing-osr-exception-exits-door-contract-patch`). An
// optimizing OSR entry has no `OsrEntryPlan`, so `resume_after_exit` cannot
// be its resume authority. What stands in for it is the guard's OWN boxed
// point, found by the address the deopt stub handed `ir_deopt_entry`, and the
// admission fact the lowerer published before anything ran.
//
// The resume itself is the easy half. The hard half is that the interpreter
// then RUNS from the guard's bci and may reach the loop header and re-enter the
// same body through its OSR stub, which seeds the interpreter's locals and
// JUMPS into code optimised over the graph's own paths only. A guard's
// failure continuation is not one of those paths, so a body may be entered
// that way only when no continuation can falsify a fact the body assumes:
//
// * EXACT guards (null, bounds, zero divisor at the guarded bytecode's own
//   bci): the re-executed bytecode throws. Outside every protected range —
//   which the door requires of every bci listed — that ends the frame, so the
//   interpreter never reaches a header along a path the graph lacks.
// * VALUE-EQUIVALENT guards (the class and layout guards of an `Integer` /
//   `Long` unbox): the interpreter's `intValue()` / `longValue()` returns the
//   value the body would have loaded, and every later state is one the graph
//   also reaches.
// * Anything else is SPECULATIVE — an `Op::Guard` other than the two exact
//   shapes `Lowerer::op_guard_is_exact` recognises (a pruned branch, a range
//   predicate the entry stub would skip, a devirtualisation, an unresolved-
//   class trap), an unconditional deopt, a non-final receiver guard — and its
//   continuation runs code the graph pruned. One of them anywhere refuses the
//   body (`guard_exit_fact` answers `None`), because a header fact derived
//   from the speculation would be assumed at the next entry. Making the
//   entry a real predecessor
//   (`r11w11-irexc-osr-entry-as-a-real-predecessor-CLOSED-20260929.md`) is what
//   admits those: since round 11 wave 14, under `CRATONVM_JIT_OSR_OPAQUE_ENTRY`,
//   a body whose every entry targets an opaque merge publishes
//   `guard_exit_fact_opaque` instead.

/// Is a guard of this reason EXACT: does its failure mean the bytecode at its
/// (resume) bci throws when the interpreter re-executes it?
///
/// Only the lowerer's own null, bounds and zero-divisor checks carry these
/// reasons (`ir_lower::Lowerer::emit_deopt_unless` callers), each at the bci
/// of the bytecode whose JVMS check it is. `Op::Guard` lowers to
/// `UncommonTrap` whatever it guards, so its reason never says "exact"; since
/// round 11 wave 13 the lowerer recognises the two builder guards that are
/// exact by SHAPE instead (`Lowerer::op_guard_is_exact`: the zero-divisor
/// guard of an integer division and the receiver null guard before a
/// sync-call `MonitorEnter`) and classifies those. Every other `Op::Guard`
/// stays speculative — conservative, never unsound.
pub(crate) fn guard_reason_is_exact(reason: DeoptReason) -> bool {
    matches!(
        reason,
        DeoptReason::NullCheck | DeoptReason::BoundsCheck | DeoptReason::DivByZero
    )
}

/// May the optimizing OSR door resume a guard exit at `p` in place with no
/// plan? A single-scope, monitor-free `REEXECUTE` frame whose bci is the
/// point's own, with no slot the in-place transfer refuses
/// (`deopt_resume::transfer_osr_guard_exit_into_live_frame`): nothing
/// `frame_state_is_resumable` rejects, and no scalar-replaced object that
/// transfer cannot re-allocate (`deopt::osr_in_place_virtuals_refusal`; before
/// the for-each residual round it re-allocated none). Checked at COMPILE time, where a refusal
/// costs the entry and not an iteration. Since round 13 wave 8 (lane chain3)
/// also a monitor-free inlined caller chain ([`chain_resumable_in_place`]).
pub(crate) fn guard_exit_point_resumable(p: &DeoptimizationPoint) -> bool {
    p.frame_state.monitors.is_empty() && resumable_in_place_but_for_monitors(p)
}

/// Kill switch for an optimizing OSR body's back-edge POLL exit at a loop
/// header inside a `synchronized` block (interpreter round i1 wave 25, lane
/// L2; `i9-L5-jvmti-frames-already-compiled-finish-compiled`, optimizing item
/// (b)): `false` restores the wave-24 rule, under which such a header keeps a
/// verdict-blind poll. Read by the lowerer (`back_edge_mode_exit_state`) and
/// by [`poll_exit_point_resumable`], so the two cannot disagree.
pub(crate) const OSR_POLL_EXITS_HOLDING_A_LOCK_ENABLED: bool = true;

/// Kill switch for an optimizing OSR body's back-edge POLL exit whose header
/// holds an ELIDED lock level (`relock`: the re-take half of a nested-lock
/// elision's split, or a lock escape analysis removed on an allocation it
/// kept) on an object in a frame word — interpreter round i1 wave 26, lane L2
/// (`i9-L5-jvmti-frames-already-compiled-finish-compiled`, optimizing item
/// (b)). The VM's in-place transfer
/// (`deopt_resume::transfer_osr_guard_exit_into_live_frame`) re-takes such a
/// level on the live frame's object after every fallible step, as
/// `build_deopt_frame_inner` does for a method-entry exit. `false` restores
/// the wave-25 rule, under which such a header keeps a verdict-blind poll.
/// Read by the lowerer (`back_edge_mode_exit_state`) and by
/// [`poll_exit_point_resumable`], so the two cannot disagree.
pub(crate) const OSR_POLL_EXITS_WITH_AN_ELIDED_LOCK_ENABLED: bool = true;

/// Switch for `OsrExit` maps that share a bci with a `REEXECUTE` guard of the
/// same frame (interpreter round i1 wave 49, lane L2;
/// `docs/internal/fixed-bugs/interpreter-L2-proposal-a-trap-is-charged-by-the-point-its-stub-baked-FIXED-20261013.md`).
/// A Rust const kill switch, as the other switches of this area are (the
/// backend reads no per-VM config).
///
/// `true`:
///
/// * [`deopt_reason_at_bci`] skips an `OsrExit` point when a non-rethrow
///   point of another reason is at the same bci (the guard is charged, never
///   `Ambiguous` because of a mode exit's map);
/// * a post-call exit claim whose successor filed only `REEXECUTE` guards of
///   the claim's own frame at its bci files its map there
///   (`x64/op_invoke.rs::Compiler::post_call_exit_claim_outcome`, outcome
///   `Own`), where it was `Dropped`;
/// * a conditional back edge whose bci already carries such guards keeps its
///   exit map (`x64/safepoint.rs::Compiler::branch_mode_exit_admits`).
///
/// `false` restores wave 48: one non-rethrow reason per exit bci.
pub const OSR_EXIT_MAPS_BESIDE_GUARDS: bool = true;

/// [`guard_exit_point_resumable`] for a back-edge POLL exit (`OsrExit`) of an
/// optimizing OSR body, which may also name locks the frame holds
/// (interpreter round i1 wave 25, lane L2). Each must be one this thread
/// already holds for the frame — `relock == false`: taken by the compiled
/// code, or by the interpreter before the entry, which the entry state lists
/// the same way — on an object in a reference frame word, `lock_depth > 0`.
/// The in-place transfer then leaves every such hold with the live frame
/// (`deopt_resume::transfer_osr_guard_exit_into_live_frame`), whose record
/// the door rewrites from them, and whose own `monitorexit`s release them. An
/// ELIDED level (`relock`) has to be taken again: since wave 26 the transfer
/// does ([`OSR_POLL_EXITS_WITH_AN_ELIDED_LOCK_ENABLED`]); with that switch off
/// it still refuses.
///
/// Not a relaxation of [`guard_exit_point_resumable`] itself: that one also
/// decides the GUARD-exit fact ([`guard_exit_fact`]), i.e. whether a body's
/// guard exits admit its OSR entry, which this does not widen. A poll exit is
/// admitted by the point it carries alone ([`guard_exit_resume_bci`]).
pub(crate) fn poll_exit_point_resumable(p: &DeoptimizationPoint) -> bool {
    use crate::deopt::FrameValue as FV;
    if p.reason != DeoptReason::OsrExit || p.semantics.rethrow_exception {
        return guard_exit_point_resumable(p);
    }
    // Since wave 26 (lane L2) an elided level too, which the transfer re-takes
    // ([`OSR_POLL_EXITS_WITH_AN_ELIDED_LOCK_ENABLED`]).
    let locks_ok = p.frame_state.monitors.is_empty()
        || (OSR_POLL_EXITS_HOLDING_A_LOCK_ENABLED
            && p.frame_state.monitors.iter().all(|m| {
                (!m.relock || OSR_POLL_EXITS_WITH_AN_ELIDED_LOCK_ENABLED)
                    && m.lock_depth > 0
                    && matches!(m.object, FV::StackSlotRef(_))
            }));
    locks_ok && resumable_in_place_but_for_monitors(p)
}

/// `CRATONVM_JIT_OSR_GUARD_EXITS_HOLDING_A_LOCK=1` (opt-in; JIT round 16 wave
/// 13, lane monitor3; page
/// `r12w8-monitor-where-a-contended-enter-spends-its-time-CLOSED-20261006.md`, "Round
/// 16 wave 13"): may an optimizing OSR body's EXACT or VALUE-EQUIVALENT guard
/// exit name the locks its frame holds, as a back-edge poll exit already may
/// ([`poll_exit_point_resumable`])?
///
/// Off (the default), a guard inside a `synchronized (o) { .. }` block -- the
/// null check of a `putfield` on a static's object, say -- makes
/// [`guard_exit_fact`] answer `None`, and the OSR door then runs the loop in
/// the SINGLE-PASS OSR body ("the body has exits and the door admits none of
/// them"): `R16MonitorCrowd`'s `shortCs` loop, the crowd rows' measured code,
/// never reached the optimizing tier, so nothing the IR does to monitors
/// (lock coarsening included) could reach those rows.
///
/// On, such a guard is admitted when every lock its frame names is one the
/// thread already holds for this activation (`relock == false`,
/// `lock_depth > 0`) on an object in a reference frame word -- the poll exit's
/// rule without its elided levels -- and the frame is flat (no inlined
/// chain). The VM side is the poll exit's, unchanged: the planless transfer
/// (`deopt_resume::transfer_osr_guard_exit_into_live_frame`) leaves each hold
/// with the live frame and the door replaces the frame's lock record with the
/// exit frame's (`CompiledLocksOfAStash::replace_live_record_with_retaken_levels_and_unpin`),
/// so javac's catch-all handler, run by the interpreter after the re-executed
/// bytecode throws, releases exactly what the compiled code took. The door
/// half (`jit_bridge::osr_optimizing_guard_exits_admissible`) admits such a
/// guard inside a protected range only when no OSR entry of the body lies on
/// a pc an exception handler of the method reaches. Read without a `static`
/// (`process_global_statics_ratchet`): at most once per compile and once per
/// guard exit taken.
///
/// JIT round 17 wave 2 (lane monitor3;
/// `r17-monitor-forked-block-coarsening-is-not-the-body-that-runs-FIXED-20261005.md`):
/// also ON whenever a loop block coarsening switch is
/// (`CRATONVM_JIT_IR_LOOP_LOCK_COARSEN_BLOCKS` /
/// `CRATONVM_JIT_IR_LOOP_LOCK_COARSEN_INSTANCE_BLOCKS`,
/// [`loop_block_coarsening_admits_lock_holding_guard_exits`]). Every exit of a
/// coarsened block that can leave holds the block's monitor (the coarsened
/// hold `o0`, exactly once, `relock == false`: `ir_optimize`'s per-copy frames),
/// so without this rule the OSR door can never enter the body the switch
/// built, and the coarsening reaches no loop that runs as an OSR body
/// (`R16MonitorCrowd.shortCs`). With both block switches off this answers
/// exactly as before. Every reader -- the compile-time fact, the opaque fact,
/// the planless resume's re-check and the VM door -- asks this one function,
/// so they cannot disagree.
///
/// JIT round 17 wave 2 (lane irfeed, MON3R17-2): this is the SWITCH-WIDE
/// answer, kept for the VM door (`jit_bridge::osr_optimizing_guard_exits_admissible`),
/// which has no compile to ask. The compile-time facts ask
/// [`osr_guard_exits_holding_a_lock_for_this_compile`] and the planless resume
/// [`osr_guard_exits_holding_a_lock_admitted_for`]: under
/// `CRATONVM_JIT_IR_LOOP_LOCK_COARSEN_GUARD_EXITS_PER_ARTIFACT` (default on)
/// a block switch widens the rule only for a body whose own compile coarsened
/// a block.
#[cfg(test)]
pub fn osr_guard_exits_holding_a_lock_enabled() -> bool {
    osr_guard_exits_holding_a_lock_switch()
        || loop_block_coarsening_admits_lock_holding_guard_exits()
}

/// `CRATONVM_JIT_OSR_GUARD_EXITS_HOLDING_A_LOCK` alone (JIT round 16 wave 13):
/// the rule for every body, coarsened or not.
fn osr_guard_exits_holding_a_lock_switch() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_OSR_GUARD_EXITS_HOLDING_A_LOCK")
}

/// `CRATONVM_JIT_IR_LOOP_LOCK_COARSEN_GUARD_EXITS_PER_ARTIFACT`, **default
/// ON** (`=0` restores round 17 wave 2's switch-wide rule; JIT round 17 wave 2,
/// lane irfeed, MON3R17-2 of `jit-r17-monitor3-proposals-RETIRED-20261006.md`): a loop block
/// coarsening switch admits lock-holding guard exits only in a body whose
/// compile coarsened a block, never in every OSR body. Live by default since
/// the round-17 close flipped both block switches ON (it was inert while they
/// were opt-in); it is what lets the block switches be on without also
/// flipping `CRATONVM_JIT_OSR_GUARD_EXITS_HOLDING_A_LOCK` (kept OFF in the r16
/// triage, "no gain") for every uncoarsened body. Read live (no `static`).
fn guard_exits_per_artifact_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on(
        "CRATONVM_JIT_IR_LOOP_LOCK_COARSEN_GUARD_EXITS_PER_ARTIFACT",
    )
}

/// The lock-holding guard rule for the compile running on this thread (the
/// lowerer's [`guard_exit_fact`] / [`guard_exit_fact_opaque`]): the
/// switch-wide rule, narrowed by MON3R17-2 to a compile that coarsened a loop
/// block (`ir_evidence::armed_compile_coarsened_a_loop_block`) when only a
/// block switch turns it on. A compile with nothing armed (a unit test) has
/// coarsened nothing.
pub(crate) fn osr_guard_exits_holding_a_lock_for_this_compile() -> bool {
    osr_guard_exits_holding_a_lock_switch()
        || (loop_block_coarsening_admits_lock_holding_guard_exits()
            && (!guard_exits_per_artifact_enabled()
                || crate::ir_evidence::armed_compile_coarsened_a_loop_block()))
}

/// The lock-holding guard rule for one finished artifact, from its deopt
/// boxes (`CompiledMethod::_deopt_point_boxes`): the switch-wide rule,
/// narrowed by MON3R17-2 to an artifact one of whose GUARD boxes holds a lock
/// ([`guard_box_holds_a_lock`]). That is the artifact-side reading of
/// [`osr_guard_exits_holding_a_lock_for_this_compile`]: a body whose
/// guard-exit fact is `Some` and has such a box was admitted under the
/// widened rule, which under a block switch alone only a coarsened compile
/// gets; a body without one passes the strict rule anyway. For the planless
/// resume ([`guard_exit_resume_bci`]) and, proposed, the VM door.
pub fn osr_guard_exits_holding_a_lock_admitted_for(boxes: &[Box<DeoptimizationPoint>]) -> bool {
    osr_guard_exits_holding_a_lock_switch()
        || (loop_block_coarsening_admits_lock_holding_guard_exits()
            && (!guard_exits_per_artifact_enabled()
                || boxes.iter().any(|p| guard_box_holds_a_lock(p))))
}

/// A GUARD box (not a reason-9 pad, not a back-edge poll's `OsrExit`) whose
/// own frame names a lock.
fn guard_box_holds_a_lock(p: &DeoptimizationPoint) -> bool {
    !p.semantics.rethrow_exception
        && p.reason != DeoptReason::OsrExit
        && !p.frame_state.monitors.is_empty()
}

/// Is a loop block coarsening switch on (JIT round 17 wave 2, lane monitor3)?
/// Read live, like the switches themselves (`ir_optimize`'s
/// `ir_loop_lock_coarsen_blocks_enabled` and its instance twin).
fn loop_block_coarsening_admits_lock_holding_guard_exits() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_LOOP_LOCK_COARSEN_BLOCKS")
        || cratonvm_types::flags::runtime_flag_default_on(
            "CRATONVM_JIT_IR_LOOP_LOCK_COARSEN_INSTANCE_BLOCKS",
        )
}

/// [`guard_exit_point_resumable`] widened to a GUARD frame that holds locks
/// ([`osr_guard_exits_holding_a_lock_enabled`]): every lock `relock == false`
/// with `lock_depth > 0` on a reference frame word, a flat frame, and every
/// other condition of the strict predicate. A poll exit and a reason-9 pad are
/// asked the strict question (a poll exit is admitted by
/// [`poll_exit_point_resumable`], a pad is not a guard).
pub(crate) fn guard_exit_point_resumable_holding_locks(p: &DeoptimizationPoint) -> bool {
    use crate::deopt::FrameValue as FV;
    if p.reason == DeoptReason::OsrExit || p.semantics.rethrow_exception {
        return guard_exit_point_resumable(p);
    }
    p.frame_state.caller.is_none()
        && p.frame_state
            .monitors
            .iter()
            .all(|m| !m.relock && m.lock_depth > 0 && matches!(m.object, FV::StackSlotRef(_)))
        && resumable_in_place_but_for_monitors(p)
}

/// The guard-exit admission predicate under `admit_held_locks`
/// ([`osr_guard_exits_holding_a_lock_enabled`]): the strict
/// [`guard_exit_point_resumable`], or its lock-holding widening.
fn guard_exit_point_admitted(p: &DeoptimizationPoint, admit_held_locks: bool) -> bool {
    guard_exit_point_resumable(p)
        || (admit_held_locks && guard_exit_point_resumable_holding_locks(p))
}

/// Every condition of [`guard_exit_point_resumable`] except the monitor one.
///
/// An inlined caller CHAIN (a guard inside a spliced body that publishes the
/// callee's frame over its parked callers, `ir_lower::splice_chain_for_site`)
/// is admitted by [`chain_resumable_in_place`] instead (round 13 wave 8, lane
/// chain3): the VM's planless transfer writes its OUTERMOST scope into the
/// live frame and pushes the inner ones
/// (`deopt_resume::transfer_osr_guard_exit_into_live_frame`).
fn resumable_in_place_but_for_monitors(p: &DeoptimizationPoint) -> bool {
    use crate::deopt::FrameValue as FV;
    let fs = &p.frame_state;
    if p.semantics.resume_kind() != ResumeSemantics::REEXECUTE || fs.bci != p.bci {
        return false;
    }
    if fs.caller.is_some() {
        return chain_resumable_in_place(fs);
    }
    crate::deopt::frame_state_is_resumable(fs)
        && if crate::deopt::osr_guard_exit_virtuals_enabled() {
            // The for-each residual round (2026-10-06): the transfer re-allocates
            // them first, monitor-free frames only.
            crate::deopt::osr_in_place_virtuals_refusal(fs).is_none()
        } else {
            !fs.locals
                .iter()
                .chain(fs.stack.iter())
                .any(|v| matches!(v, FV::VirtualObject(_) | FV::VirtualObjectRef(_)))
        }
}

/// `CRATONVM_JIT_OSR_CHAIN_GUARD_EXITS`, **default ON** (round 13 wave 8,
/// lane chain3; `r13w6-chain2-osr-guard-exit-chain-lock-record-patch` parts
/// (a) and (b)): may an optimizing OSR body's guard exit whose frame is an
/// inlined caller chain be resumed in place ([`chain_resumable_in_place`])?
/// `=0` restores the round-11 rule, under which one chain guard box makes
/// [`guard_exit_fact`] answer `None` and the OSR door runs the single-pass
/// body instead. Chain boxes exist only under
/// `CRATONVM_JIT_IR_SPLICE_FRAME_STATES`, so the switch is inert without it.
/// Not memoised (`process_global_statics_ratchet`): asked per chain box at
/// compile time and once per chain exit at the door.
pub(crate) fn osr_chain_guard_exits_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_OSR_CHAIN_GUARD_EXITS")
}

/// May the planless OSR guard exit resume the inlined chain `fs` in place?
/// Round 13 wave 8, lane chain3.
///
/// The VM writes the OUTERMOST scope (the OSR'd method, parked after its
/// `invoke`) into the live frame and PUSHES every scope beneath it
/// (`deopt_resume::transfer_osr_exit_chain_into_live_frame`), so:
///
/// * the chain must be one the chain sinks rebuild at all
///   ([`crate::deopt::inlined_chain_refusal`]: depth, register homes, no
///   elided lock);
/// * no scope may carry a value the in-place transfer refuses
///   ([`crate::deopt::first_unresumable_slot`], which also names every
///   scalar-replaced object in a chain; since the for-each residual round such
///   an object is admitted when the transfer re-allocates it,
///   [`crate::deopt::osr_in_place_virtuals_refusal`]);
/// * no scope may hold a monitor. The door rewrites the live frame's lock
///   record after a guard-exit transfer (from the outermost scope since lane
///   sync5's part (c) of the patch page); with every scope monitor-free that
///   record is empty, which is exactly what the outermost scope says the
///   frame holds (an optimizing body describes every lock it holds, the
///   pre-entry ones included). A lock can only be the looping method's own
///   (a spliced body never takes one), and javac's catch-all around it covers
///   the outermost `invoke`, which the door refuses anyway.
///
/// Re-entering the body later is sound for the reason a flat guard's is: an
/// EXACT guard's re-executed bytecode throws in the innermost callee, which
/// has no exception table (the inline resolver refuses one), so the throw
/// reaches the outermost frame AT its `invoke`, the bci [`guard_exit_fact`]
/// lists for the door's exception-table check; a VALUE-EQUIVALENT guard's
/// continuation computes what the body would have.
fn chain_resumable_in_place(fs: &crate::deopt::FrameState) -> bool {
    if !osr_chain_guard_exits_enabled()
        || crate::deopt::inlined_chain_refusal(fs).is_some()
        || (crate::deopt::first_unresumable_slot(fs).is_some()
            && !(crate::deopt::osr_guard_exit_virtuals_enabled()
                && crate::deopt::frame_state_is_resumable(fs)
                && crate::deopt::osr_in_place_virtuals_refusal(fs).is_none()))
    {
        return false;
    }
    // `inlined_chain_refusal` bounded the depth, so this walk is short.
    let mut scope = Some(fs);
    let mut seen = 0usize;
    while let Some(f) = scope {
        if !f.monitors.is_empty() {
            return false;
        }
        seen += 1;
        if seen > crate::deopt::MAX_OSR_INLINE_RESUME_DEPTH + 1 {
            return false;
        }
        scope = f.caller.as_deref();
    }
    true
}

/// The lowerer's phase-3 fact (`CompiledMethod::ir_osr_guard_exit_bcis`).
///
/// `boxes` are every point the body's stubs can hand the deopt entry;
/// `rethrow_exception` ones are the reason-9 pads, which the exception route
/// resolves and are not guard exits. `classified_guard_exits` is how many of
/// the rest the lowerer classified EXACT or VALUE-EQUIVALENT as it emitted
/// them: a guard box it did not classify (an `Op::Guard`, or a producer added
/// later without a classification) makes the counts disagree, and the answer
/// is `None` — fail closed. `call_exception_bcis` are the publishing-nothing
/// exits, listed beside the guards because the door requires the same of both
/// (no exception-table row covers the bci).
pub(crate) fn guard_exit_fact(
    boxes: &[Box<DeoptimizationPoint>],
    classified_guard_exits: usize,
    call_exception_bcis: &[usize],
    propagating_exit_holds_monitor: bool,
) -> Option<Box<[u32]>> {
    guard_exit_fact_with(
        boxes,
        Some(classified_guard_exits),
        call_exception_bcis,
        propagating_exit_holds_monitor,
        osr_guard_exits_holding_a_lock_for_this_compile(),
    )
}

/// `CRATONVM_JIT_OSR_OPAQUE_ENTRY=1` — round 11 wave 14 (lane osrmerge),
/// `r11w11-irexc-osr-entry-as-a-real-predecessor`: build every loop header of
/// an optimizing body with the opaque OSR entry merge in front of it
/// (`IrBuilder::build_opaque_osr_merge`), enter the body through that merge's
/// arm (`ir_lower::Lowerer::emit_osr_entry_stubs`), and admit what such a body
/// no longer has to refuse: a header an exception handler reaches (the door's
/// `closed_by_a_handler`), and SPECULATIVE guard exits
/// (`guard_exit_fact_opaque`). **Default OFF**; declared by
/// `r11w14-osrmerge-types-osr-opaque-entry-flag-patch-FIXED-20260925.md`. No
/// `static` (`process_global_statics_ratchet`): asked a few times per compile
/// and once per door verdict.
pub fn osr_opaque_entry_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_OSR_OPAQUE_ENTRY")
}

/// [`guard_exit_fact`] for a body whose EVERY OSR entry targets an opaque
/// merge (round 11 wave 14, lane osrmerge).
///
/// The classification count is what `guard_exit_fact` uses to refuse a
/// SPECULATIVE guard: its interpreter continuation runs code the graph pruned
/// and may re-enter the body at a header whose facts assumed the speculation.
/// Through an opaque merge no header fact assumes anything about the entering
/// state — every local arrives through a φ with an `Op::OsrLocal` input and
/// memory through `Op::OsrMemory` — so that continuation is as good as any
/// other interpreter state, and the count is not asked. Everything else is:
/// every guard frame must be resumable in place (the planless resume,
/// `guard_exit_resume_bci`, re-checks it), no propagating exit may hold a
/// monitor, and the door still requires every listed bci to lie outside the
/// exception table.
///
/// The caller must publish this only when it emitted no entry that is NOT
/// opaque (`ir_lower::lower_inner`).
pub(crate) fn guard_exit_fact_opaque(
    boxes: &[Box<DeoptimizationPoint>],
    call_exception_bcis: &[usize],
    propagating_exit_holds_monitor: bool,
) -> Option<Box<[u32]>> {
    guard_exit_fact_with(
        boxes,
        None,
        call_exception_bcis,
        propagating_exit_holds_monitor,
        osr_guard_exits_holding_a_lock_for_this_compile(),
    )
}

/// The shared body of [`guard_exit_fact`] (`Some(count)`: every guard must
/// have been classified) and [`guard_exit_fact_opaque`] (`None`).
/// `admit_held_locks`: [`osr_guard_exits_holding_a_lock_enabled`] (JIT round
/// 16 wave 13), passed in so the tests can pin either rule.
fn guard_exit_fact_with(
    boxes: &[Box<DeoptimizationPoint>],
    classified_guard_exits: Option<usize>,
    call_exception_bcis: &[usize],
    propagating_exit_holds_monitor: bool,
    admit_held_locks: bool,
) -> Option<Box<[u32]>> {
    if propagating_exit_holds_monitor {
        return None;
    }
    let mut bcis: Vec<u32> = Vec::with_capacity(boxes.len() + call_exception_bcis.len());
    let mut guards = 0usize;
    for p in boxes {
        if p.semantics.rethrow_exception {
            // The for-each residual round: the OSR exception route's in-place
            // transfer re-allocates nothing, and its refusal propagates the
            // exception past a handler that matched it
            // (`jit_bridge::route_osr_exception_out_of_artifact`). With the
            // guard exits now naming deleted objects, a body whose reason-9 pad
            // names one is not entered at all.
            // OSR round (2026-10-06): unless that transfer re-allocates them
            // (`CRATONVM_JIT_OSR_EXCEPTION_EXIT_VIRTUALS`); the pad producer
            // refused every frame it cannot (`ir_lower::rethrow_frame_refusal`),
            // and the refusal is re-asked here of what was published.
            if crate::deopt::osr_guard_exit_virtuals_enabled()
                && crate::deopt::frame_state_names_virtual(&p.frame_state)
                && crate::deopt::osr_exception_exit_virtuals_refusal(&p.frame_state).is_some()
            {
                return None;
            }
            continue;
        }
        guards += 1;
        if !guard_exit_point_admitted(p, admit_held_locks) {
            return None;
        }
        // The OSR'd method's own bci: a chain box's `bci` is its innermost
        // (spliced callee's), and the door checks every listed bci against
        // THIS method's exception table, which is where a chain's re-executed
        // throw arrives (at the outermost scope's `invoke`; round 13 wave 8,
        // lane chain3). A flat frame's outermost scope is itself.
        bcis.push(crate::deopt::outermost_scope(&p.frame_state).bci);
    }
    if classified_guard_exits.is_some_and(|classified| guards != classified) {
        return None;
    }
    for &bci in call_exception_bcis {
        bcis.push(u32::try_from(bci).ok()?);
    }
    bcis.sort_unstable();
    bcis.dedup();
    Some(bcis.into_boxed_slice())
}

/// The resume bci of a guard exit taken by an optimizing OSR body — the
/// planless stand-in for `OsrEntryPlan::resume_after_exit`, and the only
/// sanctioned resume point for such an exit.
///
/// `point_addr` is the address `ir_deopt_entry` stashed with the frame
/// (`deopt::take_last_deopt_with_point`). Provenance is by that address, not
/// by bci: the point must be one of THIS artifact's boxes, a `REEXECUTE` guard
/// (not a reason-9 pad), naming the frame's own bci, at a bci the lowerer
/// admitted (`admitted`, i.e. `ir_osr_guard_exit_bcis`, sorted), and still
/// resumable. Every one of those held at admission; an `Err` here means a
/// mis-routed stash, and the caller must not resume.
pub fn guard_exit_resume_bci(
    boxes: &[Box<DeoptimizationPoint>],
    admitted: Option<&[u32]>,
    point_addr: usize,
    frame_bci: u32,
) -> Result<usize, &'static str> {
    guard_exit_resume_bci_with(
        boxes,
        admitted,
        point_addr,
        frame_bci,
        osr_guard_exits_holding_a_lock_admitted_for(boxes),
    )
}

/// [`guard_exit_resume_bci`] with the lock-holding guard rule passed in
/// (`admit_held_locks`, [`osr_guard_exits_holding_a_lock_enabled`]): the
/// planless resume re-asks the predicate the compile-time fact asked.
fn guard_exit_resume_bci_with(
    boxes: &[Box<DeoptimizationPoint>],
    admitted: Option<&[u32]>,
    point_addr: usize,
    frame_bci: u32,
    admit_held_locks: bool,
) -> Result<usize, &'static str> {
    if point_addr == 0 {
        return Err("the stash names no point");
    }
    let Some(p) = boxes.iter().find(|p| {
        let point: &DeoptimizationPoint = p;
        point as *const DeoptimizationPoint as usize == point_addr
    }) else {
        return Err("the point is not one of this artifact's guards");
    };
    if p.semantics.rethrow_exception {
        return Err("the point is a reason-9 rethrow pad, not a guard");
    }
    if p.bci != frame_bci {
        return Err("the frame's bci is not its point's");
    }
    // A back-edge poll's mode exit (interpreter round i1 wave 15, lane L2;
    // `ir_lower::Lowerer::back_edge_mode_exit_state`) needs no admission: it
    // resumes at its loop header with the state the back edge carries, a
    // state the graph reaches itself, so no continuation falsifies a header
    // fact and nothing is thrown. The lowerer keeps it out of the admitted
    // list on purpose (an exit that resumes exactly must not refuse a body
    // whose header a handler covers). The only `OsrExit` points an
    // optimizing body carries are these.
    if p.reason != DeoptReason::OsrExit {
        let Some(admitted) = admitted else {
            return Err("the artifact admitted no guard exit");
        };
        // Listed by the OUTERMOST scope's bci ([`guard_exit_fact`]); the same
        // as `p.bci` for a flat frame. For an inlined chain the answer below
        // is still the point's (innermost) bci: the VM's chain transfer parks
        // the live frame after the outermost `invoke` itself.
        if admitted
            .binary_search(&crate::deopt::outermost_scope(&p.frame_state).bci)
            .is_err()
        {
            return Err("the guard's bci was not admitted");
        }
    }
    // A poll exit may name the locks its frame holds (interpreter round i1
    // wave 25, lane L2); a guard may not. `poll_exit_point_resumable` asks
    // `guard_exit_point_resumable` of every point that is not a poll exit.
    // JIT round 16 wave 13 (lane monitor3): a guard may too under
    // `CRATONVM_JIT_OSR_GUARD_EXITS_HOLDING_A_LOCK`, by the rule the
    // compile-time fact asked ([`guard_exit_point_resumable_holding_locks`]).
    let resumable = poll_exit_point_resumable(p)
        || (admit_held_locks
            && p.reason != DeoptReason::OsrExit
            && guard_exit_point_resumable_holding_locks(p));
    if !resumable {
        return Err("the guard's frame is not resumable in place");
    }
    usize::try_from(p.bci).map_err(|_| "bci out of range")
}

/// Does this optimizing body carry a back-edge poll mode exit (interpreter
/// round i1 wave 15, lane L2)? Asked by the VM's OSR door, which counts such a
/// body in `osr_bodies_running` so an agent's loop-exit pause reaches it, as
/// it does a single-pass body entered with a plan. Only the lowerer's poll
/// exits are `OsrExit` boxes; a single-pass artifact keeps its exit maps in
/// `deopt_points` and answers `false` here. (Since wave 18 an optimizing
/// METHOD-ENTRY artifact can carry them too, `ir_lower::IrPollModeExits`;
/// the OSR door never enters one, so its count is unaffected.)
pub fn has_poll_mode_exits(boxes: &[Box<DeoptimizationPoint>]) -> bool {
    boxes
        .iter()
        .any(|p| p.reason == DeoptReason::OsrExit && !p.semantics.rethrow_exception)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deopt::{DeoptAction, FrameState, FrameValue};

    fn point(bci: u32, native_offset: u32, reason: DeoptReason) -> DeoptimizationPoint {
        DeoptimizationPoint {
            native_offset,
            bci,
            reason,
            action: DeoptAction::Reinterpret,
            speculation_id: 0,
            frame_state: FrameState {
                method_key: "P.loop:(I)J".to_string(),
                bci,
                locals: vec![FrameValue::Int(7)],
                stack: Vec::new(),
                monitors: Vec::new(),
                caller: None,
            },
            semantics: ResumeSemantics::for_reason(reason),
        }
    }

    /// Interpreter round i1 wave 50, lane L1: the one cause-less lookup. The
    /// optimizing tier's `NullCheck` + `BoundsCheck` pair at an `iaload` is
    /// `Ambiguous` without a frame (the frame-less sinks) and is told apart by
    /// the re-executed frame's array reference with one (the frame sinks); an
    /// `OsrExit` map beside them is skipped by both, and a lone map answers.
    #[test]
    fn one_lookup_answers_both_sinks() {
        // aload_1; iload_2; iaload
        let code = [0x2b, 0x1c, 0x2e];
        let pts = vec![
            point(2, 0x10, DeoptReason::OsrExit),
            point(2, 0x20, DeoptReason::NullCheck),
            point(2, 0x30, DeoptReason::BoundsCheck),
        ];
        assert_eq!(deopt_reason_at_bci(&pts, 2), ReasonAtBci::Ambiguous);
        let null_array = [FrameValue::Object(0), FrameValue::Int(3)];
        assert_eq!(
            trap_reason_at_bci(&pts, 2, Some((&code[..], &null_array[..]))),
            ReasonAtBci::Unique(DeoptReason::NullCheck)
        );
        let array = [FrameValue::Object(0x1000), FrameValue::Int(3)];
        assert_eq!(
            trap_reason_at_bci(&pts, 2, Some((&code[..], &array[..]))),
            ReasonAtBci::Unique(DeoptReason::BoundsCheck)
        );
        // Not an array access: the frame cannot tell, as before.
        assert_eq!(
            trap_reason_at_bci(&pts, 2, Some((&[0x00u8, 0x00, 0x00][..], &array[..]))),
            ReasonAtBci::Ambiguous
        );
        let lone = vec![point(5, 0x40, DeoptReason::OsrExit)];
        assert_eq!(
            trap_reason_at_bci(&lone, 5, Some((&code[..], &array[..]))),
            ReasonAtBci::Unique(DeoptReason::OsrExit)
        );
        assert_eq!(trap_reason_at_bci(&lone, 6, None), ReasonAtBci::None);
    }

    /// The shape the four VM-side `find(|dp| dp.bci == …)` copies get wrong: a
    /// protected invoke carries a re-execute guard and a rethrow frame at ONE
    /// bci, and an ordinary stash can only have come from the guard.
    #[test]
    fn a_protected_invoke_reports_the_guard_not_the_rethrow() {
        let pts = vec![
            // Recorded FIRST, which is what makes the list-order pick wrong.
            point(6, 0x10, DeoptReason::PendingException),
            point(6, 0x20, DeoptReason::ReceiverTypeChanged),
        ];
        assert!(pts[0].semantics.rethrow_exception);
        assert_eq!(
            deopt_reason_at_bci(&pts, 6),
            ReasonAtBci::Unique(DeoptReason::ReceiverTypeChanged),
            "an ordinary stash is never published by a RETHROW point"
        );
        assert_eq!(
            exceptional_reason_at_bci(&pts, 6),
            ReasonAtBci::Unique(DeoptReason::PendingException),
            "and the exceptional stash reads the other one"
        );
    }

    /// Two guards at one bci cannot be told apart from the bci, so the lookup
    /// refuses rather than picking one and applying its policy to the other.
    #[test]
    fn two_guards_at_one_bci_are_refused_rather_than_guessed() {
        let pts = vec![
            point(16, 0x37e, DeoptReason::ReceiverTypeChanged),
            point(16, 0x3c1, DeoptReason::BoundsCheck),
        ];
        assert_eq!(deopt_reason_at_bci(&pts, 16), ReasonAtBci::Ambiguous);
        // The RESUME question is still answered — the two agree on
        // `semantics`, which is the only field the resume reads by bci, and
        // refusing this shape cost `CratonBench.matrixKernel` its OSR.
        assert!(matches!(
            resume_image(&pts, 16),
            ResumeImage::Unique {
                reason_ambiguous: true,
                ..
            }
        ));
    }

    /// Several points at one bci that agree on the reason are not ambiguous —
    /// there is nothing to tell apart.
    #[test]
    fn points_that_agree_on_the_reason_are_unique() {
        let pts = vec![
            point(4, 0x10, DeoptReason::BoundsCheck),
            point(4, 0x20, DeoptReason::BoundsCheck),
        ];
        assert_eq!(
            deopt_reason_at_bci(&pts, 4),
            ReasonAtBci::Unique(DeoptReason::BoundsCheck)
        );
    }

    /// A bci with only a rethrow point has no ordinary reason, and vice versa.
    #[test]
    fn each_stash_sees_only_its_own_points() {
        let pts = vec![point(9, 0x10, DeoptReason::PendingException)];
        assert_eq!(deopt_reason_at_bci(&pts, 9), ReasonAtBci::None);
        assert_eq!(
            exceptional_reason_at_bci(&pts, 9),
            ReasonAtBci::Unique(DeoptReason::PendingException)
        );
        assert_eq!(deopt_reason_at_bci(&pts, 999), ReasonAtBci::None);
        assert_eq!(exceptional_reason_at_bci(&pts, 999), ReasonAtBci::None);
    }

    #[test]
    fn a_recorded_loop_boundary_is_the_loop_boundary() {
        let pts = vec![point(12, 0x40, DeoptReason::OsrExit)];
        assert_eq!(
            classify_exit_site(&[12], &pts, 12),
            OsrExitSite::LoopBoundary
        );
    }

    /// The `invokedynamic` trap shape, in both spellings.
    ///
    /// The trap SHARES the exit-map machinery, so its bci lands in
    /// `osr_exit_points` exactly like a loop boundary — which is why
    /// membership alone was never going to answer the lane's question. A
    /// speculative-BCE guard is the other spelling: a deopt point with no
    /// exit-map entry at all. Both are off the loop boundary and both were
    /// indistinguishable from a loop-boundary exit before this.
    #[test]
    fn a_point_that_is_not_a_loop_boundary_is_off_boundary_in_both_spellings() {
        let pts = vec![point(30, 0x80, DeoptReason::UnreachedCode)];
        // In the set (the indy trap pushes itself there)…
        assert_eq!(
            classify_exit_site(&[12, 30], &pts, 30),
            OsrExitSite::OffLoopBoundary(DeoptReason::UnreachedCode)
        );
        // …and not in it (a BCE guard).
        let guard = vec![point(30, 0x80, DeoptReason::BoundsCheck)];
        assert_eq!(
            classify_exit_site(&[12], &guard, 30),
            OsrExitSite::OffLoopBoundary(DeoptReason::BoundsCheck)
        );
    }

    /// The cross-check itself: an `OsrExit` point whose bci is missing from
    /// `osr_exit_points`. The two are written by one function, so this must
    /// never happen — and until now nothing could have said so.
    ///
    /// The exact edit that produces it: rebuild one of the two vectors through
    /// the loop transform's provenance map and not the other. `driver.rs` maps
    /// `osr_exit_points` through `LoopXform::bci_at`; a future producer that
    /// forgets to do the same for the points would land here.
    #[test]
    fn an_exit_map_missing_from_the_set_is_named_as_such() {
        let pts = vec![point(12, 0x40, DeoptReason::OsrExit)];
        assert_eq!(
            classify_exit_site(&[], &pts, 12),
            OsrExitSite::ExitMapMissing
        );
    }

    /// The one that is a defect. A bci in neither set means the artifact
    /// cannot describe the frame at all.
    #[test]
    fn a_bci_in_neither_set_is_unrecorded() {
        let pts = vec![point(12, 0x40, DeoptReason::OsrExit)];
        assert_eq!(classify_exit_site(&[12], &pts, 99), OsrExitSite::Unrecorded);
        // …and the empty artifact, which is what a production compile with no
        // exit maps looks like.
        assert_eq!(classify_exit_site(&[], &[], 0), OsrExitSite::Unrecorded);
    }

    /// The ordinary compiled-counted-loop shape — the loop-boundary exit map
    /// and the speculative-BCE range guard on one header bci — classifies as
    /// `LoopBoundary` **in either recording order**.
    ///
    /// Before the `OsrExit`-wins tie-break, the answer was whichever point the
    /// emitter happened to record first, so the same loop was charged to
    /// `osr_exit_at_loop_boundary` or to `osr_exit_off_loop_boundary` depending
    /// on emission order alone.
    #[test]
    fn a_loop_header_carrying_its_range_guard_is_still_a_loop_boundary() {
        let guard_first = vec![
            point(16, 0x3c1, DeoptReason::BoundsCheck),
            point(16, 0x37e, DeoptReason::OsrExit),
        ];
        let map_first = vec![
            point(16, 0x37e, DeoptReason::OsrExit),
            point(16, 0x3c1, DeoptReason::BoundsCheck),
        ];
        // The reason lookup refuses with the wave-49 switch off; on, it
        // names the GUARD (the exit map is a mode exit, whose stub stashes
        // its own cause; see `deopt_reason_at_bci`). Either way it never
        // hands the escalation the `OsrExit` site rule for a guard trap.
        for pts in [&guard_first, &map_first] {
            assert_eq!(
                deopt_reason_at_bci(pts, 16),
                if OSR_EXIT_MAPS_BESIDE_GUARDS {
                    ReasonAtBci::Unique(DeoptReason::BoundsCheck)
                } else {
                    ReasonAtBci::Ambiguous
                }
            );
        }
        for pts in [&guard_first, &map_first] {
            assert_eq!(
                classify_exit_site(&[16], pts, 16),
                OsrExitSite::LoopBoundary,
                "emission order must not decide the site"
            );
        }
    }

    /// And the half that is a defect detector rather than an accounting one: an
    /// `OsrExit` point whose bci is MISSING from `osr_exit_points` must read
    /// `ExitMapMissing` even when a guard shares its bci and was recorded first.
    ///
    /// The two sets are written by one function and rebuilt separately under a
    /// loop transform, so `ExitMapMissing` is the cross-check for "one was
    /// translated and the other was not". A first-in-list-order pick answered
    /// `OffLoopBoundary(BoundsCheck)` here and hid it.
    #[test]
    fn an_exit_map_missing_from_the_set_is_named_even_behind_a_guard() {
        let pts = vec![
            point(16, 0x3c1, DeoptReason::BoundsCheck),
            point(16, 0x37e, DeoptReason::OsrExit),
        ];
        assert_eq!(
            classify_exit_site(&[], &pts, 16),
            OsrExitSite::ExitMapMissing
        );
    }

    /// The tie-break does not invent an `OsrExit`: two non-`OsrExit` reasons at
    /// one bci still name the first of them and stay off the boundary.
    #[test]
    fn an_ambiguous_bci_with_no_osr_exit_point_stays_off_boundary() {
        let pts = vec![
            point(30, 0x80, DeoptReason::UnreachedCode),
            point(30, 0x90, DeoptReason::BoundsCheck),
        ];
        assert_eq!(
            classify_exit_site(&[30], &pts, 30),
            OsrExitSite::OffLoopBoundary(DeoptReason::UnreachedCode)
        );
    }

    /// A `RETHROW` point is not a candidate for the tie-break either: a
    /// protected loop header carrying a `PendingException` frame and its exit
    /// map has ONE non-rethrow reason, so the lookup is `Unique` and never
    /// reaches the ambiguous arm at all.
    #[test]
    fn a_rethrow_point_does_not_make_a_loop_header_ambiguous() {
        let pts = vec![
            point(16, 0x100, DeoptReason::PendingException),
            point(16, 0x37e, DeoptReason::OsrExit),
        ];
        assert_eq!(
            deopt_reason_at_bci(&pts, 16),
            ReasonAtBci::Unique(DeoptReason::OsrExit)
        );
        assert_eq!(
            classify_exit_site(&[16], &pts, 16),
            OsrExitSite::LoopBoundary
        );
    }

    /// Every site maps to a declared metric row. The exact edit that trips it:
    /// add a variant and forget the counter.
    #[test]
    fn every_exit_site_names_a_declared_metric() {
        for site in [
            OsrExitSite::LoopBoundary,
            OsrExitSite::OffLoopBoundary(DeoptReason::UnreachedCode),
            OsrExitSite::ExitMapMissing,
            OsrExitSite::PostCallOwnPoint,
            OsrExitSite::Unrecorded,
        ] {
            assert!(
                crate::metrics::OSR_EVENTS.contains(&site.metric()),
                "{site:?} names an undeclared metric row {}",
                site.metric()
            );
        }
    }

    /// A single-pass chain point: recorded with the spliced callee's bci and
    /// its caller chain (`frame_state.caller`).
    fn chain_point(bci: u32, native_offset: u32, reason: DeoptReason) -> DeoptimizationPoint {
        let mut p = point(bci, native_offset, reason);
        p.frame_state.method_key = "Q.callee:()V".to_string();
        p.frame_state.caller = Some(Box::new(FrameState {
            method_key: "P.loop:(I)J".to_string(),
            bci: 40,
            locals: vec![FrameValue::Int(7)],
            stack: Vec::new(),
            monitors: Vec::new(),
            caller: None,
        }));
        p
    }

    /// JIT round 19 wave 11, lane tail11 (DEOPT10-3): the frame-keyed site
    /// classification counts only points of the exit frame's own kind. A root
    /// loop-boundary map at bci 16 and a chain guard at callee bci 16: the raw
    /// scan's tie-break names the map for the CHAIN exit too (it reads
    /// `LoopBoundary`, a header this callee does not have); keyed, the chain
    /// exit is its own guard and the root exit the map. A chain `OsrExit`
    /// point is never answered from the root loop-boundary set.
    #[test]
    fn the_frame_keyed_site_classification_counts_only_its_own_kind() {
        let pts = vec![
            point(16, 0x10, DeoptReason::OsrExit),
            chain_point(16, 0x20, DeoptReason::ClassCheck),
        ];
        assert_eq!(
            classify_exit_site_keyed(&[16], &pts, 16, None),
            OsrExitSite::LoopBoundary
        );
        assert_eq!(
            classify_exit_site_keyed(&[16], &pts, 16, Some(true)),
            OsrExitSite::OffLoopBoundary(DeoptReason::ClassCheck)
        );
        assert_eq!(
            classify_exit_site_keyed(&[16], &pts, 16, Some(false)),
            OsrExitSite::LoopBoundary
        );
        // A root exit at a bci only a chain point names: nothing describes it.
        let only_chain = vec![chain_point(9, 0x30, DeoptReason::ClassCheck)];
        assert_eq!(
            classify_exit_site_keyed(&[], &only_chain, 9, Some(false)),
            OsrExitSite::Unrecorded
        );
        // A chain post-call exit (`OsrExit` with a caller chain) whose callee
        // bci happens to be in the root set is not a loop-boundary exit.
        let chain_exit = vec![chain_point(16, 0x40, DeoptReason::OsrExit)];
        assert_eq!(
            classify_exit_site_keyed(&[16], &chain_exit, 16, Some(true)),
            OsrExitSite::ExitMapMissing
        );
        assert_eq!(
            classify_exit_site_keyed(&[16], &chain_exit, 16, None),
            OsrExitSite::LoopBoundary
        );
        // The raw entry point is the `None` arm.
        assert_eq!(
            classify_exit_site(&[16], &pts, 16),
            OsrExitSite::LoopBoundary
        );
    }

    /// JIT round 19 wave 10, lane deopt10
    /// (`i62-L2-proposal-single-pass-chain-points-leave-the-roots-bci-key-space`):
    /// a chain guard whose CALLEE bci equals a root bci. Off (today), it joins
    /// that root bci's candidates -- the root trap's reason reads `Ambiguous`,
    /// and a chain post-call point of other semantics makes the bci an
    /// ambiguous image (the OSR admission refusal). On, every root-keyed
    /// reader skips it; the raw site classification still sees it.
    #[test]
    fn chain_points_leave_the_roots_bci_key_space_under_the_switch() {
        assert!(point(5, 0x10, DeoptReason::NullCheck).names_a_root_bci());
        assert!(!chain_point(5, 0x20, DeoptReason::ClassCheck).names_a_root_bci());
        let pts = vec![
            point(5, 0x10, DeoptReason::NullCheck),
            chain_point(5, 0x20, DeoptReason::ClassCheck),
        ];
        assert_eq!(
            trap_reason_at_bci_keyed(&pts, 5, None, false),
            ReasonAtBci::Ambiguous
        );
        assert_eq!(
            trap_reason_at_bci_keyed(&pts, 5, None, true),
            ReasonAtBci::Unique(DeoptReason::NullCheck)
        );
        // A bci only a chain point names: nothing at the root bci.
        let only_chain = vec![chain_point(9, 0x30, DeoptReason::ClassCheck)];
        assert_eq!(
            trap_reason_at_bci_keyed(&only_chain, 9, None, true),
            ReasonAtBci::None
        );
        assert_eq!(resume_image_keyed(&only_chain, 9, true), ResumeImage::None);
        assert!(matches!(
            resume_image_keyed(&only_chain, 9, false),
            ResumeImage::Unique { index: 0, .. }
        ));
        // Images: a chain post-call point (continue-after) beside a root guard
        // (re-execute) at one bci disagree on semantics off, not on.
        let mut post_call = chain_point(5, 0x30, DeoptReason::OsrExit);
        post_call.semantics = ResumeSemantics::RESUME;
        let images = vec![point(5, 0x10, DeoptReason::NullCheck), post_call];
        assert_eq!(
            first_ambiguous_resume_bci_keyed(&images, false),
            Some((5, 0, 1))
        );
        assert_eq!(first_ambiguous_resume_bci_keyed(&images, true), None);
        assert!(has_reason_ambiguous_bci_keyed(&pts, false));
        assert!(!has_reason_ambiguous_bci_keyed(&pts, true));
        // The site classification keeps the raw scan (its caller hands it a
        // chain exit's own callee bci).
        assert_eq!(
            classify_exit_site(&[], &only_chain, 9),
            OsrExitSite::OffLoopBoundary(DeoptReason::ClassCheck)
        );
        // The public readers follow the switch.
        let key = "CRATONVM_DEOPT_CHAIN_POINTS_OFF_ROOT_BCIS";
        let off = cratonvm_types::flags::with_thread_overrides(&[(key, None)], || {
            (
                deopt_reason_at_bci(&pts, 5),
                first_ambiguous_resume_bci(&images),
            )
        });
        assert_eq!(off, (ReasonAtBci::Ambiguous, Some((5, 0, 1))));
        let on = cratonvm_types::flags::with_thread_overrides(&[(key, Some("1"))], || {
            (
                deopt_reason_at_bci(&pts, 5),
                first_ambiguous_resume_bci(&images),
            )
        });
        assert_eq!(on, (ReasonAtBci::Unique(DeoptReason::NullCheck), None));
    }

    #[test]
    fn one_point_is_one_image() {
        let pts = vec![point(12, 0x40, DeoptReason::OsrExit)];
        assert_eq!(
            resume_image(&pts, 12),
            ResumeImage::Unique {
                index: 0,
                reason_ambiguous: false
            }
        );
        assert_eq!(resume_image(&pts, 13), ResumeImage::None);
        assert_eq!(first_ambiguous_resume_bci(&pts), None);
        assert!(!has_reason_ambiguous_bci(&pts));
    }

    /// The loop-transform shape: several copies of one bytecode, each with its
    /// own native offset. They are NOT ambiguous — differing native offsets is
    /// what makes them copies, and every consumer that RECONSTRUCTS a frame
    /// finds its point through the copy's own baked box pointer, which is an
    /// identity (`x64/deopt_stubs.rs::resolve_baked_point`; the `*_box_ptr_by_bci`
    /// maps are keyed by emitter pc, so each copy has its own entry). What the
    /// remaining by-bci lookups read is identical here, so the pick cannot be
    /// wrong.
    ///
    /// This used to say "by native offset or through the copy's own baked box".
    /// The offset half was removed in round 10: the offset is not an identity
    /// (see [`deopt_reason_at_bci`]'s doc note) and nothing looks a point up by
    /// it. The baked-pointer half was always the whole argument.
    #[test]
    fn copies_that_agree_on_what_the_consumer_reads_are_one_image() {
        let pts = vec![
            point(12, 0x40, DeoptReason::OsrExit),
            point(12, 0x90, DeoptReason::OsrExit),
            point(12, 0xE0, DeoptReason::OsrExit),
        ];
        assert_eq!(
            resume_image(&pts, 12),
            ResumeImage::Unique {
                index: 0,
                reason_ambiguous: false
            }
        );
        assert_eq!(first_ambiguous_resume_bci(&pts), None);
    }

    /// A `reason`-only disagreement is OBSERVED, not refused — and this is the
    /// test the measurement rewrote.
    ///
    /// The shape is `CratonBench.matrixKernel(I)I` at `bci 16`: the
    /// loop-boundary exit map and the speculative-BCE range guard on the same
    /// header. The first draft refused it, which cost that kernel its OSR
    /// permanently (the refusal is memoed) — 10 of the workload's 11 refusals.
    /// `ResumeSemantics::for_reason` answers `REEXECUTE` for everything but
    /// `PendingException`, so a reason disagreement never implies a semantics
    /// one, and the resume bci is not in doubt.
    #[test]
    fn a_reason_only_disagreement_is_counted_and_not_refused() {
        let pts = vec![
            point(16, 0x37e, DeoptReason::OsrExit),
            point(16, 0x3c1, DeoptReason::BoundsCheck),
        ];
        assert_eq!(
            resume_image(&pts, 16),
            ResumeImage::Unique {
                index: 0,
                reason_ambiguous: true
            }
        );
        assert_eq!(
            first_ambiguous_resume_bci(&pts),
            None,
            "refusing this costs a compiled counted loop its OSR for no soundness gain"
        );
        assert!(has_reason_ambiguous_bci(&pts));
    }

    /// The `semantics` disagreement, which is the unsound half.
    ///
    /// Only `REEXECUTE` vs `RESUME` is a disagreement between two RESUME
    /// IMAGES, and that is the pair this must refuse. A `RESUME` point is also
    /// refused wholesale by `osr_exit_policy` before this check sees it in
    /// practice; it is asserted here so the check itself stays covered.
    #[test]
    fn points_that_disagree_on_the_semantics_are_ambiguous() {
        let mut pts = vec![
            point(12, 0x40, DeoptReason::OsrExit),
            point(12, 0x90, DeoptReason::OsrExit),
        ];
        pts[1].semantics = ResumeSemantics::RESUME;
        assert!(matches!(
            resume_image(&pts, 12),
            ResumeImage::Ambiguous { .. }
        ));
    }

    /// A `RETHROW` point sharing a bci with a resume point is NOT ambiguous.
    ///
    /// The correction of 2026-08-17, and the shape that motivated it: after the
    /// RBC.6b lift a `try { foo(x); } catch (...)` loop puts a
    /// speculative-dispatch guard (`REEXECUTE`) and a `PendingException` frame
    /// (`RETHROW`) on the same invoke bci. Refusing that took `osr_entered` to
    /// ZERO on `probes/OsrExcTableProbe.java` — the artifact compiled, every
    /// correctness arm passed, and no back edge ever entered it.
    ///
    /// A `RETHROW` point is not a resume image: its bci names a throwing
    /// instruction, `resume_after_exit` rejects non-`REEXECUTE` semantics
    /// explicitly, and the exception path finds its own point by
    /// `(bci, reason == PendingException)`. So this bci has exactly one image,
    /// and `resume_image` must name it rather than call the pair arbitrary.
    #[test]
    fn a_rethrow_point_is_not_a_competing_resume_image() {
        let pts = vec![
            point(12, 0x40, DeoptReason::OsrExit),
            point(12, 0x90, DeoptReason::PendingException),
        ];
        assert_eq!(
            ResumeSemantics::for_reason(DeoptReason::PendingException),
            ResumeSemantics::RETHROW,
            "the fixture only means anything while PendingException is RETHROW"
        );
        assert!(
            matches!(resume_image(&pts, 12), ResumeImage::Unique { index: 0, .. }),
            "the OsrExit point is the one and only resume image at bci 12"
        );
        assert_eq!(
            first_ambiguous_resume_bci(&pts),
            None,
            "refusing this costs every try/catch loop its OSR entry"
        );

        // Order-independent: the rethrow point first must not become the
        // representative, which would hand `resume_after_exit` a bci it then
        // rejects for its semantics.
        let flipped = vec![
            point(12, 0x90, DeoptReason::PendingException),
            point(12, 0x40, DeoptReason::OsrExit),
        ];
        assert!(matches!(
            resume_image(&flipped, 12),
            ResumeImage::Unique { index: 1, .. }
        ));

        // A bci carrying ONLY a rethrow point names no resume image at all.
        let only_rethrow = vec![point(12, 0x90, DeoptReason::PendingException)];
        assert_eq!(resume_image(&only_rethrow, 12), ResumeImage::None);
        assert_eq!(first_ambiguous_resume_bci(&only_rethrow), None);
    }

    /// The duplicate-`native_offset` shape, as the PRODUCER can actually make
    /// it — and the reason an offset-keyed lookup would be worse here, not
    /// better.
    ///
    /// Round 10 wave 7 established that two deopt points can share one
    /// `native_offset`: `build_and_record_deopt_point` records `buf.pos()` and
    /// emits no machine code, and the three recorder families de-duplicate in
    /// three maps that do not consult each other. What that page framed as "two
    /// points at one pc with two different bcis" cannot happen, and the
    /// difference matters: the published `bci` is `resume_bci_for(pc)`, a
    /// function of the emitter pc alone, so both members of a same-offset pair
    /// carry the SAME bci and can differ only in `reason`.
    ///
    /// Which is exactly the shape this module refuses to guess at. Pinned here
    /// so that nobody re-derives "the offset is a finer key" from the fact that
    /// the bci lookup is coarse: on this pair the offset is not finer at all,
    /// and `CompiledMethod::find_deopt_point` — a `binary_search_by_key` on the
    /// offset with no `Ambiguous` to return — would have answered one of the two
    /// reasons arbitrarily where this answers [`ReasonAtBci::Ambiguous`]. (That
    /// search was deleted in round 10 wave 9; the reason it would have been the
    /// wrong thing to reach for is why.)
    ///
    /// Read, not executed: no compile was observed emitting the pair. The
    /// assertion is about what these lookups do with one, which is a pure-API
    /// question.
    #[test]
    fn a_same_offset_pair_shares_a_bci_and_the_reason_lookup_still_refuses_it() {
        // One emitter pc, two recorder families, nothing emitted between them:
        // one offset, one bci, two reasons.
        let pts = vec![
            point(16, 0x37e, DeoptReason::OsrExit),
            point(16, 0x37e, DeoptReason::BoundsCheck),
        ];
        assert_eq!(
            pts[0].native_offset, pts[1].native_offset,
            "the fixture only means anything while the two offsets are equal"
        );
        assert_eq!(
            pts[0].bci, pts[1].bci,
            "a same-offset pair necessarily agrees on bci: both are stamped \
             resume_bci_for(pc) for one pc"
        );
        // Wave 49 (`OSR_EXIT_MAPS_BESIDE_GUARDS`): a same-offset pair whose
        // other member is the `OsrExit` MAP answers the guard -- not a guess:
        // a cause-less stash cannot come from the map's stub, which stashes
        // its own cause. Two GUARDS still refuse
        // (`two_guards_at_one_bci_are_refused_rather_than_guessed`).
        assert_eq!(
            deopt_reason_at_bci(&pts, 16),
            if OSR_EXIT_MAPS_BESIDE_GUARDS {
                ReasonAtBci::Unique(DeoptReason::BoundsCheck)
            } else {
                ReasonAtBci::Ambiguous
            },
            "the escalation and de-spec paths must get a refusal, not a guess"
        );
        // The RESUME question is still answered, for the reason the module note
        // gives: `semantics` agrees, and refusing this shape cost
        // `CratonBench.matrixKernel` its OSR.
        assert!(matches!(
            resume_image(&pts, 16),
            ResumeImage::Unique {
                reason_ambiguous: true,
                ..
            }
        ));
        // And the site classification still names the loop boundary, via the
        // `OsrExit`-wins tie-break.
        assert_eq!(
            classify_exit_site(&[16], &pts, 16),
            OsrExitSite::LoopBoundary
        );
    }

    /// [`exceptional_reason_at_bci`] cannot answer [`ReasonAtBci::Ambiguous`],
    /// for every reason in the enum.
    ///
    /// Not a property of that function — a property of
    /// `ResumeSemantics::for_reason`, which answers `RETHROW` for
    /// `PendingException` and `REEXECUTE` for everything else. So the rethrow
    /// points at one bci all carry one reason and there is nothing to tell
    /// apart. Pinned because the function's own doc used to promise the "same
    /// refusal rule" as [`deopt_reason_at_bci`], which a reader could take as
    /// evidence that rethrow reasons vary. They do not.
    ///
    /// If `for_reason` gains a second `RETHROW` reason this test fails, and that
    /// failure is the signal to re-check the three places that today inherit
    /// "a rethrow bci names a THROWING instruction" from `PendingException`
    /// alone: this module's note, [`resume_image`] and [`osr_entry_frame_state`].
    ///
    /// It is NOT a signal about the exceptional sink in
    /// `vm/src/runtime/interpreter/deopt_resume.rs`. That sink used to filter on
    /// `reason == PendingException`, which a second `RETHROW` reason would have
    /// made incomplete; it now asks [`exceptional_reason_at_bci`], which is
    /// `semantics`-keyed and accepts any rethrow point. That is the fix rather
    /// than the hazard.
    #[test]
    fn the_exceptional_lookup_has_no_reachable_ambiguity() {
        let all = [
            DeoptReason::NullCheck,
            DeoptReason::ClassCheck,
            DeoptReason::BoundsCheck,
            DeoptReason::DivByZero,
            DeoptReason::ReceiverTypeChanged,
            DeoptReason::ClassLoading,
            DeoptReason::UninitializedAccess,
            DeoptReason::TransferToInterpreter,
            DeoptReason::UncommonTrap,
            DeoptReason::SpeculationFailed,
            DeoptReason::NotCompiled,
            DeoptReason::UnreachedCode,
            DeoptReason::OsrExit,
            DeoptReason::PendingException,
            DeoptReason::UnstableIf,
        ];
        // Totality, checked by the compiler rather than by a count: a new
        // `DeoptReason` fails to build here until it is added to `all` above.
        for r in all {
            match r {
                DeoptReason::NullCheck
                | DeoptReason::ClassCheck
                | DeoptReason::BoundsCheck
                | DeoptReason::DivByZero
                | DeoptReason::ReceiverTypeChanged
                | DeoptReason::ClassLoading
                | DeoptReason::UninitializedAccess
                | DeoptReason::TransferToInterpreter
                | DeoptReason::UncommonTrap
                | DeoptReason::SpeculationFailed
                | DeoptReason::NotCompiled
                | DeoptReason::UnreachedCode
                | DeoptReason::OsrExit
                | DeoptReason::PendingException
                | DeoptReason::UnstableIf => {}
            }
        }
        // Exactly one reason maps to RETHROW. That is the whole argument.
        let rethrowing: Vec<DeoptReason> = all
            .into_iter()
            .filter(|r| ResumeSemantics::for_reason(*r).rethrow_exception)
            .collect();
        assert_eq!(
            rethrowing,
            vec![DeoptReason::PendingException],
            "exceptional_reason_at_bci's refusal arm is reachable only if two \
             reasons map to RETHROW; if that changed, re-check resume_image, \
             osr_entry_frame_state and this module's note, which all argue from \
             PendingException being the only one"
        );
        // Every pair of reasons at one bci: the exceptional lookup is never
        // ambiguous, because at most one of the two is a rethrow point.
        for a in all {
            for b in all {
                let pts = vec![point(6, 0x10, a), point(6, 0x20, b)];
                assert_ne!(
                    exceptional_reason_at_bci(&pts, 6),
                    ReasonAtBci::Ambiguous,
                    "{a:?} + {b:?} produced an ambiguous exceptional reason"
                );
            }
        }
    }

    /// A disagreement at a bci OTHER than the first one scanned is still
    /// found — the walk covers every distinct bci, not just the first.
    #[test]
    fn ambiguity_is_found_at_any_bci_not_only_the_first() {
        let mut pts = vec![
            point(4, 0x10, DeoptReason::OsrExit),
            point(12, 0x40, DeoptReason::OsrExit),
            point(12, 0x90, DeoptReason::OsrExit),
        ];
        pts[2].semantics = ResumeSemantics::RESUME;
        assert_eq!(first_ambiguous_resume_bci(&pts), Some((12, 1, 2)));
    }

    // ── Tier proposal 27, phase 3 (round 11 wave 12, lane irexc) ──────────

    fn boxed(bci: u32, reason: DeoptReason) -> Box<DeoptimizationPoint> {
        Box::new(point(bci, 0, reason))
    }

    fn addr(p: &DeoptimizationPoint) -> usize {
        p as *const DeoptimizationPoint as usize
    }

    /// Only the lowerer's own JVMS checks are exact; `Op::Guard`'s reason and
    /// every receiver/unreached guard are not.
    #[test]
    fn r11w12_only_null_bounds_and_divisor_guards_are_exact() {
        assert!(guard_reason_is_exact(DeoptReason::NullCheck));
        assert!(guard_reason_is_exact(DeoptReason::BoundsCheck));
        assert!(guard_reason_is_exact(DeoptReason::DivByZero));
        assert!(!guard_reason_is_exact(DeoptReason::UncommonTrap));
        assert!(!guard_reason_is_exact(DeoptReason::ReceiverTypeChanged));
        assert!(!guard_reason_is_exact(DeoptReason::UnreachedCode));
        assert!(!guard_reason_is_exact(DeoptReason::PendingException));
    }

    /// The fact lists guard and call-exception bcis together, sorted and
    /// distinct, skips the reason-9 pads, and fails closed on a guard the
    /// lowerer did not classify.
    #[test]
    fn r11w12_the_guard_exit_fact_fails_closed_on_an_unclassified_guard() {
        let boxes = vec![
            boxed(30, DeoptReason::NullCheck),
            boxed(64, DeoptReason::ReceiverTypeChanged),
            boxed(64, DeoptReason::NullCheck),
            boxed(40, DeoptReason::PendingException),
        ];
        assert_eq!(
            guard_exit_fact(&boxes, 3, &[27, 17], false).as_deref(),
            Some(&[17u32, 27, 30, 64][..]),
            "the pad at 40 is not a guard exit and is not listed"
        );
        assert_eq!(
            guard_exit_fact(&boxes, 2, &[], false),
            None,
            "three guard boxes, two classified: an Op::Guard slipped in"
        );
        assert_eq!(guard_exit_fact(&boxes, 3, &[], true), None);
        assert_eq!(
            guard_exit_fact(&[], 0, &[5], false).as_deref(),
            Some(&[5u32][..]),
            "no guard: the phase-1 list"
        );
    }

    /// A frame the in-place transfer would decline is refused at compile time.
    #[test]
    fn r11w12_an_unresumable_guard_frame_refuses_the_fact() {
        let mut held = boxed(30, DeoptReason::NullCheck);
        held.frame_state.monitors.push(crate::deopt::MonitorInfo {
            object: FrameValue::Int(0),
            lock_depth: 1,
            relock: false,
        });
        assert_eq!(guard_exit_fact(&[held], 1, &[], false), None);

        let mut dead = boxed(30, DeoptReason::NullCheck);
        dead.frame_state.locals = vec![FrameValue::Unsupported];
        assert_eq!(guard_exit_fact(&[dead], 1, &[], false), None);

        let mut elsewhere = boxed(30, DeoptReason::NullCheck);
        elsewhere.frame_state.bci = 31;
        assert_eq!(guard_exit_fact(&[elsewhere], 1, &[], false), None);
    }

    /// The planless resume finds the guard by ADDRESS and answers its own bci;
    /// a point from elsewhere, a pad, an unadmitted bci or a mismatched frame
    /// bci is refused.
    #[test]
    fn r11w12_the_planless_resume_is_by_point_provenance() {
        let boxes = vec![
            boxed(30, DeoptReason::NullCheck),
            boxed(40, DeoptReason::PendingException),
        ];
        let admitted: &[u32] = &[30];
        assert_eq!(
            guard_exit_resume_bci(&boxes, Some(admitted), addr(&boxes[0]), 30),
            Ok(30)
        );
        let stranger = boxed(30, DeoptReason::NullCheck);
        assert!(guard_exit_resume_bci(&boxes, Some(admitted), addr(&stranger), 30).is_err());
        assert!(guard_exit_resume_bci(&boxes, Some(admitted), addr(&boxes[1]), 40).is_err());
        assert!(guard_exit_resume_bci(&boxes, Some(admitted), addr(&boxes[0]), 31).is_err());
        assert!(guard_exit_resume_bci(&boxes, Some(&[31][..]), addr(&boxes[0]), 30).is_err());
        assert!(guard_exit_resume_bci(&boxes, None, addr(&boxes[0]), 30).is_err());
        assert!(guard_exit_resume_bci(&boxes, Some(admitted), 0, 30).is_err());
    }

    /// Round 11 wave 14 (lane osrmerge): behind an opaque merge a SPECULATIVE
    /// guard (an `Op::Guard`, `UncommonTrap`, which the lowerer classifies
    /// neither exact nor value-equivalent) is admitted — the classification
    /// count is not asked — while every other refusal of the strict fact
    /// still holds, and the strict fact is unchanged.
    #[test]
    fn r11w14_an_opaque_body_admits_a_speculative_guard_and_nothing_else() {
        let boxes = vec![
            boxed(30, DeoptReason::NullCheck),
            boxed(52, DeoptReason::UncommonTrap),
            boxed(40, DeoptReason::PendingException),
        ];
        // Strict: one of the two guards is unclassified, so no fact.
        assert_eq!(guard_exit_fact(&boxes, 1, &[], false), None);
        // Opaque: both guards listed, the pad is not, the call exits are.
        assert_eq!(
            guard_exit_fact_opaque(&boxes, &[17], false).as_deref(),
            Some(&[17u32, 30, 52][..])
        );
        // A propagating exit that holds a monitor still refuses.
        assert_eq!(guard_exit_fact_opaque(&boxes, &[], true), None);
        // So does a guard frame the in-place transfer would decline.
        let mut held = boxed(52, DeoptReason::UncommonTrap);
        held.frame_state.monitors.push(crate::deopt::MonitorInfo {
            object: FrameValue::Int(0),
            lock_depth: 1,
            relock: false,
        });
        assert_eq!(guard_exit_fact_opaque(&[held], &[], false), None);
        let mut elsewhere = boxed(52, DeoptReason::UncommonTrap);
        elsewhere.frame_state.bci = 53;
        assert_eq!(guard_exit_fact_opaque(&[elsewhere], &[], false), None);
        // And the planless resume of an admitted speculative guard is by
        // provenance, exactly as for an exact one.
        let admitted = guard_exit_fact_opaque(&boxes, &[], false);
        assert_eq!(
            guard_exit_resume_bci(&boxes, admitted.as_deref(), addr(&boxes[1]), 52),
            Ok(52)
        );
    }

    // ── Round 13 wave 8 (lane chain3): chain guard exits ──────────────────

    /// A guard at callee bci `inner_bci` inside a body spliced at the OSR'd
    /// method's `invoke` at `outer_bci`.
    fn chain_box(inner_bci: u32, outer_bci: u32, reason: DeoptReason) -> Box<DeoptimizationPoint> {
        let mut p = point(inner_bci, 0, reason);
        p.frame_state.method_key = "Q.get:(I)I".to_string();
        p.frame_state.caller = Some(Box::new(FrameState {
            method_key: "P.loop:(I)J".to_string(),
            bci: outer_bci,
            locals: vec![
                FrameValue::Int(1),
                FrameValue::Long(2),
                FrameValue::Undefined,
            ],
            stack: vec![FrameValue::Int(3)],
            monitors: Vec::new(),
            caller: None,
        }));
        Box::new(p)
    }

    /// A monitor-free chain box is admitted and listed by the OSR'd method's
    /// own bci (the outermost scope's), where the door's exception-table check
    /// and the re-executed throw both land; the planless resume finds it by
    /// address, compares the frame's innermost bci with the point's, and looks
    /// the admission up by the outermost bci.
    #[test]
    fn r13w8_a_chain_guard_exit_is_admitted_at_its_outermost_bci() {
        if !osr_chain_guard_exits_enabled() {
            return;
        }
        let boxes = vec![
            chain_box(5, 40, DeoptReason::NullCheck),
            boxed(30, DeoptReason::BoundsCheck),
        ];
        assert!(guard_exit_point_resumable(&boxes[0]));
        let fact = guard_exit_fact(&boxes, 2, &[], false);
        assert_eq!(
            fact.as_deref(),
            Some(&[30u32, 40][..]),
            "the chain is listed at its invoke (40), not at the callee's bci 5"
        );
        assert_eq!(
            guard_exit_resume_bci(&boxes, fact.as_deref(), addr(&boxes[0]), 5),
            Ok(5)
        );
        assert!(
            guard_exit_resume_bci(&boxes, Some(&[5][..]), addr(&boxes[0]), 5).is_err(),
            "the callee's bci is not a bci of the OSR'd method"
        );
        assert!(guard_exit_resume_bci(&boxes, fact.as_deref(), addr(&boxes[0]), 40).is_err());
    }

    /// Every refusal of the flat rule holds for a chain, in ANY scope: a held
    /// monitor (the door's lock-record rewrite reads the trapping scope), an
    /// unresumable slot, a scalar-replaced object (no materialiser in place),
    /// a mismatched innermost bci.
    #[test]
    fn r13w8_a_chain_guard_exit_refuses_what_the_in_place_transfer_cannot_write() {
        let lock = crate::deopt::MonitorInfo {
            object: FrameValue::StackSlotRef(-24),
            lock_depth: 1,
            relock: false,
        };
        let mut outer_lock = chain_box(5, 40, DeoptReason::NullCheck);
        if let Some(c) = outer_lock.frame_state.caller.as_mut() {
            c.monitors.push(lock);
        }
        assert!(!guard_exit_point_resumable(&outer_lock));
        assert_eq!(guard_exit_fact(&[outer_lock], 1, &[], false), None);

        let mut dead = chain_box(5, 40, DeoptReason::NullCheck);
        if let Some(c) = dead.frame_state.caller.as_mut() {
            c.locals.push(FrameValue::Unsupported);
        }
        assert!(!guard_exit_point_resumable(&dead));

        let mut boxed_obj = chain_box(5, 40, DeoptReason::NullCheck);
        if let Some(c) = boxed_obj.frame_state.caller.as_mut() {
            c.locals.push(FrameValue::VirtualObjectRef(0));
        }
        assert!(!guard_exit_point_resumable(&boxed_obj));

        let mut elsewhere = chain_box(5, 40, DeoptReason::NullCheck);
        elsewhere.frame_state.bci = 6;
        assert!(!guard_exit_point_resumable(&elsewhere));
    }

    /// Interpreter round i1 wave 25, lane L2: a back-edge POLL exit (`OsrExit`)
    /// may name the locks its frame holds — `relock == false`, each on a
    /// reference frame word — and the planless resume takes it; an elided
    /// level (`relock`), a lock object that is not a reference frame word,
    /// and a GUARD naming any lock still refuse, and the guard-exit fact
    /// (OSR entry admission) is not widened by the poll's rule.
    #[test]
    fn a_poll_exit_may_hold_the_frames_locks_and_a_guard_may_not() {
        let lock = |object: FrameValue, relock: bool| crate::deopt::MonitorInfo {
            object,
            lock_depth: 1,
            relock,
        };
        let mut poll = boxed(6, DeoptReason::OsrExit);
        poll.frame_state
            .monitors
            .push(lock(FrameValue::StackSlotRef(-24), false));
        assert!(!guard_exit_point_resumable(&poll));
        assert_eq!(
            poll_exit_point_resumable(&poll),
            OSR_POLL_EXITS_HOLDING_A_LOCK_ENABLED
        );
        let boxes = vec![poll];
        if OSR_POLL_EXITS_HOLDING_A_LOCK_ENABLED {
            // A poll exit needs no admission: `admitted` is `None`.
            assert_eq!(
                guard_exit_resume_bci(&boxes, None, addr(&boxes[0]), 6),
                Ok(6)
            );
        }

        let mut elided = boxed(6, DeoptReason::OsrExit);
        elided
            .frame_state
            .monitors
            .push(lock(FrameValue::StackSlotRef(-24), true));
        // Wave 26 (lane L2): the in-place transfer re-takes an elided level.
        assert_eq!(
            poll_exit_point_resumable(&elided),
            OSR_POLL_EXITS_HOLDING_A_LOCK_ENABLED && OSR_POLL_EXITS_WITH_AN_ELIDED_LOCK_ENABLED
        );
        let mut elided_guard = boxed(9, DeoptReason::NullCheck);
        elided_guard
            .frame_state
            .monitors
            .push(lock(FrameValue::StackSlotRef(-24), true));
        assert!(
            !poll_exit_point_resumable(&elided_guard),
            "a guard still names no lock, elided or not"
        );
        let mut not_a_word = boxed(6, DeoptReason::OsrExit);
        not_a_word
            .frame_state
            .monitors
            .push(lock(FrameValue::Register(3), false));
        assert!(!poll_exit_point_resumable(&not_a_word));

        let mut guard = boxed(9, DeoptReason::NullCheck);
        guard
            .frame_state
            .monitors
            .push(lock(FrameValue::StackSlotRef(-24), false));
        assert!(!poll_exit_point_resumable(&guard), "a guard names no lock");
        let guards = vec![guard];
        assert_eq!(guard_exit_fact(&guards, 1, &[], false), None);
        // The strict rule: since the round-17 close the loop block coarsening
        // switches are default ON and widen the planless resume to a
        // lock-holding guard (`osr_guard_exits_holding_a_lock_admitted_for`),
        // so pin them off here.
        let strict = cratonvm_types::flags::with_thread_overrides(
            &[
                ("CRATONVM_JIT_OSR_GUARD_EXITS_HOLDING_A_LOCK", None),
                ("CRATONVM_JIT_IR_LOOP_LOCK_COARSEN_BLOCKS", Some("0")),
                (
                    "CRATONVM_JIT_IR_LOOP_LOCK_COARSEN_INSTANCE_BLOCKS",
                    Some("0"),
                ),
            ],
            || guard_exit_resume_bci(&guards, Some(&[9]), addr(&guards[0]), 9),
        );
        assert!(strict.is_err());
    }

    // ── JIT round 16 wave 13 (lane monitor3): guards holding the frame's lock ──

    /// `R16MonitorCrowd.shortCs`'s optimizing OSR body: the `putfield` null
    /// checks at 27 and 39 inside `synchronized (LOCK)` (local 2), and the
    /// `monitorexit`'s reason-9 pad at 43.
    fn short_cs_boxes() -> Vec<Box<DeoptimizationPoint>> {
        let held = |bci: u32, reason: DeoptReason| {
            let mut b = boxed(bci, reason);
            b.frame_state.monitors.push(crate::deopt::MonitorInfo {
                object: FrameValue::StackSlotRef(-24),
                lock_depth: 1,
                relock: false,
            });
            b
        };
        vec![
            held(27, DeoptReason::NullCheck),
            held(39, DeoptReason::NullCheck),
            held(43, DeoptReason::PendingException),
        ]
    }

    /// Off, a guard naming a lock refuses the fact (the shipped rule, and the
    /// positive control of the arm below on the SAME boxes); on, both guards
    /// are listed, the pad is not, and the planless resume takes a listed
    /// guard only under the same rule.
    #[test]
    fn r16w13_a_guard_holding_the_frames_lock_is_admitted_only_under_its_switch() {
        let boxes = short_cs_boxes();
        assert_eq!(
            guard_exit_fact_with(&boxes, Some(2), &[], false, false),
            None
        );
        let fact = guard_exit_fact_with(&boxes, Some(2), &[], false, true);
        assert_eq!(fact.as_deref(), Some(&[27u32, 39][..]));
        assert_eq!(
            guard_exit_resume_bci_with(&boxes, fact.as_deref(), addr(&boxes[1]), 39, true),
            Ok(39)
        );
        assert!(
            guard_exit_resume_bci_with(&boxes, fact.as_deref(), addr(&boxes[1]), 39, false)
                .is_err(),
            "the strict rule still refuses the lock-holding guard at run time"
        );
        assert!(
            guard_exit_resume_bci_with(&boxes, fact.as_deref(), addr(&boxes[2]), 43, true).is_err(),
            "a reason-9 pad is never a guard exit"
        );
        // The opaque fact widens the same way, and only under the switch.
        assert_eq!(guard_exit_fact_with(&boxes, None, &[], false, false), None);
        assert_eq!(
            guard_exit_fact_with(&boxes, None, &[17], false, true).as_deref(),
            Some(&[17u32, 27, 39][..])
        );
    }

    /// Enabling the held-lock extension must retain unlocked inline chains
    /// already admitted by the strict rule. The runtime transfer uses the
    /// same union; replacing strict admission with the flat-only extension
    /// used to withdraw otherwise valid compiled bodies.
    #[test]
    fn held_lock_extension_preserves_unlocked_guard_chains() {
        let chain = chain_box(5, 40, DeoptReason::NullCheck);
        assert!(guard_exit_point_resumable(&chain));
        assert!(!guard_exit_point_resumable_holding_locks(&chain));
        let boxes = vec![chain];
        let strict = guard_exit_fact_with(&boxes, Some(1), &[], false, false);
        let widened = guard_exit_fact_with(&boxes, Some(1), &[], false, true);
        assert_eq!(strict.as_deref(), Some(&[40u32][..]));
        assert_eq!(widened, strict);
        assert_eq!(
            guard_exit_resume_bci_with(&boxes, widened.as_deref(), addr(&boxes[0]), 5, true),
            Ok(5)
        );
    }

    /// Everything but a taken, flat, frame-word lock still refuses with the
    /// switch on: an elided level, a lock object outside a reference frame
    /// word, a zero depth, a lock in an inlined chain, a propagating exit
    /// that holds a monitor, an unclassified guard, an unresumable slot. A
    /// poll exit keeps its own rule.
    #[test]
    fn r16w13_the_lock_holding_guard_rule_admits_only_taken_frame_word_locks() {
        let lock = |object: FrameValue, relock: bool, lock_depth: u32| crate::deopt::MonitorInfo {
            object,
            lock_depth,
            relock,
        };
        let guard_with = |m: crate::deopt::MonitorInfo| {
            let mut b = boxed(27, DeoptReason::NullCheck);
            b.frame_state.monitors.push(m);
            b
        };
        let ok = guard_with(lock(FrameValue::StackSlotRef(-24), false, 1));
        assert!(guard_exit_point_resumable_holding_locks(&ok));
        assert!(!guard_exit_point_resumable(&ok));
        assert!(
            !poll_exit_point_resumable(&ok),
            "the poll rule is not widened"
        );

        for bad in [
            guard_with(lock(FrameValue::StackSlotRef(-24), true, 1)),
            guard_with(lock(FrameValue::Register(3), false, 1)),
            guard_with(lock(FrameValue::Int(0), false, 1)),
            guard_with(lock(FrameValue::StackSlotRef(-24), false, 0)),
        ] {
            assert!(!guard_exit_point_resumable_holding_locks(&bad));
            assert_eq!(
                guard_exit_fact_with(&[bad], Some(1), &[], false, true),
                None
            );
        }

        let mut chain = chain_box(5, 40, DeoptReason::NullCheck);
        if let Some(c) = chain.frame_state.caller.as_mut() {
            c.monitors
                .push(lock(FrameValue::StackSlotRef(-24), false, 1));
        }
        assert!(!guard_exit_point_resumable_holding_locks(&chain));

        let mut dead = guard_with(lock(FrameValue::StackSlotRef(-24), false, 1));
        dead.frame_state.locals = vec![FrameValue::Unsupported];
        assert!(!guard_exit_point_resumable_holding_locks(&dead));

        let boxes = short_cs_boxes();
        assert_eq!(
            guard_exit_fact_with(&boxes, Some(2), &[], true, true),
            None,
            "a publishing-nothing exit that holds a monitor still refuses"
        );
        assert_eq!(
            guard_exit_fact_with(&boxes, Some(1), &[], false, true),
            None,
            "an unclassified guard still refuses"
        );

        // A poll exit is asked its own question whatever the switch says.
        let mut poll = boxed(2, DeoptReason::OsrExit);
        poll.frame_state
            .monitors
            .push(lock(FrameValue::StackSlotRef(-24), false, 1));
        assert_eq!(
            guard_exit_point_resumable_holding_locks(&poll),
            guard_exit_point_resumable(&poll)
        );
    }

    /// The switch is read through the thread-overridable flag layer: the
    /// env-reading fact follows it (default off). The loop block coarsening
    /// switches (default ON since the round-17 close) widen the switch-wide
    /// answer too, so the off arm pins them to `=0`.
    #[test]
    fn r16w13_the_fact_follows_the_lock_holding_guard_switch() {
        let boxes = short_cs_boxes();
        let key = "CRATONVM_JIT_OSR_GUARD_EXITS_HOLDING_A_LOCK";
        let no_blocks = [
            (key, None),
            ("CRATONVM_JIT_IR_LOOP_LOCK_COARSEN_BLOCKS", Some("0")),
            (
                "CRATONVM_JIT_IR_LOOP_LOCK_COARSEN_INSTANCE_BLOCKS",
                Some("0"),
            ),
        ];
        let off = cratonvm_types::flags::with_thread_overrides(&no_blocks, || {
            (
                osr_guard_exits_holding_a_lock_enabled(),
                guard_exit_fact(&boxes, 2, &[], false),
            )
        });
        assert_eq!(off, (false, None));
        let on = cratonvm_types::flags::with_thread_overrides(&[(key, Some("1"))], || {
            (
                osr_guard_exits_holding_a_lock_enabled(),
                guard_exit_fact(&boxes, 2, &[], false),
            )
        });
        assert!(on.0);
        assert_eq!(on.1.as_deref(), Some(&[27u32, 39][..]));
    }

    /// JIT round 17 wave 2 (lane monitor3): either loop block coarsening
    /// switch turns the lock-holding guard rule on (the coarsened block's
    /// exits all hold its monitor); all three off keep the strict rule.
    #[test]
    fn r17w2_monitor3_a_block_coarsening_switch_admits_lock_holding_guard_exits() {
        let boxes = short_cs_boxes();
        let hold = "CRATONVM_JIT_OSR_GUARD_EXITS_HOLDING_A_LOCK";
        let blocks = "CRATONVM_JIT_IR_LOOP_LOCK_COARSEN_BLOCKS";
        let instance = "CRATONVM_JIT_IR_LOOP_LOCK_COARSEN_INSTANCE_BLOCKS";
        // Round 17 wave 2 (lane irfeed): the switch-wide rule this test pins
        // is MON3R17-2's `=0` arm.
        let per = "CRATONVM_JIT_IR_LOOP_LOCK_COARSEN_GUARD_EXITS_PER_ARTIFACT";
        let ask = |b: Option<&'static str>, i: Option<&'static str>| {
            cratonvm_types::flags::with_thread_overrides(
                &[(hold, None), (blocks, b), (instance, i), (per, Some("0"))],
                || {
                    (
                        osr_guard_exits_holding_a_lock_enabled(),
                        guard_exit_fact(&boxes, 2, &[], false),
                    )
                },
            )
        };
        // Both block switches are default ON since the round-17 close.
        let (on, fact) = ask(None, None);
        assert!(on, "default");
        assert_eq!(fact.as_deref(), Some(&[27u32, 39][..]), "default");
        assert_eq!(ask(Some("0"), Some("0")), (false, None));
        for (b, i) in [
            (Some("1"), Some("0")),
            (Some("0"), Some("1")),
            (Some("1"), Some("1")),
        ] {
            let (on, fact) = ask(b, i);
            assert!(on, "{b:?} {i:?}");
            assert_eq!(fact.as_deref(), Some(&[27u32, 39][..]), "{b:?} {i:?}");
        }
    }

    /// JIT round 17 wave 2 (lane irfeed, MON3R17-2): with the per-artifact
    /// rule (default on), a block switch widens the compile-time fact only
    /// inside a compile that coarsened a block, and the artifact-side answer
    /// only for boxes that hold a lock at a guard. The round-16 switch still
    /// widens every body.
    #[test]
    fn r17w2_irfeed_block_switch_widens_only_a_coarsened_compile() {
        let boxes = short_cs_boxes();
        let lock_free = vec![boxed(27, DeoptReason::NullCheck)];
        let hold = "CRATONVM_JIT_OSR_GUARD_EXITS_HOLDING_A_LOCK";
        let blocks = "CRATONVM_JIT_IR_LOOP_LOCK_COARSEN_BLOCKS";
        let instance = "CRATONVM_JIT_IR_LOOP_LOCK_COARSEN_INSTANCE_BLOCKS";
        let per = "CRATONVM_JIT_IR_LOOP_LOCK_COARSEN_GUARD_EXITS_PER_ARTIFACT";
        let under = |h: Option<&'static str>, b: Option<&'static str>, f: &dyn Fn() -> bool| {
            cratonvm_types::flags::with_thread_overrides(
                // The instance switch is default ON since the round-17 close.
                &[(hold, h), (blocks, b), (instance, Some("0")), (per, None)],
                f,
            )
        };
        // Nothing armed: no compile coarsened anything.
        assert!(!under(
            None,
            Some("1"),
            &osr_guard_exits_holding_a_lock_for_this_compile
        ));
        assert!(under(None, Some("1"), &|| guard_exit_fact(
            &boxes,
            2,
            &[],
            false
        )
        .is_none()));
        // The switch-wide answer (the VM door's) is unchanged.
        assert!(under(
            None,
            Some("1"),
            &osr_guard_exits_holding_a_lock_enabled
        ));
        // A compile that coarsened a block.
        crate::ir_evidence::begin_compile();
        crate::ir_evidence::note_loop_block_coarsened();
        let armed = under(None, Some("1"), &|| {
            guard_exit_fact(&boxes, 2, &[], false).as_deref() == Some(&[27u32, 39][..])
        });
        let off = under(
            None,
            Some("0"),
            &osr_guard_exits_holding_a_lock_for_this_compile,
        );
        let _ = crate::ir_evidence::take();
        assert!(
            armed,
            "the coarsened compile's fact admits the lock-holding guards"
        );
        assert!(!off, "and only under a block switch");
        // The artifact side: boxes that hold a lock at a guard, or not.
        assert!(under(None, Some("1"), &|| {
            osr_guard_exits_holding_a_lock_admitted_for(&boxes)
        }));
        assert!(!under(None, Some("1"), &|| {
            osr_guard_exits_holding_a_lock_admitted_for(&lock_free)
        }));
        assert!(under(Some("1"), None, &|| {
            osr_guard_exits_holding_a_lock_admitted_for(&lock_free)
        }));
    }
}

/// The for-each residual round (2026-10-06): an optimizing OSR body's guard
/// exit may name a scalar-replaced object, which the in-place transfer
/// re-allocates (`CRATONVM_JIT_OSR_GUARD_EXIT_VIRTUALS`, default on); a
/// reason-9 pad naming one withdraws the whole fact, because the exception
/// route's transfer re-allocates nothing.
#[cfg(test)]
mod fer_osr_guard_exit_virtual_tests {
    use super::*;
    use crate::deopt::{DeoptAction, FrameState, FrameValue, VirtualObjectState};

    const KEY: &str = "CRATONVM_JIT_OSR_GUARD_EXIT_VIRTUALS";

    fn boxed(bci: u32, reason: DeoptReason, locals: Vec<FrameValue>) -> Box<DeoptimizationPoint> {
        Box::new(DeoptimizationPoint {
            native_offset: 0x40 + bci,
            bci,
            reason,
            action: DeoptAction::Reinterpret,
            speculation_id: 0,
            frame_state: FrameState {
                method_key: "P.loop:(I)J".to_string(),
                bci,
                locals,
                stack: Vec::new(),
                monitors: Vec::new(),
                caller: None,
            },
            semantics: ResumeSemantics::for_reason(reason),
        })
    }

    fn iterator() -> FrameValue {
        FrameValue::VirtualObject(VirtualObjectState {
            array_element_type: None,
            id: 1,
            class_id: 232,
            num_fields: 2,
            field_values: vec![FrameValue::Int(3), FrameValue::Int(-1)],
        })
    }

    #[test]
    fn a_guard_naming_a_deleted_iterator_is_admitted_only_with_the_switch() {
        let boxes = vec![boxed(
            57,
            DeoptReason::NullCheck,
            vec![FrameValue::Int(7), iterator()],
        )];
        let on = cratonvm_types::flags::with_thread_overrides(&[(KEY, None)], || {
            guard_exit_fact_with(&boxes, Some(1), &[], false, false)
        });
        assert_eq!(on.as_deref(), Some(&[57u32][..]));
        let off = cratonvm_types::flags::with_thread_overrides(&[(KEY, Some("0"))], || {
            guard_exit_fact_with(&boxes, Some(1), &[], false, false)
        });
        assert_eq!(off, None);
    }

    /// OSR round (2026-10-06): a pad naming a deleted object keeps the fact
    /// while the OSR exception route re-allocates it, and withdraws it with
    /// `CRATONVM_JIT_OSR_EXCEPTION_EXIT_VIRTUALS=0` (the for-each residual
    /// round's rule) or when the frame holds a monitor.
    #[test]
    fn a_pad_naming_a_deleted_object_withdraws_the_fact_only_when_unreallocated() {
        const EXC: &str = "CRATONVM_JIT_OSR_EXCEPTION_EXIT_VIRTUALS";
        let boxes = vec![
            boxed(57, DeoptReason::NullCheck, vec![FrameValue::Int(7)]),
            boxed(60, DeoptReason::PendingException, vec![iterator()]),
        ];
        let on = cratonvm_types::flags::with_thread_overrides(&[(KEY, None), (EXC, None)], || {
            guard_exit_fact_with(&boxes, Some(1), &[], false, false)
        });
        assert_eq!(on.as_deref(), Some(&[57u32][..]));
        let exc_off =
            cratonvm_types::flags::with_thread_overrides(&[(KEY, None), (EXC, Some("0"))], || {
                guard_exit_fact_with(&boxes, Some(1), &[], false, false)
            });
        assert_eq!(exc_off, None);
        let mut locked = vec![
            boxed(57, DeoptReason::NullCheck, vec![FrameValue::Int(7)]),
            boxed(60, DeoptReason::PendingException, vec![iterator()]),
        ];
        locked[1]
            .frame_state
            .monitors
            .push(crate::deopt::MonitorInfo {
                object: FrameValue::StackSlotRef(-16),
                lock_depth: 1,
                relock: false,
            });
        let held =
            cratonvm_types::flags::with_thread_overrides(&[(KEY, None), (EXC, None)], || {
                guard_exit_fact_with(&locked, Some(1), &[], false, false)
            });
        assert_eq!(held, None);
        // The guard-exit kill switch restores the old rule, which skipped every pad.
        let off = cratonvm_types::flags::with_thread_overrides(&[(KEY, Some("0"))], || {
            guard_exit_fact_with(&boxes, Some(1), &[], false, false)
        });
        assert_eq!(off.as_deref(), Some(&[57u32][..]));
    }
}
