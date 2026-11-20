use super::*;
use crate::recording::RecordingSettings;

#[test]
fn test_builtin_event_count() {
    let mut registry = EventTypeRegistry::new();
    register_builtin_events(&mut registry);
    assert!(
        registry.len() >= 20,
        "Expected at least 20 built-in events, got {}",
        registry.len()
    );
}

#[test]
fn test_builtin_exact_count_23() {
    let mut registry = EventTypeRegistry::new();
    register_builtin_events(&mut registry);
    // 48th: `cratonvm.JitCompileDecision` (2026-09-01). The test NAME is
    // historical and has been wrong since the count passed 23; the
    // assertion is what matters.
    // 49th: `jdk.GCPhaseConcurrent` (gen r5w2/obs6, 2026-09-26).
    // 50th: `jdk.GCPhasePauseLevel1` (gci i9/jc and zr z4/jf), 51st-55th: the ZGC events (zr z4/jf).
    assert_eq!(registry.len(), 55); // 28 original + 19 T6.2 + 1 cratonvm.* + GCPhaseConcurrent + 6 zr z4/jf
}

// --- GC events ---

#[test]
fn test_gc_event_fields() {
    let mut reg = EventTypeRegistry::new();
    register_builtin_events(&mut reg);
    let id = reg.find_by_name("jdk.GarbageCollection").unwrap();
    let et = reg.get(id).unwrap();
    assert_eq!(et.fields.len(), 5);
    assert_eq!(et.fields[0].name, "gcId");
    assert_eq!(et.fields[1].name, "name");
    assert_eq!(et.fields[2].name, "cause");
    assert!(et.has_thread);
    assert!(!et.has_stacktrace);
    assert!(et.threshold.is_some());
}

#[test]
fn test_gc_phase_pause_exists() {
    let mut reg = EventTypeRegistry::new();
    register_builtin_events(&mut reg);
    assert!(reg.find_by_name("jdk.GCPhasePause").is_some());
}

/// gci i9/jc: `jdk.GCPhasePauseLevel1` has HotSpot's shape (`gcId`,
/// `name`), is registered after every earlier type (none of their ids
/// moved), is enabled by both profiles, and an emitted row carries its
/// gcId, phase name and span through the recording.
#[test]
fn gci_i9_jc_gc_phase_pause_level1_shape_and_emit() {
    let mut reg = EventTypeRegistry::new();
    register_builtin_events(&mut reg);
    let id = reg.find_by_name("jdk.GCPhasePauseLevel1").unwrap();
    let et = reg.get(id).unwrap();
    assert_eq!(et.fields.len(), 2);
    assert_eq!(et.fields[0].name, "gcId");
    assert_eq!(et.fields[1].name, "name");
    assert!(et.category.contains(&"GC".to_string()));
    let conc = reg.find_by_name("jdk.GCPhaseConcurrent").unwrap();
    assert!(id.0 > conc.0, "registered last: no earlier type id moved");
    for profile in [JfrProfile::default_profile(), JfrProfile::detailed_profile()] {
        assert!(
            profile
                .settings
                .iter()
                .any(|s| s.event_name == "jdk.GCPhasePauseLevel1" && s.enabled),
            "{} enables jdk.GCPhasePauseLevel1",
            profile.name
        );
    }

    let mut fr = make_recorder();
    let rid = fr.new_recording(RecordingSettings::new("test"));
    let _g = drain_ring_baseline();
    fr.start_recording(rid);
    emit_gc_phase_pause_level1_event(&mut fr, 4, "cheney_drain", 3_000, 700);
    fr.drain_per_thread_into_repository();
    let level1 = fr.type_registry.find_by_name("jdk.GCPhasePauseLevel1");
    let rec = fr.get_recording_mut(rid).unwrap();
    assert_eq!(rec.event_count(), 1);
    let ev = &rec.get_events()[0];
    assert_eq!(Some(ev.type_id), level1);
    assert_eq!((ev.start_time, ev.end_time), (3_000, 3_700));
    assert!(matches!(ev.fields[0], EventValue::Int(4)));
    assert!(matches!(ev.fields[1], EventValue::Str("cheney_drain")));
}

/// gen r5w2/obs6: `jdk.GCPhaseConcurrent` has HotSpot's shape (`gcId`,
/// `name`, like `jdk.GCPhasePause`), is registered after every earlier
/// type (so none of their ids moved), and an emitted row carries its gcId,
/// phase name and wall span through the recording.
#[test]
fn test_gc_phase_concurrent_shape_and_emit() {
    let mut reg = EventTypeRegistry::new();
    register_builtin_events(&mut reg);
    let id = reg.find_by_name("jdk.GCPhaseConcurrent").unwrap();
    let et = reg.get(id).unwrap();
    assert_eq!(et.fields.len(), 2);
    assert_eq!(et.fields[0].name, "gcId");
    assert_eq!(et.fields[1].name, "name");
    assert!(et.category.contains(&"GC".to_string()));
    let jit = reg
        .find_by_name(crate::jit_decision::JIT_COMPILE_DECISION_EVENT)
        .unwrap();
    assert!(id.0 > jit.0, "registered last: no earlier type id moved");

    let mut fr = make_recorder();
    let rid = fr.new_recording(RecordingSettings::new("test"));
    let _g = drain_ring_baseline();
    fr.start_recording(rid);
    emit_gc_phase_concurrent_event(&mut fr, 7, "Concurrent Mark", 1_000, 500);
    fr.drain_per_thread_into_repository();
    let rec = fr.get_recording_mut(rid).unwrap();
    assert_eq!(rec.event_count(), 1);
    let ev = &rec.get_events()[0];
    assert_eq!((ev.start_time, ev.end_time), (1_000, 1_500));
    assert!(matches!(ev.fields[0], EventValue::Int(7)));
    assert!(matches!(ev.fields[1], EventValue::Str("Concurrent Mark")));
}

/// gen r5w2/obs6: a `jdk.GCPhasePause` row emitted with a type resolved in
/// advance lands in the recording exactly like one emitted through the
/// recorder, with the declared field shape.
#[test]
fn test_typed_gc_phase_pause_emit_matches_the_declared_shape() {
    let mut fr = make_recorder();
    let rid = fr.new_recording(RecordingSettings::new("test"));
    let _g = drain_ring_baseline();
    fr.start_recording(rid);
    let ty = gc_phase_pause_type(&fr).expect("jdk.GCPhasePause is registered");
    assert_eq!(Some(ty), fr.type_registry.find_by_name("jdk.GCPhasePause"));
    emit_gc_phase_pause_event_typed(ty, 3, "Pause Remark", 2_000, 250);
    fr.drain_per_thread_into_repository();
    {
        let rec = fr.get_recording_mut(rid).unwrap();
        assert_eq!(rec.event_count(), 1);
        let ev = rec.get_events()[0].clone();
        assert_eq!(ev.type_id, ty);
        assert_eq!((ev.start_time, ev.end_time), (2_000, 2_250));
        assert!(matches!(ev.fields[1], EventValue::Str("Pause Remark")));
    }
    #[cfg(debug_assertions)]
    {
        let ev = fr.get_recording_mut(rid).unwrap().get_events()[0].clone();
        assert!(validate_builtin_field_shape(&fr, ev.type_id, &ev.fields).is_ok());
    }
}

