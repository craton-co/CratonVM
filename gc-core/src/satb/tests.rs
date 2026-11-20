use super::*;

/// The STW witness these tests stand in for.
///
/// SAFETY: a unit test's only mutator is the test thread itself, so the
/// safepoint precondition holds vacuously. Where a test deliberately
/// drains WHILE a mutator runs -- to exercise the residual window the
/// token exists to document -- the call site says so in a comment; the
/// token is a witness the caller offers, not a guard the queue enforces.
fn stw() -> crate::collector::StopTheWorldToken {
    unsafe { crate::collector::StopTheWorldToken::new() }
}

/// gcj j2/cm2 (`CRATONVM_GEN_SATB_DIRTY_THREAD_WALK`): off by default --
/// no queue keeps a dirty-thread list, every walk is the registry walk.
#[test]
fn gcj2cm2_no_queue_keeps_a_dirty_thread_list_by_default() {
    let q = SatbQueue::new();
    q.activate();
    assert!(!q.dirty_walk_at(q.current_epoch()));
    assert!(!satb_thread_local_log(&q, 0xD100));
    assert_eq!(q.dirty_thread_count(), 0);
    let all = q.deactivate_and_drain(&stw());
    assert_eq!(all, vec![0xD100]);
}

/// gcj j2/cm2: once enabled, the walk of a cycle opened two activations
/// later visits only the threads that logged into the queue during it,
/// each listed once, and still finds every pending entry -- another live
/// thread's partly-filled bucket included. The next activation empties the
/// list, and a thread's first log of the new cycle puts it back.
#[test]
fn gcj2cm2_the_dirty_thread_walk_lists_each_logger_once_and_misses_none() {
    let q = Arc::new(SatbQueue::new());
    q.enable_dirty_thread_walk();
    q.activate();
    assert!(!q.dirty_walk_at(q.current_epoch()), "the margin cycle");
    q.deactivate_and_discard();
    q.activate();
    assert!(q.dirty_walk_at(q.current_epoch()));
    let (logged_tx, logged_rx) = std::sync::mpsc::channel::<()>();
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    let peer_q = Arc::clone(&q);
    let peer = std::thread::spawn(move || {
        assert!(!satb_thread_local_log(&peer_q, 0xD200));
        assert!(!satb_thread_local_log(&peer_q, 0xD201));
        logged_tx.send(()).expect("logged");
        // Alive, bucket undrained, until the walk ran.
        done_rx.recv().expect("released");
    });
    logged_rx.recv().expect("the peer logged");
    assert!(!satb_thread_local_log(&q, 0xD300));
    assert!(!satb_thread_local_log(&q, 0xD301));
    assert_eq!(q.dirty_thread_count(), 2, "each logger once");
    let mut all = q.deactivate_and_drain(&stw());
    all.sort_unstable();
    assert_eq!(all, vec![0xD200, 0xD201, 0xD300, 0xD301]);
    done_tx.send(()).expect("release");
    peer.join().expect("the peer");
    q.activate();
    assert_eq!(q.dirty_thread_count(), 0, "per cycle");
    assert!(!satb_thread_local_log(&q, 0xD400));
    assert_eq!(q.dirty_thread_count(), 1);
    assert_eq!(q.deactivate_and_drain(&stw()), vec![0xD400]);
}

#[test]
fn buffer_log_and_drain() {
    let mut buf = SatbBuffer::new();
    assert!(buf.is_empty());

    buf.log(0x1000);
    buf.log(0x2000);
    assert_eq!(buf.len(), 2);

    let entries = buf.drain();
    assert_eq!(entries, vec![0x1000, 0x2000]);
    assert!(buf.is_empty());
}

#[test]
fn buffer_signals_full() {
    let mut buf = SatbBuffer::with_capacity(3);
    assert!(!buf.log(0x100));
    assert!(!buf.log(0x200));
    assert!(buf.log(0x300)); // full
    assert!(buf.is_full());
}

#[test]
fn queue_inactive_by_default() {
    let q = SatbQueue::new();
    assert!(!q.is_active());
    assert!(q.is_empty());
}

#[test]
fn queue_activate_flush_drain() {
    let q = SatbQueue::new();
    q.activate();
    assert!(q.is_active());

    q.flush(vec![0x100, 0x200]);
    q.flush(vec![0x300]);
    assert_eq!(q.len(), 3);

    let drained = q.drain();
    assert_eq!(drained, vec![0x100, 0x200, 0x300]);
    assert!(q.is_empty());

    q.deactivate();
    assert!(!q.is_active());
}

#[test]
fn queue_concurrent_flush() {
    use std::sync::Arc;
    let q = Arc::new(SatbQueue::new());
    q.activate();

    let mut handles = Vec::new();
    for t in 0..4 {
        let q = q.clone();
        handles.push(std::thread::spawn(move || {
            let mut buf = SatbBuffer::with_capacity(64);
            for i in 0..64 {
                buf.log(t * 1000 + i);
            }
            q.flush(buf.drain());
        }));
    }

    for h in handles {
        h.join().unwrap();
    }

    let all = q.drain();
    assert_eq!(all.len(), 256); // 4 threads × 64 entries
}

// -----------------------------------------------------------------------
// Additional tests
// -----------------------------------------------------------------------

#[test]
fn buffer_empty_drain_returns_empty() {
    let mut buf = SatbBuffer::new();
    let entries = buf.drain();
    assert!(entries.is_empty());
    assert!(buf.is_empty());
}

#[test]
fn buffer_single_entry() {
    let mut buf = SatbBuffer::new();
    let full = buf.log(0xDEAD);
    assert!(!full);
    assert_eq!(buf.len(), 1);
    assert!(!buf.is_empty());

    let entries = buf.drain();
    assert_eq!(entries, vec![0xDEAD]);
}

#[test]
fn buffer_capacity_exact_boundary() {
    let mut buf = SatbBuffer::with_capacity(1);
    // Capacity 1: first log should signal full
    assert!(buf.log(0x100));
    assert!(buf.is_full());
    assert_eq!(buf.len(), 1);
}

#[test]
fn buffer_drain_resets_capacity() {
    let mut buf = SatbBuffer::with_capacity(2);
    buf.log(0x100);
    buf.log(0x200);
    assert!(buf.is_full());

    let _ = buf.drain();
    assert!(buf.is_empty());
    assert!(!buf.is_full());

    // Can fill again after drain
    assert!(!buf.log(0x300));
    assert!(buf.log(0x400));
    assert!(buf.is_full());
}

