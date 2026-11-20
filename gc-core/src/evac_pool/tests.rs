use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Every dispatched index runs exactly once, and the driver's share runs on
/// the calling thread.
#[test]
fn a_job_reaches_every_worker_exactly_once() {
    let pool = EvacPool::new(3);
    let seen: Vec<AtomicUsize> = (0..3).map(|_| AtomicUsize::new(0)).collect();
    let driver_thread = std::thread::current().id();
    let driver_ran = AtomicUsize::new(0);

    for _ in 0..50 {
        for s in &seen {
            s.store(0, Ordering::Relaxed);
        }
        let body = |i: usize| {
            seen[i].fetch_add(1, Ordering::Relaxed);
        };
        pool.scope(3, &body, || {
            assert_eq!(std::thread::current().id(), driver_thread);
            driver_ran.fetch_add(1, Ordering::Relaxed);
        });
        for (i, s) in seen.iter().enumerate() {
            assert_eq!(
                s.load(Ordering::Relaxed),
                1,
                "worker {i} ran the wrong number of times"
            );
        }
    }
    assert_eq!(driver_ran.load(Ordering::Relaxed), 50);
}

/// gcj pcp: an elastic round returns once the driver's share and every
/// helper that STARTED have finished; a helper that has not started by
/// then is released (counted) and never runs that round's body, and the
/// pool keeps serving later rounds, plain and elastic, exactly.
#[test]
fn gcj_pcp_an_elastic_round_does_not_wait_for_a_helper_that_never_started() {
    let pool = EvacPool::new(3);
    // A driver that returns at once: each helper either started before
    // the close (and ran once) or was released (and never runs it).
    let ran = AtomicUsize::new(0);
    let body = |_: usize| {
        ran.fetch_add(1, Ordering::Relaxed);
    };
    for round in 1..=200usize {
        pool.scope_elastic(3, &body, || {});
        let total = ran.load(Ordering::Relaxed) + pool.census().elastic_skipped as usize;
        assert_eq!(total, 3 * round, "round {round}: a helper ran twice or was lost");
    }
    // The pool still serves a plain round on every helper, and an elastic
    // one whose driver waits until all three have started.
    let seen: Vec<AtomicUsize> = (0..3).map(|_| AtomicUsize::new(0)).collect();
    let body = |i: usize| {
        seen[i].fetch_add(1, Ordering::Relaxed);
    };
    pool.scope(3, &body, || {});
    assert!(seen.iter().all(|s| s.load(Ordering::Relaxed) == 1));
    let before = pool.census().elastic_skipped;
    let arrived = AtomicUsize::new(0);
    let release = std::sync::atomic::AtomicBool::new(false);
    let body = |_: usize| {
        arrived.fetch_add(1, Ordering::Relaxed);
        while !release.load(Ordering::Acquire) {
            std::thread::yield_now();
        }
    };
    // The driver returns only once all three helpers started; they are
    // still running then, so the close must wait for them, not skip them.
    pool.scope_elastic(3, &body, || {
        while arrived.load(Ordering::Relaxed) < 3 {
            std::thread::yield_now();
        }
        release.store(true, Ordering::Release);
    });
    assert_eq!(arrived.load(Ordering::Relaxed), 3);
    assert_eq!(pool.census().elastic_skipped, before, "nobody skipped: all started");
}

/// The barrier is real: nothing a worker writes may still be in flight when
/// `scope` returns, because the driver reads those slots immediately
/// afterwards to merge the forwarding shards.
#[test]
fn scope_does_not_return_before_every_worker_has_finished() {
    let pool = EvacPool::new(3);
    let counter = AtomicUsize::new(0);
    let body = |_: usize| {
        // Long enough that a missing barrier loses the race reliably.
        std::thread::sleep(std::time::Duration::from_millis(5));
        counter.fetch_add(1, Ordering::SeqCst);
    };
    pool.scope(3, &body, || {});
    assert_eq!(counter.load(Ordering::SeqCst), 3);
}

/// A pool with fewer threads than the job asks for must still terminate —
/// `pending` has to be sized from the clamped count, not the request.
#[test]
fn a_request_larger_than_the_pool_is_clamped_rather_than_hanging() {
    let pool = EvacPool::new(2);
    let counter = AtomicUsize::new(0);
    let body = |_: usize| {
        counter.fetch_add(1, Ordering::SeqCst);
    };
    pool.scope(8, &body, || {});
    assert_eq!(counter.load(Ordering::SeqCst), 2);
}