#[test]
fn test_young_gc_exists() {
    let mut reg = EventTypeRegistry::new();
    register_builtin_events(&mut reg);
    let id = reg.find_by_name("jdk.YoungGarbageCollection").unwrap();
    let et = reg.get(id).unwrap();
    assert_eq!(et.fields.len(), 2);
}

#[test]
fn test_old_gc_exists() {
    let mut reg = EventTypeRegistry::new();
    register_builtin_events(&mut reg);
    let id = reg.find_by_name("jdk.OldGarbageCollection").unwrap();
    let et = reg.get(id).unwrap();
    assert_eq!(et.fields.len(), 1);
}

#[test]
fn test_gc_heap_summary() {
    let mut reg = EventTypeRegistry::new();
    register_builtin_events(&mut reg);
    let id = reg.find_by_name("jdk.GCHeapSummary").unwrap();
    let et = reg.get(id).unwrap();
    assert_eq!(et.fields.len(), 6);
    assert!(!et.has_thread);
}

#[test]
fn test_metaspace_summary() {
    let mut reg = EventTypeRegistry::new();
    register_builtin_events(&mut reg);
    let id = reg.find_by_name("jdk.MetaspaceSummary").unwrap();
    let et = reg.get(id).unwrap();
    assert_eq!(et.fields.len(), 5);
}

// --- Thread events ---

#[test]
fn test_thread_start_fields() {
    let mut reg = EventTypeRegistry::new();
    register_builtin_events(&mut reg);
    let id = reg.find_by_name("jdk.ThreadStart").unwrap();
    let et = reg.get(id).unwrap();
    assert_eq!(et.fields.len(), 2);
    assert!(et.has_thread);
    assert!(et.has_stacktrace);
    assert!(et.threshold.is_none());
}

#[test]
fn test_thread_end_fields() {
    let mut reg = EventTypeRegistry::new();
    register_builtin_events(&mut reg);
    let id = reg.find_by_name("jdk.ThreadEnd").unwrap();
    let et = reg.get(id).unwrap();
    assert_eq!(et.fields.len(), 1);
    assert!(!et.has_stacktrace);
}

#[test]
fn test_thread_sleep() {
    let mut reg = EventTypeRegistry::new();
    register_builtin_events(&mut reg);
    let id = reg.find_by_name("jdk.ThreadSleep").unwrap();
    let et = reg.get(id).unwrap();
    assert_eq!(et.threshold, Some(Duration::from_millis(10)));
}

#[test]
fn test_thread_park() {
    let mut reg = EventTypeRegistry::new();
    register_builtin_events(&mut reg);
    let id = reg.find_by_name("jdk.ThreadPark").unwrap();
    let et = reg.get(id).unwrap();
    assert_eq!(et.fields.len(), 3);
}

#[test]
fn test_java_monitor_wait() {
    let mut reg = EventTypeRegistry::new();
    register_builtin_events(&mut reg);
    let id = reg.find_by_name("jdk.JavaMonitorWait").unwrap();
    let et = reg.get(id).unwrap();
    assert_eq!(et.fields.len(), 5);
}

#[test]
fn test_java_monitor_enter() {
    let mut reg = EventTypeRegistry::new();
    register_builtin_events(&mut reg);
    let id = reg.find_by_name("jdk.JavaMonitorEnter").unwrap();
    let et = reg.get(id).unwrap();
    assert_eq!(et.fields.len(), 3);
}

#[test]
fn test_java_thread_statistics() {
    let mut reg = EventTypeRegistry::new();
    register_builtin_events(&mut reg);
    let id = reg.find_by_name("jdk.JavaThreadStatistics").unwrap();
    let et = reg.get(id).unwrap();
    assert_eq!(et.fields.len(), 4);
    assert!(!et.has_thread);
}

// --- Class loading events ---

#[test]
fn test_class_load() {
    let mut reg = EventTypeRegistry::new();
    register_builtin_events(&mut reg);
    let id = reg.find_by_name("jdk.ClassLoad").unwrap();
    let et = reg.get(id).unwrap();
    assert_eq!(et.fields.len(), 3);
    assert!(et.has_stacktrace);
}

#[test]
fn test_class_unload() {
    let mut reg = EventTypeRegistry::new();
    register_builtin_events(&mut reg);
    let id = reg.find_by_name("jdk.ClassUnload").unwrap();
    let et = reg.get(id).unwrap();
    assert_eq!(et.fields.len(), 2);
    assert!(!et.has_stacktrace);
    assert!(et.threshold.is_none());
}

// --- Compiler events ---

#[test]
fn test_compilation() {
    let mut reg = EventTypeRegistry::new();
    register_builtin_events(&mut reg);
    let id = reg.find_by_name("jdk.Compilation").unwrap();
    let et = reg.get(id).unwrap();
    assert_eq!(et.fields.len(), 7);
    assert_eq!(et.threshold, Some(Duration::from_millis(100)));
}

// --- Allocation events ---

#[test]
fn test_allocation_in_new_tlab() {
    let mut reg = EventTypeRegistry::new();
    register_builtin_events(&mut reg);
    let id = reg.find_by_name("jdk.ObjectAllocationInNewTLAB").unwrap();
    let et = reg.get(id).unwrap();
    assert_eq!(et.fields.len(), 3);
    assert!(et.has_stacktrace);
}

#[test]
fn test_allocation_outside_tlab() {
    let mut reg = EventTypeRegistry::new();
    register_builtin_events(&mut reg);
    let id = reg.find_by_name("jdk.ObjectAllocationOutsideTLAB").unwrap();
    let et = reg.get(id).unwrap();
    assert_eq!(et.fields.len(), 2);
}

// --- Profiling events ---

#[test]
fn test_execution_sample() {
    let mut reg = EventTypeRegistry::new();
    register_builtin_events(&mut reg);
    let id = reg.find_by_name("jdk.ExecutionSample").unwrap();
    let et = reg.get(id).unwrap();
    assert_eq!(et.fields.len(), 3);
}

#[test]
fn test_cpu_load() {
    let mut reg = EventTypeRegistry::new();
    register_builtin_events(&mut reg);
    let id = reg.find_by_name("jdk.CPULoad").unwrap();
    let et = reg.get(id).unwrap();
    assert_eq!(et.fields.len(), 3);
    assert!(!et.has_thread);
}

