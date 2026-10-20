// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! gci i25/pl -- unit tests for the Survivor-PLAB copy path's per-copy
//! `parallel_copy_end` publish (`SharedEvac::evacuate`,
//! `plab_copy_end_needs_publish`, `CRATONVM_G1_PLAB_COPY_END_ELIDE`), page
//! `gci-i24ym-g1-survivor-plab-arm-promoting-pause-is-slower-per-byte-RETIRED-UNFIXED-20261018`.
//!
//! A child module of `g1` (see the `#[path]` declaration in `g1.rs`), so the
//! private items are reachable through `super`.
//!
//! Filter: `cargo test -j 5 -p cratonvm-gc --lib gci_i25_pl_`.

use super::*;
use crate::collector::{GarbageCollector, MonitorCleanup};
use cratonvm_types::{ArrayElementType, ClassId, Value};

const MIB: usize = 1024 * 1024;

struct NoopMonitors;
impl MonitorCleanup for NoopMonitors {
    fn remap_after_gc(&self, _pointer_map: &cratonvm_types::PointerMap) {}
}

/// Eight workers, 96 one-MiB regions, the whole heap committed (as the
/// `mod tests` `parallel_config` the PLAB tests of i14/i15/i16 use).
fn collector() -> G1Collector {
    G1Collector::new(G1CollectorConfig {
        heap_size: 96 * MIB,
        initial_heap_size: 96 * MIB,
        region_size: MIB,
        max_gc_pause_ms: 200,
        ihop_percent: 45,
        promotion_age: 3,
        gc_worker_threads: 8,
        string_dedup_enabled: false,
        mixed_gc_count_target: 8,
        old_cset_region_threshold_percent: 10,
        mixed_gc_live_threshold_percent: 85,
        heap_waste_percent: 0,
    })
}

/// `n` int arrays of ~1 KiB each (about 31 to a 32 KiB PLAB chunk, ~3 MiB in
/// all, so the shared Survivor region switches several times per pause),
/// held by one reference array. Element 0 of array `i` is `i`, the last is
/// `!i`.
fn build(gc: &G1Collector, n: usize) -> ObjectRef {
    let root = gc.alloc_array(ClassId::new(0), ArrayElementType::Reference, n);
    for i in 0..n {
        let a = gc.alloc_array(ClassId::new(0), ArrayElementType::Int, 256);
        gc.set_array_element(a, 0, Value::Int(i as i32)).unwrap();
        gc.set_array_element(a, 255, Value::Int(!(i as i32))).unwrap();
        gc.set_array_element(root, i, Value::Object(Some(a))).unwrap();
    }
    root
}

/// Every array keeps its identity and both ends of its body, and no two
/// elements share a copy.
fn verify(gc: &G1Collector, root: ObjectRef, n: usize, round: usize) {
    let mut seen: std::collections::HashSet<usize> = std::collections::HashSet::new();
    for i in 0..n {
        let a = match gc.get_array_element(root, i) {
            Ok(Value::Object(Some(a))) => a,
            other => panic!("round {round}: element {i} lost -> {other:?}"),
        };
        assert_eq!(
            gc.get_array_element(a, 0).ok().and_then(|v| v.as_int()),
            Some(i as i32),
            "round {round}: element {i} names the wrong array"
        );
        assert_eq!(
            gc.get_array_element(a, 255).ok().and_then(|v| v.as_int()),
            Some(!(i as i32)),
            "round {round}: element {i} lost the end of its body"
        );
        assert!(seen.insert(a.as_ptr() as usize), "round {round}: two elements share one copy");
    }
}

/// `(regions, regions whose parallel_copy_end is non-zero)` over the Survivor
/// regions the last pause left.
fn survivor_copy_end_census(gc: &G1Collector) -> (usize, usize) {
    let regions = gc.regions.read();
    let surv: Vec<usize> = regions
        .iter()
        .filter(|r| r.region_type() == RegionType::Survivor)
        .map(|r| r.parallel_copy_end())
        .collect();
    (surv.len(), surv.iter().filter(|&&e| e > 0).count())
}