/// Sizing a job below the pool's width must leave the unused workers parked
/// AND still let the barrier fall, which is the case a naive
/// `pending = threads.len()` would hang on.
#[test]
fn a_job_narrower_than_the_pool_leaves_the_extra_workers_parked() {
    let pool = EvacPool::new(4);
    let seen: Vec<AtomicUsize> = (0..4).map(|_| AtomicUsize::new(0)).collect();
    let body = |i: usize| {
        seen[i].fetch_add(1, Ordering::Relaxed);
    };
    pool.scope(2, &body, || {});
    assert_eq!(seen[0].load(Ordering::Relaxed), 1);
    assert_eq!(seen[1].load(Ordering::Relaxed), 1);
    assert_eq!(seen[2].load(Ordering::Relaxed), 0);
    assert_eq!(seen[3].load(Ordering::Relaxed), 0);
    // ...and the pool is still usable at full width afterwards.
    pool.scope(4, &body, || {});
    for s in &seen {
        assert!(s.load(Ordering::Relaxed) >= 1);
    }
}

/// THE hang case. A worker panic must surface on the driver thread, not
/// leave `pending` stuck above zero with the regions lock held.
#[test]
fn a_panicking_worker_fails_the_pause_instead_of_wedging_it() {
    let pool = EvacPool::new(2);
    let body = |i: usize| {
        if i == 0 {
            panic!("worker exploded");
        }
    };
    let outcome = catch_unwind(AssertUnwindSafe(|| pool.scope(2, &body, || {})));
    assert!(outcome.is_err(), "the driver must observe the worker panic");
    // And the pool survives it: a leaked `pending` would hang here.
    let counter = AtomicUsize::new(0);
    let ok = |_: usize| {
        counter.fetch_add(1, Ordering::SeqCst);
    };
    pool.scope(2, &ok, || {});
    assert_eq!(counter.load(Ordering::SeqCst), 2);
}

/// gengc-round1-alloc, 2026-09-20 — the worker's OWN panic payload reaches
/// the driver.
///
/// The message is the whole diagnostic value of a worker panic: it carries
/// the assertion text, the source location and (for the evacuator's
/// header screens) the object address. Replacing it with a fixed summary
/// string left the surviving report naming the mechanism and nothing about
/// the cause.
#[test]
fn a_worker_panic_arrives_with_its_own_message() {
    let pool = EvacPool::new(2);
    let body = |i: usize| {
        if i == 1 {
            panic!("forwarding pointer at 0x2008684 is not a base");
        }
    };
    let err = catch_unwind(AssertUnwindSafe(|| pool.scope(2, &body, || {})))
        .expect_err("the driver must observe the worker panic");
    let msg = err
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| err.downcast_ref::<&'static str>().copied())
        .unwrap_or("");
    assert!(
        msg.contains("0x2008684"),
        "the worker's own payload must reach the driver, got {msg:?}",
    );

    // ...and it is not re-raised against the NEXT collection.
    pool.scope(2, &|_: usize| {}, || {});
}

/// A driver panic must not skip the barrier — the workers still hold a
/// reference to the driver's closure environment.
#[test]
fn a_driver_panic_still_waits_for_the_workers() {
    let pool = EvacPool::new(2);
    let counter = AtomicUsize::new(0);
    let body = |_: usize| {
        std::thread::sleep(std::time::Duration::from_millis(5));
        counter.fetch_add(1, Ordering::SeqCst);
    };
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        pool.scope(2, &body, || panic!("driver exploded"))
    }));
    assert!(outcome.is_err());
    assert_eq!(
        counter.load(Ordering::SeqCst),
        2,
        "the barrier must hold even when the driver unwinds"
    );
}

