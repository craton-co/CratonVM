use super::*;
use cratonvm_gc_core::heap::SLOT_SIZE;
use cratonvm_types::ClassId;

/// Deterministic xorshift, so a failing seed can be replayed.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// Footprint of a legacy object with `slots` 16-byte cells.
fn object_size(slots: u32) -> usize {
    HEADER_SIZE + SLOT_SIZE * slots as usize
}

/// Footprint of a reference array of `len` elements.
fn ref_array_size(len: u32) -> usize {
    let data = array_data_size(len as usize, ArrayElementType::Reference).unwrap();
    (ARRAY_DATA_OFFSET + data + 7) & !7
}

/// Footprint of an `int[]` of `len` elements.
fn int_array_size(len: u32) -> usize {
    let data = array_data_size(len as usize, ArrayElementType::Int).unwrap();
    (ARRAY_DATA_OFFSET + data + 7) & !7
}

/// One object to lay down: `(footprint, header)`.
fn random_object(rng: &mut Rng) -> (usize, ObjectHeader) {
    match rng.below(5) {
        0 | 1 => {
            let slots = rng.below(6) as u32;
            (
                object_size(slots),
                ObjectHeader::new(
                    ClassId::new(1),
                    ObjectKind::Object,
                    ArrayElementType::Reference,
                    0,
                    slots,
                ),
            )
        }
        2 => {
            // Up to ~4 cards of references, so arrays straddle cards.
            let len = rng.below(260) as u32;
            (
                ref_array_size(len),
                ObjectHeader::new(
                    ClassId::new(2),
                    ObjectKind::Array,
                    ArrayElementType::Reference,
                    len,
                    len,
                ),
            )
        }
        3 => {
            // Occasionally a big one spanning many cards.
            let len = if rng.below(8) == 0 {
                2000 + rng.below(3000) as u32
            } else {
                rng.below(40) as u32
            };
            (
                ref_array_size(len),
                ObjectHeader::new(
                    ClassId::new(2),
                    ObjectKind::Array,
                    ArrayElementType::Reference,
                    len,
                    len,
                ),
            )
        }
        _ => {
            let len = rng.below(300) as u32;
            (
                int_array_size(len),
                ObjectHeader::new(
                    ClassId::new(3),
                    ObjectKind::Array,
                    ArrayElementType::Int,
                    len,
                    len,
                ),
            )
        }
    }
}

/// Allocate one object (its own block) and write its header.
fn alloc_one(og: &mut OldGen, rng: &mut Rng) -> Option<(usize, usize)> {
    let (size, header) = random_object(rng);
    let ptr = og.alloc(size, 8)?;
    // SAFETY: `ptr` is a fresh, zeroed old-gen block of `size` bytes.
    unsafe { std::ptr::write(ptr as *mut ObjectHeader, header) };
    Some((ptr as usize - og.base_ptr() as usize, size))
}

/// A promotion-buffer-shaped block: carve objects end to end from its
/// start, then hand the unused tail back — what `gen_evac` does.
fn alloc_buffer(og: &mut OldGen, rng: &mut Rng, out: &mut Vec<(usize, usize)>) {
    let cap = 4096 + rng.below(8) * 1024;
    let Some(p) = og.alloc_unzeroed_buffer(cap, 8) else {
        return;
    };
    let base = og.base_ptr() as usize;
    let start = p as usize - base;
    let mut cursor = 0usize;
    while rng.below(12) != 0 {
        let (size, header) = random_object(rng);
        if cursor + size > cap {
            break;
        }
        // `gen_evac::old_lab_alloc`'s rule: never leave a tail that cannot
        // go back to the free list (0 or at least a header).
        let rest = cap - cursor - size;
        if rest != 0 && rest < HEADER_SIZE {
            break;
        }
        // SAFETY: `[p + cursor, p + cursor + size)` lies in the buffer.
        unsafe {
            std::ptr::write_bytes(p.add(cursor), 0, size);
            std::ptr::write(p.add(cursor) as *mut ObjectHeader, header);
        }
        out.push((start + cursor, size));
        cursor += size;
    }
    // 0, or at least HEADER_SIZE by the rule above (cap >= 4096 > size).
    let tail = cap - cursor;
    if tail > 0 {
        // SAFETY: `[p + cursor, p + cap)` is the never-written tail of the
        // buffer (the whole buffer when nothing was carved).
        unsafe { og.release_unused_tail(p.add(cursor), tail) };
    }
}

