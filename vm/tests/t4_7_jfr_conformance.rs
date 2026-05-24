// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! T4.7 -- JFR Conformance Tests
//!
//! These tests verify that the CratonVM Java Flight Recorder implementation
//! conforms to the `jdk.jfr` API surface expected by OpenJDK 25.  They exercise
//! the Rust-side JFR crate (`cratonvm-jfr`) directly, covering:
//!
//! - Recording lifecycle (new -> start -> record -> stop -> close)
//! - Binary dump to the JFR v2.0 format with correct magic/version
//! - Live EventStream consumption over an active recording
//!
//! Run with:
//!
//!     cargo test -p cratonvm-vm --test t4_7_jfr_conformance
//!
//! Some tests are `#[ignore]` because they depend on full JFR wiring through the
//! VM.  The non-ignored tests exercise the JFR crate's public API directly.

use std::sync::Arc;

use cratonvm_jfr::event::{EventInstance, EventTypeId, EventValue};
use cratonvm_jfr::recording::{FlightRecorder, RecordingSettings, RecordingState};
use cratonvm_jfr::dump::{JFR_MAGIC, JFR_VERSION_MAJOR, JFR_VERSION_MINOR, HEADER_SIZE};
use cratonvm_jfr::stream::EventStream;
use cratonvm_jfr::create_flight_recorder;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Create a FlightRecorder pre-populated with built-in event types.
fn make_recorder() -> FlightRecorder {
    create_flight_recorder()
}

/// Build a simple event with the given type ID and timestamps.
fn make_event(type_id: EventTypeId, start: u64, end: u64) -> EventInstance {
    EventInstance {
        type_id,
        start_time: start,
        end_time: end,
        thread_id: 1,
        // Round-5 JFR Fix 1: `EventInstance.fields` is now
        // `SmallVec<[EventValue; 8]>` to avoid the per-event heap
        // allocation; tests construct an empty one the same way.
        fields: smallvec::SmallVec::new(),
    }
}

/// Build an event with field values.
#[allow(dead_code)]
fn make_event_with_fields(
    type_id: EventTypeId,
    start: u64,
    end: u64,
    fields: Vec<EventValue>,
) -> EventInstance {
    EventInstance {
        type_id,
        start_time: start,
        end_time: end,
        thread_id: 1,
        fields: smallvec::SmallVec::from_vec(fields),
    }
}

/// Create a temporary directory for JFR dump tests.
fn jfr_temp_dir(test_name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("cratonvm_t4_7_{test_name}"));
    let _ = std::fs::create_dir_all(&dir);
    dir
}

// ---------------------------------------------------------------------------
// T4.7.1 -- jfr_event_recording_start_stop
// ---------------------------------------------------------------------------

