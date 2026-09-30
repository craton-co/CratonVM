// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Long-running leak soak — Enhancement #2 from
//! `review-2026-05-24/gc.md` §2.3.
//!
//! Allocates ~10k small objects per second for 5 minutes (≈ 3M
//! allocations), periodically triggering minor GC. The contract under
//! test:
//!
//!   1. Every allocation returns a valid (non-null, header-bearing)
//!      `ObjectRef`. A double-free or use-after-free in the Cheney
//!      copy path would surface here long before the test finishes.
//!   2. Live-set size (the `young_from_used()` reading sampled after
//!      each GC) stays bounded — specifically, never exceeds 2× its
//!      steady-state mean. A leak in pointer remapping, a forgotten
//!      root, or a stale dirty-card entry would manifest as monotonic
//!      growth.
//!
//! # `#[ignore]`-gated
//!
//! This test takes 5 minutes wall-clock and is not run by default.
//! Opt in with:
//!
//! ```text
//! cargo test --release -p cratonvm-gc -- --ignored leak_soak
//! ```
//!
//! # RSS check — best-effort
//!
//! The original review §2.3 wording asks for RSS bounded within 2× of
//! steady state. We do not depend on `sysinfo` or `procfs` (would
//! pull a transitive that's not in workspace deps) — instead we use
//! `young_from_used()` + `old_gen_used()` as a proxy for the heap's
//! own bookkeeping. Allocator-level RSS that exceeds heap-tracked
//! used bytes is a glibc/`mimalloc` artifact, not a GC bug, and is
//! out of scope for this test. If the heap reports bounded usage but
//! `top` shows growing RSS, that is a separate issue tracked under
//! the allocator-fragmentation work.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use cratonvm_gc::collector::MonitorCleanup;
use cratonvm_gc::GenerationalHeap;
use cratonvm_types::{ClassId, ObjectRef, Value};

/// No-op monitor cleanup — the soak does not exercise monitors.
struct NoMonitors;
impl MonitorCleanup for NoMonitors {
    fn remap_after_gc(&self, _pointer_map: &cratonvm_types::PointerMap) {}
}

/// Soak parameters. Sized so the test wall-clock fits in 5 min on a
/// typical CI agent. With a 64 MB young gen and ~64-byte objects the
/// allocator can sustain ~10k alloc/s while running ~12 GC cycles
/// per minute. If your local machine is faster, the test still
/// passes — the assertions are sound regardless of throughput.
const SOAK_DURATION: Duration = Duration::from_secs(5 * 60);
const ALLOCS_PER_BATCH: usize = 1_000;
const BATCH_INTERVAL: Duration = Duration::from_millis(100); // 10 batches/s × 1000 = 10k/s
const GC_EVERY_N_BATCHES: usize = 50; // GC every ~5 s
/// Live-set size cap as a multiple of the steady-state mean we
/// measure during the first quarter of the run. The 2× factor
/// matches the review's "bounded within 2× steady-state" wording.
const LIVE_SET_MULTIPLIER: u64 = 2;

