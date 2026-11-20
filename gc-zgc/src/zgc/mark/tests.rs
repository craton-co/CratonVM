use super::*;
use rustc_hash::FxHashMap;
use std::sync::atomic::AtomicBool as StdAtomicBool;

/// Build a graph from `(node, children)` pairs, auto-inserting any child
/// that was not declared as a leaf.
fn graph(edges: &[(u64, &[u64])]) -> FxHashMap<u64, Vec<u64>> {
    let mut g: FxHashMap<u64, Vec<u64>> = FxHashMap::default();
    for (node, children) in edges {
        g.insert(*node, children.to_vec());
        for c in children.iter() {
            g.entry(*c).or_default();
        }
    }
    g
}

fn sorted(mut v: Vec<u64>) -> Vec<u64> {
    v.sort_unstable();
    v
}

/// Generous restart budget: none of these tests should need more than a
/// couple, but a loaded CI box can interleave a mutator mark into more
/// than one pass and that is correct behaviour, not a failure.
const RESTART_BUDGET: usize = 64;

// -- sizing -------------------------------------------------------------

#[test]
fn stripe_count_is_a_clamped_power_of_two() {
    for workers in 1..=64usize {
        let s = stripe_count_for(workers);
        assert!(s.is_power_of_two(), "workers={workers} -> {s}");
        assert!(s >= Z_MARK_MIN_STRIPES, "workers={workers} -> {s}");
        assert!(s <= Z_MARK_MAX_STRIPES, "workers={workers} -> {s}");
        assert!(
            s >= workers.min(Z_MARK_MAX_STRIPES),
            "at least one stripe per worker: workers={workers} -> {s}"
        );
    }
    assert_eq!(stripe_count_for(1), Z_MARK_MIN_STRIPES);
    assert_eq!(stripe_count_for(4), 16);
    assert_eq!(stripe_count_for(1024), Z_MARK_MAX_STRIPES);
}

#[test]
fn mark_color_maps_only_the_two_mark_bits() {
    assert_eq!(mark_color_for(Z_MARKED0), Some(ZColor::Marked0));
    assert_eq!(mark_color_for(Z_MARKED1), Some(ZColor::Marked1));
    assert_eq!(mark_color_for(0), None);
    assert_eq!(mark_color_for(Z_MARKED0 | Z_MARKED1), None);
}

// -- local stack / stripes ----------------------------------------------

#[test]
fn local_stack_publishes_the_oldest_half() {
    let mut s = ZMarkLocalStack::new();
    for i in 0..10u64 {
        s.push(i);
    }
    let half = s.split_off_oldest_half();
    assert_eq!(half, vec![0, 1, 2, 3, 4], "the OLDEST half is handed out");
    // The newest entries — this worker's DFS frontier — stay local, and
    // still pop in LIFO order.
    assert_eq!(s.pop(), Some(9));
    assert_eq!(s.len(), 4);
}

#[test]
fn steal_half_splits_and_drain_from_takes_everything() {
    let stripes = ZMarkStripeSet::new(8);
    let mut work: Vec<u64> = (0..10).collect();
    assert_eq!(stripes.publish(3, &mut work), 10);
    assert!(work.is_empty(), "publish moves, it does not copy");
    assert!(stripes.has_work());

    let mut out = Vec::new();
    assert_eq!(stripes.steal_half(3, &mut out, Z_MARK_STEAL_CAP), 5);
    assert_eq!(out.len(), 5);
    assert!(stripes.has_work(), "the victim keeps half");

    let mut rest = Vec::new();
    assert_eq!(stripes.drain_from(3, &mut rest, Z_MARK_STEAL_CAP), 5);
    assert!(!stripes.has_work());
    assert_eq!(stripes.total_len(), 0);
}

#[test]
fn steal_cap_bounds_a_single_steal() {
    let stripes = ZMarkStripeSet::new(8);
    let mut work: Vec<u64> = (0..(Z_MARK_STEAL_CAP as u64 * 4)).collect();
    stripes.publish(0, &mut work);
    let mut out = Vec::new();
    let got = stripes.steal_half(0, &mut out, Z_MARK_STEAL_CAP);
    assert_eq!(got, Z_MARK_STEAL_CAP);
    assert_eq!(out.len(), Z_MARK_STEAL_CAP);
}

// -- single worker ------------------------------------------------------

#[test]
fn single_worker_marks_the_reachable_set_and_nothing_else() {
    // Reachable from root 1: {1,2,3,4,5}. Detached: {90,91}. Never
    // declared at all (a wild pointer): 999, referenced by 5.
    let mut g = graph(&[
        (1, &[2, 3]),
        (2, &[4]),
        (3, &[4, 5]),
        (4, &[]),
        (5, &[999]),
        (90, &[91]),
        (91, &[]),
    ]);
    g.remove(&999);
    let ctx = Arc::new(TestMarkContext::new(g));
    let pool = ZMarkCoordinator::new(ctx.clone(), 1);

    pool.begin_cycle();
    assert_eq!(pool.push_roots(&[1]), 1);
    let report = pool.mark_to_completion(RESTART_BUDGET);
    pool.end_cycle();

    assert!(!report.budget_exhausted);
    assert_eq!(ctx.marked_sorted(), vec![1, 2, 3, 4, 5]);
    assert!(
        report.stats.off_heap_children >= 1,
        "the undeclared child 999 must have been refused, not traced"
    );
    pool.shutdown();
}

#[test]
fn cycles_in_the_object_graph_terminate() {
    // A pure cycle, plus a self-loop, plus a back edge to the root.
    let g = graph(&[(1, &[2]), (2, &[3]), (3, &[1, 4]), (4, &[4, 2])]);
    let ctx = Arc::new(TestMarkContext::new(g));
    let pool = ZMarkCoordinator::new(ctx.clone(), 2);

    pool.begin_cycle();
    pool.push_roots(&[1]);
    let report = pool.mark_to_completion(RESTART_BUDGET);
    pool.end_cycle();

    assert!(!report.budget_exhausted);
    assert_eq!(ctx.marked_sorted(), vec![1, 2, 3, 4]);
    // Each node scanned at most a bounded number of times: the mark bit,
    // not the graph shape, is what bounds the work.
    assert_eq!(
        report.stats.objects_scanned, 4,
        "try_mark must gate re-entry, or a cycle re-scans forever"
    );
    pool.shutdown();
}

/// zr zs s1/mc (`CRATONVM_ZGC_MARK_CPU_ACCOUNTING`): a worker that spends
/// CPU inside a cycle charges it to that cycle's `worker_cpu_ns`, and the
/// charge is in the stats the driver reads at the fixed point (the
/// snapshot `mark_to_completion` returns). The kill arm charges nothing.
/// Positive control: the root's visit BURNS ~80 ms of CPU (a spin, not a
/// sleep -- a sleeping worker costs no CPU), above the coarsest thread
/// clock this runs on (Windows' scheduler quantum, ~16 ms).
#[test]
fn zs1mc_the_pool_charges_worker_cpu_to_the_cycle_and_the_kill_arm_nothing() {
    let (g, expected) = binary_tree(10);
    let run = |on: bool| {
        let ctx = Arc::new(
            TestMarkContext::new(g.clone()).with_visit_hook(Box::new(|addr| {
                if addr == 1 {
                    // zs s2/mc2: spin until the THREAD's clock has moved
                    // 80 ms (bounded by 5 s of wall time). An 80 ms WALL
                    // spin got under 20 ms of CPU when the parallel test
                    // runner shared a loaded box, and failed the assert
                    // below without anything being wrong.
                    let t = std::time::Instant::now();
                    let c0 = thread_cpu_nanos();
                    while t.elapsed() < std::time::Duration::from_secs(5) {
                        let spent = match (c0, thread_cpu_nanos()) {
                            (Some(a), Some(b)) => b.saturating_sub(a) >= 80_000_000,
                            _ => t.elapsed() >= std::time::Duration::from_millis(80),
                        };
                        if spent {
                            break;
                        }
                        std::hint::spin_loop();
                    }
                }
            })),
        );
        let pool = ZMarkCoordinator::new(ctx.clone(), 2);
        pool.set_cpu_accounting(on);
        pool.begin_cycle();
        pool.push_roots(&[1]);
        let r = pool.mark_to_completion(RESTART_BUDGET);
        pool.end_cycle();
        let marked = ctx.marked_sorted();
        pool.shutdown();
        (r, marked)
    };
    let (on, on_set) = run(true);
    let (off, off_set) = run(false);
    assert_eq!(on_set, expected, "accounting on: mark set");
    assert_eq!(off_set, expected, "accounting off: mark set");
    assert_eq!(off.stats.worker_cpu_ns, 0, "the kill arm reads no clock");
    if thread_cpu_nanos().is_some() {
        assert!(
            on.stats.worker_cpu_ns >= 20_000_000,
            "an 80 ms spin inside the cycle must be charged to it, got {} ns",
            on.stats.worker_cpu_ns
        );
    }
}

/// zs s2/mc2 (`CRATONVM_ZGC_MARK_DIRECT_SCAN`, `CRATONVM_ZGC_MARK_PREFETCH_RUNS`):
/// both arms of the direct scan (the trait's default body here) and every
/// run-prefetch distance mark the same set with the same scan count. The
/// run counter is the POSITIVE CONTROL: it fires on the wide fan-out (64
/// and 16 children per scan) and never on a binary tree, whose nodes claim
/// two children -- the shape the retired `PREFETCH_NEXT` lost 12 % on.
#[test]
fn zs2mc2_direct_scan_and_run_prefetch_mark_the_same_set() {
    for (name, (g, expected)) in [("tree", binary_tree(12)), ("wide", wide_graph())] {
        for direct in [true, false] {
            for runs in [0usize, 1, 4, Z_MARK_PREFETCH_RUN_MAX_DISTANCE] {
                let ctx = Arc::new(TestMarkContext::new(g.clone()));
                let pool = ZMarkCoordinator::new(ctx.clone(), 2);
                pool.set_direct_scan(direct);
                pool.set_prefetch_runs(runs);
                pool.begin_cycle();
                pool.push_roots(&[1]);
                let r = pool.mark_to_completion(RESTART_BUDGET);
                pool.end_cycle();
                let arm = format!("{name} direct={direct} runs={runs}");
                assert!(r.mark_set_complete(), "{arm}");
                let mut want = expected.clone();
                want.sort_unstable();
                assert_eq!(ctx.marked_sorted(), want, "{arm}");
                assert_eq!(r.stats.objects_scanned, want.len() as u64, "{arm}");
                assert_eq!(r.stats.off_heap_children, 0, "{arm}");
                if runs == 0 || name == "tree" {
                    assert_eq!(r.stats.run_prefetches, 0, "{arm}");
                } else {
                    assert!(r.stats.run_prefetches > 0, "{arm}: {:?}", r.stats);
                }
                // The default body keeps no memo.
                assert_eq!(r.stats.layout_memo_hits, 0, "{arm}");
                pool.shutdown();
            }
        }
    }
}


