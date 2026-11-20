use super::{LoaderNamespaceIndex, NamespaceRow};

const META: NamespaceRow = NamespaceRow {
    ns: 5,
    from_meta: true,
    parent_settled: false,
};
const STORE: NamespaceRow = NamespaceRow {
    ns: 9,
    from_meta: false,
    parent_settled: true,
};

/// `CRATONVM_LOADER_NAMESPACE_INDEX`: a row answers only for its own
/// address and only in the GC epoch it was filed in; the first filing of
/// a later epoch drops every older row (an address is a loader only
/// between two collections).
#[test]
fn rows_answer_only_in_the_epoch_they_were_filed_in() {
    let mut index = LoaderNamespaceIndex::default();
    assert_eq!(index.get(0, 0x1000), None);
    index.insert(4, 0x1000, META);
    index.insert(4, 0x2000, STORE);
    assert_eq!(index.get(4, 0x1000), Some(META));
    assert_eq!(index.get(4, 0x2000), Some(STORE));
    assert_eq!(
        index.get(4, 0x3000),
        None,
        "another address asks for itself"
    );
    assert_eq!(
        index.get(5, 0x1000),
        None,
        "a collection ran: the address may be anything now"
    );
    index.insert(
        4,
        0x1000,
        NamespaceRow {
            parent_settled: true,
            ..META
        },
    );
    assert_eq!(index.len(), 2, "a repeat is one row");
    assert!(index.get(4, 0x1000).is_some_and(|row| row.parent_settled));

    index.insert(5, 0x3000, STORE);
    assert_eq!(index.len(), 1, "a later epoch starts the table over");
    assert_eq!(index.get(5, 0x1000), None);
    assert_eq!(index.get(5, 0x3000), Some(STORE));
    assert_eq!(
        index.get(4, 0x3000),
        None,
        "an older epoch's ask never sees a newer row"
    );
}

/// The rows start over at the cap; the real-layout memo outlives epochs
/// and has its own cap.
#[test]
fn the_halves_clear_at_their_caps_and_the_layout_memo_outlives_epochs() {
    let mut index = LoaderNamespaceIndex::default();
    for addr in 0..LoaderNamespaceIndex::CAP {
        index.insert(1, (addr + 1) * 16, META);
    }
    assert_eq!(index.len(), LoaderNamespaceIndex::CAP);
    index.insert(1, 16, STORE);
    assert_eq!(
        index.len(),
        LoaderNamespaceIndex::CAP,
        "replacing a held row does not clear"
    );
    index.insert(1, 0x7fff_0000, STORE);
    assert_eq!(index.len(), 1, "a new row at the cap starts the table over");

    assert!(!index.is_real_layout(42));
    index.note_real_layout(42);
    index.insert(2, 0x10, META);
    assert!(
        index.is_real_layout(42),
        "a class's layout does not depend on the epoch"
    );
    for id in 100..(100 + LoaderNamespaceIndex::CAP as u32 - 1) {
        index.note_real_layout(id);
    }
    assert!(index.is_real_layout(42));
    index.note_real_layout(7);
    assert!(
        !index.is_real_layout(42),
        "a new class at the cap starts the memo over"
    );
    assert!(index.is_real_layout(7));
}
