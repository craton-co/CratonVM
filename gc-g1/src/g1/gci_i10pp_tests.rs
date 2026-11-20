//! gci i10/pp (G1 half of `gci-i1orch-windows-jni-foreign-attach-hangs-RETIRED-UNFIXED-20261018`):
//! the parallel Phase-4 walk (`CRATONVM_G1_PARALLEL_FIXUP`) and the
//! above-cursor pin screen (`CRATONVM_G1_PIN_SKIP_ABOVE_CURSOR`). Neither
//! test reads the environment: both arms are driven through the functions
//! the switches select.
use super::*;

const HOLDERS: usize = 3;
const PER_HOLDER: usize = 6;

fn collector(workers: usize) -> G1Collector {
    G1Collector::new(G1CollectorConfig {
        heap_size: 8 * 1024 * 1024,
        initial_heap_size: 8 * 1024 * 1024,
        region_size: 1024 * 1024,
        gc_worker_threads: workers,
        ..G1CollectorConfig::default()
    })
}

/// Three Old holder regions of six one-slot objects each, every slot naming
/// object `a` of an Eden region (the CSet); the pointer map forwards `a` to
/// `b` in a Survivor region.
struct Fixture {
    holders: Vec<usize>,
    objects: Vec<*mut u8>,
    cset: RegionSet,
    dst: usize,
    a: usize,
    b: usize,
    map: cratonvm_types::PointerMap,
}

fn fixture(gc: &G1Collector) -> Fixture {
    let regions = gc.regions.write();
    let free: Vec<usize> = regions
        .iter()
        .enumerate()
        .filter(|(_, r)| r.region_type() == RegionType::Free)
        .map(|(i, _)| i)
        .take(HOLDERS + 2)
        .collect();
    assert_eq!(free.len(), HOLDERS + 2, "the fixture needs five free regions");
    let (src, dst) = (free[HOLDERS], free[HOLDERS + 1]);
    regions[src].set_region_type(RegionType::Eden);
    regions[dst].set_region_type(RegionType::Survivor);
    let (a, _) = regions[src]
        .bump_alloc(HEADER_SIZE + SLOT_SIZE, 8, "test")
        .expect("room in the source");
    let (b, _) = regions[dst]
        .bump_alloc(HEADER_SIZE + SLOT_SIZE, 8, "test")
        .expect("room in the destination");
    let mut objects = Vec::new();
    for &h in &free[..HOLDERS] {
        regions[h].set_region_type(RegionType::Old);
        for _ in 0..PER_HOLDER {
            let (p, _) = regions[h]
                .bump_alloc(HEADER_SIZE + SLOT_SIZE, 8, "test")
                .expect("room in the holder");
            let header = ObjectHeader::new(
                ClassId::new(999_994),
                ObjectKind::Object,
                ArrayElementType::Reference,
                0,
                1,
            );
            // SAFETY: a fresh span of one header plus one legacy slot
            // inside a live region.
            unsafe {
                std::ptr::write(p as *mut ObjectHeader, header);
                write_flat_object_reference(p.add(HEADER_SIZE), a as usize, false);
            }
            objects.push(p);
        }
    }
    let mut map = cratonvm_types::PointerMap::new();
    map.insert(a as usize, b as usize);
    Fixture {
        holders: free[..HOLDERS].to_vec(),
        objects,
        cset: [src].into_iter().collect(),
        dst,
        a: a as usize,
        b: b as usize,
        map,
    }
}

/// The one slot of a fixture object.
fn slot_of(p: *mut u8) -> usize {
    // SAFETY: a fixture object, header written by `fixture`.
    let header = unsafe { &*(p as *const ObjectHeader) };
    let mut got = 0usize;
    for_each_flat_object_reference_capped(p, header, 0, 1, |_, raw, _| got = raw);
    got
}

/// What every walk of the fixture must leave: every slot forwarded, every
/// holder's card dirty, and the destination's rset naming every holder.
fn assert_fixed_up(gc: &G1Collector, f: &Fixture, arm: &str) {
    for &p in &f.objects {
        assert_eq!(slot_of(p), f.b, "{arm}: slot of 0x{:x} not forwarded", p as usize);
        assert!(gc.cards().is_dirty_addr(p as usize), "{arm}: holder card clean");
    }
    let regions = gc.regions.read();
    for &h in &f.holders {
        assert!(
            regions[f.dst].rset.names_source(h),
            "{arm}: the destination's rset must name holder {h}"
        );
    }
}

/// The pool walk forwards, dirties and records exactly what the serial
/// loop does, and reports the same denominators.
#[test]
fn gci_i10pp_parallel_fixup_matches_the_serial_walk() {
    let screen = true;

    let serial_gc = collector(1);
    let s = fixture(&serial_gc);
    assert_eq!(slot_of(s.objects[0]), s.a, "the fixture names the CSet object");
    let narrow: RegionSet = s.holders.iter().copied().collect();
    let census = {
        let mut regions = serial_gc.regions.write();
        serial_gc.update_references_in_regions(&mut regions, &s.cset, &s.map, Some(&narrow))
    };
    assert_fixed_up(&serial_gc, &s, "serial");

    let par_gc = collector(4);
    let p = fixture(&par_gc);
    let walk_order: Vec<usize> = p.holders.clone();
    let helpers = phase4_fixup_helpers(
        par_gc.parallel_worker_count(),
        par_gc.evac_pool().helpers(),
        walk_order.len(),
    );
    let (n, bytes, aborted) = {
        let mut regions = par_gc.regions.write();
        par_gc.update_references_in_regions_parallel(
            &mut regions,
            &p.cset,
            &p.map,
            true,
            &walk_order,
            &[],
            false,
            screen,
            helpers,
        )
    };
    assert_fixed_up(&par_gc, &p, "parallel");
    assert!(!aborted, "a well-formed region walks to its cursor");
    assert_eq!(n, HOLDERS as u32);
    assert_eq!(n, census.walked_regions, "regions: parallel vs serial");
    assert_eq!(bytes, census.walked_bytes, "bytes: parallel vs serial");
    assert_eq!(
        bytes,
        (HOLDERS * PER_HOLDER * (HEADER_SIZE + SLOT_SIZE)) as u64,
        "every holder byte walked once"
    );
    let walks = par_gc.phase_totals.fixup_parallel_walks.load(Ordering::Relaxed);
    assert_eq!(walks, 1, "the census counts the pool walk");
}

