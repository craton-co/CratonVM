use super::*;

const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;
const GIB: u64 = 1024 * MIB;
const TIB: u64 = 1024 * GIB;

/// A plausible, aligned arena base for constructing address spaces.
const TEST_BASE: u64 = 0x7F00_0000_0000 & !0xFFF;

// -- bit-layout invariants ----------------------------------------------

#[test]
fn fields_are_pairwise_disjoint() {
    let fields = [
        ("offset", Z_OFFSET_MASK),
        ("metadata", Z_METADATA_MASK),
        ("tag", Z_COLORED_TAG),
        ("mbz", Z_RESERVED_MBZ_MASK),
    ];
    for (i, (na, a)) in fields.iter().enumerate() {
        for (nb, b) in fields.iter().skip(i + 1) {
            assert_eq!(a & b, 0, "{na} overlaps {nb}");
        }
    }
}

#[test]
fn fields_cover_all_64_bits() {
    let union = Z_OFFSET_MASK | Z_METADATA_MASK | Z_COLORED_TAG | Z_RESERVED_MBZ_MASK;
    assert_eq!(union, u64::MAX, "the four fields must partition the word");
}

#[test]
fn metadata_bits_are_distinct_single_bits() {
    let bits = [Z_MARKED0, Z_MARKED1, Z_REMAPPED, Z_FINALIZABLE];
    for b in bits {
        assert_eq!(b.count_ones(), 1, "{b:#x} is not a single bit");
        assert_eq!(b & Z_METADATA_MASK, b, "{b:#x} escapes the metadata field");
        assert_eq!(b & Z_OFFSET_MASK, 0, "{b:#x} overlaps the offset field");
    }
    assert_eq!(
        (Z_MARKED0 | Z_MARKED1 | Z_REMAPPED | Z_FINALIZABLE),
        Z_METADATA_MASK
    );
}

/// The layout must match OpenJDK's `zGlobals.hpp` exactly in bits 45-0,
/// because every ZGC reference anyone reads while extending this describes
/// those positions.
#[test]
fn matches_openjdk_bit_positions() {
    assert_eq!(Z_OFFSET_BITS, 42);
    assert_eq!(Z_METADATA_SHIFT, 42);
    assert_eq!(Z_MARKED0, 1u64 << 42);
    assert_eq!(Z_MARKED1, 1u64 << 43);
    assert_eq!(Z_REMAPPED, 1u64 << 44);
    assert_eq!(Z_FINALIZABLE, 1u64 << 45);
    assert_eq!(Z_METADATA_MASK, 0xFu64 << 42);
}

/// The whole point of bit 63. A colored word must never look like a
/// plausible machine pointer to the rest of the VM.
#[test]
fn colored_words_are_never_plausible_heap_pointers() {
    for c in ZColor::ALL {
        for offset in [0u64, 8, 0x1000, 0x1_0000_0000, Z_OFFSET_MASK & !7] {
            let w = color(offset, c);
            assert!(w > Z_MAX_ADDRESS, "{w:#x} fits the 47-bit window");
            assert!(
                !cratonvm_types::plausible_heap_pointer(w),
                "colored word {w:#x} passed plausible_heap_pointer"
            );
            assert!(is_colored_word(w));
        }
    }
    // Null is the exception, and correctly so: it is not a pointer either.
    assert!(!cratonvm_types::plausible_heap_pointer(Z_NULL));
    assert!(!is_colored_word(Z_NULL));
}

// -- color / uncolor round trips ----------------------------------------

#[test]
fn color_offset_roundtrip_many_offsets() {
    let mut offsets: Vec<u64> = vec![0, 8, 16, 4096, 1 << 20, 1 << 30, 1 << 40];
    // A dense low sweep plus a sparse high sweep.
    for i in 0..512u64 {
        offsets.push(i * 8);
    }
    for i in 0..64u64 {
        offsets.push((Z_OFFSET_MASK & !7) - i * 4096);
    }
    for &off in &offsets {
        for c in ZColor::ALL {
            let w = color(off, c);
            assert_eq!(offset_of(w), off, "offset {off:#x} color {c:?}");
            assert_eq!(color_of(w), Some(c), "offset {off:#x} color {c:?}");
            assert!(is_well_formed(w));
            // Offset 0 is a real object slot, not null: the tag bit keeps
            // `color(0, c)` distinguishable from `Z_NULL`.
            assert!(!is_null(w), "offset {off:#x} color {c:?} looked null");
        }
    }
}

