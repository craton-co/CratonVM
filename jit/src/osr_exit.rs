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
//! # What "the same image" means
//!
//! Two deopt points at one bci are interchangeable **for this consumer** when
//! they agree on the fields the by-bci lookups read without checking them
//! against anything:
//!
//! | Field | Read by | Consequence of picking the wrong copy |
//! |---|---|---|
//! | `semantics` | [`crate::OsrEntryPlan::resume_after_exit`] | parking the interpreter at a bci that already took effect — the same double-execution one bytecode down |
//! | `reason` | the de-speculation step at the OSR-exit reject sink | the wrong recompile policy: `OsrExit`'s count-based retry where `UnreachedCode`'s "give up immediately" was meant (the recorded Groovy `IndyInterface` regression) |
//!
//! Everything else about two points at one bci is *supposed* to differ:
//! `native_offset` by construction (that IS what makes them two images), and
//! the per-slot machine locations with it. `frame_state`'s slot **types** are
//! re-verified against the live interpreter frame by the entry contract, so a
//! divergence there is not this check's business —
//! `x64::loop_rewrite::deopt_point_difference` already draws that same line and
//! argues it at length.
//!
//! So the rule is: group by bci, and refuse only when two points at one bci
//! disagree on `(semantics, reason)`. An artifact where they agree has one
//! image *as far as any consumer can tell*, and refusing it would cost OSR for
//! no soundness gain.

use crate::deopt::{DeoptReason, DeoptimizationPoint, ResumeSemantics};

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
    let in_set = osr_exit_points.iter().any(|p| *p == bci as usize);
    // The reason comes from the point the resume path would pick, so the two
    // answers cannot be derived from different copies.
    let reason = match resume_image(deopt_points, bci) {
        ResumeImage::Unique { index } => deopt_points[index].reason,
        // Ambiguous: several images that disagree. Take the first, only to
        // NAME the site — the entry was already refused for this artifact
        // (`osr-entry-ambiguous-exit-image`), so this arm exists so the count
        // still lands somewhere rather than being dropped.
        ResumeImage::Ambiguous { first, .. } => deopt_points[first].reason,
        ResumeImage::None => return OsrExitSite::Unrecorded,
    };
    match (in_set, reason) {
        (true, DeoptReason::OsrExit) => OsrExitSite::LoopBoundary,
        (false, DeoptReason::OsrExit) => OsrExitSite::ExitMapMissing,
        (_, other) => OsrExitSite::OffLoopBoundary(other),
    }
}

/// How many resume images the artifact records at one bci.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResumeImage {
    /// No deopt point at this bci: nothing can describe the frame.
    None,
    /// Exactly one, or several that agree on `(semantics, reason)` — see the
    /// module note. `index` is the representative's position in `deopt_points`.
    Unique { index: usize },
    /// Two or more that disagree, so a by-bci lookup picks arbitrarily.
    /// Carries both indices so the diagnostic can name what differs.
    Ambiguous { first: usize, second: usize },
}

/// Which resume image `bci` names in `deopt_points`.
///
/// The agreement predicate is `(semantics, reason)` and nothing else; see the
/// module note for why the per-slot state is deliberately excluded.
pub fn resume_image(deopt_points: &[DeoptimizationPoint], bci: u32) -> ResumeImage {
    let mut first: Option<(usize, ResumeSemantics, DeoptReason)> = None;
    for (i, p) in deopt_points.iter().enumerate() {
        if p.bci != bci {
            continue;
        }
        match first {
            None => first = Some((i, p.semantics, p.reason)),
            Some((j, sem, reason)) => {
                if sem != p.semantics || reason != p.reason {
                    return ResumeImage::Ambiguous {
                        first: j,
                        second: i,
                    };
                }
            }
        }
    }
    match first {
        Some((i, _, _)) => ResumeImage::Unique { index: i },
        None => ResumeImage::None,
    }
}

