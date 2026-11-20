//! gci i9/ng (i8/nt review item 1): `EVAC_HOLDER_CLAMPED` counts LEGACY
//! holders only. A compact holder's cap is computed with the legacy
//! 16-byte stride over a field count and its walk ignores the cap, so it
//! was noise (30 000-77 000 per netty run) that hid a real clamp.
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

/// A holder declaring four slots with room for one: the only object in a
/// fresh Old region, so the cursor bound ends right after its first slot.
fn clamped_holder(gc: &G1Collector, compact: bool) -> *mut u8 {
    let regions = gc.regions.write();
    let idx = regions
        .iter()
        .position(|r| r.region_type() == RegionType::Free)
        .expect("a free region");
    regions[idx].set_region_type(RegionType::Old);
    let (ptr, _) = regions[idx]
        .bump_alloc(HEADER_SIZE + SLOT_SIZE, 8, "test")
        .expect("room for one slot");
    let header = ObjectHeader::new(
        ClassId::new(999_995),
        ObjectKind::Object,
        ArrayElementType::Reference,
        0,
        4,
    );
    // SAFETY: a fresh span of the header's size inside a live region.
    unsafe { std::ptr::write(ptr as *mut ObjectHeader, header) };
    if compact {
        // SAFETY: the header written above.
        let h = unsafe { &*(ptr as *const ObjectHeader) };
        h.mark_word.fetch_or(
            u32::from(cratonvm_types::GC_FLAG_COMPACT) << 24,
            Ordering::Relaxed,
        );
        assert!(cratonvm_types::is_compact_object(h));
    }
    ptr
}

#[test]
fn gci_i9ng_a_legacy_clamp_is_counted() {
    let gc = collector();
    let holder = clamped_holder(&gc, false);
    let before = EVAC_HOLDER_CLAMPED.load(Ordering::Relaxed);
    let walkable = {
        let regions = gc.regions.read();
        gc.holder_walkable_slots(&regions, holder, 4, SLOT_SIZE)
    };
    assert!(walkable < 4, "the fixture must clamp: walkable={walkable}");
    assert!(
        EVAC_HOLDER_CLAMPED.load(Ordering::Relaxed) > before,
        "a legacy clamp is the counter's whole point"
    );
}

#[test]
fn gci_i9ng_a_compact_holder_is_capped_the_same_but_not_counted() {
    let gc = collector();
    let legacy = clamped_holder(&gc, false);
    let compact = clamped_holder(&gc, true);
    let (legacy_room, compact_room) = {
        let regions = gc.regions.read();
        (
            gc.holder_walkable_slots(&regions, legacy, 4, SLOT_SIZE),
            gc.holder_walkable_slots(&regions, compact, 4, SLOT_SIZE),
        )
    };
    assert!(compact_room < 4, "the compact fixture must reach the cap");
    assert_eq!(compact_room, legacy_room, "the returned cap is unchanged");
    // Process-wide counter, other tests may clamp concurrently: 1000
    // compact clamps must not show up as (anywhere near) 1000 counts.
    let before = EVAC_HOLDER_CLAMPED.load(Ordering::Relaxed);
    {
        let regions = gc.regions.read();
        for _ in 0..1000 {
            let _ = gc.holder_walkable_slots(&regions, compact, 4, SLOT_SIZE);
        }
    }
    let delta = EVAC_HOLDER_CLAMPED.load(Ordering::Relaxed) - before;
    assert!(delta < 1000, "compact holders were counted: delta={delta}");
}
