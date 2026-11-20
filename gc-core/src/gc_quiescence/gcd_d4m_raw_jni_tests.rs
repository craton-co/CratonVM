use super::*;

/// A raw scope counts in its own VM's ledger only, until it drops; an
/// indirect one counts nothing; a pause open does not clear the count.
#[test]
fn a_raw_jni_scope_counts_in_its_own_vm_until_it_drops() {
    let a = std::sync::Arc::new(PauseLedger::new());
    let b = std::sync::Arc::new(PauseLedger::new());
    assert!(!with_pause_ledger(&a, raw_jni_locals_open));
    {
        let _indirect = RawJniLocalsScope::enter(&a, false);
        assert!(
            !with_pause_ledger(&a, raw_jni_locals_open),
            "indirect locals do not count"
        );
    }
    let outer = RawJniLocalsScope::enter(&a, true);
    let inner = RawJniLocalsScope::enter(&a, true);
    assert!(with_pause_ledger(&a, raw_jni_locals_open));
    assert!(
        !with_pause_ledger(&b, raw_jni_locals_open),
        "another VM's natives do not count"
    );
    with_pause_ledger(&a, reset_peer_proven_jit_depth);
    assert!(
        with_pause_ledger(&a, raw_jni_locals_open),
        "a standing count survives a pause open"
    );
    drop(inner);
    assert!(
        with_pause_ledger(&a, raw_jni_locals_open),
        "nested: the outer call still holds"
    );
    drop(outer);
    assert!(!with_pause_ledger(&a, raw_jni_locals_open));
}

/// The unscoped pair (a foreign attachment) balances and never goes below
/// zero.
#[test]
fn the_unscoped_pair_balances_and_saturates() {
    let a = std::sync::Arc::new(PauseLedger::new());
    note_raw_jni_locals_open(&a);
    assert!(with_pause_ledger(&a, raw_jni_locals_open));
    note_raw_jni_locals_closed(&a);
    note_raw_jni_locals_closed(&a);
    assert!(!with_pause_ledger(&a, raw_jni_locals_open));
    note_raw_jni_locals_open(&a);
    assert!(
        with_pause_ledger(&a, raw_jni_locals_open),
        "a stray close did not go negative"
    );
    note_raw_jni_locals_closed(&a);
}

/// gch h2/yg: the census names both imbalances -- a standing count with
/// no native running, and the stray closes the saturation absorbs -- of
/// the ledger asked, never another VM's.
#[test]
fn gch_h2yg_the_raw_jni_census_counts_the_standing_count_and_stray_closes() {
    let a = std::sync::Arc::new(PauseLedger::new());
    let b = std::sync::Arc::new(PauseLedger::new());
    assert_eq!(raw_jni_locals_census(&a), (0, 0));
    note_raw_jni_locals_open(&a);
    let scope = RawJniLocalsScope::enter(&a, true);
    assert_eq!(raw_jni_locals_census(&a), (2, 0));
    drop(scope);
    note_raw_jni_locals_closed(&a);
    note_raw_jni_locals_closed(&a);
    note_raw_jni_locals_closed(&a);
    assert_eq!(raw_jni_locals_census(&a), (0, 2), "two stray closes");
    assert_eq!(raw_jni_locals_census(&b), (0, 0), "another VM's ledger");
}

/// gci i4/pn: a scope whose count a stray unscoped close already took
/// drops to zero and is counted as an underflow; it used to wrap the
/// count to `usize::MAX`, which read "raw locals open" for the VM's life.
#[test]
fn gci_i4pn_a_scope_drop_after_a_stray_close_saturates_and_is_counted() {
    let a = std::sync::Arc::new(PauseLedger::new());
    let scope = RawJniLocalsScope::enter(&a, true);
    note_raw_jni_locals_closed(&a);
    assert_eq!(raw_jni_locals_census(&a), (0, 0));
    drop(scope);
    assert_eq!(raw_jni_locals_census(&a), (0, 1), "saturated and counted, not wrapped");
    assert!(!with_pause_ledger(&a, raw_jni_locals_open));
    // A balanced scope still counts and releases exactly.
    let again = RawJniLocalsScope::enter(&a, true);
    assert_eq!(raw_jni_locals_census(&a), (1, 1));
    drop(again);
    assert_eq!(raw_jni_locals_census(&a), (0, 1));
}

/// gcd d10/t: the take-over's stack-incomplete count is a row of the
/// pause's OWN ledger -- another VM's notes do not reach it -- and a
/// take-over start (`reset_xt_cycle`) clears it.
#[test]
fn takeover_stack_incomplete_is_a_row_of_the_pause_ledger() {
    let a = std::sync::Arc::new(PauseLedger::new());
    let b = std::sync::Arc::new(PauseLedger::new());
    with_pause_ledger(&a, note_xt_takeover_stack_incomplete);
    with_pause_ledger(&a, note_xt_takeover_stack_incomplete);
    with_pause_ledger(&b, note_xt_takeover_stack_incomplete);
    assert_eq!(
        with_pause_ledger(&a, xt_takeover_stack_incomplete_this_pause),
        Some(2)
    );
    assert_eq!(
        with_pause_ledger(&b, xt_takeover_stack_incomplete_this_pause),
        Some(1)
    );
    with_pause_ledger(&a, reset_xt_cycle);
    assert_eq!(
        with_pause_ledger(&a, xt_takeover_stack_incomplete_this_pause),
        Some(0)
    );
    assert_eq!(
        with_pause_ledger(&b, xt_takeover_stack_incomplete_this_pause),
        Some(1)
    );
}
