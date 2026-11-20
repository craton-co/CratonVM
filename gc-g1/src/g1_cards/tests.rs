use super::*;

/// A synthetic arena: the table never dereferences the addresses it is
/// given, so a plausible base is enough.
const BASE: usize = 0x1000_0000;

/// W5-B — an [`ObjectStart`] over a synthetic address.
///
/// These tests build a table over `BASE`, which names no mapped memory, so
/// the runtime vouch cannot read a header there — it would not fail, it
/// would fault. [`ObjectStart::from_asserted_start`] is the hatch for
/// exactly this, and it is `unsafe` so that reaching for it in production
/// code is a decision a reviewer sees. The block-offset table's behaviour
/// under these addresses is otherwise identical to a real producer's:
/// `from_asserted_start` vouches, so every case below exercises the
/// installing path.
fn start(addr: usize) -> ObjectStart<'static> {
    // SAFETY: a synthetic address in a table over memory that does not
    // exist. Nothing dereferences it; see the doc comment above.
    unsafe { ObjectStart::from_asserted_start(addr) }
}

#[test]
fn a_fresh_table_is_entirely_clean() {
    let t = G1CardTable::new(BASE, 4 * G1_CARD_BYTES);
    assert_eq!(t.card_count(), 4);
    assert!(!t.any_dirty_in(BASE, 4 * G1_CARD_BYTES));
    assert_eq!(t.dirty_cards_in(BASE, 4 * G1_CARD_BYTES), 0);
}

#[test]
fn dirtying_an_address_dirties_exactly_its_own_card() {
    let t = G1CardTable::new(BASE, 4 * G1_CARD_BYTES);
    t.dirty_addr(BASE + G1_CARD_BYTES + 17);
    assert!(!t.is_dirty_addr(BASE));
    assert!(t.is_dirty_addr(BASE + G1_CARD_BYTES));
    assert!(t.is_dirty_addr(BASE + 2 * G1_CARD_BYTES - 1));
    assert!(!t.is_dirty_addr(BASE + 2 * G1_CARD_BYTES));
    assert_eq!(t.dirty_cards_in(BASE, 4 * G1_CARD_BYTES), 1);
}

/// The per-object screen: an object straddling a card boundary must be
/// scanned when EITHER card is dirty, because a reference slot anywhere in
/// it may be the one the barrier recorded.
#[test]
fn a_straddling_span_is_dirty_if_any_card_it_touches_is() {
    let t = G1CardTable::new(BASE, 4 * G1_CARD_BYTES);
    t.dirty_addr(BASE + 2 * G1_CARD_BYTES);
    // A span ending one byte inside card 2.
    assert!(t.any_dirty_in(BASE + 2 * G1_CARD_BYTES - 8, 9));
    // The same span one byte shorter stops before card 2.
    assert!(!t.any_dirty_in(BASE + 2 * G1_CARD_BYTES - 8, 8));
}

#[test]
fn clearing_a_range_leaves_its_neighbours_alone() {
    let t = G1CardTable::new(BASE, 8 * G1_CARD_BYTES);
    for c in 0..8 {
        t.dirty_addr(BASE + c * G1_CARD_BYTES);
    }
    t.clear_range(BASE + 2 * G1_CARD_BYTES, 3 * G1_CARD_BYTES);
    assert!(t.is_dirty_addr(BASE + G1_CARD_BYTES));
    assert!(!t.is_dirty_addr(BASE + 2 * G1_CARD_BYTES));
    assert!(!t.is_dirty_addr(BASE + 3 * G1_CARD_BYTES));
    assert!(!t.is_dirty_addr(BASE + 4 * G1_CARD_BYTES));
    assert!(t.is_dirty_addr(BASE + 5 * G1_CARD_BYTES));
    assert_eq!(t.dirty_cards_in(BASE, 8 * G1_CARD_BYTES), 5);
}

/// Out-of-arena addresses are silently ignored rather than panicking or
/// aliasing card 0 — the write barrier hands this table whatever the
/// mutator stored, including addresses in metaspace or off-heap.
#[test]
fn addresses_outside_the_arena_touch_nothing() {
    let t = G1CardTable::new(BASE, 4 * G1_CARD_BYTES);
    t.dirty_addr(BASE - 1);
    t.dirty_addr(BASE + 4 * G1_CARD_BYTES);
    t.dirty_addr(0);
    assert!(!t.any_dirty_in(BASE, 4 * G1_CARD_BYTES));
    assert!(!t.is_dirty_addr(BASE - 1));
}

/// A span that starts before the arena and ends inside it is clamped, not
/// wrapped: `card_range` must not compute a huge index from an underflow.
#[test]
fn a_span_straddling_the_arena_base_is_clamped() {
    let t = G1CardTable::new(BASE, 4 * G1_CARD_BYTES);
    t.dirty_addr(BASE);
    assert!(t.any_dirty_in(BASE - 4096, 4096 + 8));
    assert_eq!(t.cards_in(BASE - 4096, 4096), 0);
}

// ── card cleaning ───────────────────────────────────────────────────

#[test]
fn a_snapshot_reports_what_the_table_held_when_it_was_taken() {
    let t = G1CardTable::new(BASE, 8 * G1_CARD_BYTES);
    t.dirty_addr(BASE + G1_CARD_BYTES);
    t.dirty_addr(BASE + 5 * G1_CARD_BYTES);
    let snap = t.snapshot(BASE, 8 * G1_CARD_BYTES);
    assert_eq!(snap.count(), 2);
    assert_eq!(snap.covered_cards(), 8);
    assert!(snap.any_in_span(BASE + G1_CARD_BYTES, 8));
    assert!(!snap.any_in_span(BASE, 8));

    // The table moving on does not move the snapshot: that independence is
    // the point — the walk decides from the snapshot while it rewrites the
    // table underneath.
    t.dirty_addr(BASE);
    assert!(!snap.any_in_span(BASE, 8));
    assert!(t.is_dirty_addr(BASE));
}

