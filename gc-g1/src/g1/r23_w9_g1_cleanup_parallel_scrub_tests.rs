use super::*;

const NODES: usize = 3000;

/// A heap of linked legacy nodes over several regions, every third one
/// marked live; `scrub` scrubs it. Returns each node's link, as the
/// index of the node it names (the two runs' heaps sit at different
/// addresses), and its second field.
fn scrub_and_read(
    scrub: impl Fn(&G1Collector, &mut Vec<G1Region>, &[(usize, usize, usize)]),
) -> Vec<(Result<usize, Value>, Value)> {
    let gc = G1Collector::new(G1CollectorConfig {
        heap_size: 16 * 1024 * 1024,
        initial_heap_size: 16 * 1024 * 1024,
        region_size: 1024 * 1024,
        gc_worker_threads: 4,
        ..G1CollectorConfig::default()
    });
    let mut nodes: Vec<ObjectRef> = Vec::with_capacity(NODES);
    for i in 0..NODES {
        // 2 KB of padding per node so the heap spans several regions.
        let node = gc.alloc_object(ClassId::new(1), 128);
        if let Some(&prev) = nodes.last() {
            gc.set_field(node, 0, Value::Object(Some(prev)));
        }
        gc.set_field(node, 1, Value::Int(i as i32));
        nodes.push(node);
    }
    let mut regions = gc.regions.write();
    for (i, &n) in nodes.iter().enumerate() {
        if i % 3 == 0 {
            let addr = n.as_ptr() as usize;
            let idx = gc.lookup_region_for_addr(addr).expect("in the heap");
            regions[idx].mark_bitmap.try_mark(addr);
        }
    }
    let mut work: Vec<(usize, usize, usize)> = Vec::new();
    for (idx, r) in regions.iter().enumerate() {
        let tams = r.tams();
        if tams > 0 && r.region_type() != RegionType::Free {
            work.push((idx, r.data.as_ptr() as usize, tams));
        }
    }
    assert!(work.len() >= 3, "the fixture spans several regions");
    scrub(&gc, &mut regions, &work);
    drop(regions);
    let indices: std::collections::HashMap<usize, usize> = nodes
        .iter()
        .enumerate()
        .map(|(i, n)| (n.as_ptr() as usize, i))
        .collect();
    nodes
        .iter()
        .map(|&n| {
            // The two scrubs run on separately allocated heaps. Preserve the
            // value tag and exact referent identity, comparing node indices
            // rather than unrelated reservation addresses.
            let link = match gc.get_field(n, 0) {
                Value::Object(Some(target)) => Ok(*indices
                    .get(&(target.as_ptr() as usize))
                    .expect("a retained link targets this graph")),
                other => Err(other),
            };
            (link, gc.get_field(n, 1))
        })
        .collect()
}

#[test]
fn r23_w9_the_parallel_cleanup_scrub_leaves_the_serial_heap() {
    let serial = scrub_and_read(|_gc, regions, work| {
        for &(idx, base, tams) in work {
            // SAFETY: the test holds the write guard; `tams` is the cursor.
            unsafe { scrub_dead_refs_below_tams(&regions[idx], base, tams, &[]) };
        }
    });
    let parallel = scrub_and_read(|gc, regions, work| {
        gc.cleanup_scrub_regions_parallel(regions, work, &[]);
    });
    assert_eq!(serial.len(), NODES);
    assert_eq!(parallel, serial, "the same heap after either scrub");
    // And the scrub did something: a dead node lost its link, a live one
    // kept it.
    assert_eq!(
        serial[1].0,
        Err(Value::Object(None)),
        "dead node 1: link nulled"
    );
    assert_eq!(
        serial[3].0,
        Ok(2),
        "live node 3 keeps its exact link to node 2"
    );
}