/// zs s2/mc2: a worker that panics inside `visit_refs` stops the pool, so
/// the cycle ends `stopped_early` (the caller then marks serially) instead
/// of waiting forever for a fixed point the dead worker can never offer.
/// Before the fix this test hung (the terminator still counted the dead
/// worker active); it runs the cycle on a helper thread and fails after
/// 20 s instead of hanging the suite.
#[test]
fn zs2mc2_a_panicking_worker_stops_the_pool_instead_of_hanging_the_cycle() {
    let (g, _) = binary_tree(10);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let ctx = Arc::new(TestMarkContext::new(g).with_visit_hook(Box::new(|addr| {
            if addr == 2 {
                panic!("zs2mc2 test: injected visit_refs panic");
            }
        })));
        let pool = ZMarkCoordinator::new(ctx, 2);
        pool.begin_cycle();
        pool.push_roots(&[1]);
        let r = pool.mark_to_completion(RESTART_BUDGET);
        pool.end_cycle();
        let stopped = pool.is_stopped();
        let _ = tx.send((r.stopped_early, r.mark_set_complete(), stopped));
        pool.shutdown();
    });
    let (stopped_early, complete, stopped) = rx
        .recv_timeout(std::time::Duration::from_secs(20))
        .expect("the cycle must end, not wait forever on a dead worker");
    assert!(stopped_early, "reported stopped_early");
    assert!(!complete, "never certified");
    assert!(
        stopped,
        "the pool reports itself stopped (a cache replaces it)"
    );
}


// -- multi worker determinism -------------------------------------------

/// A wide graph: 1 root -> 64 children -> 16 grandchildren each, plus a
/// detached component that must stay unmarked.
fn wide_graph() -> (FxHashMap<u64, Vec<u64>>, Vec<u64>) {
    let mut g: FxHashMap<u64, Vec<u64>> = FxHashMap::default();
    let mut expected: Vec<u64> = vec![1];
    let mut children = Vec::new();
    for c in 0..64u64 {
        let cid = 1000 + c;
        children.push(cid);
        expected.push(cid);
        let mut grand = Vec::new();
        for k in 0..16u64 {
            let gid = 100_000 + c * 100 + k;
            grand.push(gid);
            expected.push(gid);
            g.insert(gid, Vec::new());
        }
        g.insert(cid, grand);
    }
    g.insert(1, children);
    // Detached component.
    for d in 0..32u64 {
        g.insert(900_000 + d, vec![900_000 + ((d + 1) % 32)]);
    }
    (g, sorted(expected))
}

#[test]
fn four_workers_mark_exactly_the_same_set_as_one() {
    let (g, expected) = wide_graph();

    let ctx1 = Arc::new(TestMarkContext::new(g.clone()));
    let pool1 = ZMarkCoordinator::new(ctx1.clone(), 1);
    pool1.begin_cycle();
    pool1.push_roots(&[1]);
    let r1 = pool1.mark_to_completion(RESTART_BUDGET);
    pool1.end_cycle();
    let single = ctx1.marked_sorted();
    pool1.shutdown();

    let ctx4 = Arc::new(TestMarkContext::new(g));
    let pool4 = ZMarkCoordinator::new(ctx4.clone(), 4);
    pool4.begin_cycle();
    pool4.push_roots(&[1]);
    let r4 = pool4.mark_to_completion(RESTART_BUDGET);
    pool4.end_cycle();
    let multi = ctx4.marked_sorted();
    pool4.shutdown();

    assert!(!r1.budget_exhausted && !r4.budget_exhausted);
    assert_eq!(single, expected, "single-worker result");
    assert_eq!(
        multi, single,
        "N=4 must produce the identical mark SET; only the order may differ"
    );
    // Every node is scanned exactly once regardless of worker count —
    // that is the `try_mark` contract, and it is the thing that would
    // break first if the local/stripe hand-off ever duplicated work.
    assert_eq!(r1.stats.objects_scanned, expected.len() as u64);
    assert_eq!(r4.stats.objects_scanned, expected.len() as u64);
}

/// A complete binary tree of `2^depth - 1` nodes, keyed heap-style
/// (children of `i` are `2i` and `2i + 1`), rooted at `1`.
fn binary_tree(depth: u32) -> (FxHashMap<u64, Vec<u64>>, Vec<u64>) {
    let n = (1u64 << depth) - 1;
    let mut g = FxHashMap::default();
    for i in 1..=n {
        let kids: Vec<u64> = [2 * i, 2 * i + 1].into_iter().filter(|k| *k <= n).collect();
        g.insert(i, kids);
    }
    (g, (1..=n).collect())
}

