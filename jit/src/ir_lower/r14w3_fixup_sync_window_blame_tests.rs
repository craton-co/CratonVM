use super::*;

/// RV3-1: the first refused window's pc is kept through later refusals
/// and a labelled bail, and a new attempt starts with none.
#[test]
fn the_first_refused_window_is_blamed_until_the_next_attempt() {
    clear_lower_bail();
    assert_eq!(last_sync_window_refused_pc(), None);
    note_sync_window_refused_pc(7);
    note_sync_window_refused_pc(40);
    note_lower_bail("synchronized splice window");
    assert_eq!(last_sync_window_refused_pc(), Some(7));
    assert_eq!(last_lower_bail(), Some("synchronized splice window"));
    clear_lower_bail();
    assert_eq!(last_sync_window_refused_pc(), None);
    assert_eq!(last_lower_bail(), None);
}
