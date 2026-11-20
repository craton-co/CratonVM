use super::*;

/// `jdk.internal.misc.VM.toThreadState`, as JDK 25 writes it.
fn to_thread_state(status: i32) -> i32 {
    if status & 0x0004 != 0 {
        1
    } else if status & 0x0400 != 0 {
        2
    } else if status & 0x0010 != 0 {
        3
    } else if status & 0x0020 != 0 {
        4
    } else if status & 0x0002 != 0 {
        5
    } else if status & 0x0001 == 0 {
        0
    } else {
        1
    }
}

#[test]
fn gci_i12td_snapshot_status_decodes_to_the_same_state() {
    for ordinal in 0..=5 {
        assert_eq!(to_thread_state(thread_snapshot_status(ordinal)), ordinal, "ordinal {ordinal}");
    }
}

#[test]
fn gci_i12td_snapshot_blocker_follows_hotspots_closure() {
    // BLOCKED: the monitor being entered.
    assert_eq!(thread_snapshot_blocker(2, false, true), Some(BLOCKER_WAITING_TO_LOCK));
    // Parked: the park blocker, ahead of any monitor.
    assert_eq!(thread_snapshot_blocker(3, true, false), Some(BLOCKER_PARK));
    assert_eq!(thread_snapshot_blocker(4, true, true), Some(BLOCKER_PARK));
    // `Object.wait`: the monitor waited on.
    assert_eq!(thread_snapshot_blocker(3, false, true), Some(BLOCKER_WAITING_ON));
    assert_eq!(thread_snapshot_blocker(4, false, true), Some(BLOCKER_WAITING_ON));
    // Sleeping, running, or a park with no blocker: none.
    assert_eq!(thread_snapshot_blocker(4, false, false), None);
    assert_eq!(thread_snapshot_blocker(1, true, true), None);
}

#[test]
fn gci_i12td_thread_snapshot_create_is_a_bridge_on_the_real_jdk_path() {
    let mut registry = NativeMethodRegistry::new();
    register_runtime_natives(&mut registry);
    assert_eq!(
        registry.kind_of(
            "jdk/internal/vm/ThreadSnapshot",
            "create",
            "(Ljava/lang/Thread;)Ljdk/internal/vm/ThreadSnapshot;"
        ),
        Some(NativeKind::Bridge)
    );
}