/// zr z2/cc: the deep-narrow starvation and its valve. A binary tree's DFS
/// stack never reaches `Z_MARK_LOCAL_SHARE_MIN` (32) at depth 16, so with
/// the idle spill OFF the pool publishes nothing but the root -- one worker
/// marks the whole tree (the positive control: `published_objects == 1`). With it
/// ON, the parked peers get work (`idle_spills > 0`) and other workers
/// scan part of the tree. Both arms mark the identical set exactly once.
#[test]
fn zr2cc_a_parked_peer_gets_work_from_a_deep_narrow_walk() {
    let (g, expected) = binary_tree(16);
    let run = |spill: bool| {
        // Hold the root's visit for a moment so the three peers have
        // certainly searched, found nothing, and parked before the walk
        // starts: the spill is about PARKED peers.
        let ctx = Arc::new(
            TestMarkContext::new(g.clone()).with_visit_hook(Box::new(|addr| {
                if addr == 1 {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
            })),
        );
        let pool = ZMarkCoordinator::new(ctx.clone(), 4);
        pool.set_idle_spill(spill);
        pool.begin_cycle();
        pool.push_roots(&[1]);
        let r = pool.mark_to_completion(RESTART_BUDGET);
        pool.end_cycle();
        let marked = ctx.marked_sorted();
        pool.shutdown();
        (r, marked)
    };

    let (off, off_set) = run(false);
    assert_eq!(off_set, expected, "spill off: mark set");
    assert_eq!(off.stats.objects_scanned, expected.len() as u64);
    assert_eq!(off.stats.idle_spills, 0, "the kill arm must never spill");
    // The one publish is `push_roots` distributing the root itself.
    assert_eq!(
        (off.stats.stripe_publishes, off.stats.published_objects),
        (1, 1),
        "positive control: without the idle spill a depth-16 DFS never opens \
         either fixed valve, so nothing but the root is ever shared"
    );

    let (on, on_set) = run(true);
    assert_eq!(on_set, expected, "spill on: identical mark set");
    assert_eq!(
        on.stats.objects_scanned,
        expected.len() as u64,
        "exactly-once scan survives the spill"
    );
    assert!(
        on.stats.idle_spills > 0,
        "parked peers must have been handed work: {:?}",
        on.stats
    );
    assert!(
        on.stats.steals_succeeded + on.stats.own_stripe_refills > 0,
        "a spill nobody took is not sharing: {:?}",
        on.stats
    );
}

/// t1 wave 2 lane Z: a `Full` flush is in flight from inside the buffer
/// lock until its guard drops, the epoch counts every flush begun, and
/// `wait_for_flushes` answers by the in-flight count (bounded).
#[test]
fn t1z_satb_full_flush_is_in_flight_until_its_guard_drops() {
    let reg = ZSatbThreadBuffers::new();
    let mut out = Vec::new();
    assert_eq!(reg.record(0x10, 2, &mut out), ZSatbRecord::Buffered);
    assert_eq!((reg.flushes_in_flight(), reg.flush_epoch()), (0, 0));
    assert_eq!(reg.record(0x18, 2, &mut out), ZSatbRecord::Full);
    let guard = reg.flush_guard();
    assert_eq!(out, vec![0x10, 0x18]);
    assert_eq!((reg.flushes_in_flight(), reg.flush_epoch()), (1, 1));
    let mut drained = Vec::new();
    assert_eq!(reg.drain_all_into(&mut drained), 0, "the batch left the buffer");
    assert!(
        !reg.wait_for_flushes(Duration::from_millis(5)),
        "an unpublished batch holds the wait (bounded)"
    );
    // Published on another thread while a waiter waits.
    let waiter = {
        let reg = Arc::clone(&reg);
        std::thread::spawn(move || reg.wait_for_flushes(Duration::from_secs(30)))
    };
    drop(guard);
    assert!(waiter.join().unwrap(), "the publish releases the wait");
    assert_eq!((reg.flushes_in_flight(), reg.flush_epoch()), (0, 1));
    assert!(reg.wait_for_flushes(Duration::ZERO), "nothing in flight: one load");
}

/// zr z2/cc: per-thread SATB buffers. A thread's entries stay private
/// until the buffer fills or the registry is drained; a thread that EXITED
/// mid-phase still has its entries drained (the registry owns the buffer);
/// two registries on one thread never share a buffer.
#[test]
fn zr2cc_satb_thread_buffers_drain_every_thread_including_dead_ones() {
    let reg = ZSatbThreadBuffers::new();
    let mut out = Vec::new();
    assert_eq!(reg.record(0x1000, 4, &mut out), ZSatbRecord::Buffered);
    assert_eq!(reg.record(0x1008, 4, &mut out), ZSatbRecord::Buffered);
    assert!(out.is_empty(), "nothing published below capacity");
    assert_eq!(reg.pending_total(), 2);

    // A second heap on the same thread: its own buffer.
    let other = ZSatbThreadBuffers::new();
    assert_eq!(other.record(0x9000, 4, &mut out), ZSatbRecord::Buffered);
    assert_eq!(reg.pending_total(), 2);
    assert_eq!(other.pending_total(), 1);

    // A thread that records and exits before the drain.
    let r2 = Arc::clone(&reg);
    std::thread::spawn(move || {
        let mut o = Vec::new();
        assert_eq!(r2.record(0x2000, 4, &mut o), ZSatbRecord::Buffered);
    })
    .join()
    .unwrap();
    assert_eq!(reg.registered(), 2);
    assert_eq!(reg.pending_total(), 3);

    // Capacity: the fourth entry moves the buffer into `out`.
    assert_eq!(reg.record(0x1010, 4, &mut out), ZSatbRecord::Buffered);
    assert_eq!(reg.record(0x1018, 4, &mut out), ZSatbRecord::Full);
    assert_eq!(out, vec![0x1000, 0x1008, 0x1010, 0x1018]);
    assert_eq!(
        reg.pending_total(),
        1,
        "only the dead thread's entry is left"
    );

    let mut drained = Vec::new();
    assert_eq!(reg.drain_all_into(&mut drained), 1);
    assert_eq!(drained, vec![0x2000], "the exited thread's entry survives");
    assert_eq!(
        reg.registered(),
        1,
        "the dead thread's buffer is pruned after its drain"
    );

    // A dropped registry: the thread's next record for a NEW registry
    // prunes the dead entry and never writes through the old one.
    drop(other);
    let third = ZSatbThreadBuffers::new();
    assert_eq!(third.record(0x3000, 4, &mut out), ZSatbRecord::Buffered);
    assert_eq!(third.pending_total(), 1);
    reg.clear_all();
    assert_eq!(reg.pending_total(), 0);
}

/// zr z2/cc: once a cycle's fixed point is declared and `end_cycle` has
/// run, NO worker still holds the bound context. The heap binds ITSELF as
/// the context, so a worker clone surviving the fixed point is a heap that
/// outlives its last reference -- and drops on a pool worker, whose pool
/// `Drop` joins that very thread. Deterministic with the fix (every worker
/// releases the context before it offers termination); before it, the
/// count was 2 whenever the last worker out had not yet been rescheduled.
#[test]
fn zr2cc_no_worker_holds_the_cycle_context_after_end_cycle() {
    let (g, _) = binary_tree(10);
    let inert: Arc<dyn ZMarkContext> = Arc::new(ZInertMarkContext);
    let pool = ZMarkCoordinator::new(inert, 4);
    for round in 0..200 {
        let ctx: Arc<dyn ZMarkContext> = Arc::new(TestMarkContext::new(g.clone()));
        pool.begin_cycle_with(Arc::clone(&ctx));
        pool.push_roots(&[1]);
        let r = pool.mark_to_completion(RESTART_BUDGET);
        assert!(r.mark_set_complete());
        pool.end_cycle();
        assert_eq!(
            Arc::strong_count(&ctx),
            1,
            "round {round}: a mark worker still holds the cycle's context after end_cycle"
        );
    }
    pool.shutdown();
}

#[test]
fn zr2cc_the_idle_waiter_count_returns_to_zero_after_a_cycle() {
    let (g, _) = binary_tree(12);
    let ctx = Arc::new(TestMarkContext::new(g));
    let pool = ZMarkCoordinator::new(ctx, 4);
    for _ in 0..3 {
        pool.begin_cycle();
        pool.push_roots(&[1]);
        let r = pool.mark_to_completion(RESTART_BUDGET);
        assert!(r.mark_set_complete());
        pool.end_cycle();
        // Every exit of the park loop decrements; a leak would leave the
        // hint permanently non-zero and spill on every 16th step forever.
        let mut spins = 0;
        while pool.shared().terminator().idle_waiters_hint() != 0 && spins < 1000 {
            std::thread::sleep(std::time::Duration::from_millis(1));
            spins += 1;
        }
        assert_eq!(pool.shared().terminator().idle_waiters_hint(), 0);
    }
    pool.shutdown();
}


// -- work stealing ------------------------------------------------------

#[test]
fn work_stealing_fires_on_a_deliberately_unbalanced_distribution() {
    // 4 workers -> 16 stripes. Worker `i` owns stripe `i & 15`, i.e.
    // stripes 0..=3. Seeding stripe 9 means NO worker owns the work, so
    // the very first acquisition by any worker is by definition a steal.
    // That makes this test deterministic rather than a race on timing.
    let n_workers = 4usize;
    let stripes = stripe_count_for(n_workers);
    assert!(
        stripes > n_workers,
        "this test needs an unowned stripe to exist"
    );
    let unowned_stripe = n_workers + 5;
    assert!(unowned_stripe < stripes);

    let mut g: FxHashMap<u64, Vec<u64>> = FxHashMap::default();
    let mut roots = Vec::new();
    let mut expected = Vec::new();
    for i in 0..2048u64 {
        let root = 10_000 + i;
        let child = 500_000 + i;
        g.insert(root, vec![child]);
        g.insert(child, Vec::new());
        roots.push(root);
        expected.push(root);
        expected.push(child);
    }

    let ctx = Arc::new(TestMarkContext::new(g));
    let pool = ZMarkCoordinator::new(ctx.clone(), n_workers);
    assert_eq!(pool.stripe_count(), stripes);

    pool.begin_cycle();
    assert_eq!(pool.dbg_push_roots_to_stripe(unowned_stripe, &roots), 2048);
    let report = pool.mark_to_completion(RESTART_BUDGET);
    pool.end_cycle();

    assert!(!report.budget_exhausted);
    assert_eq!(ctx.marked_sorted(), sorted(expected));
    assert!(
        report.stats.steals_succeeded > 0,
        "no steal fired despite every root living in an unowned stripe \
         (attempts={}, own_refills={})",
        report.stats.steal_attempts,
        report.stats.own_stripe_refills
    );
    assert!(
        report.stats.stolen_objects > 0,
        "a successful steal must move at least one object"
    );
    pool.shutdown();
}

// -- termination --------------------------------------------------------

/// The race the [`ZMarkTerminator`] docs describe, constructed on
/// purpose.
///
/// Two workers. One root is a leaf (`FAST`), so whichever worker takes it
/// finishes instantly and offers itself for termination. The other root
/// (`SLOW`) blocks *inside* `visit_refs` — i.e. its 300 children exist
/// nowhere the termination probe could see them — until the test thread
/// observes that a worker has actually gone idle.
///
/// With a naive "all queues are empty, therefore we are done" test, the
/// idle worker declares completion here and the 300 children are never
/// marked. With the active-worker handshake, it cannot: the worker inside
/// `visit_refs` is still counted, so the idle worker is not the last one
/// out and must park.
///
/// The assertion is set equality, never elapsed time.
#[test]
fn termination_does_not_fire_while_a_worker_holds_work_in_flight() {
    const FAST: u64 = 1;
    const SLOW: u64 = 2;

    let mut g: FxHashMap<u64, Vec<u64>> = FxHashMap::default();
    g.insert(FAST, Vec::new());
    let mut slow_children = Vec::new();
    let mut expected = vec![FAST, SLOW];
    for i in 0..300u64 {
        let c = 700_000 + i;
        slow_children.push(c);
        expected.push(c);
        g.insert(c, Vec::new());
    }
    g.insert(SLOW, slow_children);

    // The test thread flips this once it has seen a worker go idle.
    let release = Arc::new(StdAtomicBool::new(false));
    let entered_slow = Arc::new(AtomicUsize::new(0));

    let hook_release = Arc::clone(&release);
    let hook_entered = Arc::clone(&entered_slow);
    let ctx = Arc::new(
        TestMarkContext::new(g).with_visit_hook(Box::new(move |addr: u64| {
            if addr != SLOW {
                return;
            }
            hook_entered.fetch_add(1, Ordering::SeqCst);
            // Bounded spin: if the interleaving does not materialise on
            // this run (e.g. a single-CPU CI box), proceed anyway. The
            // test still checks correctness; it just does not exercise
            // the race that time. Never assert on how long this took.
            let mut spins: u64 = 0;
            while !hook_release.load(Ordering::Acquire) && spins < 20_000_000 {
                spins += 1;
                std::thread::yield_now();
            }
        })),
    );

    let pool = ZMarkCoordinator::new(ctx.clone(), 2);
    pool.begin_cycle();
    // Two stripes, one root each, so both workers find work immediately.
    pool.dbg_push_roots_to_stripe(0, &[FAST]);
    pool.dbg_push_roots_to_stripe(1, &[SLOW]);
    pool.start_marking();

    // Wait until a worker is actually inside SLOW's visit_refs AND the
    // other has offered itself for termination. Bounded, and the loop
    // asserts nothing about duration.
    let mut spins: u64 = 0;
    while spins < 20_000_000 {
        if entered_slow.load(Ordering::SeqCst) > 0 && pool.active_workers() < 2 {
            break;
        }
        spins += 1;
        std::thread::yield_now();
    }
    let raced = entered_slow.load(Ordering::SeqCst) > 0 && pool.active_workers() < 2;
    release.store(true, Ordering::Release);

    pool.wait_for_fixed_point();
    assert_eq!(pool.try_end_mark(), ZMarkEndResult::Complete);
    pool.end_cycle();

    assert_eq!(
        ctx.marked_sorted(),
        sorted(expected),
        "a worker that went idle while another held children in flight \
         terminated the cycle early (raced={raced})"
    );
    pool.shutdown();
}

#[test]
fn a_fixed_point_with_an_empty_ingress_completes_in_one_pass() {
    let g = graph(&[(1, &[2, 3]), (2, &[]), (3, &[])]);
    let ctx = Arc::new(TestMarkContext::new(g));
    let pool = ZMarkCoordinator::new(ctx.clone(), 3);
    pool.begin_cycle();
    pool.push_roots(&[1]);
    let report = pool.mark_to_completion(RESTART_BUDGET);
    pool.end_cycle();
    assert_eq!(report.restarts, 0);
    assert_eq!(report.passes, 1);
    assert_eq!(ctx.marked_sorted(), vec![1, 2, 3]);
    pool.shutdown();
}

// -- mutator marking ----------------------------------------------------

/// A mutator mark that arrives after the workers reached a fixed point
/// must not be lost: the mark-end probe has to see it and restart.
///
/// This is the difference between ZGC and SATB stated as a test. Under
/// SATB the remark pause is final by construction; here it is a decision
/// point that can send everybody back around.
#[test]
fn a_mutator_mark_after_the_fixed_point_forces_a_restart_and_is_not_lost() {
    let g = graph(&[
        (1, &[2]),
        (2, &[]),
        // Reachable only via the simulated load barrier.
        (50, &[51, 52]),
        (51, &[53]),
        (52, &[]),
        (53, &[]),
    ]);
    let ctx = Arc::new(TestMarkContext::new(g));
    let pool = ZMarkCoordinator::new(ctx.clone(), 2);
    let handle = pool.handle();

    pool.begin_cycle();
    assert!(handle.is_marking());
    pool.push_roots(&[1]);
    pool.start_marking();
    pool.wait_for_fixed_point();
    assert_eq!(ctx.marked_sorted(), vec![1, 2]);

    // A mutator loads a stale reference to 50 *after* the workers
    // converged. In OpenJDK this is the load-barrier slow path.
    assert!(handle.mark_live(0, 50));
    assert!(
        !handle.mark_live(0, 50),
        "a second load of the same reference must not re-enqueue it"
    );

    // Mark end, mutators stopped (we are the only one and we are here).
    assert_eq!(
        pool.try_end_mark(),
        ZMarkEndResult::Restart,
        "the mark-end probe must see the mutator's mark"
    );

    // Run the concurrent phase again.
    pool.start_marking();
    pool.wait_for_fixed_point();
    assert_eq!(pool.try_end_mark(), ZMarkEndResult::Complete);
    pool.end_cycle();

    assert_eq!(
        ctx.marked_sorted(),
        vec![1, 2, 50, 51, 52, 53],
        "the mutator's object AND its transitive closure must be marked"
    );
    assert!(pool.stats().mark_end_restarts.load(Ordering::Relaxed) >= 1);
    pool.shutdown();
}

// -----------------------------------------------------------------------
// N3 (2026-08-07): the barrier's offset domain meets this module's
// machine-address domain, and the meeting point is `mark_live_offset`.
// -----------------------------------------------------------------------

/// The origin used by the offset-domain tests below. Node ids in
/// [`TestMarkContext`] then stand for machine addresses, and the barrier's
/// offset for node `n` is `n - HEAP_BASE` — the same relation
/// `ZRelocate::offset_of_address` implements over a real reservation.
const HEAP_BASE: u64 = 0x4000;

/// N3, POSITIVE half. A bare heap offset — what
/// `barrier::load_barrier_slow` actually passes to
/// `ZBarrierContext::mark_live` — reaches the marker when it goes through
/// [`ZMarkHandle::mark_live_offset`], and drags its transitive closure with
/// it through the restart loop.
#[test]
fn a_barrier_offset_marks_its_object_through_mark_live_offset() {
    let g = graph(&[
        (HEAP_BASE + 1, &[HEAP_BASE + 2]),
        (HEAP_BASE + 2, &[]),
        // Reachable only through the simulated load barrier.
        (HEAP_BASE + 50, &[HEAP_BASE + 51]),
        (HEAP_BASE + 51, &[]),
    ]);
    let ctx = Arc::new(TestMarkContext::new(g).with_heap_base(HEAP_BASE));
    let pool = ZMarkCoordinator::new(ctx.clone(), 2);
    let handle = pool.handle();
    assert_eq!(handle.heap_base(), Some(HEAP_BASE));

    pool.begin_cycle();
    pool.push_roots(&[HEAP_BASE + 1]);
    pool.start_marking();
    pool.wait_for_fixed_point();

    // This is the value the barrier holds: `raw & Z_OFFSET_MASK`, with the
    // heap base already subtracted by whoever wrote the slot. It is NOT an
    // address, and `is_in_heap` would refuse it as one.
    let barrier_offset: u64 = 50;
    assert!(is_bare_offset(barrier_offset, Z_OFFSET_MASK));
    assert!(
        handle.mark_live_offset(0, barrier_offset),
        "the seam must convert the offset to HEAP_BASE + 50 and mark it"
    );
    assert!(
        !handle.mark_live_offset(0, barrier_offset),
        "a second load of the same reference must not re-enqueue it"
    );

    assert_eq!(pool.try_end_mark(), ZMarkEndResult::Restart);
    pool.start_marking();
    pool.wait_for_fixed_point();
    assert_eq!(pool.try_end_mark(), ZMarkEndResult::Complete);
    pool.end_cycle();

    assert_eq!(
        ctx.marked_sorted(),
        vec![HEAP_BASE + 1, HEAP_BASE + 2, HEAP_BASE + 50, HEAP_BASE + 51],
        "the barrier's object AND its transitive closure must be marked"
    );
    let s = pool.stats().snapshot();
    assert_eq!(s.domain_refusals, 0);
    assert_eq!(s.off_heap_children, 0);
    pool.shutdown();
}

/// N3, THE DEFECT. This is the test that would have caught it, and it
/// asserts the *silence* rather than a crash.
///
/// Handing the same bare offset to [`ZMarkHandle::mark_live`] — the
/// machine-address entry point, which is what a naive
/// `ZBarrierContext::mark_live` adapter would have called — does not fail,
/// does not assert and does not log anything a reader would act on. It is
/// refused by [`ZMarkContext::is_in_heap`] exactly as an honest wild pointer
/// would be, counted as `off_heap_children`, and the object stays unmarked
/// and is swept. The two calls in this test differ only in which method
/// name the adapter reached for.
#[test]
fn the_same_offset_is_silently_discarded_by_the_machine_address_entry_point() {
    let g = graph(&[(HEAP_BASE + 50, &[])]);
    let ctx = Arc::new(TestMarkContext::new(g).with_heap_base(HEAP_BASE));
    let pool = ZMarkCoordinator::new(ctx.clone(), 1);
    let handle = pool.handle();
    pool.begin_cycle();

    let barrier_offset: u64 = 50;

    // The wrong entry point. No panic, no error log, no domain refusal —
    // just a `false` and a wild-pointer counter, which is what makes this
    // the most dangerous shape a seam bug can have.
    assert!(
        !handle.mark_live(0, barrier_offset),
        "an offset is not a registered object base, so is_in_heap refuses it"
    );
    let s = pool.stats().snapshot();
    assert_eq!(
        s.off_heap_children, 1,
        "the mark was discarded and counted as a wild pointer"
    );
    assert_eq!(
        s.domain_refusals, 0,
        "and nothing anywhere identified it as a DOMAIN error"
    );
    assert_eq!(ctx.marked_count(), 0, "the live object is now sweepable");

    // The right entry point, same value, same heap, same cycle.
    assert!(handle.mark_live_offset(0, barrier_offset));
    assert_eq!(ctx.marked_sorted(), vec![HEAP_BASE + 50]);

    pool.end_cycle();
    pool.shutdown();
}

/// N3, NEGATIVE half: a machine address handed to the *offset* entry point
/// is refused at the boundary, counted separately from a wild pointer, and
/// never reaches `try_mark`.
///
/// # Why `catch_unwind` and not `#[should_panic]`
///
/// The tripwire is a `tracing::error!` **plus** a `debug_assert!`, so it
/// aborts the caller where assertions are on and returns `false` where they
/// are not. A `#[should_panic]` test would therefore pass in debug and fail
/// in release — cfg-dependent test behaviour is a documented trap in this
/// repo, and it is the same trap that motivated making the check itself
/// survive `--release`. The counter is bumped *before* the assert, so
/// asserting on the counter means the same thing in both profiles.
#[test]
fn mark_live_offset_refuses_a_machine_address_and_counts_a_domain_refusal() {
    // ~47 bits, five bits past the 42-bit offset field: a legal Linux
    // reservation address, and invisible as a bug on the Windows dev host
    // where the reservation is placed low. Same value `barrier.rs`'s
    // `is_bare_offset_separates_a_heap_offset_from_a_machine_pointer` uses.
    const LINUX_SHAPED_HEAP_ADDRESS: u64 = 0x7f12_3456_7890;
    assert!(!is_bare_offset(LINUX_SHAPED_HEAP_ADDRESS, Z_OFFSET_MASK));

    let g = graph(&[(HEAP_BASE + 1, &[])]);
    let ctx = Arc::new(TestMarkContext::new(g).with_heap_base(HEAP_BASE));
    let pool = ZMarkCoordinator::new(ctx.clone(), 1);
    let handle = pool.handle();
    pool.begin_cycle();

    let attempted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        handle.mark_live_offset(0, LINUX_SHAPED_HEAP_ADDRESS)
    }));
    if let Ok(marked) = attempted {
        assert!(!marked, "a machine address must not mark anything");
    }

    let s = pool.stats().snapshot();
    assert_eq!(
        s.domain_refusals, 1,
        "the refusal must be attributed to the DOMAIN"
    );
    assert_eq!(
        s.off_heap_children, 0,
        "and must NOT hide inside the wild-pointer counter"
    );
    assert_eq!(ctx.marked_count(), 0);

    pool.end_cycle();
    pool.shutdown();
}

