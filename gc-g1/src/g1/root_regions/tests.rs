use super::*;

struct NoMonitors;
impl MonitorCleanup for NoMonitors {
    fn remap_after_gc(&self, _pointer_map: &cratonvm_types::PointerMap) {}
}

fn stw() -> cratonvm_gc_core::collector::StopTheWorldToken {
    // SAFETY: single-threaded unit test; no mutator runs.
    unsafe { cratonvm_gc_core::collector::StopTheWorldToken::new() }
}

fn collector() -> G1Collector {
    G1Collector::new(G1CollectorConfig {
        heap_size: 16 * 1024 * 1024,
        initial_heap_size: 16 * 1024 * 1024,
        region_size: 1024 * 1024,
        promotion_age: 4,
        gc_worker_threads: 1,
        ..G1CollectorConfig::default()
    })
}

fn region_type_of(gc: &G1Collector, obj: ObjectRef) -> RegionType {
    let idx = gc.lookup_region_for_addr(obj.as_ptr() as usize).expect("in the heap");
    gc.regions.read()[idx].region_type()
}

fn marked(gc: &G1Collector, obj: ObjectRef) -> bool {
    let addr = obj.as_ptr() as usize;
    let idx = gc.lookup_region_for_addr(addr).expect("in the heap");
    gc.regions.read()[idx].mark_bitmap.is_marked(addr)
}

/// `(old, survivor)`: an Old object reachable only through a Survivor
/// object, which a root names.
fn old_behind_a_survivor(gc: &G1Collector) -> (ObjectRef, ObjectRef) {
    let old = gc.alloc_object(ClassId::new(0x7A01), 1);
    let mut roots = vec![old];
    for _ in 0..6 {
        gc.young_collection(&mut roots, &NoMonitors);
    }
    let old = roots[0];
    assert_eq!(region_type_of(gc, old), RegionType::Old, "fixture: promoted");
    let holder = gc.alloc_object(ClassId::new(0x7A02), 1);
    gc.set_field(holder, 0, Value::Object(Some(old)));
    let mut roots = vec![holder];
    gc.young_collection(&mut roots, &NoMonitors);
    let holder = roots[0];
    assert_eq!(region_type_of(gc, holder), RegionType::Survivor, "fixture: a survivor");
    (old, holder)
}





#[test]
fn g1ep7_a_reserved_tail_in_a_young_region_declines() {
    let gc = collector();
    let obj = gc.alloc_object(ClassId::new(0x7A03), 1);
    let addr = obj.as_ptr() as usize;
    let regions = gc.regions.read();
    assert!(parse_young_regions(&regions, &[]).is_some_and(|o| o.iter().any(|&(a, _)| a == addr)));
    assert!(parse_young_regions(&regions, &[(addr, addr + 64)]).is_none());
}
