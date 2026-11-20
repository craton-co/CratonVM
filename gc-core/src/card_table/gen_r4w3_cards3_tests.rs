use super::*;

const ON: CardTableOptions = CardTableOptions {
    summary: true,
    precise_ref_array_marks: false,
};

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

fn brute(ct: &CardTable) -> Vec<usize> {
    (0..ct.num_cards()).filter(|&i| ct.is_dirty(i)).collect()
}

/// The equivalence the summary rests on: over randomized interleavings of
/// all four mark paths and all three scans, a summarised table returns
/// exactly the card sets, and leaves exactly the byte map, that the flat
/// (legacy) table does. The region is deliberately not a whole number of
/// summary groups, and the first round also marks the first and last card
/// of a group on both sides of a group boundary.
#[test]
fn the_summary_scan_finds_exactly_what_the_flat_scan_finds() {
    let _arm = super::tests::METRICS_ARM.lock();
    let base = 0x40_0000usize;
    let num = SUMMARY_GROUP * 7 + 13;
    for seed in 1..=24u64 {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15 ^ seed);
        let flat = CardTable::with_options(base, CARD_SIZE * num, CardTableOptions::LEGACY);
        let summ = CardTable::with_options(base, CARD_SIZE * num, ON);
        let fixed = [
            0,
            SUMMARY_GROUP - 1,
            SUMMARY_GROUP,
            3 * SUMMARY_GROUP - 1,
            num - 1,
        ];
        for round in 0..40 {
            let cards: Vec<usize> = if round == 0 {
                fixed.to_vec()
            } else {
                (0..rng.below(12)).map(|_| rng.below(num)).collect()
            };
            for card in cards {
                let addr = base + card * CARD_SIZE + rng.below(CARD_SIZE);
                let path = rng.below(4);
                for ct in [&flat, &summ] {
                    match path {
                        0 => ct.mark_dirty_lockfree(addr),
                        1 => ct.mark_dirty(addr),
                        2 => ct.mark_dirty_bulk(&[addr]),
                        _ => {
                            ct.thread_local_dirty_addr(addr);
                            ct.flush_dirty_buffer();
                            ct.drain_pending();
                        }
                    }
                }
            }
            match rng.below(4) {
                0 => assert_eq!(
                    summ.dirty_card_indices(),
                    flat.dirty_card_indices(),
                    "seed {seed} round {round}: dirty_card_indices"
                ),
                1 | 2 => assert_eq!(
                    summ.take_dirty_cards(),
                    flat.take_dirty_cards(),
                    "seed {seed} round {round}: take_dirty_cards"
                ),
                _ => {
                    summ.clear_all();
                    flat.clear_all();
                }
            }
            assert_eq!(
                brute(&summ),
                brute(&flat),
                "seed {seed} round {round}: byte map"
            );
        }
        // And the table's own brute force agrees with its scans at the end.
        assert_eq!(summ.dirty_card_indices(), brute(&summ), "seed {seed}");
        assert_eq!(
            summ.take_dirty_cards(),
            flat.take_dirty_cards(),
            "seed {seed}"
        );
    }
}

/// The summary actually skips, and says how much: one dirty card at each
/// end of a 32-group bound reads two groups and skips thirty. A second
/// scan, after the first cleared both summary bytes, skips everything.
#[test]
fn the_summary_skips_clean_groups_and_counts_them() {
    let _arm = super::tests::METRICS_ARM.lock();
    let base = 0x10_0000usize;
    let groups = 32;
    let ct = CardTable::with_options(base, CARD_SIZE * SUMMARY_GROUP * groups, ON);
    let far = SUMMARY_GROUP * (groups - 1) + 1;
    ct.mark_dirty_lockfree(base + 5 * CARD_SIZE);
    ct.mark_dirty_lockfree(base + far * CARD_SIZE);
    assert_eq!(ct.scan_bound_cards(), far + 1);
    assert_eq!(ct.take_dirty_cards(), vec![5, far]);
    assert_eq!(
        ct.cards_skipped_by_summary(),
        (SUMMARY_GROUP * (groups - 2)) as u64,
        "thirty whole groups between the two marks were never read",
    );
    assert!(ct.take_dirty_cards().is_empty());
    assert_eq!(
        ct.cards_skipped_by_summary(),
        (SUMMARY_GROUP * (groups - 2) + far + 1) as u64,
        "the consuming scan cleared both summary bytes, so the second skips all",
    );
    // The flat table never counts a summary skip.
    let flat = CardTable::with_options(base, CARD_SIZE * 64, CardTableOptions::LEGACY);
    flat.mark_dirty_lockfree(base);
    assert_eq!(flat.take_dirty_cards(), vec![0]);
    assert_eq!(flat.cards_skipped_by_summary(), 0);
}