/// The first bci of this artifact that names more than one resume image, if
/// any — the admission-time form of the lane's "what to refuse".
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
    // Quadratic in the number of DISTINCT bcis only because `resume_image`
    // rescans; artifacts carry tens of points, and this runs once per OSR
    // admission, not per iteration. A map would be faster and would need an
    // allocation on a path that today makes none.
    let mut seen: Vec<u32> = Vec::new();
    for p in deopt_points {
        if seen.contains(&p.bci) {
            continue;
        }
        seen.push(p.bci);
        if let ResumeImage::Ambiguous { first, second } = resume_image(deopt_points, p.bci) {
            return Some((p.bci, first, second));
        }
    }
    None
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
        assert_eq!(classify_exit_site(&[], &pts, 12), OsrExitSite::ExitMapMissing);
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

    /// Every site maps to a declared metric row. The exact edit that trips it:
    /// add a variant and forget the counter.
    #[test]
    fn every_exit_site_names_a_declared_metric() {
        for site in [
            OsrExitSite::LoopBoundary,
            OsrExitSite::OffLoopBoundary(DeoptReason::UnreachedCode),
            OsrExitSite::ExitMapMissing,
            OsrExitSite::Unrecorded,
        ] {
            assert!(
                crate::metrics::OSR_EVENTS.contains(&site.metric()),
                "{site:?} names an undeclared metric row {}",
                site.metric()
            );
        }
    }

    #[test]
    fn one_point_is_one_image() {
        let pts = vec![point(12, 0x40, DeoptReason::OsrExit)];
        assert_eq!(resume_image(&pts, 12), ResumeImage::Unique { index: 0 });
        assert_eq!(resume_image(&pts, 13), ResumeImage::None);
        assert_eq!(first_ambiguous_resume_bci(&pts), None);
    }

    /// The loop-transform shape: several copies of one bytecode, each with its
    /// own native offset. They are NOT ambiguous — differing native offsets is
    /// what makes them copies, and every consumer that reconstructs a frame
    /// finds its point by native offset or through the copy's own baked box.
    /// What the by-bci lookups read is identical, so the pick cannot be wrong.
    #[test]
    fn copies_that_agree_on_what_the_consumer_reads_are_one_image() {
        let pts = vec![
            point(12, 0x40, DeoptReason::OsrExit),
            point(12, 0x90, DeoptReason::OsrExit),
            point(12, 0xE0, DeoptReason::OsrExit),
        ];
        assert_eq!(resume_image(&pts, 12), ResumeImage::Unique { index: 0 });
        assert_eq!(first_ambiguous_resume_bci(&pts), None);
    }

    /// The one that must refuse. Two points at one bci whose `reason` differs
    /// is the recorded Groovy `IndyInterface` shape: the de-speculation step
    /// looks the reason up BY BCI and takes the first, so an `UnreachedCode`
    /// trap reported as an `OsrExit` gets the count-based recompile-and-retry
    /// policy instead of "give up immediately", and the method keeps
    /// re-entering the trap on every call.
    #[test]
    fn points_that_disagree_on_the_reason_are_ambiguous() {
        let pts = vec![
            point(12, 0x40, DeoptReason::OsrExit),
            point(12, 0x90, DeoptReason::UnreachedCode),
        ];
        assert_eq!(
            resume_image(&pts, 12),
            ResumeImage::Ambiguous {
                first: 0,
                second: 1
            }
        );
        assert_eq!(first_ambiguous_resume_bci(&pts), Some((12, 0, 1)));
    }

    /// And on the semantics, which is the unsound half: parking the
    /// interpreter at a bci that already took effect re-executes it.
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

    /// A disagreement at a bci OTHER than the first one scanned is still
    /// found — the walk covers every distinct bci, not just the first.
    #[test]
    fn ambiguity_is_found_at_any_bci_not_only_the_first() {
        let mut pts = vec![
            point(4, 0x10, DeoptReason::OsrExit),
            point(12, 0x40, DeoptReason::OsrExit),
            point(12, 0x90, DeoptReason::OsrExit),
        ];
        pts[2].reason = DeoptReason::BoundsCheck;
        assert_eq!(first_ambiguous_resume_bci(&pts), Some((12, 1, 2)));
    }
}
