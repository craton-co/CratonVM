//! Interpreter round i4 wave 95, lane L6
//! (`CRATONVM_VTHREAD_RETIRE_MANAGER_ENTRIES`; page
//! `i94-L6-terminated-jdk-virtual-threads-stay-in-the-manager-map`).
use super::*;

fn run(mgr: &VirtualThreadManager, ids: std::ops::RangeInclusive<u64>) {
    for id in ids {
        mgr.create_jdk_continuation(id, "vt", 1);
        mgr.terminate(id);
    }
}

#[test]
fn terminated_jdk_entries_stay_bounded_and_the_newest_are_kept() {
    let mgr = VirtualThreadManager::new(1)
        .with_jdk_continuations(true)
        .with_retired_jdk_entry_cap(Some(4));
    run(&mgr, 1..=50);
    assert!(mgr.thread_count() <= 8, "{}", mgr.thread_count());
    assert!(mgr.is_virtual(50));
    assert_eq!(mgr.get_state(50), Some(VirtualThreadState::Terminated));
    assert!(!mgr.is_virtual(1));
    assert!(!mgr.join(1, 99), "a removed id joins as a terminated one");
    // A live JDK continuation and a bound virtual thread are never removed.
    mgr.create_jdk_continuation(100, "live", 1);
    mgr.create_virtual_thread_with_id(101, "bound");
    mgr.terminate(101);
    run(&mgr, 200..=260);
    assert!(mgr.is_jdk_continuation(100));
    assert!(mgr.is_virtual(101));
    // A second terminate of a kept entry is not counted twice.
    mgr.terminate(260);
    assert!(mgr.is_virtual(260));
    mgr.shutdown();
}

#[test]
fn without_a_cap_every_entry_is_kept() {
    let mgr = VirtualThreadManager::new(1).with_jdk_continuations(true);
    run(&mgr, 1..=40);
    assert_eq!(mgr.thread_count(), 40);
    assert!(mgr.is_virtual(1));
    mgr.shutdown();
}
