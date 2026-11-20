use super::*;

/// The in-place widenings are the encodings of `MOVSX r64, r8` /
/// `MOVSX r64, r16` / `MOVSXD r64, r32` / `MOVZX r32, r16` with the
/// register in both ModRM fields, REX.R/REX.B set together for R8-R15.
#[test]
fn widen_bytes_encode_the_register_in_place() {
    // RDI (7): no extension bits.
    assert_eq!(ffm_const_widen_bytes(7, FFM_CONST_S8), [0x48, 0x0F, 0xBE, 0xFF]);
    assert_eq!(ffm_const_widen_bytes(7, FFM_CONST_S16), [0x48, 0x0F, 0xBF, 0xFF]);
    assert_eq!(ffm_const_widen_bytes(7, FFM_CONST_S32), [0x48, 0x63, 0xFF]);
    assert_eq!(ffm_const_widen_bytes(7, FFM_CONST_U16), [0x0F, 0xB7, 0xFF]);
    // R9 (9): REX.R and REX.B.
    assert_eq!(ffm_const_widen_bytes(9, FFM_CONST_S8), [0x4D, 0x0F, 0xBE, 0xC9]);
    assert_eq!(ffm_const_widen_bytes(9, FFM_CONST_U16), [0x45, 0x0F, 0xB7, 0xC9]);
    // RAX (0), the return.
    assert_eq!(ffm_const_widen_bytes(RAX, FFM_CONST_S32), [0x48, 0x63, 0xC0]);
    assert!(ffm_const_widen_bytes(RAX, FFM_CONST_W64).is_empty());
    assert!(ffm_const_widen_bytes(RAX, FFM_CONST_VOID).is_empty());
}

/// The SysV integer argument order.
#[test]
fn sysv_argument_registers_are_rdi_rsi_rdx_rcx_r8_r9() {
    assert_eq!(FFM_CONST_SYSV_ARGS, [7, 6, 2, 1, 8, 9]);
}

/// A row names its descriptor only through a filled pointer and length.
#[test]
fn a_row_answers_the_descriptor_it_was_filed_for() {
    let desc: Box<str> = Box::from("(J)J");
    let mut row = FfmConstRow {
        fn_addr: 1,
        arity: 1,
        params: [FFM_CONST_W64, 0, 0, 0, 0, 0],
        ret: FFM_CONST_W64,
        flags: FFM_CONST_CRITICAL,
        guard_words: [0; 2],
        guard_values: [0; 2],
        call_helper: 0,
        desc_ptr: desc.as_ptr() as u64,
        desc_len: desc.len() as u64,
        seg_call_helper: 0,
        seg_param: FFM_CONST_NO_SEGMENT,
    };
    assert_eq!(row.descriptor(), Some("(J)J"));
    row.desc_ptr = 0;
    assert_eq!(row.descriptor(), None);
}

/// The row query rides the constant-`VarHandle` channel: its tag decodes.
#[test]
fn the_row_query_decodes_with_its_tag() {
    let which = vh_const_query(WELL_KNOWN_FFM_CONST_ROW_TAG, 555, 7);
    assert_eq!(
        vh_const_query_decode(which),
        Some((WELL_KNOWN_FFM_CONST_ROW_TAG, 555, 7))
    );
}
