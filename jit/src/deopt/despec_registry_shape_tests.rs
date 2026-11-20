use super::*;

/// The two-level map must answer exactly what the flat set answered:
/// per-method, per-bci, idempotent, with the wildcard honoured and an
/// empty key matching nothing — across many methods, which is the case
/// the flat set's linear scan made slow.
#[test]
fn many_methods_do_not_see_each_others_entries() {
    let r = DespecRegistry::new();
    for m in 0..200u32 {
        let key = format!("C{m}.m:()V");
        r.insert(&key, m, speculation_id(m, DeoptReason::ClassCheck));
        // Idempotent.
        r.insert(&key, m, speculation_id(m, DeoptReason::ClassCheck));
    }
    r.insert("W.m:()V", 9, SPECULATION_ID_ANY);
    for m in 0..200u32 {
        let key = format!("C{m}.m:()V");
        assert!(r.contains(&key, m));
        assert!(!r.contains(&key, m + 1), "wrong bci must not match");
        assert!(r.contains_speculation(&key, m, speculation_id(m, DeoptReason::ClassCheck)));
        assert!(!r.contains_speculation(
            &key,
            m,
            speculation_id(m, DeoptReason::PendingException)
        ));
        assert_eq!(r.count_for(&key), 1, "insert must be idempotent");
    }
    // The wildcard suppresses every speculation at its bci, and only there.
    assert!(r.contains_speculation("W.m:()V", 9, 12345));
    assert!(!r.contains_speculation("W.m:()V", 10, 12345));
    // An unknown method and the empty legacy key match nothing.
    assert!(!r.contains("Nope.m:()V", 0));
    assert!(!r.contains("", 0));
    assert_eq!(r.count_for("Nope.m:()V"), 0);
}

/// L1W19-2 (lane qNull): the implicit-null fault count records its entry
/// on exactly the `limit`-th fault, answers `true` once, stays per
/// `(method, bci)`, is never read through a wildcard, and is forgotten
/// with its class.
#[test]
fn implicit_null_traps_record_once_at_the_limit_per_site() {
    let r = DespecRegistry::new();
    let key = "q/N.get:(Lq/H;)I";
    for _ in 0..3 {
        assert!(!r.note_implicit_null_trap(key, 4, 4));
    }
    assert!(!r.implicit_null_trapped(key, 4));
    assert!(
        r.note_implicit_null_trap(key, 4, 4),
        "the 4th fault records"
    );
    assert!(r.implicit_null_trapped(key, 4));
    assert!(
        !r.note_implicit_null_trap(key, 4, 4),
        "faults from the old body after the entry never ask a second recompile"
    );
    assert!(
        !r.implicit_null_trapped(key, 5),
        "another bci is its own site"
    );
    assert!(!r.implicit_null_trapped("q/N.other:()I", 4));
    // A wildcard at a bci is not this entry.
    r.insert(key, 9, SPECULATION_ID_ANY);
    assert!(!r.implicit_null_trapped(key, 9));
    // The id is apart from both NullCheck ids of the bci.
    for bci in [0u32, 1, 4, 9, 65_535] {
        let id = implicit_null_trap_speculation_id(bci);
        assert_ne!(id, SPECULATION_ID_ANY);
        assert_ne!(id, speculation_id(bci, DeoptReason::NullCheck));
        assert_ne!(
            id,
            expected_throw_speculation_id(bci, DeoptReason::NullCheck)
        );
    }
    // The empty legacy key never records.
    assert!(!r.note_implicit_null_trap("", 4, 1));
    // Unload: counts below the limit and the verdict both go.
    assert!(!r.note_implicit_null_trap(key, 7, 4));
    let mut gone = FxHashSet::default();
    gone.insert("q/N");
    r.forget_classes(&gone);
    assert!(!r.implicit_null_trapped(key, 4));
    for _ in 0..3 {
        assert!(
            !r.note_implicit_null_trap(key, 7, 4),
            "bci 7 restarted from zero"
        );
    }
    assert!(r.note_implicit_null_trap(key, 7, 4));
}
