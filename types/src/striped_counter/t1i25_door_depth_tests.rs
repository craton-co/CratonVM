use super::*;

/// A local `DoorDepth` stands in for [`DOOR_DEPTH`] (other tests of the
/// process may hold real door entries), so the folding is checked by
/// hand on the same arithmetic `DoorFoldedCounter` uses.
#[test]
fn t1i25_an_unengaged_door_depth_reads_zero_without_a_walk() {
    static D: DoorDepth = DoorDepth::new();
    assert!(D.is_zero());
    assert_eq!(D.get(), 0);
    assert!(!D.engaged.load(Ordering::Acquire));
    let t = D.enter();
    assert!(D.engaged.load(Ordering::Acquire), "engaged before it counts");
    assert!(!D.is_zero());
    assert_eq!(D.get(), 1);
    D.leave(t);
    assert!(D.is_zero(), "balanced");
    assert!(D.engaged.load(Ordering::Acquire), "engagement is sticky");
    D.leave(StripeToken::UNSET);
    D.leave(t);
    assert_eq!(D.get(), 0, "a stray leave saturates");
}

/// The door token carries its stripe across a thread exit, as
/// `StripedCounter::inc_token` does.
#[test]
fn t1i25_a_door_leave_after_thread_exit_lands_on_its_stripe() {
    static D: DoorDepth = DoorDepth::new();
    let t = std::thread::spawn(|| D.enter()).join().expect("worker");
    assert_eq!(D.get(), 1);
    D.leave(t);
    assert!(D.is_zero());
}

/// A folded counter's own writers never move `DOOR_DEPTH`, and its
/// readers include whatever `DOOR_DEPTH` holds.
#[test]
fn t1i25_a_folded_counter_reads_own_plus_door() {
    static F: DoorFoldedCounter = DoorFoldedCounter::new();
    let door_before = DOOR_DEPTH.get();
    let t = F.inc_token();
    assert!(!F.is_zero());
    assert!(F.get() >= 1);
    F.dec_token(t);
    assert_eq!(F.own.get(), 0);
    assert_eq!(DOOR_DEPTH.get(), door_before, "own writers leave the door counter alone");
    // A door entry is visible through the folded reader.
    let d = DOOR_DEPTH.enter();
    assert!(!F.is_zero(), "a door entry keeps every folded counter non-zero");
    assert!(F.get() >= 1);
    assert_eq!(F.own.get(), 0);
    DOOR_DEPTH.leave(d);
    F.reset();
}
