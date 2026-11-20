use super::*;

/// gci i20/vh: a tid qualifies only when a blocked entry on it holds a
/// deferred chain record and none lacks one; the answer is sorted and
/// deduplicated.
#[test]
fn gci_i20vh_deferred_chain_peers_need_every_blocked_entry_on_the_tid_deferred() {
    assert!(deferred_chain_peer_tids(Vec::new()).is_empty());
    assert_eq!(deferred_chain_peer_tids(vec![(7, true)]), vec![7]);
    assert!(deferred_chain_peer_tids(vec![(7, false)]).is_empty());
    // A mounted virtual thread shares its carrier's tid: one entry without
    // a record keeps the tid out, in either order.
    assert!(deferred_chain_peer_tids(vec![(7, true), (7, false)]).is_empty());
    assert!(deferred_chain_peer_tids(vec![(7, false), (7, true)]).is_empty());
    assert_eq!(
        deferred_chain_peer_tids(vec![(9, true), (3, true), (9, true), (5, false)]),
        vec![3, 9]
    );
}

/// gci i20/vh: on a registry with no blocked entry the roster answer is
/// empty, and an entry that is not in a blocked region never qualifies.
#[test]
fn gci_i20vh_an_unblocked_registry_has_no_deferred_chain_peer() {
    let registry = ThreadRegistry::new();
    assert!(registry.blocked_os_tids_with_deferred_jit_chain().is_empty());
    let tid = registry.next_thread_id();
    registry.register(tid, "t", None);
    assert!(registry.blocked_os_tids_with_deferred_jit_chain().is_empty());
}

/// gci i24/jp: a row counts as holding a record only when the record is
/// one the collector's walk will resolve (the owner is not dropped); a
/// dropped owner's row keeps its tid OUT, like a row without a record,
/// even when another blocked row on the tid holds a record.
#[test]
fn gci_i24jp_a_dropped_owner_row_never_demotes_its_tid() {
    assert!(deferred_chain_row(false, true));
    assert!(!deferred_chain_row(true, true), "the resolve skips a dropped owner");
    assert!(!deferred_chain_row(false, false));
    assert!(!deferred_chain_row(true, false));
    let rows = vec![
        (7, deferred_chain_row(false, true)),
        (7, deferred_chain_row(true, true)),
        (9, deferred_chain_row(false, true)),
    ];
    assert_eq!(deferred_chain_peer_tids(rows), vec![9]);
}

/// gci i24/jp: the roster reads the row through `deferred_chain_row` with
/// the owner flag (a source pin; LF line endings assumed).
#[test]
fn gci_i24jp_the_roster_applies_the_owner_filter() {
    let src = concat!(include_str!("../thread_registry/mod.rs"), "\n", include_str!("../thread_registry/handshake_request.rs"), "\n", include_str!("../thread_registry/deposit.rs"), "\n", include_str!("../thread_registry/unrecorded_root_guard_deposits.rs"));
    let head = concat!("pub(crate) fn blocked_os_tids_with_deferred_jit_chain", "(&self)");
    let at = src.find(head).expect("the roster");
    let body = &src[at..at + src[at..].find("\n    }\n").expect("its end")];
    assert!(body.contains(concat!("deferred_chain_row", "(")), "{body}");
    assert!(body.contains(concat!("gc_block_state.owner_dropped", "()")), "{body}");
}
