use super::{cat2_model_disagreements, reconcile_top_cat2};

/// The ordinary case: the abstract interpretation answers and the
/// peephole, where it answers at all, says the same thing.
#[test]
fn the_analysis_answers_when_the_peephole_agrees_with_it() {
    assert_eq!(reconcile_top_cat2(0, true, Some(true)), Some(true));
    assert_eq!(reconcile_top_cat2(0, false, Some(false)), Some(false));
}

/// The reach the unification buys. The peephole declines whenever the
/// producing instruction is a shuffle, a block boundary, an
/// `invokedynamic`, a `wide`, a `checkcast` or an `iinc`; the analysis
/// carries the width through all of them, and `<shuffle>; dup2` is a
/// method that used to be refused by this tier alone.
#[test]
fn the_analysis_answers_where_the_peephole_has_no_opinion() {
    assert_eq!(reconcile_top_cat2(0, true, None), Some(true));
    assert_eq!(reconcile_top_cat2(0, false, None), Some(false));
}

/// A disagreement is a refusal and not a vote: two independent analyses of
/// one question cannot both be right, this call site cannot know which is
/// wrong, and a compile that bails is always semantically legal.
#[test]
fn a_disagreement_refuses_rather_than_picking_a_side() {
    assert_eq!(reconcile_top_cat2(7, true, Some(false)), None);
    assert_eq!(reconcile_top_cat2(7, false, Some(true)), None);
}

/// …and it is COUNTED. The complaint in `NOTES-irfront.md` §6 is that the
/// three models could diverge "for reasons nothing records"; a divergence
/// nobody can observe is worse than one that costs a method its compile.
#[test]
fn a_disagreement_is_recorded_where_a_soak_run_can_find_it() {
    let before = cat2_model_disagreements();
    assert_eq!(reconcile_top_cat2(11, true, Some(false)), None);
    assert!(
        cat2_model_disagreements() > before,
        "the counter is what makes the divergence observable"
    );
    // The counter is a defect signal, not a traffic meter: an agreement
    // answers without touching it. Asserted as "the agreement still
    // answers" rather than as an exact count, because the counter is
    // process-wide and the test binary runs these in parallel.
    assert_eq!(reconcile_top_cat2(11, true, Some(true)), Some(true));
    assert_eq!(reconcile_top_cat2(11, true, None), Some(true));
}
