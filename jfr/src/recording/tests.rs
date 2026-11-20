use super::*;
use crate::event::{EventInstance, EventTypeId, EventValue};
use smallvec::smallvec;
use std::sync::Arc;

fn make_event(type_id: EventTypeId, start: u64, end: u64) -> EventInstance {
    EventInstance {
        type_id,
        start_time: start,
        end_time: end,
        thread_id: 1,
        fields: smallvec![],
    }
}

// --- RecordingState ---

#[test]
fn test_recording_state_equality() {
    assert_eq!(RecordingState::New, RecordingState::New);
    assert_eq!(RecordingState::Running, RecordingState::Running);
    assert_eq!(RecordingState::Stopped, RecordingState::Stopped);
    assert_eq!(RecordingState::Closed, RecordingState::Closed);
    assert_ne!(RecordingState::New, RecordingState::Running);
}

#[test]
fn test_recording_state_debug() {
    let dbg = format!("{:?}", RecordingState::Running);
    assert!(dbg.contains("Running"));
}

#[test]
fn test_recording_state_clone_copy() {
    let s = RecordingState::Stopped;
    let s2 = s; // Copy
    let s3 = s.clone();
    assert_eq!(s, s2);
    assert_eq!(s, s3);
}

// --- RecordingSettings ---

#[test]
fn test_recording_settings_defaults() {
    let s = RecordingSettings::new("test");
    assert_eq!(s.name, "test");
    assert!(s.max_age.is_none());
    assert!(s.max_size.is_none());
    assert!(!s.disk);
    assert!(!s.dump_on_exit);
    assert!(s.duration.is_none());
    assert!(s.enabled_events.is_empty());
    assert!(s.event_thresholds.is_empty());
}

#[test]
fn test_recording_settings_with_max_age() {
    let mut s = RecordingSettings::new("aged");
    s.max_age = Some(Duration::from_secs(60));
    assert_eq!(s.max_age.unwrap(), Duration::from_secs(60));
}

/// obsaudit D12 (2026-07-26): `max_size`/`max_age` are now enforced —
/// renamed from `obsaudit_max_size_and_max_age_are_not_enforced`, which
/// pinned the opposite (inert) behaviour. With `max_age` set to 1ns and
/// events spaced 1 second apart, every push's age-based eviction pass
/// clears everything older than (this event's time - 1ns) — which is
/// every prior event, since 1s >> 1ns — so only the just-pushed event
/// ever survives. `max_size = Some(2)` is strictly looser than that and
/// never binds first.
#[test]
fn obsaudit_max_size_and_max_age_are_enforced() {
    let mut settings = RecordingSettings::new("capped");
    settings.max_size = Some(2); // "keep at most 2 events"
    settings.max_age = Some(Duration::from_nanos(1)); // "keep only the newest"

    let mut rec = Recording::new(1, settings);
    rec.start();
    for i in 0..10u64 {
        rec.record_event(EventInstance {
            type_id: EventTypeId(1),
            start_time: i * 1_000_000_000,
            end_time: i * 1_000_000_000 + 1,
            thread_id: 1,
            fields: smallvec::SmallVec::new(),
        });
    }

    assert_eq!(
        rec.event_count(),
        1,
        "max_age=1ns evicts every event older than the one just pushed"
    );
}

/// obsaudit D12: `max_size` alone (age unset) behaves like a smaller
/// version of the default ring — a plain, size-only cap.
#[test]
fn obsaudit_max_size_alone_caps_without_age_eviction() {
    let mut settings = RecordingSettings::new("size-capped");
    settings.max_size = Some(3);

    let mut rec = Recording::new(1, settings);
    rec.start();
    for i in 0..10u64 {
        rec.record_event(EventInstance {
            type_id: EventTypeId(1),
            start_time: i * 1_000_000_000,
            end_time: i * 1_000_000_000 + 1,
            thread_id: 1,
            fields: smallvec::SmallVec::new(),
        });
    }

    assert_eq!(rec.event_count(), 3);
}

