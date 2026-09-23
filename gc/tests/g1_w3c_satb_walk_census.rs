// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Lane W3-C — the SATB per-thread registry walk, measured instead of argued.
//!
//! # What this file is for
//!
//! `docs/internal/g1-2026-09-20/w2c-the-satb-registry-walk-is-still-o-threads-per-cycle.md`
//! filed the walk as O(mutator threads) lock acquisitions inside a
//! stop-the-world pause, refused to choose between three candidate fixes
//! without a number, and named the number: **calls, threads visited, threads
//! that actually held entries, nanoseconds.** It also named the discriminator
//! and the reason for it — a fast total is equally consistent with "the walk
//! is cheap" and with "this workload has four threads".
//!
//! `satb::registry_walk_census()` is that census. This file pins what it
//! counts, because a census that drifts from its name is the failure mode this
//! round has caught twice already (`work_units_done` was `steps × 256`; the
//! `EXHAUSTED ITS POOL` warning's `with_room` counted the pause's own TLAB
//! regions).
//!
//! # Why there is no wall-clock assertion here
//!
//! Three wall-clock bounds have already been widened in this round. The
//! scaling number belongs on a release binary under a real workload — see
//! `w3c-the-satb-registry-walk-census.md`, which carries it. What a test can
//! pin, and what is actually load-bearing, is that `threads_visited` counts
//! buffer locks taken and `threads_held` counts buffers that had something in
//! them, because the ENTIRE value of the dirty-buffer-list fix is the
//! difference between those two.
//!
//! # The process-global registry
//!
//! `SATB_BUFFER_REGISTRY` and the census behind it are process-global, and
//! Rust runs `#[test]` functions on parallel threads of ONE process per
//! integration-test file. Every test here therefore takes `SERIAL` and resets
//! the census under it. That is also why these live in their own file rather
//! than beside another lane's: a neighbouring test that logs a SATB entry
//! would land in this file's `threads_held`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex};

use cratonvm_gc::satb::{
    registry_walk_census, reset_registry_walk_census, satb_thread_local_log, SatbQueue,
};
use cratonvm_types::flags;

/// The STW witness these tests stand in for.
///
/// SAFETY: the safepoint precondition holds vacuously where the test's only
/// mutator is the test thread. Where a test drains WHILE a mutator runs, to
/// exercise the residual window, its call site says so.
///
/// Every test in THIS file is the second kind: the threads it registers are
/// parked on a barrier holding open SATB buckets, which is the state the
/// registry walk is being censused over. Nothing here treats a drained entry
/// as a snapshot — the subject is the walk, not what it returns.
fn stw() -> cratonvm_gc::StopTheWorldToken {
    unsafe { cratonvm_gc::StopTheWorldToken::new() }
}

/// Serialises every test in this file against the process-global registry.
static SERIAL: Mutex<()> = Mutex::new(());

/// `(calls, threads_visited, threads_held, orphans_visited, orphans_held,
/// entries, nanos)`.
type Census = (u64, u64, u64, u64, u64, u64, u64);

/// Park `threads` threads, each holding an open SATB bucket for `queue`, with
/// the first `logging` of them having logged one entry apiece. Returns once
/// every thread is registered and parked; the threads stay alive (and
/// therefore stay in the registry) until the returned `release` barrier is
/// waited on.
///
/// The threads must be ALIVE and PARKED, not joined: a joined thread's buffer
/// moves to `ORPHANED_SATB_BUFFERS` and is counted in a different column, and
/// the column this file is about is the live-mutator one.
fn park_registered_threads(
    queue: &Arc<SatbQueue>,
    threads: usize,
    logging: usize,
) -> (Arc<Barrier>, Vec<std::thread::JoinHandle<()>>) {
    let ready = Arc::new(Barrier::new(threads + 1));
    let release = Arc::new(Barrier::new(threads + 1));
    let mut handles = Vec::with_capacity(threads);
    for i in 0..threads {
        let q = Arc::clone(queue);
        let ready = Arc::clone(&ready);
        let release = Arc::clone(&release);
        handles.push(
            std::thread::Builder::new()
                .name(format!("w3c-satb-{i}"))
                .spawn(move || {
                    // Touching the thread-local is what registers this thread's
                    // buffer, so even a non-logging thread has to do it — those
                    // are the `visited - held` threads the census is about.
                    if i < logging {
                        // A sentinel unique per thread, so a lost entry is
                        // identifiable rather than merely absent.
                        satb_thread_local_log(&q, 0x5A7B_0000 + i * 8);
                    } else {
                        // Register without logging: `log` against an INACTIVE
                        // queue would not create a bucket, so use the same
                        // call and then take the bucket straight back out.
                        satb_thread_local_log(&q, 0x5A7B_0000 + i * 8);
                        cratonvm_gc::flush_thread_satb_buffer(&q);
                    }
                    ready.wait();
                    release.wait();
                })
                .expect("spawn"),
        );
    }
    ready.wait();
    (release, handles)
}