/// The oracle: every object from the unfiltered `walk_objects`, reported
/// the way `walk_card_ranges` promises.
fn oracle(og: &OldGen, ranges: &[(usize, usize)], overlaps: bool) -> Vec<CardRangeHit> {
    let base = og.base_ptr() as usize;
    let mut out = Vec::new();
    for (ptr, size) in og.walk_objects() {
        let o = ptr as usize - base;
        let end = o + size;
        if ranges.iter().any(|&(lo, hi)| o >= lo && o < hi) {
            out.push(CardRangeHit {
                ptr,
                size,
                window: None,
            });
            continue;
        }
        if !overlaps {
            continue;
        }
        // SAFETY: `walk_objects` yields valid headers.
        let h = unsafe { &*(ptr as *const ObjectHeader) };
        if h.kind() != ObjectKind::Array || h.element_type() != ArrayElementType::Reference {
            continue;
        }
        for &(lo, hi) in ranges {
            if lo > o && lo < end {
                out.push(CardRangeHit {
                    ptr,
                    size,
                    window: Some((lo - o, hi.min(end) - o)),
                });
            }
        }
    }
    out
}

/// Random dirty ranges, card-aligned and coalesced exactly the way
/// `scan_dirty_cards_inner` builds them.
fn random_ranges(rng: &mut Rng, len: usize) -> Vec<(usize, usize)> {
    let cards = len / BOT_CARD;
    let mut idx: Vec<usize> = (0..rng.below(24)).map(|_| rng.below(cards)).collect();
    if rng.below(4) == 0 {
        idx.extend([0, cards - 1]);
    }
    idx.sort_unstable();
    idx.dedup();
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for c in idx {
        let (lo, hi) = (c * BOT_CARD, (c + 1) * BOT_CARD);
        if let Some(last) = ranges.last_mut() {
            if last.1 >= lo {
                last.1 = last.1.max(hi);
                continue;
            }
        }
        ranges.push((lo, hi));
    }
    ranges
}

/// THE equivalence test. A randomized old generation — single-object
/// blocks, promotion-buffer blocks with released tails, arrays spanning
/// many cards, frees (so blocks are re-used at different boundaries),
/// coalescing, and a compaction now and then — with walks interleaved
/// between the mutations, so the trusted prefix is extended, cut back by
/// frees and re-derived many times. After every mutation batch the
/// anchored walk must return EXACTLY what the linear walk returns, and,
/// with overlaps, exactly what the unfiltered-walk oracle says.
#[test]
fn the_anchored_walk_matches_the_linear_walk_on_randomized_heaps() {
    for seed in 1..=12u64 {
        let mut rng = Rng(0xD1B5_4A32_D192_ED03 ^ (seed * 0x9E37_79B9));
        let mut og = OldGen::new(512 * 1024);
        og.set_block_offset_table_enabled(true);
        let mut live: Vec<(usize, usize)> = Vec::new();
        for round in 0..60 {
            // Mutate.
            for _ in 0..rng.below(40) {
                match rng.below(10) {
                    0..=4 => {
                        if let Some(o) = alloc_one(&mut og, &mut rng) {
                            live.push(o);
                        }
                    }
                    5 | 6 => alloc_buffer(&mut og, &mut rng, &mut live),
                    _ => {
                        if !live.is_empty() {
                            let (off, size) = live.swap_remove(rng.below(live.len()));
                            let p = (og.base_ptr() as usize + off) as *mut u8;
                            // SAFETY: `(off, size)` is a live object of this
                            // generation, freed exactly once.
                            unsafe { og.free(p, size) };
                        }
                    }
                }
            }
            if rng.below(5) == 0 {
                og.coalesce_free_blocks();
            }
            if rng.below(25) == 0 {
                // A compaction with everything live: relocates, so the
                // table must start over.
                for (off, _) in &live {
                    let h = (og.base_ptr() as usize + off) as *const ObjectHeader;
                    // SAFETY: a live header of this generation.
                    unsafe { &*h }.add_gc_flags(cratonvm_gc_core::heap::GC_FLAG_MARKED);
                }
                let map = og.compact();
                let base = og.base_ptr() as usize;
                for (off, _) in live.iter_mut() {
                    if let Some(&n) = map.get(&(base + *off)) {
                        *off = n - base;
                    }
                }
            }
            // Walk, several times per round.
            for _ in 0..3 {
                let ranges = random_ranges(&mut rng, og.capacity());
                for overlaps in [false, true] {
                    let got = og.walk_card_ranges(&ranges, overlaps);
                    assert_eq!(
                        got,
                        oracle(&og, &ranges, overlaps),
                        "seed {seed} round {round} overlaps={overlaps}: anchored walk vs oracle"
                    );
                    // gcj j1yg-pause: the same ranges split into 1-5
                    // contiguous groups, each walked by itself and the
                    // hits concatenated in group order: the serial walk.
                    for parts in 1..=5usize {
                        let n = ranges.len();
                        let split = og.with_card_walk_view(&ranges, |view| {
                            let mut all = Vec::new();
                            for p in 0..parts {
                                let (g0, g1) = (n * p / parts, n * (p + 1) / parts);
                                if g0 < g1 {
                                    view.walk_group(&ranges, g0, g1, overlaps, &mut all)
                                        .expect("a sane heap walks without a break");
                                }
                            }
                            all
                        });
                        assert!(
                            split.is_some() || ranges.is_empty(),
                            "seed {seed} round {round}: the view must not refuse a sane heap"
                        );
                        if let Some(split) = split {
                            assert_eq!(
                                split, got,
                                "seed {seed} round {round} overlaps={overlaps} parts={parts}: \
                                 group walks vs the serial walk"
                            );
                        }
                    }
                }
                let linear: Vec<CardRangeHit> = og
                    .walk_objects_in_card_ranges(&ranges)
                    .into_iter()
                    .map(|(ptr, size)| CardRangeHit {
                        ptr,
                        size,
                        window: None,
                    })
                    .collect();
                assert_eq!(
                    og.walk_card_ranges(&ranges, false),
                    linear,
                    "seed {seed} round {round}: anchored walk vs walk_objects_in_card_ranges"
                );
            }
        }
        let stats = og.block_offset_stats();
        assert!(
            stats.enabled,
            "seed {seed}: the table must not have latched off"
        );
        assert_eq!(stats.anomalies, 0, "seed {seed}: {stats:?}");
        // gen r5w1/oldgen5: a sane heap has no walk gaps.
        assert_eq!(stats.gap_windows, 0, "seed {seed}: {stats:?}");
        assert_eq!(stats.gap_rederivations, 0, "seed {seed}: {stats:?}");
        assert!(
            stats.walks > 0 && stats.anchored_ranges > 0,
            "seed {seed}: {stats:?}"
        );
        assert!(
            stats.invalidations > 0,
            "seed {seed}: frees must have cut the prefix"
        );
    }
}

