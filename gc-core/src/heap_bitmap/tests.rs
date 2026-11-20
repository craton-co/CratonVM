use super::*;

/// The block summary skips the never-bumped middle without changing an
/// answer: every probe agrees with a plain word-by-word backward scan, on
/// a bitmap with starts at both ends, one in the middle, every writer
/// (`insert`, `claim`, `set_range`) and probes on, just above and far
/// above each set bit and across block boundaries (2026-10-07,
/// `first-segment-access-starves-under-back-to-back-system-gc`).
#[test]
fn young_bases_in_matches_a_full_walk_and_forgets_old_only_blocks() {
    const BASE: usize = 0x10_0000;
    const BLOCK_BYTES: usize = BLOCK_WORDS * 64 * 8;
    let span = 8 * BLOCK_BYTES;
    let (reg, old) = (HeapBitmap::new(BASE, span), HeapBitmap::new(BASE, span));
    // block 0: young + old; block 2: old only; block 5: young only.
    let (y0, o0, o2, y5) = (BASE + 64, BASE + 128, BASE + 2 * BLOCK_BYTES, BASE + 5 * BLOCK_BYTES);
    for a in [y0, o0, o2, y5] {
        reg.insert(a);
    }
    for a in [o0, o2] {
        old.insert(a);
    }
    let mut got = Vec::new();
    assert_eq!(reg.young_bases_in(&old, BASE, BASE + span, true, &mut got), 3);
    assert_eq!(got, vec![y0, y5]);
    // Block 2 held only an old base: forgotten. The rest keep their bit.
    got.clear();
    assert_eq!(reg.young_bases_in(&old, BASE, BASE + span, true, &mut got), 2);
    assert_eq!(got, vec![y0, y5]);
    // The range filter applies; an insert into a forgotten block re-flags it.
    got.clear();
    reg.insert(o2 + 8);
    assert_eq!(reg.young_bases_in(&old, BASE + BLOCK_BYTES, BASE + 4 * BLOCK_BYTES, true, &mut got), 1);
    assert_eq!(got, vec![o2 + 8]);
}

#[test]
fn nearest_base_skips_empty_blocks_and_agrees_with_a_plain_scan() {
    const BASE: usize = 0x4000_0000;
    let span = 64 * 1024 * 1024; // 131 072 words, 2 048 blocks
    let bm = HeapBitmap::labelled(BASE, span, "test");
    bm.insert(BASE);
    bm.insert(BASE + 8 * 77);
    bm.claim(BASE + 40 * 1024 * 1024 + 16);
    bm.set_range(BASE + 32 * 1024 - 64, BASE + 32 * 1024 + 64); // straddles a block edge
    bm.insert(BASE + span - 8);
    let plain = |addr: usize| -> Option<usize> {
        let mut a = addr.min(BASE + span - 1) & !7;
        loop {
            if bm.contains(a) {
                return Some(a);
            }
            if a == BASE {
                return None;
            }
            a -= 8;
        }
    };
    let probes = [
        BASE,
        BASE + 8,
        BASE + 8 * 77,
        BASE + 8 * 78,
        BASE + 32 * 1024 - 72,
        BASE + 32 * 1024 - 64,
        BASE + 32 * 1024 + 56,
        BASE + 32 * 1024 + 64,
        BASE + 1024 * 1024,
        BASE + 40 * 1024 * 1024 + 8,
        BASE + 40 * 1024 * 1024 + 16,
        BASE + 40 * 1024 * 1024 + 4096 * 8,
        BASE + span - 16,
        BASE + span - 8,
        BASE + span + 4096,
    ];
    for addr in probes {
        assert_eq!(
            bm.nearest_base_at_or_below(addr),
            plain(addr),
            "addr {:#x}",
            addr
        );
    }
    // Nothing at or below: the empty low side of a fresh bitmap.
    let empty = HeapBitmap::labelled(BASE, span, "test");
    empty.insert(BASE + span / 2);
    assert_eq!(empty.nearest_base_at_or_below(BASE + span / 2 - 8), None);
    assert_eq!(
        empty.nearest_base_at_or_below(BASE + span - 8),
        Some(BASE + span / 2)
    );
}

