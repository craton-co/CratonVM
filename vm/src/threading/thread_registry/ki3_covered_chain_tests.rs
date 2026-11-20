//! ki3 (`CRATONVM_XT_HELPER_WINDOW_COVERED_CHAIN_PIN_ONLY`): the roster of
//! blocked peers whose window's deposit covered their compiled chain.
use super::*;

/// A blocked entry qualifies when its deposit walked the chain itself
/// (`deposit_walked_jit_chain`) as when it deferred it; an unblocked
/// entry, a blocked one without either, and a dropped owner never do.
#[test]
fn ki3_a_blocked_peer_whose_deposit_walked_its_chain_is_covered() {
    let registry = ThreadRegistry::new();
    assert!(registry.blocked_os_tids_with_covered_jit_chain().is_empty());
    let tid = ThreadId(1);
    registry.register(tid, "parked", None);
    let state = {
        let threads = registry.threads.read();
        let entry = threads.get(&tid).unwrap();
        entry.os_tid.store(4242, Ordering::Release);
        entry.gc_block_state.clone()
    };
    state
        .deposit_walked_jit_chain
        .store(true, Ordering::Release);
    assert!(
        registry.blocked_os_tids_with_covered_jit_chain().is_empty(),
        "not blocked"
    );
    state.in_blocked_region.store(true, Ordering::Release);
    assert_eq!(
        registry.blocked_os_tids_with_covered_jit_chain(),
        vec![4242]
    );
    // The eager twin never makes the deferred roster (G1's demotion) answer.
    assert!(registry
        .blocked_os_tids_with_deferred_jit_chain()
        .is_empty());
    state
        .deposit_walked_jit_chain
        .store(false, Ordering::Release);
    assert!(registry.blocked_os_tids_with_covered_jit_chain().is_empty());
}

/// The roster reads the walked flag and the record through
/// `deferred_chain_row`, with the owner filter (a source pin; LF line
/// endings assumed).
#[test]
fn ki3_the_covered_roster_applies_the_owner_filter() {
    let src = concat!(include_str!("../thread_registry/mod.rs"), "\n", include_str!("../thread_registry/handshake_request.rs"), "\n", include_str!("../thread_registry/deposit.rs"), "\n", include_str!("../thread_registry/unrecorded_root_guard_deposits.rs"));
    let head = concat!(
        "pub(crate) fn blocked_os_tids_with_covered_jit_chain",
        "(&self)"
    );
    let at = src.find(head).expect("the roster");
    let body = &src[at..at + src[at..].find("\n    }\n").expect("its end")];
    assert!(body.contains(concat!("deferred_chain_row", "(")), "{body}");
    assert!(
        body.contains(concat!("gc_block_state.owner_dropped", "()")),
        "{body}"
    );
    assert!(
        body.contains(concat!("deposit_walked_jit_chain", "\n")),
        "{body}"
    );
    assert!(
        body.contains(concat!("deferred_jit_chain", ".lock()")),
        "{body}"
    );
}
