use super::*;

fn stw() -> cratonvm_gc_core::collector::StopTheWorldToken {
    // SAFETY: these tests run the heap single-threaded; no mutator runs.
    unsafe { cratonvm_gc_core::collector::StopTheWorldToken::new() }
}

/// 8 MiB, fully committed, 1 MiB regions, one worker.
fn collector() -> G1Collector {
    G1Collector::new(G1CollectorConfig {
        heap_size: 8 * 1024 * 1024,
        initial_heap_size: 8 * 1024 * 1024,
        region_size: 1024 * 1024,
        gc_worker_threads: 1,
        heap_waste_percent: 0,
        ..G1CollectorConfig::default()
    })
}

fn is_marked(gc: &G1Collector, addr: usize) -> bool {
    let regions = gc.regions.read();
    let idx = gc
        .region_for_ptr(&regions, addr as *mut u8)
        .expect("fixture address is in a live region");
    regions[idx].mark_bitmap.is_marked(addr)
}

/// The frozen peer's store, completed after the pause WITHOUT the SATB
/// pre-barrier: cell 0 of a legacy holder overwritten in place.
fn unlogged_overwrite_cell0(holder: ObjectRef, value: Value) {
    // SAFETY: cell 0 of a live legacy instance with one field.
    unsafe {
        cratonvm_types::write_value_atomic(
            (holder.as_ptr() as usize + HEADER_SIZE) as *mut Value,
            value,
        )
    };
}

/// Holder `O` with `O.f == A`; the cycle opens with `O` a frozen peer's
/// root; after the pause the peer writes `O.f = B` unlogged. `A` must
/// still be marked: it was reachable at the snapshot.
#[test]
fn gci_i12wb_a_frozen_holders_snapshot_value_survives_an_unlogged_overwrite() {
    let gc = collector();
    let holder = gc.alloc_object(ClassId::new(0x00AB_CE01), 1);
    let a = gc.alloc_object(ClassId::new(0x00AB_CE02), 0);
    let b = gc.alloc_object(ClassId::new(0x00AB_CE02), 0);
    // SAFETY: `holder` is a freshly allocated object.
    let header = unsafe { &*(holder.as_ptr() as *const ObjectHeader) };
    assert!(
        !cratonvm_types::is_compact_object(header),
        "fixture: the holder must use the legacy cell layout"
    );
    gc.set_field(holder, 0, Value::Object(Some(a)));

    gc.start_concurrent_mark(&stw());
    gc.remark(&stw(), &[holder]);
    assert_eq!(gc.scan_frozen_held_objects(&stw(), &[holder]), 1);
    unlogged_overwrite_cell0(holder, Value::Object(Some(b)));
    while !gc.concurrent_mark_step(usize::MAX) {}

    assert!(is_marked(&gc, holder.as_ptr() as usize));
    assert!(is_marked(&gc, b.as_ptr() as usize), "the new value, through the holder");
    assert!(
        is_marked(&gc, a.as_ptr() as usize),
        "the snapshot value was grayed inside the pause"
    );
}

/// The control: the same sequence without the scan loses `A`, which is
/// the defect the scan exists for (the fixture discriminates).
#[test]
fn gci_i12wb_without_the_scan_the_unlogged_overwrite_hides_the_snapshot_value() {
    let gc = collector();
    let holder = gc.alloc_object(ClassId::new(0x00AB_CE03), 1);
    let a = gc.alloc_object(ClassId::new(0x00AB_CE04), 0);
    let b = gc.alloc_object(ClassId::new(0x00AB_CE04), 0);
    gc.set_field(holder, 0, Value::Object(Some(a)));

    gc.start_concurrent_mark(&stw());
    gc.remark(&stw(), &[holder]);
    unlogged_overwrite_cell0(holder, Value::Object(Some(b)));
    while !gc.concurrent_mark_step(usize::MAX) {}

    assert!(is_marked(&gc, b.as_ptr() as usize));
    assert!(
        !is_marked(&gc, a.as_ptr() as usize),
        "without the in-pause scan nothing names A any more"
    );
}

/// A reference array a frozen peer holds is scanned WHOLE: an element
/// past the first mark chunk is grayed too.
#[test]
fn gci_i12wb_a_frozen_reference_array_is_scanned_past_its_first_chunk() {
    let gc = collector();
    let len = MARK_ARRAY_CHUNK + 904;
    let at = MARK_ARRAY_CHUNK + 404;
    let arr = gc.alloc_array(ClassId::new(0x00AB_CE05), ArrayElementType::Reference, len);
    let a = gc.alloc_object(ClassId::new(0x00AB_CE06), 0);
    assert!(gc.set_array_element(arr, at, Value::Object(Some(a))).is_ok());

    gc.start_concurrent_mark(&stw());
    gc.remark(&stw(), &[arr]);
    assert_eq!(gc.scan_frozen_held_objects(&stw(), &[arr]), 1);
    // The frozen `aastore` completes unlogged: element `at` = null.
    // SAFETY: element `at` of a live reference array of `len` 8-byte slots.
    unsafe {
        std::ptr::write(
            (arr.as_ptr() as usize + ARRAY_DATA_OFFSET + at * 8) as *mut u64,
            0,
        )
    };
    while !gc.concurrent_mark_step(usize::MAX) {}

    assert!(is_marked(&gc, a.as_ptr() as usize), "element {at} was grayed in the pause");
}

/// Item 4's premise: G1's own store accessors publish the overwritten
/// value while marking, so the interpreter / `jit_putfield_object` no
/// longer log it a second time on G1. The overwritten `A` (no root names
/// it) must be marked through the accessor's log alone.
#[test]
fn gci_i12wb_g1_accessors_publish_the_overwritten_value_themselves() {
    let gc = collector();
    let holder = gc.alloc_object(ClassId::new(0x00AB_CE07), 1);
    let arr = gc.alloc_array(ClassId::new(0x00AB_CE08), ArrayElementType::Reference, 4);
    let a = gc.alloc_object(ClassId::new(0x00AB_CE09), 0);
    let c = gc.alloc_object(ClassId::new(0x00AB_CE09), 0);
    let b = gc.alloc_object(ClassId::new(0x00AB_CE09), 0);
    gc.set_field(holder, 0, Value::Object(Some(a)));
    assert!(gc.set_array_element(arr, 2, Value::Object(Some(c))).is_ok());

    gc.start_concurrent_mark(&stw());
    gc.set_field(holder, 0, Value::Object(Some(b)));
    assert!(gc.set_array_element(arr, 2, Value::Object(Some(b))).is_ok());
    // No root names `a` or `c`: only the accessors' SATB logs can.
    gc.remark(&stw(), &[]);
    while !gc.concurrent_mark_step(usize::MAX) {}

    assert!(is_marked(&gc, a.as_ptr() as usize), "set_field logged the old field value");
    assert!(is_marked(&gc, c.as_ptr() as usize), "set_array_element logged the old element");
}

/// Nothing to scan: no frozen peer, a word outside the heap, a primitive
/// array.
#[test]
fn gci_i12wb_the_scan_skips_what_names_no_reference_holder() {
    let gc = collector();
    let prim = gc.alloc_array(ClassId::new(0), ArrayElementType::Int, 16);
    gc.start_concurrent_mark(&stw());
    assert_eq!(gc.scan_frozen_held_objects(&stw(), &[]), 0);
    // SAFETY: an aligned address outside the arena, only carried.
    let outside = unsafe { ObjectRef::from_raw(0x1000 as *mut u8) };
    assert_eq!(gc.scan_frozen_held_objects(&stw(), &[outside, prim]), 0);
}
