use super::*;

// -----------------------------------------------------------------------
// Virtual Thread Scheduler tests
// -----------------------------------------------------------------------

fn make_task(name: &str) -> VirtualThreadTask {
    VirtualThreadTask {
        thread_id: 0,
        name: name.to_string(),
        state: VThreadState::New,
        priority: 5,
        continuation_id: 0,
    }
}

#[test]
fn test_scheduler_spawn_assigns_id() {
    let mut sched = VirtualThreadScheduler::new(SchedulerConfig::default());
    let id = sched.spawn(make_task("t1"));
    assert_eq!(id, 1);
    assert_eq!(sched.total_spawned, 1);
    assert_eq!(sched.run_queue.len(), 1);
}

#[test]
fn test_scheduler_spawn_increments_ids() {
    let mut sched = VirtualThreadScheduler::new(SchedulerConfig::default());
    let id1 = sched.spawn(make_task("t1"));
    let id2 = sched.spawn(make_task("t2"));
    assert_eq!(id1, 1);
    assert_eq!(id2, 2);
    assert_eq!(sched.total_spawned, 2);
}

#[test]
fn test_scheduler_spawn_sets_runnable() {
    let mut sched = VirtualThreadScheduler::new(SchedulerConfig::default());
    sched.spawn(make_task("t1"));
    assert_eq!(sched.run_queue[0].state, VThreadState::Runnable);
}

#[test]
fn test_scheduler_mount_next() {
    let mut sched = VirtualThreadScheduler::new(SchedulerConfig::default());
    sched.spawn(make_task("t1"));
    let task = sched.mount_next(0).unwrap();
    assert_eq!(task.state, VThreadState::Running);
    assert_eq!(sched.mounted_count, 1);
    assert!(sched.run_queue.is_empty());
}

#[test]
fn test_scheduler_mount_empty_queue() {
    let mut sched = VirtualThreadScheduler::new(SchedulerConfig::default());
    assert!(sched.mount_next(0).is_none());
}

#[test]
fn test_scheduler_mount_invalid_carrier() {
    let mut sched = VirtualThreadScheduler::new(SchedulerConfig::default());
    sched.spawn(make_task("t1"));
    assert!(sched.mount_next(999).is_none());
}

#[test]
fn test_scheduler_unmount_complete() {
    let mut sched = VirtualThreadScheduler::new(SchedulerConfig::default());
    sched.spawn(make_task("t1"));
    let task = sched.mount_next(0).unwrap();
    sched.unmount(0, task, UnmountReason::Complete);
    assert_eq!(sched.mounted_count, 0);
    assert_eq!(sched.total_completed, 1);
}

#[test]
fn test_scheduler_unmount_yield_requeues() {
    let mut sched = VirtualThreadScheduler::new(SchedulerConfig::default());
    sched.spawn(make_task("t1"));
    let task = sched.mount_next(0).unwrap();
    sched.unmount(0, task, UnmountReason::Yield);
    assert_eq!(sched.run_queue.len(), 1);
    assert_eq!(sched.run_queue[0].state, VThreadState::Runnable);
}

#[test]
fn test_scheduler_unmount_park_parks_thread() {
    let mut sched = VirtualThreadScheduler::new(SchedulerConfig::default());
    sched.spawn(make_task("t1"));
    let task = sched.mount_next(0).unwrap();
    let tid = task.thread_id;
    sched.unmount(0, task, UnmountReason::Park);
    assert!(sched.parked_threads.contains_key(&tid));
}

#[test]
fn test_scheduler_park() {
    let mut sched = VirtualThreadScheduler::new(SchedulerConfig::default());
    let id = sched.spawn(make_task("t1"));
    sched.park(id, ParkReason::LockSupport);
    assert!(sched.parked_threads.contains_key(&id));
    assert!(sched.run_queue.is_empty());
}

#[test]
fn test_scheduler_unpark() {
    let mut sched = VirtualThreadScheduler::new(SchedulerConfig::default());
    let id = sched.spawn(make_task("t1"));
    sched.park(id, ParkReason::LockSupport);
    assert!(sched.unpark(id));
    assert!(!sched.parked_threads.contains_key(&id));
    assert_eq!(sched.run_queue.len(), 1);
}

#[test]
fn test_scheduler_unpark_not_parked() {
    let mut sched = VirtualThreadScheduler::new(SchedulerConfig::default());
    assert!(!sched.unpark(999));
}

