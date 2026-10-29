// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `G1Collector::needs_gc`'s recount cadence: correctness of the thread-local
//! counter, and the in-process A/B that retired
//! `docs/feature-designs/concurrent-gc-maturation.md`.
//!
//! The A/B runs both arms in ONE process, over ONE collector, interleaved
//! round by round, so host load moves both arms alike: arm `shared` adds the
//! collector-wide `fetch_add` the old `needs_gc` began with, arm `local` is the
//! shipped path. It is `#[ignore]`d (a timing run, not a gate) and prints one
//! line per thread count; run it with
//! `cargo test -p cratonvm-gc --release --lib needs_gc_scaling -- --ignored --nocapture`.

use super::*;
use std::sync::atomic::AtomicUsize;

fn collector() -> G1Collector {
    G1Collector::new(G1CollectorConfig {
        heap_size: 64 * 1024 * 1024,
        initial_heap_size: 64 * 1024 * 1024,
        region_size: 1024 * 1024,
        max_gc_pause_ms: 200,
        ihop_percent: 45,
        promotion_age: 3,
        gc_worker_threads: 1,
        string_dedup_enabled: false,
        mixed_gc_count_target: 8,
        old_cset_region_threshold_percent: 10,
        mixed_gc_live_threshold_percent: 85,
        heap_waste_percent: 0,
    })
}

/// The cadence is per thread: each thread's own `INTERVAL`-th query is the
/// one that recounts, whatever other threads do in between.
#[test]
fn the_recount_cadence_is_per_thread() {
    let handles: Vec<_> = (0..4)
        .map(|_| {
            std::thread::spawn(|| {
                let mut due = 0usize;
                for _ in 0..(NEEDS_GC_RECOUNT_INTERVAL * 3) {
                    if needs_gc_recount_due() {
                        due += 1;
                    }
                }
                due
            })
        })
        .collect();
    for h in handles {
        assert_eq!(
            h.join().unwrap(),
            3,
            "exactly one recount per interval per thread"
        );
    }
}

/// `needs_gc` answers the same through the thread-local cadence as through a
/// fresh recount, on a heap whose cache is exact.
#[test]
fn needs_gc_agrees_with_a_recount_on_an_exact_cache() {
    let gc = collector();
    let cached = gc.free_region_count.load(Ordering::Relaxed);
    assert_eq!(cached, gc.recount_free_regions());
    let first = <G1Collector as GarbageCollector>::needs_gc(&gc);
    for _ in 0..(NEEDS_GC_RECOUNT_INTERVAL * 2) {
        assert_eq!(<G1Collector as GarbageCollector>::needs_gc(&gc), first);
    }
}

/// Median ns per `needs_gc` query for `threads` threads, `iters` queries each,
/// with (`shared_rmw`) or without the old collector-wide counter.
fn run_arm(
    gc: &G1Collector,
    shared: &AtomicUsize,
    threads: usize,
    iters: usize,
    shared_rmw: bool,
) -> f64 {
    let start = std::sync::Barrier::new(threads + 1);
    let elapsed = std::thread::scope(|s| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                let start = &start;
                s.spawn(move || {
                    start.wait();
                    let t0 = std::time::Instant::now();
                    let mut hits = 0usize;
                    for _ in 0..iters {
                        if shared_rmw {
                            // The pre-change first statement of `needs_gc`.
                            std::hint::black_box(shared.fetch_add(1, Ordering::Relaxed));
                        }
                        if <G1Collector as GarbageCollector>::needs_gc(gc) {
                            hits += 1;
                        }
                    }
                    std::hint::black_box(hits);
                    t0.elapsed()
                })
            })
            .collect();
        start.wait();
        handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .max()
            .unwrap()
    });
    elapsed.as_nanos() as f64 / iters as f64
}