/// N3, the "no base" arm: a context that does not participate in the offset
/// domain refuses instead of guessing a base.
///
/// `ZgcRealHeap` is exactly this context today — it stores raw pointers in
/// its slots and never colours them — so this is the behaviour a barrier
/// wired to it right now would get: loud and counted, not silent.
#[test]
fn mark_live_offset_refuses_a_context_with_no_heap_base() {
    let g = graph(&[(50, &[])]);
    // No `.with_heap_base(..)`: the trait default, `None`.
    let ctx = Arc::new(TestMarkContext::new(g));
    let pool = ZMarkCoordinator::new(ctx.clone(), 1);
    let handle = pool.handle();
    assert_eq!(handle.heap_base(), None);
    pool.begin_cycle();

    // 50 IS a valid node id here, so a context that silently assumed a base
    // of 0 would have marked it and looked entirely correct — which is why
    // the default must refuse rather than default to zero.
    let attempted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        handle.mark_live_offset(0, 50)
    }));
    if let Ok(marked) = attempted {
        assert!(!marked);
    }

    assert_eq!(pool.stats().domain_refusals.load(Ordering::Relaxed), 1);
    assert_eq!(
        ctx.marked_count(),
        0,
        "refusing beats guessing a base of 0 and marking by coincidence"
    );

    pool.end_cycle();
    pool.shutdown();
}

/// The buffered twin carries the same boundary. It matters more here than
/// on the unbuffered path: `mark_live_buffered` sets the mark bit before
/// the address reaches the ingress, so a wrong-domain value that got past
/// the boundary would be marked-and-unscanned rather than merely refused.
#[test]
fn mark_live_buffered_offset_converts_and_the_buffer_flushes_the_address() {
    let g = graph(&[(HEAP_BASE + 60, &[HEAP_BASE + 61]), (HEAP_BASE + 61, &[])]);
    let ctx = Arc::new(TestMarkContext::new(g).with_heap_base(HEAP_BASE));
    let pool = ZMarkCoordinator::new(ctx.clone(), 1);
    let handle = pool.handle();
    pool.begin_cycle();

    let mut buf = handle.new_buffer(0);
    assert!(handle.mark_live_buffered_offset(&mut buf, 60));
    assert_eq!(
        ctx.marked_sorted(),
        vec![HEAP_BASE + 60],
        "the mark bit is set at HEAP_BASE + 60, not at 60"
    );

    assert_eq!(handle.flush_buffer(&mut buf), 1);
    // Fold the ingress into the stripes deterministically rather than
    // racing a worker for it, then run the concurrent phase.
    assert_eq!(pool.try_end_mark(), ZMarkEndResult::Restart);
    pool.start_marking();
    pool.wait_for_fixed_point();
    assert_eq!(pool.try_end_mark(), ZMarkEndResult::Complete);
    pool.end_cycle();

    assert_eq!(
        ctx.marked_sorted(),
        vec![HEAP_BASE + 60, HEAP_BASE + 61],
        "the converted address must be scannable, not just markable"
    );
    assert_eq!(pool.stats().snapshot().domain_refusals, 0);
    pool.shutdown();
}

/// The same guarantee, but with the mutator marking *while* the workers
/// are draining, from a different thread.
#[test]
fn concurrent_mutator_marks_are_folded_in_by_the_restart_loop() {
    let mut g: FxHashMap<u64, Vec<u64>> = FxHashMap::default();
    let mut expected: Vec<u64> = Vec::new();
    // A chain the workers trace from the roots. Node ids start at **1**:
    // address `0` is this engine's null everywhere — `push_roots` skips
    // it, `ZMarkHandle::mark_live` returns `false` for it, and `drain`'s
    // child filter drops it — so a graph keyed on 0 silently loses its own
    // root and marks nothing at all.
    for i in 1..=500u64 {
        g.insert(i, if i < 500 { vec![i + 1] } else { Vec::new() });
        expected.push(i);
    }
    // A disjoint component only the "mutator" knows about.
    let mut mutator_seeds = Vec::new();
    for i in 0..64u64 {
        let a = 800_000 + i * 2;
        let b = 800_001 + i * 2;
        g.insert(a, vec![b]);
        g.insert(b, Vec::new());
        mutator_seeds.push(a);
        expected.push(a);
        expected.push(b);
    }

    let ctx = Arc::new(TestMarkContext::new(g));
    let pool = ZMarkCoordinator::new(ctx.clone(), 4);
    let handle = pool.handle();

    pool.begin_cycle();
    assert_eq!(
        pool.push_roots(&[1]),
        1,
        "the root must be accepted; a silently dropped root would make the \
         mark-set assertion below fail for a reason that has nothing to do \
         with the mutator fold this test is about"
    );
    pool.start_marking();

    // "Mutator" thread: hand addresses over while the pool is running.
    let mutator_handle = handle.clone();
    let mutator = std::thread::spawn(move || {
        let mut buf = ZMarkMutatorBuffer::new(7);
        for seed in mutator_seeds {
            mutator_handle.mark_live_buffered(&mut buf, seed);
            std::thread::yield_now();
        }
        // Safepoint contract: every per-thread buffer must be flushed
        // before the mark-end probe runs, exactly as G1's remark flushes
        // the thread-local SATB buffer.
        mutator_handle.flush_buffer(&mut buf);
    });

    pool.wait_for_fixed_point();
    mutator.join().expect("mutator thread");

    // Now drive the restart loop to completion.
    loop {
        match pool.try_end_mark() {
            ZMarkEndResult::Complete => break,
            ZMarkEndResult::Restart => {
                pool.start_marking();
                pool.wait_for_fixed_point();
            }
        }
    }
    pool.end_cycle();

    assert_eq!(
        ctx.marked_sorted(),
        sorted(expected),
        "mid-cycle mutator marks (and their closures) must survive"
    );
    pool.shutdown();
}

