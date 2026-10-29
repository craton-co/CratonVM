//! JIT round 21 lane parkcredit -- `g1_kept_pin_words`: the initiator's movable
//! claim drops only its own published words; another thread's deposit or a
//! blocked peer's helper-window word naming the same object still pins it
//! (`jr21-g1-initiator-movable-claim-unpinned-peer-words`).

use super::g1_kept_pin_words;

const X: usize = 0x1000;
const Y: usize = 0x2000;
const Z: usize = 0x3000;

#[test]
fn a_peer_word_pins_an_object_the_initiator_claims_movable() {
    // The initiator's oop-map slot names X (claimed movable, no veto); a
    // parked peer's register names X too. X must stay pinned.
    let (kept, n_mov, n_unrew) = g1_kept_pin_words(vec![X], vec![X], true, |a| a == X, |_| false);
    assert_eq!(kept, vec![X]);
    assert_eq!((n_mov, n_unrew), (1, 0));
}

#[test]
fn the_initiators_own_claimed_word_alone_is_dropped() {
    let (kept, n_mov, _) = g1_kept_pin_words(vec![X, Y], vec![], true, |a| a == X, |_| false);
    assert_eq!(kept, vec![Y]);
    assert_eq!(n_mov, 1);
}

#[test]
fn the_veto_and_the_switch_keep_own_words() {
    // Vetoed: kept.
    let (kept, _, n_unrew) = g1_kept_pin_words(vec![X], vec![], true, |_| true, |a| a == X);
    assert_eq!(kept, vec![X]);
    assert_eq!(n_unrew, 1);
    // Filter off (`CRATONVM_GC_G1_MOVABLE_PINS=0`): everything pins.
    let (kept, _, _) = g1_kept_pin_words(vec![X], vec![Y], false, |_| true, |_| false);
    assert_eq!(kept, vec![X, Y]);
}

#[test]
fn others_are_never_consulted_against_the_callers_claims() {
    // Even a peer word the caller's sets claim movable pins, and the counts
    // cover only the caller's own words.
    let (mut kept, n_mov, n_unrew) =
        g1_kept_pin_words(vec![Z], vec![X, Y], true, |_| true, |_| false);
    kept.sort_unstable();
    assert_eq!(kept, vec![X, Y]);
    assert_eq!((n_mov, n_unrew), (1, 0));
}