/// `snapshot_word_runs` copies exactly the merged word ranges, and
/// `retain_marked_within` prunes exactly what `retain_marked` would when the
/// ranges hold every start.
#[test]
fn word_runs_and_bounded_retain_match_the_whole_bitmap_forms() {
    const BASE: usize = 0x4000_0000;
    let span = 1024 * 1024;
    let starts = HeapBitmap::labelled(BASE, span, "test");
    let marks = HeapBitmap::labelled(BASE, span, "test");
    for off in [0usize, 8, 512, 4096, 8192] {
        starts.insert(BASE + off);
    }
    starts.insert(BASE + span - 8);
    marks.insert(BASE + 8);
    marks.insert(BASE + span - 8);
    let ranges = [
        (BASE, BASE + 16 * 1024),
        (BASE + span - 4096, BASE + span),
        (BASE + 100, BASE + 200),
    ];
    let runs = starts.snapshot_word_runs(&ranges);
    assert_eq!(
        runs.len(),
        2,
        "the overlapping third range merges into the first"
    );
    for (w0, words) in &runs {
        for (i, word) in words.iter().enumerate() {
            assert_eq!(*word, starts.word_at(w0 + i));
        }
    }
    starts.retain_marked_within(&marks, &ranges[..2]);
    for w in 0..starts.word_count() {
        assert_eq!(starts.word_at(w), marks.word_at(w), "word {w}");
    }
}

/// `word_range(word_base(lo), word_base(hi))` must return `(lo, hi)`.
///
/// Checked at a span that is NOT a multiple of 512, which is where the two
/// functions could disagree: `word_base(nwords)` runs past `base + span`
/// and `word_range` clamps it back, so the round trip survives only
/// because the clamp lands inside the same final word.
#[test]
fn the_word_index_to_address_round_trip_is_exact() {
    for span in [512usize, 4096, 1000, 1 << 16, 8] {
        let bm = HeapBitmap::labelled(0x10_0000, span, "test");
        let n = bm.word_count();
        for lo in 0..n {
            for hi in (lo + 1)..=n {
                let (a, b) = bm.word_range(bm.word_base(lo), bm.word_base(hi));
                assert_eq!(
                    (a, b),
                    (lo, hi),
                    "span={span} nwords={n}: words ({lo},{hi}) did not round trip"
                );
            }
        }
    }
}

/// A `clear_within` whose range covers every set bit wipes all of them,
/// while storing into only the words it was given.
///
/// This is exactly the contract `MarkBitmap::clear` now leans on: it
/// converts its marked-word WINDOW into this address range, and the
/// window is a superset of the set bits by construction. Note the shape of
/// the test — it does NOT set a bit outside the range and check it
/// survives, because `clear_within` opens with
/// `debug_assert_clear_outside`, so a caller that leaves a bit outside its
/// bounds is a debug-build panic rather than an observable behaviour.
/// Every word outside is already zero; the saving is that it stays
/// un-stored-to.
#[test]
fn clear_within_wipes_every_bit_inside_its_range() {
    let bm = HeapBitmap::labelled(0x10_0000, 1 << 16, "test");
    let base = bm.base;
    assert_eq!(bm.word_count(), 128, "1<<16 bytes is 128 bitmap words");

    // Three bases inside words 5..8, i.e. arena bytes 2560..4096.
    let addrs = [base + 5 * 512, base + 6 * 512 + 24, base + 7 * 512 + 504];
    for a in addrs {
        bm.insert(a);
    }
    assert!(!bm.has_spill(), "every address is on the 8-byte grid");
    for a in addrs {
        assert!(bm.contains(a), "{a:#x} should be registered");
    }

    // The window a caller would derive: words 5..8, 3 of 128.
    bm.clear_within(&[(bm.word_base(5), bm.word_base(8))]);
    for a in addrs {
        assert!(!bm.contains(a), "{a:#x} survived a clear that covered it");
    }

    // A second clear over the same range is a no-op, and a full-range one
    // still works on an already-empty bitmap (the degenerate case
    // `MarkBitmap::clear`'s `lo >= hi` arm falls back to).
    bm.clear_within(&[(bm.word_base(5), bm.word_base(8))]);
    bm.clear_all();
    for a in addrs {
        assert!(!bm.contains(a));
    }
}

