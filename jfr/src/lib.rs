// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

#![deny(
    clippy::missing_safety_doc,
    clippy::not_unsafe_ptr_arg_deref,
    clippy::undocumented_unsafe_blocks
)]

//! # cratonvm-jfr — Java Flight Recorder support for CratonVM
//!
//! ## Per-thread event rings
//!
//! High-throughput emit goes through [`push_to_thread_ring`], which writes to
//! a thread-local ring (capacity [`DEFAULT_THREAD_RING_CAPACITY`] = 1024).
//! Dump-time consumers call [`global_ring_registry`]`().drain_all()` to harvest
//! all threads' events.
//!
//! This avoids global `Mutex` contention on the hot emit path: every producer
//! thread owns its own shard (registered once on first emit), and the registry
//! mutex is only taken at registration time and during drainage.
//!
//! See [`ThreadEventRing`] for the local (non-registered) ring type, and
//! [`ThreadRingRegistry`] for the multi-thread aggregator used by the dumper.
//!
//! ---------------------------------------------------------------------------
//! # LIVENESS — observability audit, 2026-07-26
//! ---------------------------------------------------------------------------
//!
//! **No production code path starts a recording, so JFR captures nothing on a
//! default run.** The emit surface is genuinely wired — ~30 `emit_*` call
//! sites across the interpreter, the JIT, `vm_exec`, `vm_util` and `vm_init`
//! cover GC, class load, thread lifecycle, monitors, compilation,
//! deoptimization, file I/O and virtual-thread pinning — and a
//! `FlightRecorder` is constructed for every VM
//! (`SharedVm::debug.flight_recorder`). What is missing is the trigger:
//!
//!  * There is **no `-XX:StartFlightRecording` command-line option**. Nothing
//!    in `vm-cli` parses one.
//!  * `FlightRecorder::new_recording` / `start_recording` have **zero callers
//!    outside `#[cfg(test)]`**, so [`set_enabled`] is never flipped and
//!    [`is_enabled`] is permanently `false`.
//!  * The `JFR.start` / `JFR.stop` / `JFR.dump` jcmd verbs in
//!    `vm/src/runtime/serviceability.rs` never touched the recorder (they
//!    returned canned success strings; the audit replaced them with honest
//!    failures), and in any case nothing constructs the `JcmdProcessor` that
//!    hosts them, nor opens an attach socket.
//!  * [`dump_to_file`] has no caller outside this crate.
//!
//! Because every `emit_*` checks [`is_enabled`] first, the standing cost is
//! one relaxed atomic load per call site — the wiring is not a performance
//! problem, it is simply unreachable. The two deliberate exceptions that
//! bypass the gate (`emit_physical_memory_event` and
//! `emit_initial_environment_variable_event`, both one-shot from `vm_init` on
//! the main thread) push into that thread's bounded ring and stay there.
//!
//! To make JFR real, in dependency order: add the CLI option and/or bind the
//! jcmd verbs to `SharedVm::debug.flight_recorder`; call `new_recording` +
//! `start_recording`; and call `FlightRecorder::dump_recording` on stop. Note
//! that the produced file is still **not JMC / `jfr print` loadable** — see
//! the FORMAT-FIDELITY GAP block on `write_metadata_section` in
//! [`dump`] — and that no event carries a real captured stack trace.
//!
//! ## Memory bounds (audited)
//!
//!  * Per-thread emit ring: [`DEFAULT_THREAD_RING_CAPACITY`] = 1024 entries,
//!    fixed, oldest-dropped on overflow.
//!  * Per-recording repository: `EventRepository::default()` = 100_000 events
//!    with ring eviction. Bounded.
//!  * Dead shards: [`ThreadRingRegistry`] reclaims retired+empty shards, but
//!    only from inside `drain_all`. Since `drain_all` runs at dump time and no
//!    dump ever happens, reclamation never runs in production today. This is
//!    currently harmless *only* because the gate keeps ordinary threads from
//!    registering a shard at all. It becomes a real per-thread leak the moment
//!    a long-lived recording is started on a workload that churns threads —
//!    fix reclamation (or drive a periodic drain) as part of wiring the
//!    trigger, not after.
//!  * `RecordingSettings::max_age` / `max_size` are **not enforced anywhere**;
//!    see their declarations in [`recording`].

pub mod builtin;
pub mod dump;
pub mod event;
pub mod recording;
pub mod repository;
pub mod stream;

pub use dump::{dump_to_file, read_events, read_jfr_header, JfrDumpError, JfrFileHeader};
pub use event::*;
pub use recording::*;
pub use repository::*;
pub use stream::EventStream;

// Explicit re-exports of the per-thread ring API. These are also covered by
// the blanket `pub use repository::*;` above, but listing them here documents
// the public surface and guards against accidental removal from the glob.
pub use repository::{
    global_ring_registry, push_to_thread_ring, ThreadEventRing, ThreadRingRegistry,
    DEFAULT_THREAD_RING_CAPACITY,
};

