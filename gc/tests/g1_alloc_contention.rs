// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! F-11 throughput probe: how much does G1's allocator cost when N threads use
//! it at once?
//!
//! `#[ignore]`d, so it never runs in the ordinary suite. It exists because the
//! finding F-11 addresses is invisible to a correctness test: before the change
//! every object allocation and every TLAB refill took the collector's ONE
//! exclusive lock, and after it the common case is a compare-exchange under a
//! shared guard. Both produce identical objects. The only difference is what
//! happens when more than one thread allocates, and that has to be measured.
//!
//! # Running it
//!
//! The arms differ by an environment variable, and `cratonvm_types::flags()`
//! reads the environment once per process, so the two arms are two processes:
//!
//! ```text
//! CRATONVM_G1_SHARED_ALLOC=1 cargo test -p cratonvm-gc --test g1_alloc_contention -- --ignored --nocapture
//! CRATONVM_G1_SHARED_ALLOC=0 cargo test -p cratonvm-gc --test g1_alloc_contention -- --ignored --nocapture
//! ```
//!
//! # Reading it honestly
//!
//! This is a same-binary A/B, which is the only kind worth quoting: one build,
//! one machine, one workload, and a single environment variable between the two
//! runs. It is still a wall-clock number on a shared developer machine, so the
//! load at the time is part of the result — the probe prints the thread count
//! and the per-allocation cost, and a pair of runs whose ratio is roughly the
//! thread count is measuring contention, while a pair that differ by less than
//! run-to-run noise is measuring the host. Run each arm several times and take
//! the best of each: the best case is the one least polluted by other work.
//!
//! The probe deliberately does NOT collect. A collection is stop-the-world and
//! takes the exclusive guard either way, so including one would dilute exactly
//! the thing being measured. The heap is sized so the whole run fits.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::time::Instant;

use cratonvm_gc::g1::G1CollectorConfig;
use cratonvm_gc::G1Collector;

/// Threads to run. Overridable so one machine can produce a scaling curve.
/// Deliberately NOT a `CRATONVM_*` name: this is probe scaffolding, not part of
/// the VM's configuration surface, and the flag census would have to carry it.
fn threads() -> usize {
    std::env::var("G1_PROBE_THREADS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8)
}

fn config(heap_mb: usize) -> G1CollectorConfig {
    G1CollectorConfig {
        heap_size: heap_mb * 1024 * 1024,
        region_size: 1024 * 1024,
        ..Default::default()
    }
}

/// Per-object allocation from N threads. Exercises `alloc_in_region`.
#[test]
#[ignore = "throughput probe — run explicitly with --ignored"]
fn object_allocation_throughput() {
    const PER_THREAD: usize = 150_000;

    let n = threads();
    // Object size matters here for a reason worth stating: at 32 bytes two
    // objects share a cache line, so N threads bumping ONE region's cursor
    // write the same lines at once. That false sharing is a property of the
    // workload, not of the lock, and it is what a TLAB exists to remove — so
    // the probe lets you vary it rather than baking one answer in.
    let obj_bytes: usize = std::env::var("G1_PROBE_OBJ_BYTES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(32);
    let gc = Arc::new(G1Collector::new(config(512)));
    let barrier = Arc::new(Barrier::new(n));
    let served = Arc::new(AtomicUsize::new(0));

    let start_gate = Arc::new(Barrier::new(n + 1));
    let workers: Vec<_> = (0..n)
        .map(|_| {
            let gc = Arc::clone(&gc);
            let barrier = Arc::clone(&barrier);
            let start_gate = Arc::clone(&start_gate);
            let served = Arc::clone(&served);
            std::thread::spawn(move || {
                start_gate.wait();
                barrier.wait();
                let mut got = 0usize;
                for _ in 0..PER_THREAD {
                    if gc.alloc_in_region(obj_bytes).is_some() {
                        got += 1;
                    }
                }
                served.fetch_add(got, Ordering::Relaxed);
            })
        })
        .collect();

    start_gate.wait();
    let t0 = Instant::now();
    for w in workers {
        w.join().expect("allocator thread");
    }
    let elapsed = t0.elapsed();

    let total = served.load(Ordering::Relaxed);
    assert_eq!(
        total,
        n * PER_THREAD,
        "the heap was too small for the probe"
    );
    eprintln!(
        "[F-11 probe] objects threads={n} bytes={obj_bytes} allocations={total} elapsed={:?} per_alloc={:.1}ns",
        elapsed,
        elapsed.as_nanos() as f64 / total as f64
    );
}

/// TLAB refills from N threads. Exercises `refill_tlab`, which is the site
/// F-11 names: at the 256 KiB default TLAB against 1 MiB regions, four refills
/// exhaust a region.
#[test]
#[ignore = "throughput probe — run explicitly with --ignored"]
fn tlab_refill_throughput() {
    // Default to the production TLAB size (`tlab::DEFAULT_TLAB_SIZE`): at
    // 256 KiB against 1 MiB regions, four refills exhaust a region, which is
    // the ratio F-11 names. Overridable, because the answer genuinely depends
    // on it — a small carve is dominated by the lock, a large one by the
    // zeroing, and only the large one is what the interpreter actually does.
    let tlab_bytes: usize = std::env::var("G1_PROBE_TLAB_BYTES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(256 * 1024);
    let n = threads();
    // Sized so the run fits the heap below with the TLAB reserve to spare.
    let per_thread = ((768 * 1024 * 1024) / tlab_bytes / n).max(1);
    let gc = Arc::new(G1Collector::new(config(1024)));
    let barrier = Arc::new(Barrier::new(n));
    let served = Arc::new(AtomicUsize::new(0));

    let start_gate = Arc::new(Barrier::new(n + 1));
    let workers: Vec<_> = (0..n)
        .map(|_| {
            let gc = Arc::clone(&gc);
            let barrier = Arc::clone(&barrier);
            let start_gate = Arc::clone(&start_gate);
            let served = Arc::clone(&served);
            std::thread::spawn(move || {
                start_gate.wait();
                barrier.wait();
                let mut got = 0usize;
                for _ in 0..per_thread {
                    if gc.refill_tlab(tlab_bytes).is_some() {
                        got += 1;
                    }
                }
                served.fetch_add(got, Ordering::Relaxed);
            })
        })
        .collect();

    start_gate.wait();
    let t0 = Instant::now();
    for w in workers {
        w.join().expect("refill thread");
    }
    let elapsed = t0.elapsed();

    let total = served.load(Ordering::Relaxed);
    // A refill can legitimately be refused once the Free pool hits the TLAB
    // reserve, so this asserts the run was long enough to mean something rather
    // than that every request was served.
    assert!(
        total >= n * per_thread / 2,
        "only {total} of {} refills were served — the heap ran out too early for the probe to \
         say anything",
        n * per_thread
    );
    eprintln!(
        "[F-11 probe] tlabs threads={n} bytes={tlab_bytes} refills={total} elapsed={:?} per_refill={:.1}ns",
        elapsed,
        elapsed.as_nanos() as f64 / total as f64
    );
}