/// T4.7.1: Verify that a JFR Recording can be instantiated, started, used to
/// record events, and stopped.  This mirrors the Java-side lifecycle:
///
/// ```java
/// Recording r = new Recording();
/// r.start();
/// // ... emit events ...
/// r.stop();
/// ```
///
/// The test exercises the full Recording state machine (New -> Running ->
/// Stopped) and verifies that events are only captured while the recording
/// is in the Running state.
#[test]
fn t4_7_1_jfr_event_recording_start_stop() {
    let mut fr = make_recorder();

    // Phase 1: Create a new recording -- state should be New.
    let rid = fr.new_recording(RecordingSettings::new("t4_7_1_conformance"));
    {
        let rec = fr.get_recording(rid).expect("recording should exist after creation");
        assert_eq!(rec.state, RecordingState::New, "newly created recording must be in New state");
        assert!(rec.start_time.is_none(), "start_time must be None before start()");
        assert_eq!(rec.event_count(), 0, "no events should exist before start");
    }

    // Phase 2: Start the recording -- state transitions to Running.
    fr.start_recording(rid);
    {
        let rec = fr.get_recording(rid).unwrap();
        assert_eq!(rec.state, RecordingState::Running, "recording must be Running after start()");
        assert!(rec.start_time.is_some(), "start_time must be set after start()");
    }
    assert_eq!(fr.active_recording_count(), 1, "one recording should be active");

    // Phase 3: Record events via the built-in emitters.
    // Use the GC event as a representative built-in event.
    cratonvm_jfr::builtin::emit_gc_event(&mut fr, 1, "G1 Young", "Allocation Failure", 1_000_000, 500_000);
    cratonvm_jfr::builtin::emit_gc_event(&mut fr, 2, "G1 Mixed", "G1 Evacuation Pause", 2_000_000, 300_000);
    cratonvm_jfr::builtin::emit_thread_start_event(&mut fr, "worker-1", "main", 10, 3_000_000);
    cratonvm_jfr::builtin::emit_class_load_event(
        &mut fr,
        "java/lang/String",
        "bootstrap",
        "bootstrap",
        4_000_000,
        200_000,
    );

    {
        let rec = fr.get_recording(rid).unwrap();
        assert_eq!(rec.event_count(), 4, "four events should have been recorded while running");
    }

    // Phase 4: Stop the recording -- state transitions to Stopped.
    fr.stop_recording(rid);
    {
        let rec = fr.get_recording(rid).unwrap();
        assert_eq!(rec.state, RecordingState::Stopped, "recording must be Stopped after stop()");
        assert!(rec.stop_time.is_some(), "stop_time must be set after stop()");
    }
    assert_eq!(fr.active_recording_count(), 0, "no recordings should be active after stop");

    // Phase 5: Events recorded while stopped must be dropped.
    cratonvm_jfr::builtin::emit_gc_event(&mut fr, 3, "G1 Full", "System.gc()", 5_000_000, 1_000_000);
    {
        let rec = fr.get_recording(rid).unwrap();
        assert_eq!(
            rec.event_count(),
            4,
            "event count must not change after stop -- events emitted while stopped are dropped"
        );
    }

    // Phase 6: Verify the captured events are accessible and have correct structure.
    {
        let rec = fr.get_recording_mut(rid).unwrap();
        let events = rec.get_events();
        assert_eq!(events.len(), 4);

        // All events should have non-zero timestamps
        for (i, event) in events.iter().enumerate() {
            assert!(event.start_time > 0, "event {i} must have a positive start_time");
            assert!(event.end_time >= event.start_time, "event {i} end_time must be >= start_time");
        }
    }
}

/// T4.7.1 extended: Verify multiple simultaneous recordings each capture
/// events independently, matching the Java API where multiple Recording
/// objects can be active at the same time.
#[test]
fn t4_7_1_jfr_multiple_simultaneous_recordings() {
    let mut fr = make_recorder();

    let r1 = fr.new_recording(RecordingSettings::new("recording-A"));
    let r2 = fr.new_recording(RecordingSettings::new("recording-B"));

    fr.start_recording(r1);
    fr.start_recording(r2);
    assert_eq!(fr.active_recording_count(), 2);

    // Emit one event -- both recordings should capture it.
    cratonvm_jfr::builtin::emit_gc_event(&mut fr, 1, "G1", "Alloc", 1000, 500);

    assert_eq!(fr.get_recording(r1).unwrap().event_count(), 1);
    assert_eq!(fr.get_recording(r2).unwrap().event_count(), 1);

    // Stop r1, emit another event -- only r2 should capture.
    fr.stop_recording(r1);
    cratonvm_jfr::builtin::emit_gc_event(&mut fr, 2, "G1", "Alloc", 2000, 300);

    assert_eq!(fr.get_recording(r1).unwrap().event_count(), 1);
    assert_eq!(fr.get_recording(r2).unwrap().event_count(), 2);

    fr.stop_recording(r2);
}

// ---------------------------------------------------------------------------
// T4.7.2 -- jfr_recording_dump_produces_valid_file
// ---------------------------------------------------------------------------