#[test]
fn clean_and_redirty_leaves_exactly_the_kept_cards_dirty() {
    let t = G1CardTable::new(BASE, 8 * G1_CARD_BYTES);
    for c in 0..8 {
        t.dirty_addr(BASE + c * G1_CARD_BYTES);
    }
    let mut keep = t.empty_set(BASE, 8 * G1_CARD_BYTES);
    keep.insert_addr(BASE + 2 * G1_CARD_BYTES + 9);
    keep.insert_addr(BASE + 6 * G1_CARD_BYTES);
    t.clean_and_redirty(BASE, 8 * G1_CARD_BYTES, &keep);

    for c in 0..8 {
        let want = c == 2 || c == 6;
        assert_eq!(t.is_dirty_addr(BASE + c * G1_CARD_BYTES), want, "card {c}");
    }
}

/// The bound matters: a walk that stopped early must not clean past where
/// it stopped, or it drops edges it never looked at.
#[test]
fn clean_and_redirty_touches_no_card_past_the_span_it_was_given() {
    let t = G1CardTable::new(BASE, 8 * G1_CARD_BYTES);
    for c in 0..8 {
        t.dirty_addr(BASE + c * G1_CARD_BYTES);
    }
    let keep = t.empty_set(BASE, 4 * G1_CARD_BYTES);
    t.clean_and_redirty(BASE, 4 * G1_CARD_BYTES, &keep);
    for c in 0..4 {
        assert!(
            !t.is_dirty_addr(BASE + c * G1_CARD_BYTES),
            "card {c} cleaned"
        );
    }
    for c in 4..8 {
        assert!(
            t.is_dirty_addr(BASE + c * G1_CARD_BYTES),
            "card {c} untouched"
        );
    }
}

// ── the card cursor ────────────────────────────────────────────────

#[test]
fn the_cursor_finds_the_next_dirty_card_and_stops_at_the_end() {
    let t = G1CardTable::new(BASE, 256 * G1_CARD_BYTES);
    t.dirty_addr(BASE + 3 * G1_CARD_BYTES);
    t.dirty_addr(BASE + 200 * G1_CARD_BYTES);
    let snap = t.snapshot(BASE, 256 * G1_CARD_BYTES);

    assert_eq!(snap.first_dirty_addr(), Some(BASE + 3 * G1_CARD_BYTES));
    // From inside card 3 the answer is still card 3: the card COVERS the
    // address, so a walk sitting in it has not stepped past it.
    assert_eq!(
        snap.next_dirty_addr(BASE + 3 * G1_CARD_BYTES + 511),
        Some(BASE + 3 * G1_CARD_BYTES)
    );
    // One byte later it has, and the cursor jumps the 196-card clean run
    // in three word loads.
    assert_eq!(
        snap.next_dirty_addr(BASE + 4 * G1_CARD_BYTES),
        Some(BASE + 200 * G1_CARD_BYTES)
    );
    assert_eq!(snap.next_dirty_addr(BASE + 201 * G1_CARD_BYTES), None);
    // Below the covered run means "from the beginning of it", not "wrap".
    assert_eq!(snap.next_dirty_addr(0), Some(BASE + 3 * G1_CARD_BYTES));
}

#[test]
fn an_entirely_clean_run_has_no_cursor() {
    let t = G1CardTable::new(BASE, 8 * G1_CARD_BYTES);
    let snap = t.snapshot(BASE, 8 * G1_CARD_BYTES);
    assert_eq!(snap.first_dirty_addr(), None);
    assert_eq!(snap.next_dirty_addr(BASE), None);
    // An out-of-arena span produces an empty set, which must answer `None`
    // rather than index card 0.
    let miss = t.empty_set(BASE + 64 * G1_CARD_BYTES, 8);
    assert_eq!(miss.covered_cards(), 0);
    assert_eq!(miss.next_dirty_addr(BASE), None);
}

/// The cursor's whole reason to exist: it must decide exactly what
/// `any_in_span` decides, object for object, or it is a screen that can
/// drop a live cross-region edge. Cross-check the two over every
/// (start, size) in a small table.
#[test]
fn the_cursor_agrees_with_any_in_span_everywhere() {
    let t = G1CardTable::new(BASE, 16 * G1_CARD_BYTES);
    for c in [0usize, 1, 5, 6, 15] {
        t.dirty_addr(BASE + c * G1_CARD_BYTES + 3);
    }
    let snap = t.snapshot(BASE, 16 * G1_CARD_BYTES);
    // Walk a synthetic object grid with several sizes, maintaining the
    // cursor exactly as the region walk does.
    for size in [8usize, 24, 512, 700, 1536] {
        let mut cursor = snap.first_dirty_addr();
        let mut addr = BASE;
        while addr + size <= BASE + 16 * G1_CARD_BYTES {
            if cursor.is_some_and(|d| d + G1_CARD_BYTES <= addr) {
                cursor = snap.next_dirty_addr(addr);
            }
            let by_cursor = cursor.is_some_and(|d| d < addr + size);
            let by_span = snap.any_in_span(addr, size);
            assert_eq!(by_cursor, by_span, "size={size} addr=+{:#x}", addr - BASE);
            addr += size;
        }
    }
}