/// The linear overlap walk (the fallback when the table is off) agrees
/// with the oracle too.
#[test]
fn the_linear_overlap_walk_matches_the_oracle() {
    for seed in 1..=6u64 {
        let mut rng = Rng(0xA076_1D64_78BD_642F ^ seed);
        let mut og = OldGen::new(256 * 1024);
        og.set_block_offset_table_enabled(false);
        let mut live = Vec::new();
        for _ in 0..200 {
            if rng.below(3) == 0 {
                alloc_buffer(&mut og, &mut rng, &mut live);
            } else if let Some(o) = alloc_one(&mut og, &mut rng) {
                live.push(o);
            }
            if rng.below(4) == 0 && !live.is_empty() {
                let (off, size) = live.swap_remove(rng.below(live.len()));
                // SAFETY: a live object, freed once.
                unsafe { og.free((og.base_ptr() as usize + off) as *mut u8, size) };
            }
        }
        for _ in 0..20 {
            let ranges = random_ranges(&mut rng, og.capacity());
            assert_eq!(
                og.walk_card_ranges(&ranges, true),
                oracle(&og, &ranges, true),
                "seed {seed}"
            );
            assert_eq!(
                og.walk_card_ranges(&ranges, false),
                oracle(&og, &ranges, false),
                "seed {seed}"
            );
        }
        assert_eq!(og.block_offset_stats().walks, 0, "the table is off");
    }
}

/// The point of the table: a dirty card near the TOP of a generation full
/// of small objects decodes the objects near that card, not all of them.
/// Measured in headers decoded while re-deriving, which is the cost the
/// linear walk paid on every pause.
#[test]
fn a_dirty_card_at_the_top_does_not_decode_the_whole_generation() {
    let mut og = OldGen::new(1024 * 1024);
    og.set_block_offset_table_enabled(true);
    let size = object_size(1);
    let mut offs = Vec::new();
    while let Some(p) = og.alloc(size, 8) {
        // SAFETY: fresh block.
        unsafe {
            std::ptr::write(
                p as *mut ObjectHeader,
                ObjectHeader::new(
                    ClassId::new(1),
                    ObjectKind::Object,
                    ArrayElementType::Reference,
                    0,
                    1,
                ),
            )
        };
        offs.push(p as usize - og.base_ptr() as usize);
    }
    let n = offs.len();
    assert!(n > 20_000);
    let top = offs[n - 3];
    let card = top / BOT_CARD;
    let ranges = [(card * BOT_CARD, (card + 1) * BOT_CARD)];
    // Every card was anchored at allocation: nothing needs re-deriving.
    let hits = og.walk_card_ranges(&ranges, false);
    assert!(hits
        .iter()
        .any(|h| h.ptr as usize - og.base_ptr() as usize == top));
    let s = og.block_offset_stats();
    assert_eq!(
        s.rederived_objects, 0,
        "allocation anchored every card: {s:?}"
    );
    assert_eq!(s.anchored_ranges, 1);

    // Free one object near the bottom: the prefix above it is re-derived
    // ONCE, and the next pause pays nothing again.
    // SAFETY: a live object, freed once.
    unsafe { og.free((og.base_ptr() as usize + offs[10]) as *mut u8, size) };
    og.walk_card_ranges(&ranges, false);
    let once = og.block_offset_stats().rederived_objects;
    assert!(once > 0 && once <= n as u64);
    og.walk_card_ranges(&ranges, false);
    assert_eq!(
        og.block_offset_stats().rederived_objects,
        once,
        "re-derived once, then trusted"
    );
}

