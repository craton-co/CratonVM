use super::*;
use cratonvm_gc_core::collector::{GarbageCollector, MonitorCleanup};
use crate::g1::{G1Collector, G1CollectorConfig};

/// Test-only `StopTheWorldToken` (I-17). These tests drive the mark cycle
/// single-threaded, so the STW invariant the token witnesses is trivially
/// satisfied.
#[inline]
fn stw() -> cratonvm_gc_core::collector::StopTheWorldToken {
    // SAFETY: single-threaded test harness; no mutator is running.
    unsafe { cratonvm_gc_core::collector::StopTheWorldToken::new() }
}
use cratonvm_gc_core::heap::ArrayElementType;
use cratonvm_types::{ClassId, Value};
use std::collections::HashMap;

struct NoopMonitors;
impl MonitorCleanup for NoopMonitors {
    fn remap_after_gc(&self, _pointer_map: &cratonvm_types::PointerMap) {}
}

fn small_collector() -> Arc<G1Collector> {
    let cfg = G1CollectorConfig {
        heap_size: 8 * 1024 * 1024,
        region_size: 1024 * 1024,
        ..Default::default()
    };
    Arc::new(G1Collector::new(cfg))
}

/// Test #1 — the concurrent-mark thread spawns and joins cleanly.
/// Verifies the basic thread-lifecycle skeleton: spawn → run → stop → join.
/// Item 9b: a seed pushed while every worker is parked is picked up by
/// the wake, not by the fallback poll. The bound is half the poll, so a
/// result inside it cannot have come from the timeout.
#[test]
fn a_seed_wakes_a_parked_marker_without_waiting_for_the_poll() {
    let g1 = small_collector();
    g1.start_concurrent_mark(&stw());
    let controller = ConcurrentMarkController::spawn(Arc::clone(&g1));
    for _ in 0..500 {
        if controller.is_quiesced() {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(
        controller.is_quiesced(),
        "the marker parks on an empty gray set"
    );

    // BEST OF N, not one shot. The bound is what carries the argument --
    // a result inside half a poll interval cannot have come from the
    // timeout -- but the measurement is wall-clock on a thread the OS is
    // free not to schedule, so a single attempt can miss the bound on a
    // loaded host without anything being wrong. Retrying does not weaken
    // the claim: a marker that only ever learns of new work from the
    // 250 ms poll cannot land inside 125 ms on ANY attempt, so one fast
    // observation still proves the wake, while five attempts stop a
    // scheduler hiccup from reading as a defect.
    let bound = Duration::from_millis(WORKER_POLL_MS / 2);
    let mut marked = false;
    let mut slowest = Duration::ZERO;
    for _ in 0..5 {
        let a = g1.alloc_object(ClassId::new(1), 0);
        let t0 = std::time::Instant::now();
        g1.remark(&stw(), &[a]);
        while t0.elapsed() < bound {
            if g1.dbg_is_marked(a.as_ptr() as usize) {
                marked = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        if marked {
            break;
        }
        slowest = slowest.max(t0.elapsed());
        // Let the marker quiesce again so the next attempt really does
        // start from a PARKED worker -- otherwise attempt two measures a
        // running worker and proves nothing.
        for _ in 0..500 {
            if controller.is_quiesced() {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    controller.request_stop_and_join().expect("worker joined");
    assert!(
        marked,
        "no seed was marked within {bound:?} in five attempts (slowest miss {slowest:?}); the parked worker was waiting for the {WORKER_POLL_MS} ms poll instead of the wake"
    );
}

#[test]
fn concurrent_mark_thread_spawns_and_joins() {
    let g1 = small_collector();
    // Seed at least one phase-state transition so the worker has a
    // well-formed environment (otherwise the worklist is empty and
    // the worker parks immediately — which is fine, but we want to
    // exercise the SATB activation path too).
    g1.start_concurrent_mark(&stw());

    let controller = ConcurrentMarkController::spawn(Arc::clone(&g1));
    assert!(controller.is_running(), "worker thread must be alive");

    // Give the worker a beat to perform at least one step.
    std::thread::sleep(Duration::from_millis(30));

    // Stop and join. A clean exit is the success condition.
    let result = controller.request_stop_and_join();
    assert!(result.is_ok(), "worker panic on join: {:?}", result.err());
}

/// Test #2 — mutator-side SATB pre-barrier captures references that
/// are overwritten while the concurrent mark thread is running.
///
/// Scenario: allocate two objects A and B with A.field[0] = B. Start
/// the concurrent mark cycle (this activates the SATB barrier). On
/// the main thread, simulate a mutator overwriting A.field[0] = null
/// via the SATB pre-barrier. Stop the worker. Inspect the SATB queue
/// to verify it observed the old reference value (B).
#[test]
fn satb_captures_mutator_writes_during_concurrent_mark() {
    let g1 = small_collector();
    let a = g1.alloc_object(ClassId::new(1), 1);
    let b = g1.alloc_object(ClassId::new(2), 1);
    g1.set_field(a, 0, Value::Object(Some(b)));

    let b_addr = b.as_ptr() as usize;

    // Initial-mark STW: activate SATB barrier + clear bitmap.
    g1.start_concurrent_mark(&stw());
    assert!(
        g1.satb_queue().is_active(),
        "SATB must be active during concurrent mark"
    );

    // Spawn the background marker. From this point mutators run
    // concurrently with the marker; the SATB barrier captures any
    // reference they overwrite.
    let controller = ConcurrentMarkController::spawn(Arc::clone(&g1));

    // Simulate a mutator: it reads A.field[0] (which is B), then
    // overwrites it. The pre-barrier logs B's old address into the
    // SATB queue BEFORE the store, so even if the marker hasn't
    // scanned A yet it will pick up B from the SATB log at remark.
    g1.satb_pre_barrier(b_addr);
    g1.set_field(a, 0, Value::Object(None));

    // Test-isolation (Step 3): immediately spill this thread's SATB buffer
    // into *this* collector's queue shards, so b_addr no longer lives in the
    // process-global thread-local buffer. G1's `remark` now drains that
    // global registry (finding #18 fix); without this spill a sibling test's
    // `remark`/`deactivate_and_drain` running in parallel could pull b_addr
    // into *its* queue before our own drain below — a shared-registry
    // parallel-test artifact, not a real bug (the whole suite is green
    // single-threaded). The shards are per-queue, so once spilled it is safe.
    cratonvm_gc_core::satb::flush_thread_satb_buffer(g1.satb_queue());

    // Let the worker observe the new state.
    std::thread::sleep(Duration::from_millis(30));

    // STW remark: stop the worker, then flush the thread-local SATB
    // buffer + deactivate-and-drain the global queue. In production
    // the safepoint protocol flushes per-thread buffers; the test
    // calls it directly since there's only one mutator thread (us).
    controller.request_stop_and_join().expect("worker joined");
    cratonvm_gc_core::satb::flush_thread_satb_buffer(g1.satb_queue());

    // B's overwritten reference must have reached the MARKER: either it
    // is still in the queue (final-drain path) or — since G1MARK-6 — the
    // background worker already pulled it into the gray set mid-cycle
    // (`concurrent_mark_step` drains the shards each step) and marked it
    // in its region's bitmap. Both routes deliver the SATB guarantee;
    // asserting on raw queue contents alone now under-approximates it.
    let drained = g1.satb_queue().deactivate_and_drain(&stw());
    let grayed_or_marked = g1.dbg_is_grayed_or_marked(b_addr);
    assert!(
        drained.contains(&b_addr) || grayed_or_marked,
        "SATB log must capture B's overwritten reference \
         (drained={:?} grayed_or_marked={})",
        drained,
        grayed_or_marked
    );
}

/// finding #18 regression — G1's `remark` must drain the per-thread SATB
/// buffer, not just the global shards.
///
/// `remark` drains with `drain()` (not `deactivate_and_drain`, which runs
/// only at end-of-cycle `cleanup`, where stragglers are *discarded*), so
/// before the fix it never pulled in a thread's partially-full local buffer:
/// a reference overwritten since that thread's last ~256-entry spill was
/// excluded from the remark snapshot and its target swept while reachable
/// (UAF). `remark` now drains the registry first.
///
/// A narrow log→remark window plus a direct per-object `is_marked` check
/// (not a global count) keep this robust against the process-global SATB
/// registry shared with parallel tests.
#[test]
fn remark_drains_thread_local_satb_buffer() {
    let g1 = small_collector();
    let obj = g1.alloc_object(ClassId::new(1), 0);
    let addr = obj.as_ptr() as usize;

    // Activate the SATB barrier, then log `addr` into THIS thread's local
    // buffer only — one entry is far below the 256-entry spill threshold, so
    // it never reaches a shard. The shard drain alone (pre-fix remark) would
    // miss it.
    g1.start_concurrent_mark(&stw());
    g1.satb_pre_barrier(addr);
    assert!(
        g1.satb_queue().is_empty(),
        "the single entry must still be purely thread-local (not spilled)"
    );

    // remark must drain the thread-local buffer into the gray set; drain the
    // worklist so the gray entry is actually marked.
    g1.remark(&stw(), &[]);
    g1.concurrent_mark_step(usize::MAX);

    g1.with_regions_mut(|regions| {
        let marked = regions.iter().any(|r| {
            let base = r.data.as_ptr() as usize;
            addr >= base && addr < base + r.data.len() && r.mark_bitmap.is_marked(addr)
        });
        assert!(
            marked,
            "remark must drain the thread-local SATB buffer and mark its ref"
        );
    });
}

/// Test #3 — live data is unchanged after a full concurrent-mark cycle.
///
/// Run the full cycle (initial-mark STW → concurrent mark → STW remark
/// → cleanup) and verify that the application-visible state of the
/// live object graph is identical: field values still readable, no
/// objects evacuated or modified by marking itself.
#[test]
fn live_data_unchanged_after_concurrent_mark_cycle() {
    let g1 = small_collector();
    let a = g1.alloc_object(ClassId::new(1), 2);
    let b = g1.alloc_object(ClassId::new(2), 1);
    g1.set_field(a, 0, Value::Object(Some(b)));
    g1.set_field(a, 1, Value::Int(0xCAFE));
    g1.set_field(b, 0, Value::Long(0xBEEF));

    let a_class = g1.class_id_of(a);
    let b_class = g1.class_id_of(b);

    // Phase 1 — initial-mark STW: prep the cycle and seed roots.
    g1.start_concurrent_mark(&stw());
    g1.remark(&stw(), &[a, b]);

    // Phase 2 — concurrent mark, mutators conceptually live.
    let controller = ConcurrentMarkController::spawn(Arc::clone(&g1));
    std::thread::sleep(Duration::from_millis(40));

    // Phase 3 — STW remark: stop worker, drain stragglers,
    // transition to cleanup.
    controller.request_stop_and_join().expect("worker joined");
    g1.remark(&stw(), &[a, b]);
    // Drain whatever the remark just pushed onto the worklist.
    let _ = g1.concurrent_mark_step(usize::MAX);
    g1.cleanup(&stw());

    // Live-data assertions: field values, class IDs, reference chain
    // must all be intact. Marking is read-only: it must not have
    // touched a single mutator-visible byte.
    assert_eq!(g1.class_id_of(a), a_class, "A's class_id must be unchanged");
    assert_eq!(g1.class_id_of(b), b_class, "B's class_id must be unchanged");
    assert_eq!(
        g1.get_field(a, 1).as_int(),
        Some(0xCAFE),
        "A.field[1] must be unchanged"
    );
    assert_eq!(
        g1.get_field(b, 0).as_long(),
        Some(0xBEEF),
        "B.field[0] must be unchanged"
    );
    if let Value::Object(Some(b_ref)) = g1.get_field(a, 0) {
        assert_eq!(
            b_ref.as_ptr(),
            b.as_ptr(),
            "A.field[0] must still point at B"
        );
    } else {
        panic!("A.field[0] must still be Object(Some(B))");
    }
}

/// Test #4 — concurrent mark drains a non-empty gray set.
///
/// Verifies that the worker does real work (not just spin-park). We
/// seed the worklist with a multi-object graph, run the worker until
/// it parks, and assert that every object in the graph is marked in
/// its owning region's bitmap.
#[test]
fn concurrent_mark_drains_gray_set() {
    let g1 = small_collector();

    // Build a small reference graph: a → b → c.
    let a = g1.alloc_object(ClassId::new(1), 1);
    let b = g1.alloc_object(ClassId::new(2), 1);
    let c = g1.alloc_object(ClassId::new(3), 0);
    g1.set_field(a, 0, Value::Object(Some(b)));
    g1.set_field(b, 0, Value::Object(Some(c)));

    // Initial-mark STW + seed roots.
    g1.start_concurrent_mark(&stw());
    g1.remark(&stw(), &[a]);

    // Run the worker until it quiesces.
    let controller = ConcurrentMarkController::spawn(Arc::clone(&g1));
    // Poll until the worker reports at least one step done; the
    // worklist had three nested objects so this is well-defined.
    for _ in 0..50 {
        if controller.steps_performed() > 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    controller.request_stop_and_join().expect("worker joined");

    // All three objects must be marked in their region's bitmap.
    // Use the pub `with_regions_mut` test hook to inspect bitmaps.
    g1.with_regions_mut(|regions| {
        for (label, obj) in [("a", a), ("b", b), ("c", c)] {
            let addr = obj.as_ptr() as usize;
            // Find the owning region linearly (test code — heap is small).
            let mut found = false;
            for region in regions.iter() {
                let base = region.data.as_ptr() as usize;
                if addr >= base && addr < base + region.data.len() {
                    assert!(
                        region.mark_bitmap.is_marked(addr),
                        "{} must be marked after concurrent mark drain",
                        label
                    );
                    found = true;
                    break;
                }
            }
            assert!(found, "{} must live in some region", label);
        }
    });
}

/// A collector whose configuration lets `concurrent_mark_worker_count`
/// derive the machine's ergonomic (`gc_worker_threads: 0`), so the F-12
/// tests below actually get more than one marker on a real machine.
fn parallel_mark_collector() -> Arc<G1Collector> {
    Arc::new(G1Collector::new(G1CollectorConfig {
        heap_size: 64 * 1024 * 1024,
        region_size: 1024 * 1024,
        ..Default::default()
    }))
}

/// F-12 — several markers must actually share the work, and the sharing
/// must come from STEALING when the seed queue cannot supply it.
///
/// A wide graph is the case that isolates the stealing path: the gray set
/// starts as ONE root, so the seed queue has nothing to hand out. Whichever
/// worker pops the root pushes every child onto its OWN deque, and the only
/// route by which its peers can get any of that work is to steal it. A
/// marker that had per-worker deques but no stealing would pass every
/// correctness test in this file and read zero here.
///
/// Verified capable of failing: making `take_marking_work` return `false`
/// instead of scanning peers (i.e. deques without stealing) leaves
/// `dbg_mark_steals()` at 0 and all the scanning on one worker.
#[test]
fn parallel_markers_steal_the_wide_frontier_from_each_other() {
    const CHILDREN: usize = 40_000;
    // THE ATTEMPT IS RETRIED. Whether a peer is SCHEDULED before the owner
    // drains its own deque is the scheduler's answer; whether it has a
    // ROUTE to the work is this collector's, and only the second is what
    // F-12 claims. On 2026-09-06 this test failed in 3 of 6 gc-lib-suite
    // runs on unmodified `dev` at host load ~20 -- always with
    // `scans [80010, 0]`, i.e. one worker did the entire frontier before
    // its peer ran at all -- on a host whose eight cores were already
    // carrying the suite's own eight test threads.
    //
    // Retrying gives nothing up: with `take_marking_work` returning `false`
    // instead of scanning peers (the arm this test was verified capable of
    // failing on) there is no route at all, so every attempt reads
    // `steals == 0` and this still goes red. A fresh collector per attempt
    // because the frontier is consumed.
    const ATTEMPTS: usize = 12;
    let mut last: Option<(Vec<usize>, usize, usize)> = None;
    for _ in 0..ATTEMPTS {
        let g1 = parallel_mark_collector();
        if g1.mark_worker_count() < 2 {
            eprintln!("[F-12] parallel marking not exercised: 1 worker on this machine");
            return;
        }

        // One root holding CHILDREN references, each child holding one more
        // object, so a stolen entry is itself worth scanning.
        let root = g1.alloc_array(ClassId::new(6), ArrayElementType::Reference, CHILDREN);
        for i in 0..CHILDREN {
            let child = g1.alloc_object(ClassId::new(7), 1);
            let leaf = g1.alloc_object(ClassId::new(8), 0);
            g1.set_field(child, 0, Value::Object(Some(leaf)));
            g1.set_array_element(root, i, Value::Object(Some(child)))
                .expect("array store");
        }

        g1.start_concurrent_mark(&stw());
        g1.remark(&stw(), &[root]);

        let controller = ConcurrentMarkController::spawn(Arc::clone(&g1));
        for _ in 0..5_000 {
            if controller.is_quiesced() {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        controller.request_stop_and_join().expect("workers joined");

        let scans = g1.dbg_mark_worker_scans();
        let busy = scans.iter().filter(|&&n| n > 0).count();
        let steals = g1.dbg_mark_steals();
        if steals > 0 && busy >= 2 {
            return;
        }
        last = Some((scans, busy, steals));
    }

    let (scans, busy, steals) = last.expect("at least one attempt ran");
    assert!(
        steals > 0,
        "no worker ever stole in {ATTEMPTS} attempts; last per-worker scans {scans:?}. A {CHILDREN}-wide frontier starts life on ONE worker's deque, so zero steals means the peers had no route to the work at all (F-12)"
    );
    assert!(
        busy >= 2,
        "only {busy} of {} markers scanned anything ({scans:?}) in {ATTEMPTS} attempts - the deques exist but the work never spread (F-12)",
        scans.len()
    );
}

/// F-12 — no marker may report convergence while work is still outstanding.
///
/// A DEEP chain is the adversarial shape for a parallel terminator: at any
/// instant only one worker has anything to do, so every peer is repeatedly
/// asking "is it over?" about a cycle that has barely started. Marking is
/// otherwise indistinguishable from the serial case, so nothing but the
/// termination decision is under test here.
///
/// The assertion is deliberately not "marking finished" — it is
/// "everything reachable was marked by the time the markers SAID they had
/// converged", read BEFORE the workers are stopped, because stopping them
/// performs a final drain that would repair a premature verdict and hide
/// it. The coordinator runs the STW remark and then cleanup on that
/// verdict, so a premature one frees a live chain.
///
/// Verified capable of failing: making `gray_set_is_empty` consult only the
/// seed queue — which is exactly what the pre-F-12 code did, and the
/// obvious way to get this wrong, because the seed queue empties the moment
/// the first worker takes its chunk — leaves 61,856 of the 100,000 chain
/// objects unmarked at the reported convergence.
///
/// The `mark_active` term of the same test could NOT be falsified this way,
/// and that is worth writing down rather than leaving as a passing test:
/// the window it closes is when a worker holds popped work in a local
/// vector (between a successful steal and extending its own deque), and a
/// batch holds the deque lock for 32 objects, so a peer probing emptiness
/// almost always samples a batch boundary where the deque is non-empty. The
/// term stays because the window is real, not because a test found it.
#[test]
fn a_deep_chain_is_fully_marked_before_the_markers_report_convergence() {
    const LINKS: usize = 100_000;
    let g1 = parallel_mark_collector();
    if g1.mark_worker_count() < 2 {
        eprintln!("[F-12] parallel termination not exercised: 1 worker on this machine");
        return;
    }

    let head = g1.alloc_object(ClassId::new(21), 1);
    let mut chain = vec![head];
    let mut prev = head;
    for _ in 1..LINKS {
        let next = g1.alloc_object(ClassId::new(21), 1);
        g1.set_field(prev, 0, Value::Object(Some(next)));
        chain.push(next);
        prev = next;
    }

    g1.start_concurrent_mark(&stw());
    g1.remark(&stw(), &[head]);

    let controller = ConcurrentMarkController::spawn(Arc::clone(&g1));
    let mut converged = false;
    for _ in 0..2_000 {
        if controller.is_quiesced() {
            converged = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    // Read the marks BEFORE stopping the workers: stopping performs a final
    // drain, which would mark whatever a premature verdict had left behind
    // and hide the very defect this test exists for.
    let unmarked: Vec<usize> = chain
        .iter()
        .map(|o| o.as_ptr() as usize)
        .filter(|&a| !g1.dbg_is_marked(a))
        .collect();
    controller.request_stop_and_join().expect("workers joined");

    assert!(converged, "the markers never reported convergence");
    assert!(
        unmarked.is_empty(),
        "{} of {LINKS} chain objects were still unmarked when the markers reported convergence — a worker declared a fixed point with gray work still outstanding (F-12)",
        unmarked.len()
    );
}

/// Test #5 — the worker observes `request_stop` even when there is
/// no work to do (parked state). Regression guard for a missed cvar
/// notification.
#[test]
fn worker_stops_promptly_when_parked() {
    let g1 = small_collector();
    // Don't seed roots → worklist is empty → worker parks immediately.
    g1.start_concurrent_mark(&stw());
    let controller = ConcurrentMarkController::spawn(Arc::clone(&g1));

    // Give it time to park.
    std::thread::sleep(Duration::from_millis(20));

    // Now request stop. Even though the worklist is empty, the
    // worker must wake and exit within a few poll intervals.
    //
    // The bound is derived from `WORKER_POLL_MS` rather than written as a
    // literal, and it is deliberately loose. What this test can actually
    // distinguish is "the stop request wakes the parked worker" from "the
    // worker never notices" — a hang is unbounded, so any finite bound
    // catches it. What it CANNOT distinguish on a loaded host is one poll
    // interval from two: this is wall-clock time on a thread the OS is
    // free not to schedule, and at 500 ms (two poll intervals) it failed
    // at 1.58 s while five other builds were running in this tree. A test
    // that fails on machine load rather than on behaviour teaches readers
    // to re-run rather than to read, which is worse than no test.
    //
    // Eight poll intervals keeps the assertion meaningful — a worker that
    // only learned of the stop from a timeout-driven re-poll would have to
    // miss eight of them — while leaving room for a scheduler that is busy.
    let start = std::time::Instant::now();
    controller.request_stop_and_join().expect("worker joined");
    let elapsed = start.elapsed();
    let bound = Duration::from_millis(WORKER_POLL_MS * 8);
    assert!(
        elapsed < bound,
        "worker did not stop promptly when parked (took {elapsed:?}, bound {bound:?})"
    );
}

/// Test #6 — performing a young collection while the mark thread is
/// running does not cause the worker to dereference stale pointers.
/// The young GC patches the mark worklist via the fix in
/// `young_collection` (round-7 audit §12); the worker may already
/// hold the region lock when the patch happens.
#[test]
fn worker_survives_young_gc_during_concurrent_mark() {
    let g1 = small_collector();
    let a = g1.alloc_object(ClassId::new(1), 1);
    let b = g1.alloc_object(ClassId::new(2), 0);
    g1.set_field(a, 0, Value::Object(Some(b)));

    g1.start_concurrent_mark(&stw());
    g1.remark(&stw(), &[a]);

    let controller = ConcurrentMarkController::spawn(Arc::clone(&g1));
    std::thread::sleep(Duration::from_millis(10));

    // Trigger a young GC. This rewrites the mark worklist's stale
    // pointers; the worker must not crash on resume.
    let mut roots = vec![a];
    let _ = g1.young_collection(&mut roots, &NoopMonitors);

    std::thread::sleep(Duration::from_millis(10));

    // Worker must still join cleanly. A panic in the worker would
    // surface here as Err.
    let result = controller.request_stop_and_join();
    assert!(result.is_ok(), "worker panicked: {:?}", result.err());
}