// ── the summary ────────────────────────────────────────────────────

/// The invariant, stated directly against the private state: no chunk may
/// be CLEAN while any card it covers is dirty. Everything the summary saves
/// rests on this, and violating it under-scans, which is a use-after-free.
fn assert_summary_invariant(t: &G1CardTable) {
    for (chunk, s) in t.summary.iter().enumerate() {
        if s.load(Ordering::Relaxed) != G1_CARD_CLEAN {
            continue;
        }
        let lo = chunk << G1_CARD_SUMMARY_SHIFT;
        let hi = (lo + G1_CARDS_PER_SUMMARY).min(t.cards.len());
        for i in lo..hi {
            assert_eq!(
                t.cards[i].load(Ordering::Relaxed),
                G1_CARD_CLEAN,
                "chunk {chunk} claims clean but card {i} is dirty"
            );
        }
    }
}

/// The screen must answer exactly what a naive per-card scan answers, and
/// the summary must stay sound across all three writers.
#[test]
fn the_summary_never_hides_a_dirty_card() {
    const N: usize = 5 * G1_CARDS_PER_SUMMARY + 7;
    let t = G1CardTable::new(BASE, N * G1_CARD_BYTES);
    assert_summary_invariant(&t);

    // A spread that crosses chunk boundaries unevenly.
    for c in (0..N).step_by(37) {
        t.dirty_addr(BASE + c * G1_CARD_BYTES + 5);
    }
    assert_summary_invariant(&t);
    let naive = |from: usize, to: usize| {
        (from..to).any(|i| t.cards[i].load(Ordering::Relaxed) != G1_CARD_CLEAN)
    };
    for start in (0..N).step_by(3) {
        for span_cards in [1usize, 2, 63, 64, 65, 200, N] {
            let end = (start + span_cards).min(N);
            assert_eq!(
                t.any_dirty_in(BASE + start * G1_CARD_BYTES, (end - start) * G1_CARD_BYTES),
                naive(start, end),
                "start={start} span={span_cards}"
            );
        }
    }
    // The snapshot must see the same set the table holds.
    let snap = t.snapshot(BASE, N * G1_CARD_BYTES);
    assert_eq!(snap.count(), t.dirty_card_total());

    // Cleaning re-derives the summary rather than leaving it stale, or the
    // screen never gets faster after a clean.
    let keep = t.empty_set(BASE, N * G1_CARD_BYTES);
    t.clean_and_redirty(BASE, N * G1_CARD_BYTES, &keep);
    assert_summary_invariant(&t);
    assert_eq!(t.dirty_card_total(), 0);
    assert!(
        t.summary
            .iter()
            .all(|s| s.load(Ordering::Relaxed) == G1_CARD_CLEAN),
        "a fully cleaned table must leave no chunk marked dirty"
    );
    assert!(!t.any_dirty_in(BASE, N * G1_CARD_BYTES));
}

/// A partial clean must not mark a chunk clean on the strength of the half
/// of it that was cleaned: `refresh_summary` reads the WHOLE chunk.
#[test]
fn a_partial_clean_keeps_a_straddling_chunk_dirty() {
    const N: usize = 2 * G1_CARDS_PER_SUMMARY;
    let t = G1CardTable::new(BASE, N * G1_CARD_BYTES);
    // One card in the first half of chunk 0, one in the second half.
    t.dirty_addr(BASE + 3 * G1_CARD_BYTES);
    t.dirty_addr(BASE + 40 * G1_CARD_BYTES);
    // Clean only the first 20 cards — a strict subset of chunk 0.
    let keep = t.empty_set(BASE, 20 * G1_CARD_BYTES);
    t.clean_and_redirty(BASE, 20 * G1_CARD_BYTES, &keep);
    assert_summary_invariant(&t);
    assert!(!t.is_dirty_addr(BASE + 3 * G1_CARD_BYTES));
    assert!(t.is_dirty_addr(BASE + 40 * G1_CARD_BYTES));
    assert!(t.any_dirty_in(BASE, N * G1_CARD_BYTES));
    // ... and `clear_range` over a strict subset behaves the same way.
    let t2 = G1CardTable::new(BASE, N * G1_CARD_BYTES);
    t2.dirty_addr(BASE + 3 * G1_CARD_BYTES);
    t2.dirty_addr(BASE + 40 * G1_CARD_BYTES);
    t2.clear_range(BASE, 20 * G1_CARD_BYTES);
    assert_summary_invariant(&t2);
    assert!(t2.any_dirty_in(BASE, N * G1_CARD_BYTES));
    assert_eq!(t2.dirty_card_total(), 1);
}

#[test]
fn the_dirty_total_is_the_saturation_numerator() {
    let t = G1CardTable::new(BASE, 8 * G1_CARD_BYTES);
    assert_eq!(t.dirty_card_total(), 0);
    t.dirty_addr(BASE);
    t.dirty_addr(BASE + 7 * G1_CARD_BYTES);
    assert_eq!(t.dirty_card_total(), 2);
    assert_eq!(t.card_count(), 8);
    t.clear_range(BASE, 8 * G1_CARD_BYTES);
    assert_eq!(t.dirty_card_total(), 0);
}

#[test]
fn a_span_straddling_two_cards_is_kept_by_either_of_them() {
    let t = G1CardTable::new(BASE, 4 * G1_CARD_BYTES);
    let mut keep = t.empty_set(BASE, 4 * G1_CARD_BYTES);
    keep.insert_addr(BASE + 2 * G1_CARD_BYTES);
    assert!(keep.any_in_span(BASE + 2 * G1_CARD_BYTES - 8, 9));
    assert!(!keep.any_in_span(BASE + 2 * G1_CARD_BYTES - 8, 8));
    assert!(!keep.is_empty());
}