/// A double free leaves overlapping free blocks; the anchored walk cannot
/// ask "which block covers p" of such a list, so it must hand the walk to
/// the linear one — whose `max` guard already copes.
#[test]
fn an_overlapping_free_list_falls_back_to_the_linear_walk() {
    let mut og = OldGen::new(64 * 1024);
    og.set_block_offset_table_enabled(true);
    let size = object_size(2);
    let mut offs = Vec::new();
    for _ in 0..8 {
        let p = og.alloc(size, 8).unwrap();
        // SAFETY: fresh block.
        unsafe {
            std::ptr::write(
                p as *mut ObjectHeader,
                ObjectHeader::new(
                    ClassId::new(1),
                    ObjectKind::Object,
                    ArrayElementType::Reference,
                    0,
                    2,
                ),
            )
        };
        offs.push(p as usize - og.base_ptr() as usize);
    }
    let base = og.base_ptr() as usize;
    // SAFETY: the modelled defect — a span freed, then a piece of it again.
    unsafe {
        og.free((base + offs[2]) as *mut u8, size * 4);
        og.free((base + offs[3]) as *mut u8, 24);
    }
    let full = [(0usize, offs[7] + size)];
    let want: Vec<CardRangeHit> = og
        .walk_objects_in_card_ranges(&full)
        .into_iter()
        .map(|(ptr, size)| CardRangeHit {
            ptr,
            size,
            window: None,
        })
        .collect();
    assert_eq!(og.walk_card_ranges(&full, false), want);
    assert_eq!(og.block_offset_stats().fallbacks, 1);
}

// --- gen r5w1/oldgen5: walk gaps ----------------------------------------

/// Carve `count` legacy objects of `slots` slots end to end out of one
/// promotion-buffer-shaped block, consumed to the byte (no tail). Returns
/// their offsets.
fn carve_buffer(og: &mut OldGen, count: usize, slots: u32) -> Vec<usize> {
    let size = object_size(slots);
    let p = og
        .alloc_unzeroed_buffer(size * count, 8)
        .expect("room for the buffer");
    let base = og.base_ptr() as usize;
    let mut offs = Vec::with_capacity(count);
    for i in 0..count {
        // SAFETY: `[p + i*size, p + (i+1)*size)` lies inside the buffer.
        unsafe {
            let q = p.add(i * size);
            std::ptr::write_bytes(q, 0, size);
            std::ptr::write(
                q as *mut ObjectHeader,
                ObjectHeader::new(
                    ClassId::new(1),
                    ObjectKind::Object,
                    ArrayElementType::Reference,
                    0,
                    slots,
                ),
            );
        }
        offs.push(p as usize - base + i * size);
    }
    offs
}

/// One legacy object on a block of its own.
fn own_block(og: &mut OldGen, slots: u32) -> usize {
    let p = og.alloc(object_size(slots), 8).expect("room");
    // SAFETY: a fresh, zeroed block of that size.
    unsafe {
        std::ptr::write(
            p as *mut ObjectHeader,
            ObjectHeader::new(
                ClassId::new(1),
                ObjectKind::Object,
                ArrayElementType::Reference,
                0,
                slots,
            ),
        )
    };
    p as usize - og.base_ptr() as usize
}

/// Make the header at `off` one no walk can size (an invalid kind tag).
fn break_header(og: &OldGen, off: usize) {
    let p = og.base_ptr() as usize + off + cratonvm_types::KIND_TAGS_BYTE_OFFSET;
    // SAFETY: a byte inside a live header of this generation.
    unsafe { std::ptr::write(p as *mut u8, 0xFF) };
}

fn rel(og: &OldGen, hits: &[CardRangeHit]) -> Vec<usize> {
    let base = og.base_ptr() as usize;
    hits.iter().map(|h| h.ptr as usize - base).collect()
}

#[test]
fn a_gap_window_runs_from_the_first_dirty_byte_to_the_resume_point() {
    let ranges = [(512, 1024), (2048, 2560)];
    // No range reaches the stretch.
    assert_eq!(gap_window(&ranges, 1024, 2048), None);
    assert_eq!(gap_window(&ranges, 3000, 4000), None);
    // A range starting inside it: from that range's start to the end.
    assert_eq!(gap_window(&ranges, 1100, 4000), Some((2048, 4000)));
    // A range the break lies inside: from the break.
    assert_eq!(gap_window(&ranges, 600, 700), Some((600, 700)));
    assert_eq!(gap_window(&ranges, 600, 600), None);
    let mut gaps = Vec::new();
    push_gap(&mut gaps, (100, 200));
    push_gap(&mut gaps, (200, 300));
    push_gap(&mut gaps, (400, 500));
    assert_eq!(gaps, vec![(100, 300), (400, 500)]);
}