/// The bound that *does* apply: `EventRepository::default()`'s fixed ring.
/// A recording cannot grow without limit even though `max_size` is inert.
#[test]
fn obsaudit_recording_memory_is_bounded_by_repository_ring() {
    let mut repo = crate::repository::EventRepository::default();
    let cap = 100_000usize;
    for i in 0..(cap + 500) {
        repo.push(EventInstance {
            type_id: EventTypeId(1),
            start_time: i as u64,
            end_time: i as u64 + 1,
            thread_id: 1,
            fields: smallvec::SmallVec::new(),
        });
    }
    assert_eq!(
        repo.len(),
        cap,
        "the default repository ring must be bounded"
    );
    assert_eq!(repo.total_recorded(), (cap + 500) as u64);
}

#[test]
fn test_recording_settings_with_enabled_events() {
    let mut s = RecordingSettings::new("selective");
    s.enabled_events.insert(EventTypeId(1));
    s.enabled_events.insert(EventTypeId(2));
    assert_eq!(s.enabled_events.len(), 2);
    assert!(s.enabled_events.contains(&EventTypeId(1)));
}

#[test]
fn test_recording_settings_with_thresholds() {
    let mut s = RecordingSettings::new("threshold");
    s.event_thresholds
        .insert(EventTypeId(1), Duration::from_millis(10));
    assert_eq!(
        s.event_thresholds.get(&EventTypeId(1)),
        Some(&Duration::from_millis(10))
    );
}

// --- Recording ---

#[test]
fn test_recording_new_state() {
    let rec = Recording::new(1, RecordingSettings::new("r"));
    assert_eq!(rec.id, 1);
    assert_eq!(rec.state, RecordingState::New);
    assert!(rec.start_time.is_none());
    assert!(rec.stop_time.is_none());
    assert_eq!(rec.event_count(), 0);
}

#[test]
fn test_recording_start() {
    let mut rec = Recording::new(1, RecordingSettings::new("r"));
    rec.start();
    assert_eq!(rec.state, RecordingState::Running);
    assert!(rec.start_time.is_some());
}

#[test]
fn test_recording_start_idempotent() {
    let mut rec = Recording::new(1, RecordingSettings::new("r"));
    rec.start();
    let t1 = rec.start_time.unwrap();
    rec.start(); // second start should be no-op since state is Running, not New
    assert_eq!(rec.start_time.unwrap(), t1);
}

#[test]
fn test_recording_stop() {
    let mut rec = Recording::new(1, RecordingSettings::new("r"));
    rec.start();
    rec.stop();
    assert_eq!(rec.state, RecordingState::Stopped);
    assert!(rec.stop_time.is_some());
}

#[test]
fn test_recording_stop_without_start() {
    let mut rec = Recording::new(1, RecordingSettings::new("r"));
    rec.stop(); // no-op, state is New not Running
    assert_eq!(rec.state, RecordingState::New);
    assert!(rec.stop_time.is_none());
}

#[test]
fn test_recording_close() {
    let mut rec = Recording::new(1, RecordingSettings::new("r"));
    rec.start();
    rec.stop();
    rec.close();
    assert_eq!(rec.state, RecordingState::Closed);
}

#[test]
fn test_recording_close_from_new() {
    let mut rec = Recording::new(1, RecordingSettings::new("r"));
    rec.close();
    assert_eq!(rec.state, RecordingState::Closed);
}

#[test]
fn test_recording_cannot_record_in_new() {
    let mut rec = Recording::new(1, RecordingSettings::new("r"));
    rec.record_event(make_event(EventTypeId(1), 100, 200));
    assert_eq!(rec.event_count(), 0);
}

#[test]
fn test_recording_cannot_record_in_stopped() {
    let mut rec = Recording::new(1, RecordingSettings::new("r"));
    rec.start();
    rec.stop();
    rec.record_event(make_event(EventTypeId(1), 100, 200));
    assert_eq!(rec.event_count(), 0);
}

#[test]
fn test_recording_cannot_record_in_closed() {
    let mut rec = Recording::new(1, RecordingSettings::new("r"));
    rec.start();
    rec.close();
    rec.record_event(make_event(EventTypeId(1), 100, 200));
    assert_eq!(rec.event_count(), 0);
}

#[test]
fn test_recording_records_in_running() {
    let mut rec = Recording::new(1, RecordingSettings::new("r"));
    rec.start();
    rec.record_event(make_event(EventTypeId(1), 100, 200));
    rec.record_event(make_event(EventTypeId(1), 200, 300));
    assert_eq!(rec.event_count(), 2);
}