#[test]
fn max_offset_roundtrips() {
    let w = color(Z_OFFSET_MASK, ZColor::Remapped);
    assert_eq!(offset_of(w), Z_OFFSET_MASK);
    assert_eq!(color_of(w), Some(ZColor::Remapped));
    assert_eq!(w & Z_RESERVED_MBZ_MASK, 0);
}

#[test]
fn recolor_preserves_offset_and_replaces_color() {
    let w = color(0xDEAD_B0, ZColor::Marked0);
    let w2 = recolor(w, ZColor::Remapped);
    assert_eq!(offset_of(w2), 0xDEAD_B0);
    assert_eq!(color_of(w2), Some(ZColor::Remapped));
    assert!(!is_marked0(w2));
    assert!(is_remapped(w2));
    assert!(same_object(w, w2));
    // Null stays null through a recolor.
    assert_eq!(recolor(Z_NULL, ZColor::Marked1), Z_NULL);
}

#[test]
fn per_bit_predicates() {
    let m0 = color(64, ZColor::Marked0);
    let m1 = color(64, ZColor::Marked1);
    let rm = color(64, ZColor::Remapped);
    let fz = color(64, ZColor::Finalizable);

    assert!(is_marked0(m0) && !is_marked1(m0) && !is_remapped(m0) && !is_finalizable(m0));
    assert!(!is_marked0(m1) && is_marked1(m1) && !is_remapped(m1) && !is_finalizable(m1));
    assert!(!is_marked_any(rm) && is_remapped(rm) && !is_finalizable(rm));
    assert!(!is_marked_any(fz) && !is_remapped(fz) && is_finalizable(fz));
    assert!(is_marked_any(m0) && is_marked_any(m1));
}

#[test]
fn well_formedness_rejects_untagged_and_multicolored() {
    // Untagged: an ordinary machine pointer.
    assert!(!is_well_formed(0x7F00_0000_1000));
    // Tagged but two metadata bits.
    assert!(!is_well_formed(
        Z_COLORED_TAG | Z_MARKED0 | Z_REMAPPED | 0x40
    ));
    // Tagged but no metadata bit.
    assert!(!is_well_formed(Z_COLORED_TAG | 0x40));
    // Tagged with a reserved bit set.
    assert!(!is_well_formed(Z_COLORED_TAG | Z_REMAPPED | (1u64 << 50)));
    // Null is well formed by definition.
    assert!(is_well_formed(Z_NULL));
}

#[test]
fn color_of_rejects_ambiguous_metadata() {
    assert_eq!(ZColor::from_bit(0), None);
    assert_eq!(ZColor::from_bit(Z_MARKED0 | Z_MARKED1), None);
    assert_eq!(ZColor::from_bit(Z_MARKED0), Some(ZColor::Marked0));
}

// -- good/bad mask semantics --------------------------------------------

#[test]
fn good_and_bad_agree_for_wellformed_words() {
    let mask = ZGoodMask::new();
    for phase in 0..6 {
        if phase % 2 == 0 {
            mask.flip_to_mark();
        } else {
            mask.flip_to_remap();
        }
        for c in ZColor::ALL {
            let w = color(0x2000, c);
            assert_eq!(
                mask.is_good(w),
                !mask.is_bad(w),
                "good/bad disagree for {c:?} at phase step {phase}"
            );
        }
    }
}

/// The asymmetry that makes ZGC's compiled barrier use the bad mask.
#[test]
fn null_is_bad_free_but_not_good() {
    let mask = ZGoodMask::new();
    assert!(
        !mask.is_bad(Z_NULL),
        "null must pass the bad-mask fast path"
    );
    assert!(
        !mask.is_good(Z_NULL),
        "null cannot pass the good-mask fast path without a null check"
    );
}