/// An unflushed mutator buffer is invisible to the mark-end probe. This
/// is a *contract* test, not a bug report: it pins the reason the
/// safepoint must flush every buffer, so that anyone who removes the
/// flush from the safepoint path fails here instead of in production.
#[test]
fn an_unflushed_mutator_buffer_is_invisible_to_the_mark_end_probe() {
    let g = graph(&[(1, &[]), (77, &[78]), (78, &[])]);
    let ctx = Arc::new(TestMarkContext::new(g));
    let pool = ZMarkCoordinator::new(ctx.clone(), 1);
    let handle = pool.handle();

    pool.begin_cycle();
    pool.push_roots(&[1]);
    pool.start_marking();
    pool.wait_for_fixed_point();

    let mut buf = ZMarkMutatorBuffer::new(3);
    assert!(handle.mark_live_buffered(&mut buf, 77));
    assert_eq!(buf.len(), 1, "well below the flush threshold");

    assert_eq!(
        pool.try_end_mark(),
        ZMarkEndResult::Complete,
        "an unflushed buffer cannot be seen — this is why the safepoint must flush"
    );

    // Flushing makes it visible, and the restart picks up the closure.
    handle.flush_buffer(&mut buf);
    assert_eq!(pool.try_end_mark(), ZMarkEndResult::Restart);
    pool.start_marking();
    pool.wait_for_fixed_point();
    assert_eq!(pool.try_end_mark(), ZMarkEndResult::Complete);
    pool.end_cycle();

    assert_eq!(ctx.marked_sorted(), vec![1, 77, 78]);
    pool.shutdown();
}

// -- thread death (FINDING A) -------------------------------------------

/// FINDING A (2026-08-07). A mutator that **exits** holding a non-empty
/// buffer must not strand the objects it already marked.
///
/// `mark_live_buffered` sets the mark bit before the address reaches the
/// ingress. So an entry left in a dying thread's buffer leaves its object
/// *marked* — no future `try_mark` can rediscover it — and in *no queue* —
/// no worker will ever scan it. Its out-edges are never traced and
/// everything reachable only through it is swept while live: the exact
/// use-after-free the termination protocol exists to prevent, arriving by
/// the back door.
///
/// `88 -> 89 -> 90` is that "everything reachable only through it". The
/// thread below exits without flushing, so only the attached buffer's
/// `Drop` can save them. Against the pre-fix code — where
/// [`ZMarkMutatorBuffer`] had no link to the pool and no `Drop` — this
/// fails at the `Restart` assertion.
///
/// Nothing here is timed; the assertions are on the mark **set** and on the
/// mark-end verdict.
#[test]
fn a_dying_thread_with_an_attached_buffer_does_not_strand_a_marked_object() {
    let g = graph(&[(1, &[]), (88, &[89]), (89, &[90]), (90, &[])]);
    let ctx = Arc::new(TestMarkContext::new(g));
    let pool = ZMarkCoordinator::new(ctx.clone(), 2);
    let handle = pool.handle();

    pool.begin_cycle();
    pool.push_roots(&[1]);
    pool.start_marking();
    pool.wait_for_fixed_point();
    assert_eq!(ctx.marked_sorted(), vec![1]);

    let mutator_handle = handle.clone();
    std::thread::spawn(move || {
        let mut buf = mutator_handle.new_buffer(5);
        assert!(
            buf.is_attached(),
            "new_buffer must produce an attached buffer"
        );
        assert!(mutator_handle.mark_live_buffered(&mut buf, 88));
        assert_eq!(buf.len(), 1, "well below the auto-flush threshold");
        // Deliberately NO flush_buffer: the thread ends here and `buf` is
        // dropped by ordinary thread teardown. That drop is the fix.
    })
    .join()
    .expect("mutator thread");

    assert_eq!(
        pool.try_end_mark(),
        ZMarkEndResult::Restart,
        "the dying thread's buffered entry was lost: 88 is already marked, \
         so nothing can rediscover it, and 89/90 are reachable only through \
         it — they would be swept while live"
    );
    pool.start_marking();
    pool.wait_for_fixed_point();
    assert_eq!(pool.try_end_mark(), ZMarkEndResult::Complete);
    pool.end_cycle();

    assert_eq!(
        ctx.marked_sorted(),
        vec![1, 88, 89, 90],
        "the stranded object's whole closure must be traced"
    );
    pool.shutdown();
}

/// The negative counterpart, and the reason
/// [`ZMarkHandle::new_buffer`] exists: a **detached**
/// [`ZMarkMutatorBuffer::new`] buffer has nowhere to flush to, so its `Drop`
/// can only log. This pins the damage shape as a named property rather than
/// a footnote, exactly as
/// `an_unflushed_mutator_buffer_is_invisible_to_the_mark_end_probe` does for
/// the live-thread case.
///
/// It is a **contract** test, not a bug report: anyone who makes `new`
/// attached-by-default, or who removes the distinction, fails here and is
/// forced to read why the distinction was drawn.
#[test]
fn a_detached_buffer_dropped_non_empty_leaves_a_marked_but_unscanned_object() {
    let g = graph(&[(1, &[]), (88, &[89]), (89, &[])]);
    let ctx = Arc::new(TestMarkContext::new(g));
    let pool = ZMarkCoordinator::new(ctx.clone(), 1);
    let handle = pool.handle();

    pool.begin_cycle();
    pool.push_roots(&[1]);
    pool.start_marking();
    pool.wait_for_fixed_point();

    let mutator_handle = handle.clone();
    std::thread::spawn(move || {
        let mut buf = ZMarkMutatorBuffer::new(5);
        assert!(
            !buf.is_attached(),
            "ZMarkMutatorBuffer::new is the detached form"
        );
        assert!(mutator_handle.mark_live_buffered(&mut buf, 88));
    })
    .join()
    .expect("mutator thread");

    assert_eq!(
        pool.try_end_mark(),
        ZMarkEndResult::Complete,
        "a detached buffer's entries reach nobody — this is the whole \
         reason ZMarkHandle::new_buffer exists"
    );
    pool.end_cycle();

    // And this is the damage, stated exactly: the object is marked (so no
    // later `try_mark` will ever hand it to a worker) and its child is not
    // marked at all (so it would be swept while live).
    assert!(
        ctx.is_marked(88),
        "the mark bit is set BEFORE the push — that is what makes a lost \
         buffer entry unrecoverable rather than merely late"
    );
    assert!(
        !ctx.is_marked(89),
        "89 is reachable only through the stranded 88; nothing traced it"
    );
    pool.shutdown();
}

/// The `is_marking()` gate is the **barrier's** obligation, not this
/// handle's — see [`ZMarkHandle::mark_live`] for the decision and the three
/// reasons. What the handle owes is visibility, and this pins it.
#[test]
fn a_mark_after_end_cycle_is_counted_late_and_cannot_leak_into_the_next_cycle() {
    let g = graph(&[(1, &[]), (44, &[])]);
    let ctx = Arc::new(TestMarkContext::new(g));
    let pool = ZMarkCoordinator::new(ctx.clone(), 1);
    let handle = pool.handle();

    pool.begin_cycle();
    pool.push_roots(&[1]);
    let report = pool.mark_to_completion(RESTART_BUDGET);
    assert!(!report.budget_exhausted);
    assert_eq!(
        report.stats.late_marks, 0,
        "nothing inside a cycle may be counted late"
    );
    pool.end_cycle();

    // A late call still marks: the handle does not gate, deliberately.
    assert!(!handle.is_marking());
    assert!(handle.mark_live(0, 44));
    assert_eq!(
        pool.stats().late_marks.load(Ordering::Relaxed),
        1,
        "a mark with no cycle in flight must be COUNTED, so a barrier that \
         gets its own gate wrong is visible in telemetry instead of \
         silently setting bits for a finished cycle"
    );

    // ...and the stale entry cannot survive into the next cycle, which is
    // why the (inherently racy) gate is not needed for correctness:
    // `begin_cycle` clears the ingress and the stripes outright, and the
    // stale mark bit belongs to a colour the next cycle flips away from.
    ctx.reset_marks();
    pool.begin_cycle();
    assert_eq!(
        pool.try_end_mark(),
        ZMarkEndResult::Complete,
        "begin_cycle must have discarded the late entry"
    );
    pool.end_cycle();
    pool.shutdown();
}

// -- termination abort wakeup (FINDING B) -------------------------------

/// Everything [`ZMarkTerminator::worker_idle`] needs, so the handshake can
/// be driven directly instead of raced for through a whole pool.
struct TerminatorRig {
    stripes: ZMarkStripeSet,
    ingress: ZMarkIngress,
    stats: ZMarkStats,
    stop: AtomicBool,
    term: ZMarkTerminator,
}

/// FINDING B (2026-08-07). When the last worker out finds work in the
/// termination probe it re-counts itself active and returns `Resume` — and
/// it must also **publish a wakeup**, or the other `n-1` workers stay
/// parked for the rest of the cycle and the pool silently runs
/// single-threaded.
///
/// # The interleaving, built rather than raced for
///
/// A parked worker waits for `work_generation` to move away from the value
/// it captured on the way in. The trap is that the bump for the *queued*
/// work may already have been **spent**: a worker publishes (bumping), and
/// a colleague whose steal sweep had already passed that stripe then parks
/// — at the new value. No further bump is pending, so its 5 ms poll re-reads
/// the identical number forever. It rejoins only once the resuming worker
/// publishes, which needs `local.len() >= 32` with its own stripe empty
/// (`maybe_share_early`) or `>= 256` (`publish_half`); on a deep, narrow
/// graph neither ever happens.
///
/// The rig below reproduces that state exactly: queue the work, spend its
/// bump, *then* let the colleague park.
///
/// # No wall clock anywhere
///
/// The primary assertion is on `work_generation`, which is deterministic.
/// The participation assertion that follows is on a recorded **outcome**,
/// not on elapsed time, and its spin budget only has to cover a condvar
/// wakeup — the 5 ms poll cannot substitute for the missing bump, which is
/// precisely why this test measures the bump and not the notification.
/// [`ZMarkStats::yields`] is deliberately not used: it is bumped at the
/// drain loop's head only, so a worker resumed and re-parked before
/// reaching that head yields without counting.
#[test]
fn a_termination_abort_publishes_a_wakeup_for_the_parked_workers() {
    const SPIN_BUDGET: u64 = 20_000_000;

    let rig = Arc::new(TerminatorRig {
        stripes: ZMarkStripeSet::new(8),
        ingress: ZMarkIngress::new(),
        stats: ZMarkStats::default(),
        stop: AtomicBool::new(false),
        term: ZMarkTerminator::new(2),
    });
    let cycle = rig.term.arm();
    assert_eq!(rig.term.active_workers(), 2);

    // Queue work and SPEND its generation bump.
    let mut work = vec![101u64, 102, 103];
    assert_eq!(rig.stripes.publish(0, &mut work), 3);
    rig.term.note_work_published();

    // The colleague now parks at that same, already-current generation.
    let outcome_slot: Arc<Mutex<Option<ZIdleOutcome>>> = Arc::new(Mutex::new(None));
    let t_rig = Arc::clone(&rig);
    let t_slot = Arc::clone(&outcome_slot);
    let colleague = std::thread::spawn(move || {
        let o = t_rig.term.worker_idle(
            cycle,
            &t_rig.stripes,
            &t_rig.ingress,
            &t_rig.stats,
            &t_rig.stop,
        );
        *t_slot.lock() = Some(o);
    });

    // Bounded, and asserts nothing about how long it took.
    let mut spins: u64 = 0;
    while rig.term.active_workers() != 1 && spins < SPIN_BUDGET {
        spins += 1;
        std::thread::yield_now();
    }
    assert_eq!(
        rig.term.active_workers(),
        1,
        "the colleague never reached the handshake; the rest of this test \
         would be vacuous"
    );

    let gen_before = rig.term.work_generation();

    // Be the last worker out: active 1 -> 0, the probe finds the queued
    // work, and the abort path runs.
    let mine = rig
        .term
        .worker_idle(cycle, &rig.stripes, &rig.ingress, &rig.stats, &rig.stop);
    assert_eq!(
        mine,
        ZIdleOutcome::Resume,
        "the probe must have found the work that is sitting in stripe 0"
    );
    assert_eq!(rig.stats.termination_aborts.load(Ordering::Relaxed), 1);
    assert_ne!(
        rig.term.work_generation(),
        gen_before,
        "FINDING B: the abort re-counted itself active but published no \
         wakeup. The colleague is parked at generation {gen_before} and its \
         5 ms poll re-reads that same value, so it can only rejoin once we \
         publish >= 32 or >= 256 entries — on a deep, narrow graph, never."
    );

    // Participation: the parked worker must actually rejoin the cycle.
    let mut spins: u64 = 0;
    while outcome_slot.lock().is_none() && spins < SPIN_BUDGET {
        spins += 1;
        std::thread::yield_now();
    }
    // Release it unconditionally so this test can never hang, whatever the
    // outcome was.
    rig.stop.store(true, Ordering::Release);
    rig.term.wake_all();
    colleague.join().expect("colleague thread");

    assert_eq!(
        *outcome_slot.lock(),
        Some(ZIdleOutcome::Resume),
        "the parked worker never rejoined; with 2 workers that is the whole \
         pool collapsing to one for the rest of the cycle"
    );
    assert_eq!(
        rig.term.active_workers(),
        2,
        "both workers must be counted active again after the abort"
    );
}

