use super::*;

// -- HappensBeforeEdge --------------------------------------------------

#[test]
fn hb_edge_all_variants_exist() {
    let variants = [
        HappensBeforeEdge::ProgramOrder,
        HappensBeforeEdge::MonitorUnlockToLock,
        HappensBeforeEdge::VolatileWriteToRead,
        HappensBeforeEdge::ThreadStart,
        HappensBeforeEdge::ThreadJoin,
        HappensBeforeEdge::ThreadInterrupt,
        HappensBeforeEdge::FinalizerStart,
        HappensBeforeEdge::DefaultValueWrite,
        HappensBeforeEdge::FinalFieldFreeze,
        HappensBeforeEdge::Transitive,
    ];
    assert_eq!(variants.len(), 10);
    // Each variant is distinct.
    for i in 0..variants.len() {
        for j in (i + 1)..variants.len() {
            assert_ne!(variants[i], variants[j]);
        }
    }
}

// -- MemoryActionType ---------------------------------------------------

#[test]
fn memory_action_type_all_variants() {
    let variants = [
        MemoryActionType::Read,
        MemoryActionType::Write,
        MemoryActionType::VolatileRead,
        MemoryActionType::VolatileWrite,
        MemoryActionType::MonitorEnter,
        MemoryActionType::MonitorExit,
        MemoryActionType::ThreadStart,
        MemoryActionType::ThreadJoin,
        MemoryActionType::FinalFieldFreeze,
    ];
    assert_eq!(variants.len(), 9);
}

// -- MemoryAction -------------------------------------------------------

#[test]
fn memory_action_creation_and_fields() {
    let action = MemoryAction {
        action_type: MemoryActionType::Write,
        thread_id: 42,
        timestamp: 10,
        variable_id: 0xDEAD,
        value: -1,
    };
    assert_eq!(action.thread_id, 42);
    assert_eq!(action.timestamp, 10);
    assert_eq!(action.variable_id, 0xDEAD);
    assert_eq!(action.value, -1);
    assert_eq!(action.action_type, MemoryActionType::Write);
}

// -- VolatileSemantics --------------------------------------------------

#[test]
fn volatile_read_fence_no_panic() {
    VolatileSemantics::volatile_read_fence();
}

#[test]
fn volatile_write_fence_no_panic() {
    VolatileSemantics::volatile_write_fence();
}

#[test]
fn volatile_full_fence_no_panic() {
    VolatileSemantics::full_fence();
}

#[test]
fn volatile_acquire_fence_no_panic() {
    VolatileSemantics::acquire_fence();
}

#[test]
fn volatile_release_fence_no_panic() {
    VolatileSemantics::release_fence();
}

#[test]
fn volatile_load_load_fence_no_panic() {
    VolatileSemantics::load_load_fence();
}

#[test]
fn volatile_store_store_fence_no_panic() {
    VolatileSemantics::store_store_fence();
}

// -- MonitorSemantics ---------------------------------------------------

#[test]
fn monitor_enter_exit_tracking() {
    let ms = MonitorSemantics::new();
    // No prior release: enter returns None.
    assert!(ms.on_monitor_enter(1, 100).is_none());
    // Release.
    ms.on_monitor_exit(1, 100, 5);
    // Now enter sees the previous release.
    let hb = ms.on_monitor_enter(1, 200);
    assert_eq!(hb, Some((100, 5)));
}

#[test]
fn monitor_last_release_returns_correct_info() {
    let ms = MonitorSemantics::new();
    assert!(ms.last_release(1).is_none());
    ms.on_monitor_exit(1, 42, 10);
    assert_eq!(ms.last_release(1), Some((42, 10)));
    // Overwrite with later release.
    ms.on_monitor_exit(1, 43, 20);
    assert_eq!(ms.last_release(1), Some((43, 20)));
}

#[test]
fn monitor_enter_after_exit_establishes_hb() {
    let ms = MonitorSemantics::new();
    ms.on_monitor_exit(5, 1, 100);
    let result = ms.on_monitor_enter(5, 2);
    assert_eq!(result, Some((1, 100)));
}

#[test]
fn monitor_different_monitors_independent() {
    let ms = MonitorSemantics::new();
    ms.on_monitor_exit(1, 10, 5);
    ms.on_monitor_exit(2, 20, 15);
    assert_eq!(ms.last_release(1), Some((10, 5)));
    assert_eq!(ms.last_release(2), Some((20, 15)));
    // Monitor 3 has no release.
    assert!(ms.last_release(3).is_none());
}

#[test]
fn monitor_clear() {
    let ms = MonitorSemantics::new();
    ms.on_monitor_exit(1, 10, 5);
    ms.clear();
    assert!(ms.last_release(1).is_none());
}

// -- FinalFieldSemantics ------------------------------------------------

#[test]
fn final_field_freeze_and_is_frozen() {
    let ff = FinalFieldSemantics::new();
    assert!(!ff.is_frozen(0x1000));
    ff.freeze(0x1000, 42);
    assert!(ff.is_frozen(0x1000));
}