#[test]
fn initial_state_is_quiescent_remapped() {
    let mask = ZGoodMask::new();
    assert_eq!(mask.phase(), ZGlobalPhase::Relocate);
    assert_eq!(mask.good(), Z_REMAPPED);
    assert_eq!(mask.bad(), Z_MARKED0 | Z_MARKED1 | Z_FINALIZABLE);
    assert_eq!(mask.allocation_color(), ZColor::Remapped);
}

/// A full mark → remap → mark cycle, checking the rotation and that each
/// phase's good color is what the barrier should let through.
#[test]
fn full_cycle_rotation() {
    let mask = ZGoodMask::new();

    // Cycle 0: the first mark uses Marked0.
    assert_eq!(mask.flip_to_mark(), Z_MARKED0);
    assert_eq!(mask.phase(), ZGlobalPhase::Mark);
    assert_eq!(mask.good(), Z_MARKED0);
    assert!(mask.is_good(color(16, ZColor::Marked0)));
    // Remapped is BAD during mark -- this is what feeds the mark stack.
    assert!(mask.is_bad(color(16, ZColor::Remapped)));
    assert!(mask.is_bad(color(16, ZColor::Marked1)));
    assert_eq!(mask.allocation_color(), ZColor::Marked0);

    mask.set_mark_complete();
    assert_eq!(mask.phase(), ZGlobalPhase::MarkComplete);
    assert_eq!(mask.good(), Z_MARKED0, "mark-complete must not move colors");

    // Relocate: Remapped is good again, Marked0 is now stale.
    assert_eq!(mask.flip_to_remap(), Z_REMAPPED);
    assert_eq!(mask.phase(), ZGlobalPhase::Relocate);
    assert!(mask.is_good(color(16, ZColor::Remapped)));
    assert!(mask.is_bad(color(16, ZColor::Marked0)));
    assert_eq!(mask.allocation_color(), ZColor::Remapped);

    // Cycle 1: rotates to Marked1, and last cycle's Marked0 is stale
    // without any heap pass having cleared it.
    assert_eq!(mask.flip_to_mark(), Z_MARKED1);
    assert!(mask.is_good(color(16, ZColor::Marked1)));
    assert!(mask.is_bad(color(16, ZColor::Marked0)));
    assert_eq!(mask.allocation_color(), ZColor::Marked1);

    // Cycle 2 rotates back to Marked0.
    mask.flip_to_remap();
    assert_eq!(mask.flip_to_mark(), Z_MARKED0);
}

/// **The two phase machines, pinned** — finding (3) of
/// `gap-c-colored-pointers-are-encoded-but-never-stored.md`.
///
/// `ZGoodMask` and `ZgcRealHeap::mark_active` both answer "is it marking",
/// and they are allowed to disagree in exactly one window: from
/// `set_mark_complete` to `flip_to_remap`. This test is what turns that
/// from a sentence in a doc into a failure when somebody derives one from
/// the other. The heap-side boolean is modelled rather than imported,
/// because `ZgcRealHeap` is not reachable from this module — what is being
/// pinned is the mask's half of the contract and the shape of the window.
#[test]
fn good_mask_and_mark_active_disagree_only_across_mark_complete() {
    let mask = ZGoodMask::new();
    // Model of the SATB window: armed inside the mark-start pause,
    // disarmed at mark end.
    let mut satb_armed = false;

    // Idle. Neither says marking.
    assert!(!mask.marking());
    assert_eq!(mask.marking(), satb_armed);

    // Mark start: both flip, in the same pause, and agree.
    satb_armed = true;
    mask.flip_to_mark();
    assert!(mask.marking());
    assert_eq!(mask.marking(), satb_armed);

    // Mark end: SATB stops, the colours do NOT. Pointers the mutator holds
    // are still this cycle's mark colour and nothing has moved, so calling
    // them stale here would send every load to the slow path for nothing.
    satb_armed = false;
    mask.set_mark_complete();
    assert!(
        mask.marking(),
        "mark-complete must not move the good colour -- see set_mark_complete",
    );
    assert_ne!(
        mask.marking(),
        satb_armed,
        "THIS is the window the gap page is about: between mark end and the remap \
         flip, 'is it marking' has two right answers. A wiring that derives one \
         boolean from the other either re-arms SATB for the relocate pause or \
         calls Remapped good while the mark bits are still live.",
    );

    // Relocate: the colours flip and the two agree again.
    mask.flip_to_remap();
    assert!(!mask.marking());
    assert_eq!(mask.marking(), satb_armed);
}

