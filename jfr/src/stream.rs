// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Live event stream for consuming JFR events as they are emitted.
//!
//! `EventStream` provides an iterator/poll interface over a recording's event
//! repository.  It tracks a reader cursor so that each call to `poll` or
//! `next_event` returns only events that have been added since the last read.
//!
//! Optionally, an `EventStream` can be configured with:
//! - A set of event type filters (only those types are delivered)
//! - One or more callbacks that are invoked for each matching event

// AUDIT 2026-05-16: std HashSet unused (replaced by FxHashSet below).

use rustc_hash::FxHashSet;

use crate::event::{EventInstance, EventTypeId};
use crate::repository::EventRepository;

/// A callback that is invoked for each event delivered by the stream.
pub type EventCallback = Box<dyn FnMut(&EventInstance) + Send>;

/// Live event stream that tracks a read cursor over an `EventRepository`.
///
/// Usage pattern:
/// ```ignore
/// let mut stream = EventStream::new();
/// stream.add_filter(my_event_type_id);
/// // ... later, in a poll loop:
/// let events = stream.poll(&repo);
/// for event in &events {
///     // process event
/// }
/// ```
pub struct EventStream {
    /// Absolute index of the next event to read (matches `EventRepository::total_recorded`).
    read_index: u64,
    /// If non-empty, only events matching these type IDs are delivered.
    /// T10.9.B: FxHashSet — EventTypeId is internal.
    type_filters: FxHashSet<EventTypeId>,
    /// Registered callbacks invoked for every matching event during `poll`.
    callbacks: Vec<EventCallback>,
    /// Whether the stream is open.  Once closed, `poll` returns empty.
    closed: bool,
}

impl EventStream {
    /// Create a new event stream starting from the current end of the repository.
    ///
    /// Events that were already in the repository before this call are *not*
    /// delivered.  To read from the beginning, use `new_from_start`.
    pub fn new(repo: &EventRepository) -> Self {
        Self {
            read_index: repo.total_recorded(),
            type_filters: FxHashSet::default(),
            callbacks: Vec::new(),
            closed: false,
        }
    }

    /// Create a new event stream that starts reading from the very beginning
    /// of the repository (including events already present).
    pub fn new_from_start() -> Self {
        Self {
            read_index: 0,
            type_filters: FxHashSet::default(),
            callbacks: Vec::new(),
            closed: false,
        }
    }

    /// Add a type filter.  When at least one filter is set, only events whose
    /// `type_id` is in the filter set are delivered.
    pub fn add_filter(&mut self, type_id: EventTypeId) {
        self.type_filters.insert(type_id);
    }

    /// Remove a previously added type filter.
    pub fn remove_filter(&mut self, type_id: &EventTypeId) {
        self.type_filters.remove(type_id);
    }

    /// Clear all type filters (all event types will be delivered).
    pub fn clear_filters(&mut self) {
        self.type_filters.clear();
    }

    /// Returns true if this stream has been closed.
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// Close the stream.  Subsequent calls to `poll` / `next_event` return empty/None.
    pub fn close(&mut self) {
        self.closed = true;
        self.callbacks.clear();
    }

    /// Register a callback that will be invoked for each matching event during `poll`.
    pub fn on_event(&mut self, callback: EventCallback) {
        self.callbacks.push(callback);
    }

    /// Returns the number of registered callbacks.
    pub fn callback_count(&self) -> usize {
        self.callbacks.len()
    }

    /// Returns the current read cursor position (absolute index).
    pub fn read_position(&self) -> u64 {
        self.read_index
    }