/// The rule: only a copy into a shared PLAB chunk, with the switch on, skips
/// the publish. A per-worker TLAB (Old, or Survivor with the PLAB off) always
/// publishes, because its cursor is written back only at retirement.
#[test]
fn gci_i25_pl_copy_end_publish_rule() {
    assert!(!plab_copy_end_needs_publish(true, true), "shared chunk, elide on: skipped");
    assert!(plab_copy_end_needs_publish(true, false), "per-worker TLAB: published");
    assert!(plab_copy_end_needs_publish(false, true), "=0 restores the publish");
    assert!(plab_copy_end_needs_publish(false, false));
}

/// Default (`CRATONVM_G1_PLAB_COPY_END_ELIDE` unset) with the PLAB on: over
/// two parallel pauses no Survivor region's watermark moves -- the copies in
/// shared chunks are covered by the region cursor their chunk claim advanced
/// -- and every copy keeps its identity and body. The census counts chunks,
/// so the PLAB arm really ran.
#[test]
fn gci_i25_pl_shared_plab_copies_skip_the_copy_end_publish() {
    cratonvm_types::flags::with_thread_overrides(
        &[
            ("CRATONVM_G1_SURVIVOR_PLAB", Some("1")),
            ("CRATONVM_G1_PLAB_COPY_END_ELIDE", None),
        ],
        || {
            let gc = collector();
            let n = 3000usize;
            let mut roots = vec![build(&gc, n)];
            for round in 0..2usize {
                let _ = gc.young_collection_parallel(&mut roots, &NoopMonitors);
                verify(&gc, roots[0], n, round);
                let (surv, published) = survivor_copy_end_census(&gc);
                // Round 1 may promote every survivor (the adaptive tenuring
                // threshold and the Survivor region cap are free to), so only
                // the first pause must leave a Survivor region.
                assert!(round > 0 || surv >= 1, "round {round}: no Survivor region");
                assert_eq!(
                    published, 0,
                    "round {round}: {published} of {surv} shared Survivor regions published a copy end"
                );
            }
            let (_, chunks, _) = gc.survivor_plab_census();
            assert!(chunks > 0, "the PLAB arm did not run");
        },
    );
}

/// The kill: `CRATONVM_G1_PLAB_COPY_END_ELIDE=0` restores the per-copy
/// publish on the shared chunks (the A/B arm of `vi25-pl.list`), and the
/// copies are just as intact.
#[test]
fn gci_i25_pl_elide_off_restores_the_copy_end_publish() {
    cratonvm_types::flags::with_thread_overrides(
        &[
            ("CRATONVM_G1_SURVIVOR_PLAB", Some("1")),
            ("CRATONVM_G1_PLAB_COPY_END_ELIDE", Some("0")),
        ],
        || {
            let gc = collector();
            let n = 3000usize;
            let mut roots = vec![build(&gc, n)];
            let _ = gc.young_collection_parallel(&mut roots, &NoopMonitors);
            verify(&gc, roots[0], n, 0);
            let (surv, published) = survivor_copy_end_census(&gc);
            assert!(published >= 1, "=0: {published} of {surv} Survivor regions published");
        },
    );
}

/// The per-worker arm (`CRATONVM_G1_SURVIVOR_PLAB=0`) is untouched by the
/// switch: its TLABs own their regions and publish the cursor only at
/// retirement, so the watermark is still published per copy.
#[test]
fn gci_i25_pl_per_worker_survivor_tlabs_still_publish() {
    cratonvm_types::flags::with_thread_overrides(
        &[
            ("CRATONVM_G1_SURVIVOR_PLAB", Some("0")),
            ("CRATONVM_G1_PLAB_COPY_END_ELIDE", None),
        ],
        || {
            let gc = collector();
            let n = 3000usize;
            let mut roots = vec![build(&gc, n)];
            let _ = gc.young_collection_parallel(&mut roots, &NoopMonitors);
            verify(&gc, roots[0], n, 0);
            let (surv, published) = survivor_copy_end_census(&gc);
            assert!(surv >= 1 && published >= 1, "PLAB off: {published} of {surv} published");
        },
    );
}
