use super::{InlineFrameLevel, InlineFrameMap};

fn level(label: &str, bci: u32) -> InlineFrameLevel {
    InlineFrameLevel {
        label: std::sync::Arc::from(label),
        bci,
        class_id: 7,
        hidden: None,
    }
}

/// The last row for an id wins (a rewound emission re-issues it), and an
/// empty chain is dropped only AFTER that, so an empty final row cannot be
/// shadowed by the rewound non-empty one.
#[test]
fn last_row_per_id_wins_before_empty_chains_are_dropped() {
    let mut map = InlineFrameMap::default();
    map.set_safepoint_id_rows(vec![
        (5, vec![level("p/A.f:()V", 3)]),
        (2, vec![level("p/B.g:()V", 1)]),
        (5, Vec::new()),
        (9, vec![level("p/C.h:()V", 4), level("p/A.f:()V", 8)]),
    ]);
    let ids: Vec<u32> = map.by_safepoint_id.iter().map(|(id, _)| *id).collect();
    assert_eq!(ids, vec![2, 9], "id 5's final row is empty: no chain");
    assert_eq!(map.by_safepoint_id[1].1.len(), 2);
    assert!(!map.is_empty(), "id rows alone make the map non-empty");
}

/// No id rows: the table stays empty and the map is empty, so the walk
/// keeps its bci key exactly as before.
#[test]
fn no_rows_leave_the_map_empty() {
    let mut map = InlineFrameMap::default();
    map.set_safepoint_id_rows(vec![(1, Vec::new())]);
    assert!(map.by_safepoint_id.is_empty());
    assert!(map.is_empty());
}

/// JIT round 19 wave 2 (lane throws2, THROWS18-2): the shared accessors
/// answer what the slice accessors answer, an id outside every splice is
/// `Some(None)` (the slice form's `Some(&[])`), and one row's chain is ONE
/// allocation named by both the offset and the bci table.
#[test]
fn shared_accessors_answer_the_slice_accessors() {
    let mut map = InlineFrameMap::from_rows(
        vec![
            super::InlineFrameRow {
                native_offset: 10,
                safepoint_bci: 4,
                chain: vec![level("p/A.f:()V", 3)],
            },
            super::InlineFrameRow {
                native_offset: 20,
                safepoint_bci: 6,
                chain: vec![level("p/B.g:()V", 1), level("p/A.f:()V", 5)],
            },
        ],
        100,
    );
    map.set_safepoint_id_rows(vec![(2, vec![level("p/C.h:()V", 4)])]);
    for off in [0u32, 10, 15, 20, 99] {
        assert_eq!(
            map.chain_for_native_offset(off),
            map.chain_for_native_offset_shared(off).map(|c| &c[..]),
            "offset {off}"
        );
    }
    for bci in [0u32, 4, 5, 6] {
        assert_eq!(
            map.chain_for_safepoint_bci(bci),
            map.chain_for_safepoint_bci_shared(bci).map(|c| &c[..]),
            "bci {bci}"
        );
    }
    assert_eq!(map.chain_for_safepoint_id(2).map(<[_]>::len), Some(1));
    assert_eq!(map.chain_for_safepoint_id(3), Some(&[][..]));
    assert!(matches!(map.chain_for_safepoint_id_shared(3), Some(None)));
    assert!(std::sync::Arc::ptr_eq(
        map.chain_for_native_offset_shared(20).expect("row"),
        map.chain_for_safepoint_bci_shared(6).expect("row"),
    ));
}