#[test]
fn test_recording_get_events() {
    let mut rec = Recording::new(1, RecordingSettings::new("r"));
    rec.start();
    rec.record_event(make_event(EventTypeId(1), 100, 200));
    let events = rec.get_events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].start_time, 100);
    assert_eq!(events[0].end_time, 200);
}

#[test]
fn test_recording_is_event_enabled_all() {
    let rec = Recording::new(1, RecordingSettings::new("r"));
    // Empty enabled_events means all enabled
    assert!(rec.is_event_enabled(EventTypeId(1)));
    assert!(rec.is_event_enabled(EventTypeId(999)));
}

#[test]
fn test_recording_is_event_enabled_selective() {
    let mut settings = RecordingSettings::new("r");
    settings.enabled_events.insert(EventTypeId(5));
    let rec = Recording::new(1, settings);
    assert!(rec.is_event_enabled(EventTypeId(5)));
    assert!(!rec.is_event_enabled(EventTypeId(6)));
}

#[test]
fn test_recording_threshold_accepts_above() {
    let mut settings = RecordingSettings::new("r");
    settings
        .event_thresholds
        .insert(EventTypeId(1), Duration::from_nanos(100));
    let mut rec = Recording::new(1, settings);
    rec.start();
    rec.record_event(make_event(EventTypeId(1), 1000, 1200)); // 200ns >= 100ns
    assert_eq!(rec.event_count(), 1);
}

#[test]
fn test_recording_threshold_rejects_below() {
    let mut settings = RecordingSettings::new("r");
    settings
        .event_thresholds
        .insert(EventTypeId(1), Duration::from_nanos(500));
    let mut rec = Recording::new(1, settings);
    rec.start();
    rec.record_event(make_event(EventTypeId(1), 1000, 1100)); // 100ns < 500ns
    assert_eq!(rec.event_count(), 0);
}

#[test]
fn test_recording_threshold_exact_boundary() {
    let mut settings = RecordingSettings::new("r");
    settings
        .event_thresholds
        .insert(EventTypeId(1), Duration::from_nanos(100));
    let mut rec = Recording::new(1, settings);
    rec.start();
    rec.record_event(make_event(EventTypeId(1), 1000, 1100)); // exactly 100ns
    assert_eq!(rec.event_count(), 1);
}

#[test]
fn test_recording_disabled_event_filtered() {
    let mut settings = RecordingSettings::new("r");
    settings.enabled_events.insert(EventTypeId(1));
    let mut rec = Recording::new(1, settings);
    rec.start();
    rec.record_event(make_event(EventTypeId(1), 100, 200)); // enabled
    rec.record_event(make_event(EventTypeId(2), 100, 200)); // disabled
    assert_eq!(rec.event_count(), 1);
}

#[test]
fn test_recording_event_with_fields() {
    let mut rec = Recording::new(1, RecordingSettings::new("r"));
    rec.start();
    let evt = EventInstance {
        type_id: EventTypeId(1),
        start_time: 100,
        end_time: 200,
        thread_id: 42,
        fields: smallvec![EventValue::Int(10), EventValue::String(Arc::from("gc"))],
    };
    rec.record_event(evt);
    let events = rec.get_events();
    assert_eq!(events[0].fields.len(), 2);
    assert_eq!(events[0].thread_id, 42);
}

// --- FlightRecorder ---

#[test]
fn test_flight_recorder_new() {
    let fr = FlightRecorder::new();
    assert_eq!(fr.active_recording_count(), 0);
    assert!(fr.type_registry.is_empty());
}

#[test]
fn test_flight_recorder_default() {
    let fr = FlightRecorder::default();
    assert_eq!(fr.active_recording_count(), 0);
}

#[test]
fn test_flight_recorder_new_recording() {
    let mut fr = FlightRecorder::new();
    let id1 = fr.new_recording(RecordingSettings::new("r1"));
    let id2 = fr.new_recording(RecordingSettings::new("r2"));
    assert_ne!(id1, id2);
}