/// The anchored walk reports the unparseable stretch as a window and
/// resumes at the next allocation block's trusted anchor, so the objects
/// of that block are still reported precisely; the linear walk reports the
/// rest of the region as the window.
#[test]
fn a_walk_gap_is_a_window_and_the_anchored_walk_resumes_at_the_next_block() {
    for bot_on in [true, false] {
        for overlaps in [false, true] {
            let mut og = OldGen::new(64 * 1024);
            og.set_block_offset_table_enabled(bot_on);
            // Two buffers back to back: one allocated region.
            let first = carve_buffer(&mut og, 26, 4);
            let second = carve_buffer(&mut og, 26, 4);
            assert_eq!(second[0], first[25] + object_size(4), "contiguous");
            assert!(
                second[0] > BOT_CARD,
                "the second buffer owns a card's first byte"
            );
            break_header(&og, first[1]);
            let end = second[25] + object_size(4);
            let everything = [(0usize, end.div_ceil(BOT_CARD) * BOT_CARD)];
            let (hits, gaps) = og.walk_card_ranges_with_gaps(&everything, overlaps);
            if bot_on {
                let mut want = vec![first[0]];
                want.extend_from_slice(&second);
                assert_eq!(rel(&og, &hits), want, "overlaps={overlaps}");
                assert_eq!(gaps, vec![(first[1], second[0])], "overlaps={overlaps}");
                let s = og.block_offset_stats();
                assert!(s.enabled);
                assert_eq!(s.gap_anchor_resumes, 1, "{s:?}");
                assert_eq!(s.gap_windows, 1, "{s:?}");
            } else {
                assert_eq!(rel(&og, &hits), vec![first[0]], "overlaps={overlaps}");
                assert_eq!(gaps, vec![(first[1], end)], "overlaps={overlaps}");
            }
            // A dirty range that ends at the break sees no window.
            let front = [(0usize, first[1])];
            let (front_hits, none) = og.walk_card_ranges_with_gaps(&front, overlaps);
            assert_eq!(rel(&og, &front_hits), vec![first[0]]);
            assert!(
                none.is_empty(),
                "bot_on={bot_on} overlaps={overlaps}: {none:?}"
            );
        }
    }
}

/// Re-deriving the table over a header it cannot size used to latch the
/// table off for the whole generation. It now anchors the stretch at the
/// break: the walk reports it as a window, a later region is still walked
/// from its own anchors, and the table stays on.
#[test]
fn re_deriving_over_a_walk_gap_keeps_the_table_on() {
    let mut og = OldGen::new(64 * 1024);
    og.set_block_offset_table_enabled(true);
    let first = carve_buffer(&mut og, 26, 4);
    let second = carve_buffer(&mut og, 26, 4);
    let region_end = second[25] + object_size(4);
    let spacer = own_block(&mut og, 1);
    let z = own_block(&mut og, 1);
    assert_eq!(spacer, region_end);
    break_header(&og, first[1]);
    let base = og.base_ptr() as usize;
    // SAFETY: live objects of this generation, each freed once. The first
    // free lowers the trusted prefix below the break; the second puts a
    // free block between the broken region and `z`.
    unsafe {
        og.free((base + first[0]) as *mut u8, object_size(4));
        og.free((base + spacer) as *mut u8, object_size(1));
    }
    let last_card = second[25] / BOT_CARD * BOT_CARD;
    let z_card_end = (z / BOT_CARD + 1) * BOT_CARD;
    let ranges = [(last_card, z_card_end)];
    let (hits, gaps) = og.walk_card_ranges_with_gaps(&ranges, false);
    assert_eq!(rel(&og, &hits), vec![z], "the later region is still walked");
    assert_eq!(
        gaps,
        vec![(last_card, region_end)],
        "the stretch from the dirty byte on"
    );
    let s = og.block_offset_stats();
    assert!(s.enabled, "the table must stay on: {s:?}");
    assert_eq!(s.gap_rederivations, 1, "{s:?}");
    // The same answer the linear walk gives.
    og.set_block_offset_table_enabled(false);
    let (lin_hits, lin_gaps) = og.walk_card_ranges_with_gaps(&ranges, false);
    assert_eq!(lin_hits, hits);
    assert_eq!(lin_gaps, gaps);
}

