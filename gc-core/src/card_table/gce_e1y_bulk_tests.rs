use super::*;

/// A bulk list with runs of the same card, cards already dirty, cards
/// outside the table and a card revisited after others leaves exactly the
/// cards (and, with the summary, exactly the groups) that one
/// `mark_dirty` per entry leaves, and the scan bound covers them all.
#[test]
fn e1y_a_bulk_remark_with_repeats_dirties_what_single_marks_dirty() {
    for options in [
        CardTableOptions::LEGACY,
        CardTableOptions {
            summary: true,
            precise_ref_array_marks: true,
        },
    ] {
        let base = 0x80_0000usize;
        let bulk = CardTable::with_options(base, CARD_SIZE * 1024, options);
        let single = CardTable::with_options(base, CARD_SIZE * 1024, options);
        let at = |card: usize, off: usize| base + card * CARD_SIZE + off;
        // Card 40 dirty before the bulk call (an already-dirty skip).
        bulk.mark_dirty(at(40, 0));
        single.mark_dirty(at(40, 0));
        let list = [
            at(3, 0),
            at(3, 8),
            at(3, 16),
            at(40, 8),
            at(900, 0),
            at(900, 504),
            at(3, 24),
            base - 8,
            base + CARD_SIZE * 1024,
            at(1023, 0),
        ];
        bulk.mark_dirty_bulk(&list);
        for &a in &list {
            single.mark_dirty(a);
        }
        assert_eq!(
            bulk.scan_bound_cards(),
            single.scan_bound_cards(),
            "{options:?}"
        );
        assert_eq!(
            bulk.dirty_card_indices(),
            single.dirty_card_indices(),
            "{options:?}"
        );
        assert_eq!(
            bulk.take_dirty_cards(),
            vec![3, 40, 900, 1023],
            "{options:?}"
        );
        assert_eq!(
            single.take_dirty_cards(),
            vec![3, 40, 900, 1023],
            "{options:?}"
        );
        assert!(
            bulk.take_dirty_cards().is_empty(),
            "consumed, summary included"
        );
        // After a consuming scan the same list marks everything again.
        bulk.mark_dirty_bulk(&list);
        assert_eq!(
            bulk.take_dirty_cards(),
            vec![3, 40, 900, 1023],
            "{options:?}"
        );
    }
}
