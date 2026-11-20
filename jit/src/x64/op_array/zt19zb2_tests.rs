//! zt t19/zb2: the single-pass `aaload` arm under an armed ZGC load
//! barrier, driven directly -- no process flag is flipped, so the other
//! tests of this binary keep their unarmed emission.

use super::*;

/// A fresh compiler whose operand stack holds an array and an index in
/// frame slots, with a reserved VM-context slot and `aaload_helper` wired.
fn compiler_with_aaload_operands(aaload_helper: usize) -> Compiler {
    let mut c = crate::x64::emit::tests::test_compiler();
    c.helpers.aaload = aaload_helper;
    c.heap_local_offset = 8;
    for off in [16, 24] {
        c.stack.push(StackSlot::Frame(off));
        c.stack_oop_marks.push(false);
    }
    c
}

/// An unwired helper table refuses the site: never the raw load the
/// armed barrier forbids, and nothing is popped.
#[test]
fn zt19zb2_an_unwired_aaload_helper_refuses_the_armed_site() {
    let mut c = compiler_with_aaload_operands(0);
    let step = c.walk_aaload_through_barrier_helper(&[0x32], 0, &[true]);
    assert!(matches!(step, WalkStep::Return(false)));
    assert_eq!(c.stack.len(), 2, "nothing was popped");
}

/// The armed arm calls `jit_aaload` and emits no raw element load; the
/// element replaces both operands and is marked as an oop.
#[test]
fn zt19zb2_the_armed_aaload_calls_the_helper_instead_of_the_inline_load() {
    const HELPER: usize = 0x7FFF_0000_1000;
    let mut c = compiler_with_aaload_operands(HELPER);
    let start = c.buf.pos();
    let calls_before = c.helper_call_patches.len();
    let step = c.walk_aaload_through_barrier_helper(&[0x32], 0, &[true]);
    let code = c.buf.as_slice()[start..].to_vec();
    assert!(
        matches!(step, WalkStep::Next(1)),
        "the walk continues after the aaload"
    );
    assert_eq!(c.stack.len(), 1, "the element replaces both operands");
    assert_eq!(c.stack_oop_marks.last(), Some(&true), "and is an oop");
    let addr = (HELPER as u64).to_le_bytes();
    let called = c.helper_call_patches.len() > calls_before
        || code.windows(addr.len()).any(|w| w == &addr[..]);
    assert!(called, "a call of the helper: {code:02x?}");
    // MOV RAX, [RAX + RCX*8 + disp8], the inline arm's load.
    let disp = u8::try_from(ARRAY_DATA_OFFSET).expect("a disp8 element base");
    let inline = [0x48u8, 0x8B, 0x44, 0xC8, disp];
    assert!(
        !code.windows(inline.len()).any(|w| w == &inline[..]),
        "no raw element load: {code:02x?}"
    );
}