/// `marking()` is derived from `good()` and from nothing else, so it can
/// never report a half-flipped state — the property the whole type is
/// built around.
#[test]
fn marking_tracks_the_good_colour_across_both_mark_bits() {
    let mask = ZGoodMask::new();
    for cycle in 0..4 {
        mask.flip_to_mark();
        assert!(mask.marking(), "cycle {cycle}");
        assert_ne!(mask.good(), Z_REMAPPED, "cycle {cycle}");
        assert_eq!(
            mask.good(),
            mask.current_marked_bit(),
            "cycle {cycle}: the good colour during mark IS the cycle's mark bit",
        );
        mask.set_mark_complete();
        assert!(
            mask.marking(),
            "cycle {cycle}: mark-complete holds the colour"
        );
        mask.flip_to_remap();
        assert!(!mask.marking(), "cycle {cycle}");
    }
}

#[test]
fn rotation_alternates_over_many_cycles() {
    let mask = ZGoodMask::new();
    for cycle in 0..16u64 {
        let expected = if cycle % 2 == 0 { Z_MARKED0 } else { Z_MARKED1 };
        assert_eq!(mask.flip_to_mark(), expected, "cycle {cycle}");
        mask.set_mark_complete();
        assert_eq!(mask.flip_to_remap(), Z_REMAPPED, "cycle {cycle}");
    }
}

#[test]
fn weak_bad_tolerates_remapped_and_finalizable() {
    let mask = ZGoodMask::new();
    mask.flip_to_mark(); // good = Marked0
                         // A strong load through a merely-Remapped or Finalizable pointer must
                         // take the slow path...
    assert!(mask.is_bad(color(32, ZColor::Remapped)));
    assert!(mask.is_bad(color(32, ZColor::Finalizable)));
    // ...but a weak load must not, or Reference.get would resurrect.
    assert!(!mask.is_weak_bad(color(32, ZColor::Remapped)));
    assert!(!mask.is_weak_bad(color(32, ZColor::Finalizable)));
    assert!(!mask.is_weak_bad(color(32, ZColor::Marked0)));
    // The stale mark bit is bad even for a weak load.
    assert!(mask.is_weak_bad(color(32, ZColor::Marked1)));
}

#[test]
fn bad_mask_is_the_complement_of_good_within_metadata() {
    let mask = ZGoodMask::new();
    for _ in 0..4 {
        mask.flip_to_mark();
        assert_eq!(mask.good() | mask.bad(), Z_METADATA_MASK);
        assert_eq!(mask.good() & mask.bad(), 0);
        mask.flip_to_remap();
        assert_eq!(mask.good() | mask.bad(), Z_METADATA_MASK);
        assert_eq!(mask.good() & mask.bad(), 0);
    }
}

// -- address space ------------------------------------------------------

#[test]
fn address_space_basic_translation() {
    let vas = ZVirtualAddressSpace::new(TEST_BASE, 64 * MIB).expect("valid");
    assert_eq!(vas.base(), TEST_BASE);
    assert_eq!(vas.size(), 64 * MIB);
    assert_eq!(vas.end(), TEST_BASE + 64 * MIB);

    assert_eq!(vas.offset_for_address(TEST_BASE), Some(0));
    assert_eq!(vas.offset_for_address(TEST_BASE + 4096), Some(4096));
    assert_eq!(vas.offset_for_address(TEST_BASE - 8), None);
    assert_eq!(vas.offset_for_address(vas.end()), None);
    assert_eq!(vas.address_for_offset(0), Some(TEST_BASE));
    assert_eq!(vas.address_for_offset(64 * MIB), None);
}