/// A CSet or Free index in the walk is skipped by the pool walk exactly as
/// by the serial one, and the merge still lands every holder's edges.
#[test]
fn gci_i10pp_parallel_fixup_skips_cset_and_free_indices() {
    let gc = collector(4);
    let f = fixture(&gc);
    let src = f.cset.iter().next().expect("one CSet region");
    let free = {
        let regions = gc.regions.read();
        regions
            .iter()
            .position(|r| r.region_type() == RegionType::Free)
            .expect("a free region")
    };
    let mut walk_order: Vec<usize> = f.holders.clone();
    walk_order.push(src);
    walk_order.push(free);
    walk_order.sort_unstable();
    let (n, _, _) = {
        let mut regions = gc.regions.write();
        gc.update_references_in_regions_parallel(
            &mut regions,
            &f.cset,
            &f.map,
            true,
            &walk_order,
            &[],
            false,
            true,
            3,
        )
    };
    assert_eq!(n, HOLDERS as u32, "only the holders are walked");
    assert_fixed_up(&gc, &f, "parallel+skips");
}

#[test]
fn gci_i10pp_fixup_helpers_clamp() {
    assert_eq!(phase4_fixup_helpers(8, 7, 11), 7);
    assert_eq!(phase4_fixup_helpers(8, 3, 11), 3, "pool narrower than asked");
    assert_eq!(phase4_fixup_helpers(8, 7, 3), 2, "the driver takes one region");
    assert_eq!(phase4_fixup_helpers(1, 7, 11), 0, "one worker: serial");
    assert_eq!(phase4_fixup_helpers(8, 0, 11), 0, "empty pool: inline");
    assert_eq!(phase4_fixup_helpers(8, 7, 1), 0, "one region: serial");
    assert_eq!(phase4_fixup_helpers(0, 7, 0), 0);
}

#[test]
fn gci_i10pp_pin_word_names_no_allocation_rule() {
    use RegionType as T;
    let (base, cursor) = (0x10_0000usize, 0x2000usize);
    let above = base + cursor + 8;
    let no_alloc = |ty: T, addr: usize| pin_word_names_no_allocation(ty, base, cursor, addr);
    assert!(no_alloc(T::Eden, above));
    assert!(no_alloc(T::Survivor, above));
    assert!(no_alloc(T::Old, above));
    assert!(pin_word_names_no_allocation(T::Old, base, 0, base + 8), "empty region");
    assert!(!no_alloc(T::Eden, base + cursor), "a derived end pointer still pins");
    assert!(!no_alloc(T::Eden, base + 64), "below the cursor");
    assert!(!no_alloc(T::HumongousStart, above), "humongous start");
    assert!(!no_alloc(T::HumongousContinuation, above), "humongous continuation");
    assert!(!no_alloc(T::Free, above), "free");
}

/// A root strictly above an Old region's cursor pins the region with the
/// screen off and not with it on; the one-past-the-end root pins either way.
#[test]
fn gci_i10pp_above_cursor_root_pins_only_without_the_screen() {
    let gc = collector(1);
    let regions = gc.regions.write();
    let idx = regions
        .iter()
        .position(|r| r.region_type() == RegionType::Free)
        .expect("a free region");
    regions[idx].set_region_type(RegionType::Old);
    regions[idx]
        .bump_alloc(HEADER_SIZE + SLOT_SIZE, 8, "test")
        .expect("room");
    let (base, cursor) = (regions[idx].data.addr(), regions[idx].cursor());
    // SAFETY: 8-aligned in-region addresses; an `ObjectRef` is only carried.
    let above = unsafe { ObjectRef::from_raw((base + cursor + 64) as *mut u8) };
    let at_end = unsafe { ObjectRef::from_raw((base + cursor) as *mut u8) };

    let off = gc.pinned_region_set_with_pin_screen(&regions, &[above], false, false);
    assert!(off.contains(&idx), "screen off: today's pin");
    let declined = || gc.phase_totals.pin_words_above_cursor.load(Ordering::Relaxed);
    assert_eq!(declined(), 0, "the off arm counts nothing");

    let on = gc.pinned_region_set_with_pin_screen(&regions, &[above], true, false);
    assert!(!on.contains(&idx), "screen on: no object lives there to hold");
    assert_eq!(declined(), 1, "one word declined");

    let edge = gc.pinned_region_set_with_pin_screen(&regions, &[at_end], true, false);
    assert!(edge.contains(&idx), "a derived end pointer still pins");
    assert_eq!(declined(), 1, "the end pointer is not declined");
}