#[test]
#[ignore = "timing A/B; run explicitly with --ignored --nocapture"]
fn needs_gc_shared_rmw_vs_thread_local_scaling() {
    let gc = collector();
    let shared = AtomicUsize::new(0);
    let rounds: usize = std::env::var("G1_NEEDS_GC_AB_ROUNDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(15);
    let iters: usize = std::env::var("G1_NEEDS_GC_AB_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2_000_000);
    let cpus = std::thread::available_parallelism().map_or(1, |n| n.get());
    for threads in [1usize, 2, 4, 8].into_iter().filter(|&t| t <= cpus.max(1)) {
        let mut local = Vec::with_capacity(rounds);
        let mut rmw = Vec::with_capacity(rounds);
        for round in 0..rounds {
            // Alternate which arm goes first so drift cannot favour one.
            if round % 2 == 0 {
                local.push(run_arm(&gc, &shared, threads, iters, false));
                rmw.push(run_arm(&gc, &shared, threads, iters, true));
            } else {
                rmw.push(run_arm(&gc, &shared, threads, iters, true));
                local.push(run_arm(&gc, &shared, threads, iters, false));
            }
        }
        let median = |v: &mut Vec<f64>| {
            v.sort_by(|a, b| a.partial_cmp(b).unwrap());
            v[v.len() / 2]
        };
        let (min_l, max_l) = (
            local.iter().cloned().fold(f64::MAX, f64::min),
            local.iter().cloned().fold(0.0, f64::max),
        );
        let (min_r, max_r) = (
            rmw.iter().cloned().fold(f64::MAX, f64::min),
            rmw.iter().cloned().fold(0.0, f64::max),
        );
        let ml = median(&mut local);
        let mr = median(&mut rmw);
        println!(
            "[needs-gc-ab] threads={threads} rounds={rounds} iters={iters} \
             local_ns_median={ml:.2} (min {min_l:.2} max {max_l:.2}) \
             shared_rmw_ns_median={mr:.2} (min {min_r:.2} max {max_r:.2}) \
             rmw_over_local={:.2}x",
            mr / ml
        );
    }
}

/// Every `runtime_var_os(<key>)` call in `src` whose result is read by
/// presence alone (`.is_some()` / `.is_none()` right after the call), as the
/// call text. The key is one string literal, so the call's first `)` closes it.
fn gci_i5gg1f_presence_reads(src: &str) -> Vec<String> {
    let needle = "runtime_var_os(";
    let mut out = Vec::new();
    let mut rest = src;
    while let Some(at) = rest.find(needle) {
        let tail = &rest[at..];
        if let Some(close) = tail.find(')') {
            let after: String = tail[close + 1..]
                .chars()
                .filter(|c| !c.is_whitespace())
                .take(10)
                .collect();
            if after.starts_with(".is_some()") || after.starts_with(".is_none()") {
                out.push(tail[..close + 1].to_string());
            }
        }
        rest = &tail[needle.len()..];
    }
    out
}

/// gci i5g/g1f -- page `gci-i3fl-gc-typed-flag-fields-and-shared-readers-still-parse-presence-CLOSED-20261014`,
/// item 3, G1 half. No G1 source reads a switch by presence (`NAME=0`,
/// `=false`, `=off` and `=` turned it ON), and no default-on G1 kill switch is
/// parsed `!= "0"` (`=false` / `=off` / `=no` left it ON). Opt-ins read
/// `runtime_flag_on`, default-on switches `runtime_flag_default_on`.
#[test]
fn gci_i5gg1f_g1_sources_read_no_switch_by_presence() {
    for (name, src) in [
        ("g1.rs", include_str!("g1.rs")),
        ("g1_overlay.rs", include_str!("g1_overlay.rs")),
        ("g1_cards.rs", include_str!("g1_cards.rs")),
        ("g1_concurrent.rs", include_str!("g1_concurrent.rs")),
    ] {
        let presence = gci_i5gg1f_presence_reads(src);
        assert!(
            presence.is_empty(),
            "{name}: presence-only switch reads (use runtime_flag_on / \
             runtime_flag_default_on): {presence:?}"
        );
        let zero_only = src.matches("v.trim() != \"0\"").count();
        assert_eq!(
            zero_only, 0,
            "{name}: a default-on switch parsed `!= \"0\"` leaves `=false` / `=off` ON; \
             use runtime_flag_default_on"
        );
    }
}

/// The scanner itself: a presence read is found across a line break, a value
/// read is not.
#[test]
fn gci_i5gg1f_presence_scanner_finds_presence_and_only_presence() {
    let presence = "x(|| flags::runtime_var_os(\"K\")\n        .is_some())";
    let absence = "x(|| flags::runtime_var_os(\"K\").is_none())";
    let value = "flags::runtime_var_os(\"K\").and_then(|v| v.into_string().ok())";
    assert_eq!(gci_i5gg1f_presence_reads(presence).len(), 1);
    assert_eq!(gci_i5gg1f_presence_reads(absence).len(), 1);
    assert!(gci_i5gg1f_presence_reads(value).is_empty());
}
