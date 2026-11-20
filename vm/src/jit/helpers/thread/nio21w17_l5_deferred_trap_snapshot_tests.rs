use super::{
    defer_trap_snapshot, take_deferred_trap_snapshot, trap_frame_snapshot_enabled,
    DeferredTrapSnapshot, JIT_SIGNALS,
};

/// A deferred record is taken exactly once, and deferring clears a stale
/// snapshot the way the setter's own snapshot would have overwritten it.
#[test]
fn a_deferred_snapshot_is_recorded_once_and_clears_the_slot() {
    if !trap_frame_snapshot_enabled() {
        return;
    }
    JIT_SIGNALS.with(|s| *s.trap_frames.borrow_mut() = Some(Vec::new()));
    defer_trap_snapshot(DeferredTrapSnapshot::AtBci(7));
    assert!(JIT_SIGNALS.with(|s| s.trap_frames.borrow().is_none()));
    assert_eq!(
        take_deferred_trap_snapshot(),
        DeferredTrapSnapshot::AtBci(7)
    );
    assert_eq!(take_deferred_trap_snapshot(), DeferredTrapSnapshot::None);
    defer_trap_snapshot(DeferredTrapSnapshot::NpeKey(3));
    assert_eq!(
        take_deferred_trap_snapshot(),
        DeferredTrapSnapshot::NpeKey(3)
    );
}
