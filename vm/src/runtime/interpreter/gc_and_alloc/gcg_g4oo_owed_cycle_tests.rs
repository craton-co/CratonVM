use super::*;

/// The deferral is opt-in and never replaces the service thread.
#[test]
fn gcg_g4oo_the_alloc_fail_cycle_is_deferred_only_without_a_service_and_opted_in() {
    assert!(alloc_fail_cycle_deferred(false, true));
    assert!(!alloc_fail_cycle_deferred(false, false));
    assert!(!alloc_fail_cycle_deferred(true, true));
    assert!(!alloc_fail_cycle_deferred(true, false));    }