#[test]
fn final_field_freeze_clock_returns_correct_value() {
    let ff = FinalFieldSemantics::new();
    assert!(ff.freeze_clock(0x2000).is_none());
    ff.freeze(0x2000, 99);
    assert_eq!(ff.freeze_clock(0x2000), Some(99));
}

#[test]
fn final_field_remove_dead_clears_dead_entries() {
    let ff = FinalFieldSemantics::new();
    ff.freeze(0x1000, 1);
    ff.freeze(0x2000, 2);
    ff.freeze(0x3000, 3);
    // Only 0x2000 is live.
    ff.remove_dead(&|addr| addr == 0x2000);
    assert!(!ff.is_frozen(0x1000));
    assert!(ff.is_frozen(0x2000));
    assert!(!ff.is_frozen(0x3000));
}

#[test]
fn final_field_update_after_gc_remaps_addresses() {
    let ff = FinalFieldSemantics::new();
    ff.freeze(0x1000, 10);
    ff.freeze(0x2000, 20);
    let mut map = HashMap::new();
    map.insert(0x1000, 0xA000);
    // 0x2000 not in map, stays the same.
    ff.update_after_gc(&map);
    assert!(ff.is_frozen(0xA000));
    assert_eq!(ff.freeze_clock(0xA000), Some(10));
    assert!(ff.is_frozen(0x2000));
    assert!(!ff.is_frozen(0x1000));
}

#[test]
fn final_field_clear() {
    let ff = FinalFieldSemantics::new();
    ff.freeze(0x1000, 1);
    ff.clear();
    assert!(!ff.is_frozen(0x1000));
}

// -- ThreadLifecycleSemantics -------------------------------------------

#[test]
fn thread_start_tracking() {
    let tls = ThreadLifecycleSemantics::new();
    tls.on_thread_start(1, 2, 10);
    // Started thread should have initial clock 0.
    assert_eq!(tls.get_clock(2), 0);
}

#[test]
fn thread_finish_records_clock() {
    let tls = ThreadLifecycleSemantics::new();
    tls.on_thread_finish(1, 50);
    assert_eq!(tls.on_thread_join(2, 1), Some(50));
}

#[test]
fn thread_join_returns_finished_clock() {
    let tls = ThreadLifecycleSemantics::new();
    // Thread not finished yet.
    assert!(tls.on_thread_join(2, 1).is_none());
    tls.on_thread_finish(1, 100);
    assert_eq!(tls.on_thread_join(2, 1), Some(100));
}

#[test]
fn thread_tick_increments_clock() {
    let tls = ThreadLifecycleSemantics::new();
    assert_eq!(tls.get_clock(1), 0);
    assert_eq!(tls.tick(1), 1);
    assert_eq!(tls.tick(1), 2);
    assert_eq!(tls.tick(1), 3);
    assert_eq!(tls.get_clock(1), 3);
}

#[test]
fn thread_happens_before_with_start() {
    let tls = ThreadLifecycleSemantics::new();
    tls.on_thread_start(1, 2, 5);
    // Thread 1 at clock 5 started thread 2, so hb holds.
    assert!(tls.happens_before(1, 5, 2));
    // Clock 6 is after the start event at 5, no hb.
    assert!(!tls.happens_before(1, 6, 2));
}

#[test]
fn thread_happens_before_same_thread() {
    let tls = ThreadLifecycleSemantics::new();
    // Same thread always has happens-before (program order).
    assert!(tls.happens_before(1, 0, 1));
    assert!(tls.happens_before(1, 999, 1));
}

#[test]
fn thread_interrupt_tracking() {
    let tls = ThreadLifecycleSemantics::new();
    tls.on_thread_interrupt(1, 2, 10);
    // Interrupt establishes hb from thread 1 at clock <= 10 to thread 2.
    assert!(tls.happens_before(1, 10, 2));
    assert!(!tls.happens_before(1, 11, 2));
}

// -- JmmManager ---------------------------------------------------------

#[test]
fn jmm_new_creates_enabled() {
    let jmm = JmmManager::new();
    assert!(jmm.is_enabled());
    assert_eq!(jmm.total_actions(), 0);
}

#[test]
fn jmm_disabled_creates_disabled() {
    let jmm = JmmManager::disabled();
    assert!(!jmm.is_enabled());
}

#[test]
fn jmm_enable_disable_toggle() {
    let jmm = JmmManager::new();
    assert!(jmm.is_enabled());
    jmm.set_enabled(false);
    assert!(!jmm.is_enabled());
    jmm.set_enabled(true);
    assert!(jmm.is_enabled());
}

#[test]
fn jmm_volatile_read_increments_counter() {
    let jmm = JmmManager::new();
    jmm.on_volatile_read(1, 0x100);
    assert_eq!(jmm.total_actions(), 1);
    jmm.on_volatile_read(1, 0x200);
    assert_eq!(jmm.total_actions(), 2);
}

