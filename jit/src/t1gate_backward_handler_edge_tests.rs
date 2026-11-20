use super::JitLocalHandlerSite;

fn site(
    throw_bci: u32,
    candidates: Vec<(&'static str, u32)>,
    no_collect: bool,
) -> JitLocalHandlerSite {
    JitLocalHandlerSite {
        candidates,
        declaring_class_id: 0,
        throw_bci,
        cache: std::sync::atomic::AtomicU64::new(JitLocalHandlerSite::CACHE_EMPTY),
        implicit_cache: JitLocalHandlerSite::empty_implicit_cache(),
        no_collect,
    }
}

/// The `synchronized` handler shape (`any` at 17 covering its own
/// `monitorexit` at 19) and the hand-written one (handler below the
/// throwing bci) are backward; a handler after the throw is not.
#[test]
fn t1gate_a_handler_at_or_below_the_throw_bci_is_a_backward_edge() {
    let s = site(19, vec![("", 17)], false);
    assert!(s.declinable_backward_handler_edge(0), "handler 17 <= throw 19");
    let s = site(19, vec![("", 19)], false);
    assert!(
        s.declinable_backward_handler_edge(0),
        "handler on the throw bci itself"
    );
    let s = site(19, vec![("", 40)], false);
    assert!(
        !s.declinable_backward_handler_edge(0),
        "a forward handler never loops"
    );
}

/// The candidate asked about is the one the lookup chose, not any of them.
#[test]
fn t1gate_only_the_chosen_candidate_decides() {
    let s = site(30, vec![("java/io/IOException", 12), ("", 50)], false);
    assert!(s.declinable_backward_handler_edge(0));
    assert!(!s.declinable_backward_handler_edge(1));
}

/// `-1` ("no local handler"), `-2` (declined) and an index past the list
/// are not edges at all.
#[test]
fn t1gate_no_handler_answers_are_never_a_backward_edge() {
    let s = site(30, vec![("", 12)], false);
    assert!(!s.declinable_backward_handler_edge(-1));
    assert!(!s.declinable_backward_handler_edge(-2));
    assert!(!s.declinable_backward_handler_edge(i32::MIN));
    assert!(!s.declinable_backward_handler_edge(1));
    assert!(!s.declinable_backward_handler_edge(i32::MAX));
}

/// A spliced-scope site must answer (its `-1` skips the callee's catch) and
/// its `throw_bci` carries bit 31; an optimizing body's probe is polled per
/// block edge by the IR. Neither is ever declined.
#[test]
fn t1gate_spliced_and_optimizing_sites_are_never_declined() {
    let spliced = site(
        JitLocalHandlerSite::SPLICED_SCOPE_BIT | 19,
        vec![("", 17)],
        false,
    );
    assert!(spliced.must_answer());
    assert!(!spliced.declinable_backward_handler_edge(0));
    let ir = site(19, vec![("", 17)], true);
    assert!(!ir.declinable_backward_handler_edge(0));
}