// ---------------------------------------------------------------------
// W4-A — the block-offset table
// ---------------------------------------------------------------------

#[test]
fn a_fresh_block_offset_table_names_no_entry_anywhere() {
    let t = G1CardTable::new(BASE, 8 * G1_CARD_BYTES);
    for c in 0..8 {
        assert_eq!(
            t.block_entry_addr(BASE + c * G1_CARD_BYTES),
            None,
            "card {c} claimed an entry before any producer named one"
        );
    }
    assert_eq!(t.block_entries_in(BASE, 8 * G1_CARD_BYTES), 0);
}

#[test]
fn a_producer_that_names_an_object_start_makes_that_card_enterable() {
    let t = G1CardTable::new(BASE, 4 * G1_CARD_BYTES);
    let obj = BASE + G1_CARD_BYTES + 64;
    t.dirty_addr_at_object_start(start(obj));
    assert!(t.is_dirty_addr(obj), "the card must still be dirtied");
    // The entry is the address the producer named, not the card's base and
    // not a rounded approximation of it: the walk DECODES an object header
    // there, so anything but the exact address is a mis-entry.
    assert_eq!(t.block_entry_addr(obj), Some(obj));
    // Any address inside the card asks the same question.
    assert_eq!(t.block_entry_addr(BASE + G1_CARD_BYTES), Some(obj));
    assert_eq!(
        t.block_entry_addr(BASE + 2 * G1_CARD_BYTES - 1),
        Some(obj),
        "the last byte of the card belongs to the same card"
    );
    // And only that card.
    assert_eq!(t.block_entry_addr(BASE), None);
    assert_eq!(t.block_entry_addr(BASE + 2 * G1_CARD_BYTES), None);
    assert_eq!(t.block_entries_in(BASE, 4 * G1_CARD_BYTES), 1);
}

/// THE ONE THAT MATTERS. Two objects start in one card and both take a
/// cross-region store; the table must name the LOWER of them however the
/// stores are ordered.
///
/// A plain last-writer-wins byte passes this in one order and fails it in
/// the other, and the failure is not an over-scan: the source walk enters
/// at the higher object and never looks at the lower one, whose reference
/// into the collection set is then a slot left pointing at a freed region.
/// That is why [`G1CardTable::dirty_addr_at_object_start`] merges with an
/// atomic minimum and not with a store.
#[test]
fn a_card_with_two_named_object_starts_is_entered_at_the_lower_one() {
    let lo = BASE + G1_CARD_BYTES + 32;
    let hi = BASE + G1_CARD_BYTES + 256;

    // Ascending: the later, higher store must not raise the entry.
    let t = G1CardTable::new(BASE, 4 * G1_CARD_BYTES);
    t.dirty_addr_at_object_start(start(lo));
    t.dirty_addr_at_object_start(start(hi));
    assert_eq!(
        t.block_entry_addr(lo),
        Some(lo),
        "a later store into a HIGHER object raised the entry past an object \
         that still holds a cross-region reference"
    );

    // Descending: the same answer, reached by the branch that does take the
    // read-modify-write.
    let t = G1CardTable::new(BASE, 4 * G1_CARD_BYTES);
    t.dirty_addr_at_object_start(start(hi));
    t.dirty_addr_at_object_start(start(lo));
    assert_eq!(t.block_entry_addr(lo), Some(lo));
}

/// The monotone minimum has to survive repetition, which is the steady
/// state on the barrier: the same holder takes a store every round.
#[test]
fn repeating_a_store_never_moves_the_entry() {
    let t = G1CardTable::new(BASE, 4 * G1_CARD_BYTES);
    let lo = BASE + 8;
    let hi = BASE + 400;
    t.dirty_addr_at_object_start(start(lo));
    for _ in 0..100 {
        t.dirty_addr_at_object_start(start(hi));
        t.dirty_addr_at_object_start(start(lo));
    }
    assert_eq!(t.block_entry_addr(BASE), Some(lo));
}

/// An object starting exactly at a card's base is offset ZERO, which is a
/// legal value and must not be read as "unknown". This is why the sentinel
/// is `0xFF`.
#[test]
fn an_object_at_the_card_base_is_a_real_entry_and_not_unknown() {
    let t = G1CardTable::new(BASE, 4 * G1_CARD_BYTES);
    let at_base = BASE + 2 * G1_CARD_BYTES;
    t.dirty_addr_at_object_start(start(at_base));
    assert_eq!(t.block_entry_addr(at_base), Some(at_base));
}

/// The conservative producer leaves no entry. It cannot: it was given an
/// address it does not know to be an object start, and the reader's only
/// safe answer to "I do not know" is the linear walk.
#[test]
fn the_conservative_producer_dirties_the_card_and_names_no_entry() {
    let t = G1CardTable::new(BASE, 4 * G1_CARD_BYTES);
    let a = BASE + G1_CARD_BYTES + 128;
    t.dirty_addr(a);
    assert!(t.is_dirty_addr(a));
    assert_eq!(t.block_entry_addr(a), None);
}