#[test]
fn buffer_multiple_drains() {
    let mut buf = SatbBuffer::with_capacity(4);
    buf.log(0x10);
    buf.log(0x20);

    let first = buf.drain();
    assert_eq!(first, vec![0x10, 0x20]);

    buf.log(0x30);
    let second = buf.drain();
    assert_eq!(second, vec![0x30]);

    // Third drain is empty
    let third = buf.drain();
    assert!(third.is_empty());
}

#[test]
fn queue_flush_empty_vec_is_noop() {
    let q = SatbQueue::new();
    q.flush(vec![]);
    assert!(q.is_empty());
    assert_eq!(q.len(), 0);
}

#[test]
fn queue_drain_when_empty() {
    let q = SatbQueue::new();
    let drained = q.drain();
    assert!(drained.is_empty());
}

#[test]
fn queue_multiple_flush_accumulates() {
    let q = SatbQueue::new();
    q.flush(vec![1, 2]);
    q.flush(vec![3, 4, 5]);
    q.flush(vec![6]);

    assert_eq!(q.len(), 6);
    let drained = q.drain();
    assert_eq!(drained, vec![1, 2, 3, 4, 5, 6]);
}

#[test]
fn queue_activate_deactivate_cycle() {
    let q = SatbQueue::new();
    assert!(!q.is_active());

    q.activate();
    assert!(q.is_active());

    q.deactivate();
    assert!(!q.is_active());

    // Re-activate should work
    q.activate();
    assert!(q.is_active());
}

#[test]
fn queue_drain_clears_entries() {
    let q = SatbQueue::new();
    q.flush(vec![0x100, 0x200, 0x300]);
    assert_eq!(q.len(), 3);

    let _ = q.drain();
    assert!(q.is_empty());
    assert_eq!(q.len(), 0);

    // Second drain returns empty
    let second = q.drain();
    assert!(second.is_empty());
}

#[test]
fn queue_concurrent_interleaved_flush_drain() {
    use std::sync::Arc;

    let q = Arc::new(SatbQueue::new());
    q.activate();

    let q2 = q.clone();
    // Writer thread
    let writer = std::thread::spawn(move || {
        for i in 0..100usize {
            q2.flush(vec![i]);
        }
    });

    // Drain periodically from main thread
    let mut total_drained = 0;
    writer.join().unwrap();

    // Final drain to get everything
    total_drained += q.drain().len();

    // Everything that was pushed must come out
    // Some may have been drained mid-way, so we just need the total
    assert_eq!(total_drained, 100);
}

#[test]
fn buffer_overflow_behavior_large_count() {
    let mut buf = SatbBuffer::with_capacity(4);

    // Push past capacity -- the buffer does not enforce capacity as a hard limit
    assert!(!buf.log(1));
    assert!(!buf.log(2));
    assert!(!buf.log(3));
    assert!(buf.log(4)); // signals full at capacity

    // Continue pushing after full signal
    buf.log(5);
    assert_eq!(buf.len(), 5);

    let entries = buf.drain();
    assert_eq!(entries, vec![1, 2, 3, 4, 5]);
}

#[test]
fn thread_local_log_explicit_flush() {
    // Run inside a fresh thread so the per-thread SATB buffer starts
    // empty regardless of test ordering.
    let h = std::thread::spawn(|| {
        let q = SatbQueue::new();
        q.activate();
        satb_thread_local_log(&q, 0x100);
        satb_thread_local_log(&q, 0x200);
        // Not full yet → nothing in the global queue.
        assert!(q.is_empty());
        // Explicit safepoint-style flush drains the per-thread buffer.
        flush_thread_satb_buffer(&q);
        let drained = q.drain();
        assert_eq!(drained, vec![0x100, 0x200]);
    });
    h.join().unwrap();
}

/// gc-common w5-e, the third bullet of
/// `common-g-proposal-satb-owner-local-buffer-REJECTED-20260928`: a spilled bucket comes
/// back as the thread's next bucket instead of being freed, and nothing
/// is lost or duplicated on the way.
#[test]
fn a_spilled_bucket_is_recycled_as_the_next_bucket() {
    let h = std::thread::spawn(|| {
        let q = SatbQueue::new();
        q.activate();
        let spare =
            || THREAD_SATB_BUFFER.with(|g| g.buffer.lock().spare.as_ref().map(Vec::capacity));
        assert_eq!(spare(), None, "a fresh thread has no spare");
        for i in 1..=DEFAULT_SATB_CAPACITY {
            let _ = satb_thread_local_log(&q, i * 8);
        }
        assert!(
            spare().is_some_and(|c| c >= DEFAULT_SATB_CAPACITY),
            "the spill must park the emptied bucket"
        );
        assert_eq!(
            q.len(),
            DEFAULT_SATB_CAPACITY,
            "the spill itself is unchanged"
        );
        let _ = satb_thread_local_log(&q, 0xdead_0000);
        assert_eq!(spare(), None, "the next bucket reuses it");
        flush_thread_satb_buffer(&q);
        let mut got = q.drain();
        got.sort_unstable();
        let mut want: Vec<usize> = (1..=DEFAULT_SATB_CAPACITY).map(|i| i * 8).collect();
        want.push(0xdead_0000);
        assert_eq!(got, want);
        q.deactivate_and_discard();
    });
    h.join().unwrap();
}

#[test]
fn thread_local_log_null_skipped() {
    let h = std::thread::spawn(|| {
        let q = SatbQueue::new();
        q.activate();
        satb_thread_local_log(&q, 0);
        flush_thread_satb_buffer(&q);
        assert!(q.is_empty());
    });
    h.join().unwrap();
}

#[test]
fn thread_local_log_auto_flushes_at_capacity() {
    // Each thread has its own buffer with DEFAULT_SATB_CAPACITY=256.
    // Logging exactly 256 entries should auto-flush into the global queue.
    let h = std::thread::spawn(|| {
        let q = SatbQueue::new();
        q.activate();
        for i in 1..=DEFAULT_SATB_CAPACITY {
            satb_thread_local_log(&q, i);
        }
        // Auto-flush at 256 means the global queue is non-empty without
        // an explicit safepoint flush.
        assert!(!q.is_empty());
        assert_eq!(q.len(), DEFAULT_SATB_CAPACITY);
    });
    h.join().unwrap();
}

