use super::*;

#[test]
fn gci_i9ng_the_label_door_names_the_reason_and_decides_nothing() {
    begin_moving_young_coverage_cycle();
    mark_moving_young_coverage_incomplete();
    assert_eq!(moving_young_incomplete_reason(), incomplete_reason::NONE);
    label_moving_young_coverage_incomplete(incomplete_reason::XT_HELPER_WINDOW);
    assert_eq!(moving_young_incomplete_reason(), incomplete_reason::XT_HELPER_WINDOW);
    assert_eq!(moving_young_incomplete_reason_mask(), 0, "deciders read the mask");
    assert!(!peer_state_reason_recorded());
    assert!(!unrewritable_peer_state());
    assert!(moving_young_coverage_incomplete());
    label_moving_young_coverage_incomplete(incomplete_reason::XT_TAKEOVER);
    assert_eq!(
        moving_young_incomplete_reason(),
        incomplete_reason::XT_HELPER_WINDOW,
        "first wins"
    );
    begin_moving_young_coverage_cycle();
    mark_moving_young_coverage_incomplete_because(incomplete_reason::UNPUBLISHED_FRAME_OOP);
    label_moving_young_coverage_incomplete(incomplete_reason::XT_TAKEOVER);
    assert_eq!(moving_young_incomplete_reason(), incomplete_reason::UNPUBLISHED_FRAME_OOP);
    begin_moving_young_coverage_cycle();
    label_moving_young_coverage_incomplete(incomplete_reason::XT_TAKEOVER);
    assert!(!moving_young_coverage_incomplete(), "a label alone refuses nothing");
    begin_moving_young_coverage_cycle();
}