/// gc-common w3-e: the `&mut` (memset) clears remove exactly what the
/// atomic ones do -- the covered words, and the overflow spill set -- and
/// leave words outside a bounded range alone.
#[test]
fn exclusive_clears_match_the_atomic_ones() {
    let mut bm = HeapBitmap::labelled(0x10_0000, 1 << 16, "test");
    let base = bm.base;
    let inside = [base + 5 * 512, base + 7 * 512 + 504];
    let outside = base + 20 * 512 + 8;
    for a in inside {
        bm.insert(a);
    }
    bm.insert(outside);
    // A bounded clear must not reach word 20. (Debug builds would trip
    // `debug_assert_clear_outside` on it, so the range covers it too and
    // the check is on what a SECOND, narrower clear leaves.)
    bm.clear_within_exclusive(&[(bm.word_base(5), bm.word_base(21))]);
    for a in inside.iter().copied().chain([outside]) {
        assert!(!bm.contains(a), "{a:#x} survived the bounded memset");
    }
    bm.insert(inside[0]);
    bm.spill(base + 3);
    assert!(bm.has_spill());
    bm.clear_all_exclusive();
    assert!(!bm.contains(inside[0]));
    assert!(!bm.has_spill(), "the overflow set must be cleared too");
    assert!((0..bm.word_count()).all(|w| bm.word_at(w) == 0));
}

/// zt t1/tp (zr4rv4 mark-pool item 4): with an off-grid address spilled,
/// an in-span address is answered by its bit (no overflow probe), and the
/// spilled and out-of-span addresses still by the set.
#[test]
fn zt1tp_a_spill_does_not_change_in_span_answers() {
    let bm = HeapBitmap::labelled(0x10_0000, 1 << 16, "test");
    let base = bm.base;
    bm.insert(base + 64);
    bm.insert(base + 3); // off the grid: spilled
    bm.insert(base + (1 << 16) + 8); // past the span: spilled
    assert!(bm.has_spill());
    assert!(bm.contains(base + 64));
    assert!(!bm.contains(base + 72), "unmarked in-span slot");
    assert!(bm.contains(base + 3));
    assert!(bm.contains(base + (1 << 16) + 8));
    assert!(!bm.contains(base + 5), "an off-grid address never spilled");
}

/// gen r5w5/old9: `set_range` sets every slot whose BASE lies in
/// `[lo, hi)` — including the slot at `floor8(hi)` when `hi` is not on the
/// grid, which the old `hi_off / 8` dropped — and nothing at or past the
/// first slot at or above `hi`, in one word and across words.
#[test]
fn set_range_covers_the_last_slot_of_an_unaligned_end() {
    let cases: [(usize, usize); 4] = [
        (5, 17 * 8 + 3),
        (5, 200 * 8 + 1),
        (0, 64 * 8 + 7),
        (3, 4 * 8),
    ];
    for (lo_slot, hi_bytes) in cases {
        let bm = HeapBitmap::labelled(0x10_0000, 1 << 16, "test");
        let base = bm.base;
        bm.set_range(base + lo_slot * 8, base + hi_bytes);
        let end_slot = hi_bytes.div_ceil(8);
        for slot in 0..(end_slot + 70) {
            assert_eq!(
                bm.contains(base + slot * 8),
                slot >= lo_slot && slot < end_slot,
                "range [{lo_slot} slots, {hi_bytes} bytes): slot {slot}",
            );
        }
    }
}