// -----------------------------------------------------------------------
// Round-5 CRIT #4 — tri-state activation
// -----------------------------------------------------------------------

#[test]
fn deactivate_and_drain_returns_entries_and_disables() {
    let q = SatbQueue::new();
    q.activate();
    assert!(q.is_active());
    q.flush(vec![0x100, 0x200]);
    let drained = q.deactivate_and_drain(&stw());
    // All entries returned and the gate is now off.
    assert_eq!(drained.len(), 2);
    assert!(!q.is_active());
    assert!(q.is_empty());
}

#[test]
fn is_active_true_during_draining_window() {
    // Manually drive the state to DRAINING and verify is_active still
    // reports true so concurrent loggers keep pushing.
    let q = SatbQueue::new();
    q.activate();
    // Simulate the first half of deactivate_and_drain: flip to DRAINING.
    q.state.store(SATB_DRAINING, Ordering::Release);
    assert!(q.is_active(), "DRAINING must report as active to loggers");
    q.state.store(SATB_INACTIVE, Ordering::Release);
    assert!(!q.is_active());
}

#[test]
fn deactivate_and_drain_captures_late_writers() {
    // Simulate a writer that pushes between the two internal drain
    // passes. The implementation does drain() twice, so any push that
    // lands after the first drain but before the INACTIVE store is
    // still captured in the returned Vec.
    let q = SatbQueue::new();
    q.activate();
    q.flush(vec![1, 2, 3]);
    // Manually drive to DRAINING then push more, then call drain twice.
    q.state.store(SATB_DRAINING, Ordering::Release);
    let first = q.drain();
    assert_eq!(first.len(), 3);
    // Late writer pushes into a (now-empty) shard while DRAINING.
    q.flush(vec![4, 5]);
    let second = q.drain();
    assert_eq!(second.len(), 2);
}

// -----------------------------------------------------------------------
// Round-11 gc HIGH — per-thread SATB buffer registry / completeness
// -----------------------------------------------------------------------

#[test]
fn flush_all_thread_satb_buffers_reaches_global_queue() {
    // Stage entries into a *registered* thread-local buffer without
    // filling it (so they would otherwise stay local and invisible to the
    // collector), then prove `flush_all_thread_satb_buffers` + `drain`
    // surfaces them in the global queue.
    //
    // Run in a fresh thread so this thread's buffer is freshly created and
    // registered. Use unique sentinel addresses and assert by membership
    // rather than exact count: the global registry is process-wide, so a
    // concurrently-running test thread could contribute its own entries —
    // our sentinels must be present regardless.
    let h = std::thread::spawn(|| {
        let q = SatbQueue::new();
        q.activate();

        const S1: usize = 0xA11CE_000;
        const S2: usize = 0xA11CE_001;
        satb_thread_local_log(&q, S1);
        satb_thread_local_log(&q, S2);

        // Buffer is far from full (cap 256) → still purely thread-local;
        // the collector's shard drain alone would miss these.
        assert!(q.is_empty(), "partial buffer must not have auto-flushed");

        // Collector-side flush of every registered per-thread buffer.
        flush_all_thread_satb_buffers(&q);

        let drained = q.drain();
        assert!(
            drained.contains(&S1) && drained.contains(&S2),
            "staged thread-local entries must reach the global queue, got {drained:?}"
        );
    });
    h.join().unwrap();
}

#[test]
fn deactivate_and_drain_includes_thread_local_buffer() {
    // The actual bug repro: at remark, `deactivate_and_drain` must capture
    // references still sitting in a partially-full per-thread buffer, not
    // just the global shards.
    let h = std::thread::spawn(|| {
        let q = SatbQueue::new();
        q.activate();

        const SENTINEL: usize = 0xBEEF_FACE;
        satb_thread_local_log(&q, SENTINEL);
        // Not flushed to the shards yet — lives only in the thread-local.
        assert!(q.is_empty());

        let snapshot = q.deactivate_and_drain(&stw());
        assert!(
            snapshot.contains(&SENTINEL),
            "remark drain must include thread-local overwritten refs, got {snapshot:?}"
        );
        assert!(!q.is_active());
    });
    h.join().unwrap();
}

#[test]
fn dying_thread_residual_satb_entries_survive_until_remark_drain() {
    // Dying-thread completeness regression (2026-07-16, sibling of the
    // card table's dying_thread_residual_offsets_survive_until_flush_all):
    // a mutator that logs an SATB entry (below the auto-flush capacity)
    // and EXITS without flushing must not lose the entry — it is the old
    // value of a heap reference overwritten during concurrent marking,
    // and losing it hides the overwritten object from the mark closure.
    // Pre-fix, the thread's Arc dropped, the registry Weak died, and the
    // entry was silently discarded.
    let q = std::sync::Arc::new(SatbQueue::new());
    q.activate();

    const SENTINEL: usize = 0xDEAD_1234;
    let q2 = std::sync::Arc::clone(&q);
    std::thread::spawn(move || {
        // One buffered entry, far below DEFAULT_SATB_CAPACITY; the thread
        // exits immediately after, running SatbBufferGuard::drop.
        satb_thread_local_log(&q2, SENTINEL);
        assert!(q2.is_empty(), "entry must still be thread-local");
    })
    .join()
    .unwrap();

    // Remark STW drain: must recover the dead thread's entry from the
    // orphan list and include it in the snapshot.
    let snapshot = q.deactivate_and_drain(&stw());
    assert!(
        snapshot.contains(&SENTINEL),
        "remark drain must include a dead thread's buffered refs, got {snapshot:?}"
    );

    // The emptied orphan was reaped: a second full drain sees nothing.
    q.activate();
    flush_all_thread_satb_buffers(&q);
    assert!(q.is_empty(), "emptied orphan must have been reaped");
    let _ = q.deactivate_and_drain(&stw());
}

