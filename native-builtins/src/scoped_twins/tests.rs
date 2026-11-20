use super::*;

/// Every row is a session-checked `*Internal` instance body, none of the
/// rows `lib.rs` registers itself is here, and no name repeats.
#[test]
fn twins_are_session_checked_internal_bodies_not_registered_elsewhere() {
    assert_eq!(WINDOWED_TWINS.len(), 221);
    let mut seen = std::collections::HashSet::new();
    for &(method, descriptor, _) in WINDOWED_TWINS {
        assert!(method.ends_with("Internal"), "{method}");
        assert!(
            descriptor.starts_with("(Ljdk/internal/foreign/MemorySessionImpl;"),
            "{method}{descriptor}"
        );
        assert!(seen.insert(method), "duplicate {method}");
        for owned in [
            "copyMemoryInternal",
            "copySwapMemoryInternal",
            "setMemoryInternal",
            "vectorizedMismatchInternal",
            "getByteInternal",
            "putIntUnalignedInternal",
            "getLongInternal",
        ] {
            assert_ne!(method, owned, "registered by lib.rs");
        }
    }
}

/// Exactly the four plain float / double rows are left unregistered; every
/// other row is a windowed twin.
#[test]
fn only_the_plain_float_double_rows_stay_untwinned() {
    let mut skipped: Vec<&str> = WINDOWED_TWINS
        .iter()
        .map(|&(m, _, _)| m)
        .filter(|m| UNTWINNED_PLAIN_FP_ROWS.contains(m))
        .collect();
    skipped.sort_unstable();
    assert_eq!(
        skipped,
        [
            "getDoubleInternal",
            "getFloatInternal",
            "putDoubleInternal",
            "putFloatInternal"
        ]
    );
}
