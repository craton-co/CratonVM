// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Phase 5 boot image, WP2: the real mapping sites in a capture run.
//!
//! One test in its own binary (its own process), because it flips the
//! process-wide `image_layout` mode to `Capture`, which every other test in a
//! shared binary would then see. It drives the actual sites -- the GC heap
//! reservation (`HeapStore::new`, also below one granule, where an `Off` run
//! takes the Rust heap), a GC side table (`HeapBitmap` on its own mapping),
//! the JIT's code-adjacent cell arena (`platform_alloc`) and a poll-page pair
//! (`map_low_pair`) -- and checks each lands at its brokered address and is
//! recorded, and that a release forgets its record.

#[cfg(target_os = "linux")]
#[test]
fn capture_places_and_records_every_boot_mapping_site() {
    use cratonvm_gc::heap_bitmap::HeapBitmap;
    use cratonvm_gc::reservation::HeapStore;
    use cratonvm_types::image_layout::{self as il, Backing, ImageMode, SegmentKind};

    const GRANULE: usize = 2 << 20;
    il::set_mode(ImageMode::Capture);
    let heap_base = SegmentKind::Heap.region().0;

    // Below one granule: an `Off` run would back this with `alloc_zeroed`.
    let small = HeapStore::new(64 * 1024, "p5-wp2");
    assert!(matches!(small, HeapStore::Reserved(_)), "capture never uses the Rust heap");
    assert_eq!(small.as_ptr() as usize, heap_base);
    let big = HeapStore::new(4 * GRANULE, "p5-wp2");
    assert_eq!(big.as_ptr() as usize, heap_base + GRANULE, "next 2 MiB slot");

    // A side table of >= 1 MiB of words goes on its own mapping.
    let bitmap = HeapBitmap::labelled_side_table(heap_base, 128 << 20, "p5-wp2", true);
    let side_base = SegmentKind::GcSideTable.region().0;

    let cell = cratonvm_jit::platform::alloc_code_adjacent_cell().expect("cell");
    let jit = SegmentKind::JitCode.region();
    assert!(
        (jit.0..jit.0 + jit.1).contains(&(cell as usize)),
        "the cell arena is brokered: {cell:p}"
    );

    #[cfg(target_arch = "x86_64")]
    {
        let pair = cratonvm_jit::poll_page::map_pair_for_measurement().expect("low pair");
        assert_eq!(pair, SegmentKind::PollPage.region().0);
    }

    let segs = il::segments();
    let at = |addr: usize| segs.iter().find(|s| s.addr == addr);
    let s = at(heap_base).expect("small heap recorded");
    assert_eq!((s.kind, s.len, s.prot), (SegmentKind::Heap, GRANULE, 0));
    assert_eq!(s.backing, Backing::Anonymous { hugetlb: false });
    assert_eq!(at(heap_base + GRANULE).expect("big heap").len, 4 * GRANULE);
    let side = at(side_base).expect("side table recorded");
    assert_eq!((side.kind, side.prot), (SegmentKind::GcSideTable, 0x3));
    assert!(segs.iter().any(|s| s.kind == SegmentKind::JitCode));
    assert_eq!(il::spoiled(), None, "nothing fell back to a kernel-chosen address");

    // Releases forget their records.
    drop(big);
    drop(bitmap);
    let after = il::segments();
    assert!(after.iter().all(|s| s.addr != heap_base + GRANULE));
    assert!(after.iter().all(|s| s.addr != side_base));
    assert!(after.iter().any(|s| s.addr == heap_base));
    drop(small);
}