use std::sync::atomic::{AtomicBool, Ordering};

/// Global JFR enabled flag — set when any recording starts, cleared when the
/// last one stops. Every `emit_*` function in `builtin.rs` checks this with a
/// single relaxed load + branch to skip the disabled-path work entirely.
///
/// This converts the disabled-path cost from ~30-100 ns/call (HashMap probe +
/// Vec alloc) to ~2-3 ns (one relaxed load + branch).
static JFR_ENABLED: AtomicBool = AtomicBool::new(false);

/// Returns whether any flight recording is currently running.
///
/// This is the fast-path check used by every `emit_*` function.
///
/// Round-5 CRIT-fix (Bug 2, 2026-05-17): uses `Acquire` ordering so it
/// correctly pairs with the `Release` store in `set_enabled`. Previously
/// this was `Relaxed`, which paired with nothing — on a weakly-ordered
/// architecture (ARM64, POWER) a producer could observe
/// `is_enabled() == true` while still seeing stale values for
/// type-registry slots, `cached_event_id` caches, or `running_ids` snapshots
/// that the recording-start path wrote *before* flipping the flag. The
/// observable bug was first-batch events on a fresh recording silently
/// dropping (or recording with an INVALID `type_id`) until the producer
/// happened to re-fetch the registry.
///
/// On x86/x86_64 (TSO) `Acquire` is identical in cost to `Relaxed` for a
/// simple load. ARM64 pays one LDAR — still cheap, and the correctness
/// pairing matters far more than the nanosecond difference per emit.
#[inline(always)]
pub fn is_enabled() -> bool {
    JFR_ENABLED.load(Ordering::Acquire)
}

/// Set the global JFR-enabled flag. Called by `FlightRecorder::start_recording`
/// and `stop_recording`. Uses `Release` ordering so writes that precede the
/// store (e.g. registry inserts during recording setup) are visible to readers
/// that subsequently observe `is_enabled() == true` via the matching
/// `Acquire` load in [`is_enabled`].
pub fn set_enabled(v: bool) {
    JFR_ENABLED.store(v, Ordering::Release);
}

/// Number of running recordings summed over every [`FlightRecorder`] in the
/// process. [`is_enabled`] is `true` exactly while this is non-zero.
static RUNNING_RECORDINGS: std::sync::atomic::AtomicIsize =
    std::sync::atomic::AtomicIsize::new(0);

