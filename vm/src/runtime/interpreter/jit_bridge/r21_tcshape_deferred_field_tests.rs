use super::{deferred_field_site_linked, deferred_field_site_wait};
use crate::config::VmConfig;
use crate::vm::SharedVm;
use std::sync::atomic::Ordering;

/// A site is waited on (by any number of bodies) until its first linked
/// execution serves it, and never again after: a body that still defers
/// it after the recompile cannot ask another one.
#[test]
fn a_deferred_field_site_is_waited_on_until_served_and_never_again() {
    let shared = SharedVm::new(VmConfig::default());
    let waiting = || {
        shared
            .jit
            .bridge
            .deferred_field_waiting_count
            .load(Ordering::Acquire)
    };
    // Nothing waits: the helper's call is one load and no row appears.
    deferred_field_site_linked(&shared, 0x7a01, 3);
    assert_eq!(waiting(), 0);
    assert!(deferred_field_site_wait(&shared, 0x7a01, 3));
    assert!(
        deferred_field_site_wait(&shared, 0x7a01, 3),
        "a second body may wait too"
    );
    assert!(deferred_field_site_wait(&shared, 0x7a01, 4));
    assert_eq!(waiting(), 2);
    deferred_field_site_linked(&shared, 0x7a01, 3);
    assert_eq!(waiting(), 1);
    assert!(
        !deferred_field_site_wait(&shared, 0x7a01, 3),
        "a served site is spent"
    );
    deferred_field_site_linked(&shared, 0x7a01, 3);
    assert_eq!(waiting(), 1, "a served site is served once");
    deferred_field_site_linked(&shared, 0x7a01, 4);
    assert_eq!(waiting(), 0);
}

/// tcm21 (TCSHAPE21-1 census): the per-VM execution counter reports only
/// on powers of two, so a run's last line is the total within a factor of
/// two, and it counts every call.
#[test]
fn tcm21_the_deferred_site_execution_census_reports_on_powers_of_two() {
    let bridge = super::JitBridgeState::default();
    let said: Vec<u64> = (0..20)
        .filter_map(|_| super::note_deferred_field_site_exec(&bridge))
        .collect();
    assert_eq!(said, vec![1, 2, 4, 8, 16]);
    assert_eq!(bridge.deferred_field_site_execs.load(Ordering::Relaxed), 20);
}