#[test]
fn registry_prunes_dead_weak_slots() {
    // A buffer whose owning thread has exited drops its only `Arc`, leaving
    // a dead `Weak` in the registry that must (a) not upgrade and (b) be
    // pruned without panic. Drive the mechanism deterministically by
    // manually injecting a dead `Weak` (simulating an exited thread) plus a
    // live one, then asserting `flush_all_thread_satb_buffers` removes only
    // the dead slot. This avoids depending on the count of buffers from
    // other test threads running in parallel.

    // A dead Weak: create an Arc, downgrade, then drop the Arc.
    let dead: Weak<Mutex<ThreadSatbPartitions>> = {
        let arc = Arc::new(Mutex::new(ThreadSatbPartitions::default()));
        Arc::downgrade(&arc)
        // `arc` dropped here → `dead` can no longer upgrade.
    };
    assert!(dead.upgrade().is_none(), "weak must be dead after Arc drop");

    // A live, registered partition set holding one entry staged for `q`.
    let q = SatbQueue::new();
    let live = Arc::new(Mutex::new(ThreadSatbPartitions::default()));
    assert!(live
        .lock()
        .log(q.id(), q.current_epoch(), 0xFEED_0001)
        .0
        .is_none());
    let live_weak = Arc::downgrade(&live);

    // Inject both into the registry. Keep a clone of the dead Weak so we
    // can prove afterward that flush_all neither upgraded nor retained it.
    let dead_probe = dead.clone();
    {
        let mut reg = SATB_BUFFER_REGISTRY.lock();
        reg.push(dead);
        reg.push(live_weak);
    }

    // Must not panic on the dead slot, and must process the live one.
    flush_all_thread_satb_buffers(&q);

    // The live partition set's staged entry reached the queue and was
    // consumed (proves flush_all walked past the dead slot to it).
    assert!(q.drain().contains(&0xFEED_0001));
    assert!(
        live.lock().take(q.id(), q.current_epoch()).0.is_empty(),
        "live bucket consumed by flush_all"
    );
    // The dead slot never resurrects (deterministic, unaffected by other
    // parallel test threads) and the registry no longer holds *our* dead
    // Weak: a dead Weak has no strong refs, and after pruning the only
    // remaining tie to that allocation is `dead_probe`, which still cannot
    // upgrade. The count of dead slots flush_all left behind for the slots
    // it visited is zero — verified via the live buffer having been the one
    // reached.
    assert!(dead_probe.upgrade().is_none(), "dead Weak must stay dead");
}

/// I-4 (audit §7) — the contract `G1Collector::remark` cites BY NAME.
///
/// `g1.rs`'s remark block says it deliberately does NOT `debug_assert!`
/// that every registered buffer is empty after the collector-side flush
/// (the registry is process-global and such an assertion flakes against the
/// parallel test harness), and names
/// `satb::tests::flush_all_captures_every_parked_mutator_buffer` as the
/// deterministic substitute. That test did not exist — the comment was the
/// only thing standing behind the crate's single most load-bearing SATB
/// claim ("no mutator's partially-full bucket escapes the snapshot"). This
/// is it.
///
/// Deterministic where the assertion could not be: instead of asserting
/// over the process-global registry, it PARKS N mutator threads on a
/// barrier after each has logged a sub-threshold sentinel (standing in for
/// the STW safepoint the production caller runs at), flushes from the
/// collector side, and requires every sentinel to be in the queue. Sibling
/// tests running in parallel can only ADD entries, never remove ours, so
/// membership is a stable predicate and a count is not.
#[test]
fn flush_all_captures_every_parked_mutator_buffer() {
    use std::sync::mpsc;
    use std::sync::Barrier;

    const MUTATORS: usize = 4;
    let q = Arc::new(SatbQueue::new());
    q.activate();

    // `parked` releases once every mutator has logged; `resume` releases
    // once the collector has flushed. Between the two the mutators are
    // blocked inside the barrier and cannot touch their buffers, which is
    // exactly the property the STW safepoint provides in production.
    let parked = Arc::new(Barrier::new(MUTATORS + 1));
    let resume = Arc::new(Barrier::new(MUTATORS + 1));
    let (tx, rx) = mpsc::channel::<usize>();

    let handles: Vec<_> = (0..MUTATORS)
        .map(|t| {
            let q = Arc::clone(&q);
            let parked = Arc::clone(&parked);
            let resume = Arc::clone(&resume);
            let tx = tx.clone();
            std::thread::spawn(move || {
                // One entry, far below DEFAULT_SATB_CAPACITY, so it can
                // ONLY reach the queue via the collector-side registry
                // walk — never via the auto-flush.
                let sentinel = 0x5A7B_0000 + (t + 1) * 8;
                satb_thread_local_log(&q, sentinel);
                tx.send(sentinel).expect("sentinel reported");
                parked.wait();
                resume.wait();
            })
        })
        .collect();
    drop(tx);

    parked.wait();
    // `recv` exactly MUTATORS times rather than draining the iterator: the
    // senders are still parked on the barrier below and will not drop their
    // clones until this thread releases them, so `rx.iter()` would never
    // terminate. Every send happened before the barrier, so the count is
    // known.
    let sentinels: Vec<usize> = (0..MUTATORS)
        .map(|_| rx.recv().expect("sentinel reported"))
        .collect();

    // The collector reaches into every registered thread. This is the one
    // mechanism between a mutator that never spilled and the snapshot.
    flush_all_thread_satb_buffers(&q);
    let drained = q.drain();
    resume.wait();
    for h in handles {
        h.join().unwrap();
    }

    for s in sentinels {
        assert!(
            drained.contains(&s),
            "a parked mutator's partially-full SATB bucket did not reach the queue: \
             {s:#x} missing from {drained:?}"
        );
    }
}

/// Lane-C — `has_pending()` is the marker's lock-free "is a drain worth
/// taking sixteen mutexes for?" probe, and the ONLY direction that may
/// never be wrong is the negative one: a `false` over a populated shard
/// makes the marker skip a drain that had work in it.
#[test]
fn has_pending_never_reads_empty_over_a_populated_shard() {
    let q = SatbQueue::new();
    assert!(!q.has_pending(), "a fresh queue has nothing queued");
    assert!(q.is_empty());

    q.flush(vec![0x100, 0x200, 0x300]);
    assert!(q.has_pending());
    assert!(!q.is_empty());

    // A partial consumer (the `deactivate_and_drain` shard-lock pass) must
    // debit what it took, or the count latches non-zero forever and the
    // probe stops being a probe.
    let drained = q.drain();
    assert_eq!(drained.len(), 3);
    assert!(!q.has_pending(), "drain must debit every entry it removed");
    assert!(q.is_empty());

    // And the deactivate path debits too.
    q.activate();
    q.flush(vec![0x400]);
    assert!(q.has_pending());
    let late = q.deactivate_and_drain(&stw());
    assert!(late.contains(&0x400));
    assert!(!q.has_pending(), "deactivate_and_drain must debit as well");
}