/// gengc-round2-alloc2, 2026-09-20 — the pool GROWS, and a worker spawned
/// into a live pool joins the next round cleanly.
///
/// This is the mechanism half of
/// `gengc-alloc-evac-pool-width-frozen-FIXED-20260923.md`: a pool built at the
/// first cycle's width used to run every later cycle at that width, so a
/// young generation that grew into a wide evacuation never got one. The
/// hazard the test is really pinning is the new worker's `last_generation`:
/// seeded at 0 it would read the round already behind it as new work and
/// `expect` a `job` the barrier has cleared, so the assertion that matters
/// is that the grow is followed by a CORRECT full-width dispatch with every
/// index running exactly once.
#[test]
fn a_pool_grows_on_demand_and_the_new_workers_join_the_next_round() {
    let pool = EvacPool::new(1);
    assert_eq!(pool.helpers(), 1);

    let seen: Vec<AtomicUsize> = (0..4).map(|_| AtomicUsize::new(0)).collect();
    let body = |i: usize| {
        seen[i].fetch_add(1, Ordering::Relaxed);
    };

    // Narrow to begin with: the clamp still holds while the pool is small.
    pool.scope(4, &body, || {});
    assert_eq!(seen[0].load(Ordering::Relaxed), 1);
    assert_eq!(seen[3].load(Ordering::Relaxed), 0, "no worker 3 exists yet");

    pool.ensure_helpers(4);
    assert_eq!(pool.helpers(), 4);
    // Idempotent, and never shrinks.
    pool.ensure_helpers(2);
    assert_eq!(pool.helpers(), 4);

    for s in &seen {
        s.store(0, Ordering::Relaxed);
    }
    pool.scope(4, &body, || {});
    for (i, s) in seen.iter().enumerate() {
        assert_eq!(
            s.load(Ordering::Relaxed),
            1,
            "after the grow, worker {i} must run exactly once"
        );
    }

    // ...and the widened pool is still correct across repeated dispatches,
    // which is where a lost or misdirected notification would show up.
    for _ in 0..20 {
        for s in &seen {
            s.store(0, Ordering::Relaxed);
        }
        pool.scope(4, &body, || {});
        for s in &seen {
            assert_eq!(s.load(Ordering::Relaxed), 1);
        }
    }
}

/// A grown pool must still shut down. Every worker has its own condvar
/// now, so a `Drop` that notified only the last round's workers would hang
/// in `join` — the exact failure this module exists to avoid.
#[test]
fn a_grown_pool_still_drops_cleanly_after_a_narrow_job() {
    let pool = EvacPool::new(1);
    pool.ensure_helpers(4);
    let body = |_: usize| {};
    // Deliberately narrow: workers 1..4 are left parked and were never
    // woken by this dispatch, so only `Drop`'s own loop can release them.
    pool.scope(1, &body, || {});
    drop(pool);
}

/// The census line must carry the field names a log grep needs, and the
/// printed width must be the derived `built_with + grew_by` rather than a
/// counter of its own.
///
/// g1gen/c6: the census is the pool's own, so the line is pinned exactly:
/// byte for byte the line the process statics printed for a process with
/// one heap (field order, spelling, `pool_count`), and the `None` form a
/// heap that never built its pool prints.
#[test]
fn the_pool_census_line_carries_the_field_names_a_log_grep_needs() {
    let pool = EvacPool::with_name(2, "gen-evac");
    pool.ensure_helpers(3);
    let body = |_: usize| {};
    pool.scope(3, &body, || {});
    pool.scope(1, &body, || {});
    let line = super::evac_pool_census_line(Some(pool.census()));
    assert_eq!(
        line,
        "[GC] evac_pool: pool_built_with=2 pool_grew_by=1 pool_width=3 \
         pool_dispatches=2 pool_inline_runs=0 pool_max_dispatched=3 \
         pool_spawn_refused=0 pool_count=1 pool_elastic_skipped=0"
    );
    assert_eq!(
        super::evac_pool_census_line(None),
        "[GC] evac_pool: pool_built_with=0 pool_grew_by=0 pool_width=0 \
         pool_dispatches=0 pool_inline_runs=0 pool_max_dispatched=0 \
         pool_spawn_refused=0 pool_count=0 pool_elastic_skipped=0"
    );
}

/// Growing a pool must move `grew_by`, and by the number of helpers that
/// actually appeared. This is the ENGAGEMENT check for the census: a
/// counter that cannot move is the failure the sibling gap pages were
/// filed over. Exact, since the census is this pool's alone.
#[test]
fn the_census_sees_a_pool_grow() {
    let pool = EvacPool::new(1);
    assert_eq!(pool.census().grew_by, 0);
    pool.ensure_helpers(3);
    assert_eq!(pool.helpers(), 3, "the pool itself must have grown");
    let c = pool.census();
    assert_eq!((c.built_with, c.grew_by, c.width()), (1, 2, 3));
}

