// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Lane D wave 2 — the census's seed/closure split is a PARTITION of the
//! bytes a pause copied, not two overlapping tallies.
//!
//! # Why this is its own test binary
//!
//! The census is a process-lifetime table (`g1::g1_evac_worker_census`) and
//! libtest runs the `#[test]`s of one binary on SEVERAL THREADS AT ONCE. Any
//! assertion of the form "this pause added exactly these bytes to the census"
//! is therefore an assertion about test scheduling in a shared binary, and the
//! first draft of this check failed exactly that way: it read a delta of
//! 425,760 bytes for a pause that copied 131,040, because three other tests
//! had collected in between.
//!
//! `cargo test` gives each `tests/*.rs` file its own process, and this file
//! holds exactly one test. That is what makes the delta below mean what it
//! says — it is the whole census, for the whole process, against the whole of
//! the one pause that produced it.
//!
//! # The property
//!
//! The split exists because the driver is the only thread that can evacuate
//! roots and keep-alives, so folding those into the closure half would make
//! every pause read as driver-heavy whether or not the closure was (see
//! `g1::EvacWorkerCensus`). For that split to be readable it has to be a
//! partition: every byte the census attributes to a worker is a byte the pause
//! actually copied, counted once.
//!
//! The bound is `<=`, not `==`, and that is not slack. The serial
//! self-forward drain runs on the driver AFTER the census is published — it
//! has to, because publishing any later would report the shape of the shard
//! merge rather than the shape of the pause — so its bytes are in the pause's
//! total and legitimately not in any worker's row. An excess in the other
//! direction has only one cause: a worker's bytes counted in two rows.

use cratonvm_gc::collector::{GarbageCollector, MonitorCleanup};
use cratonvm_gc::g1::G1CollectorConfig;
use cratonvm_gc::G1Collector;
use cratonvm_types::{ClassId, ObjectRef, Value};

struct NoopMonitors;
impl MonitorCleanup for NoopMonitors {
    fn remap_after_gc(&self, _pointer_map: &cratonvm_types::PointerMap) {}
}

fn config() -> G1CollectorConfig {
    G1CollectorConfig {
        heap_size: 64 * 1024 * 1024,
        initial_heap_size: 64 * 1024 * 1024,
        region_size: 1024 * 1024,
        ..Default::default()
    }
}

const FANOUT: usize = 4;
const FIELDS: usize = FANOUT + 1;
const ID_SLOT: usize = FANOUT;

fn build_tree(gc: &G1Collector, depth: u32) -> (ObjectRef, usize) {
    fn rec(gc: &G1Collector, depth: u32, next: &mut i32) -> ObjectRef {
        let node = gc.alloc_object(ClassId::new(1), FIELDS);
        let id = *next;
        *next += 1;
        gc.set_field(node, ID_SLOT, Value::Int(id));
        if depth > 0 {
            for slot in 0..FANOUT {
                let child = rec(gc, depth - 1, next);
                gc.set_field(node, slot, Value::Object(Some(child)));
            }
        }
        node
    }
    let mut next = 0;
    let root = rec(gc, depth, &mut next);
    (root, next as usize)
}

#[test]
fn the_seed_and_closure_halves_partition_the_copied_bytes() {
    let gc = G1Collector::new(config());
    let (root, count) = build_tree(&gc, 5);
    assert!(
        cratonvm_gc::g1::g1_evac_worker_census().is_empty(),
        "this binary must hold exactly ONE test, so that nothing has collected \
         before this line. The delta below is only meaningful against an empty \
         census"
    );

    let mut roots = vec![root];
    let result = gc.young_collection(&mut roots, &NoopMonitors);
    assert_eq!(
        result.stats.objects_copied, count,
        "the pause did not evacuate the tree"
    );

    let rows = cratonvm_gc::g1::g1_evac_worker_census();
    let attributed: u64 = rows.iter().map(|r| r.total_bytes()).sum();
    assert!(
        attributed > 0,
        "the pause copied {} bytes and the census attributed none of them to \
         any worker",
        result.stats.bytes_copied,
    );
    assert!(
        attributed <= result.stats.bytes_copied as u64,
        "the census attributed {attributed} bytes across {} worker rows, but \
         the pause only copied {}. The seed half and the closure half are \
         overlapping rather than partitioning — check the latch in \
         `parallel_evacuate` that snapshots `seed_objs`/`seed_bytes` before \
         the Phase-3 dispatch",
        rows.len(),
        result.stats.bytes_copied,
    );
    // THE STRONG FORM, and the one that actually answers "does the census
    // triple-count?".
    //
    // The only legitimate reason `attributed` can be less than the pause total
    // is the serial self-forward drain, and that runs only when an allocation
    // failed — which is exactly what `PARALLEL_TLAB_POOL_EXHAUSTED` counts. On
    // a pause that never exhausted its pool there is no drain, so the two
    // numbers must be EQUAL to the byte. Anything else is a miscount, and the
    // direction says which kind: greater means a copy credited to two workers
    // (the seed half and the closure half both claiming it, or a CAS loser's
    // abandoned copy credited beside the winner's); smaller means a worker's
    // shard was merged into the driver before the census read it.
    let exhausted =
        cratonvm_gc::g1::PARALLEL_TLAB_POOL_EXHAUSTED.load(std::sync::atomic::Ordering::Relaxed);
    if exhausted == 0 {
        assert_eq!(
            attributed, result.stats.bytes_copied as u64,
            "the pause exhausted no pool, so no serial self-forward drain ran \
             and the census must account for EXACTLY the bytes the pause \
             copied. Rows: {rows:?}"
        );
    }

    // The same partition, stated per row: a row's closure half is its total
    // minus its seed half, and neither can be negative. `total_bytes` is the
    // sum, so this is really a check that the subtraction in the publish used
    // `saturating_sub` against the RIGHT snapshot rather than a later one.
    for (i, r) in rows.iter().enumerate() {
        assert_eq!(
            r.total_bytes(),
            r.seed_bytes + r.bytes_copied,
            "worker {i}'s row does not add up: {r:?}"
        );
    }
}
