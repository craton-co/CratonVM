use super::{zgc_satb_window_latch_holds, zgc_satb_window_refuses};
use std::cell::Cell;

#[test]
fn a_mark_start_that_froze_nobody_opens_and_never_reads_the_switch() {
    let asked = Cell::new(false);
    assert!(!zgc_satb_window_refuses(0, || {
        asked.set(true);
        true
    }));
    assert!(
        !asked.get(),
        "the switch is read only when a peer was frozen"
    );
}

#[test]
fn a_frozen_peer_refuses_unless_the_kill_switch_is_off() {
    assert!(zgc_satb_window_refuses(1, || true));
    assert!(zgc_satb_window_refuses(7, || true));
    assert!(
        !zgc_satb_window_refuses(1, || false),
        "=0 restores the open"
    );
}

#[test]
fn the_latch_holds_for_the_refusing_epoch_only() {
    assert!(!zgc_satb_window_latch_holds(0, 0), "never refused");
    assert!(!zgc_satb_window_latch_holds(0, 5), "never refused");
    // Refused at collection count 5: stored as 6.
    assert!(zgc_satb_window_latch_holds(6, 5));
    assert!(!zgc_satb_window_latch_holds(6, 6), "a collection lifts it");
    assert!(!zgc_satb_window_latch_holds(6, 9));
    // Refused before the first collection: stored as 1, `0` stays "none".
    assert!(zgc_satb_window_latch_holds(1, 0));
    assert!(!zgc_satb_window_latch_holds(1, 1));
}