/// g1gen/c6 (`zgc-zt10rl3`, carried to G1 / Generational): two pools of
/// one process -- G1's `g1-evac` and the generational heap's `gen-evac`,
/// as two VMs of an embedding would own -- each count only their own
/// jobs. With the process statics, each line was the sum (and the widest
/// `built_with`) of both.
#[test]
fn g1gen_c6_two_heaps_pools_keep_separate_censuses() {
    let g1 = EvacPool::new(3);
    let gen = EvacPool::with_name(1, "gen-evac");
    let body = |_: usize| {};
    for _ in 0..5 {
        g1.scope(3, &body, || {});
    }
    gen.scope(1, &body, || {});
    gen.ensure_helpers(2);
    let (a, b) = (g1.census(), gen.census());
    assert_eq!(
        (a.built_with, a.grew_by, a.dispatches, a.max_dispatched),
        (3, 0, 5, 3),
        "G1's pool"
    );
    assert_eq!(
        (b.built_with, b.grew_by, b.dispatches, b.max_dispatched),
        (1, 1, 1, 1),
        "the generational pool"
    );
    let la = super::evac_pool_census_line(Some(a));
    let lb = super::evac_pool_census_line(Some(b));
    assert!(la.contains("pool_dispatches=5 ") && la.contains("pool_count=1"), "{la}");
    assert!(lb.contains("pool_dispatches=1 ") && lb.contains("pool_count=1"), "{lb}");
}

/// gen r4w6/young6: the lock-free `pending` decrement and the notify
/// outside `state`, under load. Thousands of back-to-back dispatches at
/// varying widths, each worker writing only its own slot with `Relaxed`,
/// and the driver reading every slot the instant `scope` returns: a
/// missing happens-before edge (the `Release` decrement / `Acquire` wait),
/// a lost wake-up (the unlocked notify) or a worker that ran twice or not
/// at all fails an assertion or hangs here.
#[test]
fn back_to_back_dispatches_keep_the_barrier_with_the_atomic_pending_count() {
    const W: usize = 8;
    let pool = EvacPool::new(W);
    let slots: Vec<AtomicUsize> = (0..W).map(|_| AtomicUsize::new(0)).collect();
    let runs: Vec<AtomicUsize> = (0..W).map(|_| AtomicUsize::new(0)).collect();
    let mut expected_runs = [0usize; W];
    for round in 1..=3000usize {
        // Widths 1..=W, so narrow rounds (sized-out workers left parked)
        // interleave with full ones.
        let width = 1 + round % W;
        let body = |i: usize| {
            slots[i].store(round, Ordering::Relaxed);
            runs[i].fetch_add(1, Ordering::Relaxed);
        };
        pool.scope(width, &body, || {});
        for (i, s) in slots.iter().enumerate().take(width) {
            assert_eq!(
                s.load(Ordering::Relaxed),
                round,
                "worker {i}'s write of round {round} was not visible after the barrier"
            );
            expected_runs[i] += 1;
        }
    }
    for (i, r) in runs.iter().enumerate() {
        assert_eq!(
            r.load(Ordering::Relaxed),
            expected_runs[i],
            "worker {i} ran the wrong number of times"
        );
    }
}

/// An empty pool is the `CRATONVM_G1_WORKERS=1` determinism mode.
#[test]
fn an_empty_pool_runs_the_driver_inline() {
    let pool = EvacPool::new(0);
    let ran = AtomicUsize::new(0);
    let body = |_: usize| unreachable!("no helpers to dispatch to");
    pool.scope(4, &body, || {
        ran.fetch_add(1, Ordering::Relaxed);
    });
    assert_eq!(ran.load(Ordering::Relaxed), 1);
}

/// zt t1/rl: a `per_heap` pool's census is exact for that pool (built,
/// grown, dispatched, inline), and the pool still names its workers.
/// Exact equalities are safe here: nothing else in the binary touches
/// THIS pool's census, unlike the process statics.
#[test]
fn zt1_rl_a_per_heap_pool_counts_its_own_census() {
    let pool = EvacPool::per_heap(1, "zgc-reloc");
    assert_eq!(pool.census().built_with, 1);
    pool.ensure_helpers(3);
    let ran = AtomicUsize::new(0);
    let body = |_: usize| {
        ran.fetch_add(1, Ordering::Relaxed);
    };
    pool.scope(2, &body, || {});
    pool.scope(3, &body, || {});
    assert_eq!(ran.load(Ordering::Relaxed), 5);
    let c = pool.census();
    assert_eq!(
        c,
        EvacPoolCensus {
            built_with: 1,
            grew_by: 2,
            spawn_refused: 0,
            dispatches: 2,
            inline_runs: 0,
            max_dispatched: 3,
            dispatch_while_busy: 0,
            elastic_skipped: 0,
        }
    );
    assert_eq!(c.width(), 3);
    let empty = EvacPool::per_heap(0, "zgc-reloc");
    empty.scope(4, &body, || {});
    assert_eq!(empty.census().inline_runs, 1);
    assert_eq!(empty.census().dispatches, 0);
}

