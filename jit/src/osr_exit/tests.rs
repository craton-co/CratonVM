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
            class_id: 0,
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
        class_id: 0,
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
        class_id: 0,
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


// JIT round 17 wave 2 (lane monitor3): either loop block coarsening
// switch turns the lock-holding guard rule on (the coarsened block's
// exits all hold its monitor); all three off keep the strict rule.


// JIT round 17 wave 2 (lane irfeed, MON3R17-2): with the per-artifact
// rule (default on), a block switch widens the compile-time fact only
// inside a compile that coarsened a block, and the artifact-side answer
// only for boxes that hold a lock at a guard. The round-16 switch still
// widens every body.