/// An allocation anchors exactly the cards whose first byte it covers.
#[test]
fn note_alloc_anchors_the_cards_whose_first_byte_the_block_covers() {
    let mut t = BlockOffsetTable::new(1 << 20, true);
    t.note_alloc(BOT_CARD - 16, 3 * BOT_CARD);
    // Card 0 starts before the block; cards 1..=3 start inside it.
    assert_eq!(t.anchor(0), Some(0), "untouched (resized with 0)");
    for c in 1..=3 {
        assert_eq!(t.anchor(c), Some(BOT_CARD - 16), "card {c}");
    }
    assert_eq!(t.anchor(4), None);
    // A free lowers the trusted prefix to the freed object's card.
    t.valid_cards = 4;
    t.note_free(2 * BOT_CARD + 8);
    assert_eq!(t.valid_cards, 3);
    t.note_free(5 * BOT_CARD);
    assert_eq!(t.valid_cards, 3, "a free above the prefix changes nothing");
    t.note_relayout();
    assert_eq!(t.valid_cards, 0);
    // Too big to encode: built off.
    assert!(!BlockOffsetTable::new(usize::MAX, true).enabled);
    assert!(!BlockOffsetTable::new(1 << 20, false).enabled);
}

// --- gen r5w5/old9: the object-base oracle ------------------------------

/// What the WALK says is at absolute address `a`: the answer
/// [`OldGen::object_at`] must give on a heap whose walk covers every
/// allocated byte.
fn walked_answer(walked: &[(*mut u8, usize)], a: usize) -> ObjectAt {
    let i = walked.partition_point(|&(p, _)| p as usize <= a);
    if i > 0 {
        let (p, s) = (walked[i - 1].0 as usize, walked[i - 1].1);
        if a < p + s {
            return if a == p {
                ObjectAt::Base { size: s }
            } else {
                ObjectAt::Interior { base: p, size: s }
            };
        }
    }
    ObjectAt::Free
}

/// The differential: every walked base, three interior bytes of every
/// walked object, `samples` random addresses across the whole capacity and
/// both ends outside the storage must get exactly the walk's answer.
fn assert_oracle_matches_walk(og: &OldGen, rng: &mut Rng, samples: usize, what: &str) {
    let base = og.base_ptr() as usize;
    let walked = og.walk_objects();
    let walked_bytes: usize = walked.iter().map(|&(_, s)| s).sum();
    assert_eq!(
        walked_bytes,
        og.used(),
        "{what}: precondition, the walk covers every allocated byte"
    );
    for &(p, size) in &walked {
        let p = p as usize;
        assert_eq!(
            og.object_at(p),
            ObjectAt::Base { size },
            "{what}: base +{:#x}",
            p - base
        );
        for q in [p + 8, p + size / 2, p + size - 1] {
            if q > p {
                assert_eq!(
                    og.object_at(q),
                    ObjectAt::Interior { base: p, size },
                    "{what}: interior +{:#x} of +{:#x}",
                    q - base,
                    p - base,
                );
            }
        }
    }
    let cap = og.capacity();
    for _ in 0..samples {
        let a = base + rng.below(cap);
        assert_eq!(
            og.object_at(a),
            walked_answer(&walked, a),
            "{what}: sample +{:#x}",
            a - base
        );
    }
    assert_eq!(
        og.object_at(base - 8),
        ObjectAt::Free,
        "{what}: below the storage"
    );
    assert_eq!(
        og.object_at(base + cap),
        ObjectAt::Free,
        "{what}: past the storage"
    );
    assert_eq!(og.object_at(0), ObjectAt::Free, "{what}: null");
}

/// FRAGMENTED layouts: the randomized heap of the anchored-walk test —
/// single-object blocks, promotion buffers with released tails, frees at
/// random, coalescing, and a compaction now and then — with the oracle
/// asked about every object after every mutation batch. The trusted prefix
/// is cut back by the frees and re-derived by the oracle itself.
#[test]
fn the_object_oracle_matches_the_walk_on_fragmented_heaps() {
    for seed in 1..=10u64 {
        let mut rng = Rng(0x5851_F42D_4C95_7F2D ^ (seed * 0x9E37_79B9));
        let mut og = OldGen::new(512 * 1024);
        og.set_block_offset_table_enabled(true);
        let mut live: Vec<(usize, usize)> = Vec::new();
        for round in 0..40 {
            for _ in 0..rng.below(40) {
                match rng.below(10) {
                    0..=4 => {
                        if let Some(o) = alloc_one(&mut og, &mut rng) {
                            live.push(o);
                        }
                    }
                    5 | 6 => alloc_buffer(&mut og, &mut rng, &mut live),
                    _ => {
                        if !live.is_empty() {
                            let (off, size) = live.swap_remove(rng.below(live.len()));
                            // SAFETY: a live object of this generation,
                            // freed exactly once.
                            unsafe { og.free((og.base_ptr() as usize + off) as *mut u8, size) };
                        }
                    }
                }
            }
            if rng.below(5) == 0 {
                og.coalesce_free_blocks();
            }
            if rng.below(20) == 0 {
                for (off, _) in &live {
                    let h = (og.base_ptr() as usize + off) as *const ObjectHeader;
                    // SAFETY: a live header of this generation.
                    unsafe { &*h }.add_gc_flags(cratonvm_gc_core::heap::GC_FLAG_MARKED);
                }
                let map = og.compact();
                let base = og.base_ptr() as usize;
                for (off, _) in live.iter_mut() {
                    if let Some(&n) = map.get(&(base + *off)) {
                        *off = n - base;
                    }
                }
            }
            assert_oracle_matches_walk(
                &og,
                &mut rng,
                300,
                &format!("seed {seed} round {round}"),
            );
        }
        let s = og.block_offset_stats();
        assert!(s.enabled, "seed {seed}: {s:?}");
        assert_eq!(
            s.oracle_unknown, 0,
            "seed {seed}: a sane heap is always answerable: {s:?}"
        );
        assert_eq!(s.anomalies, 0, "seed {seed}: {s:?}");
        assert!(
            s.oracle_queries > 0 && s.oracle_strides > 0,
            "seed {seed}: {s:?}"
        );
    }
}