#[test]
fn address_colored_roundtrip() {
    let vas = ZVirtualAddressSpace::new(TEST_BASE, 16 * MIB).expect("valid");
    for i in 0..256u64 {
        let addr = TEST_BASE + i * 4096;
        for c in ZColor::ALL {
            let w = vas.color_address(addr, c).expect("in range");
            assert_eq!(vas.uncolor(w), Some(addr));
            assert_eq!(vas.uncolor_unchecked(w), addr);
            assert_eq!(color_of(w), Some(c));
        }
    }
}

#[test]
fn null_address_roundtrips_as_null_word() {
    let vas = ZVirtualAddressSpace::new(TEST_BASE, MIB).expect("valid");
    assert_eq!(vas.color_address(0, ZColor::Remapped), Some(Z_NULL));
    assert_eq!(vas.uncolor(Z_NULL), Some(0));
}

#[test]
fn out_of_range_address_is_refused_not_wrapped() {
    let vas = ZVirtualAddressSpace::new(TEST_BASE, MIB).expect("valid");
    assert_eq!(vas.color_address(TEST_BASE - 8, ZColor::Remapped), None);
    assert_eq!(vas.color_address(TEST_BASE + MIB, ZColor::Remapped), None);
    // A word encoding an offset past the end decodes to None rather than a
    // wild address.
    let bogus = color(MIB + 4096, ZColor::Remapped);
    assert_eq!(vas.uncolor(bogus), None);
}

// -- max heap size boundary ---------------------------------------------

#[test]
fn max_heap_size_is_four_tebibytes() {
    assert_eq!(Z_MAX_HEAP_SIZE, 4 * TIB);
    assert_eq!(ZVirtualAddressSpace::MAX_HEAP_SIZE, 4 * TIB);
    assert_eq!(Z_MAX_HEAP_SIZE, 1u64 << 42);
}

#[test]
fn heap_exactly_at_max_is_accepted() {
    // Base must be low enough that base + 4 TiB stays inside the 47-bit
    // window; 0x1000 is the smallest aligned base above the null page.
    let vas = ZVirtualAddressSpace::new(0x1000, Z_MAX_HEAP_SIZE).expect("max heap");
    assert_eq!(vas.size(), Z_MAX_HEAP_SIZE);
    // The very last addressable byte still round-trips.
    let last = 0x1000 + Z_MAX_HEAP_SIZE - 8;
    let w = vas.color_address(last, ZColor::Marked0).expect("in range");
    assert_eq!(vas.uncolor(w), Some(last));
}

#[test]
fn heap_one_byte_over_max_is_refused() {
    let err = ZVirtualAddressSpace::new(0x1000, Z_MAX_HEAP_SIZE + 1).unwrap_err();
    assert_eq!(
        err,
        ZAddressSpaceError::HeapTooLarge {
            requested: Z_MAX_HEAP_SIZE + 1,
            max: Z_MAX_HEAP_SIZE,
        }
    );
    // And it must be reported, not silently truncated to a working heap.
    assert!(err.to_string().contains("exceeds"));
}

#[test]
fn zero_size_is_refused() {
    assert_eq!(
        ZVirtualAddressSpace::new(TEST_BASE, 0).unwrap_err(),
        ZAddressSpaceError::ZeroSize
    );
}

#[test]
fn bad_base_is_refused() {
    assert_eq!(
        ZVirtualAddressSpace::new(0, MIB).unwrap_err(),
        ZAddressSpaceError::BadBase { base: 0 }
    );
    assert_eq!(
        ZVirtualAddressSpace::new(TEST_BASE + 1, MIB).unwrap_err(),
        ZAddressSpaceError::BadBase {
            base: TEST_BASE + 1
        }
    );
}