// --- Flight Recorder events ---

#[test]
fn test_active_recording() {
    let mut reg = EventTypeRegistry::new();
    register_builtin_events(&mut reg);
    let id = reg.find_by_name("jdk.ActiveRecording").unwrap();
    let et = reg.get(id).unwrap();
    assert_eq!(et.fields.len(), 5);
    assert_eq!(et.category, vec!["Flight Recorder"]);
}

#[test]
fn test_active_setting() {
    let mut reg = EventTypeRegistry::new();
    register_builtin_events(&mut reg);
    let id = reg.find_by_name("jdk.ActiveSetting").unwrap();
    let et = reg.get(id).unwrap();
    assert_eq!(et.fields.len(), 3);
}

// --- Category checks ---

#[test]
fn test_gc_events_have_gc_category() {
    let mut reg = EventTypeRegistry::new();
    register_builtin_events(&mut reg);
    let gc_names = [
        "jdk.GarbageCollection",
        "jdk.GCPhasePause",
        "jdk.YoungGarbageCollection",
        "jdk.OldGarbageCollection",
    ];
    for name in &gc_names {
        let id = reg.find_by_name(name).unwrap();
        let et = reg.get(id).unwrap();
        assert!(
            et.category.contains(&"GC".to_string()),
            "{} should have GC category",
            name
        );
    }
}

#[test]
fn test_thread_events_have_threading_category() {
    let mut reg = EventTypeRegistry::new();
    register_builtin_events(&mut reg);
    let thread_names = [
        "jdk.ThreadStart",
        "jdk.ThreadEnd",
        "jdk.ThreadSleep",
        "jdk.ThreadPark",
    ];
    for name in &thread_names {
        let id = reg.find_by_name(name).unwrap();
        let et = reg.get(id).unwrap();
        assert!(
            et.category.contains(&"Threading".to_string()),
            "{} should have Threading category",
            name
        );
    }
}

#[test]
fn test_all_builtin_ids_unique() {
    let mut reg = EventTypeRegistry::new();
    register_builtin_events(&mut reg);
    let ids: Vec<EventTypeId> = reg.iter().map(|(id, _)| *id).collect();
    let unique: std::collections::HashSet<_> = ids.iter().collect();
    assert_eq!(
        ids.len(),
        unique.len(),
        "All builtin event IDs should be unique"
    );
}

#[test]
fn test_all_builtin_names_unique() {
    let mut reg = EventTypeRegistry::new();
    register_builtin_events(&mut reg);
    let names: Vec<&str> = reg.iter().map(|(_, et)| et.name.as_str()).collect();
    let unique: std::collections::HashSet<_> = names.iter().collect();
    assert_eq!(
        names.len(),
        unique.len(),
        "All builtin event names should be unique"
    );
}

// --- Emission function tests ---
//
// Round-4/5 per-thread-ring contract (C33): `emit_*` no longer writes
// directly into the recording's `EventRepository`. Events land in the
// calling thread's `SpscEventRing` shard and are not visible to
// `Recording::event_count()` / `get_events()` until the test calls
// `FlightRecorder::drain_per_thread_into_repository()`.
//
// The pattern below in every test is:
//   1. Drain the global ring once to clear any stragglers pushed by
//      other tests running in parallel on this thread or others.
//   2. Emit the event under test.
//   3. Drain into the recording's repository.
//   4. Assert on `event_count()` / `get_events()`.

fn make_recorder() -> FlightRecorder {
    let mut fr = FlightRecorder::new();
    register_builtin_events(&mut fr.type_registry);
    fr
}

/// Acquire the process-wide test serialization lock and discard any events
/// left in the per-thread ring registry before the test under load runs.
///
/// The returned [`MutexGuard`](parking_lot::MutexGuard) must be held for
/// the whole test (`let _g = drain_ring_baseline();`): it serializes this
/// test against every other test that emits into / drains the global ring,
/// so a concurrent `drain_all()` cannot steal events and break exact
/// `event_count() == N` assertions. The baseline drain then clears any
/// stragglers left on this thread's shard by earlier tests.
#[must_use]
fn drain_ring_baseline() -> parking_lot::MutexGuard<'static, ()> {
    let guard = crate::repository::jfr_test_guard();
    let _ = crate::repository::global_ring_registry().drain_all();
    guard
}

#[test]
fn test_emit_gc_event() {
    let mut fr = make_recorder();
    let rid = fr.new_recording(RecordingSettings::new("test"));
    let _g = drain_ring_baseline();
    fr.start_recording(rid);
    emit_gc_event(&mut fr, 1, "G1 Young", "Allocation Failure", 1000, 500);
    fr.drain_per_thread_into_repository();
    let rec = fr.get_recording_mut(rid).unwrap();
    assert_eq!(rec.event_count(), 1);
}

#[test]
fn test_builtin_event_id_cache_is_registry_keyed() {
    let _g = drain_ring_baseline();

    let mut first = make_recorder();
    let first_gc_id = first
        .type_registry
        .find_by_name("jdk.GarbageCollection")
        .unwrap();
    let first_rid = first.new_recording(RecordingSettings::new("first"));
    first.start_recording(first_rid);
    emit_gc_event(&mut first, 1, "G1 Young", "Allocation Failure", 1000, 500);
    first.drain_per_thread_into_repository();
    {
        let rec = first.get_recording_mut(first_rid).unwrap();
        assert_eq!(rec.event_count(), 1);
        assert_eq!(rec.get_events()[0].type_id, first_gc_id);
    }
    first.stop_recording(first_rid);

    let mut second = FlightRecorder::new();
    let custom_id = register_custom_event(
        &mut second.type_registry,
        "test.BeforeBuiltins",
        &["Test"],
        "pre-registered custom event",
        &[],
        true,
        false,
        None,
    );
    assert_eq!(custom_id, EventTypeId(1));
    register_builtin_events(&mut second.type_registry);
    let second_gc_id = second
        .type_registry
        .find_by_name("jdk.GarbageCollection")
        .unwrap();
    assert_ne!(first_gc_id, second_gc_id);

    let second_rid = second.new_recording(RecordingSettings::new("second"));
    second.start_recording(second_rid);
    emit_gc_event(&mut second, 2, "G1 Young", "Allocation Failure", 2000, 500);
    second.drain_per_thread_into_repository();
    let rec = second.get_recording_mut(second_rid).unwrap();
    assert_eq!(rec.event_count(), 1);
    assert_eq!(rec.get_events()[0].type_id, second_gc_id);
}

