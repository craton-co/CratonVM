use super::*;
use smallvec::smallvec;

fn make_event(type_id: EventTypeId, start: u64, end: u64) -> EventInstance {
    EventInstance {
        type_id,
        start_time: start,
        end_time: end,
        thread_id: 1,
        fields: smallvec![],
    }
}

#[test]
fn test_new_repository() {
    let repo = EventRepository::new(10);
    assert_eq!(repo.len(), 0);
    assert!(repo.is_empty());
    assert_eq!(repo.total_recorded(), 0);
}

#[test]
fn test_default_repository() {
    let repo = EventRepository::default();
    assert_eq!(repo.len(), 0);
    assert!(repo.is_empty());
}

#[test]
fn test_push_single() {
    let mut repo = EventRepository::new(10);
    repo.push(make_event(EventTypeId(1), 100, 200));
    assert_eq!(repo.len(), 1);
    assert!(!repo.is_empty());
    assert_eq!(repo.total_recorded(), 1);
}

#[test]
fn test_push_multiple() {
    let mut repo = EventRepository::new(10);
    for i in 0..5 {
        repo.push(make_event(EventTypeId(1), i * 100, i * 100 + 50));
    }
    assert_eq!(repo.len(), 5);
    assert_eq!(repo.total_recorded(), 5);
}

#[test]
fn test_eviction_at_capacity() {
    let mut repo = EventRepository::new(3);
    for i in 0..5u64 {
        repo.push(make_event(EventTypeId(1), i * 100, i * 100 + 50));
    }
    assert_eq!(repo.len(), 3);
    assert_eq!(repo.total_recorded(), 5);
    // Oldest events should be evicted
    let events = repo.events();
    assert_eq!(events[0].start_time, 200);
    assert_eq!(events[1].start_time, 300);
    assert_eq!(events[2].start_time, 400);
}

#[test]
fn test_eviction_capacity_one() {
    let mut repo = EventRepository::new(1);
    repo.push(make_event(EventTypeId(1), 100, 200));
    repo.push(make_event(EventTypeId(1), 300, 400));
    assert_eq!(repo.len(), 1);
    assert_eq!(repo.total_recorded(), 2);
    assert_eq!(repo.events()[0].start_time, 300);
}

#[test]
fn test_events_returns_slice() {
    let mut repo = EventRepository::new(10);
    repo.push(make_event(EventTypeId(1), 100, 200));
    repo.push(make_event(EventTypeId(2), 300, 400));
    let events = repo.events();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].type_id, EventTypeId(1));
    assert_eq!(events[1].type_id, EventTypeId(2));
}

#[test]
fn test_clear() {
    let mut repo = EventRepository::new(10);
    repo.push(make_event(EventTypeId(1), 100, 200));
    repo.push(make_event(EventTypeId(1), 200, 300));
    assert_eq!(repo.len(), 2);
    repo.clear();
    assert_eq!(repo.len(), 0);
    assert!(repo.is_empty());
    // total_recorded is NOT cleared by clear()
    assert_eq!(repo.total_recorded(), 2);
}

#[test]
fn test_events_by_type_single_type() {
    let mut repo = EventRepository::new(10);
    let t1 = EventTypeId(1);
    repo.push(make_event(t1, 100, 200));
    repo.push(make_event(t1, 200, 300));
    let matched = repo.events_by_type(t1);
    assert_eq!(matched.len(), 2);
}

#[test]
fn test_events_by_type_mixed() {
    let mut repo = EventRepository::new(10);
    let t1 = EventTypeId(1);
    let t2 = EventTypeId(2);
    let t3 = EventTypeId(3);
    repo.push(make_event(t1, 100, 200));
    repo.push(make_event(t2, 200, 300));
    repo.push(make_event(t1, 300, 400));
    repo.push(make_event(t3, 400, 500));
    assert_eq!(repo.events_by_type(t1).len(), 2);
    assert_eq!(repo.events_by_type(t2).len(), 1);
    assert_eq!(repo.events_by_type(t3).len(), 1);
}

#[test]
fn test_events_by_type_none_found() {
    let mut repo = EventRepository::new(10);
    repo.push(make_event(EventTypeId(1), 100, 200));
    assert_eq!(repo.events_by_type(EventTypeId(99)).len(), 0);
}

