use super::*;

#[test]
fn gcj_j1yg_the_unproven_read_tolerates_only_accepted_reasons() {
    use incomplete_reason as r;
    let unreg = 1usize << r::UNREGISTERED_JIT_FRAME;
    let accepted = unreg | (1usize << r::UNPUBLISHED_FRAME_OOP);
    assert!(!coverage_refuses_ledger_read_with(false, false, 0, None));
    assert!(coverage_refuses_ledger_read_with(true, false, unreg, None));
    assert!(coverage_refuses_ledger_read_with(false, true, unreg, None));
    assert!(!coverage_refuses_ledger_read_with(true, true, unreg, Some(accepted)));
    assert!(!coverage_refuses_ledger_read_with(false, false, 0, Some(accepted)));
    // A reason outside the accepted set, no reason at all, or an unregistered
    // frame flagged without its reason.
    let foreign = 1usize << r::FOREIGN_INNERMOST_RBP;
    assert!(coverage_refuses_ledger_read_with(true, true, unreg | foreign, Some(accepted)));
    assert!(coverage_refuses_ledger_read_with(false, true, 0, Some(accepted)));
    let unpub = 1usize << r::UNPUBLISHED_FRAME_OOP;
    assert!(coverage_refuses_ledger_read_with(true, true, unpub, Some(accepted)));
    assert!(!coverage_refuses_ledger_read_with(false, true, unpub, Some(accepted)));
}
