pub mod event;
pub mod dump;
pub mod recording;
pub mod repository;
pub mod builtin;
pub mod stream;

pub use event::*;
pub use recording::*;
pub use repository::*;
pub use dump::{JfrDumpError, JfrFileHeader, dump_to_file, read_events, read_jfr_header};
pub use stream::EventStream;

/// Create a new FlightRecorder with all built-in events registered.
pub fn create_flight_recorder() -> FlightRecorder {
    let mut fr = FlightRecorder::new();
    builtin::register_builtin_events(&mut fr.type_registry);
    fr
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn make_event(type_id: EventTypeId, start: u64, end: u64) -> EventInstance {
        EventInstance {
            type_id,
            start_time: start,
            end_time: end,
            thread_id: 1,
            fields: vec![],
        }
    }

    fn make_event_with_fields(type_id: EventTypeId, fields: Vec<EventValue>) -> EventInstance {
        EventInstance {
            type_id,
            start_time: 1000,
            end_time: 2000,
            thread_id: 1,
            fields,
        }
    }

    // --- Event type registration and lookup ---

    #[test]
    fn test_event_type_registration() {
        let mut reg = EventTypeRegistry::new();
        let id = reg.register(EventType {
            id: EventTypeId(0),
            name: "test.Event".into(),
            category: vec!["Test".into()],
            description: "A test event".into(),
            fields: vec![],
            has_thread: false,
            has_stacktrace: false,
            period: EventPeriod::None,
            threshold: None,
        });
        assert_eq!(reg.len(), 1);
        assert!(reg.get(id).is_some());
        assert_eq!(reg.get(id).unwrap().name, "test.Event");
    }

    #[test]
    fn test_event_type_lookup_by_name() {
        let mut reg = EventTypeRegistry::new();
        reg.register(EventType {
            id: EventTypeId(0),
            name: "test.Alpha".into(),
            category: vec![],
            description: "".into(),
            fields: vec![],
            has_thread: false,
            has_stacktrace: false,
            period: EventPeriod::None,
            threshold: None,
        });
        assert!(reg.find_by_name("test.Alpha").is_some());
        assert!(reg.find_by_name("test.Missing").is_none());
    }

    #[test]
    fn test_event_type_not_found() {
        let reg = EventTypeRegistry::new();
        assert!(reg.get(EventTypeId(999)).is_none());
    }

    // --- Recording lifecycle ---

    #[test]
    fn test_recording_lifecycle() {
        let mut rec = Recording::new(1, RecordingSettings::new("test"));
        assert_eq!(rec.state, RecordingState::New);

        rec.start();
        assert_eq!(rec.state, RecordingState::Running);
        assert!(rec.start_time.is_some());

        rec.stop();
        assert_eq!(rec.state, RecordingState::Stopped);
        assert!(rec.stop_time.is_some());

        rec.close();
        assert_eq!(rec.state, RecordingState::Closed);
    }

    #[test]
    fn test_recording_record_events() {
        let mut rec = Recording::new(1, RecordingSettings::new("test"));
        let tid = EventTypeId(1);

        // Cannot record in New state
        rec.record_event(make_event(tid, 100, 200));
        assert_eq!(rec.event_count(), 0);

        rec.start();
        rec.record_event(make_event(tid, 100, 200));
        rec.record_event(make_event(tid, 200, 300));
        assert_eq!(rec.event_count(), 2);

        rec.stop();
        // Cannot record in Stopped state
        rec.record_event(make_event(tid, 300, 400));
        assert_eq!(rec.event_count(), 2);
    }

    #[test]
    fn test_recording_get_events() {
        let mut rec = Recording::new(1, RecordingSettings::new("test"));
        rec.start();
        rec.record_event(make_event(EventTypeId(1), 100, 200));
        let events = rec.get_events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].start_time, 100);
    }

    // --- Event repository push/evict/query ---

    #[test]
    fn test_repository_push_and_len() {
        let mut repo = EventRepository::new(10);
        repo.push(make_event(EventTypeId(1), 100, 200));
        repo.push(make_event(EventTypeId(1), 200, 300));
        assert_eq!(repo.len(), 2);
        assert_eq!(repo.total_recorded(), 2);
    }

    #[test]
    fn test_repository_ring_buffer_eviction() {
        let mut repo = EventRepository::new(3);
        for i in 0..5 {
            repo.push(make_event(EventTypeId(1), i * 100, i * 100 + 50));
        }
        assert_eq!(repo.len(), 3);
        assert_eq!(repo.total_recorded(), 5);
        // The oldest two should have been evicted; first remaining starts at 200
        assert_eq!(repo.events()[0].start_time, 200);
    }

    #[test]
    fn test_repository_clear() {
        let mut repo = EventRepository::new(10);
        repo.push(make_event(EventTypeId(1), 100, 200));
        repo.clear();
        assert_eq!(repo.len(), 0);
        assert!(repo.is_empty());
    }

    #[test]
    fn test_repository_events_by_type() {
        let mut repo = EventRepository::new(10);
        let t1 = EventTypeId(1);
        let t2 = EventTypeId(2);
        repo.push(make_event(t1, 100, 200));
        repo.push(make_event(t2, 200, 300));
        repo.push(make_event(t1, 300, 400));
        assert_eq!(repo.events_by_type(t1).len(), 2);
        assert_eq!(repo.events_by_type(t2).len(), 1);
        assert_eq!(repo.events_by_type(EventTypeId(99)).len(), 0);
    }

    #[test]
    fn test_repository_events_in_range() {
        let mut repo = EventRepository::new(10);
        let t = EventTypeId(1);
        repo.push(make_event(t, 100, 150));
        repo.push(make_event(t, 200, 250));
        repo.push(make_event(t, 300, 350));
        let in_range = repo.events_in_range(150, 250);
        assert_eq!(in_range.len(), 1);
        assert_eq!(in_range[0].start_time, 200);
    }

    // --- Built-in events ---

    #[test]
    fn test_builtin_events_at_least_20() {
        let fr = create_flight_recorder();
        assert!(fr.type_registry.len() >= 20);
    }

    #[test]
    fn test_builtin_gc_event_exists() {
        let fr = create_flight_recorder();
        assert!(fr.type_registry.find_by_name("jdk.GarbageCollection").is_some());
    }

    #[test]
    fn test_builtin_thread_events_exist() {
        let fr = create_flight_recorder();
        assert!(fr.type_registry.find_by_name("jdk.ThreadStart").is_some());
        assert!(fr.type_registry.find_by_name("jdk.ThreadEnd").is_some());
    }

    // --- Recording with threshold filtering ---

    #[test]
    fn test_recording_threshold_filtering() {
        let tid = EventTypeId(1);
        let mut settings = RecordingSettings::new("threshold-test");
        settings.event_thresholds.insert(tid, Duration::from_nanos(500));

        let mut rec = Recording::new(1, settings);
        rec.start();

        // Duration 100ns < threshold 500ns => filtered out
        rec.record_event(make_event(tid, 1000, 1100));
        assert_eq!(rec.event_count(), 0);

        // Duration 600ns >= threshold 500ns => accepted
        rec.record_event(make_event(tid, 2000, 2600));
        assert_eq!(rec.event_count(), 1);
    }

    // --- Disabled event not recorded ---

    #[test]
    fn test_disabled_event_not_recorded() {
        let enabled = EventTypeId(10);
        let disabled = EventTypeId(20);
        let mut settings = RecordingSettings::new("selective");
        settings.enabled_events.insert(enabled);

        let mut rec = Recording::new(1, settings);
        rec.start();

        rec.record_event(make_event(enabled, 100, 200));
        rec.record_event(make_event(disabled, 100, 200));
        assert_eq!(rec.event_count(), 1);
    }

    // --- Multiple simultaneous recordings ---

    #[test]
    fn test_multiple_simultaneous_recordings() {
        let mut fr = create_flight_recorder();
        let r1 = fr.new_recording(RecordingSettings::new("rec1"));
        let r2 = fr.new_recording(RecordingSettings::new("rec2"));
        fr.start_recording(r1);
        fr.start_recording(r2);
        assert_eq!(fr.active_recording_count(), 2);

        let evt = make_event(EventTypeId(1), 100, 200);
        fr.record_event(evt);

        assert_eq!(fr.get_recording(r1).unwrap().event_count(), 1);
        assert_eq!(fr.get_recording(r2).unwrap().event_count(), 1);

        fr.stop_recording(r1);
        assert_eq!(fr.active_recording_count(), 1);

        let evt2 = make_event(EventTypeId(1), 300, 400);
        fr.record_event(evt2);
        assert_eq!(fr.get_recording(r1).unwrap().event_count(), 1);
        assert_eq!(fr.get_recording(r2).unwrap().event_count(), 2);
    }

    // --- FlightRecorder create/start/stop ---

    #[test]
    fn test_flight_recorder_create_start_stop() {
        let mut fr = FlightRecorder::new();
        let id = fr.new_recording(RecordingSettings::new("main"));
        assert_eq!(fr.active_recording_count(), 0);
        fr.start_recording(id);
        assert_eq!(fr.active_recording_count(), 1);
        fr.stop_recording(id);
        assert_eq!(fr.active_recording_count(), 0);
    }

    // --- Event field access ---

    #[test]
    fn test_event_field_access() {
        let fields = vec![
            EventValue::Int(42),
            EventValue::String(std::sync::Arc::from("hello")),
            EventValue::Boolean(true),
            EventValue::Long(123456),
            EventValue::Float(3.25),
            EventValue::Double(2.5),
            EventValue::Null,
        ];
        let evt = make_event_with_fields(EventTypeId(1), fields);
        assert_eq!(evt.fields.len(), 7);
        assert!(matches!(&evt.fields[0], EventValue::Int(42)));
        assert!(matches!(&evt.fields[1], EventValue::String(s) if &**s == "hello"));
    }

    // --- EventField::new helper ---

    #[test]
    fn test_event_field_new() {
        let f = EventField::new("gcId", "int", "GC Identifier");
        assert_eq!(f.name, "gcId");
        assert_eq!(f.type_name, "int");
        assert_eq!(f.description, "GC Identifier");
    }

    // --- Registry empty check ---

    #[test]
    fn test_registry_empty() {
        let reg = EventTypeRegistry::new();
        assert!(reg.is_empty());
        assert_eq!(reg.len(), 0);
    }

    // --- Repository default ---

    #[test]
    fn test_repository_default_capacity() {
        let repo = EventRepository::default();
        assert_eq!(repo.len(), 0);
        assert_eq!(repo.total_recorded(), 0);
    }

    // --- create_flight_recorder ---

    #[test]
    fn test_create_flight_recorder_has_events() {
        let fr = create_flight_recorder();
        assert!(fr.type_registry.find_by_name("jdk.ClassLoad").is_some());
        assert!(fr.type_registry.find_by_name("jdk.CPULoad").is_some());
        assert!(fr.type_registry.find_by_name("jdk.ExecutionSample").is_some());
    }
}