/// HUMONGOUS layouts: multi-megabyte arrays on blocks of their own (one
/// placed from the top of the generation, which leaves a free gap below
/// it), small objects and buffers around and between them, frees on both
/// sides. A query deep inside a humongous array is ONE stride: its every
/// card is anchored at its start.
#[test]
fn the_object_oracle_matches_the_walk_on_a_humongous_layout() {
    let mut rng = Rng(0x2545_F491_4F6C_DD1D);
    let mut og = OldGen::new(16 * 1024 * 1024);
    og.set_block_offset_table_enabled(true);
    let mut live: Vec<(usize, usize)> = Vec::new();
    let humongous = |og: &mut OldGen, len: u32, top: bool| -> usize {
        let size = ref_array_size(len);
        let p = if top {
            og.alloc_from_top(size, 8, true)
        } else {
            og.alloc(size, 8)
        }
        .expect("room for a humongous array");
        // SAFETY: a fresh, zeroed block of `size` bytes.
        unsafe {
            std::ptr::write(
                p as *mut ObjectHeader,
                ObjectHeader::new(
                    ClassId::new(2),
                    ObjectKind::Array,
                    ArrayElementType::Reference,
                    len,
                    len,
                ),
            )
        };
        p as usize
    };
    for _ in 0..50 {
        if let Some(o) = alloc_one(&mut og, &mut rng) {
            live.push(o);
        }
    }
    let low = humongous(&mut og, 400_000, false);
    for _ in 0..8 {
        alloc_buffer(&mut og, &mut rng, &mut live);
    }
    let high = humongous(&mut og, 300_000, true);
    for _ in 0..50 {
        if let Some(o) = alloc_one(&mut og, &mut rng) {
            live.push(o);
        }
    }
    // Frees on both sides of the arrays.
    for _ in 0..30 {
        let (off, size) = live.swap_remove(rng.below(live.len()));
        // SAFETY: a live object, freed once.
        unsafe { og.free((og.base_ptr() as usize + off) as *mut u8, size) };
    }
    assert_oracle_matches_walk(&og, &mut rng, 2000, "humongous");
    for (h, len) in [(low, 400_000u32), (high, 300_000u32)] {
        let size = ref_array_size(len);
        let before = og.block_offset_stats().oracle_strides;
        let deep = h + size - 3 * BOT_CARD - 5;
        assert_eq!(og.object_at(deep), ObjectAt::Interior { base: h, size });
        assert_eq!(
            og.block_offset_stats().oracle_strides - before,
            1,
            "one stride from the array's own anchor",
        );
    }
    // Freeing the low array leaves a multi-megabyte hole the oracle
    // reports as free, and the high one is still answered exactly.
    // SAFETY: the array was allocated above, freed once.
    unsafe { og.free(low as *mut u8, ref_array_size(400_000)) };
    assert_eq!(og.object_at(low + 1024 * 1024), ObjectAt::Free);
    assert_oracle_matches_walk(&og, &mut rng, 2000, "humongous after the low array died");
    assert_eq!(og.block_offset_stats().oracle_unknown, 0);
}