#[test]
fn test_emit_class_load_event() {
    let mut fr = make_recorder();
    let rid = fr.new_recording(RecordingSettings::new("test"));
    let _g = drain_ring_baseline();
    fr.start_recording(rid);
    emit_class_load_event(
        &mut fr,
        "java/lang/Object",
        "bootstrap",
        "bootstrap",
        2000,
        100,
    );
    fr.drain_per_thread_into_repository();
    let rec = fr.get_recording_mut(rid).unwrap();
    assert_eq!(rec.event_count(), 1);
}

#[test]
fn test_emit_thread_start_event() {
    let mut fr = make_recorder();
    let rid = fr.new_recording(RecordingSettings::new("test"));
    let _g = drain_ring_baseline();
    fr.start_recording(rid);
    emit_thread_start_event(&mut fr, "main", "", 1, 3000);
    fr.drain_per_thread_into_repository();
    let rec = fr.get_recording_mut(rid).unwrap();
    assert_eq!(rec.event_count(), 1);
}

#[test]
fn test_emit_compilation_event() {
    let mut fr = make_recorder();
    let rid = fr.new_recording(RecordingSettings::new("test"));
    let _g = drain_ring_baseline();
    fr.start_recording(rid);
    emit_compilation_event(
        &mut fr,
        "java/lang/String.hashCode:()I",
        1,
        3,
        true,
        false,
        256,
        128,
        4000,
        200,
    );
    fr.drain_per_thread_into_repository();
    let rec = fr.get_recording_mut(rid).unwrap();
    assert_eq!(rec.event_count(), 1);
}

#[test]
fn test_emit_allocation_in_new_tlab_event() {
    let mut fr = make_recorder();
    let rid = fr.new_recording(RecordingSettings::new("test"));
    let _g = drain_ring_baseline();
    fr.start_recording(rid);
    emit_allocation_in_new_tlab_event(&mut fr, "java/lang/Object", 64, 4096, 1, 5000);
    fr.drain_per_thread_into_repository();
    let rec = fr.get_recording_mut(rid).unwrap();
    assert_eq!(rec.event_count(), 1);
}

#[test]
fn test_emit_event_not_recorded_when_stopped() {
    let mut fr = make_recorder();
    let rid = fr.new_recording(RecordingSettings::new("test"));
    // Do NOT start the recording. With no running recording in this
    // FlightRecorder, `drain_per_thread_into_repository` discards every
    // drained event (regardless of which other test pushed it onto the
    // global ring), so the assertion remains exact.
    let _g = drain_ring_baseline();
    emit_gc_event(&mut fr, 1, "G1", "Test", 1000, 500);
    fr.drain_per_thread_into_repository();
    let rec = fr.get_recording_mut(rid).unwrap();
    assert_eq!(rec.event_count(), 0);
}

// --- New emission function tests (M26 fixes) ---

#[test]
fn test_emit_thread_end_event() {
    let mut fr = make_recorder();
    let rid = fr.new_recording(RecordingSettings::new("test"));
    let _g = drain_ring_baseline();
    fr.start_recording(rid);
    emit_thread_end_event(&mut fr, "worker-1", 42, 6000);
    fr.drain_per_thread_into_repository();
    let rec = fr.get_recording_mut(rid).unwrap();
    assert_eq!(rec.event_count(), 1);
}

#[test]
fn test_emit_thread_sleep_event() {
    let mut fr = make_recorder();
    let rid = fr.new_recording(RecordingSettings::new("test"));
    let _g = drain_ring_baseline();
    fr.start_recording(rid);
    emit_thread_sleep_event(&mut fr, 1_000_000, 1, 7000, 1_000_000);
    fr.drain_per_thread_into_repository();
    let rec = fr.get_recording_mut(rid).unwrap();
    assert_eq!(rec.event_count(), 1);
}

#[test]
fn test_emit_monitor_wait_event() {
    let mut fr = make_recorder();
    let rid = fr.new_recording(RecordingSettings::new("test"));
    let _g = drain_ring_baseline();
    fr.start_recording(rid);
    emit_monitor_wait_event(
        &mut fr,
        "java/lang/Object",
        "main",
        0,
        false,
        0xDEAD,
        1,
        8000,
        500,
    );
    fr.drain_per_thread_into_repository();
    let rec = fr.get_recording_mut(rid).unwrap();
    assert_eq!(rec.event_count(), 1);
}

#[test]
fn test_emit_monitor_enter_event() {
    let mut fr = make_recorder();
    let rid = fr.new_recording(RecordingSettings::new("test"));
    let _g = drain_ring_baseline();
    fr.start_recording(rid);
    emit_monitor_enter_event(&mut fr, "java/util/HashMap", "main", 0xBEEF, 1, 9000, 200);
    fr.drain_per_thread_into_repository();
    let rec = fr.get_recording_mut(rid).unwrap();
    assert_eq!(rec.event_count(), 1);
}

#[test]
fn test_emit_class_unload_event() {
    let mut fr = make_recorder();
    let rid = fr.new_recording(RecordingSettings::new("test"));
    let _g = drain_ring_baseline();
    fr.start_recording(rid);
    emit_class_unload_event(&mut fr, "com/example/OldClass", "app", 10000);
    fr.drain_per_thread_into_repository();
    let rec = fr.get_recording_mut(rid).unwrap();
    assert_eq!(rec.event_count(), 1);
}

#[test]
fn test_emit_thread_park_event() {
    let mut fr = make_recorder();
    let rid = fr.new_recording(RecordingSettings::new("test"));
    let _g = drain_ring_baseline();
    fr.start_recording(rid);
    emit_thread_park_event(
        &mut fr,
        "java/util/concurrent/locks/AQS",
        0,
        0xCAFE,
        1,
        11000,
        300,
    );
    fr.drain_per_thread_into_repository();
    let rec = fr.get_recording_mut(rid).unwrap();
    assert_eq!(rec.event_count(), 1);
}

#[test]
fn test_emit_gc_heap_summary_event() {
    let mut fr = make_recorder();
    let rid = fr.new_recording(RecordingSettings::new("test"));
    let _g = drain_ring_baseline();
    fr.start_recording(rid);
    emit_gc_heap_summary_event(
        &mut fr,
        1,
        "Before GC",
        "G1 Eden",
        1024 * 1024,
        512 * 1024,
        2048 * 1024,
        12000,
    );
    fr.drain_per_thread_into_repository();
    let rec = fr.get_recording_mut(rid).unwrap();
    assert_eq!(rec.event_count(), 1);
}

#[test]
fn test_emit_thread_sleep_saturating_add() {
    let mut fr = make_recorder();
    let rid = fr.new_recording(RecordingSettings::new("test"));
    let _g = drain_ring_baseline();
    fr.start_recording(rid);
    // Use u64::MAX to test saturating_add doesn't overflow
    emit_thread_sleep_event(&mut fr, i64::MAX, 1, u64::MAX - 10, u64::MAX);
    fr.drain_per_thread_into_repository();
    let rec = fr.get_recording_mut(rid).unwrap();
    assert_eq!(rec.event_count(), 1);
}

