//! JIT round 19 wave 2 (lane throws2, WALK17-4): the per-walk activation
//! memo keeps the last two keys, answers only an equal key (the slot word
//! is part of it), and the walk's lookups remember the census column they
//! counted last, which a hit replays.
use super::{ActivationMemo, ActivationMemoEntry, ActivationMemoKey, WalkLookups};

fn key(cm: usize, pc: usize, sp_id: Option<u32>) -> ActivationMemoKey {
    ActivationMemoKey { cm, pc, sp_id }
}

fn entry(bci: i32) -> ActivationMemoEntry {
    ActivationMemoEntry {
        bci,
        census: 3,
        chain: None,
        chain_exact: true,
    }
}

#[test]
fn two_ways_and_an_exact_key() {
    let mut memo = ActivationMemo::default();
    assert!(memo.get(&key(1, 2, Some(3))).is_none());
    memo.insert(key(1, 2, Some(3)), entry(10));
    memo.insert(key(1, 4, Some(3)), entry(11));
    assert_eq!(memo.get(&key(1, 2, Some(3))).map(|e| e.bci), Some(10));
    assert_eq!(memo.get(&key(1, 4, Some(3))).map(|e| e.bci), Some(11));
    // Another slot word, another artifact, no slot: misses.
    assert!(memo.get(&key(1, 2, Some(4))).is_none());
    assert!(memo.get(&key(9, 2, Some(3))).is_none());
    assert!(memo.get(&key(1, 2, None)).is_none());
    // A third key replaces the oldest way.
    memo.insert(key(5, 6, None), entry(12));
    assert!(memo.get(&key(1, 2, Some(3))).is_none());
    assert_eq!(memo.get(&key(1, 4, Some(3))).map(|e| e.bci), Some(11));
    assert_eq!(memo.get(&key(5, 6, None)).map(|e| e.bci), Some(12));
}

#[test]
fn lookups_remember_the_last_census_column() {
    let mut lookups = WalkLookups::per_call();
    lookups.note(cratonvm_jit::FRAME_LINE_ANSWERED_IR);
    assert_eq!(lookups.last_noted, cratonvm_jit::FRAME_LINE_ANSWERED_IR);
    lookups.note(cratonvm_jit::FRAME_LINE_REFUSED_NO_SP_ID);
    assert_eq!(
        lookups.last_noted,
        cratonvm_jit::FRAME_LINE_REFUSED_NO_SP_ID
    );
}