/// AN ARENA BASE THAT IS NOT CARD-ALIGNED. The card index is relative to
/// `base`, so a block offset must be too; measuring it against the absolute
/// address instead names an address inside the right card at the wrong
/// offset, which is a mis-entry.
///
/// Today's arena base is an `mmap`/`VirtualAlloc` reservation and is
/// page-aligned, so the two forms agree and the bug this catches would be
/// latent. `G1CardTable`'s own type docs are explicit that region-to-card
/// alignment holds "whatever `base`'s own alignment is", and
/// `jit/src/x64/objects.rs` says of the same subtraction that being correct
/// for ANY base "is the property to preserve". This test is what preserves
/// it here.
#[test]
fn a_base_that_is_not_card_aligned_still_names_the_exact_object() {
    // Deliberately not a multiple of `G1_CARD_BYTES`, and not a multiple of
    // 64 either, so neither the card nor the summary shift can hide it.
    const ODD_BASE: usize = 0x1000_0000 + 8;
    let t = G1CardTable::new(ODD_BASE, 8 * G1_CARD_BYTES);
    // Three objects spread across the covered run, one of them in the first
    // partial card and one just past a card boundary relative to the base.
    for obj in [
        ODD_BASE,
        ODD_BASE + 24,
        ODD_BASE + G1_CARD_BYTES,
        ODD_BASE + 3 * G1_CARD_BYTES + 128,
    ] {
        let t = G1CardTable::new(ODD_BASE, 8 * G1_CARD_BYTES);
        t.dirty_addr_at_object_start(start(obj));
        assert_eq!(
            t.block_entry_addr(obj),
            Some(obj),
            "an odd arena base moved the entry off the object it names"
        );
    }
    // And the minimum still resolves within one card when the card's own
    // run is offset from the address space's.
    let lo = ODD_BASE + G1_CARD_BYTES + 16;
    let hi = ODD_BASE + G1_CARD_BYTES + 400;
    t.dirty_addr_at_object_start(start(hi));
    t.dirty_addr_at_object_start(start(lo));
    assert_eq!(t.block_entry_addr(hi), Some(lo));
}

/// An unaligned address is not an object start anywhere in this VM. It must
/// not be truncated to the grain, because truncation names an address BELOW
/// the real object — the one direction that mis-enters a walk.
#[test]
fn an_unaligned_address_records_no_entry_and_disarms() {
    let _guard = ENFORCEMENT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    reset_block_offset_enforcement_for_tests();
    let t = G1CardTable::new(BASE, 4 * G1_CARD_BYTES);
    let a = BASE + G1_CARD_BYTES + 37;
    t.dirty_addr_at_object_start(start(a));
    assert!(t.is_dirty_addr(a), "the card is still dirtied");
    assert_eq!(
        t.block_entry_addr(a),
        None,
        "an unaligned address was rounded down into an entry"
    );
    // W5-B — and it DISARMS. Refusing the offset without disarming is the
    // hole `BLOCK_OFFSETS_DISARMED` exists for: a second object starting
    // in this card would leave the card's surviving entry above this one,
    // and the walk would jump over it.
    assert!(
        block_offsets_disarmed(),
        "an unaligned producer address left the jump armed"
    );
    reset_block_offset_enforcement_for_tests();
}

/// REGION RECYCLE IS THE ONE EVENT THAT INVALIDATES AN ENTRY, because it is
/// the one event that changes which addresses are object starts. If this
/// ever stops holding, the source walk decodes a header in bytes that a
/// reset zero-filled and a new incarnation has laid out differently.
#[test]
fn clearing_a_range_forgets_its_block_entries() {
    let t = G1CardTable::new(BASE, 8 * G1_CARD_BYTES);
    let inside = BASE + 2 * G1_CARD_BYTES + 64;
    let outside = BASE + 6 * G1_CARD_BYTES + 64;
    t.dirty_addr_at_object_start(start(inside));
    t.dirty_addr_at_object_start(start(outside));
    t.clear_range(BASE, 4 * G1_CARD_BYTES);
    assert_eq!(t.block_entry_addr(inside), None);
    assert_eq!(
        t.block_entry_addr(outside),
        Some(outside),
        "clearing one region's range forgot a neighbour's entry"
    );
}

/// Card CLEANING moves no object, so it must not forget an entry — the
/// whole value of the table is that it survives from the pause that
/// recorded it to the pause that uses it.
#[test]
fn cleaning_a_card_keeps_its_block_entry() {
    let t = G1CardTable::new(BASE, 4 * G1_CARD_BYTES);
    let obj = BASE + G1_CARD_BYTES + 64;
    t.dirty_addr_at_object_start(start(obj));
    let empty = t.empty_set(BASE, 4 * G1_CARD_BYTES);
    t.clean_and_redirty(BASE, 4 * G1_CARD_BYTES, &empty);
    assert!(!t.is_dirty_addr(obj), "the card was supposed to go clean");
    assert_eq!(
        t.block_entry_addr(obj),
        Some(obj),
        "card cleaning forgot a block entry it had no reason to"
    );
    // And a store that re-dirties the card can only pull the entry DOWN.
    let lower = BASE + G1_CARD_BYTES + 16;
    t.dirty_addr_at_object_start(start(lower));
    assert_eq!(t.block_entry_addr(obj), Some(lower));
}

/// Addresses outside the arena touch nothing here either — the twin of
/// `addresses_outside_the_arena_touch_nothing` for the new table.
#[test]
fn a_block_entry_outside_the_arena_is_none() {
    let t = G1CardTable::new(BASE, 4 * G1_CARD_BYTES);
    t.dirty_addr_at_object_start(start(BASE - 8));
    t.dirty_addr_at_object_start(start(BASE + 4 * G1_CARD_BYTES));
    assert_eq!(t.block_entry_addr(BASE - 8), None);
    assert_eq!(t.block_entry_addr(BASE + 4 * G1_CARD_BYTES), None);
    assert_eq!(t.block_entries_in(BASE, 4 * G1_CARD_BYTES), 0);
}

