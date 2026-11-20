use super::not_offered_reason;

#[test]
fn nothing_promoted_says_nothing() {
    assert_eq!(not_offered_reason(false, false, false, false), None);
    assert_eq!(not_offered_reason(false, true, true, true), None);
}

#[test]
fn every_open_gate_offers_the_graph() {
    assert_eq!(not_offered_reason(true, true, true, true), None);
}

#[test]
fn each_closed_gate_is_named_first_cause_first() {
    let off = not_offered_reason(true, false, false, false).expect("refused");
    assert!(off.contains("unroller is off"), "{off}");
    let spent = not_offered_reason(true, true, false, true).expect("refused");
    assert!(spent.contains("latch is spent"), "{spent}");
    let switch = not_offered_reason(true, true, true, false).expect("refused");
    assert!(switch.contains("UNROLL_PROMOTED_LOOPS"), "{switch}");
}