#[test]
fn test_events_by_type_empty_repo() {
    let repo = EventRepository::new(10);
    assert_eq!(repo.events_by_type(EventTypeId(1)).len(), 0);
}

#[test]
fn test_events_in_range_inclusive() {
    let mut repo = EventRepository::new(10);
    repo.push(make_event(EventTypeId(1), 100, 150));
    repo.push(make_event(EventTypeId(1), 200, 250));
    repo.push(make_event(EventTypeId(1), 300, 350));
    // Range [200, 300] should include events at start_time 200 and 300
    let in_range = repo.events_in_range(200, 300);
    assert_eq!(in_range.len(), 2);
}

#[test]
fn test_events_in_range_none_match() {
    let mut repo = EventRepository::new(10);
    repo.push(make_event(EventTypeId(1), 100, 150));
    repo.push(make_event(EventTypeId(1), 500, 550));
    let in_range = repo.events_in_range(200, 400);
    assert_eq!(in_range.len(), 0);
}

#[test]
fn test_events_in_range_exact_boundary() {
    let mut repo = EventRepository::new(10);
    repo.push(make_event(EventTypeId(1), 100, 150));
    // Range [100, 100] should include event at start_time 100
    let in_range = repo.events_in_range(100, 100);
    assert_eq!(in_range.len(), 1);
}

#[test]
fn test_events_in_range_empty_repo() {
    let repo = EventRepository::new(10);
    assert_eq!(repo.events_in_range(0, 1000).len(), 0);
}

#[test]
fn test_total_recorded_persists_after_eviction() {
    let mut repo = EventRepository::new(2);
    for i in 0..10u64 {
        repo.push(make_event(EventTypeId(1), i, i + 1));
    }
    assert_eq!(repo.len(), 2);
    assert_eq!(repo.total_recorded(), 10);
}

#[test]
fn test_push_after_clear() {
    let mut repo = EventRepository::new(10);
    repo.push(make_event(EventTypeId(1), 100, 200));
    repo.clear();
    repo.push(make_event(EventTypeId(2), 300, 400));
    assert_eq!(repo.len(), 1);
    assert_eq!(repo.events()[0].type_id, EventTypeId(2));
}

#[test]
fn test_type_index_after_eviction() {
    let mut repo = EventRepository::new(2);
    repo.push(make_event(EventTypeId(1), 100, 200));
    repo.push(make_event(EventTypeId(2), 200, 300));
    repo.push(make_event(EventTypeId(1), 300, 400));
    // First event (type 1) was evicted, only the new type 1 remains
    assert_eq!(repo.events_by_type(EventTypeId(1)).len(), 1);
    assert_eq!(repo.events_by_type(EventTypeId(2)).len(), 1);
}

#[test]
fn test_type_index_cleared_on_clear() {
    let mut repo = EventRepository::new(10);
    repo.push(make_event(EventTypeId(1), 100, 200));
    repo.clear();
    assert_eq!(repo.events_by_type(EventTypeId(1)).len(), 0);
}

#[test]
fn test_iter() {
    let mut repo = EventRepository::new(10);
    repo.push(make_event(EventTypeId(1), 100, 200));
    repo.push(make_event(EventTypeId(2), 200, 300));
    let collected: Vec<_> = repo.iter().collect();
    assert_eq!(collected.len(), 2);
    assert_eq!(collected[0].type_id, EventTypeId(1));
    assert_eq!(collected[1].type_id, EventTypeId(2));
}

// -----------------------------------------------------------------------
// ThreadEventRing tests
// -----------------------------------------------------------------------

#[test]
fn thread_event_ring_push_and_drain_round_trip() {
    let ring = ThreadEventRing::new(8);
    assert!(ring.is_empty());
    ring.push(make_event(EventTypeId(1), 100, 200));
    ring.push(make_event(EventTypeId(2), 300, 400));
    assert_eq!(ring.len(), 2);

    let mut out = Vec::new();
    ring.drain_into(&mut out);
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].start_time, 100);
    assert_eq!(out[1].start_time, 300);
    assert!(ring.is_empty());
}

#[test]
fn thread_event_ring_drops_oldest_on_overflow() {
    let ring = ThreadEventRing::new(3);
    for i in 0..5u64 {
        ring.push(make_event(EventTypeId(1), i * 100, i * 100 + 50));
    }
    assert_eq!(ring.len(), 3);
    let mut out = Vec::new();
    ring.drain_into(&mut out);
    // Expect the last 3 inserts (start_time 200, 300, 400).
    let starts: Vec<u64> = out.iter().map(|e| e.start_time).collect();
    assert_eq!(starts, vec![200, 300, 400]);
}

