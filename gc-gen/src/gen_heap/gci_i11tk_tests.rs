use super::*;
use cratonvm_gc_core::gc_quiescence::TakeoverVerdict;

/// A verdict the pinned copy can honour: one helper window pinned whole.
fn pinnable() -> TakeoverVerdict {
    TakeoverVerdict {
        helper_windows: 1,
        ..TakeoverVerdict::NONE
    }
}

/// The take-over arm's other divert terms are exactly promotion-OOM risk,
/// an explicit full GC and a GPU veto; with none of them a pinnable
/// take-over cycle is taken (the compaction default no longer appears in
/// the arm's inputs at all), and the decline census agrees.
#[test]
fn gci_i11tk_compaction_intent_is_not_a_takeover_divert_term() {
    for bits in 0u32..8 {
        let (oom, full, gpu) = (bits & 1 == 1, bits & 2 == 2, bits & 4 == 4);
        let div = takeover_other_divert_terms(oom, full, gpu);
        assert_eq!(div, oom || full || gpu, "bits={bits:#b}");
        let take = pinned_copy_takes_takeover_cycle(true, false, false, div, pinnable());
        assert_eq!(take, !div, "bits={bits:#b}");
        let why = pinned_takeover_decline_reason(true, false, false, div, pinnable());
        assert_eq!(why, (!take).then_some(takeover_decline::OTHER_DIVERT), "bits={bits:#b}");
    }
}

/// What lets the arm drop the term: a ledger-pinned cycle's Phase 5 major
/// never compacts -- not on `CRATONVM_OLDGEN_COMPACT`, not on a pending
/// fragmentation request, whatever the JIT guard and P-C say -- and the
/// guard is not even read on such a cycle.
#[test]
fn gci_i11tk_a_ledger_pinned_cycle_never_compacts_its_phase5_major() {
    for bits in 0u32..32 {
        let bit = |i: u32| (bits >> i) & 1 == 1;
        let (compiled, guard, oldgen_compact, frag, pc) = (bit(0), bit(1), bit(2), bit(3), bit(4));
        let reads = std::cell::Cell::new(0u32);
        let no_compact = phase5_major_no_compact(true, compiled, || {
            reads.set(reads.get() + 1);
            guard
        });
        assert!(no_compact, "bits={bits:#b}");
        assert_eq!(reads.get(), 0, "the guard is not read on a ledger-pinned cycle");
        assert_eq!(
            moving_major_compaction_plan(no_compact, oldgen_compact, frag, pc),
            (false, false),
            "bits={bits:#b}"
        );
    }
}

/// The factored veto is the inline expression it replaced on the
/// unpinned cycles too: the JIT guard decides only with compiled frames
/// live, and is read only then.
#[test]
fn gci_i11tk_the_phase5_veto_matches_the_d4n_expression_off_the_pinned_copy() {
    let reads = std::cell::Cell::new(0u32);
    let guard = |answer: bool| {
        let reads = &reads;
        move || {
            reads.set(reads.get() + 1);
            answer
        }
    };
    assert!(!phase5_major_no_compact(false, false, guard(true)));
    assert_eq!(reads.get(), 0, "no compiled frame: the guard is not read");
    assert!(phase5_major_no_compact(false, true, guard(true)));
    assert!(!phase5_major_no_compact(false, true, guard(false)));
    assert_eq!(reads.get(), 2);
    // Off the veto, OLDGEN_COMPACT compacts unless P-C arms, as before.
    assert_eq!(moving_major_compaction_plan(false, true, false, false), (true, false));
    assert_eq!(moving_major_compaction_plan(false, true, false, true), (false, true));
}