// S41 — JFR Event Completeness

#[test]
fn s41_builtin_event_count_47() {
    let mut registry = EventTypeRegistry::new();
    register_builtin_events(&mut registry);
    // 47 `jdk.*` + `cratonvm.JitCompileDecision` + `jdk.GCPhaseConcurrent`
    // (gen r5w2/obs6) + `jdk.GCPhasePauseLevel1` (gci i9/jc, zr z4/jf) + 5
    // ZGC events (zr z4/jf). The name is kept at 47 because
    // `docs/observability/phase-accounting.md` cites these tests by name.
    assert_eq!(registry.len(), 55);
}

#[test]
fn s41_new_event_types_registered() {
    let mut registry = EventTypeRegistry::new();
    register_builtin_events(&mut registry);
    assert!(registry.find_by_name("jdk.Deoptimization").is_some());
    assert!(registry.find_by_name("jdk.FileRead").is_some());
    assert!(registry.find_by_name("jdk.FileWrite").is_some());
    assert!(registry.find_by_name("jdk.SocketRead").is_some());
    assert!(registry.find_by_name("jdk.SocketWrite").is_some());
}

#[test]
fn s41_emit_deoptimization_event() {
    let mut fr = make_recorder();
    let rid = fr.new_recording(RecordingSettings::new("test"));
    let _g = drain_ring_baseline();
    fr.start_recording(rid);
    emit_deoptimization_event(
        &mut fr,
        "com/example/Foo.bar:()V",
        1,
        "NullCheck",
        "Reinterpret",
        42,
        1,
        10000,
    );
    fr.drain_per_thread_into_repository();
    let rec = fr.get_recording_mut(rid).unwrap();
    // With cross-test parallelism we may pick up stragglers; find our
    // event by its unique method-name marker rather than asserting an
    // exact count.
    let events = rec.get_events();
    let ev = events
        .iter()
        .find(|e| match e.fields.first() {
            Some(EventValue::String(s)) => s.contains("Foo"),
            _ => false,
        })
        .expect("expected deoptimization event with 'Foo' method to be drained");
    assert_eq!(ev.fields.len(), 5);
}

#[test]
fn s41_emit_file_read_event() {
    let mut fr = make_recorder();
    let rid = fr.new_recording(RecordingSettings::new("test"));
    let _g = drain_ring_baseline();
    fr.start_recording(rid);
    emit_file_read_event(&mut fr, "fd:3", 1024, false, 1, 10000, 500);
    fr.drain_per_thread_into_repository();
    let rec = fr.get_recording_mut(rid).unwrap();
    assert_eq!(rec.event_count(), 1);
    let events = rec.get_events();
    assert_eq!(events[0].fields.len(), 3);
}

#[test]
fn s41_emit_file_write_event() {
    let mut fr = make_recorder();
    let rid = fr.new_recording(RecordingSettings::new("test"));
    let _g = drain_ring_baseline();
    fr.start_recording(rid);
    emit_file_write_event(&mut fr, "fd:1", 512, 1, 10000, 300);
    fr.drain_per_thread_into_repository();
    let rec = fr.get_recording_mut(rid).unwrap();
    assert_eq!(rec.event_count(), 1);
}

#[test]
fn s41_emit_socket_read_event() {
    let mut fr = make_recorder();
    let rid = fr.new_recording(RecordingSettings::new("test"));
    let _g = drain_ring_baseline();
    fr.start_recording(rid);
    emit_socket_read_event(&mut fr, "localhost", 8080, 256, false, 1, 10000, 2000);
    fr.drain_per_thread_into_repository();
    let rec = fr.get_recording_mut(rid).unwrap();
    assert_eq!(rec.event_count(), 1);
    let events = rec.get_events();
    assert_eq!(events[0].fields.len(), 4);
}

#[test]
fn s41_emit_socket_write_event() {
    let mut fr = make_recorder();
    let rid = fr.new_recording(RecordingSettings::new("test"));
    let _g = drain_ring_baseline();
    fr.start_recording(rid);
    emit_socket_write_event(&mut fr, "example.com", 443, 128, 1, 10000, 1000);
    fr.drain_per_thread_into_repository();
    let rec = fr.get_recording_mut(rid).unwrap();
    assert_eq!(rec.event_count(), 1);
}

#[test]
fn s41_emit_gc_phase_pause_event() {
    let mut fr = make_recorder();
    let rid = fr.new_recording(RecordingSettings::new("test"));
    let _g = drain_ring_baseline();
    fr.start_recording(rid);
    emit_gc_phase_pause_event(&mut fr, 1, "Mark", 10000, 5000);
    fr.drain_per_thread_into_repository();
    let rec = fr.get_recording_mut(rid).unwrap();
    assert_eq!(rec.event_count(), 1);
}

#[test]
fn s41_emit_young_gc_event() {
    let mut fr = make_recorder();
    let rid = fr.new_recording(RecordingSettings::new("test"));
    let _g = drain_ring_baseline();
    fr.start_recording(rid);
    emit_young_gc_event(&mut fr, 1, 15, 10000, 3000);
    fr.drain_per_thread_into_repository();
    let rec = fr.get_recording_mut(rid).unwrap();
    assert_eq!(rec.event_count(), 1);
}

#[test]
fn s41_emit_old_gc_event() {
    let mut fr = make_recorder();
    let rid = fr.new_recording(RecordingSettings::new("test"));
    let _g = drain_ring_baseline();
    fr.start_recording(rid);
    emit_old_gc_event(&mut fr, 2, 20000, 10000);
    fr.drain_per_thread_into_repository();
    let rec = fr.get_recording_mut(rid).unwrap();
    assert_eq!(rec.event_count(), 1);
}

#[test]
fn s41_emit_metaspace_summary_event() {
    let mut fr = make_recorder();
    let rid = fr.new_recording(RecordingSettings::new("test"));
    let _g = drain_ring_baseline();
    fr.start_recording(rid);
    emit_metaspace_summary_event(&mut fr, 1, "After GC", 1024, 2048, 4096, 15000);
    fr.drain_per_thread_into_repository();
    let rec = fr.get_recording_mut(rid).unwrap();
    assert_eq!(rec.event_count(), 1);
}

#[test]
fn s41_emit_allocation_outside_tlab_event() {
    let mut fr = make_recorder();
    let rid = fr.new_recording(RecordingSettings::new("test"));
    let _g = drain_ring_baseline();
    fr.start_recording(rid);
    emit_allocation_outside_tlab_event(&mut fr, "java/lang/Object", 64, 1, 10000);
    fr.drain_per_thread_into_repository();
    let rec = fr.get_recording_mut(rid).unwrap();
    assert_eq!(rec.event_count(), 1);
}