// ----------------------------------------------------------------------
// W5-B — the enforcement: the runtime vouch and the global disarm.
//
// These four are the only tests in this file that need REAL memory, and
// they need it for the reason the whole mechanism exists: the vouch reads
// sixteen bytes and asks the allocator's own bit whether they are a
// published header. A synthetic base cannot answer that question.
//
// [`BLOCK_OFFSETS_DISARMED`] is a process-wide latch, so the two cases
// that deliberately trip it would otherwise poison every other test in
// this binary — cargo runs them on several threads in one process. They
// take a mutex and reset the latch on the way in.
// ----------------------------------------------------------------------

/// Serialises the tests that trip the global disarm latch.
static ENFORCEMENT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// A miniature arena with REAL object headers in it, at addresses this
/// test file chooses rather than at whatever the allocator hands back.
///
/// Four cards of backing store, card-aligned, with a fresh
/// `ObjectHeader::new` written at each requested card-relative offset of
/// the SECOND card. Controlling the addresses is what lets the
/// two-objects-in-one-card case below be arithmetic rather than luck: a
/// `Box<ObjectHeader>` lands wherever the allocator puts it, which may be
/// eight bytes below a card boundary.
struct MiniArena {
    _backing: Vec<u64>,
    base: usize,
    table: G1CardTable,
}

impl MiniArena {
    fn new() -> Self {
        // Six cards of store so that a four-card, card-ALIGNED arena fits
        // inside it wherever the allocator lands.
        let backing = vec![0u64; 6 * G1_CARD_BYTES / 8];
        let raw = backing.as_ptr() as usize;
        let base = (raw + G1_CARD_BYTES - 1) & !(G1_CARD_BYTES - 1);
        let table = G1CardTable::new(base, 4 * G1_CARD_BYTES);
        Self {
            _backing: backing,
            base,
            table,
        }
    }

    /// Publish a real object header at `base + off` and return its address.
    fn put_header(&self, off: usize) -> usize {
        let addr = self.base + off;
        // SAFETY: `off` is inside the four cards the arena owns and is
        // 8-aligned, and the backing store outlives every use.
        unsafe {
            std::ptr::write(
                addr as *mut ObjectHeader,
                ObjectHeader::new(
                    cratonvm_types::ClassId::new(0),
                    cratonvm_types::ObjectKind::Object,
                    cratonvm_types::ArrayElementType::Reference,
                    0,
                    0,
                ),
            );
        }
        addr
    }

    /// The published header at `addr`, as the producers hold it.
    fn header_at(&self, addr: usize) -> &ObjectHeader {
        // SAFETY: `put_header` published one there.
        unsafe { &*(addr as *const ObjectHeader) }
    }
}

/// The ordinary case: `ObjectHeader::new` sets `GC_FLAG_HEADER`, so a
/// producer naming a real object start records its entry and nothing is
/// disarmed.
#[test]
fn a_real_object_header_vouches_and_its_entry_is_recorded() {
    let _guard = ENFORCEMENT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    reset_block_offset_enforcement_for_tests();
    let a = MiniArena::new();
    let addr = a.put_header(G1_CARD_BYTES + 128);
    a.table
        .dirty_addr_at_object_start(ObjectStart::of_header(a.header_at(addr)));
    assert!(a.table.is_dirty_addr(addr));
    assert_eq!(a.table.block_entry_addr(addr), Some(addr));
    // The census is read as a DELTA around this call, not as an absolute.
    // `vouch_checks` is a process-global counter and `ENFORCEMENT_LOCK`
    // only serialises the tests that know about it -- every OTHER test in
    // this binary that dirties an object start bumps it too, on libtest's
    // other threads. Asserting `== 1` therefore passed alone and failed in
    // the full suite: an assertion about test ordering wearing the costume
    // of an assertion about the vouch.
    let (checks, unvouched, disarmed) = block_offset_enforcement_census();
    assert!(
        checks >= 1,
        "a real header must be vouched, saw checks={checks}"
    );
    assert_eq!(unvouched, 0, "a real header must not be counted unvouched");
    assert!(!disarmed, "a real header must not disarm the jump");
    reset_block_offset_enforcement_for_tests();
}

/// The case the enforcement exists for. Sixteen bytes without
/// `GC_FLAG_HEADER` are not an object start whatever the caller believed,
/// so: the card IS dirtied (a remembered set may only over-remember), NO
/// entry is recorded, the address is counted, and the jump is disarmed for
/// the whole process.
#[test]
fn a_producer_naming_bytes_without_the_header_flag_records_nothing_and_disarms() {
    let _guard = ENFORCEMENT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    reset_block_offset_enforcement_for_tests();
    let a = MiniArena::new();
    let addr = a.put_header(G1_CARD_BYTES + 128);
    // Exactly what a slot address looks like to the vouch: bytes that are
    // not a published header.
    a.header_at(addr).set_gc_flags(0);
    a.table
        .dirty_addr_at_object_start(ObjectStart::of_header(a.header_at(addr)));
    assert!(
        a.table.is_dirty_addr(addr),
        "the card must still be dirty — refusing the OFFSET may not refuse the EDGE"
    );
    assert_eq!(
        a.table.block_entry_addr(addr),
        None,
        "an unvouched address must not become a walk entry"
    );
    // The census is read as a DELTA, not as an absolute. `vouch_checks`
    // and `unvouched` are process-global and `ENFORCEMENT_LOCK` only
    // serialises the tests that KNOW about it -- every other test in this
    // binary that dirties an object start bumps them too, on libtest's
    // other threads. Asserting `== (1, 1, true)` therefore passed when run
    // alone and failed in the full suite: an assertion about test ordering
    // wearing the costume of an assertion about the refusal. This is the
    // same fix the sibling test above already carries.
    let (checks, unvouched, disarmed) = block_offset_enforcement_census();
    assert!(
        checks >= 1,
        "the refusal must be counted, saw checks={checks}"
    );
    assert!(
        unvouched >= 1,
        "an unvouched producer must be counted unvouched, saw {unvouched}"
    );
    assert!(disarmed, "an unvouched producer must disarm the jump");
    assert!(block_offsets_disarmed());
    reset_block_offset_enforcement_for_tests();
}

