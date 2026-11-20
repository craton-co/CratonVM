use super::*;

/// Every mark path — the locked one, the bulk one (out of order and with
/// duplicates, as the collector's deferred re-marks come), the lock-free
/// barrier and the buffered drain — is found by the byte scan, ascending
/// and once each, on the summary table and on the legacy one; a second take
/// finds nothing; a bulk re-mark after `clear_all` is found again.
#[test]
fn the_byte_scan_returns_every_mark_ascending_and_once() {
    for options in [
        CardTableOptions::LEGACY,
        CardTableOptions {
            summary: true,
            precise_ref_array_marks: false,
        },
    ] {
        let base = 0x40_0000usize;
        let ct = CardTable::with_options(base, CARD_SIZE * 512, options);
        let at = |card: usize| base + card * CARD_SIZE + 8;
        ct.mark_dirty(at(300));
        ct.mark_dirty_bulk(&[at(200), at(7), at(200), at(450), at(7)]);
        ct.mark_dirty_lockfree(at(64));
        ct.mark_dirty_lockfree(at(300));
        ct.thread_local_dirty_addr(at(100));
        ct.flush_dirty_buffer();
        assert_eq!(ct.drain_pending(), 1, "card 100 was clean");
        assert_eq!(ct.take_dirty_cards(), vec![7, 64, 100, 200, 300, 450]);
        assert!(ct.take_dirty_cards().is_empty(), "consumed");
        ct.mark_dirty_bulk(&[at(9), at(3)]);
        ct.clear_all();
        assert!(ct.take_dirty_cards().is_empty(), "cleared");
        ct.mark_dirty_bulk(&[at(11), at(5), at(11)]);
        assert_eq!(ct.take_dirty_cards(), vec![5, 11], "re-marked");
    }
}