#[test]
fn test_scheduler_complete() {
    let mut sched = VirtualThreadScheduler::new(SchedulerConfig::default());
    let id = sched.spawn(make_task("t1"));
    sched.complete(id);
    assert!(sched.run_queue.is_empty());
    assert_eq!(sched.total_completed, 1);
}

#[test]
fn test_scheduler_stats() {
    let mut sched = VirtualThreadScheduler::new(SchedulerConfig::default());
    sched.spawn(make_task("t1"));
    sched.spawn(make_task("t2"));
    let id3 = sched.spawn(make_task("t3"));
    sched.park(id3, ParkReason::Sleep(1000));
    let stats = sched.get_stats();
    assert_eq!(stats.total_spawned, 3);
    assert_eq!(stats.queue_depth, 2);
    assert_eq!(stats.currently_parked, 1);
}

#[test]
fn test_scheduler_work_stealing_counter() {
    let mut sched = VirtualThreadScheduler::new(SchedulerConfig::default());
    sched.spawn(make_task("t1"));
    let _ = sched.mount_next(0);
    assert_eq!(sched.carrier_threads[0].steal_count, 1);
}

#[test]
fn test_carrier_thread_tasks_executed() {
    let mut sched = VirtualThreadScheduler::new(SchedulerConfig::default());
    sched.spawn(make_task("t1"));
    let task = sched.mount_next(0).unwrap();
    sched.unmount(0, task, UnmountReason::Complete);
    assert_eq!(sched.carrier_threads[0].tasks_executed, 1);
}

// -----------------------------------------------------------------------
// Monitor tests
// -----------------------------------------------------------------------

#[test]
fn test_monitor_acquire() {
    let mut mgr = MonitorManager::new();
    assert_eq!(mgr.acquire(100, 1), MonitorResult::Acquired);
}

#[test]
fn test_monitor_reentrant() {
    let mut mgr = MonitorManager::new();
    mgr.acquire(100, 1);
    assert_eq!(mgr.acquire(100, 1), MonitorResult::AlreadyOwned);
    assert_eq!(mgr.monitors[&100].entry_count, 2);
}

#[test]
fn test_monitor_release() {
    let mut mgr = MonitorManager::new();
    mgr.acquire(100, 1);
    assert_eq!(mgr.release(100, 1), MonitorResult::Released);
    assert!(mgr.monitors[&100].owner.is_none());
}

#[test]
fn test_monitor_release_reentrant() {
    let mut mgr = MonitorManager::new();
    mgr.acquire(100, 1);
    mgr.acquire(100, 1); // reentrant
    mgr.release(100, 1);
    // Still owned (entry_count was 2, now 1)
    assert!(mgr.is_held_by(100, 1));
}

#[test]
fn test_monitor_release_not_owner() {
    let mut mgr = MonitorManager::new();
    mgr.acquire(100, 1);
    assert_eq!(mgr.release(100, 2), MonitorResult::NotOwner);
}

#[test]
fn test_monitor_contention() {
    let mut mgr = MonitorManager::new();
    mgr.acquire(100, 1);
    assert_eq!(mgr.acquire(100, 2), MonitorResult::WaitEntered);
    assert_eq!(mgr.monitors[&100].entry_set.len(), 1);
}

#[test]
fn test_monitor_wait() {
    let mut mgr = MonitorManager::new();
    mgr.acquire(100, 1);
    assert_eq!(mgr.wait(100, 1, None), MonitorResult::WaitEntered);
    assert!(mgr.monitors[&100].wait_set.contains(&1));
    assert!(mgr.monitors[&100].owner.is_none());
}

#[test]
fn test_monitor_wait_not_owner() {
    let mut mgr = MonitorManager::new();
    mgr.acquire(100, 1);
    assert_eq!(mgr.wait(100, 2, None), MonitorResult::NotOwner);
}

#[test]
fn test_monitor_notify() {
    let mut mgr = MonitorManager::new();
    mgr.acquire(100, 1);
    mgr.wait(100, 1, None);
    // Thread 1 is in wait set now
    let result = mgr.notify(100);
    assert_eq!(result, MonitorResult::Notified(1));
    assert!(mgr.monitors[&100].wait_set.is_empty());
    assert!(mgr.monitors[&100].entry_set.contains(&1));
}

#[test]
fn test_monitor_notify_all() {
    let mut mgr = MonitorManager::new();
    mgr.acquire(100, 1);
    mgr.wait(100, 1, None);
    // Manually push another thread into wait set for testing
    mgr.monitors.get_mut(&100).unwrap().wait_set.push(2);
    let result = mgr.notify_all(100);
    assert_eq!(result, MonitorResult::Notified(2));
    assert!(mgr.monitors[&100].wait_set.is_empty());
}

