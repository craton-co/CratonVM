//! Round 13 wave 6 (lane sync3): no poll of a self-locking body may leave
//! for the interpreter, whichever of the three exit admissions it asks.

/// The body of `fn <name>` in this file, up to the next `fn `.
fn fn_body<'a>(src: &'a str, name: &str) -> &'a str {
    let at = src
        .find(&format!("fn {name}("))
        .unwrap_or_else(|| panic!("`fn {name}` not found"));
    let rest = &src[at + 3..];
    let end = [rest.find("\n    fn "), rest.find("\n    pub(super) fn ")]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or(rest.len());
    &rest[..end]
}

/// Interpreter round i2 wave 56 (lane L2): the two loop admissions refuse
/// it unless its exits hand the monitor over
/// (`self_lock_mode_exits_hand_over`, `CRATONVM_JIT_SELF_LOCK_MODE_EXIT_HANDOVER`);
/// the self-tail admission always does.
#[test]
fn every_mode_exit_admission_refuses_a_self_locking_body() {
    let src = include_str!("../safepoint.rs");
    // `branch_mode_exit_target` asks `branch_mode_exit_admits` (wave 48),
    // which the post-call exit claims ask too, and which asks
    // `branch_mode_exit_refusal` since interpreter round i2 wave 55 (this
    // test read the delegating `branch_mode_exit_admits` until wave 56,
    // and failed from wave 55 on).
    for name in [
        "mode_exit_target",
        "branch_mode_exit_refusal",
        "self_tail_mode_exit_target",
    ] {
        assert!(
            fn_body(src, name).contains("self.holds_self_lock_no_frame_names()"),
            "`{name}` must refuse a body holding its method monitor"
        );
    }
    for name in ["mode_exit_target", "branch_mode_exit_refusal"] {
        assert!(
            fn_body(src, name).contains(
                "self.holds_self_lock_no_frame_names() && !self.self_lock_mode_exits_hand_over()"
            ),
            "`{name}` admits a self-locking body only when its exits hand the monitor over"
        );
    }
    assert!(
        !fn_body(src, "self_tail_mode_exit_target").contains("self_lock_mode_exits_hand_over"),
        "a self-tail exit would resume a new activation: never handed over"
    );
}

#[test]
fn the_refusal_is_the_self_lock_home() {
    let mut c = super::super::emit::tests::test_compiler();
    assert!(!c.holds_self_lock_no_frame_names());
    c.self_lock_obj_off = 8;
    assert!(c.holds_self_lock_no_frame_names());
}
