use super::*;

#[test]
fn only_a_blocked_armed_and_deposited_park_is_credited() {
    let registry = ThreadRegistry::new();
    let (parked, other) = (ThreadId(1), ThreadId(2));
    registry.register(parked, "parked", None);
    registry.register(other, "other", None);
    registry.publish_os_tid_claim(parked, 6161);
    registry.publish_os_tid_claim(other, 6262);
    registry.mark_stw_ready(parked);
    registry.mark_stw_ready(other);
    let state = |id: ThreadId| {
        registry
            .threads
            .read()
            .get(&id)
            .unwrap()
            .gc_block_state
            .clone()
    };
    assert!(
        registry.park_credited_os_tids().is_empty(),
        "nobody blocked"
    );
    state(parked)
        .park_credit_armed
        .store(true, Ordering::Release);
    assert!(
        registry.park_credited_os_tids().is_empty(),
        "armed but not blocked"
    );
    for id in [parked, other] {
        state(id).in_blocked_region.store(true, Ordering::Release);
    }
    assert!(
        registry.park_credited_os_tids().is_empty(),
        "armed and blocked, but no deposit scanned its chain"
    );
    state(parked)
        .park_credit_deposited
        .store(true, Ordering::Release);
    assert_eq!(registry.park_credited_os_tids(), vec![6161]);
    state(other)
        .park_credit_deposited
        .store(true, Ordering::Release);
    assert_eq!(
        registry.park_credited_os_tids(),
        vec![6161],
        "deposited but not armed is not credited"
    );
    assert_eq!(registry.blocked_os_tids().len(), 2, "both still read");
}
