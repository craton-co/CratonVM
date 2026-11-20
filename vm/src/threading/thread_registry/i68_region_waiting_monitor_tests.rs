use super::*;

/// The region's monitor reads as the JMX lock while it is up, never
/// replaces a monitor a real wait published, counts no wait, and is gone
/// once withdrawn.
#[test]
fn a_region_publishes_its_monitor_for_its_span_only() {
    let registry = ThreadRegistry::new();
    let tid = ThreadId(9681);
    registry.register(tid, "i68-region-waiter", None);
    // SAFETY: only stored and compared by address; never dereferenced.
    let lock = unsafe { ObjectRef::from_raw(0x68_0000 as *mut u8) };
    // SAFETY: as above.
    let other = unsafe { ObjectRef::from_raw(0x68_1000 as *mut u8) };
    assert!(registry.publish_region_waiting_monitor(tid, lock));
    assert_eq!(registry.peek_jmx_waiting_monitor(tid), Some(lock));
    let (_, waiting, _, _, _, _, _, waited_count) =
        registry.jmx_lock_snapshot(tid).expect("registered");
    assert_eq!(waiting, Some(lock), "JMX reads the region's monitor");
    assert!(
        !registry.publish_region_waiting_monitor(tid, other),
        "a published monitor is never replaced"
    );
    assert_eq!(registry.peek_jmx_waiting_monitor(tid), Some(lock));
    registry.withdraw_region_waiting_monitor(tid);
    assert_eq!(registry.peek_jmx_waiting_monitor(tid), None);
    let (_, _, _, _, _, _, _, after) = registry.jmx_lock_snapshot(tid).expect("registered");
    assert_eq!(after, waited_count, "a region's slices count no JMX wait");
    // A real wait's monitor stays where it is.
    registry.set_jmx_waiting_monitor(tid, other);
    assert!(!registry.publish_region_waiting_monitor(tid, lock));
    assert_eq!(registry.peek_jmx_waiting_monitor(tid), Some(other));
    registry.take_jmx_waiting_monitor(tid);
    assert!(
        !registry.publish_region_waiting_monitor(ThreadId(9682), lock),
        "unknown thread"
    );
}

#[test]
fn the_switch_has_a_test_hook() {
    let mut registry = ThreadRegistry::new();
    registry.set_stand_in_wait_objects_for_test(true);
    assert!(registry.stand_in_wait_objects());
    registry.set_stand_in_wait_objects_for_test(false);
    assert!(!registry.stand_in_wait_objects());
}