#[test]
fn test_flight_recorder_start_stop() {
    // start/stop toggle the process-global `JFR_ENABLED` flag; hold the
    // test lock so we don't flip it under a concurrent `emit_*` test that
    // depends on `is_enabled()`.
    let _g = crate::repository::jfr_test_guard();
    let mut fr = FlightRecorder::new();
    let id = fr.new_recording(RecordingSettings::new("r"));
    assert_eq!(fr.active_recording_count(), 0);
    fr.start_recording(id);
    assert_eq!(fr.active_recording_count(), 1);
    fr.stop_recording(id);
    assert_eq!(fr.active_recording_count(), 0);
}

#[test]
fn test_flight_recorder_get_recording() {
    let mut fr = FlightRecorder::new();
    let id = fr.new_recording(RecordingSettings::new("findme"));
    let rec = fr.get_recording(id).unwrap();
    assert_eq!(rec.settings.name, "findme");
}

#[test]
fn test_flight_recorder_get_missing_recording() {
    let fr = FlightRecorder::new();
    assert!(fr.get_recording(999).is_none());
}

#[test]
fn test_flight_recorder_record_event_to_all_running() {
    let mut fr = FlightRecorder::new();
    let r1 = fr.new_recording(RecordingSettings::new("r1"));
    let r2 = fr.new_recording(RecordingSettings::new("r2"));
    let r3 = fr.new_recording(RecordingSettings::new("r3"));
    fr.start_recording(r1);
    fr.start_recording(r2);
    // r3 is not started
    // Serialize against other global-ring tests so a concurrent drain can't
    // steal our event before we drain it.
    let _g = crate::repository::jfr_test_guard();
    // Drain pre-existing thread-ring contents so cross-test bleed-through
    // doesn't pollute the per-recording event counts.
    let _ = crate::repository::global_ring_registry().drain_all();
    fr.record_event(make_event(EventTypeId(1), 100, 200));
    // record_event now writes to the per-thread ring; the dump path
    // (and tests that immediately query the repository) must drain
    // explicitly before querying.
    fr.drain_per_thread_into_repository();
    assert_eq!(fr.get_recording(r1).unwrap().event_count(), 1);
    assert_eq!(fr.get_recording(r2).unwrap().event_count(), 1);
    assert_eq!(fr.get_recording(r3).unwrap().event_count(), 0);
}