#[test]
fn s41_emit_execution_sample_event() {
    let mut fr = make_recorder();
    let rid = fr.new_recording(RecordingSettings::new("test"));
    let _g = drain_ring_baseline();
    fr.start_recording(rid);
    emit_execution_sample_event(&mut fr, "main", "Main.run:5", "RUNNABLE", 1, 10000);
    fr.drain_per_thread_into_repository();
    let rec = fr.get_recording_mut(rid).unwrap();
    assert_eq!(rec.event_count(), 1);
}

#[test]
fn s41_emit_cpu_load_event() {
    let mut fr = make_recorder();
    let rid = fr.new_recording(RecordingSettings::new("test"));
    let _g = drain_ring_baseline();
    fr.start_recording(rid);
    emit_cpu_load_event(&mut fr, 0.25, 0.05, 0.60, 10000);
    fr.drain_per_thread_into_repository();
    let rec = fr.get_recording_mut(rid).unwrap();
    // Find our event by the distinctive 0.25 jvmUser field; cross-test
    // emits of jdk.CPULoad with other values are possible.
    let events = rec.get_events();
    let found = events.iter().any(|e| match e.fields.first() {
        Some(EventValue::Float(v)) => (v - 0.25).abs() < 0.001,
        _ => false,
    });
    assert!(
        found,
        "expected cpu_load event with 0.25 in field[0] to be drained"
    );
}

#[test]
fn s41_emit_thread_statistics_event() {
    let mut fr = make_recorder();
    let rid = fr.new_recording(RecordingSettings::new("test"));
    let _g = drain_ring_baseline();
    fr.start_recording(rid);
    emit_thread_statistics_event(&mut fr, 10, 3, 50, 12, 10000);
    fr.drain_per_thread_into_repository();
    let rec = fr.get_recording_mut(rid).unwrap();
    assert_eq!(rec.event_count(), 1);
}

#[test]
fn s41_emit_active_recording_event() {
    let mut fr = make_recorder();
    let rid = fr.new_recording(RecordingSettings::new("test"));
    let _g = drain_ring_baseline();
    fr.start_recording(rid);
    emit_active_recording_event(&mut fr, 1, "default", "/tmp/rec.jfr", 0, 0, 10000);
    fr.drain_per_thread_into_repository();
    let rec = fr.get_recording_mut(rid).unwrap();
    assert_eq!(rec.event_count(), 1);
}

#[test]
fn s41_emit_active_setting_event() {
    let mut fr = make_recorder();
    let rid = fr.new_recording(RecordingSettings::new("test"));
    let _g = drain_ring_baseline();
    fr.start_recording(rid);
    emit_active_setting_event(&mut fr, 1, "threshold", "10ms", 10000);
    fr.drain_per_thread_into_repository();
    let rec = fr.get_recording_mut(rid).unwrap();
    assert_eq!(rec.event_count(), 1);
}

#[test]
fn s41_deopt_event_has_correct_fields() {
    let fr = crate::create_flight_recorder();
    let et = fr.type_registry.find_by_name("jdk.Deoptimization").unwrap();
    let event_type = fr.type_registry.get(et).unwrap();
    assert_eq!(event_type.fields.len(), 5);
    assert_eq!(event_type.fields[0].name, "method");
    assert_eq!(event_type.fields[2].name, "reason");
    assert_eq!(event_type.fields[4].name, "bci");
}

#[test]
fn s41_file_io_events_have_threshold() {
    let fr = crate::create_flight_recorder();
    let read_id = fr.type_registry.find_by_name("jdk.FileRead").unwrap();
    let read_type = fr.type_registry.get(read_id).unwrap();
    assert!(read_type.threshold.is_some());
    assert_eq!(read_type.threshold.unwrap(), Duration::from_millis(10));

    let write_id = fr.type_registry.find_by_name("jdk.FileWrite").unwrap();
    let write_type = fr.type_registry.get(write_id).unwrap();
    assert!(write_type.threshold.is_some());
}

#[test]
fn s41_socket_events_have_correct_fields() {
    let fr = crate::create_flight_recorder();
    let read_id = fr.type_registry.find_by_name("jdk.SocketRead").unwrap();
    let read_type = fr.type_registry.get(read_id).unwrap();
    assert_eq!(read_type.fields.len(), 4);
    assert_eq!(read_type.fields[0].name, "host");
    assert_eq!(read_type.fields[1].name, "port");
    assert_eq!(read_type.fields[2].name, "bytesRead");
    assert_eq!(read_type.fields[3].name, "endOfStream");
}

// --- T6.2 new events ---

#[test]
fn t6_safepoint_events_exist() {
    let fr = crate::create_flight_recorder();
    assert!(fr
        .type_registry
        .find_by_name("jdk.SafepointBegin")
        .is_some());
    assert!(fr.type_registry.find_by_name("jdk.SafepointEnd").is_some());
}

#[test]
fn t6_allocation_sample_exists() {
    let fr = crate::create_flight_recorder();
    let id = fr
        .type_registry
        .find_by_name("jdk.ObjectAllocationSample")
        .unwrap();
    let et = fr.type_registry.get(id).unwrap();
    assert_eq!(et.fields.len(), 2);
    assert!(et.has_stacktrace);
}

#[test]
fn t6_native_method_sample_exists() {
    let fr = crate::create_flight_recorder();
    assert!(fr
        .type_registry
        .find_by_name("jdk.NativeMethodSample")
        .is_some());
}

#[test]
fn t6_allocation_requiring_gc_exists() {
    let fr = crate::create_flight_recorder();
    assert!(fr
        .type_registry
        .find_by_name("jdk.AllocationRequiringGC")
        .is_some());
}

#[test]
fn t6_exception_events_exist() {
    let fr = crate::create_flight_recorder();
    assert!(fr
        .type_registry
        .find_by_name("jdk.JavaExceptionThrow")
        .is_some());
    assert!(fr
        .type_registry
        .find_by_name("jdk.JavaErrorThrow")
        .is_some());
    assert!(fr
        .type_registry
        .find_by_name("jdk.ExceptionStatistics")
        .is_some());
}

#[test]
fn t6_network_utilization_exists() {
    let fr = crate::create_flight_recorder();
    let id = fr
        .type_registry
        .find_by_name("jdk.NetworkUtilization")
        .unwrap();
    let et = fr.type_registry.get(id).unwrap();
    assert_eq!(et.fields.len(), 3);
    assert_eq!(et.fields[0].name, "networkInterface");
}

#[test]
fn t6_container_events_exist() {
    let fr = crate::create_flight_recorder();
    assert!(fr
        .type_registry
        .find_by_name("jdk.ContainerCPUUsage")
        .is_some());
    assert!(fr
        .type_registry
        .find_by_name("jdk.ContainerMemoryUsage")
        .is_some());
}