/// T4.7.2: After stopping a recording, dump it to a file and verify:
/// - The file exists
/// - It starts with the JFR magic bytes `FLR\0`
/// - The version header reads 2.0
/// - File size is > 0
/// - The header contains valid checkpoint/metadata offsets
///
/// This mirrors `Recording.dump(Path)` from the Java API.
#[test]
fn t4_7_2_jfr_recording_dump_produces_valid_file() {
    let mut fr = make_recorder();

    let rid = fr.new_recording(RecordingSettings::new("t4_7_2_dump_test"));
    fr.start_recording(rid);

    // Emit a representative set of events to produce a non-trivial dump.
    cratonvm_jfr::builtin::emit_gc_event(&mut fr, 1, "G1 Young", "Allocation Failure", 1_000_000, 500_000);
    cratonvm_jfr::builtin::emit_thread_start_event(&mut fr, "main", "", 1, 2_000_000);
    cratonvm_jfr::builtin::emit_class_load_event(
        &mut fr,
        "java/lang/Object",
        "bootstrap",
        "bootstrap",
        3_000_000,
        100_000,
    );
    cratonvm_jfr::builtin::emit_compilation_event(
        &mut fr,
        "java/lang/String.hashCode",
        1,     // compile_id
        3,     // compile_level
        true,  // succeeded
        false, // is_osr
        256,   // code_size
        64,    // inlined_bytes
        4_000_000,
        50_000,
    );

    fr.stop_recording(rid);

    // Dump to file.
    let dir = jfr_temp_dir("dump_valid");
    let dump_path = dir.join("t4_7_2.jfr");

    let bytes_written = fr
        .dump_recording(rid, &dump_path)
        .expect("dump_recording must succeed for a stopped recording with events");

    // Assertion 1: The file exists.
    assert!(dump_path.exists(), "dump file must exist at {:?}", dump_path);

    // Assertion 2: File size is > 0.
    let metadata = std::fs::metadata(&dump_path).expect("must be able to stat the dump file");
    assert!(metadata.len() > 0, "dump file must not be empty");
    assert_eq!(
        metadata.len(),
        bytes_written,
        "reported bytes_written must match actual file size"
    );

    // Assertion 3: Magic bytes are FLR\0.
    let raw = std::fs::read(&dump_path).expect("must be able to read dump file");
    assert!(raw.len() >= 8, "dump file must be at least 8 bytes for magic + version");
    assert_eq!(
        &raw[0..4],
        &JFR_MAGIC,
        "first 4 bytes must be JFR magic: FLR\\0"
    );

    // Assertion 4: Version header is 2.0.
    let major = u16::from_be_bytes([raw[4], raw[5]]);
    let minor = u16::from_be_bytes([raw[6], raw[7]]);
    assert_eq!(major, JFR_VERSION_MAJOR, "JFR major version must be 2");
    // Round-5 JFR Fix 3: minor version is now 1 (delta-encoded timestamps).
    assert_eq!(minor, JFR_VERSION_MINOR, "JFR minor version must match crate constant");

    // Assertion 5: Parse the full header for structural validity.
    let header = cratonvm_jfr::read_jfr_header(&dump_path)
        .expect("read_jfr_header must succeed on a valid dump");
    assert_eq!(header.magic, JFR_MAGIC);
    assert_eq!(header.major, 2);
    assert_eq!(header.minor, 0);
    assert_eq!(header.file_size, bytes_written);
    assert_eq!(header.file_state, 1, "file_state must be COMPLETE (1)");
    assert_eq!(header.ticks_per_second, 1_000_000_000, "ticks_per_second must be 1e9 (nanoseconds)");

    // Structural: checkpoint is after the header, metadata is after checkpoint.
    assert!(
        header.checkpoint_offset >= HEADER_SIZE,
        "checkpoint must be at or after header end"
    );
    assert!(
        header.metadata_offset > header.checkpoint_offset,
        "metadata must come after checkpoint"
    );
    assert!(
        header.metadata_offset < header.file_size,
        "metadata offset must be within file bounds"
    );

    // Cleanup.
    let _ = std::fs::remove_file(&dump_path);
    let _ = std::fs::remove_dir(&dir);
}

