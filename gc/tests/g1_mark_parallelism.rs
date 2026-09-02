// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! F-12 throughput probe: how long does one G1 concurrent-mark cycle take on
//! one worker versus several?
//!
//! `#[ignore]`d, so it never runs in the ordinary suite. It exists because the
//! finding F-12 addresses is invisible to a correctness test: the same objects
//! end up marked either way. What changes is the DURATION, and mark duration is
//! not only a CPU cost — it sets how much headroom the IHOP heuristic has to
//! leave before starting a cycle, so a slow marker is paid for in heap.
//!
//! # Running it
//!
//! `cratonvm_types::flags()` reads the environment once per process, so the two
//! arms are two processes:
//!
//! ```text
//! CRATONVM_G1_PARALLEL_MARK=1 cargo test -p cratonvm-gc --release --test g1_mark_parallelism -- --ignored --nocapture
//! CRATONVM_G1_PARALLEL_MARK=0 cargo test -p cratonvm-gc --release --test g1_mark_parallelism -- --ignored --nocapture
//! ```
//!
//! # Reading it honestly
//!
//! The probe prints the worker count it actually got, because a run where that
//! is 1 in both arms is measuring nothing — `concurrent_mark_worker_count` is a
//! quarter of the evacuation width, so a machine with few cores, or a stray
//! `CRATONVM_G1_WORKERS=1`, collapses both arms onto the same code path. It also
//! prints the per-worker scan split, since a parallel marker whose work all
//! landed on worker 0 is a serial marker with extra locks, and the wall clock
//! alone cannot tell you which you measured.
//!
//! The graph is deliberately WIDE-then-deep: a wide frontier is what gives the
//! workers something to divide, and the per-child chains are what make each
//! stolen entry worth more than the steal that fetched it. A pure chain would
//! measure the termination protocol rather than the parallelism.
//!
//! Run each arm several times, interleaved, and compare the best of each; a
//! shared developer machine's load changes the absolute numbers by more than
//! this change does.

use std::sync::Arc;
use std::time::{Duration, Instant};

use cratonvm_gc::collector::{GarbageCollector, StopTheWorldToken};
use cratonvm_gc::g1::G1CollectorConfig;
use cratonvm_gc::{ConcurrentMarkController, G1Collector};
use cratonvm_types::{ClassId, Value};

/// Test-only STW token: the graph is built before any marker exists.
#[inline]
fn stw() -> StopTheWorldToken {
    // SAFETY: the probe builds its graph single-threaded, before spawn.
    unsafe { StopTheWorldToken::new() }
}

#[test]
#[ignore = "throughput probe — run explicitly with --ignored"]
fn concurrent_mark_cycle_duration() {
    const BRANCHES: usize = 4_000;
    const DEPTH: usize = 40;

    let g1 = Arc::new(G1Collector::new(G1CollectorConfig {
        heap_size: 512 * 1024 * 1024,
        region_size: 1024 * 1024,
        ..Default::default()
    }));

    // One root object per branch, each the head of a DEPTH-long chain. Roots are
    // handed to `remark` directly, so the seed queue starts wide.
    let mut roots = Vec::with_capacity(BRANCHES);
    for _ in 0..BRANCHES {
        let head = g1.alloc_object(ClassId::new(31), 1);
        let mut prev = head;
        for _ in 1..DEPTH {
            let next = g1.alloc_object(ClassId::new(31), 1);
            g1.set_field(prev, 0, Value::Object(Some(next)));
            prev = next;
        }
        roots.push(head);
    }
    let objects = BRANCHES * DEPTH;

    g1.start_concurrent_mark(&stw());
    g1.remark(&stw(), &roots);

    let workers = g1.dbg_mark_worker_scans_public().len();
    let t0 = Instant::now();
    let controller = ConcurrentMarkController::spawn(Arc::clone(&g1));
    let mut converged = false;
    for _ in 0..60_000 {
        if controller.is_quiesced() {
            converged = true;
            break;
        }
        std::thread::sleep(Duration::from_micros(200));
    }
    let elapsed = t0.elapsed();
    let scans = g1.dbg_mark_worker_scans_public();
    let steals = g1.dbg_mark_steals_public();
    controller.request_stop_and_join().expect("workers joined");

    assert!(converged, "marking never converged");
    eprintln!(
        "[F-12 probe] workers={workers} objects={objects} elapsed={elapsed:?} \
         per_object={:.1}ns steals={steals} scans={scans:?}",
        elapsed.as_nanos() as f64 / objects as f64
    );
}