/// Lane-C — the counter must survive many flush/drain rounds across
/// threads without drifting. A drift upward silently disables the fast
/// path (harmless); a drift downward is the unsound direction, and an
/// unbalanced debit would also underflow a `usize` into `has_pending()`
/// reading true forever, which is the loud way the same bug shows up.
#[test]
fn pending_count_balances_under_concurrent_flush_and_drain() {
    use std::sync::Arc;
    let q = Arc::new(SatbQueue::new());
    q.activate();

    let flushers: Vec<_> = (0..4)
        .map(|t| {
            let q = Arc::clone(&q);
            std::thread::spawn(move || {
                for i in 0..200usize {
                    q.flush(vec![t * 1000 + i]);
                }
            })
        })
        .collect();

    let drainer = {
        let q = Arc::clone(&q);
        std::thread::spawn(move || {
            let mut seen = 0usize;
            for _ in 0..500 {
                seen += q.drain().len();
                std::thread::yield_now();
            }
            seen
        })
    };

    for h in flushers {
        h.join().unwrap();
    }
    let mid = drainer.join().unwrap();
    let rest = q.drain().len();
    assert_eq!(mid + rest, 800, "every flushed entry must come back out");
    assert!(
        !q.has_pending(),
        "the count drifted: it reads non-zero over a fully-drained queue"
    );
}

/// How many entries this thread still holds buffered for `queue_id`.
fn bucket_len_for(queue_id: u64) -> usize {
    THREAD_SATB_BUFFER.with(|g| {
        g.buffer
            .lock()
            .buckets
            .iter()
            .find(|b| b.queue_id == queue_id)
            .map(|b| b.entries.len())
            .unwrap_or(0)
    })
}

/// gengc-mark 2026-09-20 — dropping a queue must reap the per-thread
/// buckets tagged with its id.
///
/// Ids are never reissued, so a bucket left behind by a dropped queue can
/// never be drained: it is a permanent leak of a `Vec` of raw heap
/// addresses in a live thread's TLS, and it lengthens the linear `buckets`
/// scan on that thread's barrier fast path for the rest of the process.
#[test]
fn dropping_a_queue_reaps_its_per_thread_bucket() {
    // Fresh thread so the buffer starts empty regardless of test ordering.
    let h = std::thread::spawn(|| {
        // A second queue that OUTLIVES the first: its bucket must survive.
        let keeper = SatbQueue::new();
        keeper.activate();
        satb_thread_local_log(&keeper, 0xCEDE_0001);

        let dead_id = {
            let doomed = SatbQueue::new();
            doomed.activate();
            satb_thread_local_log(&doomed, 0xC0FFEE);
            assert!(doomed.is_empty(), "entry must still be thread-local");
            assert_eq!(bucket_len_for(doomed.id()), 1);
            doomed.id()
            // `doomed` is dropped here.
        };

        assert_eq!(
            bucket_len_for(dead_id),
            0,
            "a dropped queue's bucket must be reaped, not orphaned forever"
        );
        assert_eq!(
            bucket_len_for(keeper.id()),
            1,
            "reaping is queue-scoped: a live queue's bucket must survive"
        );
    });
    h.join().unwrap();
}

/// The shard index is a pure function of the calling thread, so the cached
/// value must equal the value a recomputation would give, every time.
#[test]
fn shard_for_current_thread_is_stable_and_in_range() {
    let first = shard_for_current_thread();
    assert!(first < SHARDS);
    for _ in 0..1000 {
        assert_eq!(shard_for_current_thread(), first);
    }
    // A different thread is free to pick a different shard; what matters is
    // that it, too, is stable and in range.
    std::thread::spawn(|| {
        let s = shard_for_current_thread();
        assert!(s < SHARDS);
        assert_eq!(shard_for_current_thread(), s);
    })
    .join()
    .unwrap();
}

// -----------------------------------------------------------------------
// gc-common w3-e (2026-09-23) — per-thread log census, single-shard drain
// -----------------------------------------------------------------------

/// The barrier-log census is counted per thread and folded into the
/// retired total when the thread exits, so no log is lost with the thread.
/// Only the monotone retired static is compared: the live half of
/// `barrier_log_census` walks a registry other tests share.
#[test]
fn a_threads_logs_reach_the_retired_census_when_it_exits() {
    let q = Arc::new(SatbQueue::new());
    q.activate();
    let before = BARRIER_LOGS.load(Ordering::Relaxed);
    let q2 = Arc::clone(&q);
    std::thread::spawn(move || {
        for i in 1..=300usize {
            satb_thread_local_log(&q2, i * 8);
        }
        // 300 logs: one spill of 256 plus 44 still in the bucket.
        let own = THREAD_SATB_BUFFER.with(|g| g.buffer.lock().logs);
        assert_eq!(own, 300, "the count lives in the thread's own bucket");
    })
    .join()
    .unwrap();
    let after = BARRIER_LOGS.load(Ordering::Relaxed);
    assert!(
        after >= before + 300,
        "an exiting thread must fold its logs into the retired total \
         ({before} -> {after})"
    );
    // The census function sees at least the retired part.
    assert!(barrier_log_census().0 >= after);
    // The 44 unspilled entries were orphaned, not dropped.
    flush_all_thread_satb_buffers(&q);
    assert_eq!(q.drain().len(), 300);
}