#[test]
fn thread_event_ring_zero_capacity_is_clamped_to_one() {
    let ring = ThreadEventRing::new(0);
    assert_eq!(ring.capacity(), 1);
    ring.push(make_event(EventTypeId(1), 100, 200));
    ring.push(make_event(EventTypeId(1), 300, 400));
    assert_eq!(ring.len(), 1);
}

// -----------------------------------------------------------------------
// ThreadRingRegistry / push_to_thread_ring tests
// -----------------------------------------------------------------------

#[test]
fn push_to_thread_ring_records_event_visible_to_drain_all() {
    // Serialize against other global-ring tests: a concurrent `drain_all`
    // could steal this thread's event before we inspect our own shard.
    let _g = jfr_test_guard();
    // Push an event with a unique tag, then verify it is reachable through
    // the same Arc shard that `register_current_thread` returns. We pop
    // every event out of the shard (SPSC consumer side) and search the
    // popped vector — this is destructive, but the test owns this
    // thread's shard for its duration so that's fine.
    //
    // The shard may already contain residue from prior tests on this
    // thread (the thread-local cell persists for the whole test binary),
    // so we drain everything and look for the unique start_time tag.
    let unique_start = 0xDEAD_BEEF_u64;
    push_to_thread_ring(make_event(EventTypeId(7), unique_start, unique_start + 1));

    // Re-fetch the same shard — `register_current_thread` is idempotent
    // and returns the cached Arc for the current thread.
    let shard = global_ring_registry().register_current_thread();
    let mut drained: Vec<EventInstance> = Vec::new();
    shard.drain_into(&mut drained);
    assert!(
        drained
            .iter()
            .any(|e| e.start_time == unique_start && e.type_id == EventTypeId(7)),
        "expected pushed event to be visible in this thread's shard (drained {} events)",
        drained.len(),
    );
}

#[test]
fn ring_drops_newest_on_overflow() {
    // The SPSC ring drops the *newest* event on overflow (the prior
    // VecDeque-backed shard dropped the oldest, but a true SPSC ring
    // cannot move the consumer-owned `tail` from the producer thread).
    // Capacity is rounded up to the next power of two, so a request of
    // 3 actually gives 4. Verify the first 4 events make it in and the
    // 5th is dropped.
    let ring = SpscEventRing::new(3);
    assert_eq!(
        ring.capacity(),
        4,
        "capacity should round up to a power of two"
    );
    for i in 0..4u64 {
        ring.push(make_event(EventTypeId(1), i * 100, i * 100 + 50))
            .expect("not full yet");
    }
    // 5th push should fail (drop-newest).
    let rejected = ring.push(make_event(EventTypeId(1), 4 * 100, 4 * 100 + 50));
    assert!(
        rejected.is_err(),
        "ring should be full after `capacity` pushes"
    );

    let mut out = Vec::new();
    ring.drain_into(&mut out);
    let starts: Vec<u64> = out.iter().map(|e| e.start_time).collect();
    assert_eq!(starts, vec![0, 100, 200, 300]);
}

#[test]
fn spsc_ring_capacity_rounded_to_power_of_two() {
    assert_eq!(SpscEventRing::new(1).capacity(), 1);
    assert_eq!(SpscEventRing::new(2).capacity(), 2);
    assert_eq!(SpscEventRing::new(3).capacity(), 4);
    assert_eq!(SpscEventRing::new(5).capacity(), 8);
    assert_eq!(SpscEventRing::new(1024).capacity(), 1024);
    // Zero is clamped to 1.
    assert_eq!(SpscEventRing::new(0).capacity(), 1);
}

#[test]
fn spsc_ring_push_pop_round_trip() {
    let ring = SpscEventRing::new(8);
    assert!(ring.is_empty());
    ring.push(make_event(EventTypeId(1), 100, 200)).unwrap();
    ring.push(make_event(EventTypeId(2), 300, 400)).unwrap();
    assert_eq!(ring.len(), 2);

    let a = ring.try_pop().unwrap();
    assert_eq!(a.start_time, 100);
    let b = ring.try_pop().unwrap();
    assert_eq!(b.start_time, 300);
    assert!(ring.try_pop().is_none());
    assert!(ring.is_empty());
}

