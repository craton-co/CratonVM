use super::*;

const MIB: usize = 1024 * 1024;

struct Monitors;
impl MonitorCleanup for Monitors {
    fn remap_after_gc(&self, _pointer_map: &cratonvm_types::PointerMap) {}
}

/// 32 regions of 1 MiB, all committed, one worker, `promotion_age` 15.
fn collector() -> G1Collector {
    G1Collector::new(G1CollectorConfig {
        heap_size: 32 * MIB,
        initial_heap_size: 32 * MIB,
        region_size: MIB,
        gc_worker_threads: 1,
        max_gc_pause_ms: 200,
        promotion_age: 15,
        ..G1CollectorConfig::default()
    })
}

fn region_type_of(gc: &G1Collector, obj: ObjectRef) -> RegionType {
    let regions = gc.regions.read();
    let idx = gc
        .region_for_ptr(&regions, obj.as_ptr())
        .expect("the object is in a region");
    regions[idx].region_type()
}

/// A rooted `holder` naming a `child` (payload 77), both in Eden.
fn fixture(gc: &G1Collector) -> Vec<ObjectRef> {
    let holder = gc.alloc_object(ClassId::new(1), 2);
    let child = gc.alloc_object(ClassId::new(1), 2);
    gc.set_field(child, 0, Value::Int(77));
    gc.set_field(holder, 0, Value::Object(Some(child)));
    vec![holder]
}

/// Through `collect_garbage`, as the door's pause runs it: under the
/// switch the holder and the child are in Old regions after one pause and
/// no Survivor region is left; switch off they are Survivor copies, as
/// before. The pause-scoped flag is cleared on exit.
#[test]
fn gci_i47w_sg47_the_system_gc_pause_tenures_every_survivor() {
    {
        let on = true;
        {
                let gc = collector();
                let mut roots = fixture(&gc);
                cratonvm_gc_core::gc_quiescence::request_system_gc_tenure();
                assert_eq!(cratonvm_gc_core::gc_quiescence::system_gc_tenure_requested(), on);
                // SAFETY: single-threaded unit test, the heap is quiescent.
                let stw = unsafe { cratonvm_gc_core::collector::StopTheWorldToken::new() };
                let _ = gc.collect_garbage(&stw, &mut roots, &Monitors);
                let _ = cratonvm_gc_core::gc_quiescence::take_system_gc_tenure_request();
                let child = gc
                    .get_field(roots[0], 0)
                    .as_object()
                    .expect("the holder still names the child");
                assert_eq!(gc.get_field(child, 0).as_int(), Some(77), "on={on}");
                let want = if on { RegionType::Old } else { RegionType::Survivor };
                assert_eq!(region_type_of(&gc, roots[0]), want, "on={on}: holder");
                assert_eq!(region_type_of(&gc, child), want, "on={on}: child");
                // Survivor regions holding any bytes (a claimed-but-empty
                // PLAB destination is not a young object).
                let survivor_regions = gc
                    .regions
                    .read()
                    .iter()
                    .filter(|r| r.region_type() == RegionType::Survivor && r.cursor() > 0)
                    .count();
                assert_eq!(survivor_regions == 0, on, "on={on}: {survivor_regions}");
                assert!(
                    !gc.pause_tenures_all.load(Ordering::Relaxed),
                    "on={on}: cleared when the pause returns"
                );
                assert_eq!(gc.evac_tenuring_threshold(), gc.tenuring_threshold());        }        }    }

/// Both evacuators read the pause flag: the serial driver and the
/// parallel one (its per-pause `promotion_age` snapshot).
#[test]
fn gci_i47w_sg47_both_evacuators_honour_the_pause_flag() {
    for parallel in [false, true] {
        let gc = collector();
        let mut roots = fixture(&gc);
        gc.pause_tenures_all.store(true, Ordering::Relaxed);
        assert_eq!(gc.evac_tenuring_threshold(), 0);
        if parallel {
            let _ = gc.young_collection_parallel(&mut roots, &Monitors);
        } else {
            let _ = gc.young_collection_serial(&mut roots, &Monitors);
        }
        gc.pause_tenures_all.store(false, Ordering::Relaxed);
        let child = gc.get_field(roots[0], 0).as_object().expect("child");
        assert_eq!(gc.get_field(child, 0).as_int(), Some(77));
        assert_eq!(region_type_of(&gc, roots[0]), RegionType::Old, "parallel={parallel}");
        assert_eq!(region_type_of(&gc, child), RegionType::Old, "parallel={parallel}");
    }
}
