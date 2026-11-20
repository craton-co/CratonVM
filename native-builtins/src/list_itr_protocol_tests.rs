use super::{list_itr_comod_violated, list_itr_has_next, list_itr_has_previous};

#[test]
fn comodification_needs_both_counts_and_a_difference() {
    assert!(list_itr_comod_violated(Some(3), Some(4)));
    assert!(!list_itr_comod_violated(Some(4), Some(4)));
    // A missing slot on either side is "no check", never a throw.
    assert!(!list_itr_comod_violated(None, Some(4)));
    assert!(!list_itr_comod_violated(Some(4), None));
    assert!(!list_itr_comod_violated(None, None));
}

#[test]
fn has_next_is_cursor_not_equal_size_under_the_jdk_protocol() {
    // Parked at the end of a 5-element list, then one element removed
    // behind it: HotSpot's `hasNext()` is `5 != 4`, true, and the `next()`
    // that follows throws CME. The legacy `<` answered false.
    assert!(list_itr_has_next(5, 4, true));
    assert!(!list_itr_has_next(5, 4, false));
    assert!(!list_itr_has_next(4, 4, true));
    assert!(list_itr_has_next(0, 4, true));
    assert!(list_itr_has_next(-1, 4, false));
}

#[test]
fn has_previous_is_cursor_not_equal_zero_under_the_jdk_protocol() {
    assert!(list_itr_has_previous(3, true));
    assert!(!list_itr_has_previous(0, true));
    assert!(list_itr_has_previous(-1, true));
    assert!(!list_itr_has_previous(-1, false));
}