fn census() -> Census {
    registry_walk_census()
}

#[test]
fn the_walk_counts_every_registered_thread_and_separately_those_that_held_entries() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let q = Arc::new(SatbQueue::new());
    q.activate();

    const THREADS: usize = 16;
    const LOGGING: usize = 5;
    let (release, handles) = park_registered_threads(&q, THREADS, LOGGING);

    reset_registry_walk_census();
    let _stragglers = q.deactivate_and_drain(&stw());
    let (calls, visited, held, _ov, _oh, entries, _nanos) = census();

    release.wait();
    for h in handles {
        h.join().expect("join");
    }

    assert_eq!(calls, 1, "one drain is one walk");
    // `>=`, not `==`: this process's own test thread is registered too, and on
    // a loaded machine the runner's other threads may be. The direction that
    // matters is that no parked mutator was skipped.
    assert!(
        visited >= THREADS as u64,
        "the walk must visit every registered thread: visited={visited} threads={THREADS}"
    );
    assert_eq!(
        held, LOGGING as u64,
        "only the threads that logged may count as holding: held={held}"
    );
    assert_eq!(
        entries, LOGGING as u64,
        "one entry per logging thread: entries={entries}"
    );
    assert!(
        visited > held,
        "this fixture exists to produce visited > held, which is the gap a \
         dirty-buffer list would remove; visited={visited} held={held}"
    );
}

#[test]
fn a_walk_that_finds_nothing_still_pays_for_every_thread() {
    // The reading that decides the page. If `held` is always equal to
    // `visited` the dirty-buffer-list fix buys nothing; if `held` can be ZERO
    // while `visited` is large, the whole walk is waste on that cycle, and
    // that is the case a mostly-idle thread pool is in.
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let q = Arc::new(SatbQueue::new());
    q.activate();

    const THREADS: usize = 12;
    let (release, handles) = park_registered_threads(&q, THREADS, 0);

    reset_registry_walk_census();
    let _stragglers = q.deactivate_and_drain(&stw());
    let (_calls, visited, held, _ov, _oh, entries, _nanos) = census();

    release.wait();
    for h in handles {
        h.join().expect("join");
    }

    assert!(visited >= THREADS as u64, "visited={visited}");
    assert_eq!(
        held, 0,
        "no thread logged, so none may be counted as holding"
    );
    assert_eq!(entries, 0, "no entries to move");
}

#[test]
fn the_discard_walk_is_measured_on_the_same_scale_as_the_flush_walk() {
    // `CRATONVM_G1_CLEANUP_SATB_DISCARD`'s A/B compares two walks. If only one
    // of them were instrumented the comparison would be between a measurement
    // and a silence, which is the shape §7.1 of
    // `orchestrator-wave-1-measurements.md` is about.
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    let run = |discard: bool| -> Census {
        let q = Arc::new(SatbQueue::new());
        q.activate();
        let (release, handles) = park_registered_threads(&q, 8, 3);
        reset_registry_walk_census();
        if discard {
            q.deactivate_and_discard();
        } else {
            let _ = q.deactivate_and_drain(&stw());
        }
        let c = census();
        release.wait();
        for h in handles {
            h.join().expect("join");
        }
        c
    };

    let (dc, dv, dh, _, _, de, _) = run(true);
    let (fc, fv, fh, _, _, fe, _) = run(false);

    assert_eq!(dc, 1, "the discard form records exactly one walk");
    assert_eq!(fc, 1, "so does the flush form");
    assert_eq!(
        dh, fh,
        "both forms must see the same number of threads HOLDING entries \
         ({dh} vs {fh}); if they differ, one of them is skipping buckets and \
         the discard form's whole correctness argument (it visits every bucket, \
         it only declines to move them) is false"
    );
    assert_eq!(de, fe, "and the same entry count ({de} vs {fe})");
    assert!(dv >= 8 && fv >= 8, "visited: discard={dv} flush={fv}");
}