#[test]
fn test_monitor_is_held_by() {
    let mut mgr = MonitorManager::new();
    mgr.acquire(100, 1);
    assert!(mgr.is_held_by(100, 1));
    assert!(!mgr.is_held_by(100, 2));
    assert!(!mgr.is_held_by(200, 1));
}

#[test]
fn test_monitor_handoff_on_release() {
    let mut mgr = MonitorManager::new();
    mgr.acquire(100, 1);
    mgr.acquire(100, 2); // thread 2 blocked in entry set
    mgr.release(100, 1);
    // Monitor handed off to thread 2
    assert!(mgr.is_held_by(100, 2));
}

#[test]
fn test_monitor_contention_stats() {
    let mut mgr = MonitorManager::new();
    mgr.acquire(100, 1);
    mgr.acquire(100, 2); // contention
    let stats = mgr.get_contention_stats();
    assert_eq!(stats.total_monitors, 1);
    assert_eq!(stats.total_contentions, 1);
    assert_eq!(stats.currently_blocked, 1);
}

// -----------------------------------------------------------------------
// Volatile & Fence tests
// -----------------------------------------------------------------------

#[test]
fn test_volatile_load_i32() {
    let mut mgr = VolatileAccessManager::new();
    let val = mgr.volatile_load_i32(0x1000, 42);
    assert_eq!(val, 42);
    assert_eq!(mgr.access_count, 1);
}

#[test]
fn test_volatile_store_i32() {
    let mut mgr = VolatileAccessManager::new();
    mgr.volatile_store_i32(0x1000, 42);
    assert_eq!(mgr.access_count, 1);
}

#[test]
fn test_volatile_load_i64() {
    let mut mgr = VolatileAccessManager::new();
    let val = mgr.volatile_load_i64(0x2000, 123456789i64);
    assert_eq!(val, 123456789i64);
}

#[test]
fn test_volatile_store_i64() {
    let mut mgr = VolatileAccessManager::new();
    mgr.volatile_store_i64(0x2000, 99);
    assert_eq!(mgr.stores, 1);
}

#[test]
fn test_volatile_load_ref() {
    let mut mgr = VolatileAccessManager::new();
    let val = mgr.volatile_load_ref(0x3000, 0xCAFE);
    assert_eq!(val, 0xCAFE);
}

#[test]
fn test_volatile_store_ref() {
    let mut mgr = VolatileAccessManager::new();
    mgr.volatile_store_ref(0x3000, 0xDEAD);
    assert_eq!(mgr.stores, 1);
}

#[test]
fn test_full_fence() {
    let mut mgr = VolatileAccessManager::new();
    mgr.full_fence();
    assert_eq!(mgr.fence_count, 1);
}

#[test]
fn test_acquire_fence() {
    let mut mgr = VolatileAccessManager::new();
    mgr.acquire_fence();
    assert_eq!(mgr.fence_count, 1);
}

#[test]
fn test_release_fence() {
    let mut mgr = VolatileAccessManager::new();
    mgr.release_fence();
    assert_eq!(mgr.fence_count, 1);
}

#[test]
fn test_volatile_stats() {
    let mut mgr = VolatileAccessManager::new();
    mgr.volatile_load_i32(0, 1);
    mgr.volatile_store_i32(0, 2);
    mgr.full_fence();
    let stats = mgr.get_stats();
    assert_eq!(stats.loads, 1);
    assert_eq!(stats.stores, 1);
    assert_eq!(stats.fences, 1);
}

// -----------------------------------------------------------------------
// CAS tests
// -----------------------------------------------------------------------

#[test]
fn test_cas_i32_success() {
    let mut cas = CasManager::new();
    let r = cas.compare_and_swap_i32(10, 10, 20);
    assert!(r.success);
    assert_eq!(r.witness, 20);
    assert_eq!(cas.successes, 1);
}

#[test]
fn test_cas_i32_failure() {
    let mut cas = CasManager::new();
    let r = cas.compare_and_swap_i32(10, 5, 20);
    assert!(!r.success);
    assert_eq!(r.witness, 10);
}

#[test]
fn test_cas_i64_success() {
    let mut cas = CasManager::new();
    let r = cas.compare_and_swap_i64(100, 100, 200);
    assert!(r.success);
    assert_eq!(r.witness, 200);
}

