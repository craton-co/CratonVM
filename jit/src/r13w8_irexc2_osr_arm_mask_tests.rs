use super::mask_osr_arm_coverage_to_handler_reach;

#[test]
fn only_handler_reached_pcs_stay_covered() {
    let mut covered = vec![true, true, false, true, true];
    mask_osr_arm_coverage_to_handler_reach(&mut covered, Some(&[1, 2, 4]));
    // pc 2 was never covered: reach does not widen coverage.
    assert_eq!(covered, vec![false, true, false, false, true]);
}

#[test]
fn unanalysable_code_builds_no_arm() {
    let mut covered = vec![true; 4];
    mask_osr_arm_coverage_to_handler_reach(&mut covered, None);
    assert!(covered.iter().all(|&c| !c));
}

#[test]
fn no_handler_reach_builds_no_arm() {
    let mut covered = vec![true; 4];
    mask_osr_arm_coverage_to_handler_reach(&mut covered, Some(&[]));
    assert!(covered.iter().all(|&c| !c));
}