/// nio20 wave 6, lane L2: `nearest_base_in(addr, floor)` is exactly
/// `nearest_base_at_or_below(addr)` restricted to `>= floor`, for every
/// address and floor over sparse and dense layouts -- including the
/// four-word skip's edges (runs of empty words of every length mod 4,
/// a floor inside, at and past a populated word) -- and `floor == 0` is
/// the unbounded scan. Checked against a brute-force walk over a plain
/// `Vec`, so the reference cannot share a bug with the bit scan.
#[test]
fn nearest_base_in_is_the_floored_nearest_base() {
    const BASE: usize = 0x40_0000;
    const SPAN: usize = 64 * 512; // 64 words
                                  // Bases chosen so the gaps between populated words are 0..=9 words.
    let layouts: [&[usize]; 4] = [
        &[],
        &[
            8,
            504,
            512,
            3 * 512 + 16,
            4 * 512,
            9 * 512 + 8,
            15 * 512,
            20 * 512 + 256,
            27 * 512,
            36 * 512 + 8,
            46 * 512,
            63 * 512 + 504,
        ],
        &[0],
        &[63 * 512 + 504],
    ];
    for bases in layouts {
        let bm = HeapBitmap::labelled(BASE, SPAN, "test");
        for &b in bases {
            bm.insert(BASE + b);
        }
        let truth = |addr: usize, floor: usize| -> Option<usize> {
            bases
                .iter()
                .map(|&b| BASE + b)
                .filter(|&b| b <= addr && b >= floor)
                .max()
        };
        // Every third grid slot plus a few misaligned and out-of-span
        // probes (every slot of every floor is ~10M calls in a debug run).
        let mut addrs: Vec<usize> = (0..SPAN / 8).step_by(3).map(|s| BASE + s * 8).collect();
        addrs.extend([
            BASE + 3,
            BASE + 512 + 7,
            BASE + SPAN,
            BASE + SPAN + 4096,
            BASE - 8,
        ]);
        for &addr in &addrs {
            assert_eq!(
                bm.nearest_base_in(addr, 0),
                bm.nearest_base_at_or_below(addr),
                "floor 0 must be the unbounded scan at {addr:#x}"
            );
            for floor_slot in (0..=SPAN / 8 + 2).step_by(13) {
                let floor = BASE + floor_slot * 8;
                assert_eq!(
                    bm.nearest_base_in(addr, floor),
                    truth(addr, floor),
                    "addr={addr:#x} floor={floor:#x} bases={bases:?}"
                );
            }
            // A floor below the bitmap is no floor at all.
            assert_eq!(bm.nearest_base_in(addr, 0x1000), truth(addr, 0));
        }
    }
}

/// nio21 wave 1 L1: the floored scan over SEVERAL summary blocks, where
/// the four-word skip now stops at each block edge so the block-summary
/// skip can jump never-noted blocks. Bases sit in blocks 0, 3 and 7 of
/// eight (one at each block's first and last slot too), every other
/// block is never noted, and one block is noted but emptied again (a
/// swept stretch: its summary bit stays set). Checked against a
/// brute-force walk, for addresses in every block and floors in, at and
/// between the populated blocks.
#[test]
fn nearest_base_in_crosses_never_noted_blocks() {
    const BASE: usize = 0x80_0000;
    const BLOCK_BYTES: usize = BLOCK_WORDS * 512;
    const SPAN: usize = 8 * BLOCK_BYTES;
    let bases: [usize; 6] = [
        8,
        3 * BLOCK_BYTES,
        3 * BLOCK_BYTES + 17 * 512 + 40,
        4 * BLOCK_BYTES - 8,
        7 * BLOCK_BYTES + 5 * 512,
        8 * BLOCK_BYTES - 8,
    ];
    let bm = HeapBitmap::labelled(BASE, SPAN, "test");
    for &b in &bases {
        bm.insert(BASE + b);
    }
    // Block 5: noted, then emptied (its summary bit stays set).
    let swept = BASE + 5 * BLOCK_BYTES + 512;
    bm.insert(swept);
    bm.remove(swept);
    let truth = |addr: usize, floor: usize| -> Option<usize> {
        bases
            .iter()
            .map(|&b| BASE + b)
            .filter(|&b| b <= addr && b >= floor)
            .max()
    };
    let mut addrs: Vec<usize> = (0..SPAN / 8).step_by(61).map(|s| BASE + s * 8).collect();
    addrs.extend(bases.iter().map(|&b| BASE + b));
    addrs.extend([
        BASE + SPAN - 8,
        BASE + SPAN,
        BASE + 6 * BLOCK_BYTES - 8,
        BASE + 4 * BLOCK_BYTES,
    ]);
    let floors: Vec<usize> = (0..=8)
        .flat_map(|blk| {
            [
                blk * BLOCK_BYTES,
                blk * BLOCK_BYTES + 8,
                (blk * BLOCK_BYTES).saturating_sub(8),
            ]
        })
        .map(|o| BASE + o)
        .chain([0, BASE + 3 * BLOCK_BYTES + 17 * 512 + 48])
        .collect();
    for &addr in &addrs {
        for &floor in &floors {
            assert_eq!(
                bm.nearest_base_in(addr, floor),
                truth(addr, floor),
                "addr={addr:#x} floor={floor:#x}"
            );
        }
    }
}