/// The disarm is what makes "do not record" SOUND, and this is the shape
/// that proves it is needed.
///
/// Two objects start in one card. The lower one's producer does not vouch,
/// so its offset is dropped; the higher one's does. Without the global
/// disarm the card's entry would name the HIGHER object and the walk would
/// jump straight past the lower one — a dropped remembered-set edge, which
/// is a use-after-free. The entry is indeed the higher one; the latch is
/// what stops anybody acting on it.
#[test]
fn dropping_an_unvouched_offset_would_leave_a_higher_entry_which_is_why_it_disarms() {
    let _guard = ENFORCEMENT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    reset_block_offset_enforcement_for_tests();
    let a = MiniArena::new();
    // Two object starts in ONE card, 64 bytes apart.
    let lower_addr = a.put_header(G1_CARD_BYTES + 64);
    let higher_addr = a.put_header(G1_CARD_BYTES + 128);
    a.header_at(lower_addr).set_gc_flags(0);

    a.table
        .dirty_addr_at_object_start(ObjectStart::of_header(a.header_at(lower_addr)));
    a.table
        .dirty_addr_at_object_start(ObjectStart::of_header(a.header_at(higher_addr)));

    assert_eq!(
        a.table.block_entry_addr(lower_addr),
        Some(higher_addr),
        "this is the hole: the surviving entry is ABOVE the object whose offset was dropped"
    );
    assert!(
        block_offsets_disarmed(),
        "and this is what closes it: nothing may read that entry again"
    );
    reset_block_offset_enforcement_for_tests();
}

/// `ObjectStart::of_header` derives the address FROM the reference, so a
/// producer cannot pass a header and an unrelated address. This is the
/// compile-time half of the enforcement, stated as a test because there is
/// no way to state it as one in the type system's own words.
#[test]
fn an_object_starts_address_is_the_headers_own_address() {
    let a = MiniArena::new();
    let addr = a.put_header(G1_CARD_BYTES + 256);
    let at = ObjectStart::of_header(a.header_at(addr));
    assert_eq!(at.addr(), addr);
    assert!(at.vouched());
    a.header_at(addr).set_gc_flags(0);
    assert!(!ObjectStart::of_header(a.header_at(addr)).vouched());
}

/// The cursor and the table compose into the thing the walk actually does:
/// find the next dirty card, then enter its grid. Exercised over a grid
/// whose object size is known, so the expected answer is arithmetic rather
/// than a re-derivation of the code under test.
#[test]
fn the_cursor_and_the_table_together_name_a_real_object_start() {
    const OBJ: usize = 64;
    let cards = 16;
    let t = G1CardTable::new(BASE, cards * G1_CARD_BYTES);
    // An object grid of 64-byte objects over the whole span, with a
    // cross-region store into every eleventh one.
    let objects = cards * G1_CARD_BYTES / OBJ;
    let mut named = Vec::new();
    for o in (0..objects).step_by(11) {
        let a = BASE + o * OBJ;
        t.dirty_addr_at_object_start(start(a));
        named.push(a);
    }
    let snap = t.snapshot(BASE, cards * G1_CARD_BYTES);

    // Walk the way the source walk does: from the region base, hop to the
    // next dirty card, enter it, and confirm the entry is one of the
    // addresses a producer actually named and is the LOWEST such address in
    // its card.
    let mut here = BASE;
    let mut entered = 0usize;
    while let Some(d) = snap.next_dirty_addr(here) {
        let entry = t
            .block_entry_addr(d)
            .expect("a dirty card with no entry, but every producer named a start");
        assert!(
            named.contains(&entry),
            "entry 0x{entry:x} is not an address any producer named"
        );
        let card_lo = d;
        let card_hi = d + G1_CARD_BYTES;
        let lowest = named
            .iter()
            .copied()
            .filter(|&a| a >= card_lo && a < card_hi)
            .min()
            .unwrap();
        assert_eq!(entry, lowest, "the entry was not the card's lowest start");
        entered += 1;
        here = card_hi;
        if here >= BASE + cards * G1_CARD_BYTES {
            break;
        }
    }
    assert!(entered > 0, "the fixture produced no dirty card at all");
}