#[test]
fn the_census_is_readable_from_the_report_that_prints_on_both_shutdown_arms() {
    // The point of the whole exercise. A number that only a test can read is
    // not an instrument on a shipped binary — see
    // `w3c-print-gc-summary-and-the-system-exit-arm.md`.
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let q = Arc::new(SatbQueue::new());
    q.activate();
    let (release, handles) = park_registered_threads(&q, 4, 2);
    let _ = q.deactivate_and_drain(&stw());
    release.wait();
    for h in handles {
        h.join().expect("join");
    }

    let text = cratonvm_gc::gc_metrics::collector_decision_report();
    assert!(
        text.contains("[GC] g1 satb-walk:"),
        "the registry-walk census must appear in collector_decision_report, \
         which vm-cli emits on BOTH the normal-return and the System.exit arm. \
         Report was:\n{text}"
    );
    for field in [
        "calls=",
        "threads_visited=",
        "threads_held=",
        "orphans_visited=",
        "entries=",
        "us_per_call=",
    ] {
        assert!(
            text.contains(field),
            "missing `{field}` from the satb-walk line:\n{text}"
        );
    }
}

#[test]
fn the_walk_scales_linearly_in_registered_threads_and_says_so_in_its_own_numbers() {
    // NOT a wall-clock bound — see the module header. This asserts the SHAPE
    // the page's argument depends on: that `threads_visited` is proportional
    // to the number of registered threads, i.e. that the cost really is
    // O(threads) and not O(threads that logged). If a future change makes the
    // walk skip clean buffers, THIS is the assertion that will fail, and the
    // failure is the signal to re-run the measurement rather than a bug.
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    let visited_at = |threads: usize| -> u64 {
        let q = Arc::new(SatbQueue::new());
        q.activate();
        let (release, handles) = park_registered_threads(&q, threads, 1);
        reset_registry_walk_census();
        let _ = q.deactivate_and_drain(&stw());
        let (_, v, _, _, _, _, _) = census();
        release.wait();
        for h in handles {
            h.join().expect("join");
        }
        v
    };

    let small = visited_at(4);
    let large = visited_at(32);
    assert!(
        large >= small + 28,
        "visiting 28 more threads must cost 28 more buffer locks: \
         small={small} large={large}. If this fails the walk has become \
         sub-linear in registered threads, which is the fix \
         `w2c-the-satb-registry-walk-is-still-o-threads-per-cycle.md` proposes \
         — good news, but the page and its measurement both need rewriting."
    );
}

#[test]
fn the_census_moves_on_a_generational_run_too_and_that_is_deliberate() {
    // `ConcurrentMarker` (the non-G1 backend's marker) reaches the same walk
    // through `deactivate_and_drain`. The constraint on this round is that
    // non-G1 collectors are behaviourally unchanged, not that they are
    // uninstrumented — and a counter that stopped at the G1 boundary would
    // report a zero on a generational run that reads as "the walk never ran".
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let hits = AtomicUsize::new(0);
    flags::with_process_overrides(&[("CRATONVM_G1_CLEANUP_SATB_DISCARD", None)], || {
        let q = Arc::new(SatbQueue::new());
        q.activate();
        let (release, handles) = park_registered_threads(&q, 3, 1);
        reset_registry_walk_census();
        let _ = q.deactivate_and_drain(&stw());
        let (calls, visited, held, _, _, _, _) = census();
        release.wait();
        for h in handles {
            h.join().expect("join");
        }
        assert_eq!(calls, 1);
        assert!(visited >= 3);
        assert_eq!(held, 1);
        hits.fetch_add(1, Ordering::Relaxed);
    });
    assert_eq!(
        hits.load(Ordering::Relaxed),
        1,
        "the override body must run"
    );
}
