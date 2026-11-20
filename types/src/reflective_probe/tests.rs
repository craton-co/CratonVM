use super::*;

#[test]
fn inactive_by_default() {
    assert!(!active());
}

#[test]
fn guard_sets_and_clears() {
    assert!(!active());
    {
        let _g = ProbeGuard::new();
        assert!(active());
    }
    assert!(!active());
}

#[test]
fn reentrant_depth() {
    assert!(!active());
    let outer = ProbeGuard::new();
    assert!(active());
    {
        let _inner = ProbeGuard::new();
        assert!(active());
    }
    // Still active after inner drops — outer still holds the scope.
    assert!(active());
    drop(outer);
    assert!(!active());
}
