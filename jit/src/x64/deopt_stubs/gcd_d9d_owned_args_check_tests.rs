//! gcd d9/d (2026-09-28): the callee-deopt check of a call site that kept
//! no argument copy hands the service a null buffer and the owned word,
//! which `jit_service_callee_deopt_body` decodes; the copying form still
//! hands it the plain count.

use crate::x64::ARG_REGS;

/// The bytes of `mov ARG_REGS[3], word`, preceded by `xor ARG_REGS[2]`
/// when `null_buffer`.
fn tail(null_buffer: bool, word: i32) -> Vec<u8> {
    let mut want = super::super::emit::tests::test_compiler();
    let w0 = want.buf.pos();
    if null_buffer {
        want.emit_xor_reg_self(ARG_REGS[2]);
    }
    want.emit_mov_imm32_sx(ARG_REGS[3], word);
    want.buf.as_slice()[w0..].to_vec()
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && hay.windows(needle.len()).any(|w| w == needle)
}

#[test]
fn the_owned_check_passes_no_buffer_and_the_owned_word() {
    if matches!(crate::sp_ic_deopt_check_mode(), crate::SpIcDeoptCheck::Off) {
        return;
    }
    let owned_word = crate::SERVICE_ARGS_OWNED_BY_CALLEE;

    let mut c = super::super::emit::tests::test_compiler();
    c.helpers.service_callee_deopt = 0x1000; // baked, never executed
    c.heap_local_offset = 16; // gch h3/fd (d1a): the service call loads the VM context
    let start = c.buf.pos();
    c.emit_inline_callee_deopt_check_owned_args(std::ptr::null());
    let owned = c.buf.as_slice()[start..].to_vec();
    assert!(
        contains(&owned, &tail(true, owned_word)),
        "the owned form must pass a null buffer and the owned word"
    );

    let mut c = super::super::emit::tests::test_compiler();
    c.helpers.service_callee_deopt = 0x1000;
    c.heap_local_offset = 16; // gch h3/fd (d1a): the service call loads the VM context
    let start = c.buf.pos();
    c.emit_inline_callee_deopt_check(std::ptr::null(), 2, 64);
    let copying = c.buf.as_slice()[start..].to_vec();
    assert!(
        contains(&copying, &tail(false, 2)),
        "the copying form passes its count"
    );
    assert!(
        !contains(&copying, &tail(false, owned_word)),
        "the copying form never passes the owned word"
    );
}