/// T4.7.2 extended: Verify that dumping an empty (but stopped) recording still
/// produces a structurally valid JFR file.
#[test]
fn t4_7_2_jfr_dump_empty_recording_produces_valid_file() {
    let mut fr = make_recorder();
    let rid = fr.new_recording(RecordingSettings::new("empty_dump"));
    fr.start_recording(rid);
    // No events emitted.
    fr.stop_recording(rid);

    let dir = jfr_temp_dir("dump_empty");
    let dump_path = dir.join("empty.jfr");

    let bytes_written = fr.dump_recording(rid, &dump_path).expect("empty dump must succeed");
    assert!(bytes_written > 0, "even an empty dump must produce a non-zero file");

    let header = cratonvm_jfr::read_jfr_header(&dump_path).expect("header must be valid");
    assert_eq!(header.magic, JFR_MAGIC);
    assert_eq!(header.major, 2);
    assert_eq!(header.minor, 0);
    assert_eq!(header.file_state, 1);

    let _ = std::fs::remove_file(&dump_path);
    let _ = std::fs::remove_dir(&dir);
}

// ---------------------------------------------------------------------------
// T4.7.3 -- jfr_event_stream_consumes_events
// ---------------------------------------------------------------------------

/// T4.7.3: Verify that `EventStream` (the Rust-side equivalent of
/// `jdk.jfr.consumer.EventStream`) can open a recording's repository and
/// iterate over events as they are emitted.
///
/// The test exercises:
/// - Creating a stream from a live recording
/// - Polling for new events
/// - Type filtering
/// - Callback invocation
/// - Stream closure
#[test]
fn t4_7_3_jfr_event_stream_consumes_events() {
    let mut fr = make_recorder();

    // Look up a known built-in event type ID for filtering.
    // Verify that built-in event types are registered.
    assert!(
        fr.type_registry.find_by_name("jdk.GarbageCollection").is_some(),
        "jdk.GarbageCollection must be registered in built-in events"
    );

    let rid = fr.new_recording(RecordingSettings::new("t4_7_3_stream_test"));
    fr.start_recording(rid);

    // Create a stream that starts at the current position (no historical events).
    let mut stream = {
        let rec = fr.get_recording(rid).unwrap();
        EventStream::new(rec.repository())
    };

    // The stream should initially return no events.
    {
        let rec = fr.get_recording(rid).unwrap();
        let events = stream.poll(rec.repository());
        assert!(events.is_empty(), "stream should return no events before any are emitted");
    }

    // Emit events into the recording.
    cratonvm_jfr::builtin::emit_gc_event(&mut fr, 1, "G1 Young", "Alloc", 1_000_000, 500_000);
    cratonvm_jfr::builtin::emit_thread_start_event(&mut fr, "main", "", 1, 2_000_000);
    cratonvm_jfr::builtin::emit_gc_event(&mut fr, 2, "G1 Mixed", "Evacuation", 3_000_000, 300_000);

    // Poll the stream -- should see all 3 new events.
    {
        let rec = fr.get_recording(rid).unwrap();
        let events = stream.poll(rec.repository());
        assert_eq!(events.len(), 3, "stream must deliver all 3 events emitted since creation");
    }

    // Subsequent poll with no new events should return empty.
    {
        let rec = fr.get_recording(rid).unwrap();
        let events = stream.poll(rec.repository());
        assert!(events.is_empty(), "poll with no new events must return empty");
    }

    // Emit one more event, verify incremental delivery.
    cratonvm_jfr::builtin::emit_class_load_event(
        &mut fr,
        "java/lang/String",
        "boot",
        "boot",
        4_000_000,
        100_000,
    );
    {
        let rec = fr.get_recording(rid).unwrap();
        let events = stream.poll(rec.repository());
        assert_eq!(events.len(), 1, "exactly one new event should be delivered");
    }

    // Close the stream -- further polls must return empty.
    stream.close();
    assert!(stream.is_closed());
    {
        cratonvm_jfr::builtin::emit_gc_event(&mut fr, 3, "G1", "System.gc()", 5_000_000, 100_000);
        let rec = fr.get_recording(rid).unwrap();
        let events = stream.poll(rec.repository());
        assert!(events.is_empty(), "closed stream must not deliver events");
    }

    fr.stop_recording(rid);
}

