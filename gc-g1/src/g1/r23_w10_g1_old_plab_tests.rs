use super::*;

struct NoopMonitors;
impl MonitorCleanup for NoopMonitors {
    fn remap_after_gc(&self, _pointer_map: &cratonvm_types::PointerMap) {}
}

const CHAINS: usize = 64;
const DEPTH: usize = 150;

/// One parallel young pause that tenures everything, over 64 chains under
/// one root array; returns (every node read back in order, Old regions
/// after the pause).
fn promoting_pause(overrides: &[(&str, Option<&str>)]) -> (Vec<i32>, usize) {
    cratonvm_types::flags::with_thread_overrides(overrides, || {
        let gc = G1Collector::new(G1CollectorConfig {
            heap_size: 16 * 1024 * 1024,
            initial_heap_size: 16 * 1024 * 1024,
            region_size: 1024 * 1024,
            gc_worker_threads: 4,
            ..G1CollectorConfig::default()
        });
        let ring = gc.alloc_object(ClassId::new(1), CHAINS);
        for c in 0..CHAINS {
            for d in 0..DEPTH {
                let node = gc.alloc_object(ClassId::new(1), 2);
                let prev = gc.get_field(ring, c);
                gc.set_field(node, 0, prev);
                gc.set_field(node, 1, Value::Int((c * DEPTH + d) as i32));
                gc.set_field(ring, c, Value::Object(Some(node)));
            }
        }
        let old_before = gc.count_regions(RegionType::Old);
        // Every copy is a promotion (`evac_tenuring_threshold` answers 0).
        gc.pause_tenures_all.store(true, Ordering::Relaxed);
        let mut roots = vec![ring];
        let _ = gc.young_collection_parallel(&mut roots, &NoopMonitors);
        gc.pause_tenures_all.store(false, Ordering::Relaxed);
        let ring = roots[0];
        let mut seen = Vec::with_capacity(CHAINS * DEPTH);
        for c in 0..CHAINS {
            let mut cur = gc.get_field(ring, c);
            while let Value::Object(Some(node)) = cur {
                let Value::Int(tag) = gc.get_field(node, 1) else {
                    panic!("chain {c}: a node lost its tag");
                };
                seen.push(tag);
                cur = gc.get_field(node, 0);
            }
        }
        (seen, gc.count_regions(RegionType::Old) - old_before)
    })
}


#[test]
fn r23_w10_the_pool_cap_quantum_follows_the_destination_model() {
    assert_eq!(evac_pool_regions_per_worker(false), EVAC_POOL_REGIONS_PER_WORKER);
    assert_eq!(evac_pool_regions_per_worker(true), EVAC_POOL_REGIONS_PER_WORKER_SHARED);
    // `KeepLong -Xmx1g`'s starved pool: 17 Free regions.
    assert_eq!((17 / evac_pool_regions_per_worker(false)).max(1), 2);
    assert_eq!((17 / evac_pool_regions_per_worker(true)).max(1), 8);
    // `-Xmx64m` near exhaustion: a pool of 3 stays at one worker.
    assert_eq!((3 / evac_pool_regions_per_worker(true)).max(1), 1);
}
