// AUDIT 2026-05-16: std HashMap/HashSet are unused (replaced by FxHashMap/FxHashSet).
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rustc_hash::{FxHashMap, FxHashSet};

use crate::dump::{self, JfrDumpError};
use crate::event::{EventInstance, EventTypeId, EventTypeRegistry};
use crate::repository::{self, EventRepository};

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
    /// Accepts `Arc<EventInstance>` so the caller can share the event across
    /// multiple recordings without re-allocating between callees.
    ///
    /// Round-5 HIGH-fix (Bug 3, 2026-05-17): we previously called
    /// `Arc::try_unwrap` first, falling back to a deep clone on `Err`. In
    /// the multi-recording fan-out path (`drain_per_thread_into_repository`,
    /// `M > 1`) the caller deliberately keeps an Arc clone live across the
    /// loop body so every recording receives a sharing reference — that
    /// means `try_unwrap` *always* fails. We were paying for an atomic CAS
    /// (the failed unwrap) on top of the deep clone for every event for
    /// every recording. The CAS was pure waste.
    ///
    /// Now: try `Arc::into_inner` (single atomic, succeeds when this caller
    /// holds the unique Arc — common in tests and in the final iteration
    /// after the producer drops its own ref). Fall back to cloning the
    /// inner directly — strictly necessary because the repository stores
    /// owned values and other recordings still hold the Arc.
    ///
    /// Skip-event fast paths (state, enabled, threshold) happen *before*
    /// the unwrap/clone, so disabled recordings pay only an Arc deref +
    /// a refcount decrement on return.
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
        // Clone the inner up front when we are not the unique owner.
        // `Arc::strong_count` is approximate but a `> 1` answer is reliable
        // for "must clone": even if another ref drops between the check and
        // `into_inner`, that races a benign single-Arc fast path. We avoid
        // the CAS-style probe of `try_unwrap` either way.
        let owned = if Arc::strong_count(&event) == 1 {
            // Unique — `into_inner` succeeds without a clone.
            Arc::into_inner(event).expect("strong_count==1 implies into_inner succeeds")
        } else {
            // Shared — clone the inner directly. This is the cost we pay
            // when M recordings fan out the same event.
            (*event).clone()
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

    /// Round-9 CRIT-3 helper: full filter check (state + enabled + threshold)
    /// used by `drain_per_thread_into_repository` to decide whether to clone
    /// an event for this recording. Mirrors the inline checks at the top of
    /// `record_event` / `record_event_arc`.
    pub fn passes_filter(&self, event: &EventInstance) -> bool {
        if self.state != RecordingState::Running {
            return false;
        }
        if !self.is_event_enabled(event.type_id) {
            return false;
        }
        if let Some(&threshold) = self.settings.event_thresholds.get(&event.type_id) {
            let duration_ns = event.end_time.saturating_sub(event.start_time);
            if duration_ns < threshold.as_nanos() as u64 {
                return false;
            }
        }
        true
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
///
/// Performance note: `running_ids` caches the IDs of currently-running
/// recordings so `record_event` doesn't have to walk the `recordings` map
/// and allocate a Vec on every call. It is recomputed only when
/// `start_recording`/`stop_recording` is called. Combined with the
/// `crate::is_enabled()` fast-path at every `emit_*` site, this keeps
/// per-event overhead near zero when no recordings are active.
pub struct FlightRecorder {
    recordings: FxHashMap<u64, Recording>,
    pub type_registry: EventTypeRegistry,
    next_recording_id: u64,
    /// Cached snapshot of running recording IDs. Recomputed by
    /// `refresh_running_ids` after each state transition. `Vec::with_capacity(4)`
    /// keeps it allocation-free for typical workloads (1-4 concurrent recordings).
    running_ids: Vec<u64>,
}

impl FlightRecorder {
    pub fn new() -> Self {
        Self {
            recordings: FxHashMap::default(),
            type_registry: EventTypeRegistry::new(),
            next_recording_id: 1,
            running_ids: Vec::with_capacity(4),
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
        self.refresh_running_ids();
    }

    pub fn stop_recording(&mut self, id: u64) {
        if let Some(rec) = self.recordings.get_mut(&id) {
            rec.stop();
        }
        self.refresh_running_ids();
    }

    /// Recompute the cached running-recording IDs and update the global
    /// `JFR_ENABLED` flag accordingly. Called after every state transition.
    fn refresh_running_ids(&mut self) {
        self.running_ids.clear();
        for (&id, rec) in &self.recordings {
            if rec.state == RecordingState::Running {
                self.running_ids.push(id);
            }
        }
        crate::set_enabled(!self.running_ids.is_empty());
    }

    /// Record an event from the calling thread.
    ///
    /// In the per-thread-ring design, the hot emit path pushes the event onto
    /// the calling thread's bounded SPSC ring shard (see
    /// `repository::push_to_thread_ring`). The shard is a globally-registered
    /// `Arc<SpscEventRing>` — lock-free, one producer (this thread) + one
    /// consumer (the dump thread via `drain_per_thread_into_repository`).
    /// The per-recording repositories are populated lazily by
    /// `drain_per_thread_into_repository`, which is invoked by the dumper
    /// before producing a snapshot.
    ///
    /// Fast path: when no recordings are running this returns immediately
    /// after a single length check — no ring access, no allocation.
    ///
    /// Note on bounded capacity: each thread's ring holds at most
    /// `repository::DEFAULT_THREAD_RING_CAPACITY` (1024) events. Long-running
    /// threads emitting at high rates between drains will *drop their oldest
    /// events* to keep the producer non-blocking. This is intentional: JFR
    /// trades best-effort completeness for bounded memory and zero-blocking
    /// emit. The dumper should drain frequently enough to keep losses
    /// negligible for typical workloads.
    pub fn record_event(&mut self, event: EventInstance) {
        // Fast path: no running recordings — drop on the floor. This mirrors
        // the previous behaviour and matches the global `JFR_ENABLED` gate
        // maintained by `refresh_running_ids`.
        if self.running_ids.is_empty() {
            return;
        }
        // Hot path: push to this thread's bounded ring shard. The shard is
        // drained into per-recording repositories by
        // `drain_per_thread_into_repository`, typically called from the
        // dumper. This keeps emit O(1) and uncontended across threads.
        repository::push_to_thread_ring(event);
    }

    /// Drain every registered per-thread ring shard and forward the merged
    /// event stream into each currently-running recording's repository,
    /// applying per-recording event-enabled filters and thresholds.
    ///
    /// This is the bridge between the lock-free hot-path (per-thread rings)
    /// and the legacy per-recording `EventRepository`. The dumper calls this
    /// immediately before producing a snapshot so that any events emitted
    /// since the last dump become visible.
    ///
    /// Ordering: events are emitted in thread-local order within each shard,
    /// but no global ordering is enforced across shards. The dumper sorts by
    /// `start_time` if a time-ordered stream is required (see `dump.rs`).
    pub fn drain_per_thread_into_repository(&mut self) {
        let drained = repository::global_ring_registry().drain_all();
        if drained.is_empty() {
            return;
        }
        let running_len = self.running_ids.len();
        if running_len == 0 {
            // No active recordings: discard. Producers may have pushed events
            // after the last `stop_recording`; respecting the global-disabled
            // gate, we drop them rather than retaining stale data.
            return;
        }

        if running_len == 1 {
            // Single recording — move each event in directly, no Arc.
            // `record_event` applies the per-recording enabled_events and
            // threshold filters; no fan-out routing needed.
            let id = self.running_ids[0];
            if let Some(rec) = self.recordings.get_mut(&id) {
                for ev in drained {
                    rec.record_event(ev);
                }
            }
        } else {
            // Round-9 CRIT-3 fix (2026-05-17): route events per-recording
            // with the per-recording filter applied *during* the drain pass.
            // Previously the fan-out Arc-cloned every event for every
            // recording, then each recording's `record_event_arc` filtered
            // and dropped — wasting Arc clones for events that no recording
            // wanted, and (more importantly) making it impossible to give
            // ownership of a uniquely-routed event to its sole recipient
            // without an extra clone.
            //
            // New shape: outer loop over recordings, inner loop over events.
            // Each recording's `passes_filter` check (cheap — hashset lookup
            // + threshold compare) gates the per-event clone. The last
            // recording moves events out of `drained` rather than cloning,
            // matching the previous "move into final recipient" optimization.
            //
            // Round-9 MED-7 fix (2026-05-17): iterate `&self.running_ids`
            // instead of cloning a fresh `Vec<u64>` every pass. Split the
            // borrow into local refs so the `&self.running_ids` iterator
            // does not conflict with `&mut self.recordings.get_mut`.
            let recordings = &mut self.recordings;
            let running_ids = &self.running_ids;
            let n = running_ids.len();
            // Fan out to all but the last recording with clones gated by the
            // recording's enabled_events + threshold filter.
            for &id in &running_ids[..n - 1] {
                if let Some(rec) = recordings.get_mut(&id) {
                    for ev in drained.iter() {
                        if rec.passes_filter(ev) {
                            rec.record_event(ev.clone());
                        }
                    }
                }
            }
            // Final recipient takes ownership of the drained Vec — events
            // that pass the filter are moved in; the rest drop in place.
            let last_id = running_ids[n - 1];
            if let Some(rec) = recordings.get_mut(&last_id) {
                for ev in drained {
                    rec.record_event(ev);
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
    ///
    /// This method first drains all per-thread ring shards into the
    /// per-recording repositories so that pending events are reflected in
    /// the snapshot. The drained events are fanned out to every running
    /// recording by `drain_per_thread_into_repository`, so we pass an empty
    /// `extra_events` to `dump_to_file` — Bug 1 fix: re-draining the global
    /// ring inside the writer would steal events from sibling recordings.
    pub fn dump_recording(&mut self, id: u64, path: &Path) -> Result<u64, JfrDumpError> {
        // Flush any events sitting in per-thread rings into the per-recording
        // repositories before snapshotting. After this call, every running
        // recording owns its own copy of the just-drained events.
        self.drain_per_thread_into_repository();

        let rec = self.recordings.get_mut(&id).ok_or(JfrDumpError::Io(
            std::io::Error::new(std::io::ErrorKind::NotFound, "recording not found"),
        ))?;

        // Round-5: collapse three linear passes (min start_time, max end_time,
        // and the `make_contiguous` slice materialization that the old
        // `repository_mut().events()` call performed) into one fold over the
        // iterator. The downstream dumper consumes events via
        // `EventRepository::iter`, so a contiguous slice is not required.
        let (start_time, end_time) = rec.repository().iter().fold(
            (u64::MAX, 0u64),
            |(min_s, max_e), e| (min_s.min(e.start_time), max_e.max(e.end_time)),
        );
        let start_time = if start_time == u64::MAX { 0 } else { start_time };
        let duration = end_time.saturating_sub(start_time);

        // `extra_events = Vec::new()` — the recording's repository already
        // contains every event it should see. See Bug 1 fix in dump.rs.
        // `durable: true` — user-initiated dumps fsync the file before
        // rename for crash durability. Periodic snapshot paths pass `false`.
        dump::dump_to_file(
            path,
            rec.repository(),
            &self.type_registry,
            start_time,
            duration,
            Vec::new(),
            true,
        )
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

    #[test]
    fn record_event_writes_to_per_thread_ring() {
        // Verify that `record_event` routes through the per-thread ring and
        // that `drain_per_thread_into_repository` makes the event visible in
        // the recording's repository.
        let mut fr = FlightRecorder::new();
        let id = fr.new_recording(RecordingSettings::new("ring"));
        fr.start_recording(id);

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
