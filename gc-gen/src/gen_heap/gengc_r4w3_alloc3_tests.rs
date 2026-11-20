use super::*;

/// The allocator gate and the server must agree on satisfiability (the
/// tlab-remnant-wedge rule). A from-space with a 4 KiB bump tail and no
/// free block: the default JIT refill gate probes the tail for the whole
/// request and refuses, although `refill_tlab` serves that tail. The
/// opt-in `CRATONVM_TLAB_GATE_BUMP_FLOOR` probe (the fragmentation floor,
/// raised to the object in hand, capped at the request — the expression in
/// `gc_and_alloc.rs::tlab_gate_bump_probe`) admits exactly the refill the
/// server then makes.
#[test]
fn the_floor_gate_admits_exactly_what_the_refill_serves() {
    let heap = GenerationalHeap::with_sizes(64 * 1024, 64 * 1024);
    {
        let mut from = heap.lock_young_from();
        let leave = from.capacity() - 4096;
        let _ = from.alloc(leave, 8).expect("a fresh 64 KiB arena");
    }
    let requested = 256 * 1024;
    let floor = cratonvm_gc_core::tlab::frag_tlab_floor();
    let object = 48;
    assert!(
        !heap.young_bump_headroom(requested),
        "the default gate refuses"
    );
    assert!(
        !heap.young_has_free_block(floor),
        "and has no hole to fall back on"
    );
    let probe = floor.max(object).min(requested);
    assert!(heap.young_bump_headroom(probe), "the floor gate admits");
    let (ptr, size) = heap
        .refill_tlab(requested)
        .expect("the server serves the tail");
    assert_eq!(size, 4096);
    // SAFETY: `refill_tlab` handed this caller `[ptr, ptr + size)`, zeroed.
    let chunk = unsafe { std::slice::from_raw_parts(ptr, size) };
    assert!(chunk.iter().all(|&b| b == 0));
    assert!(
        !heap.young_bump_headroom(probe),
        "and the tail is now spent"
    );
}

/// The `[GC] gen-alloc:` line carries every key an A/B grep needs, and its
/// per-heap counts move with the door taken: a slow-path young object
/// ticks `gen_young_slow_allocs=`, a humongous array `gen_old_allocs=`.
/// (The `tlab_refill_*` counts are process-wide and other tests move them,
/// so only their presence is asserted.)
#[test]
fn the_alloc_path_census_counts_this_heaps_doors() {
    let heap = GenerationalHeap::with_sizes(64 * 1024, 1024 * 1024);
    let line = heap.alloc_path_census_line();
    for key in [
        "[GC] gen-alloc: ",
        "gen_young_slow_allocs=0 ",
        "gen_old_allocs=0 ",
        "tlab_refill_attempts=",
        "tlab_refill_ok=",
        "tlab_refill_retries=",
        "tlab_refill_retry_ok=",
        // gci i23/lc: the skip-edge and lean-POST counters, zero untraced.
        "gen_jit_card_sp_skip_young=0 ",
        "gen_jit_card_sp_skip_null=0 ",
        "gen_jit_card_sp_skip_oldval=0 ",
        "gen_jit_card_sp_skip_outside=0 ",
        "gen_jit_card_sp_lean_post=0 ",
        // gcj j2/jt2: the post-only card sites and their take-over completions.
        "gen_jit_card_post_only_sites=0 ",
        "gen_jit_card_post_only_completed=0 ",
    ] {
        assert!(line.contains(key), "{key:?} missing from {line}");
    }
    let _obj = heap
        .try_alloc_object(ClassId::new(0), 2)
        .expect("a 64 KiB nursery holds one small object");
    // Larger than half the 64 KiB semi-space: humongous, straight to old.
    let _big = heap
        .try_alloc_array(ClassId::new(0), ArrayElementType::Byte, 40 * 1024)
        .expect("the 1 MiB old generation holds one 40 KiB array");
    let line = heap.alloc_path_census_line();
    assert!(line.contains("gen_young_slow_allocs=1 "), "{line}");
    assert!(line.contains("gen_old_allocs=1 "), "{line}");
}