/// zt t1/rl: the classic constructors keep their own census too (since
/// g1gen/c6 the only one).
#[test]
fn zt1_rl_a_classic_pool_also_keeps_its_own_census() {
    let pool = EvacPool::with_name(2, "gen-evac");
    let body = |_: usize| {};
    pool.scope(2, &body, || {});
    let c = pool.census();
    assert_eq!((c.built_with, c.dispatches, c.max_dispatched), (2, 1, 2));
}

/// gen r4w2/youngpolicy: `with_name` names the workers, and `new` keeps
/// G1's historical `g1-evac-N` names that crash records are keyed on.
#[test]
fn a_named_pool_names_its_workers_and_new_keeps_the_g1_name() {
    let names = |pool: &EvacPool| {
        let seen: Mutex<Vec<String>> = Mutex::new(Vec::new());
        let body = |_: usize| {
            seen.lock()
                .push(std::thread::current().name().unwrap_or("").to_string());
        };
        pool.scope(2, &body, || {});
        let mut v = seen.into_inner();
        v.sort();
        v
    };
    assert_eq!(
        names(&EvacPool::with_name(2, "gen-evac")),
        vec!["gen-evac-0".to_string(), "gen-evac-1".to_string()],
    );
    assert_eq!(
        names(&EvacPool::new(2)),
        vec!["g1-evac-0".to_string(), "g1-evac-1".to_string()],
    );
}

/// gci i2/st: run `f` on its own thread and FAIL when it has not returned
/// within `secs`, so a lost wake-up is a test failure, not a hung binary.
fn st2_within<F: FnOnce() + Send + 'static>(secs: u64, what: &str, f: F) {
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        f();
        let _ = tx.send(());
    });
    match rx.recv_timeout(std::time::Duration::from_secs(secs)) {
        Ok(()) => {}
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            panic!("{what}: not done within {secs} s (lost wake-up or unreachable barrier)")
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            panic!("{what}: panicked")
        }
    }
}

/// gci i2/st -- a helper that JOINS LATE: `ensure_helpers` spawns the
/// threads and the very next call dispatches to them, usually before a new
/// thread has run its first instruction, let alone parked on its condvar.
/// The dispatch's `notify_one` then reaches nobody; the worker must find
/// the round through the generation it reads on its first lock, or the
/// barrier never falls. Fresh pools, so the race is re-run every time.
#[test]
fn st2_a_worker_spawned_just_before_a_dispatch_still_runs_it() {
    st2_within(120, "spawn-then-dispatch", || {
        for round in 0..100usize {
            let pool = EvacPool::new(0);
            let width = 1 + round % 4;
            pool.ensure_helpers(width);
            let seen: Vec<AtomicUsize> = (0..width).map(|_| AtomicUsize::new(0)).collect();
            let body = |i: usize| {
                seen[i].fetch_add(1, Ordering::Relaxed);
            };
            pool.scope(width, &body, || {});
            for (i, s) in seen.iter().enumerate() {
                assert_eq!(s.load(Ordering::Relaxed), 1, "round {round} worker {i}");
            }
        }
    });
}

/// gci i2/st -- helpers that reach the job AFTER the driver's own share
/// returned (the driver's share is empty, each helper yields or sleeps a
/// varying few tens of microseconds first), across widths that shrink and
/// grow from one round to the next so a worker sized out of one round is
/// woken for the next. Every dispatched helper must run exactly once per
/// round, and `scope` must not return before the last of them.
#[test]
fn st2_helpers_that_reach_the_job_after_the_driver_finished_hold_the_barrier() {
    st2_within(120, "late-helper barrier", || {
        let pool = EvacPool::new(3);
        let done = AtomicUsize::new(0);
        for round in 0..600usize {
            let width = 1 + round % 3;
            let body = |i: usize| {
                match (round + i) % 3 {
                    0 => std::thread::yield_now(),
                    1 => std::thread::sleep(std::time::Duration::from_micros(
                        ((round * 7 + i) % 5) as u64 * 20,
                    )),
                    _ => {}
                }
                done.fetch_add(1, Ordering::SeqCst);
            };
            let before = done.load(Ordering::SeqCst);
            pool.scope(width, &body, || {});
            assert_eq!(
                done.load(Ordering::SeqCst),
                before + width,
                "round {round}: scope returned before every helper finished"
            );
        }
    });
}