#[test]
fn t6_module_events_exist() {
    let fr = crate::create_flight_recorder();
    assert!(fr.type_registry.find_by_name("jdk.ModuleRequire").is_some());
    assert!(fr.type_registry.find_by_name("jdk.ModuleExport").is_some());
}

#[test]
fn t6_system_gc_event_exists() {
    let fr = crate::create_flight_recorder();
    let id = fr.type_registry.find_by_name("jdk.SystemGC").unwrap();
    let et = fr.type_registry.get(id).unwrap();
    assert!(et.has_stacktrace);
}

#[test]
fn t6_total_event_count_47() {
    let fr = crate::create_flight_recorder();
    // 47 `jdk.*` + `cratonvm.JitCompileDecision` + `jdk.GCPhaseConcurrent`
    // + `jdk.GCPhasePauseLevel1` + 5 ZGC events; see `s41_builtin_event_count_47`.
    assert_eq!(fr.type_registry.len(), 55);
}

// --- Custom event support ---

#[test]
fn t6_register_custom_event() {
    let mut reg = EventTypeRegistry::new();
    let id = register_custom_event(
        &mut reg,
        "com.example.MyEvent",
        &["Application", "Custom"],
        "A custom event",
        &[("message", "string", "Message"), ("count", "int", "Count")],
        true,
        true,
        None,
    );
    let et = reg.get(id).unwrap();
    assert_eq!(et.name, "com.example.MyEvent");
    assert_eq!(et.fields.len(), 2);
    assert!(et.has_thread);
    assert!(et.has_stacktrace);
}

#[test]
fn t6_emit_custom_event() {
    let mut fr = crate::create_flight_recorder();
    let _id = register_custom_event(
        &mut fr.type_registry,
        "test.CustomEvent",
        &["Test"],
        "test",
        &[("data", "string", "Data")],
        false,
        false,
        None,
    );
    let rid = fr.new_recording(RecordingSettings::new("test"));
    let _g = drain_ring_baseline();
    fr.start_recording(rid);
    emit_custom_event(
        &mut fr,
        "test.CustomEvent",
        vec![EventValue::from_str("hello")],
        1000,
        1,
    )
    .expect("emit should succeed");
    fr.drain_per_thread_into_repository();
    let rec = fr.get_recording(rid).unwrap();
    assert_eq!(rec.event_count(), 1);
}

// -----------------------------------------------------------------------
// Task #30 (HIGH correctness): emit_custom_event variant validation
// -----------------------------------------------------------------------
//
// The JFR writer dispatches on the Rust `EventValue` variant; the reader
// dispatches on the registry-declared `type_name`. A per-emit mismatch
// would silently desync the entire chunk. These tests confirm that:
//   1. float-vs-double mismatch is detected and rejected
//   2. string-vs-int mismatch is detected and rejected
//   3. correctly-shaped emits succeed across many calls
// and that the per-event-type shape lock cannot be bypassed by a
// subsequent caller swapping variants.

fn t30_setup_recorder(
    field_type: &str,
) -> (
    FlightRecorder,
    EventTypeId,
    u64,
    parking_lot::MutexGuard<'static, ()>,
) {
    // Hold the global test lock for the test's duration: these tests depend
    // on `is_enabled()` staying true (set by our own `start_recording`); a
    // concurrent test's `stop_recording` would otherwise flip the global
    // `JFR_ENABLED` flag off, making `emit_custom_event` early-return
    // `Ok(())` before validation runs and breaking the rejection asserts.
    let guard = crate::repository::jfr_test_guard();
    let mut fr = crate::create_flight_recorder();
    let id = register_custom_event(
        &mut fr.type_registry,
        "test.task30.Validate",
        &["Test"],
        "task #30 variant validation",
        &[("value", field_type, "A typed value")],
        false,
        false,
        None,
    );
    let rid = fr.new_recording(RecordingSettings::new("task30"));
    fr.start_recording(rid);
    (fr, id, rid, guard)
}

// Mismatch tests use `#[cfg_attr(debug_assertions, should_panic)]` because
// in debug builds the validator fires a `debug_assert!` with a diagnostic
// (acceptance criterion #2). In release builds the same call returns
// `Err(...)` (criterion #3) — which we check explicitly below.
//
// Each pair (debug / release) is split into two named tests so the failure
// modes can be inspected independently.

#[test]
#[cfg_attr(debug_assertions, should_panic(expected = "validation failed"))]
fn t30_float_vs_double_mismatch_rejected() {
    // Declared "double" + supplied `EventValue::Float(_)`:
    //   writer would emit 4 bytes, reader expects 8 → chunk desync.
    let (mut fr, _id, _rid, _g) = t30_setup_recorder("double");
    let err = emit_custom_event(
        &mut fr,
        "test.task30.Validate",
        vec![EventValue::Float(1.5)],
        1000,
        1,
    );
    // Release path: the call above returned an Err; assert its shape.
    // (Debug path: the `debug_assert!` inside the emit panicked, so
    // execution never reaches this point — `#[should_panic]` covers it.)
    assert!(
        matches!(
            err,
            Err(EmitError::DeclaredTypeMismatch {
                field_index: 0,
                declared: FieldKind::Double,
                actual: FieldKind::Float,
            })
        ),
        "expected DeclaredTypeMismatch(Double, Float), got {:?}",
        err
    );
}

#[test]
#[cfg_attr(debug_assertions, should_panic(expected = "validation failed"))]
fn t30_double_vs_float_mismatch_rejected() {
    // Symmetric case: declared "float" + supplied `EventValue::Double(_)`.
    // Writer would emit 8 bytes, reader would read 4 — same desync hazard.
    let (mut fr, _id, _rid, _g) = t30_setup_recorder("float");
    let err = emit_custom_event(
        &mut fr,
        "test.task30.Validate",
        vec![EventValue::Double(1.5)],
        1000,
        1,
    );
    assert!(
        matches!(
            err,
            Err(EmitError::DeclaredTypeMismatch {
                field_index: 0,
                declared: FieldKind::Float,
                actual: FieldKind::Double,
            })
        ),
        "expected DeclaredTypeMismatch(Float, Double), got {:?}",
        err
    );
}

#[test]
#[cfg_attr(debug_assertions, should_panic(expected = "validation failed"))]
fn t30_string_vs_int_mismatch_rejected() {
    // Declared "int" + supplied `EventValue::String(_)`.
    // The reader would try to varint-decode the string's tag/length
    // prefix as an i32 and then misread every following field.
    let (mut fr, _id, _rid, _g) = t30_setup_recorder("int");
    let err = emit_custom_event(
        &mut fr,
        "test.task30.Validate",
        vec![EventValue::from_str("not an int")],
        1000,
        1,
    );
    assert!(
        matches!(
            err,
            Err(EmitError::DeclaredTypeMismatch {
                field_index: 0,
                declared: FieldKind::Int,
                actual: FieldKind::String,
            })
        ),
        "expected DeclaredTypeMismatch(Int, String), got {:?}",
        err
    );
}