    /// Poll the repository for new events since the last read.
    ///
    /// Returns cloned copies of all matching new events and advances the
    /// read cursor.  Also invokes any registered callbacks for each event.
    ///
    /// If the stream is closed, returns an empty vector.
    pub fn poll(&mut self, repo: &EventRepository) -> Vec<EventInstance> {
        if self.closed {
            return Vec::new();
        }

        let total = repo.total_recorded();
        if self.read_index >= total {
            return Vec::new();
        }

        let repo_len = repo.len() as u64;
        let base_index = total - repo_len;

        // If our read cursor has fallen behind the ring buffer (events were evicted),
        // jump forward to the oldest available event.
        let effective_start = if self.read_index < base_index {
            base_index
        } else {
            self.read_index
        };

        let relative_start = (effective_start - base_index) as usize;
        // Perf: previously `repo.iter().skip(relative_start)` walked the
        // VecDeque from the front on every poll — O(n) even when the cursor
        // was already near the end. `EventRepository::get` indexes the
        // backing VecDeque in O(1) (same fix already applied to
        // `next_event`). We iterate `[relative_start, repo_len)` directly.
        let repo_len_usize = repo.len();
        let mut results = Vec::with_capacity(repo_len_usize.saturating_sub(relative_start));

        for rel in relative_start..repo_len_usize {
            let event = match repo.get(rel) {
                Some(e) => e,
                None => break,
            };
            if !self.type_filters.is_empty() && !self.type_filters.contains(&event.type_id) {
                continue;
            }
            // Invoke callbacks
            for cb in &mut self.callbacks {
                cb(event);
            }
            results.push(event.clone());
        }

        self.read_index = total;
        results
    }

    /// Open a previously-dumped JFR recording on disk and populate a fresh
    /// in-memory `EventRepository` from its event records, so that the rest of
    /// the streaming API (`poll`, `next_event`, type filters, callbacks) works
    /// against it unchanged. This is the Rust analogue of
    /// `jdk.jfr.consumer.EventStream.openRepository(Path)`.
    ///
    /// The `registry` must describe every event type in the file; callers
    /// typically pass the same registry used to dump, or a registry populated
    /// via `register_builtin_events` for built-in event files.
    ///
    /// Returns the populated repository and a stream whose cursor is at the
    /// beginning so that every event on disk is delivered exactly once.
    pub fn open_repository(
        path: &std::path::Path,
        registry: &crate::event::EventTypeRegistry,
    ) -> Result<(EventRepository, Self), crate::dump::JfrDumpError> {
        let events = crate::dump::read_events(path, registry)?;
        // Capacity: fit every event (no eviction for replay).
        let mut repo = EventRepository::new(events.len().max(1));
        for ev in events {
            repo.push(ev);
        }
        let stream = Self {
            read_index: 0,
            type_filters: FxHashSet::default(),
            callbacks: Vec::new(),
            closed: false,
        };
        Ok((repo, stream))
    }