/// PINNED layouts: after `compact_around_pins` slides the unpinned
/// survivors down around pinned islands (the table is discarded by the
/// relayout), the oracle re-derives and matches the walk again — gaps in
/// front of pins, pins at the end, dead objects dropped.
#[test]
fn the_object_oracle_matches_the_walk_after_a_pinned_compaction() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut og = OldGen::new(1024 * 1024);
    og.set_block_offset_table_enabled(true);
    let mut objs: Vec<usize> = Vec::new();
    objs.extend(carve_buffer(&mut og, 40, 2));
    for _ in 0..20 {
        objs.push(own_block(&mut og, 3));
    }
    objs.extend(carve_buffer(&mut og, 40, 1));
    let base = og.base_ptr() as usize;
    assert_oracle_matches_walk(&og, &mut rng, 500, "before");
    // Two in three survive; two pins, one of them on a dead object (the
    // pin retains it), one near the top.
    for (i, &o) in objs.iter().enumerate() {
        if i % 3 != 0 {
            // SAFETY: a live header of this generation.
            unsafe { &*((base + o) as *const ObjectHeader) }
                .add_gc_flags(cratonvm_gc_core::heap::GC_FLAG_MARKED);
        }
    }
    let pins = vec![base + objs[30], base + objs[95]];
    let walked = og.walk_objects();
    let seq = og.free_list_seq();
    let done = og
        .compact_around_pins(
            &walked,
            seq,
            pins,
            (0, 0),
            &std::collections::HashMap::new(),
        )
        .expect("a sane heap compacts");
    assert_eq!(done.pinned, 2);
    assert!(done.moved > 0);
    assert_oracle_matches_walk(&og, &mut rng, 1000, "after the pinned compaction");
    // The pins kept their addresses and are still bases.
    assert!(matches!(
        og.object_at(base + objs[30]),
        ObjectAt::Base { .. }
    ));
    assert!(matches!(
        og.object_at(base + objs[95]),
        ObjectAt::Base { .. }
    ));
    assert_eq!(og.block_offset_stats().oracle_unknown, 0);
}

/// A header no walk can size is a walk gap: the oracle refuses to answer
/// behind it (never a guess), while an object whose card is anchored past
/// the break is still answered exactly.
#[test]
fn the_object_oracle_refuses_a_walk_gap_and_an_overlapping_free_list() {
    let mut og = OldGen::new(64 * 1024);
    og.set_block_offset_table_enabled(true);
    let first = carve_buffer(&mut og, 26, 4);
    let second = carve_buffer(&mut og, 26, 4);
    break_header(&og, first[1]);
    let base = og.base_ptr() as usize;
    assert_eq!(
        og.object_at(base + first[0]),
        ObjectAt::Base {
            size: object_size(4)
        }
    );
    assert_eq!(
        og.object_at(base + first[2]),
        ObjectAt::Unknown,
        "behind the break"
    );
    let last = second[25];
    assert!(
        last / BOT_CARD * BOT_CARD >= second[0],
        "the card is the second buffer's own"
    );
    assert_eq!(
        og.object_at(base + last),
        ObjectAt::Base {
            size: object_size(4)
        }
    );
    assert!(og.block_offset_stats().oracle_unknown >= 1);

    // A double free: "which free block covers p" is ill-defined.
    let mut og = OldGen::new(64 * 1024);
    og.set_block_offset_table_enabled(true);
    let objs = carve_buffer(&mut og, 8, 2);
    let base = og.base_ptr() as usize;
    // SAFETY: the modelled defect — a span freed, then a piece of it again.
    unsafe {
        og.free((base + objs[2]) as *mut u8, object_size(2) * 4);
        og.free((base + objs[3]) as *mut u8, 24);
    }
    assert_eq!(og.object_at(base + objs[7]), ObjectAt::Unknown);

    // A generation without the table answers nothing.
    let mut og = OldGen::new(64 * 1024);
    og.set_block_offset_table_enabled(false);
    let objs = carve_buffer(&mut og, 4, 1);
    assert_eq!(
        og.object_at(og.base_ptr() as usize + objs[1]),
        ObjectAt::Unknown
    );
}

/// The long stride is a promotion buffer's (every card anchored at the
/// buffer's start). The oracle pays it once: the cards it passes are
/// re-anchored exactly, so asking again costs one card's objects. (gcg
/// g6/og: the second half, a rebuild from a live sweep's kept objects,
/// went with `rebuild_block_offsets_from_live`.)
#[test]
fn the_oracle_compresses_a_buffer_once() {
    let mut og = OldGen::new(256 * 1024);
    og.set_block_offset_table_enabled(true);
    let objs = carve_buffer(&mut og, 400, 1);
    let base = og.base_ptr() as usize;
    let per_card = BOT_CARD / object_size(1) + 2;
    let last = base + objs[399];
    let s0 = og.block_offset_stats().oracle_strides;
    assert!(matches!(og.object_at(last), ObjectAt::Base { .. }));
    let first_cost = og.block_offset_stats().oracle_strides - s0;
    assert!(first_cost >= 390, "from the buffer's start: {first_cost}");
    let s1 = og.block_offset_stats().oracle_strides;
    assert!(matches!(og.object_at(last), ObjectAt::Base { .. }));
    let second_cost = (og.block_offset_stats().oracle_strides - s1) as usize;
    assert!(
        second_cost <= per_card,
        "compressed: {second_cost} > {per_card}"
    );
    assert!(og.block_offset_stats().oracle_compressed > 0);
    let mut rng = Rng(7);
    assert_oracle_matches_walk(&og, &mut rng, 500, "after the compression");
}
