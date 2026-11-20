use super::*;

const SMALL_REGION: usize = MIN_REGION_SIZE; // 256 KB
const HEAP_SIZE: usize = SMALL_REGION * 8; // 8 regions

fn make_heap() -> RegionHeap {
    RegionHeap::new(HEAP_SIZE, SMALL_REGION)
}

#[test]
fn new_heap_all_free() {
    let heap = make_heap();
    assert_eq!(heap.num_regions(), 8);
    assert_eq!(heap.region_size(), SMALL_REGION);
    assert_eq!(heap.count_regions(RegionType::Free), 8);
    assert_eq!(heap.allocated_bytes(), 0);
}

#[test]
fn alloc_eden_basic() {
    let mut heap = make_heap();
    let ptr = heap.alloc_eden(64, 8).unwrap();
    assert!(!ptr.is_null());
    assert_eq!(heap.count_regions(RegionType::Eden), 1);
    assert_eq!(heap.allocated_bytes(), 64);
}

#[test]
fn alloc_eden_multiple_same_region() {
    let mut heap = make_heap();
    let p1 = heap.alloc_eden(64, 8).unwrap();
    let p2 = heap.alloc_eden(128, 8).unwrap();
    assert_ne!(p1, p2);
    // Both should be in the same Eden region
    assert_eq!(heap.count_regions(RegionType::Eden), 1);
    assert_eq!(heap.allocated_bytes(), 192);
}

#[test]
fn alloc_fills_region_spills_to_next() {
    let mut heap = make_heap();
    // Fill a region with many small allocations (staying under humongous threshold)
    let chunk = 1024; // 1 KB each
    let count = SMALL_REGION / chunk - 1; // nearly fill it
    for _ in 0..count {
        heap.alloc_eden(chunk, 8).unwrap();
    }
    assert_eq!(heap.count_regions(RegionType::Eden), 1);

    // Allocate more than the remaining space to force a new region
    heap.alloc_eden(chunk * 2, 8).unwrap();
    assert_eq!(heap.count_regions(RegionType::Eden), 2);
}

#[test]
fn alloc_humongous() {
    let mut heap = make_heap();
    // Object larger than region_size/2
    let huge_size = SMALL_REGION / 2 + 1;
    let ptr = heap.alloc_eden(huge_size, 8).unwrap();
    assert!(!ptr.is_null());
    assert_eq!(heap.count_regions(RegionType::HumongousStart), 1);
}

#[test]
fn alloc_humongous_multi_region() {
    let mut heap = make_heap();
    // Object spanning 2 full regions
    let huge_size = SMALL_REGION + 100;
    let ptr = heap.alloc_eden(huge_size, 8).unwrap();
    assert!(!ptr.is_null());
    assert_eq!(heap.count_regions(RegionType::HumongousStart), 1);
    assert_eq!(heap.count_regions(RegionType::HumongousContinuation), 1);
}

#[test]
fn region_index_for_address() {
    let heap = make_heap();
    let base = heap.base_addr();
    assert_eq!(heap.region_index_for(base), Some(0));
    assert_eq!(heap.region_index_for(base + SMALL_REGION), Some(1));
    assert_eq!(
        heap.region_index_for(base + SMALL_REGION * 7 + 100),
        Some(7)
    );
    assert_eq!(heap.region_index_for(base + HEAP_SIZE), None);
    assert_eq!(heap.region_index_for(0), None);
}

#[test]
fn write_barrier_cross_region() {
    let mut heap = make_heap();

    // Manually set up two Eden regions with allocations in each
    heap.regions[0].region_type = RegionType::Eden;
    heap.regions[0].top = 64;
    heap.regions[1].region_type = RegionType::Eden;
    heap.regions[1].top = 64;
    let p1 = heap.region_start(0) as *mut u8;
    let p2 = heap.region_start(1) as *mut u8;

    let addr1 = p1 as usize;
    let addr2 = p2 as usize;

    // These should be in different regions
    let r1 = heap.region_index_for(addr1).unwrap();
    let r2 = heap.region_index_for(addr2).unwrap();
    assert_ne!(r1, r2);

    // Write barrier: r1 stores ref to r2
    heap.write_barrier(addr1, addr2);
    assert_eq!(heap.regions[r2].rset.source_count(), 1);
}

#[test]
fn write_barrier_same_region_no_rset() {
    let mut heap = make_heap();
    let p1 = heap.alloc_eden(64, 8).unwrap();
    let p2 = heap.alloc_eden(64, 8).unwrap();

    let r1 = heap.region_index_for(p1 as usize).unwrap();
    let r2 = heap.region_index_for(p2 as usize).unwrap();

    // Same region — no RSet entry
    if r1 == r2 {
        heap.write_barrier(p1 as usize, p2 as usize);
        assert_eq!(heap.regions[r1].rset.source_count(), 0);
    }
}

#[test]
fn young_collection_set() {
    let mut heap = make_heap();
    // Manually set up regions
    heap.regions[0].region_type = RegionType::Eden;
    heap.regions[0].top = 100;
    heap.regions[1].region_type = RegionType::Eden;
    heap.regions[1].top = 200;
    heap.regions[5].region_type = RegionType::Survivor;
    heap.regions[6].region_type = RegionType::Old;

    let cset = heap.young_collection_set();
    assert!(cset.contains(&0));
    assert!(cset.contains(&1));
    assert!(cset.contains(&5));
    assert!(!cset.contains(&6));
}