/// nio21 wave 9 L3: `forget_empty_blocks` clears exactly the noted blocks
/// whose words are all zero (a swept stretch), keeps every populated one,
/// changes no `nearest_base_in` answer, and a later insert into a
/// forgotten block notes it again and is found.
#[test]
fn forget_empty_blocks_clears_only_swept_blocks_and_keeps_answers() {
    const BASE: usize = 0x80_0000;
    const BLOCK_BYTES: usize = BLOCK_WORDS * 512;
    const SPAN: usize = 8 * BLOCK_BYTES;
    let bm = HeapBitmap::labelled(BASE, SPAN, "test");
    let live: Vec<usize> = vec![
        BASE + 8,
        BASE + 3 * BLOCK_BYTES - 8,
        BASE + 6 * BLOCK_BYTES + 4096,
    ];
    for &b in &live {
        bm.insert(b);
    }
    // Blocks 1, 4 and 5 are noted and then swept empty (block 4 at its
    // first slot, block 5 at its last word, block 1 by a range).
    let swept = [BASE + 4 * BLOCK_BYTES, BASE + 6 * BLOCK_BYTES - 8];
    for &s in &swept {
        bm.insert(s);
        bm.remove(s);
    }
    let ranged = BASE + BLOCK_BYTES + 512;
    bm.set_range(ranged, ranged + 64);
    for a in (ranged..ranged + 64).step_by(8) {
        bm.remove(a);
    }
    for blk in [0, 1, 2, 4, 5, 6] {
        assert!(bm.block_noted(blk), "block {blk} noted before the pass");
    }
    assert_eq!(bm.forget_empty_blocks(), 3, "blocks 1, 4 and 5 are empty");
    for (blk, noted) in [
        (0, true),
        (1, false),
        (2, true),
        (3, false),
        (4, false),
        (5, false),
        (6, true),
        (7, false),
    ] {
        assert_eq!(bm.block_noted(blk), noted, "block {blk}");
    }
    assert_eq!(bm.forget_empty_blocks(), 0, "idempotent");
    let truth = |live: &[usize], addr: usize, floor: usize| -> Option<usize> {
        live.iter()
            .copied()
            .filter(|&b| b <= addr && b >= floor)
            .max()
    };
    let check = |live: &[usize]| {
        for slot in (0..SPAN / 8 + 4).step_by(29) {
            let addr = BASE + slot * 8;
            for floor in [0, BASE, BASE + 2 * BLOCK_BYTES, BASE + 5 * BLOCK_BYTES + 8] {
                assert_eq!(
                    bm.nearest_base_in(addr, floor),
                    truth(live, addr, floor),
                    "addr={addr:#x} floor={floor:#x}"
                );
            }
        }
    };
    check(&live[..]);
    // A new allocation in a forgotten block re-notes it before its bit.
    let reused = BASE + 4 * BLOCK_BYTES + 1024;
    bm.insert(reused);
    assert!(bm.block_noted(4));
    let mut live2 = live.clone();
    live2.push(reused);
    check(&live2[..]);
}