#[test]
fn spsc_ring_wraps_around() {
    // Push, pop, push again past the original capacity, verify we get
    // every pushed event back in FIFO order.
    let ring = SpscEventRing::new(4);
    for i in 0..3u64 {
        ring.push(make_event(EventTypeId(1), i, i + 1)).unwrap();
    }
    // Pop two so head moves ahead of tail.
    assert_eq!(ring.try_pop().unwrap().start_time, 0);
    assert_eq!(ring.try_pop().unwrap().start_time, 1);
    // Now push three more — index arithmetic must wrap correctly.
    for i in 3..6u64 {
        ring.push(make_event(EventTypeId(1), i, i + 1)).unwrap();
    }
    let mut out = Vec::new();
    ring.drain_into(&mut out);
    let starts: Vec<u64> = out.iter().map(|e| e.start_time).collect();
    assert_eq!(starts, vec![2, 3, 4, 5]);
}

#[test]
fn multiple_threads_register_independently() {
    use std::sync::Arc;
    use std::sync::Barrier;
    use std::thread;

    let registry = Arc::new(ThreadRingRegistry::new(64));
    let n_threads = 4usize;
    let barrier = Arc::new(Barrier::new(n_threads));

    let mut handles = Vec::with_capacity(n_threads);
    for t in 0..n_threads {
        let reg = Arc::clone(&registry);
        let b = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            b.wait();
            let ring = reg.register_current_thread();
            // Each thread pushes a unique-tagged event into its own SPSC
            // shard (single producer = this thread).
            ring.push(make_event(
                EventTypeId(t as u32 + 1),
                1000 + t as u64,
                2000 + t as u64,
            ))
            .expect("SPSC ring should have room for one event");
        }));
    }
    for h in handles {
        h.join().expect("worker panicked");
    }

    // Each worker thread should have registered its own distinct shard.
    assert_eq!(registry.registered_thread_count(), n_threads);

    // `drain_all` is the single consumer for every shard (SPSC invariant).
    let drained = registry.drain_all();
    assert_eq!(drained.len(), n_threads);
    // Verify every type_id 1..=n_threads is represented exactly once.
    for t in 0..n_threads {
        let expected = EventTypeId(t as u32 + 1);
        let count = drained.iter().filter(|e| e.type_id == expected).count();
        assert_eq!(
            count, 1,
            "expected exactly one event for type {:?}",
            expected
        );
    }
}

#[test]
fn same_thread_registries_have_independent_tls_shards() {
    let registry_a = ThreadRingRegistry::new(8);
    let registry_b = ThreadRingRegistry::new(8);

    let ring_a = registry_a.register_current_thread();
    ring_a.push(make_event(EventTypeId(1), 10, 11)).unwrap();

    let ring_b = registry_b.register_current_thread();
    ring_b.push(make_event(EventTypeId(2), 20, 21)).unwrap();

    assert!(
        !Arc::ptr_eq(&ring_a, &ring_b),
        "same-thread registration in distinct registries must not reuse a TLS shard"
    );
    assert_eq!(registry_a.registered_thread_count(), 1);
    assert_eq!(registry_b.registered_thread_count(), 1);

    let drained_a = registry_a.drain_all();
    let drained_b = registry_b.drain_all();

    assert_eq!(drained_a.len(), 1);
    assert_eq!(drained_a[0].type_id, EventTypeId(1));
    assert_eq!(drained_b.len(), 1);
    assert_eq!(drained_b[0].type_id, EventTypeId(2));
}

// Task #31: SpscEventRing::Drop must be bounded and must not leak the
// entire ring on a wedged consumer. The two tests below exercise both
// ends of the contract: the wedged-consumer fast-fail path, and the
// active-consumer happy path.

