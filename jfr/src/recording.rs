use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rustc_hash::{FxHashMap, FxHashSet};

use crate::dump::{self, JfrDumpError};
use crate::event::{EventInstance, EventTypeId, EventTypeRegistry};
use crate::repository::EventRepository;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordingState {
    New,
    Running,
    Stopped,
    Closed,
}

pub struct RecordingSettings {
    pub name: String,
    pub max_age: Option<Duration>,
    pub max_size: Option<usize>,
    pub disk: bool,
    pub dump_on_exit: bool,
    pub duration: Option<Duration>,
    /// T10.9.B: FxHashSet — EventTypeId is internal JFR definition.
    pub enabled_events: FxHashSet<EventTypeId>,
    /// T10.9.B: FxHashMap — EventTypeId is internal JFR definition.
    pub event_thresholds: FxHashMap<EventTypeId, Duration>,
}

impl RecordingSettings {
    /// Create settings with a name and all events enabled by default (empty set means all).
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            max_age: None,
            max_size: None,
            disk: false,
            dump_on_exit: false,
            duration: None,
            enabled_events: FxHashSet::default(),
            event_thresholds: FxHashMap::default(),
        }
    }
}

pub struct Recording {
    pub id: u64,
    pub settings: RecordingSettings,
    pub state: RecordingState,
    pub start_time: Option<Instant>,
    pub stop_time: Option<Instant>,
    repository: EventRepository,
}

impl Recording {
    pub fn new(id: u64, settings: RecordingSettings) -> Self {
        Self {
            id,
            settings,
            state: RecordingState::New,
            start_time: None,
            stop_time: None,
            repository: EventRepository::default(),
        }
    }

    pub fn start(&mut self) {
        if self.state == RecordingState::New {
            self.state = RecordingState::Running;
            self.start_time = Some(Instant::now());
        }
    }

    pub fn stop(&mut self) {
        if self.state == RecordingState::Running {
            self.state = RecordingState::Stopped;
            self.stop_time = Some(Instant::now());
        }
    }

    pub fn close(&mut self) {
        self.state = RecordingState::Closed;
    }

    /// Record an event if the recording is running and the event is enabled.
    /// Accepts `Arc<EventInstance>` to avoid cloning across multiple recordings.
    pub fn record_event_arc(&mut self, event: Arc<EventInstance>) {
        if self.state != RecordingState::Running {
            return;
        }
        if !self.is_event_enabled(event.type_id) {
            return;
        }
        // Check threshold: if the event type has a threshold, the event duration must meet it.
        if let Some(&threshold) = self.settings.event_thresholds.get(&event.type_id) {
            let duration_ns = event.end_time.saturating_sub(event.start_time);
            if duration_ns < threshold.as_nanos() as u64 {
                return;
            }
        }
        // Unwrap the Arc — the repository stores owned EventInstance.
        // Arc::try_unwrap will succeed if this is the last reference; otherwise clone.
        let owned = match Arc::try_unwrap(event) {
            Ok(e) => e,
            Err(arc) => (*arc).clone(),
        };
        self.repository.push(owned);
    }

    /// Record an event (owned). Convenience for single-recording use.
    pub fn record_event(&mut self, event: EventInstance) {
        if self.state != RecordingState::Running {
            return;
        }
        if !self.is_event_enabled(event.type_id) {
            return;
        }
        if let Some(&threshold) = self.settings.event_thresholds.get(&event.type_id) {
            let duration_ns = event.end_time.saturating_sub(event.start_time);
            if duration_ns < threshold.as_nanos() as u64 {
                return;
            }
        }
        self.repository.push(event);
    }

    pub fn get_events(&mut self) -> &[EventInstance] {
        self.repository.events()
    }

    /// Check if an event type is enabled. An empty `enabled_events` set means all events are enabled.
    pub fn is_event_enabled(&self, type_id: EventTypeId) -> bool {
        self.settings.enabled_events.is_empty()
            || self.settings.enabled_events.contains(&type_id)
    }

    pub fn event_count(&self) -> usize {
        self.repository.len()
    }

    /// Provide read-only access to the underlying repository (for dump/stream).
    pub fn repository(&self) -> &EventRepository {
        &self.repository
    }

    /// Provide mutable access to the underlying repository (for dump).
    pub fn repository_mut(&mut self) -> &mut EventRepository {
        &mut self.repository
    }
}

/// Central flight recorder that manages recordings and the type registry.
///
/// Recordings are stored in an FxHashMap for O(1) lookup by ID.
/// T10.9.B: FxHashMap — recording IDs are internal monotonic counters.
pub struct FlightRecorder {
    recordings: FxHashMap<u64, Recording>,
    pub type_registry: EventTypeRegistry,
    next_recording_id: u64,
}

impl FlightRecorder {
    pub fn new() -> Self {
        Self {
            recordings: FxHashMap::default(),
            type_registry: EventTypeRegistry::new(),
            next_recording_id: 1,
        }
    }

