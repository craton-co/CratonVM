use super::{sort_mod_count_outcome, SortModCount};

#[test]
fn an_unchanged_count_is_bumped_once() {
    // `ArrayList.sort` ends in `modCount++` even when nothing moved.
    assert_eq!(sort_mod_count_outcome(Some(7), Some(7)), SortModCount::Bump);
    assert_eq!(
        sort_mod_count_outcome(Some(i32::MAX), Some(i32::MAX)),
        SortModCount::Bump
    );
}

#[test]
fn a_comparator_that_modified_the_list_is_a_cme() {
    assert_eq!(
        sort_mod_count_outcome(Some(7), Some(8)),
        SortModCount::Comodified
    );
    assert_eq!(
        sort_mod_count_outcome(Some(7), None),
        SortModCount::Comodified
    );
}

#[test]
fn a_receiver_outside_the_protocol_is_left_alone() {
    assert_eq!(sort_mod_count_outcome(None, Some(3)), SortModCount::Skip);
    assert_eq!(sort_mod_count_outcome(None, None), SortModCount::Skip);
}