#[test]
fn test_cas_i64_failure() {
    let mut cas = CasManager::new();
    let r = cas.compare_and_swap_i64(100, 50, 200);
    assert!(!r.success);
    assert_eq!(r.witness, 100);
}

#[test]
fn test_cas_ref_success() {
    let mut cas = CasManager::new();
    let r = cas.compare_and_swap_ref(0xABC, 0xABC, 0xDEF);
    assert!(r.success);
}

#[test]
fn test_cas_ref_failure() {
    let mut cas = CasManager::new();
    let r = cas.compare_and_swap_ref(0xABC, 0x123, 0xDEF);
    assert!(!r.success);
    assert_eq!(r.witness, 0xABC);
}

#[test]
fn test_get_and_set_i32() {
    let mut cas = CasManager::new();
    let old = cas.get_and_set_i32(42, 99);
    assert_eq!(old, 42);
    assert_eq!(cas.attempts, 1);
}

#[test]
fn test_get_and_add_i32() {
    let mut cas = CasManager::new();
    let old = cas.get_and_add_i32(10, 5);
    assert_eq!(old, 10);
}

#[test]
fn test_get_and_set_i64() {
    let mut cas = CasManager::new();
    let old = cas.get_and_set_i64(1000, 2000);
    assert_eq!(old, 1000);
}

#[test]
fn test_get_and_add_i64() {
    let mut cas = CasManager::new();
    let old = cas.get_and_add_i64(500, 100);
    assert_eq!(old, 500);
}

#[test]
fn test_cas_attempts_tracking() {
    let mut cas = CasManager::new();
    cas.compare_and_swap_i32(1, 1, 2);
    cas.compare_and_swap_i32(3, 1, 2);
    cas.get_and_set_i32(5, 6);
    assert_eq!(cas.attempts, 3);
    assert_eq!(cas.successes, 2); // first CAS + get_and_set
}

// -----------------------------------------------------------------------
// Scoped Value Binding tests
// -----------------------------------------------------------------------

#[test]
fn test_scoped_bind() {
    let mut sv = ScopedValueBindings::new();
    sv.bind(1, 0xABC);
    assert!(sv.is_bound(1));
    assert_eq!(sv.get(1), Some(0xABC));
}

#[test]
fn test_scoped_unbind() {
    let mut sv = ScopedValueBindings::new();
    sv.bind(1, 0xABC);
    assert!(sv.unbind(1));
    assert!(!sv.is_bound(1));
}

#[test]
fn test_scoped_unbind_not_bound() {
    let mut sv = ScopedValueBindings::new();
    assert!(!sv.unbind(999));
}

#[test]
fn test_scoped_get_not_bound() {
    let sv = ScopedValueBindings::new();
    assert_eq!(sv.get(1), None);
}

#[test]
fn test_scoped_snapshot() {
    let mut sv = ScopedValueBindings::new();
    sv.bind(1, 0xABC);
    sv.bind(2, 0xDEF);
    let child = sv.snapshot();
    assert_eq!(child.get(1), Some(0xABC));
    assert_eq!(child.get(2), Some(0xDEF));
    assert!(child.bindings[&1].inherited);
    assert!(child.bindings[&2].inherited);
    assert_eq!(child.snapshot_id, 1);
}

#[test]
fn test_scoped_snapshot_isolation() {
    let mut sv = ScopedValueBindings::new();
    sv.bind(1, 0xABC);
    let mut child = sv.snapshot();
    child.bind(3, 0x999);
    // Parent should not see child's binding
    assert!(!sv.is_bound(3));
    assert!(child.is_bound(3));
}

#[test]
fn test_scoped_binding_count() {
    let mut sv = ScopedValueBindings::new();
    assert_eq!(sv.binding_count(), 0);
    sv.bind(1, 100);
    sv.bind(2, 200);
    assert_eq!(sv.binding_count(), 2);
    sv.unbind(1);
    assert_eq!(sv.binding_count(), 1);
}

#[test]
fn test_scoped_bind_overwrite() {
    let mut sv = ScopedValueBindings::new();
    sv.bind(1, 100);
    sv.bind(1, 200);
    assert_eq!(sv.get(1), Some(200));
    assert_eq!(sv.binding_count(), 1);
}

#[test]
fn test_scoped_original_not_inherited() {
    let mut sv = ScopedValueBindings::new();
    sv.bind(1, 100);
    assert!(!sv.bindings[&1].inherited);
}