    /// Create a new recording and return its id.
    pub fn new_recording(&mut self, settings: RecordingSettings) -> u64 {
        let id = self.next_recording_id;
        self.next_recording_id = self.next_recording_id.checked_add(1).expect("recording ID overflow");
        self.recordings.insert(id, Recording::new(id, settings));
        id
    }

    pub fn start_recording(&mut self, id: u64) {
        if let Some(rec) = self.recordings.get_mut(&id) {
            rec.start();
        }
    }

    pub fn stop_recording(&mut self, id: u64) {
        if let Some(rec) = self.recordings.get_mut(&id) {
            rec.stop();
        }
    }

    /// Record an event into all currently running recordings.
    /// Uses `Arc<EventInstance>` to share the event across recordings without cloning.
    pub fn record_event(&mut self, event: EventInstance) {
        let running: Vec<u64> = self
            .recordings
            .iter()
            .filter(|(_, r)| r.state == RecordingState::Running)
            .map(|(&id, _)| id)
            .collect();

        if running.is_empty() {
            return;
        }

        if running.len() == 1 {
            // Single recording: no Arc overhead, just move
            if let Some(rec) = self.recordings.get_mut(&running[0]) {
                rec.record_event(event);
            }
        } else {
            // Multiple recordings: wrap in Arc to share
            let arc_event = Arc::new(event);
            for id in running {
                if let Some(rec) = self.recordings.get_mut(&id) {
                    rec.record_event_arc(Arc::clone(&arc_event));
                }
            }
        }
    }

    pub fn get_recording(&self, id: u64) -> Option<&Recording> {
        self.recordings.get(&id)
    }

    pub fn get_recording_mut(&mut self, id: u64) -> Option<&mut Recording> {
        self.recordings.get_mut(&id)
    }

    pub fn active_recording_count(&self) -> usize {
        self.recordings
            .values()
            .filter(|r| r.state == RecordingState::Running)
            .count()
    }

    /// Dump a recording to a JFR binary file.
    ///
    /// The recording must exist and be in `Stopped` or `Running` state.
    /// Events are serialized using the JFR v2.0 binary format.
    pub fn dump_recording(&mut self, id: u64, path: &Path) -> Result<u64, JfrDumpError> {
        let rec = self.recordings.get_mut(&id).ok_or(JfrDumpError::Io(
            std::io::Error::new(std::io::ErrorKind::NotFound, "recording not found"),
        ))?;

        let events = rec.repository_mut().events();
        let start_time = events.iter().map(|e| e.start_time).min().unwrap_or(0);
        let end_time = events.iter().map(|e| e.end_time).max().unwrap_or(0);
        let duration = end_time.saturating_sub(start_time);

        dump::dump_to_file(path, rec.repository(), &self.type_registry, start_time, duration)
    }
}

impl Default for FlightRecorder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{EventInstance, EventTypeId, EventValue};
    use std::sync::Arc;

    fn make_event(type_id: EventTypeId, start: u64, end: u64) -> EventInstance {
        EventInstance {
            type_id,
            start_time: start,
            end_time: end,
            thread_id: 1,
            fields: vec![],
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
        s.event_thresholds.insert(EventTypeId(1), Duration::from_millis(10));
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
        settings.event_thresholds.insert(EventTypeId(1), Duration::from_nanos(100));
        let mut rec = Recording::new(1, settings);
        rec.start();
        rec.record_event(make_event(EventTypeId(1), 1000, 1200)); // 200ns >= 100ns
        assert_eq!(rec.event_count(), 1);
    }

    #[test]
    fn test_recording_threshold_rejects_below() {
        let mut settings = RecordingSettings::new("r");
        settings.event_thresholds.insert(EventTypeId(1), Duration::from_nanos(500));
        let mut rec = Recording::new(1, settings);
        rec.start();
        rec.record_event(make_event(EventTypeId(1), 1000, 1100)); // 100ns < 500ns
        assert_eq!(rec.event_count(), 0);
    }

    #[test]
    fn test_recording_threshold_exact_boundary() {
        let mut settings = RecordingSettings::new("r");
        settings.event_thresholds.insert(EventTypeId(1), Duration::from_nanos(100));
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
            fields: vec![EventValue::Int(10), EventValue::String(Arc::from("gc"))],
        };
        rec.record_event(evt);
        let events = rec.get_events();
        assert_eq!(events[0].fields.len(), 2);
        assert_eq!(events[0].thread_id, 42);
    }

    #[test]
    fn test_recording_record_event_arc() {
        let mut rec = Recording::new(1, RecordingSettings::new("r"));
        rec.start();
        let evt = Arc::new(make_event(EventTypeId(1), 100, 200));
        rec.record_event_arc(Arc::clone(&evt));
        rec.record_event_arc(evt);
        assert_eq!(rec.event_count(), 2);
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
        fr.record_event(make_event(EventTypeId(1), 100, 200));
        assert_eq!(fr.get_recording(r1).unwrap().event_count(), 1);
        assert_eq!(fr.get_recording(r2).unwrap().event_count(), 1);
        assert_eq!(fr.get_recording(r3).unwrap().event_count(), 0);
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
}