#[test]
#[cfg_attr(debug_assertions, should_panic(expected = "validation failed"))]
fn t30_int_vs_string_mismatch_rejected() {
    // Symmetric case: declared "string" + supplied `EventValue::Int(_)`.
    let (mut fr, _id, _rid, _g) = t30_setup_recorder("string");
    let err = emit_custom_event(
        &mut fr,
        "test.task30.Validate",
        vec![EventValue::Int(42)],
        1000,
        1,
    );
    assert!(
        matches!(
            err,
            Err(EmitError::DeclaredTypeMismatch {
                field_index: 0,
                declared: FieldKind::String,
                actual: FieldKind::Int,
            })
        ),
        "expected DeclaredTypeMismatch(String, Int), got {:?}",
        err
    );
}

#[test]
fn t30_correct_shape_succeeds_across_many_calls() {
    // Declared "double" + always-`Double` emits: every call must succeed,
    // and the per-event-type shape lock must accept all of them.
    let (mut fr, _id, _rid, _g) = t30_setup_recorder("double");
    for i in 0..32u64 {
        let r = emit_custom_event(
            &mut fr,
            "test.task30.Validate",
            vec![EventValue::Double(i as f64 + 0.5)],
            1000 + i,
            1,
        );
        assert!(r.is_ok(), "emit #{} should succeed, got {:?}", i, r);
    }
}

#[test]
fn t30_shape_lock_populated_on_first_emit() {
    // The first emit must populate `field_shape_lock` with the runtime
    // variant sequence; same-shape subsequent emits then keep working.
    let (mut fr, type_id, _rid, _g) = t30_setup_recorder("string");
    emit_custom_event(
        &mut fr,
        "test.task30.Validate",
        vec![EventValue::from_str("v1")],
        1000,
        1,
    )
    .expect("first emit succeeds");
    assert_eq!(
        fr.field_shape_lock.get(&type_id).cloned(),
        Some(vec![FieldKind::String])
    );
    for i in 0..8 {
        emit_custom_event(
            &mut fr,
            "test.task30.Validate",
            vec![EventValue::from_str(&format!("v{}", i + 2))],
            1000 + i as u64,
            1,
        )
        .expect("same-shape emit succeeds");
    }
}

#[test]
#[cfg_attr(debug_assertions, should_panic(expected = "validation failed"))]
fn t30_field_count_mismatch_rejected() {
    // Declares 2 fields but emit supplies 1 — guards against partial
    // event payloads silently truncating the chunk.
    // Hold the global test lock so a concurrent `stop_recording` cannot
    // flip `JFR_ENABLED` off and make the emit early-return before
    // validation runs.
    let _g = crate::repository::jfr_test_guard();
    let mut fr = crate::create_flight_recorder();
    let _id = register_custom_event(
        &mut fr.type_registry,
        "test.task30.Count",
        &["Test"],
        "count",
        &[("a", "long", "a"), ("b", "long", "b")],
        false,
        false,
        None,
    );
    let rid = fr.new_recording(RecordingSettings::new("count"));
    fr.start_recording(rid);
    let err = emit_custom_event(
        &mut fr,
        "test.task30.Count",
        vec![EventValue::Long(1)],
        1000,
        1,
    );
    assert!(
        matches!(
            err,
            Err(EmitError::FieldCountMismatch {
                expected: 2,
                actual: 1
            })
        ),
        "expected FieldCountMismatch(2,1), got {:?}",
        err
    );
}

// --- JFR profiles ---

#[test]
fn t6_default_profile() {
    let profile = JfrProfile::default_profile();
    assert_eq!(profile.name, "default");
    assert!(!profile.settings.is_empty());
    // GC events should be enabled
    assert!(profile
        .settings
        .iter()
        .any(|s| s.event_name == "jdk.GarbageCollection" && s.enabled));
    // Exception throw should be disabled in default profile
    assert!(profile
        .settings
        .iter()
        .any(|s| s.event_name == "jdk.JavaExceptionThrow" && !s.enabled));
}

#[test]
fn t6_detailed_profile() {
    let profile = JfrProfile::detailed_profile();
    assert_eq!(profile.name, "profile");
    assert!(!profile.settings.is_empty());
    // Exception throw should be enabled in detailed profile
    assert!(profile
        .settings
        .iter()
        .any(|s| s.event_name == "jdk.JavaExceptionThrow" && s.enabled));
    // Safepoints should be enabled
    assert!(profile
        .settings
        .iter()
        .any(|s| s.event_name == "jdk.SafepointBegin" && s.enabled));
}

#[test]
fn t6_profile_apply_to_settings() {
    let fr = crate::create_flight_recorder();
    let profile = JfrProfile::default_profile();
    let mut settings = RecordingSettings::new("profiled");
    profile.apply_to(&mut settings, &fr.type_registry);
    // At least some events should be enabled
    assert!(!settings.enabled_events.is_empty());
}

// --- New emit helpers ---

#[test]
fn t6_emit_safepoint_events() {
    let mut fr = make_recorder();
    let rid = fr.new_recording(RecordingSettings::new("test"));
    let _g = drain_ring_baseline();
    fr.start_recording(rid);
    emit_safepoint_begin_event(&mut fr, 1, 10, 0, 1000);
    emit_safepoint_end_event(&mut fr, 1, 1000, 500);
    fr.drain_per_thread_into_repository();
    let rec = fr.get_recording(rid).unwrap();
    assert_eq!(rec.event_count(), 2);
}

#[test]
fn t6_emit_exception_event() {
    let mut fr = make_recorder();
    let rid = fr.new_recording(RecordingSettings::new("test"));
    let _g = drain_ring_baseline();
    fr.start_recording(rid);
    emit_java_exception_throw_event(
        &mut fr,
        "test error",
        "java.lang.RuntimeException",
        1000,
        1,
    );
    fr.drain_per_thread_into_repository();
    let rec = fr.get_recording(rid).unwrap();
    assert_eq!(rec.event_count(), 1);
}

#[test]
fn t6_emit_network_utilization() {
    let mut fr = make_recorder();
    let rid = fr.new_recording(RecordingSettings::new("test"));
    let _g = drain_ring_baseline();
    fr.start_recording(rid);
    emit_network_utilization_event(&mut fr, "eth0", 1024, 512, 1000);
    fr.drain_per_thread_into_repository();
    let rec = fr.get_recording(rid).unwrap();
    assert_eq!(rec.event_count(), 1);
}