    /// Return the next single matching event, or `None` if no new events are available.
    ///
    /// This is a convenience method that advances the cursor by one event at a time.
    /// Non-matching events (filtered out by type) are skipped but still advance the cursor.
    pub fn next_event(&mut self, repo: &EventRepository) -> Option<EventInstance> {
        if self.closed {
            return None;
        }

        let total = repo.total_recorded();
        if self.read_index >= total {
            return None;
        }

        let repo_len = repo.len() as u64;
        let base_index = total - repo_len;

        // Jump past evicted events
        if self.read_index < base_index {
            self.read_index = base_index;
        }

        // Round-5 HIGH-fix (Bug 5, 2026-05-17): previously this loop called
        // `repo.iter().nth(rel)` per iteration, which walks the VecDeque
        // from the front every call — quadratic over a full filtered drain.
        // `EventRepository::get(rel)` is O(1).
        while self.read_index < total {
            let rel = (self.read_index - base_index) as usize;
            self.read_index += 1;

            if let Some(event) = repo.get(rel) {
                if self.type_filters.is_empty() || self.type_filters.contains(&event.type_id) {
                    for cb in &mut self.callbacks {
                        cb(event);
                    }
                    return Some(event.clone());
                }
            }
        }

        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{EventInstance, EventTypeId, EventValue};
    use smallvec::smallvec;
    use std::sync::Arc;

    fn make_event(type_id: EventTypeId, start: u64) -> EventInstance {
        EventInstance {
            type_id,
            start_time: start,
            end_time: start + 100,
            thread_id: 1,
            fields: smallvec![],
        }
    }

    #[allow(dead_code)]
    fn make_event_with_field(type_id: EventTypeId, start: u64, val: i32) -> EventInstance {
        EventInstance {
            type_id,
            start_time: start,
            end_time: start + 100,
            thread_id: 1,
            fields: smallvec![EventValue::Int(val)],
        }
    }

    // --- Basic stream creation ---

    #[test]
    fn test_stream_new_starts_at_end() {
        let mut repo = EventRepository::new(100);
        repo.push(make_event(EventTypeId(1), 100));
        repo.push(make_event(EventTypeId(1), 200));

        let stream = EventStream::new(&repo);
        assert_eq!(stream.read_position(), 2);
        assert!(!stream.is_closed());
    }

    #[test]
    fn test_stream_new_from_start() {
        let stream = EventStream::new_from_start();
        assert_eq!(stream.read_position(), 0);
    }

    // --- Polling ---

    #[test]
    fn test_poll_returns_new_events() {
        let mut repo = EventRepository::new(100);
        let mut stream = EventStream::new_from_start();

        repo.push(make_event(EventTypeId(1), 100));
        repo.push(make_event(EventTypeId(1), 200));

        let events = stream.poll(&repo);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].start_time, 100);
        assert_eq!(events[1].start_time, 200);
    }

    #[test]
    fn test_poll_only_returns_new_events_after_cursor() {
        let mut repo = EventRepository::new(100);
        repo.push(make_event(EventTypeId(1), 100));

        let mut stream = EventStream::new(&repo);
        // Stream starts at the end, so no events yet
        let events = stream.poll(&repo);
        assert_eq!(events.len(), 0);

        // Add a new event
        repo.push(make_event(EventTypeId(1), 200));
        let events = stream.poll(&repo);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].start_time, 200);

        // No more new events
        let events = stream.poll(&repo);
        assert_eq!(events.len(), 0);
    }

    #[test]
    fn test_poll_advances_cursor() {
        let mut repo = EventRepository::new(100);
        let mut stream = EventStream::new_from_start();

        repo.push(make_event(EventTypeId(1), 100));
        let _ = stream.poll(&repo);
        assert_eq!(stream.read_position(), 1);

        repo.push(make_event(EventTypeId(1), 200));
        repo.push(make_event(EventTypeId(1), 300));
        let _ = stream.poll(&repo);
        assert_eq!(stream.read_position(), 3);
    }

    #[test]
    fn test_poll_empty_repo() {
        let repo = EventRepository::new(100);
        let mut stream = EventStream::new_from_start();
        let events = stream.poll(&repo);
        assert_eq!(events.len(), 0);
    }

    // --- Type filtering ---

    #[test]
    fn test_poll_with_type_filter() {
        let mut repo = EventRepository::new(100);
        let mut stream = EventStream::new_from_start();
        stream.add_filter(EventTypeId(1));

        repo.push(make_event(EventTypeId(1), 100));
        repo.push(make_event(EventTypeId(2), 200));
        repo.push(make_event(EventTypeId(1), 300));
        repo.push(make_event(EventTypeId(3), 400));

        let events = stream.poll(&repo);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].start_time, 100);
        assert_eq!(events[1].start_time, 300);
    }

    #[test]
    fn test_poll_with_multiple_type_filters() {
        let mut repo = EventRepository::new(100);
        let mut stream = EventStream::new_from_start();
        stream.add_filter(EventTypeId(1));
        stream.add_filter(EventTypeId(3));

        repo.push(make_event(EventTypeId(1), 100));
        repo.push(make_event(EventTypeId(2), 200));
        repo.push(make_event(EventTypeId(3), 300));
        repo.push(make_event(EventTypeId(4), 400));

        let events = stream.poll(&repo);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].type_id, EventTypeId(1));
        assert_eq!(events[1].type_id, EventTypeId(3));
    }

    #[test]
    fn test_filter_no_match() {
        let mut repo = EventRepository::new(100);
        let mut stream = EventStream::new_from_start();
        stream.add_filter(EventTypeId(99));

        repo.push(make_event(EventTypeId(1), 100));
        repo.push(make_event(EventTypeId(2), 200));

        let events = stream.poll(&repo);
        assert_eq!(events.len(), 0);
    }

    #[test]
    fn test_remove_filter() {
        let mut repo = EventRepository::new(100);
        let mut stream = EventStream::new_from_start();
        stream.add_filter(EventTypeId(1));
        stream.add_filter(EventTypeId(2));
        stream.remove_filter(&EventTypeId(1));

        repo.push(make_event(EventTypeId(1), 100));
        repo.push(make_event(EventTypeId(2), 200));

        let events = stream.poll(&repo);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].type_id, EventTypeId(2));
    }

    #[test]
    fn test_clear_filters() {
        let mut repo = EventRepository::new(100);
        let mut stream = EventStream::new_from_start();
        stream.add_filter(EventTypeId(1));
        stream.clear_filters();

        repo.push(make_event(EventTypeId(1), 100));
        repo.push(make_event(EventTypeId(2), 200));

        // With no filters, all events pass
        let events = stream.poll(&repo);
        assert_eq!(events.len(), 2);
    }

    // --- Closing ---

    #[test]
    fn test_close_stops_delivery() {
        let mut repo = EventRepository::new(100);
        let mut stream = EventStream::new_from_start();

        repo.push(make_event(EventTypeId(1), 100));
        stream.close();
        assert!(stream.is_closed());

        let events = stream.poll(&repo);
        assert_eq!(events.len(), 0);
    }

    #[test]
    fn test_close_clears_callbacks() {
        let repo = EventRepository::new(100);
        let mut stream = EventStream::new(&repo);
        stream.on_event(Box::new(|_| {}));
        assert_eq!(stream.callback_count(), 1);
        stream.close();
        assert_eq!(stream.callback_count(), 0);
    }

    // --- Callbacks ---

    #[test]
    fn test_on_event_callback_invoked() {
        let mut repo = EventRepository::new(100);
        let mut stream = EventStream::new_from_start();

        let counter = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let counter_clone = counter.clone();
        stream.on_event(Box::new(move |_event| {
            counter_clone.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }));

        repo.push(make_event(EventTypeId(1), 100));
        repo.push(make_event(EventTypeId(1), 200));
        repo.push(make_event(EventTypeId(1), 300));

        let _ = stream.poll(&repo);
        assert_eq!(counter.load(std::sync::atomic::Ordering::Relaxed), 3);
    }

    #[test]
    fn test_callback_receives_correct_event_data() {
        let mut repo = EventRepository::new(100);
        let mut stream = EventStream::new_from_start();

        let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured_clone = captured.clone();
        stream.on_event(Box::new(move |event| {
            captured_clone.lock().unwrap().push(event.start_time);
        }));

        repo.push(make_event(EventTypeId(1), 100));
        repo.push(make_event(EventTypeId(1), 200));

        let _ = stream.poll(&repo);
        let times = captured.lock().unwrap().clone();
        assert_eq!(times, vec![100, 200]);
    }

    #[test]
    fn test_callback_respects_type_filter() {
        let mut repo = EventRepository::new(100);
        let mut stream = EventStream::new_from_start();
        stream.add_filter(EventTypeId(1));

        let counter = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let counter_clone = counter.clone();
        stream.on_event(Box::new(move |_| {
            counter_clone.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }));

        repo.push(make_event(EventTypeId(1), 100));
        repo.push(make_event(EventTypeId(2), 200)); // filtered out
        repo.push(make_event(EventTypeId(1), 300));

        let _ = stream.poll(&repo);
        assert_eq!(counter.load(std::sync::atomic::Ordering::Relaxed), 2);
    }

    #[test]
    fn test_multiple_callbacks() {
        let mut repo = EventRepository::new(100);
        let mut stream = EventStream::new_from_start();

        let c1 = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let c2 = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let c1c = c1.clone();
        let c2c = c2.clone();

        stream.on_event(Box::new(move |_| {
            c1c.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }));
        stream.on_event(Box::new(move |_| {
            c2c.fetch_add(10, std::sync::atomic::Ordering::Relaxed);
        }));
        assert_eq!(stream.callback_count(), 2);

        repo.push(make_event(EventTypeId(1), 100));
        let _ = stream.poll(&repo);

        assert_eq!(c1.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert_eq!(c2.load(std::sync::atomic::Ordering::Relaxed), 10);
    }

    // --- next_event ---

    #[test]
    fn test_next_event_one_at_a_time() {
        let mut repo = EventRepository::new(100);
        let mut stream = EventStream::new_from_start();

        repo.push(make_event(EventTypeId(1), 100));
        repo.push(make_event(EventTypeId(1), 200));
        repo.push(make_event(EventTypeId(1), 300));

        let e1 = stream.next_event(&repo).unwrap();
        assert_eq!(e1.start_time, 100);
        let e2 = stream.next_event(&repo).unwrap();
        assert_eq!(e2.start_time, 200);
        let e3 = stream.next_event(&repo).unwrap();
        assert_eq!(e3.start_time, 300);
        assert!(stream.next_event(&repo).is_none());
    }

    #[test]
    fn test_next_event_with_filter_skips_non_matching() {
        let mut repo = EventRepository::new(100);
        let mut stream = EventStream::new_from_start();
        stream.add_filter(EventTypeId(2));

        repo.push(make_event(EventTypeId(1), 100));
        repo.push(make_event(EventTypeId(2), 200));
        repo.push(make_event(EventTypeId(1), 300));

        let e = stream.next_event(&repo).unwrap();
        assert_eq!(e.start_time, 200);
        assert_eq!(e.type_id, EventTypeId(2));
        assert!(stream.next_event(&repo).is_none());
    }

    #[test]
    fn test_next_event_closed_returns_none() {
        let mut repo = EventRepository::new(100);
        let mut stream = EventStream::new_from_start();
        repo.push(make_event(EventTypeId(1), 100));
        stream.close();
        assert!(stream.next_event(&repo).is_none());
    }

    #[test]
    fn test_next_event_invokes_callback() {
        let mut repo = EventRepository::new(100);
        let mut stream = EventStream::new_from_start();

        let counter = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let counter_clone = counter.clone();
        stream.on_event(Box::new(move |_| {
            counter_clone.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }));

        repo.push(make_event(EventTypeId(1), 100));
        let _ = stream.next_event(&repo);
        assert_eq!(counter.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    // --- Ring buffer eviction handling ---

    #[test]
    fn test_stream_handles_eviction() {
        let mut repo = EventRepository::new(3); // small ring buffer
        let mut stream = EventStream::new_from_start();

        // Push 5 events; first 2 will be evicted
        for i in 0..5u64 {
            repo.push(make_event(EventTypeId(1), i * 100));
        }

        // Stream should deliver only the 3 remaining events (200, 300, 400)
        let events = stream.poll(&repo);
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].start_time, 200);
        assert_eq!(events[1].start_time, 300);
        assert_eq!(events[2].start_time, 400);
    }

    #[test]
    fn test_stream_cursor_jumps_past_evicted() {
        let mut repo = EventRepository::new(2);
        let mut stream = EventStream::new_from_start();

        repo.push(make_event(EventTypeId(1), 100));
        let _ = stream.poll(&repo); // cursor at 1
        assert_eq!(stream.read_position(), 1);

        // Push 3 more, evicting the first one and the second
        repo.push(make_event(EventTypeId(1), 200));
        repo.push(make_event(EventTypeId(1), 300));
        repo.push(make_event(EventTypeId(1), 400));

        // Cursor was at 1 but base is now 2, so it jumps to 2
        let events = stream.poll(&repo);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].start_time, 300);
        assert_eq!(events[1].start_time, 400);
    }

    // --- Incremental consumption ---

    #[test]
    fn test_incremental_poll() {
        let mut repo = EventRepository::new(100);
        let mut stream = EventStream::new_from_start();

        repo.push(make_event(EventTypeId(1), 100));
        let batch1 = stream.poll(&repo);
        assert_eq!(batch1.len(), 1);

        repo.push(make_event(EventTypeId(1), 200));
        repo.push(make_event(EventTypeId(1), 300));
        let batch2 = stream.poll(&repo);
        assert_eq!(batch2.len(), 2);

        // No new events
        let batch3 = stream.poll(&repo);
        assert_eq!(batch3.len(), 0);
    }

    // --- EventStream with real FlightRecorder ---

    #[test]
    fn test_stream_with_flight_recorder() {
        // Round-4/5 (C33): `emit_*` writes through the per-thread ring;
        // events are not visible in the recording's repository (and so not
        // in the stream's polled slice) until
        // `drain_per_thread_into_repository` runs. Each emit batch below is
        // followed by an explicit drain. Baseline drain at the start clears
        // any stragglers from other tests running in parallel.
        // Serialize against other global-ring tests so a concurrent emit/drain
        // can't perturb our exact event-count assertions.
        let _g = crate::repository::jfr_test_guard();
        let mut fr = crate::create_flight_recorder();
        let rid = fr.new_recording(crate::recording::RecordingSettings::new("stream-test"));
        fr.start_recording(rid);

        let _ = crate::repository::global_ring_registry().drain_all();

        let rec = fr.get_recording(rid).unwrap();
        let mut stream = EventStream::new(rec.repository());

        // Emit some events
        crate::builtin::emit_gc_event(&mut fr, 1, "G1", "Alloc", 1000, 500);
        crate::builtin::emit_thread_start_event(&mut fr, "main", "", 1, 2000);

        fr.drain_per_thread_into_repository();
        let rec = fr.get_recording(rid).unwrap();
        let events = stream.poll(rec.repository());
        assert_eq!(events.len(), 2);

        // Emit one more
        crate::builtin::emit_class_load_event(
            &mut fr,
            "java/lang/Object",
            "boot",
            "boot",
            3000,
            100,
        );

        fr.drain_per_thread_into_repository();
        let rec = fr.get_recording(rid).unwrap();
        let events = stream.poll(rec.repository());
        assert_eq!(events.len(), 1);

        // Close
        stream.close();
        let rec = fr.get_recording(rid).unwrap();
        let events = stream.poll(rec.repository());
        assert_eq!(events.len(), 0);
    }

    #[test]
    fn test_stream_filter_by_event_type_name() {
        // Same per-thread-ring contract as above: drain after emits before
        // the polled stream slice is materialised. The stream filter is what
        // we're actually exercising — it must accept only `gc_type_id`
        // events.
        // Serialize against other global-ring tests so a concurrent emit/drain
        // can't perturb our exact event-count assertions.
        let _g = crate::repository::jfr_test_guard();
        let mut fr = crate::create_flight_recorder();
        let gc_type_id = fr
            .type_registry
            .find_by_name("jdk.GarbageCollection")
            .unwrap();

        let rid = fr.new_recording(crate::recording::RecordingSettings::new("filter-test"));
        fr.start_recording(rid);

        let _ = crate::repository::global_ring_registry().drain_all();

        let rec = fr.get_recording(rid).unwrap();
        let mut stream = EventStream::new(rec.repository());
        stream.add_filter(gc_type_id);

        crate::builtin::emit_gc_event(&mut fr, 1, "G1", "Alloc", 1000, 500);
        crate::builtin::emit_thread_start_event(&mut fr, "main", "", 1, 2000);
        crate::builtin::emit_gc_event(&mut fr, 2, "G1", "Alloc", 3000, 300);

        fr.drain_per_thread_into_repository();
        let rec = fr.get_recording(rid).unwrap();
        let events = stream.poll(rec.repository());
        assert_eq!(events.len(), 2);
        for e in &events {
            assert_eq!(e.type_id, gc_type_id);
        }
    }

    // -----------------------------------------------------------------------
    // T6.2: EventStream::open_repository — disk roundtrip
    // -----------------------------------------------------------------------

    #[test]
    fn t62_open_repository_roundtrips_written_recording() {
        use crate::dump::dump_to_file;
        use crate::event::{EventField, EventPeriod, EventType, EventTypeRegistry};

        // Build a registry with one event type covering every primitive.
        let mut reg = EventTypeRegistry::new();
        let tid = reg.register(EventType {
            id: EventTypeId(0),
            name: "test.RoundTrip".into(),
            category: vec!["Test".into()],
            description: "roundtrip".into(),
            fields: vec![
                EventField::new("i", "int", ""),
                EventField::new("l", "long", ""),
                EventField::new("b", "boolean", ""),
                EventField::new("s", "string", ""),
                EventField::new("f", "float", ""),
                EventField::new("d", "double", ""),
            ],
            has_thread: false,
            has_stacktrace: false,
            period: EventPeriod::None,
            threshold: None,
        });

        let mut repo = EventRepository::new(10);
        repo.push(EventInstance {
            type_id: tid,
            start_time: 1_000,
            end_time: 1_500,
            thread_id: 7,
            fields: smallvec![
                EventValue::Int(-42),
                EventValue::Long(1 << 40),
                EventValue::Boolean(true),
                EventValue::String(Arc::from("hello")),
                EventValue::Float(3.25),
                EventValue::Double(2.5e10),
            ],
        });
        repo.push(EventInstance {
            type_id: tid,
            start_time: 2_000,
            end_time: 2_001,
            thread_id: 9,
            fields: smallvec![
                EventValue::Int(0),
                EventValue::Long(0),
                EventValue::Boolean(false),
                EventValue::Null,
                EventValue::Float(-1.0),
                EventValue::Double(-0.5),
            ],
        });

        let dir = std::env::temp_dir().join("jfr_stream_openrepo");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("stream_roundtrip.jfr");
        dump_to_file(&path, &repo, &reg, 1_000, 2_000, Vec::new(), false).unwrap();

        let (disk_repo, mut stream) = EventStream::open_repository(&path, &reg).unwrap();
        assert_eq!(disk_repo.len(), 2);

        let delivered = stream.poll(&disk_repo);
        assert_eq!(delivered.len(), 2);

        let first = &delivered[0];
        assert_eq!(first.type_id, tid);
        assert_eq!(first.start_time, 1_000);
        assert_eq!(first.end_time, 1_500);
        assert_eq!(first.thread_id, 7);
        match &first.fields[0] {
            EventValue::Int(v) => assert_eq!(*v, -42),
            _ => panic!("expected int"),
        }
        match &first.fields[1] {
            EventValue::Long(v) => assert_eq!(*v, 1i64 << 40),
            _ => panic!("expected long"),
        }
        match &first.fields[2] {
            EventValue::Boolean(v) => assert!(*v),
            _ => panic!("expected bool"),
        }
        match &first.fields[3] {
            EventValue::String(s) => assert_eq!(s.as_ref(), "hello"),
            _ => panic!("expected string"),
        }
        match &first.fields[4] {
            EventValue::Float(v) => assert!((*v - 3.25).abs() < 1e-6),
            _ => panic!("expected float"),
        }
        match &first.fields[5] {
            EventValue::Double(v) => assert!((*v - 2.5e10).abs() < 1.0),
            _ => panic!("expected double"),
        }

        let second = &delivered[1];
        assert!(matches!(second.fields[3], EventValue::Null));

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn t62_open_repository_rejects_unknown_event_type() {
        use crate::dump::dump_to_file;
        use crate::event::{EventField, EventPeriod, EventType, EventTypeRegistry};

        let mut writer_reg = EventTypeRegistry::new();
        let tid = writer_reg.register(EventType {
            id: EventTypeId(0),
            name: "writer.Only".into(),
            category: vec![],
            description: "".into(),
            fields: vec![EventField::new("i", "int", "")],
            has_thread: false,
            has_stacktrace: false,
            period: EventPeriod::None,
            threshold: None,
        });
        let mut repo = EventRepository::new(2);
        repo.push(EventInstance {
            type_id: tid,
            start_time: 1,
            end_time: 2,
            thread_id: 1,
            fields: smallvec![EventValue::Int(1)],
        });

        let dir = std::env::temp_dir().join("jfr_stream_unknown");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("unknown.jfr");
        dump_to_file(&path, &repo, &writer_reg, 0, 0, Vec::new(), false).unwrap();

        // Reader with a different registry can't resolve the type.
        let reader_reg = EventTypeRegistry::new();
        assert!(EventStream::open_repository(&path, &reader_reg).is_err());

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }
}
