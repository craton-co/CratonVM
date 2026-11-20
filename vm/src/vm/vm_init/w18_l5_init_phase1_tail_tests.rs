use super::InitPhase1TailPlacement as P;

#[test]
fn init_phase1_tail_word_selects_the_placement() {
    assert_eq!(P::from_word(None), P::AfterBootLayer);
    assert_eq!(P::from_word(Some("layer")), P::AfterBootLayer);
    assert_eq!(P::from_word(Some("1")), P::AfterBootLayer);
    assert_eq!(P::from_word(Some("")), P::AfterBootLayer);
    for off in [
        "0",
        "off",
        "false",
        "no",
        "early",
        " Early ",
        "before-phase1",
    ] {
        assert_eq!(P::from_word(Some(off)), P::BeforePhase1, "{off:?}");
    }
    assert_eq!(P::from_word(Some("phase1")), P::AfterPhase1);
    assert_eq!(P::from_word(Some("after-phase1")), P::AfterPhase1);
}