/// Why the lock-free barrier stores the summary byte TWICE.
///
/// A peer frozen between the first summary store and its card store (BUG-03
/// `SuspendThread`) lets a scan clear the summary byte and find the card
/// still clean. When the peer resumes, its card store lands under a clean
/// summary byte. With only the first store, no later scan would descend
/// into that group again until something else in it was dirtied — the edge
/// would be lost for good. The second store re-arms the group.
///
/// Modelled by driving `mark_dirty_lockfree`'s three stores by hand around
/// a consuming scan.
#[test]
fn the_barriers_second_summary_store_rearms_a_group_a_scan_cleared() {
    let _arm = super::tests::METRICS_ARM.lock();
    let base = 0x20_0000usize;
    let ct = CardTable::with_options(base, CARD_SIZE * SUMMARY_GROUP * 4, ON);
    let idx = SUMMARY_GROUP * 2 + 9;
    // SAFETY: `cards_addr` is this live table's `[AtomicU8; num_cards]`
    // and `idx < num_cards`.
    let card = unsafe { &*(ct.cards_addr as *const AtomicU8).add(idx) };

    // Peer: cover + first summary store, then frozen.
    ct.cover_card_index(idx);
    ct.summarize(idx);
    // Collector: consumes the group — clears its summary byte, finds nothing.
    assert!(ct.take_dirty_cards().is_empty());
    // Peer resumes: the card store alone leaves the hazard in place...
    card.store(CARD_DIRTY, Ordering::Release);
    assert_eq!(brute(&ct), vec![idx]);
    assert!(
        ct.dirty_card_indices().is_empty(),
        "a dirty card under a clean summary byte is invisible to the scans",
    );
    // ...and the second summary store is what closes it.
    ct.summarize(idx);
    assert_eq!(ct.take_dirty_cards(), vec![idx]);
    assert!(brute(&ct).is_empty());

    // The production barrier does all three, so the ordinary path is fine.
    ct.mark_dirty_lockfree(base + idx * CARD_SIZE);
    assert_eq!(ct.take_dirty_cards(), vec![idx]);
}

/// A card stored ONLY through the raw address cannot maintain the summary,
/// so handing the address out must switch the scans back to the byte map —
/// the summary's version of `handing_out_the_raw_address_disarms_the_scan_bound`,
/// and the test the O(heap) page asked for ("marks a card only through
/// `jit_cards_addr` and asserts `take_dirty_cards` still returns it").
#[test]
fn a_raw_card_store_is_found_once_the_address_has_escaped() {
    let base = 0x30_0000usize;
    let ct = CardTable::with_options(base, CARD_SIZE * SUMMARY_GROUP * 4, ON);
    assert!(ct.summary_active());
    let cards = ct.jit_cards_addr() as *const AtomicU8;
    assert!(
        !ct.summary_active(),
        "an escaped address disarms the summary"
    );
    let idx = SUMMARY_GROUP * 3 + 7;
    // SAFETY: the table's live byte map; `idx < num_cards`.
    unsafe { &*cards.add(idx) }.store(CARD_DIRTY, Ordering::Release);
    assert_eq!(ct.dirty_card_indices(), vec![idx]);
    assert_eq!(ct.take_dirty_cards(), vec![idx]);
    // SAFETY: as above.
    unsafe { &*cards.add(idx) }.store(CARD_DIRTY, Ordering::Release);
    ct.clear_all();
    assert!(brute(&ct).is_empty());
}

/// The options are what the table was built with, and LEGACY builds no
/// summary at all.
#[test]
fn the_legacy_options_build_the_pre_summary_table() {
    let ct = CardTable::with_options(0x1000, CARD_SIZE * 1000, CardTableOptions::LEGACY);
    assert_eq!(ct.options(), CardTableOptions::LEGACY);
    assert!(ct.summary.is_empty());
    assert!(!ct.precise_ref_array_marks());
    let on = CardTable::with_options(
        0x1000,
        CARD_SIZE * 1000,
        CardTableOptions {
            summary: true,
            precise_ref_array_marks: true,
        },
    );
    assert_eq!(on.summary.len(), 1000usize.div_ceil(SUMMARY_GROUP));
    assert!(on.precise_ref_array_marks());
}
