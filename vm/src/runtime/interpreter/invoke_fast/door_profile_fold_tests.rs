use super::{door_invocation_profile_fold, INVOCATION_SYNC_EVERY};

#[test]
fn a_template_counter_folds_its_first_call_once() {
    assert_eq!(door_invocation_profile_fold(1, false), 0);
    assert_eq!(door_invocation_profile_fold(1, true), 1);
    assert_eq!(door_invocation_profile_fold(INVOCATION_SYNC_EVERY, false), INVOCATION_SYNC_EVERY);
    assert_eq!(door_invocation_profile_fold(INVOCATION_SYNC_EVERY, true), INVOCATION_SYNC_EVERY - 1);
    assert_eq!(door_invocation_profile_fold(INVOCATION_SYNC_EVERY * 2, true), INVOCATION_SYNC_EVERY);
}