#[test]
fn range_leaving_the_47_bit_window_is_refused() {
    // A base near the top of the 47-bit window plus any real heap.
    let base = (Z_MAX_ADDRESS & !0xFFF) - 4096;
    assert_eq!(
        ZVirtualAddressSpace::new(base, 64 * MIB).unwrap_err(),
        ZAddressSpaceError::RangeOutOfBounds {
            base,
            size: 64 * MIB
        }
    );
}

#[test]
fn for_region_matches_new() {
    let ptr = TEST_BASE as *const u8;
    let a = ZVirtualAddressSpace::for_region(ptr, (8 * MIB) as usize).expect("valid");
    let b = ZVirtualAddressSpace::new(TEST_BASE, 8 * MIB).expect("valid");
    assert_eq!(a, b);
}

// -- phase enum ---------------------------------------------------------

#[test]
fn phase_from_u8_defaults_to_the_safe_phase() {
    assert_eq!(ZGlobalPhase::from_u8(0), ZGlobalPhase::Relocate);
    assert_eq!(ZGlobalPhase::from_u8(1), ZGlobalPhase::Mark);
    assert_eq!(ZGlobalPhase::from_u8(2), ZGlobalPhase::MarkComplete);
    assert_eq!(ZGlobalPhase::from_u8(200), ZGlobalPhase::Relocate);
}

// -- compiled bad-mask cell (zt t21/zb4) ---------------------------------

/// The cell tracks the good mask through a whole two-cycle rotation, its
/// published mask is always `ZGoodMask::bad`, and its address is stable
/// and 8-aligned (a memory operand compiled code can test).
#[test]
fn zt21zb4_the_compiled_bad_mask_cell_follows_every_flip() {
    let good = ZGoodMask::new();
    let cell = ZCompiledBadMask::new();
    let addr = cell.word_address();
    assert_eq!(addr % 8, 0);
    assert_eq!(addr % 64, 0, "cache-line aligned");
    assert_eq!(
        cell.load(),
        good.bad(),
        "quiescent cell = quiescent bad mask"
    );
    assert_eq!(cell.load(), Z_MARKED0 | Z_MARKED1 | Z_FINALIZABLE);

    assert_eq!(good.flip_to_mark_publishing(&cell), Z_MARKED0);
    assert_eq!(cell.load(), good.bad());
    assert_ne!(cell.load() & Z_REMAPPED, 0, "Remapped is bad while marking");
    good.set_mark_complete();
    assert_eq!(cell.load(), good.bad(), "mark end changes no color");

    assert_eq!(good.flip_to_remap_publishing(&cell), Z_REMAPPED);
    assert_eq!(cell.load(), good.bad());
    assert_ne!(cell.load() & Z_MARKED0, 0, "last cycle's mark bit is bad");

    assert_eq!(
        good.flip_to_mark_publishing(&cell),
        Z_MARKED1,
        "cycle 1 rotates"
    );
    assert_eq!(cell.load(), good.bad());
    assert_ne!(cell.load() & Z_MARKED0, 0, "cycle 0's mark bit stays bad");
    assert_eq!(cell.word_address(), addr, "the address never moves");
}

/// Under every phase of the rotation, a word colored with the good color
/// passes the cell's test and every other color fails it -- the property
/// the stage-2e inline barrier's `TEST RAX, [cell]` relies on.
#[test]
fn zt21zb4_exactly_the_good_color_passes_the_cell() {
    let good = ZGoodMask::new();
    let cell = ZCompiledBadMask::new();
    let colors = [
        ZColor::Marked0,
        ZColor::Marked1,
        ZColor::Remapped,
        ZColor::Finalizable,
    ];
    let check = |phase: &str| {
        for c in colors {
            let w = color(0x1238, c);
            let passes = w & cell.load() == 0;
            assert_eq!(passes, c.bit() == good.good(), "{phase}: {c:?}");
        }
    };
    check("quiescent");
    good.flip_to_mark_publishing(&cell);
    check("mark 0");
    good.flip_to_remap_publishing(&cell);
    check("remap 0");
    good.flip_to_mark_publishing(&cell);
    check("mark 1");
}
