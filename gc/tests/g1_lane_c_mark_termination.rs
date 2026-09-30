// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Lane C — the parallel marker's termination protocol must not be able to
//! strand gray work where no worker can reach it.
//!
//! # The defect this pins
//!
//! `G1Collector::take_marking_work` stole HALF of a peer's deque and required
//! that peer to hold at least two entries (`theirs.len() >= 2`) — a guard
//! against thief/victim ping-pong. Halving bottoms out at one, and a deque
//! holding exactly one entry failed that guard for every thief forever.
//!
//! So a worker that stopped while still holding work left behind an entry no
//! other worker could ever take, and `gray_set_is_empty()` stayed false for the
//! rest of the cycle. That is not a throughput problem. The production final
//! remark is
//!
//! ```text
//! while !state.collector.concurrent_mark_step(usize::MAX) {}
//! ```
//!
//! (`VmHeap::g1_final_remark_and_cleanup`), which drives **worker 0**. Worker 0
//! drains its own deque, fails to steal the singleton, breaks out of its batch
//! loop, reports "gray set non-empty ⇒ not converged", and is called again.
//! With the world stopped.
//!
//! Stopping a worker mid-frontier is ordinary rather than exotic:
//! `ConcurrentMarkController::worker_loop`'s final pass after it observes
//! `should_stop` is bounded by `WORKER_STEP_BUDGET` (256 objects) and then the
//! thread returns, so any stop issued while a marker is descending a wide
//! frontier leaves the remainder behind.
//!
//! # Why the reproduction is deterministic
//!
//! It does not race a background thread into the state. `concurrent_mark_step_worker`
//! is `pub`, so the test loads worker 1's deque directly with a bounded step and
//! then asks worker 0 to drain to a fixed point — which is exactly the shape the
//! production driver produces, with the scheduler taken out of it.
//!
//! The drain runs on its own thread against a deadline, because the pre-fix
//! behaviour is an infinite loop and a test that reproduces it by hanging the
//! harness reports nothing. A leaked spinning thread dies with the process.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use cratonvm_gc::collector::{GarbageCollector, StopTheWorldToken};
use cratonvm_gc::g1::G1CollectorConfig;
use cratonvm_gc::G1Collector;
use cratonvm_types::{ClassId, Value};

/// Test-only STW witness (I-17): this file drives the mark cycle by hand with
/// no mutators running, so the invariant the token stands for holds trivially.
#[inline]
fn stw() -> StopTheWorldToken {
    // SAFETY: single-threaded test driver; no mutator is executing.
    unsafe { StopTheWorldToken::new() }
}

fn collector() -> Arc<G1Collector> {
    Arc::new(G1Collector::new(G1CollectorConfig {
        heap_size: 64 * 1024 * 1024,
        region_size: 1024 * 1024,
        ..Default::default()
    }))
}