#[test]
fn drop_with_wedged_consumer_finishes_within_timeout() {
    use std::sync::atomic::Ordering;
    use std::sync::Arc;

    // Use a short timeout so the test runs quickly. The default 1s is
    // fine for production but inflates CI time unnecessarily for tests.
    let ring = Arc::new(SpscEventRing::with_shutdown_timeout(
        8,
        Duration::from_millis(200),
    ));

    // Push a few events so the Drop path has real slots to walk. Use
    // dynamically-allocated `Arc<str>` payloads so the test would
    // demonstrate a leak via Miri's leak checker if `Drop` skipped
    // them. (We can't observe the leak without Miri, but at least the
    // code path that releases them is exercised.)
    use crate::event::{EventFields, EventValue};
    use smallvec::smallvec;
    for i in 0..6u64 {
        let payload: Arc<str> = Arc::from(format!("wedged-{}", i));
        let ev = EventInstance {
            type_id: EventTypeId(1),
            start_time: i,
            end_time: i + 1,
            thread_id: 1,
            fields: smallvec![EventValue::String(payload)] as EventFields,
        };
        ring.push(ev).expect("not full");
    }

    // Simulate a wedged consumer by directly latching `consumer_busy`.
    // No live consumer will touch the slots; the Drop path must still
    // bound its wait and then drain best-effort.
    assert!(
        ring.consumer_busy
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok(),
        "gate was unexpectedly already held"
    );

    // Drop the last Arc<SpscEventRing> reference and measure how long
    // `Drop` takes. With the 200ms timeout it must complete in well
    // under 2 seconds.
    let start = Instant::now();
    drop(ring);
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(2),
        "Drop blocked for {:?} on a wedged consumer (expected < 2s)",
        elapsed
    );
    // And it must have waited at least roughly the configured budget —
    // i.e. we didn't accidentally short-circuit the wait.
    assert!(
        elapsed >= Duration::from_millis(150),
        "Drop returned in {:?}; expected to wait close to the 200ms budget",
        elapsed
    );
}

#[test]
fn drop_with_active_consumer_drains_all_events() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::thread;

    // Active-consumer happy path: a consumer thread continually
    // drain_into's the ring while the producer pushes a known number
    // of events. When the producer drops its `Arc`, the consumer
    // should already have everything; the Drop path must not panic
    // and must release any remaining slots cleanly.
    const N_EVENTS: u64 = 64;
    let ring = Arc::new(SpscEventRing::new(16));
    let stop = Arc::new(AtomicBool::new(false));

    let drained = Arc::new(parking_lot::Mutex::new(Vec::<EventInstance>::new()));

    let consumer_ring = Arc::clone(&ring);
    let consumer_stop = Arc::clone(&stop);
    let consumer_out = Arc::clone(&drained);
    let consumer = thread::spawn(move || {
        let mut local: Vec<EventInstance> = Vec::new();
        // Spin-drain until told to stop AND the ring is empty.
        loop {
            consumer_ring.drain_into(&mut local);
            if consumer_stop.load(Ordering::Acquire) && consumer_ring.is_empty() {
                break;
            }
            thread::yield_now();
        }
        // One final drain in case the producer pushed after our last
        // pass but before setting the stop flag.
        consumer_ring.drain_into(&mut local);
        consumer_out.lock().extend(local);
    });

    // Producer side: push N events with `Arc<str>` payloads so we can
    // verify payload contents (and exercise the Arc release path).
    use crate::event::{EventFields, EventValue};
    use smallvec::smallvec;
    for i in 0..N_EVENTS {
        // Tight retry loop: drop-newest semantics would lose events if
        // we push faster than the consumer drains. We instead retry
        // until the consumer makes room.
        let mut next = EventInstance {
            type_id: EventTypeId(1),
            start_time: i,
            end_time: i + 1,
            thread_id: 1,
            fields: smallvec![EventValue::String(Arc::from(format!("ev-{}", i)))]
                as EventFields,
        };
        loop {
            match ring.push(next) {
                Ok(()) => break,
                Err(returned) => {
                    next = returned;
                    thread::yield_now();
                }
            }
        }
    }

    // Signal stop and drop our producer-side Arc. The consumer holds
    // the other Arc, so this Drop path runs on the consumer thread
    // when it exits — which means we want the consumer to observe
    // everything first.
    stop.store(true, Ordering::Release);
    // Drop the producer-side Arc explicitly so any post-stop pushes
    // are impossible (there are none here, but be explicit).
    drop(ring);

    consumer.join().expect("consumer panicked");

    let drained = drained.lock();
    assert_eq!(
        drained.len(),
        N_EVENTS as usize,
        "expected exactly {} events drained, got {}",
        N_EVENTS,
        drained.len()
    );
    // Verify payload integrity — every event's string field must match
    // its start_time tag (rules out double-drop / use-after-free of
    // the Arc<str>).
    for ev in drained.iter() {
        let i = ev.start_time;
        match &ev.fields[0] {
            EventValue::String(s) => {
                assert_eq!(
                    &**s,
                    format!("ev-{}", i),
                    "payload corrupted for event {}",
                    i
                );
            }
            other => panic!("unexpected field variant: {:?}", other),
        }
    }
}

