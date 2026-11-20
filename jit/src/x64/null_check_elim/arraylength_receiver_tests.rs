use super::*;

/// `getfield #42; arraylength` must NOT read the getfield's trailing
/// index byte as `aload_0`.
///
/// This is the shape that made `emit_null_check_arraylength` unsound
/// until 2026-09-16: it decoded its receiver by reading `code[pc - 1]`
/// backwards and matching `0x2A..=0x2D`. `getfield #42` encodes as
/// `B4 00 2A`, so the byte before the `arraylength` is `0x2A` — the low
/// half of the constant-pool index, which reads as `aload_0`. In an
/// instance method the dataflow proves local 0 (`this`) non-null, so the
/// null check was elided on a value that was never checked: a SIGSEGV
/// where JVMS requires a `NullPointerException`.
///
/// The receiver of the `arraylength` here is the *result of the
/// getfield*, which lives on the operand stack and is not a local at
/// all, so the only correct answer is `None`.
#[test]
fn a_getfield_index_byte_is_not_an_aload_before_arraylength() {
    // 0: aload_0            (this)
    // 1: getfield #42       -> B4 00 2A   (the 0x2A trap)
    // 4: arraylength        -> BE
    // 5: ireturn
    let code = [0x2A, 0xB4, 0x00, 0x2A, 0xBE, 0xAC];
    let starts = bytecode_analysis::instruction_starts(&code, code.len());
    assert!(starts[0] && starts[1] && starts[4] && starts[5]);
    assert!(
        !starts[3],
        "byte 3 is the getfield's low index byte, not an instruction start"
    );
    assert_eq!(
        preceding_aload_nonnull_local_with_starts(&code, 4, &starts),
        None,
        "the instruction before `arraylength` is a getfield, not an aload — \
         answering `Some(0)` here elides a null check on a stack value"
    );
    // And the wrapper agrees, since that is what the emitter calls.
    assert_eq!(preceding_aload_nonnull_local(&code, 4), None);
}

/// The two-byte arm has the same trap one byte further back: any 3-byte
/// instruction whose FIRST operand byte is `0x19` used to read as
/// `aload <second operand byte>` — an arbitrary local.
#[test]
fn a_sipush_operand_is_not_an_aload_before_arraylength() {
    // 0: sipush 0x1907      -> 11 19 07   (0x19 is the `aload` opcode)
    // 3: arraylength        -> BE
    // 4: ireturn
    let code = [0x11, 0x19, 0x07, 0xBE, 0xAC];
    let starts = bytecode_analysis::instruction_starts(&code, code.len());
    assert!(!starts[1], "byte 1 is sipush's high operand byte");
    assert_eq!(
        preceding_aload_nonnull_local_with_starts(&code, 3, &starts),
        None,
        "the backward read would have named local 7 from an sipush operand"
    );
}

/// The positive control: a real `aload_0` before `arraylength` still
/// resolves, so the fix did not simply switch the elision off.
#[test]
fn a_real_aload_before_arraylength_still_resolves() {
    // 0: aload_0 ; 1: arraylength ; 2: ireturn
    let code = [0x2A, 0xBE, 0xAC];
    let starts = bytecode_analysis::instruction_starts(&code, code.len());
    assert_eq!(
        preceding_aload_nonnull_local_with_starts(&code, 1, &starts),
        Some(0)
    );
    // And the wide-index form.
    // 0: aload 7 ; 2: arraylength ; 3: ireturn
    let code2 = [0x19, 0x07, 0xBE, 0xAC];
    let starts2 = bytecode_analysis::instruction_starts(&code2, code2.len());
    assert_eq!(
        preceding_aload_nonnull_local_with_starts(&code2, 2, &starts2),
        Some(7)
    );
}
