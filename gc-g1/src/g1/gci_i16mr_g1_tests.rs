use super::*;

fn stw() -> cratonvm_gc_core::collector::StopTheWorldToken {
    // SAFETY: these tests run the heap single-threaded; no mutator runs.
    unsafe { cratonvm_gc_core::collector::StopTheWorldToken::new() }
}

fn collector() -> G1Collector {
    G1Collector::new(G1CollectorConfig {
        heap_size: 8 * 1024 * 1024,
        initial_heap_size: 8 * 1024 * 1024,
        region_size: 1024 * 1024,
        gc_worker_threads: 1,
        ..G1CollectorConfig::default()
    })
}

/// The pure rule: the lowest tail starting below the cursor wins; a tail
/// at or above the cursor, an empty span and a span outside the region
/// change nothing.
#[test]
fn gci_i16mr_tams_drops_to_the_lowest_reserved_tail_inside_the_fill() {
    const BASE: usize = 0x4000_0000;
    assert_eq!(tams_below_reserved_tails(BASE, 0x8000, &[]), 0x8000);
    let spans = [
        (BASE + 0x6000, BASE + 0x7000),
        (BASE + 0x2000, BASE + 0x3000),
        (BASE + 0x8000, BASE + 0x9000),
        (BASE + 0x1000, BASE + 0x1000),
        (BASE - 0x1000, BASE),
        (0x9000_0000, 0x9000_1000),
    ];
    assert_eq!(tams_below_reserved_tails(BASE, 0x8000, &spans), 0x2000);
    assert_eq!(tams_below_reserved_tails(BASE, 0x2000, &spans), 0x2000, "a tail at the cursor");
    assert_eq!(tams_below_reserved_tails(BASE, 0x1800, &spans), 0x1800, "nothing below");
}

/// The frozen-peer hazard (i16/ca's suspicion, confirmed by reading): a
/// TLAB chunk carved before the initial mark sits below the region cursor,
/// so with TAMS at the cursor an object its owner bumps there after the
/// pause reads DEAD to `is_live_after_mark` (unmarked, below TAMS). With
/// the chunk published as a reserved tail, TAMS starts at it and the same
/// address reads live; below the tail the bitmap still decides.
#[test]
fn gci_i16mr_an_object_bumped_into_a_frozen_tail_after_the_initial_mark_reads_live() {
    let gc = collector();
    let before = gc.alloc_object(ClassId::new(9), 1);
    gc.set_field(before, 0, Value::Int(7));
    let (chunk, len) = gc.refill_tlab(64 * 1024).expect("a TLAB chunk");
    let tail = chunk as usize;
    assert!(len > 512, "fixture: a usable chunk ({len})");
    let inside = tail + 256;
    let region = gc.lookup_region_for_addr(tail).expect("the chunk is in a region");

    gc.clear_jit_tlab_skip_regions();
    gc.start_concurrent_mark(&stw());
    let dead_without = !gc.is_live_after_mark(inside);
    gc.abort_concurrent_mark();

    gc.set_jit_tlab_skip_regions(&[(tail, tail + len)]);
    gc.start_concurrent_mark(&stw());
    let live_with = gc.is_live_after_mark(inside);
    let (tams, base) = {
        let regions = gc.regions.read();
        (regions[region].tams(), regions[region].data.addr())
    };
    let b = before.as_ptr() as usize;
    let below_in_region = gc.lookup_region_for_addr(b) == Some(region) && b < tail;
    let before_live = gc.is_live_after_mark(b);
    gc.abort_concurrent_mark();
    gc.clear_jit_tlab_skip_regions();

    assert!(dead_without, "TAMS at the cursor: the post-pause object reads dead");
    assert!(live_with, "TAMS at the tail: the post-pause object is implicitly live");
    assert_eq!(tams, tail - base, "the region's own TAMS mirror agrees with the snapshot");
    if below_in_region {
        assert!(!before_live, "below the tail the (empty) bitmap still decides");
    }
}

/// The remark's row closure on the real marker: an owner marked and
/// drained before its `metadata_pin` row was filed stays black without
/// the row's value (the drain after the filing has nothing gray); the
/// closure the VM's remark callback runs (`is_live_after_mark` on the
/// owner, `resurrect_after_remark` on its values) marks it.
#[test]
fn gci_i16mr_a_row_filed_after_its_owner_was_marked_is_closed_by_the_resurrection_drain() {
    const VM: usize = 0x16ad_0001;
    let gc = collector();
    let owner = gc.alloc_object(ClassId::new(9), 1);
    gc.set_field(owner, 0, Value::Int(1));
    let value = gc.alloc_object(ClassId::new(9), 1);
    gc.set_field(value, 0, Value::Int(42));
    let (o, v) = (owner.as_ptr() as usize, value.as_ptr() as usize);
    cratonvm_types::metadata_pin::forget_vm_metadata_pins(VM);

    gc.start_concurrent_mark(&stw());
    gc.remark(&stw(), &[owner]);
    while !gc.concurrent_mark_step(usize::MAX) {}
    let owner_live = gc.is_live_after_mark(o);
    cratonvm_types::metadata_pin::add_metadata_pin(VM, o, v);
    gc.remark(&stw(), &[]);
    while !gc.concurrent_mark_step(usize::MAX) {}
    let missed = !gc.is_live_after_mark(v);
    let rows = cratonvm_types::metadata_pin::snapshot().expect("the row was filed");
    let values = rows.get(&o).cloned().unwrap_or_default();
    if gc.is_live_after_mark(o) {
        gc.resurrect_after_remark(&values);
    }
    let closed = gc.is_live_after_mark(v);
    gc.abort_concurrent_mark();
    cratonvm_types::metadata_pin::forget_vm_metadata_pins(VM);

    eprintln!("[gci-i16mr] marker missed the late row: {missed}");
    assert!(owner_live, "fixture: the root owner is marked");
    assert_eq!(values, vec![v]);
    assert!(closed, "the closure marks the late row's value");
}