/// T4.7.3 extended: Verify type-filtered streaming and callback invocation.
#[test]
fn t4_7_3_jfr_event_stream_filtered_with_callback() {
    let mut fr = make_recorder();

    let gc_type_id = fr
        .type_registry
        .find_by_name("jdk.GarbageCollection")
        .expect("jdk.GarbageCollection must exist");

    let rid = fr.new_recording(RecordingSettings::new("filtered_stream"));
    fr.start_recording(rid);

    // Create a stream from the start with a GC-only filter.
    let mut stream = {
        let rec = fr.get_recording(rid).unwrap();
        let mut s = EventStream::new(rec.repository());
        s.add_filter(gc_type_id);
        s
    };

    // Register a callback to count delivered events.
    let counter = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let counter_clone = counter.clone();
    stream.on_event(Box::new(move |_event| {
        counter_clone.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }));
    assert_eq!(stream.callback_count(), 1);

    // Emit a mix of event types.
    cratonvm_jfr::builtin::emit_gc_event(&mut fr, 1, "G1", "Alloc", 1000, 500);
    cratonvm_jfr::builtin::emit_thread_start_event(&mut fr, "worker-1", "main", 2, 2000);
    cratonvm_jfr::builtin::emit_gc_event(&mut fr, 2, "G1", "Alloc", 3000, 300);
    cratonvm_jfr::builtin::emit_class_load_event(&mut fr, "Foo", "app", "app", 4000, 100);

    // Poll: only GC events should pass the filter.
    {
        let rec = fr.get_recording(rid).unwrap();
        let events = stream.poll(rec.repository());
        assert_eq!(events.len(), 2, "only GC events should pass the type filter");
        for e in &events {
            assert_eq!(e.type_id, gc_type_id, "filtered events must have the GC type ID");
        }
    }

    // The callback should have been invoked exactly twice.
    assert_eq!(
        counter.load(std::sync::atomic::Ordering::Relaxed),
        2,
        "callback must be invoked once per delivered event"
    );

    fr.stop_recording(rid);
}

/// T4.7.3 extended: Verify `next_event()` delivers events one at a time.
#[test]
fn t4_7_3_jfr_event_stream_next_event() {
    let mut fr = make_recorder();
    let rid = fr.new_recording(RecordingSettings::new("next_event_test"));
    fr.start_recording(rid);

    let mut stream = EventStream::new_from_start();

    // Emit three events.
    cratonvm_jfr::builtin::emit_gc_event(&mut fr, 1, "G1", "A", 100, 50);
    cratonvm_jfr::builtin::emit_gc_event(&mut fr, 2, "G1", "B", 200, 50);
    cratonvm_jfr::builtin::emit_gc_event(&mut fr, 3, "G1", "C", 300, 50);

    let rec = fr.get_recording(rid).unwrap();
    let repo = rec.repository();

    // Consume one at a time.
    let e1 = stream.next_event(repo);
    assert!(e1.is_some(), "first event must be available");

    let e2 = stream.next_event(repo);
    assert!(e2.is_some(), "second event must be available");

    let e3 = stream.next_event(repo);
    assert!(e3.is_some(), "third event must be available");

    // No more events.
    let e4 = stream.next_event(repo);
    assert!(e4.is_none(), "no more events should be available");

    fr.stop_recording(rid);
}
