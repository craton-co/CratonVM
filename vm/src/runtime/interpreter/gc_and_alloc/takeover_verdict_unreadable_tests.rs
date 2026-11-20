use super::takeover_verdict_unreadable;
use cratonvm_gc::gc_quiescence::{MoveLicence, TakeoverVerdict};

/// gc-common w3-g: an unread BLOCKED peer (no window found) used to
/// publish a `Move` licence.
#[test]
fn an_unread_blocked_peer_voids_the_licence() {
    let unreadable = takeover_verdict_unreadable(0, 0, 1);
    assert_eq!(unreadable, 1);
    let v = TakeoverVerdict {
        unreadable: unreadable as u32,
        ..TakeoverVerdict::NONE
    };
    assert_eq!(v.licence(true), MoveLicence::NonMoving);
}

/// With windows found, a refusal is already `pins_complete == false`;
/// it is not double-counted as an unread peer.
#[test]
fn refusals_beside_a_window_are_not_unread_peers() {
    assert_eq!(takeover_verdict_unreadable(0, 2, 1), 0);
    assert_eq!(takeover_verdict_unreadable(3, 0, 0), 3);
    assert_eq!(takeover_verdict_unreadable(3, 0, 2), 5);
}
