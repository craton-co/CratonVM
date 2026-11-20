use super::*;

struct NoopMonitors;
impl MonitorCleanup for NoopMonitors {
    fn remap_after_gc(&self, _pointer_map: &cratonvm_types::PointerMap) {}
}

#[test]
#[ignore]
fn in_place_scale_keeplong_shape() {
    let gc = G1Collector::new(G1CollectorConfig {
        heap_size: 1024 * 1024 * 1024,
        initial_heap_size: 1024 * 1024 * 1024,
        region_size: 1024 * 1024,
        gc_worker_threads: 8,
        promotion_age: 1,
        ..G1CollectorConfig::default()
    });
    const CHAINS: usize = 64;
    let per_round: usize = std::env::var("SCALE_NODES").ok().and_then(|v| v.parse().ok()).unwrap_or(1_000_000);
    let mut roots: Vec<ObjectRef> = Vec::new();
    for round in 0..2 {
        let ring = gc.alloc_object(ClassId::new(1), CHAINS);
        if round == 0 {
            roots.push(ring);
        } else {
            roots[0] = ring;
        }
        for i in 0..per_round {
            let c = i % CHAINS;
            let node = gc.alloc_object(ClassId::new(1), 2);
            let payload = gc.alloc_array(ClassId::new(9), ArrayElementType::Byte, 224);
            let prev = gc.get_field(roots[0], c);
            gc.set_field(node, 0, prev);
            gc.set_field(node, 1, Value::Object(Some(payload)));
            gc.set_field(roots[0], c, Value::Object(Some(node)));
            if i % 200_000 == 199_999 {
                gc.pause_tenures_all.store(true, Ordering::Relaxed);
                let _ = gc.young_collection_parallel(&mut roots, &NoopMonitors);
                gc.pause_tenures_all.store(false, Ordering::Relaxed);
            }
        }
        gc.pause_tenures_all.store(true, Ordering::Relaxed);
        let _ = gc.young_collection_parallel(&mut roots, &NoopMonitors);
        gc.pause_tenures_all.store(false, Ordering::Relaxed);
    }
    let old = gc.count_regions(RegionType::Old);
    let t0 = std::time::Instant::now();
    let r = gc.full_compaction_in_place(&mut roots, &NoopMonitors);
    let dt = t0.elapsed();
    match r {
        Ok(res) => eprintln!(
            "in-place scale: old {old}->{} in {:?}: moved={}MB reclaimed={}MB forwards={}",
            gc.count_regions(RegionType::Old),
            dt,
            res.stats.bytes_copied >> 20,
            res.stats.bytes_freed >> 20,
            res.pointer_map.len()
        ),
        Err(e) => eprintln!("in-place scale: declined {e} after {dt:?}"),
    }
}
