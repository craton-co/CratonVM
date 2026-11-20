use super::*;

/// `java/lang/ref/Reference.reachabilityFence` is constant-pool entry 42.
fn fence(cp: u16) -> bool {
    cp == 42
}

/// `ScopedMemoryAccess.getByteInternal` (JDK 25, javap): the handler is
/// the method's tail, with javac's self-covering row.
const GET_BYTE_INTERNAL: [u8; 34] = [
    0x2b, // 0: aload_1
    0xc6, 0x00, 0x07, // 1: ifnull 8
    0x2b, // 4: aload_1
    0xb6, 0x00, 0x1b, // 5: invokevirtual #27
    0xb2, 0x00, 0x20, // 8: getstatic #32
    0x2c, // 11: aload_2
    0x21, // 12: lload_3
    0xb6, 0x01, 0x4f, // 13: invokevirtual #335
    0x36, 0x05, // 16: istore 5
    0x2b, // 18: aload_1
    0xb8, 0x00, 0x2a, // 19: invokestatic #42
    0x15, 0x05, // 22: iload 5
    0xac, // 24: ireturn
    0x3a, 0x06, // 25: astore 6
    0x2b, // 27: aload_1
    0xb8, 0x00, 0x2a, // 28: invokestatic #42
    0x19, 0x06, // 31: aload 6
    0xbf, // 33: athrow
];
const GET_BYTE_ROWS: [(usize, usize, usize, u16); 2] = [(0, 18, 25, 0), (25, 27, 25, 0)];

/// `ScopedMemoryAccess.putIntUnalignedInternal`: `goto` over the handler.
const PUT_INT_INTERNAL: [u8; 37] = [
    0x2b, // 0: aload_1
    0xc6, 0x00, 0x07, // 1: ifnull 8
    0x2b, // 4: aload_1
    0xb6, 0x00, 0x1b, // 5: invokevirtual #27
    0xb2, 0x00, 0x20, // 8: getstatic #32
    0x2c, // 11: aload_2
    0x21, // 12: lload_3
    0x15, 0x05, // 13: iload 5
    0x15, 0x06, // 15: iload 6
    0xb6, 0x03, 0xd3, // 17: invokevirtual #979
    0x2b, // 20: aload_1
    0xb8, 0x00, 0x2a, // 21: invokestatic #42
    0xa7, 0x00, 0x0c, // 24: goto 36
    0x3a, 0x07, // 27: astore 7
    0x2b, // 29: aload_1
    0xb8, 0x00, 0x2a, // 30: invokestatic #42
    0x19, 0x07, // 33: aload 7
    0xbf, // 35: athrow
    0xb1, // 36: return
];
const PUT_INT_ROWS: [(usize, usize, usize, u16); 2] = [(0, 20, 27, 0), (27, 29, 27, 0)];

#[test]
fn a_tail_fence_handler_is_cut_off() {
    let s = strip_fence_finally_handler(
        &GET_BYTE_INTERNAL,
        GET_BYTE_INTERNAL.len(),
        &GET_BYTE_ROWS,
        &fence,
    )
    .expect("the javac finally shape is admitted");
    assert_eq!(s.code_len, 25);
    assert_eq!(s.code, GET_BYTE_INTERNAL[..25].to_vec());
    assert_eq!((s.handler_pc, s.handler_end), (25, 34));
    assert_eq!(s.fenced_locals, vec![1]);
}

#[test]
fn a_handler_behind_a_goto_becomes_nops() {
    let s = strip_fence_finally_handler(
        &PUT_INT_INTERNAL,
        PUT_INT_INTERNAL.len(),
        &PUT_INT_ROWS,
        &fence,
    )
    .expect("the void finally shape is admitted");
    assert_eq!(s.code_len, PUT_INT_INTERNAL.len());
    assert_eq!(&s.code[..24], &PUT_INT_INTERNAL[..24]);
    assert!(
        s.code[24..36].iter().all(|&b| b == 0x00),
        "goto + handler are nops"
    );
    assert_eq!(s.code[36], 0xb1);
    assert_eq!(s.fenced_locals, vec![1]);
}