/// gci i15/rs -- an OLD_ONLY card is dirty to every `!= CLEAN` reader and
/// clean to every young-only reader, and the next producer store promotes
/// it back to young-dirty.
#[test]
fn i15rs_an_old_only_card_is_dirty_to_the_sticky_readers_and_clean_to_the_young_ones() {
    let t = G1CardTable::new(BASE, 8 * G1_CARD_BYTES);
    let a = BASE + 2 * G1_CARD_BYTES + 8;
    t.dirty_addr_at_object_start(start(a));
    let keep = t.empty_set(BASE, 8 * G1_CARD_BYTES);
    assert_eq!(t.demote_unkept_young(BASE, 8 * G1_CARD_BYTES, &keep), 1);
    // The sticky readers: unchanged.
    assert!(t.is_dirty_addr(a));
    assert!(t.any_dirty_in(BASE, 8 * G1_CARD_BYTES));
    assert_eq!(t.dirty_cards_in(BASE, 8 * G1_CARD_BYTES), 1);
    assert_eq!(t.snapshot(BASE, 8 * G1_CARD_BYTES).count(), 1);
    // The young readers: clean.
    assert!(!t.is_young_dirty_addr(a));
    assert!(!t.any_young_dirty_in(BASE, 8 * G1_CARD_BYTES));
    assert!(!t.any_young_dirty_in(a, 16));
    assert_eq!(t.snapshot_young(BASE, 8 * G1_CARD_BYTES).count(), 0);
    // The block-offset entry survives the demotion (no object moved).
    assert_eq!(t.block_entry_addr(a), Some(a));
    // A producer store promotes it back.
    t.dirty_addr_at_object_start(start(a));
    assert!(t.is_young_dirty_addr(a));
    assert!(t.any_young_dirty_in(BASE, 8 * G1_CARD_BYTES));
    // A plain `dirty_addr` too.
    assert_eq!(t.demote_unkept_young(BASE, 8 * G1_CARD_BYTES, &keep), 1);
    t.dirty_addr(a + 16);
    assert!(t.is_young_dirty_addr(a));
}

/// gci i15/rs -- `keep` wins: a kept card ends young-dirty whatever it was
/// (CLEAN, OLD_ONLY or DIRTY), an unkept DIRTY card is demoted, an unkept
/// CLEAN card stays CLEAN (demotion never invents a dirty card), and the
/// summary stays a valid index for both readers.
#[test]
fn i15rs_demotion_keeps_the_kept_cards_and_never_cleans_or_invents_one() {
    let n = 2 * G1_CARDS_PER_SUMMARY;
    let t = G1CardTable::new(BASE, n * G1_CARD_BYTES);
    let card = |c: usize| BASE + c * G1_CARD_BYTES;
    for c in [1usize, 3, 5, 70] {
        t.dirty_addr_at_object_start(start(card(c)));
    }
    // First pass: keep card 3 only.
    let mut keep = t.empty_set(BASE, n * G1_CARD_BYTES);
    keep.insert_addr(card(3) + 40);
    assert_eq!(t.demote_unkept_young(BASE, n * G1_CARD_BYTES, &keep), 3);
    assert!(t.is_young_dirty_addr(card(3)));
    for c in [1usize, 5, 70] {
        assert!(t.is_dirty_addr(card(c)) && !t.is_young_dirty_addr(card(c)), "card {c}");
    }
    assert!(!t.is_dirty_addr(card(2)), "an unkept CLEAN card must stay clean");
    // Second pass: keep a CLEAN card (an object whose start card was never
    // named but whose walk found a young reference) and an OLD_ONLY one.
    let mut keep2 = t.empty_set(BASE, n * G1_CARD_BYTES);
    keep2.insert_addr(card(2));
    keep2.insert_addr(card(70));
    assert_eq!(t.demote_unkept_young(BASE, n * G1_CARD_BYTES, &keep2), 1, "card 3");
    assert!(t.is_young_dirty_addr(card(2)));
    assert!(t.is_young_dirty_addr(card(70)));
    assert!(!t.is_young_dirty_addr(card(3)));
    // Both readers through the summary path (a span over 64 cards).
    assert!(t.any_young_dirty_in(BASE, n * G1_CARD_BYTES));
    assert!(t.any_young_dirty_in(card(64), 64 * G1_CARD_BYTES));
    assert_eq!(t.snapshot_young(BASE, n * G1_CARD_BYTES).count(), 2);
    assert_eq!(t.snapshot(BASE, n * G1_CARD_BYTES).count(), 5);
    // A range that is only OLD_ONLY reads young-clean through the summary
    // path as well, and still sticky-dirty.
    let empty = t.empty_set(BASE, n * G1_CARD_BYTES);
    t.demote_unkept_young(BASE, n * G1_CARD_BYTES, &empty);
    assert!(!t.any_young_dirty_in(BASE, n * G1_CARD_BYTES));
    assert!(t.any_dirty_in(BASE, n * G1_CARD_BYTES));
}

/// gci i15/rs -- the young snapshot drives the cursor exactly as the sticky
/// one does: `next_dirty_addr` steps over OLD_ONLY cards.
#[test]
fn i15rs_the_young_snapshot_cursor_steps_over_old_only_cards() {
    let t = G1CardTable::new(BASE, 16 * G1_CARD_BYTES);
    let card = |c: usize| BASE + c * G1_CARD_BYTES;
    for c in [2usize, 6, 11] {
        t.dirty_addr_at_object_start(start(card(c)));
    }
    let mut keep = t.empty_set(BASE, 16 * G1_CARD_BYTES);
    keep.insert_addr(card(11));
    t.demote_unkept_young(BASE, 16 * G1_CARD_BYTES, &keep);
    let snap = t.snapshot_young(BASE, 16 * G1_CARD_BYTES);
    assert_eq!(snap.first_dirty_addr(), Some(card(11)));
    assert_eq!(snap.next_dirty_addr(card(3)), Some(card(11)));
    assert_eq!(snap.next_dirty_addr(card(12)), None);
    assert!(!snap.any_in_span(card(2), G1_CARD_BYTES));
    assert!(snap.any_in_span(card(10), 2 * G1_CARD_BYTES));
}