// -----------------------------------------------------------------------
// Registry-leak fix (HIGH, 2026-06-17): retired-shard reclamation tests.
// -----------------------------------------------------------------------

#[test]
fn drain_all_reclaims_retired_empty_shards() {
    // A retired + empty shard must be removed from the registry by
    // `drain_all`, bounding the otherwise unbounded `rings` growth.
    let registry = ThreadRingRegistry::new(8);

    // Register two shards directly (each Arc stands in for a producer
    // thread's clone). We don't spawn real threads here so the test is
    // deterministic — instead we manually mark one shard retired, which is
    // exactly what `RegisteredRingGuard::drop` does on real thread exit.
    let live = Arc::new(SpscEventRing::new(8));
    let dead = Arc::new(SpscEventRing::new(8));
    registry.rings.write().push(Arc::clone(&live));
    registry.rings.write().push(Arc::clone(&dead));
    assert_eq!(registry.registered_thread_count(), 2);

    // The dead thread emitted one event, then exited.
    dead.push(make_event(EventTypeId(1), 1, 2)).unwrap();
    dead.mark_retired();

    // First drain takes the dead shard's event. The shard is now retired
    // AND empty, so this same `drain_all` reclaims it.
    let drained = registry.drain_all();
    assert_eq!(
        drained.len(),
        1,
        "the dead shard's event must still be drained"
    );
    assert_eq!(
        registry.registered_thread_count(),
        1,
        "retired + empty shard must be reclaimed, leaving only the live one"
    );

    // The live shard is untouched and still usable.
    live.push(make_event(EventTypeId(2), 3, 4)).unwrap();
    let drained2 = registry.drain_all();
    assert_eq!(drained2.len(), 1);
    assert_eq!(registry.registered_thread_count(), 1);
}

#[test]
fn drain_all_keeps_retired_shard_with_unread_events() {
    // A retired shard that still holds unread events (e.g. a drainer lost
    // the consumer CAS) must NOT be reclaimed — dropping it would lose
    // those events. We simulate "unread" by latching `consumer_busy` so
    // `drain_into` is a no-op for the dead shard during this pass.
    use std::sync::atomic::Ordering;

    let registry = ThreadRingRegistry::new(8);
    let dead = Arc::new(SpscEventRing::new(8));
    registry.rings.write().push(Arc::clone(&dead));

    dead.push(make_event(EventTypeId(1), 10, 11)).unwrap();
    dead.mark_retired();
    // Wedge the consumer gate so this drain cannot take the event.
    assert!(dead
        .consumer_busy
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_ok());

    let drained = registry.drain_all();
    assert_eq!(
        drained.len(),
        0,
        "gate is held, so nothing drains this pass"
    );
    assert_eq!(
        registry.registered_thread_count(),
        1,
        "retired but non-empty shard must be retained — its event is unread"
    );

    // Release the gate; the next drain must recover the event and only then
    // reclaim the now-empty retired shard.
    dead.consumer_busy.store(false, Ordering::Release);
    let drained2 = registry.drain_all();
    assert_eq!(
        drained2.len(),
        1,
        "event must survive and drain on the next pass"
    );
    assert_eq!(
        registry.registered_thread_count(),
        0,
        "shard reclaimed only after its last event was drained"
    );
}

#[test]
fn registered_ring_guard_marks_ring_retired_on_thread_exit() {
    // The thread-local Drop guard must flag the ring retired when the
    // producer thread exits. Spawn a thread that registers + emits, then
    // joins; afterwards the shard it created must report `is_retired()`.
    use std::thread;

    let registry = Arc::new(ThreadRingRegistry::new(8));
    let reg = Arc::clone(&registry);
    let shard = thread::spawn(move || {
        let ring = reg.register_current_thread();
        ring.push(make_event(EventTypeId(1), 1, 2)).unwrap();
        // Return the registry's view of this thread's shard.
        ring
    })
    .join()
    .expect("worker panicked");

    // The worker thread has fully exited, so its `RegisteredRingGuard`
    // TLS destructor ran and marked the shard retired.
    assert!(
        shard.is_retired(),
        "ring must be retired after its producer thread exits"
    );
    // It still holds the unread event, so it would NOT yet be reclaimed.
    assert!(!shard.is_empty());

    // Draining recovers the event and reclaims the now retired+empty shard.
    let drained = registry.drain_all();
    assert_eq!(drained.len(), 1);
    assert_eq!(registry.registered_thread_count(), 0);
}