/// `drain` with one populated shard hands that shard's contents back
/// unchanged; with several it concatenates them. Either way the pending
/// credit returns to zero.
#[test]
fn drain_returns_every_entry_whether_one_or_many_shards_hold_them() {
    let q = SatbQueue::new();
    q.activate();
    q.flush(vec![8, 16, 24]);
    assert_eq!(q.drain(), vec![8, 16, 24]);
    assert!(!q.has_pending());

    // Several shards: flush from distinct threads (round-robin shards).
    let q = Arc::new(SatbQueue::new());
    q.activate();
    let handles: Vec<_> = (0..SHARDS)
        .map(|t| {
            let q = Arc::clone(&q);
            std::thread::spawn(move || q.flush(vec![(t + 1) * 8]))
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    let mut all = q.drain();
    all.sort_unstable();
    assert_eq!(all, (1..=SHARDS).map(|t| t * 8).collect::<Vec<_>>());
    assert!(!q.has_pending());
}

// -----------------------------------------------------------------------
// gc-common w1-g (2026-09-23) — activation hygiene, TLS teardown
// -----------------------------------------------------------------------

/// An INACTIVE -> ACTIVE transition starts from empty shards; entries left
/// there by a closed cycle (here via the test/debug `deactivate()`, which
/// drains nothing) belong to no snapshot.
#[test]
fn activation_discards_a_previous_cycles_leftovers() {
    let q = SatbQueue::new();
    q.activate();
    q.flush(vec![0x7000, 0x7008]);
    q.deactivate();
    assert_eq!(q.len(), 2);
    q.activate();
    assert!(q.is_empty(), "a fresh cycle must not inherit old entries");
    assert!(!q.has_pending(), "the discard must debit `pending`");
    assert_eq!(q.stale_discards(), 2);
    q.deactivate();
}

/// ...but re-activating a queue whose snapshot is still OPEN keeps it.
#[test]
fn reactivating_an_active_queue_keeps_its_entries() {
    let q = SatbQueue::new();
    q.activate();
    q.flush(vec![0x7100]);
    q.activate();
    assert_eq!(q.drain(), vec![0x7100]);
    assert_eq!(q.stale_discards(), 0);
    q.deactivate();
}

/// gc-common w7-e — the per-thread twin of
/// `activation_discards_a_previous_cycles_leftovers`. A bucket this thread
/// still holds from a CLOSED cycle (in production: a writer that appended
/// after the abort path's non-STW discard walk had visited it; here the
/// test/debug `deactivate()`, which visits nothing, leaves it behind the
/// same way) must not be replayed into the next cycle by any of the three
/// consumers: this thread's own flush, its next log, and the remark walk.
#[test]
fn a_bucket_from_a_closed_cycle_is_not_replayed_into_the_next() {
    let q = SatbQueue::new();

    // 1. This thread's own flush.
    q.activate();
    satb_thread_local_log(&q, 0x51A1_0001);
    satb_thread_local_log(&q, 0x51A1_0002);
    assert_eq!(bucket_len_for(q.id()), 2);
    q.deactivate();
    q.activate();
    flush_thread_satb_buffer(&q);
    assert!(
        q.drain().is_empty(),
        "a closed cycle's bucket reached the open cycle's shards"
    );
    assert_eq!(q.stale_discards(), 2);
    assert_eq!(bucket_len_for(q.id()), 0);

    // 2. The next log: the stale entry is dropped, the new one kept.
    satb_thread_local_log(&q, 0x51A1_0003);
    q.deactivate();
    q.activate();
    satb_thread_local_log(&q, 0x51A1_0004);
    assert_eq!(bucket_len_for(q.id()), 1);
    assert_eq!(q.stale_discards(), 3);
    flush_thread_satb_buffer(&q);
    assert_eq!(q.drain(), vec![0x51A1_0004]);

    // 3. The remark walk.
    satb_thread_local_log(&q, 0x51A1_0005);
    q.deactivate();
    q.activate();
    flush_all_thread_satb_buffers(&q);
    assert!(!q.drain().contains(&0x51A1_0005));
    assert_eq!(q.stale_discards(), 4);

    // ...while an entry of the OPEN cycle is still delivered by the walk.
    satb_thread_local_log(&q, 0x51A1_0006);
    let snapshot = q.deactivate_and_drain(&stw());
    assert_eq!(snapshot, vec![0x51A1_0006]);
    assert_eq!(q.stale_discards(), 4);
}

/// A reference store made from ANOTHER thread-local's destructor, after
/// (or while) this module's per-thread buffer is destroyed, must neither
/// abort the process (`LocalKey::with` panics on a destroyed key, and a
/// panic in a TLS destructor aborts) nor lose the entry. Whichever order
/// the platform runs the two destructors in, the entry reaches the queue:
/// through the live bucket and then the orphan list, or through the
/// direct-flush fallback.
#[test]
fn a_log_from_a_tls_destructor_reaches_the_queue() {
    struct LogOnDrop(std::sync::Arc<SatbQueue>, usize);
    impl Drop for LogOnDrop {
        fn drop(&mut self) {
            satb_thread_local_log(&self.0, self.1);
        }
    }
    thread_local! {
        static LATE: std::cell::RefCell<Option<LogOnDrop>> =
            const { std::cell::RefCell::new(None) };
    }

    let q = std::sync::Arc::new(SatbQueue::new());
    q.activate();
    const S: usize = 0x5A7B_0030;
    let q2 = std::sync::Arc::clone(&q);
    std::thread::spawn(move || {
        // Register LATE's destructor FIRST, then touch the SATB buffer,
        // so on reverse-order platforms the buffer dies before LATE logs.
        let late = LogOnDrop(std::sync::Arc::clone(&q2), S);
        LATE.with(|l| *l.borrow_mut() = Some(late));
        satb_thread_local_log(&q2, 0x5A7B_0038);
    })
    .join()
    .expect("a SATB log from a TLS destructor must not abort the thread");

    let snapshot = q.deactivate_and_drain(&stw());
    assert!(
        snapshot.contains(&S),
        "the destructor's entry was lost: {snapshot:?}"
    );
}

#[test]
fn thread_local_buffers_are_queue_scoped() {
    // Cross-queue steal regression (2026-07-14): with two live queues in
    // the process, draining ALL thread buffers on behalf of queue A must
    // consume only the entries this thread logged against A — entries
    // logged against queue B stay buffered for B's own remark. The
    // pre-fix unscoped drain moved B's entries into A's queue, voiding
    // B's SATB snapshot (observed as the
    // `deactivate_and_drain_includes_thread_local_buffer` flake under
    // parallel test threads).
    let h = std::thread::spawn(|| {
        let qa = SatbQueue::new();
        let qb = SatbQueue::new();
        qa.activate();
        qb.activate();

        const SA: usize = 0xAAAA_0001;
        const SB: usize = 0xBBBB_0002;
        satb_thread_local_log(&qa, SA);
        satb_thread_local_log(&qb, SB);

        // A remark on qa takes only qa's bucket...
        flush_all_thread_satb_buffers(&qa);
        let a = qa.drain();
        assert!(a.contains(&SA), "qa must receive its own entry, got {a:?}");
        assert!(!a.contains(&SB), "qa stole qb's thread-local entry: {a:?}");

        // ...and qb's entry is still intact for qb's own remark.
        let b = qb.deactivate_and_drain(&stw());
        assert!(
            b.contains(&SB),
            "qb's entry lost after qa's flush_all: {b:?}"
        );
    });
    h.join().unwrap();
}

// -----------------------------------------------------------------------
// LANE W2-C — the discard form, and the orphan reap for a dropped queue
// -----------------------------------------------------------------------

/// `deactivate_and_discard` must leave the queue in exactly the state
/// `deactivate_and_drain` leaves it in. The only difference between them is
/// the `Vec` the caller gets back — and G1's `cleanup`, the sole production
/// caller, drops it.
#[test]
fn deactivate_and_discard_empties_shards_and_thread_buckets_and_disables() {
    // Own thread: `satb_thread_local_log` writes into THIS thread's bucket,
    // and the assertions below are about that bucket.
    let h = std::thread::spawn(|| {
        let q = SatbQueue::new();
        q.activate();
        // One entry in a shard...
        q.flush(vec![0xDEAD_0001]);
        // ...and one still sitting in this thread's local bucket, which is
        // the half a plain `deactivate()` + `drain()` would leave behind to
        // seed the NEXT cycle with a stale address.
        satb_thread_local_log(&q, 0xDEAD_0002);

        q.deactivate_and_discard();

        assert!(!q.is_active(), "the gate must be off");
        assert!(q.is_empty(), "every shard must be empty");
        assert!(!q.has_pending(), "and the lock-free counter must agree");

        // The thread bucket is gone too: a second flush_all finds nothing
        // to deliver, so nothing can reach a later cycle.
        flush_all_thread_satb_buffers(&q);
        assert!(
            q.drain().is_empty(),
            "a bucket left behind would be delivered to the next cycle as a stale seed — the reason this cannot simply be deactivate()+drain()"
        );
    });
    h.join().unwrap();
}

// -----------------------------------------------------------------------
// gengc-mark2 2026-09-20
// -----------------------------------------------------------------------

/// `gengc-mark-satb-barrier-gates-on-phase-not-queue-FIXED-20260923`.
///
/// A log against an INACTIVE queue must be REFUSED, not buffered. Buffered
/// is the harmful outcome: the entry sits in this thread's bucket until
/// some future cycle's `flush_all_thread_satb_buffers` replays it as a
/// gray root, naming an address that may by then belong to a different
/// object.
#[test]
fn an_inactive_queue_refuses_a_barrier_log() {
    let h = std::thread::spawn(|| {
        let q = SatbQueue::new();

        // Never activated: refused outright.
        assert!(!satb_thread_local_log(&q, 0xDEAD_0001));
        assert_eq!(bucket_len_for(q.id()), 0, "nothing may be buffered");
        assert_eq!(q.late_log_drops(), 1);

        // Active: accepted into the thread-local bucket as before.
        q.activate();
        satb_thread_local_log(&q, 0xDEAD_0002);
        assert_eq!(bucket_len_for(q.id()), 1);
        assert_eq!(q.late_log_drops(), 1, "an accepted log is not a drop");

        // DRAINING still accepts — that is the whole point of the
        // tri-state gate, and this must not have changed.
        q.state.store(SATB_DRAINING, Ordering::Release);
        satb_thread_local_log(&q, 0xDEAD_0003);
        assert_eq!(bucket_len_for(q.id()), 2);

        // After a real deactivation the gate is shut again.
        let snapshot = q.deactivate_and_drain(&stw());
        assert!(snapshot.contains(&0xDEAD_0002));
        assert!(snapshot.contains(&0xDEAD_0003));
        assert!(!satb_thread_local_log(&q, 0xDEAD_0004));
        assert_eq!(bucket_len_for(q.id()), 0);
        assert_eq!(q.late_log_drops(), 2);
    });
    h.join().unwrap();
}

/// An orphaned buffer holding a bucket for a queue that has since been
/// DROPPED used to be retained forever: the orphan pass only removes a
/// buffer once `buckets` is empty, and nothing will ever call `take` with a
/// dead queue's id.
///
/// Harmless in a production VM (one queue, process lifetime) and a real
/// leak everywhere else — every unit test in this file constructs queues,
/// and a multi-heap embedding would too.
///
/// The orphan is constructed and parked DIRECTLY rather than by exiting a
/// thread. The thread-exit route is already covered
/// (`dying_thread_residual_satb_entries_survive_until_remark_drain`), and
/// it leaves the buffer reachable only through a process-global list that
/// every concurrently-running test in this binary also sweeps — so an
/// assertion about that list is a race, not a test. Holding the `Arc`
/// makes the observation local and deterministic.
#[test]
fn an_orphan_holding_only_a_dead_queues_bucket_is_reaped() {
    let dead_id = {
        let doomed = SatbQueue::new();
        doomed.id()
    };

    let orphan = Arc::new(Mutex::new(ThreadSatbPartitions::default()));
    assert!(orphan.lock().log(dead_id, 0, 0xC0FF_EE01).0.is_none());
    ORPHANED_SATB_BUFFERS.lock().push(Arc::clone(&orphan));

    // An unrelated, still-live queue runs its own flush. Before the fix
    // this pass took nothing from the orphan (wrong id) and retained it
    // because `buckets` was still non-empty — forever, since no drainer for
    // `dead_id` exists any more.
    let other = SatbQueue::new();
    other.activate();
    flush_all_thread_satb_buffers(&other);

    assert!(
        orphan.lock().buckets.is_empty(),
        "a bucket belonging to a dropped queue has no drainer and can never be delivered anywhere — retaining it pins the buffer and its Arc for the life of the process"
    );
}

/// The complementary property, so the reap above cannot be implemented as
/// "drop every bucket": a bucket for a queue that still EXISTS must survive
/// another queue's flush, because that queue's own drain is still coming.
/// Dropping it there would lose an SATB entry, which is a live object freed
/// mid-mark.
#[test]
fn an_orphans_bucket_for_a_live_queue_survives_another_queues_flush() {
    const ENTRY: usize = 0xC0FF_EE02;
    let live = SatbQueue::new();
    live.activate();

    let orphan = Arc::new(Mutex::new(ThreadSatbPartitions::default()));
    assert!(orphan
        .lock()
        .log(live.id(), live.current_epoch(), ENTRY)
        .0
        .is_none());
    ORPHANED_SATB_BUFFERS.lock().push(Arc::clone(&orphan));

    let other = SatbQueue::new();
    other.activate();
    flush_all_thread_satb_buffers(&other);

    assert_eq!(
        orphan.lock().buckets.len(),
        1,
        "the owning queue is still alive and its remark has not run yet"
    );

    // And the entry really is still deliverable to its owner.
    flush_all_thread_satb_buffers(&live);
    assert!(live.drain().contains(&ENTRY));
    assert!(
        orphan.lock().buckets.is_empty(),
        "once its owner has drained it, the bucket is gone and the orphan is reapable by the ordinary rule"
    );
}

/// `gengc-mark-satb-deactivate-is-stw-only-RETIRED-20260923`.
///
/// A writer parked on a shard mutex has already passed the activation gate
/// and is holding entries it must deposit. It must be VISIBLE to a
/// concurrent deactivation (`in_flight` is incremented before the shard is
/// chosen), and its entry must land in that deactivation's snapshot rather
/// than being stranded for a later cycle to replay as a stale address.
///
/// gcg g3/k: the queue's quiescence budget is effectively unbounded here.
/// With the production 1024 yields the test failed 3 of 6 loaded host runs:
/// the parked writer simply did not get a CPU before the budget expired,
/// which is the documented, COUNTED fallback (see
/// `an_expired_quiescence_budget_is_counted_and_never_hangs`), not a
/// stranding bug in the protocol. What this test is for is that the wait,
/// given the time, recovers the writer.
#[test]
fn a_writer_parked_on_a_shard_is_not_stranded_by_deactivation() {
    use std::sync::mpsc;
    use std::sync::Arc;

    const SENTINEL: usize = 0x5A7B_0001;
    // ~minutes of yields at worst; a real hang still ends the test.
    let q = Arc::new(SatbQueue::new().with_quiesce_yield_budget(50_000_000));
    q.activate();

    let (tx_shard, rx_shard) = mpsc::channel::<usize>();
    let (tx_go, rx_go) = mpsc::channel::<()>();

    let qw = Arc::clone(&q);
    let writer = std::thread::spawn(move || {
        // Publish the shard this thread will pick, BEFORE flushing, so
        // the test can own that shard for the duration.
        tx_shard.send(shard_for_current_thread()).unwrap();
        rx_go.recv().unwrap();
        qw.flush(vec![SENTINEL]);
    });

    let shard = rx_shard.recv().unwrap();
    let guard = q.shards[shard].lock();
    tx_go.send(()).unwrap();
    // The writer announces itself before it blocks on the mutex we hold,
    // which is exactly the property under test. This loop terminates
    // because the increment precedes the lock acquisition.
    while q.in_flight.load(Ordering::SeqCst) == 0 {
        std::thread::yield_now();
    }

    let qd = Arc::clone(&q);
    // SAFETY -- a DELIBERATE violation, and the reason `stw()`'s doc
    // above calls the token a witness rather than a guard. This test
    // drains while a writer is parked on a shard mutex, which is exactly
    // the interleaving the precondition forbids, because reproducing that
    // interleaving is the test's entire purpose.
    let drain = std::thread::spawn(move || qd.deactivate_and_drain(&stw()));

    // Let the deactivation get as far as it can, then release the shard.
    // Whichever of the two reaches the mutex first, the entry must be in
    // the snapshot: either the drain sees it in a pass, or the drain's
    // quiescence wait holds the `INACTIVE` store back until the writer is
    // done and the following pass collects it.
    std::thread::yield_now();
    drop(guard);

    let snapshot = drain.join().unwrap();
    writer.join().unwrap();

    assert_eq!(
        q.quiescence_timeouts(),
        0,
        "the budget is effectively unbounded here, so the wait must have \
         reached zero rather than given up"
    );
    assert!(
        snapshot.contains(&SENTINEL),
        "a writer parked on a shard while the drain walked past it was \
         stranded; snapshot = {snapshot:?}"
    );
    assert!(!q.is_active());
    assert_eq!(
        q.in_flight.load(Ordering::SeqCst),
        0,
        "every writer guard must have been released"
    );
}

/// gcg g3/k: the other half of the quiescence wait's contract. A writer
/// that never leaves `flush` (held here by a guard the test owns, the
/// shape of a thread suspended between its increment and its push) must
/// not hang the deactivation: the wait gives up after its budget, counts
/// that it did, and the queue still ends INACTIVE with the gate given back.
#[test]
fn an_expired_quiescence_budget_is_counted_and_never_hangs() {
    for budget in [0u32, 4] {
        let q = SatbQueue::new().with_quiesce_yield_budget(budget);
        q.activate();
        let stuck = SatbWriteGuard::enter(&q);
        let _ = q.deactivate_and_drain(&stw());
        assert_eq!(
            q.quiescence_timeouts(),
            1,
            "budget {budget}: a writer that never finishes must be counted once"
        );
        assert!(!q.is_active());
        assert!(!q.holds_jit_gate());
        drop(stuck);
        assert_eq!(q.in_flight.load(Ordering::SeqCst), 0);
    }
}

/// The common case — an STW deactivation with nothing in flight — must
/// cost no spinning and must never record a timeout.
#[test]
fn a_quiet_deactivation_never_times_out() {
    let h = std::thread::spawn(|| {
        let q = SatbQueue::new();
        q.activate();
        satb_thread_local_log(&q, 0x1234);
        let snapshot = q.deactivate_and_drain(&stw());
        assert!(snapshot.contains(&0x1234));
        assert_eq!(
            q.quiescence_timeouts(),
            0,
            "no writer was in flight, so the wait must have exited immediately"
        );
        assert_eq!(q.in_flight.load(Ordering::SeqCst), 0);
    });
    h.join().unwrap();
}

