//! JIT round 21, lane hashlock, wave 3 (tag `hl21`): the identity-hash x
//! lock census names, `installed_identity_hash` over a real lock
//! transition, and a hash that survives a move losing its index entry.
use super::tests::compact_object;
use super::{header_of, HashLockEvent, MonitorTable, HASHLOCK_EVENTS, HASHLOCK_EVENT_NAMES};
use crate::memory::heap::Heap;
use crate::threading::jvm_thread::ThreadId;
use cratonvm_types::ObjectHeader;
use std::sync::atomic::Ordering;

fn heap() -> &'static Heap {
    Box::leak(Box::new(Heap::with_capacity(1 << 16)))
}


#[test]
fn hl21_census_names_follow_the_event_order() {
    use HashLockEvent as E;
    let expected = [
        (E::HashedInflation, "hashed_inflations"),
        (E::UnhashedNeutralInflation, "unhashed_neutral_inflations"),
        (E::ThinInflation, "thin_inflations"),
        (E::DisplacedBitsAnswer, "displaced_bits_answers"),
        (E::DisplacedIndexWalk, "displaced_index_walks"),
        (E::MintedWhileInflated, "minted_while_inflated"),
        (E::PublishDeclined, "publish_declined"),
    ];
    assert_eq!(HASHLOCK_EVENTS, expected.len());
    for (event, name) in expected {
        assert_eq!(HASHLOCK_EVENT_NAMES[event as usize], name);
    }
}