#[test]
#[ignore = "5-minute soak — run with `cargo test --release -- --ignored leak_soak`"]
fn gc_soak_5min_no_leak() {
    // 64 MB total heap so the young gen has room for ~250k 64-byte
    // objects between GCs — well above per-batch live count.
    let heap = Arc::new(GenerationalHeap::with_capacity(64 * 1024 * 1024));
    let class_id = ClassId::new(1);

    // Permanent roots — survive every GC. Keep small so the bulk of
    // each batch is short-lived garbage that the collector should
    // reclaim. The 16 here is arbitrary; what matters is that some
    // roots are pinned across the entire soak so we can verify their
    // payloads survive.
    let mut perma_roots: Vec<ObjectRef> = (0..16).map(|_| heap.alloc_object(class_id, 4)).collect();
    // Tag each permanent root with a known sentinel in field 0 so we
    // can verify GC did not corrupt its payload across cycles.
    for (i, r) in perma_roots.iter().enumerate() {
        heap.set_field(*r, 0, Value::Int(0xDEAD_0000u32 as i32 + i as i32));
    }

    // Track the per-batch live-set size (heap-reported `used`) so we
    // can compute a steady-state mean and assert no leak.
    let mut live_samples: Vec<u64> = Vec::new();
    let alloc_count = AtomicU64::new(0);

    let start = Instant::now();
    let mut batch_idx: usize = 0;
    while start.elapsed() < SOAK_DURATION {
        // Allocate a batch of garbage. We do NOT hold references to
        // these objects — they become dead immediately and should be
        // reclaimed at the next GC.
        for _ in 0..ALLOCS_PER_BATCH {
            let r = heap.alloc_object(class_id, 4);
            // Touch the header to verify it's a valid allocation —
            // tweaking field 0 also forces the write barrier.
            heap.set_field(r, 0, Value::Int(0x4242));
            alloc_count.fetch_add(1, Ordering::Relaxed);
        }

        batch_idx += 1;

        if batch_idx % GC_EVERY_N_BATCHES == 0 {
            // Pass `perma_roots` as the only roots — every allocated
            // batch object is unreachable and must be collected.
            // SAFETY: this stress test runs the heap single-threaded.
            let stw = unsafe { cratonvm_gc::collector::StopTheWorldToken::new_unchecked() };
            let _ = heap.collect_garbage(&stw, &mut perma_roots, &NoMonitors);

            // Verify permanent root payloads are intact (no
            // corruption across the cycle).
            for (i, r) in perma_roots.iter().enumerate() {
                match heap.get_field(*r, 0) {
                    Value::Int(v) => {
                        let expected = 0xDEAD_0000u32 as i32 + i as i32;
                        assert_eq!(
                            v, expected,
                            "perma root {} payload corrupted after batch {}: got {:#x}, want {:#x}",
                            i, batch_idx, v, expected
                        );
                    }
                    other => panic!(
                        "perma root {} field 0 unexpected type after batch {}: {:?}",
                        i, batch_idx, other
                    ),
                }
            }

            // Record post-GC live set. With only `perma_roots`
            // reachable, this should be ~16 * (HEADER + 4 slots)
            // ≈ 1.5 KB regardless of how many batches preceded.
            let live = (heap.young_from_used() + heap.old_gen_used()) as u64;
            live_samples.push(live);
        }

        // Pace the allocator so we hit ~10k allocs/s and the test
        // takes the expected wall-clock duration. Without this the
        // loop saturates a core and the soak finishes in seconds —
        // not the duration the review §2.3 enhancement intends.
        std::thread::sleep(BATCH_INTERVAL);
    }

    let total_allocs = alloc_count.load(Ordering::Relaxed);
    assert!(
        total_allocs >= 100_000,
        "soak ran too short: only {} allocs in {:?}",
        total_allocs,
        start.elapsed()
    );

    // Compute steady-state mean from the first quarter of samples
    // (after the GC warm-up has stabilized) and assert no later
    // sample exceeds LIVE_SET_MULTIPLIER × that mean.
    assert!(
        live_samples.len() >= 8,
        "too few live-set samples ({}): GC_EVERY_N_BATCHES tuning bug?",
        live_samples.len()
    );
    let warmup_n = live_samples.len() / 4;
    let warmup_mean: u64 = live_samples[..warmup_n].iter().sum::<u64>() / warmup_n as u64;
    let leak_threshold = warmup_mean * LIVE_SET_MULTIPLIER;
    for (i, &sample) in live_samples.iter().enumerate().skip(warmup_n) {
        assert!(
            sample <= leak_threshold,
            "live-set leak detected at sample {}: {} bytes (warm-up mean {} bytes, \
             threshold {} bytes). Full samples: {:?}",
            i,
            sample,
            warmup_mean,
            leak_threshold,
            live_samples
        );
    }

    eprintln!(
        "leak_soak OK: {} total allocs, {} samples, warm-up mean {} bytes, \
         max post-warmup sample {} bytes (threshold {} bytes)",
        total_allocs,
        live_samples.len(),
        warmup_mean,
        live_samples
            .iter()
            .skip(warmup_n)
            .copied()
            .max()
            .unwrap_or(0),
        leak_threshold,
    );
}
