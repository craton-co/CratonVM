//! Round 13 wave 5 (lane ffm3): the "downcall is inside a GC blocking
//! region" flag an upcall reads nests with the context guards: a nested
//! downcall starts clear, and each guard's drop restores its enclosing
//! downcall's value.
use super::*;
use crate::test_utils::mock_ctx;

#[test]
fn the_gc_safe_flag_nests_with_the_guards() {
    let mut outer_ctx = mock_ctx();
    let mut inner_ctx = mock_ctx();
    assert!(!active_downcall_gc_safe());
    {
        let outer = ActiveContextGuard::install(&mut outer_ctx);
        assert!(!active_downcall_gc_safe(), "a fresh guard starts clear");
        outer.set_gc_safe(true);
        assert!(active_downcall_gc_safe());
        {
            let _inner = ActiveContextGuard::install(&mut inner_ctx);
            assert!(!active_downcall_gc_safe(), "a nested downcall starts clear");
        }
        assert!(
            active_downcall_gc_safe(),
            "the outer downcall's flag is restored"
        );
        outer.set_gc_safe(false);
        assert!(!active_downcall_gc_safe());
    }
    assert!(!active_downcall_gc_safe());
    assert!(!has_active_context());
}