/// The abort's wakeup must not become a storm: a worker woken into an
/// already-emptied pool has to settle, not spin.
///
/// This is the failure the fix could plausibly have traded for, and this
/// tree has been bitten by a livelock-for-throughput trade before. The
/// structural reason it cannot happen is asserted here: the abort branch is
/// reachable **only** by the last worker out (`active` 1 -> 0), and it
/// immediately re-counts itself active, so `active` cannot reach 0 again —
/// and the branch cannot re-fire — until that worker has itself drained and
/// re-offered. Each abort therefore corresponds to work that really was
/// queued, and the count is bounded by real publishes.
#[test]
fn an_abort_wakeup_settles_instead_of_storming() {
    let rig = TerminatorRig {
        stripes: ZMarkStripeSet::new(8),
        ingress: ZMarkIngress::new(),
        stats: ZMarkStats::default(),
        stop: AtomicBool::new(false),
        term: ZMarkTerminator::new(1),
    };
    let cycle = rig.term.arm();

    let mut work = vec![7u64];
    rig.stripes.publish(0, &mut work);
    rig.term.note_work_published();

    // Sole worker: abort fires, generation moves once.
    let g0 = rig.term.work_generation();
    assert_eq!(
        rig.term
            .worker_idle(cycle, &rig.stripes, &rig.ingress, &rig.stats, &rig.stop),
        ZIdleOutcome::Resume
    );
    let g1 = rig.term.work_generation();
    assert_ne!(g1, g0);

    // Take the work, as the resuming worker does, and re-offer. The queues
    // are empty now, so the probe must terminate rather than abort again —
    // one wakeup per abort, and no abort without work.
    let mut taken = Vec::new();
    assert_eq!(rig.stripes.drain_from(0, &mut taken, Z_MARK_STEAL_CAP), 1);
    assert_eq!(
        rig.term
            .worker_idle(cycle, &rig.stripes, &rig.ingress, &rig.stats, &rig.stop),
        ZIdleOutcome::Terminated
    );
    assert_eq!(
        rig.term.work_generation(),
        g1,
        "a terminating probe must not publish a wakeup — that would be the \
         storm: a generation that keeps moving with no work behind it"
    );
    assert_eq!(rig.stats.termination_aborts.load(Ordering::Relaxed), 1);
    assert_eq!(rig.stats.terminations.load(Ordering::Relaxed), 1);
}

// -- chunked reference scanning -----------------------------------------

/// The default [`ZMarkContext::visit_refs_chunked`] must be exactly one
/// [`ZMarkContext::visit_refs`] that finishes the object.
///
/// This is the property that makes the method addable: every existing
/// implementor inherits it, so nothing changes for a context that has not
/// opted in. Asserted directly rather than inferred from a mark set,
/// because "the mark set is the same" would also hold for a default that
/// visited the object twice.
#[test]
fn the_inherited_chunked_visit_is_one_unsplit_visit_refs() {
    /// A context that overrides **only** the required methods, so
    /// `visit_refs_chunked` really is the trait's provided body and not a
    /// copy of it. `TestMarkContext` overrides the method (to offer
    /// `with_chunked_visits`) and therefore cannot answer this question.
    struct BareContext;
    impl ZMarkContext for BareContext {
        fn good_mask(&self) -> u64 {
            Z_MARKED0
        }
        fn try_mark(&self, _addr: u64) -> bool {
            true
        }
        fn is_marked(&self, _addr: u64) -> bool {
            false
        }
        fn visit_refs(&self, _addr: u64, f: &mut dyn FnMut(u64)) {
            for child in [2u64, 3, 4] {
                f(child);
            }
        }
        fn is_in_heap(&self, _addr: u64) -> bool {
            true
        }
    }

    let ctx = BareContext;
    let mut seen = Vec::new();
    let next = ctx.visit_refs_chunked(1, 0, 1, &mut |c| seen.push(c));
    assert_eq!(
        next, None,
        "the inherited body must report the object FINISHED, whatever budget \
         it is given — a `Some` here would park a cursor no override exists \
         to consume"
    );
    assert_eq!(sorted(seen), vec![2, 3, 4], "and must report every edge");

    // And it must not double-report if it is ever asked to resume.
    let mut again = Vec::new();
    assert_eq!(
        ctx.visit_refs_chunked(1, 7, 1, &mut |c| again.push(c)),
        None
    );
    assert!(
        again.is_empty(),
        "a nonzero cursor into the inherited body must be a no-op, not a \
         second full visit"
    );
}

/// Splitting an object changes nothing about what is reached.
///
/// Two identical graphs, one context chunking at 7 children per call and
/// one not, marked by real worker pools. The mark sets must be equal — that
/// is the whole correctness claim of
/// `gap-a-huge-reference-arrays-bound-the-yield-latency.md`'s fix: every
/// edge is reported exactly once **across** the chunks, so an object that is
/// half-scanned when a worker leaves is finished by the same worker and
/// never dropped.
///
/// The counters are the second half: the chunked arm must actually have
/// split (or the equality is vacuous), and the plain arm must not have
/// (or the inherited default has stopped being the default).
#[test]
fn chunked_and_unchunked_scans_produce_the_same_mark_set() {
    // One 200-child hub, which is the shape the gap page is about, scaled
    // down to a chunk limit rather than to a real array length.
    let children: Vec<u64> = (2..=201u64).collect();
    let g = graph(&[(1, children.as_slice())]);

    let plain = Arc::new(TestMarkContext::new(g.clone()));
    let plain_pool = ZMarkCoordinator::new(plain.clone(), 2);
    plain_pool.begin_cycle();
    assert_eq!(plain_pool.push_roots(&[1]), 1);
    let plain_report = plain_pool.mark_to_completion(RESTART_BUDGET);
    plain_pool.end_cycle();

    let chunky = Arc::new(TestMarkContext::new(g).with_chunked_visits(7));
    let chunky_pool = ZMarkCoordinator::new(chunky.clone(), 2);
    chunky_pool.begin_cycle();
    assert_eq!(chunky_pool.push_roots(&[1]), 1);
    let chunky_report = chunky_pool.mark_to_completion(RESTART_BUDGET);
    chunky_pool.end_cycle();

    assert!(!plain_report.budget_exhausted && !chunky_report.budget_exhausted);
    assert_eq!(
        chunky.marked_sorted(),
        plain.marked_sorted(),
        "chunking must not change what is reachable"
    );
    assert_eq!(chunky.marked_count(), 201, "the hub plus every child");

    assert_eq!(
        plain_report.stats.ref_chunks, 0,
        "the inherited visit never parks a cursor"
    );
    // 200 children at 7 per call is 29 calls, the last of which finishes
    // the object and returns `None`.
    assert_eq!(
        chunky_report.stats.ref_chunks, 28,
        "one continuation per call that did NOT finish the hub"
    );
    assert_eq!(
        chunky_report.stats.objects_scanned, 201,
        "`objects_scanned` counts objects FINISHED, not chunks — a drain \
         that conflated the two would make every huge array look like \
         hundreds of objects"
    );

    plain_pool.shutdown();
    chunky_pool.shutdown();
}

/// A safepoint taken while an object is half scanned must lose nothing.
///
/// The in-progress cursor lives in a worker-private field that no stripe
/// and no other worker can see, so a drain that dropped it on the way out
/// to a pause would leave an object **marked** with its remaining
/// out-edges untraced — the exact shape that closes a mark set over a live
/// subgraph and then sweeps it. `run_cycle` therefore asks
/// [`ZMarkWorker::has_work`] rather than looking at the local stack, and
/// this test is what holds that in place.
///
/// Like `workers_yield_for_a_safepoint_and_resume_without_losing_work`, it
/// asserts on the mark **set** and never on timing: a run in which no
/// safepoint happened to land mid-array degrades coverage instead of
/// failing.
#[test]
fn a_safepoint_while_an_object_is_half_scanned_loses_no_edges() {
    let children: Vec<u64> = (2..=5001u64).collect();
    let g = graph(&[(1, children.as_slice())]);
    let ctx = Arc::new(TestMarkContext::new(g).with_chunked_visits(4));
    let pool = ZMarkCoordinator::new(ctx.clone(), 3);

    pool.begin_cycle();
    assert_eq!(pool.push_roots(&[1]), 1);
    pool.start_marking();

    // Pause and resume repeatedly while the hub is being chunked through.
    // Each guard's construction is itself the assertion that every worker
    // reached a yield point — which, before chunking, a worker inside this
    // hub could not do until the whole 5000-slot scan had finished.
    for _ in 0..16 {
        let guard = pool.pause_for_safepoint();
        drop(guard);
        std::thread::yield_now();
    }

    pool.wait_for_fixed_point();
    assert_eq!(pool.try_end_mark(), ZMarkEndResult::Complete);
    pool.end_cycle();

    let mut expected: Vec<u64> = vec![1];
    expected.extend(2..=5001u64);
    assert_eq!(
        ctx.marked_sorted(),
        expected,
        "a safepoint dropped the half-scanned hub's remaining edges"
    );
    assert!(
        pool.stats().snapshot().ref_chunks > 0,
        "the fixture must actually have split the hub, or this asserts nothing"
    );
    pool.shutdown();
}

// -- safepoint yielding -------------------------------------------------

