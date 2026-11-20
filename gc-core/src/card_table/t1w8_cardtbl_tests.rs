use super::*;

fn opts() -> CardTableOptions {
    CardTableOptions {
        summary: true,
        precise_ref_array_marks: true,
    }
}

/// A hull whose low end lies within the first summary group's bytes of
/// address 0 used to compute `hull_lo` with an unchecked subtraction that
/// wraps below 0; it is refused instead (the historical table is kept).
#[test]
fn t1w8_cardtbl_a_hull_rounding_below_address_zero_is_refused() {
    let group = CARD_SIZE * SUMMARY_GROUP;
    let base = group + CARD_SIZE; // one card past the first group
    assert!(CardTable::with_hull(base, CARD_SIZE * 64, opts(), (256, base + CARD_SIZE * 64), usize::MAX, 1)
        .is_none());
    // A hull whose rounded low end stays at or above 0 is still accepted.
    let ct = CardTable::with_hull(base, CARD_SIZE * 64, opts(), (CARD_SIZE, base + CARD_SIZE * 64), usize::MAX, 1)
        .expect("a hull that does not round below 0 is accepted");
    assert_eq!(ct.jit_card_view().hull_lo.load(Ordering::Relaxed), base - group);
}

/// A hull whose low end is NOT a whole number of groups below the base:
/// `hull_lo` steps down to the next group boundary, so the compiled hull
/// summary index names the table's own summary byte for every old card,
/// and the hull map's length covers the old range's last card.
#[test]
fn t1w8_cardtbl_an_unaligned_hull_low_end_keeps_both_summary_indexings_equal() {
    let group = CARD_SIZE * SUMMARY_GROUP;
    let old_base = 0x400_0000usize;
    let old_size = CARD_SIZE * 1000; // not a whole number of groups
    let young_lo = old_base - 3 * group - 5 * CARD_SIZE;
    let ct = CardTable::with_hull(old_base, old_size, opts(), (young_lo, old_base + old_size), usize::MAX, 1)
        .expect("accepted");
    let v = ct.jit_card_view();
    let lo = v.hull_lo.load(Ordering::Relaxed);
    let len = v.hull_len.load(Ordering::Relaxed);
    assert!(lo <= young_lo && (old_base - lo) % group == 0);
    assert!(lo + len >= old_base + old_size && len % group == 0);
    let hull_summary = v.hull_summary_neg.load(Ordering::Relaxed).wrapping_neg();
    let own_summary = v.summary_neg.load(Ordering::Relaxed).wrapping_neg();
    let buf = v.hull_buf_neg.load(Ordering::Relaxed).wrapping_neg();
    let cards = v.cards_neg.load(Ordering::Relaxed).wrapping_neg();
    for card in [0usize, 63, 64, 999] {
        let mark = old_base + card * CARD_SIZE;
        assert_eq!(((mark - lo) >> cratonvm_types::CARD_SHIFT) + buf, cards + card, "card {card}");
        assert_eq!(
            ((mark - lo) >> (cratonvm_types::CARD_SHIFT + 6)) + hull_summary,
            (card >> 6) + own_summary,
            "summary of card {card}"
        );
    }
}