#[test]
fn mixed_collection_set() {
    let mut heap = make_heap();
    heap.regions[0].region_type = RegionType::Eden;
    heap.regions[0].top = 100;
    heap.regions[3].region_type = RegionType::Old;
    heap.regions[3].live_bytes = 100;
    heap.regions[4].region_type = RegionType::Old;
    heap.regions[4].live_bytes = 50; // more garbage

    let cset = heap.mixed_collection_set(1);
    // Should include Eden(0) and the most garbage Old region (4, fewer live bytes)
    assert!(cset.contains(&0));
    assert!(cset.contains(&4));
    // Region 3 not included (only 1 old region allowed)
    assert!(!cset.contains(&3));
}

#[test]
fn evacuate_basic() {
    let mut heap = make_heap();

    // Allocate a small object with a proper header
    let obj_size = HEADER_SIZE + 2 * SLOT_SIZE;
    let ptr = heap.alloc_eden(obj_size, 8).unwrap();
    let obj_addr = ptr as usize;

    // Write header
    unsafe {
        let header = &mut *(ptr as *mut ObjectHeader);
        header.set_num_slots(2);
        header.set_shape_tags(ObjectKind::Object, ArrayElementType::Reference);
    }

    let eden_region = heap.region_index_for(obj_addr).unwrap();
    let cset = vec![eden_region];

    let mut roots = vec![obj_addr];
    let forwarding = heap.evacuate(&cset, &mut roots, |addr| addr == obj_addr);

    // Object should have been forwarded
    assert_eq!(forwarding.len(), 1);
    assert!(forwarding.contains_key(&obj_addr));

    // Root should be updated
    let new_addr = roots[0];
    assert_ne!(new_addr, obj_addr);
    assert_eq!(forwarding[&obj_addr], new_addr);

    // Original region should be free
    assert_eq!(heap.regions[eden_region].region_type, RegionType::Free);
}

#[test]
fn needs_gc_threshold() {
    let mut heap = make_heap();
    assert!(!heap.needs_gc(50));

    // Use 5/8 regions (62.5%)
    for i in 0..5 {
        heap.regions[i].region_type = RegionType::Eden;
    }
    assert!(heap.needs_gc(60));
    assert!(!heap.needs_gc(70));
}

#[test]
fn age_survivor_promotion() {
    let mut heap = make_heap();
    heap.regions[0].region_type = RegionType::Survivor;
    heap.regions[0].age = 2;
    heap.tenuring_threshold = 3;

    heap.age_survivor_regions();

    // Age 2 → 3 >= threshold → promoted to Old
    assert_eq!(heap.regions[0].region_type, RegionType::Old);
}

#[test]
fn free_region_count() {
    let mut heap = make_heap();
    assert_eq!(heap.free_region_count(), 8);
    heap.alloc_eden(64, 8).unwrap();
    assert_eq!(heap.free_region_count(), 7);
}

#[test]
fn remembered_set_basic() {
    let mut rset = RememberedSet::default();
    assert_eq!(rset.source_count(), 0);
    rset.add_reference(3);
    rset.add_reference(7);
    rset.add_reference(3); // duplicate
    assert_eq!(rset.source_count(), 2);
    rset.clear();
    assert_eq!(rset.source_count(), 0);
}

/// G1AUD-5 (defect G1-8) — an entry carries the generation of the NEWEST
/// edge from that source.
///
/// Keeping the newest is what makes the staleness test correct: a source
/// that was recycled and then wrote a fresh edge must not be pruned on the
/// strength of the older, dead edge's stamp. The generation-less entry point
/// records the never-prune stamp, which must dominate everything.
#[test]
fn remembered_set_entries_keep_the_newest_generation_stamp() {
    let rset = RememberedSet::default();

    rset.add_reference_in_generation(3, 7);
    assert_eq!(rset.recorded_generation(3), Some(7));

    // Older re-record: the live edge is still the newer one.
    rset.add_reference_in_generation(3, 2);
    assert_eq!(
        rset.recorded_generation(3),
        Some(7),
        "an out-of-order add must not age an entry backwards — that would \
         prune a live edge"
    );

    rset.add_reference_in_generation(3, 9);
    assert_eq!(rset.recorded_generation(3), Some(9));
    assert_eq!(rset.source_count(), 1, "still one source");

    // The generation-less path pins the entry against age-based pruning.
    rset.add_reference(3);
    assert_eq!(rset.recorded_generation(3), Some(RSET_GENERATION_PINNED));
    assert_eq!(rset.recorded_generation(4), None);

    // The generation-aware retain sees both halves of every entry.
    rset.add_reference_in_generation(4, 1);
    let mut seen: Vec<(usize, u64)> = Vec::new();
    rset.retain_sources_in_generation(|s, gen| {
        seen.push((s, gen));
        s == 4
    });
    seen.sort_unstable();
    assert_eq!(seen, vec![(3, RSET_GENERATION_PINNED), (4, 1)]);
    assert_eq!(rset.sources(), vec![4]);
}