/// The safepoint yield, with the interleaving *constructed* rather than
/// raced for.
///
/// The first worker to enter `visit_refs` parks inside it, which pins
/// `ZMarkPauseControl::in_drain >= 1` until a releaser thread lets it go —
/// and the releaser only does so once the pause has actually been
/// requested. So at the instant `pause_for_safepoint` stores the flag,
/// a worker is *known* to be inside a drain region, and the guard's
/// construction is a real assertion that the coordinator waited for it.
/// Without the handoff, a 5000-node chain is often fully marked before the
/// test thread reaches its first pause, and the test passes vacuously.
///
/// Every spin here is bounded and falls through, exactly as in
/// `termination_does_not_fire_while_a_worker_holds_work_in_flight`: this
/// test asserts on the mark **set**, never on elapsed time, and can never
/// hang waiting for an interleaving that did not materialise.
#[test]
fn workers_yield_for_a_safepoint_and_resume_without_losing_work() {
    // Bounded-spin budget; see the doc comment. A spin count, never a
    // duration — nothing here asserts on wall clock.
    const SPIN_BUDGET: u64 = 20_000_000;

    let mut g: FxHashMap<u64, Vec<u64>> = FxHashMap::default();
    let mut expected = Vec::new();
    // Ids start at 1 — address `0` is this engine's null; see the comment
    // in `concurrent_mutator_marks_are_folded_in_by_the_restart_loop`.
    for i in 1..=5000u64 {
        g.insert(i, if i < 5000 { vec![i + 1] } else { Vec::new() });
        expected.push(i);
    }

    let in_visit = Arc::new(StdAtomicBool::new(false));
    let released = Arc::new(StdAtomicBool::new(false));
    let gate_taken = Arc::new(AtomicUsize::new(0));

    let hook_in_visit = Arc::clone(&in_visit);
    let hook_released = Arc::clone(&released);
    let hook_taken = Arc::clone(&gate_taken);
    let ctx = Arc::new(
        TestMarkContext::new(g).with_visit_hook(Box::new(move |_addr: u64| {
            // Only the very first visit of the run gates; every later one
            // passes straight through, so this costs one atomic per
            // scanned object and cannot stall the drain.
            if hook_taken.fetch_add(1, Ordering::SeqCst) != 0 {
                return;
            }
            hook_in_visit.store(true, Ordering::SeqCst);
            let mut spins: u64 = 0;
            while !hook_released.load(Ordering::Acquire) && spins < SPIN_BUDGET {
                spins += 1;
                std::thread::yield_now();
            }
        })),
    );

    let pool = ZMarkCoordinator::new(ctx.clone(), 3);

    pool.begin_cycle();
    assert_eq!(pool.push_roots(&[1]), 1, "the root must be accepted");
    pool.start_marking();

    // Releaser: frees the gated worker only once a pause has actually been
    // requested, so `wait_until_quiescent` is guaranteed to have started
    // with a non-empty drain region. Bounded, so a missed interleaving
    // degrades coverage instead of deadlocking.
    let releaser_shared = Arc::clone(pool.shared());
    let releaser_released = Arc::clone(&released);
    let releaser = std::thread::spawn(move || {
        let mut spins: u64 = 0;
        while !releaser_shared.pause().pause_requested() && spins < SPIN_BUDGET {
            spins += 1;
            std::thread::yield_now();
        }
        releaser_released.store(true, Ordering::Release);
    });

    // Wait until a worker is demonstrably inside `visit_refs`.
    let mut spins: u64 = 0;
    while !in_visit.load(Ordering::SeqCst) && spins < SPIN_BUDGET {
        spins += 1;
        std::thread::yield_now();
    }
    let gated = in_visit.load(Ordering::SeqCst) && !released.load(Ordering::Acquire);

    // Take a "safepoint" a few times while marking is in flight. The
    // guard's construction is the assertion: it returns only once every
    // worker has left its drain region.
    for _ in 0..3 {
        let guard = pool.pause_for_safepoint();
        // Marking is quiescent here; nothing may be inside visit_refs.
        drop(guard);
        std::thread::yield_now();
    }
    released.store(true, Ordering::Release);
    releaser.join().expect("releaser thread");

    pool.wait_for_fixed_point();
    assert_eq!(pool.try_end_mark(), ZMarkEndResult::Complete);
    pool.end_cycle();

    assert_eq!(
        ctx.marked_sorted(),
        sorted(expected),
        "a safepoint yield dropped work that had already been popped into a \
         worker's local stack (gated={gated})"
    );
    pool.shutdown();
}

// -- reference processing hook ------------------------------------------

struct KeepAliveHook {
    /// Referents to resurrect if they are not already strongly marked.
    candidates: Vec<u64>,
    /// Records what liveness the hook observed, so the test can prove the
    /// hook saw the *final* strong-reachability answer.
    observed_live: Mutex<Vec<u64>>,
}

impl ZNonStrongRefHook for KeepAliveHook {
    fn process(&self, is_marked: &dyn Fn(u64) -> bool, keep_alive: &mut dyn FnMut(u64)) {
        let mut live = self.observed_live.lock();
        for &c in &self.candidates {
            if is_marked(c) {
                live.push(c);
            } else {
                // Policy says keep it: resurrect.
                keep_alive(c);
            }
        }
    }
}

#[test]
fn the_non_strong_ref_hook_resurrects_and_the_redrain_traces_the_closure() {
    let g = graph(&[
        (1, &[2]),
        (2, &[]),
        // Soft-reachable island, unreachable strongly.
        (60, &[61]),
        (61, &[62]),
        (62, &[]),
    ]);
    let ctx = Arc::new(TestMarkContext::new(g));
    let pool = ZMarkCoordinator::new(ctx.clone(), 2);

    pool.begin_cycle();
    pool.push_roots(&[1]);
    let first = pool.mark_to_completion(RESTART_BUDGET);
    assert!(!first.budget_exhausted);
    assert_eq!(ctx.marked_sorted(), vec![1, 2]);

    let hook = KeepAliveHook {
        candidates: vec![2, 60],
        observed_live: Mutex::new(Vec::new()),
    };
    let resurrected = pool.process_non_strong_refs(&hook);
    assert_eq!(resurrected, 1, "only 60 was dead; 2 was already live");
    assert_eq!(
        hook.observed_live.lock().clone(),
        vec![2],
        "the hook must see the final strong-reachability answer, and must \
         not be told an object is live merely because the hook itself \
         just marked it"
    );

    // The resurrection has unscanned out-edges — re-drain.
    let second = pool.mark_to_completion(RESTART_BUDGET);
    pool.end_cycle();
    assert!(!second.budget_exhausted);
    assert_eq!(
        ctx.marked_sorted(),
        vec![1, 2, 60, 61, 62],
        "a resurrected referent drags its whole subgraph with it"
    );
    pool.shutdown();
}

// -- pool lifecycle -----------------------------------------------------

#[test]
fn a_pool_with_no_roots_reaches_a_fixed_point_immediately() {
    let ctx = Arc::new(TestMarkContext::new(graph(&[(1, &[])])));
    let pool = ZMarkCoordinator::new(ctx.clone(), 4);
    pool.begin_cycle();
    let report = pool.mark_to_completion(RESTART_BUDGET);
    pool.end_cycle();
    assert_eq!(report.passes, 1);
    assert_eq!(report.restarts, 0);
    assert_eq!(ctx.marked_count(), 0);
    pool.shutdown();
}

#[test]
fn successive_cycles_reuse_the_same_pool() {
    let g = graph(&[(1, &[2]), (2, &[3]), (3, &[]), (10, &[11]), (11, &[])]);
    let ctx = Arc::new(TestMarkContext::new(g));
    let pool = ZMarkCoordinator::new(ctx.clone(), 3);

    for round in 0..4 {
        ctx.reset_marks();
        pool.begin_cycle();
        let roots: &[u64] = if round % 2 == 0 { &[1] } else { &[10] };
        pool.push_roots(roots);
        let report = pool.mark_to_completion(RESTART_BUDGET);
        pool.end_cycle();
        assert!(!report.budget_exhausted, "round {round}");
        let expected: Vec<u64> = if round % 2 == 0 {
            vec![1, 2, 3]
        } else {
            vec![10, 11]
        };
        assert_eq!(ctx.marked_sorted(), expected, "round {round}");
    }
    pool.shutdown();
}

#[test]
fn dropping_the_coordinator_stops_and_joins_every_worker() {
    let ctx = Arc::new(TestMarkContext::new(graph(&[(1, &[])])));
    {
        let pool = ZMarkCoordinator::new(ctx.clone(), 4);
        pool.begin_cycle();
        pool.push_roots(&[1]);
        pool.start_marking();
        // Dropped here with a cycle armed: Drop must stop and join, not
        // detach. A detached worker outliving its heap is exactly the
        // process-global-state failure mode this module avoids.
    }
    assert_eq!(ctx.marked_sorted(), vec![1]);
}

#[test]
fn ingress_buckets_do_not_alias_across_slots() {
    let ingress = ZMarkIngress::new();
    for slot in 0..Z_MARK_INGRESS_BUCKETS {
        ingress.push(slot, 1000 + slot as u64);
    }
    assert!(ingress.has_work());
    assert_eq!(ingress.pending_hint(), Z_MARK_INGRESS_BUCKETS);
    let mut out = Vec::new();
    assert_eq!(ingress.drain_into(&mut out), Z_MARK_INGRESS_BUCKETS);
    assert_eq!(sorted(out).len(), Z_MARK_INGRESS_BUCKETS);
    assert!(!ingress.has_work());
}

// -- zs s5/mc3: stealable reference-array chunks --------------------------

/// Every chunk of an `n`-chunk array is walked exactly once by the task
/// decomposition: chunk 0 by the splitter, every other chunk as the OWN
/// chunk of exactly one task, reached through the sub-task tree.
#[test]
fn zs5mc3_the_task_decomposition_covers_every_chunk_once() {
    for n in 1..=1100usize {
        let mut seen = vec![0u32; n];
        seen[0] += 1;
        let mut pending: Vec<usize> = Vec::new();
        array_subtasks(0, n, |t| pending.push(t));
        let mut prev = usize::MAX;
        for &t in &pending {
            assert!(
                t < prev,
                "n={n}: first-visit tasks are pushed largest first"
            );
            prev = t;
        }
        while let Some(t) = pending.pop() {
            assert!(t >= 1 && t < n, "n={n}: task {t} out of range");
            seen[t] += 1;
            array_subtasks(t, n, |s| {
                assert!(s > t, "n={n}: a sub-task lies above its task");
                pending.push(s);
            });
        }
        assert!(seen.iter().all(|c| *c == 1), "n={n}: {seen:?}");
    }
}

#[test]
fn zs5mc3_a_task_word_round_trips_and_refuses_what_it_cannot_carry() {
    for (base, idx) in [
        (8u64, 1usize),
        (0x7fff_ffff_fff8, Z_MARK_ARRAY_TASK_MAX_CHUNKS - 1),
        (0x5555_5555_5550, 12345),
    ] {
        let w = array_task_encode(base, idx).expect("encodable");
        assert_eq!(w & Z_MARK_ARRAY_TASK_TAG, Z_MARK_ARRAY_TASK_TAG);
        assert_eq!(array_task_decode(w), Some((base, idx)));
        assert_eq!(work_entry_base(w), base);
    }
    assert_eq!(array_task_decode(0x1000), None, "a base is not a task");
    assert_eq!(work_entry_base(0x1000), 0x1000);
    assert_eq!(array_task_encode(12, 1), None, "unaligned base");
    assert_eq!(array_task_encode(1 << 47, 1), None, "base past 2^47");
    assert_eq!(array_task_encode(8, 0), None, "chunk 0 is never a task");
    assert_eq!(
        array_task_encode(8, Z_MARK_ARRAY_TASK_MAX_CHUNKS),
        None,
        "index past the field"
    );
}

