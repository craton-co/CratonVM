use super::*;

struct NoopMonitors;
impl MonitorCleanup for NoopMonitors {
    fn remap_after_gc(&self, _pointer_map: &cratonvm_types::PointerMap) {}
}

#[test]
fn r23_w6_parallel_evacuation_keeps_every_chain_node() {
    let gc = G1Collector::new(G1CollectorConfig {
        heap_size: 16 * 1024 * 1024,
        initial_heap_size: 16 * 1024 * 1024,
        region_size: 1024 * 1024,
        gc_worker_threads: 4,
        ..G1CollectorConfig::default()
    });
    const CHAINS: usize = 64;
    const DEPTH: usize = 150;
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
    let mut roots: Vec<ObjectRef> = vec![ring];
    let result = gc.young_collection_parallel(&mut roots, &NoopMonitors);
    let ring = roots[0];
    let mut seen = 0usize;
    for c in 0..CHAINS {
        let mut cur = gc.get_field(ring, c);
        let mut d = DEPTH;
        while let Value::Object(Some(node)) = cur {
            d -= 1;
            assert_eq!(
                gc.get_field(node, 1),
                Value::Int((c * DEPTH + d) as i32),
                "chain {c}: node {d} is the one that was linked there"
            );
            seen += 1;
            cur = gc.get_field(node, 0);
        }
        assert_eq!(d, 0, "chain {c} lost {d} node(s)");
    }
    assert_eq!(seen, CHAINS * DEPTH);
    assert!(
        result.pointer_map.len() > CHAINS * DEPTH,
        "every node (and the ring) has a forward: {}",
        result.pointer_map.len()
    );
}