#[test]
fn a_singleton_peer_deque_cannot_wedge_the_final_remark_drain() {
    const CHILDREN: usize = 4_000;

    let g1 = collector();
    // `dbg_mark_worker_scans_public` is sized by `mark_deques`, which is the
    // number this test needs: with one deque there is no peer to strand work
    // on and nothing here is exercised.
    let workers = g1.dbg_mark_worker_scans_public().len();
    if workers < 2 {
        eprintln!("[lane-c] one marking worker on this machine — nothing to strand");
        return;
    }

    // A wide frontier under a single root. The root is the only seed, so
    // whichever worker pops it owns the entire frontier on ITS deque — which is
    // precisely the state a stop leaves behind.
    let root = g1.alloc_array(
        ClassId::new(41),
        cratonvm_gc::heap::ArrayElementType::Reference,
        CHILDREN,
    );
    for i in 0..CHILDREN {
        let child = g1.alloc_object(ClassId::new(42), 0);
        g1.set_array_element(root, i, Value::Object(Some(child)))
            .expect("array store");
    }

    g1.start_concurrent_mark(&stw());
    g1.remark(&stw(), &[root]);

    // Worker 1 takes the root from the shared seed queue, scans it, and fills
    // its OWN deque with the frontier — then runs out of budget and stops,
    // exactly as `worker_loop` does on a stop request. Several small steps so
    // the root is definitely scanned and the deque definitely non-empty.
    for _ in 0..4 {
        let _ = g1.concurrent_mark_step_worker(1, 8);
    }

    // Now the production final-remark drain, on worker 0. Pre-fix this never
    // returns: worker 0 halves worker 1's deque down to one entry and then has
    // no route to it.
    let done = Arc::new(AtomicBool::new(false));
    {
        let g1 = Arc::clone(&g1);
        let done = Arc::clone(&done);
        std::thread::Builder::new()
            .name("lane-c-final-remark-drain".into())
            .spawn(move || {
                while !g1.concurrent_mark_step(usize::MAX) {}
                done.store(true, Ordering::Release);
            })
            .expect("drain thread");
    }

    // Generous against a loaded CI host, and still three orders of magnitude
    // short of "never".
    let deadline = Instant::now() + Duration::from_secs(20);
    while !done.load(Ordering::Acquire) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        done.load(Ordering::Acquire),
        "the final-remark drain never reached a fixed point: a peer worker's deque \
         holds gray work worker 0 has no route to, so `concurrent_mark_step` reports \
         `false` forever and `VmHeap::g1_final_remark_and_cleanup` spins inside the \
         stop-the-world pause"
    );

    // And the work was not merely abandoned — the drain must have SCANNED it.
    let scans = g1.dbg_mark_worker_scans_public();
    let total: usize = scans.iter().sum();
    assert!(
        total >= CHILDREN,
        "the drain converged without scanning the frontier ({total} scans over \
         {CHILDREN} children, split {scans:?})"
    );
}

/// The complementary property, stated on its own because the fix above could
/// have been written as "steal everything, always" and that would have passed
/// the test above while destroying the parallelism F-12 exists for.
///
/// A BULK victim must still be split rather than emptied: the thief takes half
/// and the victim keeps half, so both have something to descend into.
#[test]
fn a_bulk_victim_is_split_not_emptied() {
    const CHILDREN: usize = 4_000;

    let g1 = collector();
    let workers = g1.dbg_mark_worker_scans_public().len();
    if workers < 2 {
        eprintln!("[lane-c] one marking worker on this machine — no stealing to observe");
        return;
    }

    let root = g1.alloc_array(
        ClassId::new(43),
        cratonvm_gc::heap::ArrayElementType::Reference,
        CHILDREN,
    );
    for i in 0..CHILDREN {
        // Each child holds one more object, so a stolen entry is itself worth
        // scanning and the split shows up in the per-worker scan split.
        let child = g1.alloc_object(ClassId::new(44), 1);
        let leaf = g1.alloc_object(ClassId::new(45), 0);
        g1.set_field(child, 0, Value::Object(Some(leaf)));
        g1.set_array_element(root, i, Value::Object(Some(child)))
            .expect("array store");
    }

    g1.start_concurrent_mark(&stw());
    g1.remark(&stw(), &[root]);

    // Load worker 1's deque with the whole frontier.
    for _ in 0..4 {
        let _ = g1.concurrent_mark_step_worker(1, 8);
    }
    let before = g1.dbg_mark_steals_public();

    // One bounded step on worker 0: it has an empty deque, the seed queue is
    // empty, so its only route is a steal — and the victim holds thousands of
    // entries, so the halving arm must be the one that fires.
    let _ = g1.concurrent_mark_step_worker(0, 16);
    assert!(
        g1.dbg_mark_steals_public() > before,
        "worker 0 had no work of its own and a bulk victim beside it, and did not steal"
    );

    // Worker 1 must still have work: a thief that emptied it would have taken
    // the whole frontier and serialised the mark.
    let mut worker1_made_progress = false;
    for _ in 0..4 {
        let scans_before = g1.dbg_mark_worker_scans_public()[1];
        let _ = g1.concurrent_mark_step_worker(1, 16);
        if g1.dbg_mark_worker_scans_public()[1] > scans_before {
            worker1_made_progress = true;
            break;
        }
    }
    assert!(
        worker1_made_progress,
        "the victim was left with nothing to do after being stolen from — the split \
         degenerated into a hand-over and the marker is serial again"
    );
}