/// Two nested hubs (8-aligned ids, so splittable) whose children are
/// leaves, the inner hub one of the outer hub's children, plus a detached
/// node. Returns the graph and the expected mark set.
fn hub_graph(outer_n: u64, inner_n: u64) -> (FxHashMap<u64, Vec<u64>>, Vec<u64>) {
    let (outer, inner) = (8u64, 16u64);
    let mut g: FxHashMap<u64, Vec<u64>> = FxHashMap::default();
    let mut expected = vec![outer, inner];
    let mut outer_children: Vec<u64> = (0..outer_n).map(|i| 1_000_001 + 2 * i).collect();
    outer_children.insert(outer_children.len() / 3, inner);
    let inner_children: Vec<u64> = (0..inner_n).map(|i| 5_000_001 + 2 * i).collect();
    for &c in outer_children.iter().chain(&inner_children) {
        if c != inner {
            g.insert(c, Vec::new());
            expected.push(c);
        }
    }
    g.insert(outer, outer_children);
    g.insert(inner, inner_children);
    g.insert(9_999_999, vec![1_000_001]);
    (g, sorted(expected))
}

fn split_run(
    g: FxHashMap<u64, Vec<u64>>,
    workers: usize,
    chunk: usize,
    limit: usize,
    split: bool,
) -> (Arc<TestMarkContext>, ZMarkCycleReport) {
    let ctx = Arc::new(
        TestMarkContext::new(g)
            .with_chunked_visits(limit)
            .with_splittable_arrays(),
    );
    let pool = ZMarkCoordinator::new(ctx.clone(), workers);
    pool.set_ref_chunk(chunk);
    pool.set_split_arrays(split);
    pool.begin_cycle();
    pool.push_roots(&[8]);
    let report = pool.mark_to_completion(RESTART_BUDGET);
    pool.end_cycle();
    pool.shutdown();
    (ctx, report)
}

/// The split arm marks the same set as the kill arm, walks every element
/// edge EXACTLY ONCE (no lost slot, no doubled slot), counts each object
/// once, and runs every task it pushed; the kill arm splits nothing
/// (kill-switch control) and parks the arrays chunk by chunk instead.
/// `limit < chunk` makes every walk stop short, so the remainder of a
/// range goes through the in-progress slot too.
#[test]
fn zs5mc3_split_arrays_walk_every_element_once_and_mark_the_same_set() {
    let (outer_n, inner_n) = (1003u64, 517u64);
    for (chunk, limit) in [(4usize, 4usize), (4, 3), (16, 5), (1, 1)] {
        let (g, expected) = hub_graph(outer_n, inner_n);
        let (ctx_k, rep_k) = split_run(g.clone(), 4, chunk, limit, false);
        let (ctx_s, rep_s) = split_run(g, 4, chunk, limit, true);
        assert!(!rep_k.budget_exhausted && !rep_s.budget_exhausted);
        assert_eq!(ctx_k.marked_sorted(), expected, "kill arm, chunk={chunk}");
        assert_eq!(ctx_s.marked_sorted(), expected, "split arm, chunk={chunk}");
        for (arm, ctx) in [("kill", &ctx_k), ("split", &ctx_s)] {
            let counts = ctx.edge_counts();
            assert_eq!(
                counts.len() as u64,
                outer_n + 1 + inner_n,
                "{arm} arm, chunk={chunk}: every element edge reported"
            );
            assert!(
                counts.values().all(|c| *c == 1),
                "{arm} arm, chunk={chunk}: no edge reported twice: {:?}",
                counts
                    .iter()
                    .filter(|(_, c)| **c != 1)
                    .take(5)
                    .collect::<Vec<_>>()
            );
        }
        let n_chunks = |len: u64| (len as usize).div_ceil(chunk) as u64;
        let tasks = n_chunks(outer_n + 1) - 1 + n_chunks(inner_n) - 1;
        let s = rep_s.stats;
        assert_eq!(s.objects_scanned, expected.len() as u64, "chunk={chunk}");
        assert_eq!(s.array_splits, 2, "chunk={chunk}: both hubs split: {s:?}");
        assert_eq!(s.array_tasks, tasks, "chunk={chunk}: n - 1 tasks per hub");
        assert_eq!(s.array_task_runs, tasks, "chunk={chunk}: every task ran");
        let k = rep_k.stats;
        assert_eq!(k.objects_scanned, expected.len() as u64);
        assert_eq!(
            (k.array_splits, k.array_tasks, k.array_task_runs),
            (0, 0, 0),
            "kill arm splits nothing"
        );
        assert!(k.ref_chunks > 0, "kill arm parks the arrays instead");
    }
}

/// zt t1/mk (`CRATONVM_ZGC_MARK_SPLIT_CHUNK`): a split UNIT smaller or
/// larger than the yield chunk still marks the same set, walks every
/// element edge exactly once and runs every task it pushed; the task count
/// follows the unit, not the chunk. `limit` below the unit or the chunk
/// makes walks stop short, so the in-progress slot carries the remainder
/// of a task (unit > chunk: every task is walked in chunk steps) and of
/// chunk task 0 (limit < unit). Control: `unit == 0` is the yield chunk,
/// i.e. the zs s5/mc3 counts.
#[test]
fn zt1mk_a_split_unit_apart_from_the_yield_chunk_walks_every_element_once() {
    let (outer_n, inner_n) = (1003u64, 517u64);
    for (chunk, unit, limit) in [
        (16usize, 4usize, 16usize),
        (16, 4, 3),
        (4, 16, 4),
        (4, 16, 3),
        (8, 0, 8),
        (5, 7, 2),
    ] {
        let (g, expected) = hub_graph(outer_n, inner_n);
        let ctx = Arc::new(
            TestMarkContext::new(g)
                .with_chunked_visits(limit)
                .with_splittable_arrays(),
        );
        let pool = ZMarkCoordinator::new(ctx.clone(), 4);
        pool.set_ref_chunk(chunk);
        pool.set_split_chunk(unit);
        pool.set_split_arrays(true);
        pool.begin_cycle();
        pool.push_roots(&[8]);
        let report = pool.mark_to_completion(RESTART_BUDGET);
        pool.end_cycle();
        pool.shutdown();
        let tag = format!("chunk={chunk} unit={unit} limit={limit}");
        assert!(!report.budget_exhausted, "{tag}");
        assert_eq!(ctx.marked_sorted(), expected, "{tag}");
        let counts = ctx.edge_counts();
        assert_eq!(
            counts.len() as u64,
            outer_n + 1 + inner_n,
            "{tag}: every element edge reported"
        );
        assert!(counts.values().all(|c| *c == 1), "{tag}: no edge reported twice");
        let u = if unit == 0 { chunk } else { unit };
        let n_tasks = |len: u64| (len as usize).div_ceil(u) as u64 - 1;
        let tasks = n_tasks(outer_n + 1) + n_tasks(inner_n);
        let s = report.stats;
        assert_eq!(s.objects_scanned, expected.len() as u64, "{tag}");
        assert_eq!(s.array_splits, 2, "{tag}: both hubs split: {s:?}");
        assert_eq!(s.array_tasks, tasks, "{tag}: tasks follow the unit");
        assert_eq!(s.array_task_runs, tasks, "{tag}: every task ran");
    }
}

/// THE POINT of the split, and the test that fails on the pre-split
/// engine: with one huge array as the critical path (every chunk walk
/// sleeps), more than one worker walks its chunks. On the kill arm --
/// which is the pre-split behaviour, kept byte for byte -- exactly one
/// worker walks all of them.
#[test]
fn zs5mc3_a_huge_array_is_walked_by_more_than_one_worker() {
    let mut g: FxHashMap<u64, Vec<u64>> = FxHashMap::default();
    let hub = 8u64;
    let children: Vec<u64> = (0..2048u64).map(|i| 3_000_001 + 2 * i).collect();
    for &c in &children {
        g.insert(c, Vec::new());
    }
    g.insert(hub, children);
    for split in [true, false] {
        let ctx = Arc::new(
            TestMarkContext::new(g.clone())
                .with_chunked_visits(8)
                .with_splittable_arrays()
                .with_chunk_delay(std::time::Duration::from_micros(300)),
        );
        let pool = ZMarkCoordinator::new(ctx.clone(), 4);
        pool.set_ref_chunk(8);
        pool.set_split_arrays(split);
        pool.set_idle_spill(true);
        pool.begin_cycle();
        pool.push_roots(&[hub]);
        let report = pool.mark_to_completion(RESTART_BUDGET);
        pool.end_cycle();
        pool.shutdown();
        assert_eq!(ctx.marked_count(), 2049, "split={split}");
        let walkers = ctx.walk_thread_count(hub);
        if split {
            assert!(
                walkers >= 2,
                "split arm: the array's chunks spread over the pool \
                 (walkers={walkers}, stats={:?})",
                report.stats
            );
            assert_eq!(report.stats.array_task_runs, 255);
        } else {
            assert_eq!(walkers, 1, "kill arm: one worker owns the whole array");
        }
    }
}

/// A one-worker pool has nobody to steal a task: it never splits, and
/// marks the same set chunk by chunk.
#[test]
fn zs5mc3_a_one_worker_pool_does_not_split() {
    let (g, expected) = hub_graph(100, 40);
    let (ctx, report) = split_run(g, 1, 4, 4, true);
    assert_eq!(ctx.marked_sorted(), expected);
    assert_eq!(report.stats.array_splits, 0);
    assert!(report.stats.ref_chunks > 0);
}

/// A reused pool splits again in its next cycle (no task word survives
/// the cycle) and marks the same set.
#[test]
fn zs5mc3_a_reused_pool_splits_every_cycle() {
    let (g, expected) = hub_graph(300, 70);
    let ctx = Arc::new(
        TestMarkContext::new(g)
            .with_chunked_visits(4)
            .with_splittable_arrays(),
    );
    let pool = ZMarkCoordinator::new(ctx.clone(), 3);
    pool.set_ref_chunk(4);
    pool.set_split_arrays(true);
    for cycle in 0..3 {
        ctx.reset_marks();
        pool.begin_cycle_with(ctx.clone());
        pool.push_roots(&[8]);
        let report = pool.mark_to_completion(RESTART_BUDGET);
        pool.end_cycle();
        assert_eq!(ctx.marked_sorted(), expected, "cycle {cycle}");
        assert_eq!(report.stats.array_splits, 2, "cycle {cycle}");
        assert_eq!(report.stats.array_tasks, report.stats.array_task_runs);
    }
    pool.shutdown();
}

/// Residual 2: the allocation-free publish moves the same oldest half,
/// in the same order, as `split_off_oldest_half` + `publish`.
#[test]
fn zs5mc3_publish_oldest_half_moves_what_split_off_moved() {
    for n in [0usize, 1, 2, 7, 256, 513] {
        let mut a = ZMarkLocalStack::new();
        let mut b = ZMarkLocalStack::new();
        for i in 0..n as u64 {
            a.push(8 * (i + 1));
            b.push(8 * (i + 1));
        }
        let sa = ZMarkStripeSet::new(8);
        let sb = ZMarkStripeSet::new(8);
        let mut half = a.split_off_oldest_half();
        let na = sa.publish(3, &mut half);
        let nb = b.publish_oldest_half(3, &sb);
        assert_eq!(na, nb, "n={n}");
        assert_eq!(
            a.take_all(),
            b.take_all(),
            "n={n}: the stack keeps the same half"
        );
        let (mut oa, mut ob) = (Vec::new(), Vec::new());
        sa.drain_from(3, &mut oa, usize::MAX);
        sb.drain_from(3, &mut ob, usize::MAX);
        assert_eq!(oa, ob, "n={n}: the stripe got the same half");        }    }