#[test]
fn jmm_monitor_enter_exit_delegates() {
    let jmm = JmmManager::new();
    jmm.on_monitor_exit(1, 10);
    // After exit, monitor 10 should have a release recorded.
    assert!(jmm.monitors.last_release(10).is_some());
    jmm.on_monitor_enter(2, 10);
    assert_eq!(jmm.total_actions(), 2);
}

#[test]
fn jmm_thread_start_delegates() {
    let jmm = JmmManager::new();
    jmm.on_thread_start(1, 2);
    assert_eq!(jmm.total_actions(), 1);
    // Thread 2 should have clock 0 (just initialized).
    assert_eq!(jmm.thread_lifecycle.get_clock(2), 0);
}

#[test]
fn jmm_total_actions_tracks_count() {
    let jmm = JmmManager::new();
    jmm.on_volatile_read(1, 100);
    jmm.on_volatile_write(1, 100);
    jmm.on_monitor_enter(1, 1);
    jmm.on_monitor_exit(1, 1);
    jmm.on_thread_start(1, 2);
    jmm.on_thread_finish(2);
    jmm.on_thread_join(1, 2);
    jmm.on_constructor_complete(0x5000);
    assert_eq!(jmm.total_actions(), 8);
}

#[test]
fn jmm_gc_cleanup_delegates() {
    let jmm = JmmManager::new();
    jmm.on_constructor_complete(0x1000);
    jmm.on_constructor_complete(0x2000);
    assert!(jmm.final_fields.is_frozen(0x1000));
    assert!(jmm.final_fields.is_frozen(0x2000));
    let pointer_map = HashMap::new();
    // Only 0x2000 is live.
    jmm.gc_cleanup(&pointer_map, &|addr| addr == 0x2000);
    assert!(!jmm.final_fields.is_frozen(0x1000));
    assert!(jmm.final_fields.is_frozen(0x2000));
}

#[test]
fn jmm_disabled_skips_tracking() {
    let jmm = JmmManager::disabled();
    jmm.on_volatile_read(1, 100);
    jmm.on_monitor_enter(1, 1);
    jmm.on_thread_start(1, 2);
    // Nothing tracked.
    assert_eq!(jmm.total_actions(), 0);
}

// -- DataRaceDetector ---------------------------------------------------

#[test]
fn race_detector_no_race_same_thread() {
    let det = DataRaceDetector::new();
    det.record_access(100, 1, MemoryActionType::Write, 0);
    let race = det.record_access(100, 1, MemoryActionType::Write, 1);
    assert!(race.is_none());
}

#[test]
fn race_detector_write_write_different_threads() {
    let det = DataRaceDetector::new();
    det.record_access(100, 1, MemoryActionType::Write, 0);
    let race = det.record_access(100, 2, MemoryActionType::Write, 0);
    assert!(race.is_some());
    let r = race.unwrap();
    assert_eq!(r.variable_id, 100);
}

#[test]
fn race_detector_read_write_different_threads() {
    let det = DataRaceDetector::new();
    det.record_access(100, 1, MemoryActionType::Read, 0);
    let race = det.record_access(100, 2, MemoryActionType::Write, 0);
    assert!(race.is_some());
}

#[test]
fn race_detector_no_race_volatile() {
    let det = DataRaceDetector::new();
    det.record_access(100, 1, MemoryActionType::VolatileWrite, 0);
    let race = det.record_access(100, 2, MemoryActionType::VolatileRead, 1);
    assert!(race.is_none());
}

#[test]
fn race_detector_clear_races() {
    let det = DataRaceDetector::new();
    det.record_access(100, 1, MemoryActionType::Write, 0);
    det.record_access(100, 2, MemoryActionType::Write, 0);
    assert!(!det.detected_races().is_empty());
    det.clear_races();
    assert!(det.detected_races().is_empty());
}

#[test]
fn race_detector_disabled_no_detect() {
    let det = DataRaceDetector::new();
    det.set_enabled(false);
    det.record_access(100, 1, MemoryActionType::Write, 0);
    let race = det.record_access(100, 2, MemoryActionType::Write, 0);
    assert!(race.is_none());
    assert!(det.detected_races().is_empty());
}

#[test]
fn race_detector_no_race_read_read() {
    let det = DataRaceDetector::new();
    det.record_access(100, 1, MemoryActionType::Read, 0);
    let race = det.record_access(100, 2, MemoryActionType::Read, 1);
    assert!(race.is_none());
}

#[test]
fn race_detector_with_history() {
    let det = DataRaceDetector::with_history(2);
    det.record_access(100, 1, MemoryActionType::Write, 0);
    det.record_access(100, 1, MemoryActionType::Write, 1);
    det.record_access(100, 1, MemoryActionType::Write, 2);
    // History is 2, so first entry was evicted. Accesses from same thread
    // don't race anyway, but this tests the pruning path.
    assert!(det.detected_races().is_empty());
}