#[test]
fn drop_with_wedged_consumer_does_not_free_slot_backing() {
    // UAF fix (MED, 2026-06-20): on the wedged-consumer path, the slot
    // array storage must NOT be freed — a parked consumer will dereference
    // its slot when it resumes, after `Drop` returns. We can't directly
    // observe the leak/UAF without Miri/ASAN, but we can model the parked
    // consumer's post-drop dereference: capture a raw pointer to a slot
    // while the gate is latched, drop the ring, then read through that
    // pointer. Under the old code (which freed the box) this is a
    // use-after-free; under the fix the storage is leaked and stays mapped,
    // so the read observes valid memory.
    use crate::event::{EventFields, EventValue};
    use std::sync::atomic::Ordering;
    use std::sync::Arc;

    let ring = Arc::new(SpscEventRing::with_shutdown_timeout(
        8,
        Duration::from_millis(50),
    ));
    for i in 0..4u64 {
        let ev = EventInstance {
            type_id: EventTypeId(1),
            start_time: i,
            end_time: i + 1,
            thread_id: 1,
            fields: smallvec![EventValue::String(Arc::from(format!("uaf-{}", i)))]
                as EventFields,
        };
        ring.push(ev).expect("not full");
    }

    // Latch the gate to simulate a wedged consumer, and capture a raw
    // pointer to the slot at the current `tail` (the consumer's "active"
    // slot — the one Drop must skip *and* must not free out from under us).
    assert!(ring
        .consumer_busy
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_ok());
    let active_tail = ring.tail.load(Ordering::Acquire);
    let active_idx = active_tail & ring.mask;
    let slot_ptr: *const UnsafeCell<MaybeUninit<EventInstance>> =
        &ring.slots[active_idx] as *const _;

    // Drop the last Arc → `SpscEventRing::drop` runs the wedged path.
    drop(ring);

    // Model the parked consumer resuming and dereferencing its slot. With
    // the fix the backing is leaked (still mapped), so this is a valid read
    // of the producer-initialised `EventInstance`. We read it without moving
    // it out (so we don't double-drop the skipped slot's payload).
    // SAFETY: this test deliberately models the consumer-resume half of
    // the wedged-drop contract. The fixed drop path leaks the slot backing
    // when the consumer gate is held, and `active_tail < head` proves this
    // producer-published slot is initialized and still mapped.
    let ev_ref: &EventInstance = unsafe { (*(*slot_ptr).get()).assume_init_ref() };
    assert_eq!(ev_ref.start_time, active_tail as u64);
    match &ev_ref.fields[0] {
        EventValue::String(s) => {
            assert_eq!(&**s, format!("uaf-{}", active_tail));
        }
        other => panic!("unexpected field variant: {:?}", other),
    }
}

#[test]
fn drop_happy_path_releases_slots_without_consumer() {
    // No consumer ever touches the ring. Drop must traverse [tail, head)
    // and release every Arc<str> payload — a leak would show up under
    // Miri or a leak checker; here we just verify the Drop path runs
    // to completion without panicking and within a tight time budget.
    use crate::event::{EventFields, EventValue};
    use smallvec::smallvec;

    let ring = SpscEventRing::with_shutdown_timeout(8, Duration::from_millis(50));
    for i in 0..6u64 {
        let ev = EventInstance {
            type_id: EventTypeId(1),
            start_time: i,
            end_time: i + 1,
            thread_id: 1,
            fields: smallvec![EventValue::String(Arc::from(format!("hp-{}", i)))]
                as EventFields,
        };
        ring.push(ev).expect("not full");
    }
    let start = Instant::now();
    drop(ring);
    // No wedged consumer → Drop should return essentially immediately
    // (no waiting at all). Generous bound of 100ms accommodates noisy
    // CI environments.
    assert!(
        start.elapsed() < Duration::from_millis(100),
        "happy-path Drop took {:?}; expected immediate",
        start.elapsed()
    );
}