/// Publish a recorder's change in running-recording count.
///
/// `JFR_ENABLED` is process-global but a recorder's running set is not, so
/// storing `!running_ids.is_empty()` directly — as `refresh_running_ids` used
/// to — lets whichever recorder transitioned last decide the flag for all of
/// them. With one recorder per process that is invisible; with several it is
/// not, and `cratonvm-vm`'s test binary builds a `SharedVm`, and therefore a
/// `FlightRecorder`, per test. A recorder with no recordings would call
/// `set_enabled(false)` and silently switch JFR off underneath a concurrent
/// test that had just started one, so every `emit_*` on that thread returned
/// at its `is_enabled()` gate and the recording came back empty.
///
/// Tracking the total instead makes the flag mean what it says: some recording
/// somewhere is running. Recorders report their own delta, so they compose.
pub(crate) fn publish_running_delta(prev: usize, now: usize) {
    let delta = now as isize - prev as isize;
    let total = if delta == 0 {
        RUNNING_RECORDINGS.load(Ordering::Acquire)
    } else {
        RUNNING_RECORDINGS.fetch_add(delta, Ordering::AcqRel) + delta
    };
    debug_assert!(total >= 0, "running-recording count went negative: {total}");
    set_enabled(total > 0);
}

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
            fields: smallvec::SmallVec::new(),
        }
    }

    fn make_event_with_fields(type_id: EventTypeId, fields: Vec<EventValue>) -> EventInstance {
        EventInstance {
            type_id,
            start_time: 1000,
            end_time: 2000,
            thread_id: 1,
            fields: smallvec::SmallVec::from_vec(fields),
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
        assert!(fr
            .type_registry
            .find_by_name("jdk.GarbageCollection")
            .is_some());
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
        settings
            .event_thresholds
            .insert(tid, Duration::from_nanos(500));

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
        // Round-4/5 (C33): `record_event` now writes into the per-thread
        // ring rather than directly into each recording's repository.
        // Tests must drain the rings via `drain_per_thread_into_repository`
        // before observing `event_count()`. Baseline drains keep cross-test
        // stragglers from leaking into our assertions.
        // Serialize against other tests that drain the global ring registry,
        // otherwise a concurrent `drain_all()` steals our events.
        let _g = crate::repository::jfr_test_guard();
        let mut fr = create_flight_recorder();
        let r1 = fr.new_recording(RecordingSettings::new("rec1"));
        let r2 = fr.new_recording(RecordingSettings::new("rec2"));
        fr.start_recording(r1);
        fr.start_recording(r2);
        assert_eq!(fr.active_recording_count(), 2);

        // Clear anything already in the per-thread rings before our first push.
        let _ = global_ring_registry().drain_all();

        let evt = make_event(EventTypeId(1), 100, 200);
        fr.record_event(evt);

        fr.drain_per_thread_into_repository();
        assert_eq!(fr.get_recording(r1).unwrap().event_count(), 1);
        assert_eq!(fr.get_recording(r2).unwrap().event_count(), 1);

        fr.stop_recording(r1);
        assert_eq!(fr.active_recording_count(), 1);

        // Drain again so the post-stop emit's effects can be observed
        // cleanly relative to the running r2 recording only.
        let _ = global_ring_registry().drain_all();

        let evt2 = make_event(EventTypeId(1), 300, 400);
        fr.record_event(evt2);
        fr.drain_per_thread_into_repository();
        assert_eq!(fr.get_recording(r1).unwrap().event_count(), 1);
        assert_eq!(fr.get_recording(r2).unwrap().event_count(), 2);
    }

    // --- FlightRecorder create/start/stop ---

    #[test]
    fn test_flight_recorder_create_start_stop() {
        // start/stop toggle the process-global `JFR_ENABLED` flag; hold the
        // test lock so we don't flip it under a concurrent `emit_*` test that
        // depends on `is_enabled()`.
        let _g = crate::repository::jfr_test_guard();
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

    // --- create_thread_recorder ---

    #[test]
    fn test_create_flight_recorder_has_events() {
        let fr = create_flight_recorder();
        assert!(fr.type_registry.find_by_name("jdk.ClassLoad").is_some());
        assert!(fr.type_registry.find_by_name("jdk.CPULoad").is_some());
        assert!(fr
            .type_registry
            .find_by_name("jdk.ExecutionSample")
            .is_some());
    }

    // -----------------------------------------------------------------------
    // Per-thread ring API smoke test
    //
    // Verifies the public surface (re-exported from `repository`) works
    // end-to-end:
    //   1. `set_enabled(true)` flips the global enable flag and `is_enabled()`
    //      reports it.
    //   2. `push_to_thread_ring(event)` writes the event into the calling
    //      thread's registered shard.
    //   3. The same event is observable through this thread's shard via the
    //      registry (`register_current_thread` is idempotent and returns the
    //      cached Arc).
    //
    // We do *not* assert on `global_ring_registry().drain_all()`'s count
    // directly, because the test binary may run other tests in parallel that
    // touch the global registry; instead we search for our uniquely-tagged
    // event in this thread's shard, which proves the wiring is correct and
    // remains race-free.
    // -----------------------------------------------------------------------

    #[test]
    fn smoke_test_thread_ring_public_api() {
        // Serialize against other tests that toggle `set_enabled` / drain the
        // global ring registry; both would race this test's state otherwise.
        let _g = crate::repository::jfr_test_guard();
        // (1) Toggle the global enable flag through the public API.
        let prior = is_enabled();
        set_enabled(true);
        assert!(
            is_enabled(),
            "set_enabled(true) should make is_enabled() return true"
        );

        // (2) Push a uniquely-tagged event through the public function.
        let unique_start: u64 = 0xCAFE_F00D_DEAD_BEEF;
        let unique_type = EventTypeId(0xA5A5_A5A5);
        let event = EventInstance {
            type_id: unique_type,
            start_time: unique_start,
            end_time: unique_start + 1,
            thread_id: 42,
            fields: smallvec::SmallVec::new(),
        };
        push_to_thread_ring(event);

        // (3) Confirm the event is reachable through the global registry —
        // by inspecting *this* thread's SPSC shard, avoiding cross-test
        // races on `drain_all`. Bug 2 fix: the shard is now an
        // `Arc<SpscEventRing>` instead of `Arc<Mutex<VecDeque<_>>>`, so we
        // drain it (consumer side) and search the drained vector.
        let shard = global_ring_registry().register_current_thread();
        let mut drained: Vec<EventInstance> = Vec::new();
        shard.drain_into(&mut drained);
        assert!(
            drained.iter().any(|e| e.start_time == unique_start && e.type_id == unique_type),
            "expected event with start_time={:#x} to be present in this thread's shard (drained {} events)",
            unique_start,
            drained.len(),
        );

        // Also confirm the default-capacity constant is the documented value
        // and that the registry honors it.
        assert_eq!(DEFAULT_THREAD_RING_CAPACITY, 1024);
        assert_eq!(
            global_ring_registry().shard_capacity(),
            DEFAULT_THREAD_RING_CAPACITY,
        );

        // Restore prior state so other tests don't see a forced-enabled flag.
        set_enabled(prior);
    }
}
