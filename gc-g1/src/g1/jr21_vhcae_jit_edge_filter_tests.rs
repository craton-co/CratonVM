use super::*;

fn collector() -> G1Collector {
    G1Collector::new(G1CollectorConfig {
        heap_size: 8 * 1024 * 1024,
        initial_heap_size: 8 * 1024 * 1024,
        region_size: 1024 * 1024,
        gc_worker_threads: 1,
        ..G1CollectorConfig::default()
    })
}

fn entry(gc: &G1Collector, dst: usize) -> u64 {
    gc.jit_edge_filter.entries[dst].load(Ordering::Relaxed)
}

/// A real object (the barrier vouches the holder's header) and the base
/// of region `dst` typed `ty` (the value is only located, never read).
fn holder_and_value(gc: &G1Collector, dst: usize, ty: RegionType) -> (ObjectRef, usize, ObjectRef) {
    let holder = gc.alloc_object(ClassId::new(0x7621), 2);
    let src = gc
        .lookup_region_for_addr(holder.as_ptr() as usize)
        .expect("the holder is in a region");
    assert_ne!(src, dst, "the test needs a cross-region pair");
    gc.with_regions_mut(|regions| regions[dst].set_region_type(ty));
    let dst_addr = gc.regions.read()[dst].data.as_ptr() as usize;
    // SAFETY: a real, mapped region base of this heap; the barrier only
    // resolves its region index.
    let value = unsafe { ObjectRef::from_raw(dst_addr as *mut u8) };
    (holder, src, value)
}

#[test]
fn the_filter_names_the_recorded_edge_in_its_epoch_and_a_bump_retires_it() {
    let gc = collector();
    let (holder, src, value) = holder_and_value(&gc, 6, RegionType::Old);
    let f = &gc.jit_edge_filter;
    assert_eq!(f.addr(), gc.jit_edge_filter_addr());
    assert_eq!(f.region_count, gc.num_regions());
    assert_eq!(f.arena_base, gc.arena.base());
    assert_eq!(f.regions_len, gc.num_regions() << gc.region_shift);
    assert_eq!(f.cards_base, gc.cards().cards_base_addr());
    assert_eq!(f.starts_base, gc.cards().starts_base_addr());
    assert_eq!(f.entries_addr, f.entries.as_ptr() as usize);
    assert_eq!(entry(&gc, 6), 0, "nothing recorded yet");

    gc.post_write_barrier_rset(holder, value);
    let epoch = gc.rset_cache_epoch.load(Ordering::Acquire);
    assert_eq!(f.epoch_mirror.load(Ordering::Acquire), epoch);
    assert_eq!(entry(&gc, 6), G1JitEdgeFilter::key(epoch, src));
    assert!(
        gc.cards().is_dirty_addr(holder.as_ptr() as usize),
        "the card half ran too"
    );
    // A repeat (a memo hit) keeps the entry.
    gc.post_write_barrier_rset(holder, value);
    assert_eq!(entry(&gc, 6), G1JitEdgeFilter::key(epoch, src));

    // A bump (what every recycle/retype phase does first) moves the
    // mirror, so the old entry no longer compares equal in compiled code.
    gc.bump_rset_cache_epoch();
    let bumped = gc.rset_cache_epoch.load(Ordering::Acquire);
    assert_eq!(bumped, epoch + 1);
    assert_eq!(f.epoch_mirror.load(Ordering::Acquire), bumped);
    assert_ne!(entry(&gc, 6), G1JitEdgeFilter::key(bumped, src));
    // The next store re-records it under the new epoch (the per-thread
    // memos were retired by the same bump).
    gc.post_write_barrier_rset(holder, value);
    assert_eq!(entry(&gc, 6), G1JitEdgeFilter::key(bumped, src));
    assert!(gc.regions.read()[6].rset.sources().contains(&src));
}

#[test]
fn a_free_destination_records_no_entry() {
    let gc = collector();
    let (holder, _src, value) = holder_and_value(&gc, 5, RegionType::Free);
    gc.post_write_barrier_rset(holder, value);
    assert_eq!(entry(&gc, 5), 0, "no rset insert, no filter entry");
}

#[test]
fn the_wrapping_bump_clears_every_entry() {
    let gc = collector();
    gc.jit_edge_filter.note_edge(1, 0, 0xFFFF_FFFF);
    gc.rset_cache_epoch.store(0xFFFF_FFFF, Ordering::Release);
    gc.bump_rset_cache_epoch();
    assert_eq!(gc.rset_cache_epoch.load(Ordering::Acquire), 1 << 32);
    assert_eq!(entry(&gc, 1), 0);
    assert_eq!(
        gc.jit_edge_filter.epoch_mirror.load(Ordering::Acquire),
        1 << 32
    );
}

/// vhc21 (VHCAE21-7): the census tells an alternation (another source
/// replacing a live-epoch entry) from a first fill or a new epoch. The
/// counters themselves move only under `CRATONVM_DBG_G1ACCESSOR`, so the
/// classifier and the booking are exercised directly.
#[test]
fn vhc21_the_census_tells_alternation_from_a_first_fill() {
    let key = G1JitEdgeFilter::key;
    assert!(!G1JitEdgeFilter::overwrite_alternates(0, key(5, 1)), "empty");
    assert!(
        !G1JitEdgeFilter::overwrite_alternates(key(4, 1), key(5, 2)),
        "an older epoch is a fill"
    );
    assert!(
        G1JitEdgeFilter::overwrite_alternates(key(5, 1), key(5, 2)),
        "another source in the same epoch"
    );
    assert!(
        G1JitEdgeFilter::overwrite_alternates(key((7u64 << 32) | 5, 1), key(5, 2)),
        "only the epoch's low half is compared, as the compiled reader does"
    );
    let gc = collector();
    let f = &gc.jit_edge_filter;
    assert_eq!(f.census(), (0, 0, 0));
    f.census_overwrite(0, key(5, 1));
    f.census_overwrite(key(5, 1), key(5, 2));
    f.census_overwrite(key(5, 2), key(5, 1));
    f.census_overwrite(key(5, 1), key(6, 1));
    assert_eq!(f.census(), (0, 2, 2));
    // The write path stays correct whatever the census does: the entry
    // names the last edge noted.
    f.note_edge(3, 1, 6);
    f.note_edge(3, 2, 6);
    assert_eq!(entry(&gc, 3), G1JitEdgeFilter::key(6, 2));
}
