// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! gci i26/pc -- unit tests for the parallel copy's per-worker survivor-age and
//! promoted-bytes counters (`TlabSet::note_survivor_age`,
//! `TlabSet::note_promoted_bytes`, `TlabSet::flush_copy_counters`, called from
//! `SharedEvac::retire_all`), page
//! `gci-i25pl-g1-parallel-copy-updates-two-global-counters-per-object-FIXED-20261017`.
//!
//! A child module of `g1` (see the `#[path]` declaration in `g1.rs`), so the
//! private items are reachable through `super`.
//!
//! Filter: `cargo test -j 5 -p cratonvm-gc --lib gci_i26_pc_`.

use super::*;
use crate::collector::{GarbageCollector, MonitorCleanup};
use cratonvm_types::{ArrayElementType, ClassId, Value};

const MIB: usize = 1024 * 1024;

struct NoopMonitors;
impl MonitorCleanup for NoopMonitors {
    fn remap_after_gc(&self, _pointer_map: &cratonvm_types::PointerMap) {}
}

/// Eight workers, 96 one-MiB regions, the whole heap committed, promotion at
/// age 2 so a few pauses exercise both the Survivor and the Old arm.
fn collector() -> G1Collector {
    G1Collector::new(G1CollectorConfig {
        heap_size: 96 * MIB,
        initial_heap_size: 96 * MIB,
        region_size: MIB,
        promotion_age: 2,
        gc_worker_threads: 8,
        string_dedup_enabled: false,
        ..Default::default()
    })
}

fn histogram(gc: &G1Collector) -> [usize; G1_MAX_TENURING_AGE] {
    std::array::from_fn(|i| gc.survivor_age_bytes[i].load(Ordering::Relaxed))
}

/// The accounting identity, without a pause: the same copy stream counted
/// per copy into the collector (the old path, still the serial evacuator's)
/// and counted into eight per-worker sets then flushed (the parallel path)
/// gives the same histogram and the same promoted total -- including ages
/// past the last bucket, which both clamp into it. A second flush adds
/// nothing (a helper slot is retired by its worker and again by the driver's
/// sweep).
#[test]
fn gci_i26_pc_worker_flush_equals_per_copy_notes() {
    let direct = collector();
    let flushed = collector();
    let mut sets: Vec<TlabSet> = (0..8).map(|_| TlabSet::default()).collect();
    // Deterministic copy stream: (worker, promoted?, age, bytes).
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    for _ in 0..20_000 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let worker = (x % 8) as usize;
        let promote = (x >> 8) % 5 == 0;
        let age = ((x >> 16) % 20) as u8; // 16..19 clamp into bucket 15
        let bytes = 16 + 8 * ((x >> 24) % 200) as usize;
        if promote {
            direct.note_promoted_bytes(bytes);
            sets[worker].note_promoted_bytes(bytes);
        } else {
            direct.note_survivor_age(age, bytes);
            sets[worker].note_survivor_age(age, bytes);
        }
    }
    for s in sets.iter_mut() {
        s.flush_copy_counters(&flushed);
    }
    assert_eq!(histogram(&flushed), histogram(&direct), "age histogram differs");
    assert_eq!(
        flushed.bytes_promoted_total(),
        direct.bytes_promoted_total(),
        "promoted total differs"
    );
    assert!(direct.bytes_promoted_total() > 0, "stream promoted nothing");
    assert!(histogram(&direct)[G1_MAX_TENURING_AGE - 1] > 0, "no clamped age reached the last bucket");

    // Flushed sets are zeroed; a second flush is a no-op.
    for s in sets.iter_mut() {
        assert!(s.age_bytes.iter().all(|&b| b == 0) && s.promoted_bytes == 0);
        s.flush_copy_counters(&flushed);
    }
    assert_eq!(histogram(&flushed), histogram(&direct), "second flush moved the histogram");
    assert_eq!(flushed.bytes_promoted_total(), direct.bytes_promoted_total());
}

/// `n` int arrays of ~1 KiB each held by one reference array.
fn build(gc: &G1Collector, n: usize) -> ObjectRef {
    let root = gc.alloc_array(ClassId::new(0), ArrayElementType::Reference, n);
    for i in 0..n {
        let a = gc.alloc_array(ClassId::new(0), ArrayElementType::Int, 256);
        gc.set_array_element(a, 0, Value::Int(i as i32)).unwrap();
        gc.set_array_element(root, i, Value::Object(Some(a))).unwrap();
    }
    root
}

/// Per pause, the bytes the pause copied are exactly the bytes its age
/// histogram holds plus the bytes it promoted: every winning copy is counted
/// once, into exactly one of the two, by both drivers. On the parallel driver
/// this is the proof that every `TlabSet` was flushed before the pause's
/// readers ran (a missed flush loses bytes, a double flush doubles them).
fn assert_pause_identity(parallel: bool) {
    let gc = collector();
    let n = 3000usize;
    let mut roots = vec![build(&gc, n)];
    let (mut any_hist, mut any_promoted) = (false, false);
    for round in 0..4usize {
        // Fresh young garbage and fresh survivors each round.
        for _ in 0..2000 {
            let junk = gc.alloc_object(ClassId::new(23), 2);
            gc.set_field(junk, 0, Value::Int(0));
        }
        let before = gc.bytes_promoted_total();
        let r = if parallel {
            gc.young_collection_parallel(&mut roots, &NoopMonitors)
        } else {
            gc.young_collection_serial(&mut roots, &NoopMonitors)
        };
        let promoted = gc.bytes_promoted_total() - before;
        // `update_tenuring_threshold_merging` snapshotted this pause's table
        // into `last_survivor_age_bytes` and cleared the live one.
        let hist: usize = gc.tenuring_state().1.iter().sum();
        assert_eq!(
            hist as u64 + promoted,
            r.stats.bytes_copied as u64,
            "parallel={parallel} round {round}: histogram {hist} + promoted {promoted} != copied {}",
            r.stats.bytes_copied
        );
        assert!(
            histogram(&gc).iter().all(|&b| b == 0),
            "parallel={parallel} round {round}: live histogram not consumed by the pause"
        );
        any_hist |= hist > 0;
        any_promoted |= promoted > 0;
    }
    assert!(any_hist, "parallel={parallel}: no pause copied to Survivor");
    assert!(any_promoted, "parallel={parallel}: no pause promoted");
    // The graph is intact.
    for i in 0..n {
        let Ok(Value::Object(Some(a))) = gc.get_array_element(roots[0], i) else {
            panic!("parallel={parallel}: element {i} lost");
        };
        assert_eq!(gc.get_array_element(a, 0).ok().and_then(|v| v.as_int()), Some(i as i32));
    }
}

#[test]
fn gci_i26_pc_parallel_pause_histogram_plus_promoted_equals_bytes_copied() {
    assert_pause_identity(true);
}

#[test]
fn gci_i26_pc_serial_pause_histogram_plus_promoted_equals_bytes_copied() {
    assert_pause_identity(false);
}