#[test]
fn a_typed_catch_or_another_call_is_refused() {
    let typed = [(0, 18, 25, 7u16), (25, 27, 25, 0)];
    assert!(strip_fence_finally_handler(&GET_BYTE_INTERNAL, 34, &typed, &fence).is_none());
    assert!(
        strip_fence_finally_handler(&GET_BYTE_INTERNAL, 34, &GET_BYTE_ROWS, &|_: u16| false)
            .is_none(),
        "a handler calling anything but the fence runs code"
    );
    // A row reaching past the handler.
    let wide = [(0, 30, 25, 0u16)];
    assert!(strip_fence_finally_handler(&GET_BYTE_INTERNAL, 34, &wide, &fence).is_none());
}

/// The fenced local must be live wherever the handler could be entered:
/// a body that overwrites it inside the protected range is refused.
#[test]
fn a_fenced_local_dead_in_the_protected_range_is_refused() {
    let mut code = GET_BYTE_INTERNAL;
    // 16: istore 5 -> astore 1 (writes the fenced local; `5` -> `1`).
    code[16] = 0x3a;
    code[17] = 0x01;
    assert!(strip_fence_finally_handler(&code, 34, &GET_BYTE_ROWS, &fence).is_none());
}

/// `Buffer.session()` (JDK 25): the `checkcast` at 11 sits on the
/// fall-through of `ifnull` at 4.
const SESSION: [u8; 20] = [
    0x2a, // 0: aload_0
    0xb4, 0x00, 0x19, // 1: getfield #25
    0xc6, 0x00, 0x0e, // 4: ifnull 18
    0x2a, // 7: aload_0
    0xb4, 0x00, 0x19, // 8: getfield #25
    0xc0, 0x00, 0x78, // 11: checkcast #120
    0xb6, 0x00, 0x7a, // 14: invokevirtual #122
    0xb0, // 17: areturn
    0x01, // 18: aconst_null
    0xb0, // 19: areturn
];

#[test]
fn a_checkcast_behind_a_never_taken_fall_through_is_cold() {
    let heap_only = |pc: usize| (pc == 4).then_some((500u32, 0u32));
    assert!(pc_behind_unexecuted_branch_edge(
        &SESSION, 20, 11, &heap_only, 20
    ));
    let mixed = |pc: usize| (pc == 4).then_some((500u32, 3u32));
    assert!(!pc_behind_unexecuted_branch_edge(
        &SESSION, 20, 11, &mixed, 20
    ));
    let thin = |pc: usize| (pc == 4).then_some((5u32, 0u32));
    assert!(
        !pc_behind_unexecuted_branch_edge(&SESSION, 20, 11, &thin, 20),
        "too few samples prove nothing"
    );
    let none = |_: usize| -> Option<(u32, u32)> { None };
    assert!(!pc_behind_unexecuted_branch_edge(
        &SESSION, 20, 11, &none, 20
    ));
    let mut profile = crate::profile::MethodProfile::default();
    profile.branches.insert(
        4,
        crate::profile::BranchCounts {
            taken: 64,
            not_taken: 0,
        },
    );
    assert!(splice_typecheck_arm_unexecuted(
        &SESSION, 20, 20, 11, &profile
    ));
}

/// JDK 25 `Pattern.curly(Node,int)`: `new Curly` at 52 is reached by the
/// `qtype != GREEDY` edge of `if_acmpne` at 9 and by the
/// `!(prev instanceof CharProperty)` edge at 36, itself behind the
/// `!(prev instanceof BmpCharProperty)` edge of `ifeq` at 16.
const CURLY: [u8; 66] = [
    0x2a, 0xb6, 0x03, 0xf3, 0x4e, 0x2d, 0xb2, 0x03, 0xba, // 0..9
    0xa6, 0x00, 0x2b, // 9: if_acmpne 52
    0x2b, 0xc1, 0x03, 0x4a, // 12: aload_1; instanceof
    0x99, 0x00, 0x10, // 16: ifeq 32
    0xbb, 0x03, 0xf7, 0x59, 0x2b, 0xc0, 0x03, 0x4a, 0x1c, 0xb7, 0x03, 0xf9, 0xb0, // 19..31
    0x2b, 0xc1, 0x02, 0x2d, // 32: aload_1; instanceof
    0x99, 0x00, 0x10, // 36: ifeq 52
    0xbb, 0x03, 0xfc, 0x59, 0x2b, 0xc0, 0x02, 0x2d, 0x1c, 0xb7, 0x03, 0xfe, 0xb0, // 39..51
    0xbb, 0x03, 0xbd, // 52: new Curly
    0x59, 0x2b, 0x1c, 0x13, 0x00, 0x05, 0x2d, 0xb7, 0x04, 0x01, 0xb0, // 55..65
];