/// `start(); commit(); stop(); dump(path)` — the shape every
/// `jdk.jfr.Recording` user writes — must put the committed event in the
/// file.
///
/// It used not to. `record_event` parks the event on a per-thread ring;
/// `drain_per_thread_into_repository` is destructive on that ring and fans
/// out only to RUNNING recordings; and `stop_recording` did not drain. So
/// the event was discarded by `dump_recording`'s own drain, and the dump
/// still reported a plausible size because the chunk's metadata is written
/// either way. The parity table in the write-up that prompted this fix
/// shows exactly that: a one-event CratonVM dump was 106 bytes larger than
/// an empty one — the size of the new event type's METADATA, with no event
/// record behind it.
#[test]
fn an_event_committed_before_stop_survives_into_the_dump() {
    let mut fr = FlightRecorder::new();
    let type_id = fr.type_registry.register(crate::event::EventType {
        id: EventTypeId::INVALID,
        name: "StopThenDumpProbe".to_owned(),
        category: vec!["Test".to_owned()],
        description: String::new(),
        fields: vec![crate::event::EventField::new("capacity", "int", "")],
        has_thread: false,
        has_stacktrace: false,
        period: crate::event::EventPeriod::None,
        threshold: None,
    });
    let id = fr.new_recording(RecordingSettings::new("stop-then-dump"));
    fr.start_recording(id);

    // The per-thread ring is process-global; serialize and pre-drain so the
    // assertion counts only this test's event.
    let _g = crate::repository::jfr_test_guard();
    let _ = crate::repository::global_ring_registry().drain_all();
    fr.record_event(EventInstance {
        type_id,
        start_time: 5_000,
        end_time: 5_100,
        thread_id: 1,
        fields: smallvec![EventValue::Int(4096)],
    });
    // No explicit drain here: stopping is what has to preserve the event.
    fr.stop_recording(id);

    let dir = std::env::temp_dir().join(format!("jfrk-stopdump-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("stopdump.jfr");
    fr.dump_recording(id, &path).expect("dump must succeed");

    let chunk = crate::jdk_chunk::read_chunk(&path).expect("the dump must be a readable chunk");
    let ours: Vec<_> = chunk
        .events
        .iter()
        .filter(|event| event.type_name == "StopThenDumpProbe")
        .collect();
    assert_eq!(
        ours.len(),
        1,
        "the event committed before stop() must be in the dump, got {:?}",
        chunk
            .events
            .iter()
            .map(|e| &e.type_name)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        ours[0].fields,
        vec![(
            "capacity".to_owned(),
            crate::jdk_chunk::ChunkValue::Int(4096)
        )]
    );
    let _ = std::fs::remove_file(&path);
}

/// The name filter's three cases, and the one that matters most: an EMPTY
/// enable list keeps nothing.
///
/// `enabled_events` (id-keyed) reads an empty set as "all events", which is
/// right for a CratonVM-internal recording and exactly wrong for a
/// `jdk.jfr.Recording` with no `enable(...)` call — HotSpot records nothing
/// for one. That is why this is a separate `Option`, not a reuse.
#[test]
fn the_name_filter_distinguishes_no_filter_from_an_empty_one() {
    let mut settings = RecordingSettings::new("names");
    assert!(
        settings.name_filter_admits(Some("ProbeEvent"), 0),
        "no name filter must admit everything"
    );
    assert!(settings.name_filter_admits(None, 0));

    settings.enabled_event_names = Some(FxHashSet::default());
    assert!(
        !settings.name_filter_admits(Some("ProbeEvent"), 0),
        "an empty enable list must admit nothing"
    );

    let mut only_probe = FxHashSet::default();
    only_probe.insert("ProbeEvent".to_owned());
    settings.enabled_event_names = Some(only_probe);
    assert!(settings.name_filter_admits(Some("ProbeEvent"), 0));
    assert!(!settings.name_filter_admits(Some("jdk.ClassLoad"), 0));
    assert!(
        !settings.name_filter_admits(None, 0),
        "an unnameable event cannot satisfy a filter that names what it wants"
    );

    settings
        .event_thresholds_by_name
        .insert("ProbeEvent".to_owned(), 1_000);
    assert!(!settings.name_filter_admits(Some("ProbeEvent"), 999));
    assert!(settings.name_filter_admits(Some("ProbeEvent"), 1_000));
}

/// End to end through the drain: a recording that enables one event name
/// keeps that one and drops the rest, and the drop is counted rather than
/// silent.
#[test]
fn the_drain_applies_the_name_filter_and_counts_what_it_drops() {
    let mut fr = FlightRecorder::new();
    let wanted = fr.type_registry.register(crate::event::EventType {
        id: EventTypeId::INVALID,
        name: "WantedEvent".to_owned(),
        category: vec!["Test".to_owned()],
        description: String::new(),
        fields: Vec::new(),
        has_thread: false,
        has_stacktrace: false,
        period: crate::event::EventPeriod::None,
        threshold: None,
    });
    let unwanted = fr.type_registry.register(crate::event::EventType {
        id: EventTypeId::INVALID,
        name: "UnwantedEvent".to_owned(),
        category: vec!["Test".to_owned()],
        description: String::new(),
        fields: Vec::new(),
        has_thread: false,
        has_stacktrace: false,
        period: crate::event::EventPeriod::None,
        threshold: None,
    });
    let mut settings = RecordingSettings::new("filtered");
    let mut names = FxHashSet::default();
    names.insert("WantedEvent".to_owned());
    settings.enabled_event_names = Some(names);
    let id = fr.new_recording(settings);
    fr.start_recording(id);

    let _g = crate::repository::jfr_test_guard();
    let _ = crate::repository::global_ring_registry().drain_all();
    fr.record_event(make_event(wanted, 100, 200));
    fr.record_event(make_event(unwanted, 100, 200));
    fr.drain_per_thread_into_repository();

    let rec = fr.get_recording(id).unwrap();
    assert_eq!(rec.event_count(), 1, "only the enabled name may be kept");
    assert_eq!(
        rec.repository().iter().next().map(|e| e.type_id),
        Some(wanted)
    );
    assert!(
        rec.stats().events_filtered_out >= 1,
        "the dropped event must be counted, not silently discarded"
    );
}

#[test]
fn record_event_writes_to_per_thread_ring() {
    // Verify that `record_event` routes through the per-thread ring and
    // that `drain_per_thread_into_repository` makes the event visible in
    // the recording's repository.
    let mut fr = FlightRecorder::new();
    let id = fr.new_recording(RecordingSettings::new("ring"));
    fr.start_recording(id);

    // Serialize against other global-ring tests so a concurrent drain can't
    // steal our event before we drain it.
    let _g = crate::repository::jfr_test_guard();
    // Drain any pending events from other tests on this thread before we
    // start, so our assertion is exact.
    let _ = crate::repository::global_ring_registry().drain_all();
    assert_eq!(fr.get_recording(id).unwrap().event_count(), 0);

    // Emit a uniquely-tagged event so we can identify it even if some
    // other test on the same thread pushed events into the registry
    // after our pre-drain (the global registry is process-wide).
    let unique = 0xC0FFEE_u64;
    fr.record_event(make_event(EventTypeId(42), unique, unique + 1));

    // Before draining, the repository should still be empty — the event
    // is sitting in this thread's ring shard.
    assert_eq!(
        fr.get_recording(id).unwrap().event_count(),
        0,
        "record_event should not have written to the repository directly"
    );

    // After draining, the repository should contain our event (and
    // possibly stragglers from concurrent tests; assert by content).
    fr.drain_per_thread_into_repository();
    let rec = fr.get_recording_mut(id).unwrap();
    let found = rec
        .get_events()
        .iter()
        .any(|e| e.type_id == EventTypeId(42) && e.start_time == unique);
    assert!(
        found,
        "expected drained event to appear in the recording's repository"
    );
}

#[test]
fn test_flight_recorder_start_nonexistent() {
    let mut fr = FlightRecorder::new();
    fr.start_recording(999); // should not panic
    assert_eq!(fr.active_recording_count(), 0);
}

#[test]
fn test_flight_recorder_stop_nonexistent() {
    let mut fr = FlightRecorder::new();
    fr.stop_recording(999); // should not panic
}

#[test]
fn test_flight_recorder_recording_ids_auto_increment() {
    let mut fr = FlightRecorder::new();
    let id1 = fr.new_recording(RecordingSettings::new("a"));
    let id2 = fr.new_recording(RecordingSettings::new("b"));
    let id3 = fr.new_recording(RecordingSettings::new("c"));
    assert_eq!(id1, 1);
    assert_eq!(id2, 2);
    assert_eq!(id3, 3);
}

#[test]
fn test_flight_recorder_get_recording_mut() {
    let mut fr = FlightRecorder::new();
    let id = fr.new_recording(RecordingSettings::new("mut"));
    let rec = fr.get_recording_mut(id).unwrap();
    rec.start();
    assert_eq!(fr.active_recording_count(), 1);
}

#[test]
fn get_recording_mut_lifecycle_resyncs_running_cache_on_record() {
    let _g = crate::repository::jfr_test_guard();
    let _ = crate::repository::global_ring_registry().drain_all();

    let mut fr = FlightRecorder::new();
    let id = fr.new_recording(RecordingSettings::new("mut-start"));
    fr.get_recording_mut(id).unwrap().start();

    let unique = EventTypeId(0xC3FF_0030);
    fr.record_event(make_event(unique, 100, 200));
    fr.drain_per_thread_into_repository();

    let rec = fr.get_recording(id).unwrap();
    assert!(
        rec.repository().iter().any(|e| e.type_id == unique),
        "direct lifecycle mutation through get_recording_mut must not leave running_ids stale"
    );
}

#[test]
fn test_flight_recorder_hashmap_lookup_performance() {
    // Verify O(1) lookup works with many recordings
    let mut fr = FlightRecorder::new();
    let mut ids = vec![];
    for i in 0..100 {
        ids.push(fr.new_recording(RecordingSettings::new(&format!("r{i}"))));
    }
    // All IDs should be retrievable
    for &id in &ids {
        assert!(fr.get_recording(id).is_some());
    }
    assert!(fr.get_recording(999).is_none());
}

#[test]
fn test_recording_ids_increment_safely() {
    let mut fr = FlightRecorder::new();
    let id1 = fr.new_recording(RecordingSettings::new("a"));
    let id2 = fr.new_recording(RecordingSettings::new("b"));
    let id3 = fr.new_recording(RecordingSettings::new("c"));
    assert_eq!(id1, 1);
    assert_eq!(id2, 2);
    assert_eq!(id3, 3);
}

#[test]
fn test_recording_lifecycle() {
    // start/stop toggle the process-global `JFR_ENABLED` flag; hold the
    // test lock so we don't flip it under a concurrent `emit_*` test that
    // depends on `is_enabled()`.
    let _g = crate::repository::jfr_test_guard();
    let mut fr = FlightRecorder::new();
    let id = fr.new_recording(RecordingSettings::new("lifecycle"));
    {
        let rec = fr.get_recording(id).unwrap();
        assert_eq!(rec.state, RecordingState::New);
    }
    fr.start_recording(id);
    {
        let rec = fr.get_recording(id).unwrap();
        assert_eq!(rec.state, RecordingState::Running);
        assert!(rec.start_time.is_some());
    }
    fr.stop_recording(id);
    {
        let rec = fr.get_recording(id).unwrap();
        assert_eq!(rec.state, RecordingState::Stopped);
        assert!(rec.stop_time.is_some());
    }
}

// ---------------------------------------------------------------------
// Round-9 CRIT-3 (2026-05-24) — filtered-event accounting
// ---------------------------------------------------------------------
//
// Tests below use deliberately high EventTypeIds (0xC3FF_xxxx) that no
// built-in event nor any other test in this crate emits, so a parallel
// test pushing into a different thread's ring shard cannot inflate
// either the `event_count()` or the `events_filtered_out()` counter.
// We still drain the global ring at the start of each test to clear
// anything left on the current thread's shard from prior tests.

/// Single-recording drain path: events that fail the recording's
/// `enabled_events` filter must be counted by `events_filtered_out`
/// instead of disappearing silently. Was CRIT-3 in the round-9 audit.
#[test]
fn drain_single_recording_counts_filtered_events() {
    // Use a private FlightRecorder; the drain path is per-FR so this
    // does not perturb other tests. The global per-thread ring IS
    // shared though, so we serialize against sibling global-ring tests
    // and drain it first to clear stragglers.
    let _g = crate::repository::jfr_test_guard();
    let _ = crate::repository::global_ring_registry().drain_all();

    // Unique-to-this-test type IDs (see module-level note) so cross-test
    // pushes on sibling threads cannot perturb our counters.
    let allowed = EventTypeId(0xC3FF_0001);
    let blocked = EventTypeId(0xC3FF_0002);

    let mut fr = FlightRecorder::new();
    let mut settings = RecordingSettings::new("filtered");
    // Enable only the allowed type — every other type id must be
    // filtered out and counted.
    settings.enabled_events.insert(allowed);
    let rid = fr.new_recording(settings);
    fr.start_recording(rid);

    // Emit three blocked events and one allowed.
    fr.record_event(make_event(blocked, 100, 200));
    fr.record_event(make_event(blocked, 300, 400));
    fr.record_event(make_event(blocked, 500, 600));
    fr.record_event(make_event(allowed, 700, 800));

    fr.drain_per_thread_into_repository();

    let rec = fr.get_recording(rid).expect("recording present");
    // Count surviving events of the *allowed* type only — a sibling
    // test could (rarely) push allowed-typed events on a different
    // thread's ring shard, which our drain would also pick up.
    let allowed_in_rec = rec
        .repository()
        .iter()
        .filter(|e| e.type_id == allowed)
        .count();
    assert_eq!(
        allowed_in_rec, 1,
        "only the allowed-typed event should survive the filter"
    );

    // Filter-out count is at LEAST the three we pushed; could be higher
    // if another test pushed unrelated-type events into a sibling
    // thread's ring between our drain-baseline and our own pushes.
    // Use `>=` rather than `==` so the test is robust under parallel
    // execution. The important property is "filtered events are
    // counted", not the exact count.
    assert!(
        rec.events_filtered_out() >= 3,
        "expected at least three filter rejections, got {}",
        rec.events_filtered_out(),
    );
    let stats = rec.stats();
    assert!(stats.events_filtered_out >= 3);
    assert_eq!(stats.events_recorded, rec.event_count());
}

/// Multi-recording drain path: each recording independently filters
/// and counts. Verifies the fan-out branch (running_len >= 2) also
/// increments `events_filtered_out` on every rejection.
#[test]
fn drain_multi_recording_counts_filtered_events_per_recording() {
    let _g = crate::repository::jfr_test_guard();
    let _ = crate::repository::global_ring_registry().drain_all();

    // Unique-to-this-test type IDs (see module-level note).
    let t1 = EventTypeId(0xC3FF_0010);
    let t2 = EventTypeId(0xC3FF_0011);

    let mut fr = FlightRecorder::new();
    let mut s1 = RecordingSettings::new("rec1");
    s1.enabled_events.insert(t1);
    let mut s2 = RecordingSettings::new("rec2");
    s2.enabled_events.insert(t2);
    let r1 = fr.new_recording(s1);
    let r2 = fr.new_recording(s2);
    fr.start_recording(r1);
    fr.start_recording(r2);

    // Three events of type t1 and two of type t2. Each recording keeps
    // events of its own type and counts the rest as filtered-out.
    fr.record_event(make_event(t1, 100, 200));
    fr.record_event(make_event(t1, 300, 400));
    fr.record_event(make_event(t1, 500, 600));
    fr.record_event(make_event(t2, 700, 800));
    fr.record_event(make_event(t2, 900, 1000));

    fr.drain_per_thread_into_repository();

    let r1_kept_t1 = fr
        .get_recording(r1)
        .unwrap()
        .repository()
        .iter()
        .filter(|e| e.type_id == t1)
        .count();
    let r2_kept_t2 = fr
        .get_recording(r2)
        .unwrap()
        .repository()
        .iter()
        .filter(|e| e.type_id == t2)
        .count();
    assert_eq!(r1_kept_t1, 3, "rec1 should keep type-t1 events");
    assert_eq!(r2_kept_t2, 2, "rec2 should keep type-t2 events");

    let s1 = fr.get_recording(r1).unwrap().stats();
    let s2 = fr.get_recording(r2).unwrap().stats();
    // Use `>=` for filter counts — see `drain_single_recording_counts_filtered_events`
    // for the parallel-test rationale.
    assert!(
        s1.events_filtered_out >= 2,
        "rec1 should filter out at least two type-t2 events (got {})",
        s1.events_filtered_out,
    );
    assert!(
        s2.events_filtered_out >= 3,
        "rec2 should filter out at least three type-t1 events (got {})",
        s2.events_filtered_out,
    );
}

/// `passes_filter` returns false outside Running, so a recording that
/// never started observes the `state != Running` short-circuit in
/// `record_event`. Verify those drops are *not* counted as filter
/// rejections (the recording wouldn't have wanted them either way,
/// and there is no operator surprise to surface). Only running
/// recordings participate in the drain fan-out, so this test
/// double-checks the running-id snapshot keeps the un-started
/// recording out of the fan-out entirely.
#[test]
fn drain_does_not_route_to_unstarted_recordings() {
    let _g = crate::repository::jfr_test_guard();
    let _ = crate::repository::global_ring_registry().drain_all();

    let unique = EventTypeId(0xC3FF_0020);

    let mut fr = FlightRecorder::new();
    let r_running = fr.new_recording(RecordingSettings::new("running"));
    let r_idle = fr.new_recording(RecordingSettings::new("idle"));
    fr.start_recording(r_running);
    // r_idle is intentionally not started.

    fr.record_event(make_event(unique, 100, 200));
    fr.drain_per_thread_into_repository();

    let running = fr.get_recording(r_running).unwrap();
    let idle = fr.get_recording(r_idle).unwrap();
    // At least our pushed event lands in the running recording.
    assert!(
        running.repository().iter().any(|e| e.type_id == unique),
        "the running recording should receive our uniquely-typed event"
    );
    // Idle recording must see neither a kept event nor a filter-out
    // bump — it wasn't in the running snapshot to begin with.
    assert_eq!(idle.event_count(), 0);
    assert_eq!(idle.events_filtered_out(), 0);
}
