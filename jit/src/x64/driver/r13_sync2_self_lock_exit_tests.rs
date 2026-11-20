use super::self_lock_exits_are_closed;

#[test]
fn only_propagating_throws_are_closed_exits() {
    assert!(self_lock_exits_are_closed(&[], false, 0));
    assert!(self_lock_exits_are_closed(
        &[(10, 4, 3), (20, 9, 3)],
        false,
        0
    ));
    for reason in [0, 2, 6, 7, 8, 9, 10, 11, 12] {
        assert!(
            !self_lock_exits_are_closed(&[(10, 4, 3), (20, 9, reason)], false, 0),
            "reason {reason} resumes or re-runs the method"
        );
    }
    assert!(
        !self_lock_exits_are_closed(&[], true, 0),
        "a frameless trap re-runs"
    );
    assert!(
        !self_lock_exits_are_closed(&[], false, 1),
        "a local handler"
    );
}
