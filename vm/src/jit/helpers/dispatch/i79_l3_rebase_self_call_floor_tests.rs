use super::*;

/// Only a door near the floor moves it; unset and suspended floors stay.
#[test]
fn only_a_door_near_the_floor_rebases() {
    let floor = 0x4000_0000usize;
    assert!(
        !floor_wants_rebase(floor + (8 << 20), floor),
        "shallow: free"
    );
    assert!(floor_wants_rebase(floor + (1 << 20), floor));
    assert!(floor_wants_rebase(floor + REFLECTIVE_REBASE_MARGIN, floor));
    assert!(!floor_wants_rebase(
        floor + REFLECTIVE_REBASE_MARGIN + 1,
        floor
    ));
    assert!(!floor_wants_rebase(16, 0), "suspended");
    assert!(!floor_wants_rebase(16, usize::MAX), "unset");
}

/// A rebase moves the floor `SELF_CALL_STACK_BUDGET` below the door, never
/// below the guard floor of the bounds it is given, and the restore gives
/// the caller's floor back; a shallow door changes and restores nothing.
#[test]
fn a_reflective_door_rebases_and_restores_the_floor() {
    let before = JIT_SELF_CALL_STACK_FLOOR.with(|f| f.get());
    let probe = 0u8;
    let sp = std::hint::black_box(&probe) as *const u8 as usize;
    // Bounds far below this frame: the guard floor does not bind.
    let bounds = Some((sp - (64 << 20), sp + (1 << 20)));
    // A cached floor just below this frame: the door is near it.
    let near = sp - (64 << 10);
    JIT_SELF_CALL_STACK_FLOOR.with(|f| f.set(near));
    let saved = rebase_self_call_stack_floor(bounds);
    assert_eq!(saved, near);
    let moved = JIT_SELF_CALL_STACK_FLOOR.with(|f| f.get());
    // JIT r23 wave 6: the budget is `SelfCallBudget::bytes` (16 MiB for
    // the default Java stack, 4 MiB with the switch at `0`).
    assert!(
        moved < near && moved >= sp - SelfCallBudget::default().bytes() - (1 << 20),
        "moved {moved:#x} near {near:#x}"
    );
    restore_self_call_stack_floor(saved);
    assert_eq!(JIT_SELF_CALL_STACK_FLOOR.with(|f| f.get()), near);
    // Far below this frame: shallow, untouched.
    let far = sp - (32 << 20);
    JIT_SELF_CALL_STACK_FLOOR.with(|f| f.set(far));
    let saved = rebase_self_call_stack_floor(bounds);
    assert_eq!(saved, FLOOR_NOT_REBASED);
    restore_self_call_stack_floor(saved);
    assert_eq!(JIT_SELF_CALL_STACK_FLOOR.with(|f| f.get()), far);
    JIT_SELF_CALL_STACK_FLOOR.with(|f| f.set(0));
    assert_eq!(rebase_self_call_stack_floor(bounds), FLOOR_NOT_REBASED);
    assert_eq!(
        JIT_SELF_CALL_STACK_FLOOR.with(|f| f.get()),
        0,
        "suspension kept"
    );
    JIT_SELF_CALL_STACK_FLOOR.with(|f| f.set(before));
}