/// nio21 wave 9 L3: a writer that sets a word bit WITHOUT `insert` (the
/// JIT's inline start-bit store) is invisible to the summary-bounded scan
/// unless its range was noted first, and visible once it was. This is
/// the invariant the VM-TLAB carve now restores with
/// `note_blocks_covering`.
#[test]
fn a_raw_word_store_needs_its_block_noted_first() {
    const BASE: usize = 0x80_0000;
    const BLOCK_BYTES: usize = BLOCK_WORDS * 512;
    const SPAN: usize = 8 * BLOCK_BYTES;
    let raw_store = |bm: &HeapBitmap, addr: usize| {
        let (w, mask) = bm.locate(addr).expect("on grid");
        // SAFETY: `locate` bounds-checked `w`.
        unsafe { (*bm.words.add(w)).fetch_or(mask, Ordering::Release) };
    };
    let obj = BASE + 5 * BLOCK_BYTES + 3 * 512 + 64;
    let probe = BASE + 7 * BLOCK_BYTES; // two blocks above, past a block edge

    let unnoted = HeapBitmap::labelled(BASE, SPAN, "test");
    unnoted.insert(BASE + 8);
    raw_store(&unnoted, obj);
    assert_eq!(
        unnoted.nearest_base_in(probe, 0),
        Some(BASE + 8),
        "the documented hazard: an un-noted block is skipped"
    );

    let noted = HeapBitmap::labelled(BASE, SPAN, "test");
    noted.insert(BASE + 8);
    noted.note_blocks_covering(BASE + 4 * BLOCK_BYTES + 512, BASE + 6 * BLOCK_BYTES + 512);
    for blk in [4, 5, 6] {
        assert!(noted.block_noted(blk), "block {blk}");
    }
    assert!(!noted.block_noted(7));
    raw_store(&noted, obj);
    assert_eq!(noted.nearest_base_in(probe, 0), Some(obj));
    assert_eq!(noted.nearest_base_in(obj + 100, obj), Some(obj));
    // An empty or out-of-span range notes nothing and does not panic.
    noted.note_blocks_covering(BASE + 7 * BLOCK_BYTES, BASE + 7 * BLOCK_BYTES);
    noted.note_blocks_covering(BASE + SPAN, BASE + SPAN + 4096);
    assert!(!noted.block_noted(7));
}

/// zr z5/sw: the per-cycle reset of the summary forgets exactly the
/// blocks wholly inside the cleared ranges, keeps a straddling block's bit
/// (the conservative direction), and every writer re-notes its block on
/// the next cycle, so `block_may_hold_bits == false` keeps proving "no
/// set word" across a clear.
#[test]
fn forgetting_blocks_after_a_clear_keeps_the_summary_sound() {
    const BASE: usize = 0x80_0000;
    const BLOCK_BYTES: usize = BLOCK_WORDS * 512;
    const SPAN: usize = 8 * BLOCK_BYTES;
    let bm = HeapBitmap::labelled(BASE, SPAN, "test");
    bm.claim(BASE + 8); // block 0
    bm.claim(BASE + 2 * BLOCK_BYTES + 64); // block 2
    bm.set_range(BASE + 5 * BLOCK_BYTES - 64, BASE + 5 * BLOCK_BYTES + 64); // blocks 4, 5
    for blk in [0, 2, 4, 5] {
        assert!(
            bm.block_may_hold_bits(blk),
            "block {blk} noted by its writer"
        );
    }
    assert!(!bm.block_may_hold_bits(1));
    assert!(!bm.block_may_hold_bits(99), "out of range reads as empty");
    // Clear [block 0 .. middle of block 5): blocks 0..=4 are whole, 5 straddles.
    let ranges = [(BASE, BASE + 5 * BLOCK_BYTES + 512)];
    bm.clear_within_sparse(&ranges);
    let forgotten = bm.forget_blocks_within(Some(&ranges));
    assert_eq!(
        forgotten, 3,
        "blocks 0, 2 and 4 were set and wholly cleared"
    );
    for blk in [0, 2, 4] {
        assert!(!bm.block_may_hold_bits(blk), "block {blk} forgotten");
    }
    assert!(
        bm.block_may_hold_bits(5),
        "a straddling block keeps its bit"
    );
    // The next cycle's claim re-notes before it sets the word.
    assert!(bm.claim(BASE + 2 * BLOCK_BYTES + 64));
    assert!(bm.block_may_hold_bits(2));
    // `None` forgets everything (the clear_all arm).
    bm.clear_all();
    assert!(bm.forget_blocks_within(None) >= 2);
    for blk in 0..8 {
        assert!(!bm.block_may_hold_bits(blk), "block {blk} after clear_all");
    }
}

