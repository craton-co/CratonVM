use super::*;

/// The g5jm shape: `new WeakReference<>(chainHead)` pops the `dup`ed
/// receiver (72) and the marker (80), both reclaimed (post 72, pre 88).
/// Both are listed; a primitive, a register-homed argument and a slot
/// below the reclaimed range (a deeper operand or a local) are not.
#[test]
fn gc_w9_gcd_only_reclaimed_reference_sources_are_listed() {
    let slots = [StackSlot::Frame(72), StackSlot::Frame(80)];
    assert_eq!(
        dead_reference_arg_sources(&slots, &[true, true], 72, 88),
        vec![72, 80]
    );
    // A primitive argument keeps its slot.
    assert_eq!(
        dead_reference_arg_sources(&slots, &[true, false], 72, 88),
        vec![72]
    );
    // A slot the cursor did not fall over (still owned) is never listed.
    assert_eq!(
        dead_reference_arg_sources(&slots, &[true, true], 80, 88),
        vec![80]
    );
    // Nothing at or above the pre-pop cursor (the staging buffer).
    assert_eq!(
        dead_reference_arg_sources(&[StackSlot::Frame(88)], &[true], 72, 88),
        Vec::<i32>::new()
    );
    // Register-homed arguments leave no frame copy.
    assert_eq!(
        dead_reference_arg_sources(&[StackSlot::CalleeSaved(12)], &[true], 0, 1024),
        Vec::<i32>::new()
    );
    // Two arguments sharing one slot (`invalidate_callee_saved`): once.
    assert_eq!(
        dead_reference_arg_sources(
            &[StackSlot::Frame(96), StackSlot::Frame(96)],
            &[true, true],
            96,
            104
        ),
        vec![96]
    );
}

/// The store is `MOV qword [rbp - off], 0` and touches no register.
#[test]
fn gc_w9_gcd_zero_frame_slot_encodes_a_qword_immediate_store() {
    let mut c = crate::x64::flag_and_header_contracts::bounds_check_test_compiler();
    let start = c.buf.pos();
    c.emit_zero_frame_slot(80);
    assert!(!c.failed);
    let end = c.buf.pos();
    let bytes: Vec<u8> = c.buf.as_slice()[start..end].to_vec();
    // REX.W C7 /0 [rbp-80] (disp8 0xB0) imm32 0.
    assert_eq!(bytes, vec![0x48, 0xC7, 0x45, 0xB0, 0, 0, 0, 0]);
    assert!(c.slot_mirror.is_none());
}