/// zr z10/mt: a bitmap on its own mapping is the same bitmap. It is on a
/// mapping exactly when asked for, big enough and on Linux; it reads zero
/// before any write; every writer and reader answers as on the
/// `alloc_zeroed` twin over the same geometry; and it is released by
/// `Drop` (a second one built after the first is dropped works too).
#[test]
fn zr10mt_a_side_table_bitmap_is_the_same_bitmap_on_its_own_mapping() {
    const BASE: usize = 0x4000_0000;
    let span = 64 * 1024 * 1024; // 1 MiB of words: the threshold
    for _ in 0..2 {
        let mapped = HeapBitmap::labelled_side_table(BASE, span, "test", true);
        let plain = HeapBitmap::labelled(BASE, span, "test");
        assert_eq!(mapped.on_own_mapping(), cfg!(target_os = "linux"));
        assert!(!plain.on_own_mapping());
        assert_eq!(mapped.nwords, plain.nwords);
        assert_eq!(mapped.reserved_bytes(), SIDE_TABLE_MAP_MIN_BYTES);
        assert_eq!(mapped.words as usize % std::mem::align_of::<AtomicU64>(), 0);
        // SAFETY: `words` holds `nwords` `AtomicU64`s for the bitmap's life.
        let words = unsafe { std::slice::from_raw_parts(mapped.words, mapped.nwords) };
        assert!(words.iter().all(|w| w.load(Ordering::Relaxed) == 0));
        for bm in [&mapped, &plain] {
            bm.insert(BASE);
            bm.insert(BASE + 8 * 77);
            assert!(bm.claim(BASE + 40 * 1024 * 1024 + 16));
            bm.insert(BASE + span - 8);
            bm.remove(BASE + 8 * 77);
        }
        for probe in [0, 8, 8 * 77, 40 * 1024 * 1024 + 16, span - 8, span - 16] {
            assert_eq!(
                mapped.contains(BASE + probe),
                plain.contains(BASE + probe),
                "probe +{probe:#x}"
            );
        }
        assert!(mapped.contains(BASE + span - 8));
        mapped.clear_all();
        assert!(!mapped.contains(BASE));
    }
}

/// zt t3/rl (`CRATONVM_ZGC_BITMAP_RELEASE_WITH_GRANULE`): the discard
/// gives back only all-zero pages wholly inside the range, keeps a page
/// with a bit, and leaves an allocator block alone. One 4 KiB page of
/// words covers 256 KiB of arena.
#[test]
fn zt3_rl_discard_gives_back_only_zero_pages_inside_the_range() {
    const BASE: usize = 0x4000_0000;
    const PAGE_ARENA: usize = 256 * 1024;
    let span = 64 * 1024 * 1024;
    let bm = HeapBitmap::labelled_side_table(BASE, span, "test", true);
    bm.insert(BASE + 3 * PAGE_ARENA + 64);
    let released = bm.discard_zero_pages_over(BASE, BASE + 8 * PAGE_ARENA);
    if !bm.on_own_mapping() {
        // Off Linux (or a refused mapping): nothing to give back.
        assert_eq!(released, 0);
        return;
    }
    // Pages 0..8 but page 3; 0 if the kernel's page is not 4 KiB.
    assert!(released == 7 * 4096 || released == 0, "{released}");
    assert!(bm.contains(BASE + 3 * PAGE_ARENA + 64), "the set page kept its bit");
    // No whole page inside: nothing.
    assert_eq!(bm.discard_zero_pages_over(BASE + 8, BASE + PAGE_ARENA - 8), 0);
    // A discarded page reads zero and takes bits again.
    assert!(!bm.contains(BASE + 64));
    bm.insert(BASE + 64);
    assert!(bm.contains(BASE + 64));
    let plain = HeapBitmap::labelled_side_table(BASE, span, "test", false);
    assert_eq!(plain.discard_zero_pages_over(BASE, BASE + span), 0);
}

/// zr z10/mt: the mapping is never taken when not asked for, for a
/// table under the threshold, or for an empty one.
#[test]
fn zr10mt_small_or_unrequested_side_tables_stay_allocator_blocks() {
    const BASE: usize = 0x4000_0000;
    let big = 64 * 1024 * 1024;
    assert!(!HeapBitmap::labelled_side_table(BASE, big, "test", false).on_own_mapping());
    let small = HeapBitmap::labelled_side_table(BASE, big / 2, "test", true);
    assert!(
        !small.on_own_mapping(),
        "512 KiB of words is under the threshold"
    );
    small.insert(BASE + 64);
    assert!(small.contains(BASE + 64));
    let empty = HeapBitmap::labelled_side_table(BASE, 0, "test", true);
    assert!(!empty.on_own_mapping());
    assert_eq!(empty.nwords, 0);
}
