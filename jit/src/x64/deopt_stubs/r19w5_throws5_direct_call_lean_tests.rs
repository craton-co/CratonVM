//! JIT round 19 wave 5, lane throws5 (THROWS4-1,
//! `CRATONVM_JIT_DIRECT_CALL_SENTINEL_LEAN`): with the compile's switch
//! latched on, a statically bound site's callee-deopt check hands the
//! service the lean word (both the copying and the owned-arguments form);
//! a virtual site, a null info and the switch off keep the historical
//! words byte for byte.

use crate::x64::ARG_REGS;

fn info(invoke_kind: u8) -> *const crate::JitInvokeInfo {
    let info: &'static crate::JitInvokeInfo = Box::leak(Box::new(crate::JitInvokeInfo {
        class_name: "C",
        method_name: "f",
        descriptor: "(II)V",
        num_jit_args: 2,
        return_type: b'V',
        invoke_kind,
        declaring_class_id: 7,
        owner_class_id: 0,
    }));
    info
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && hay.windows(needle.len()).any(|w| w == needle)
}

fn imm64(word: i64) -> Vec<u8> {
    let mut want = super::super::emit::tests::test_compiler();
    let w0 = want.buf.pos();
    want.emit_mov_imm64(ARG_REGS[3], word);
    want.buf.as_slice()[w0..].to_vec()
}

fn imm32(word: i32) -> Vec<u8> {
    let mut want = super::super::emit::tests::test_compiler();
    let w0 = want.buf.pos();
    want.emit_mov_imm32_sx(ARG_REGS[3], word);
    want.buf.as_slice()[w0..].to_vec()
}

fn emit(lean: bool, info: *const crate::JitInvokeInfo, owned: bool) -> Vec<u8> {
    let mut c = super::super::emit::tests::test_compiler();
    c.helpers.service_callee_deopt = 0x1000; // baked, never executed
    c.heap_local_offset = 16; // gch (d1a): the service call loads the VM context
    c.direct_call_sentinel_lean = lean;
    let start = c.buf.pos();
    if owned {
        c.emit_inline_callee_deopt_check_owned_args(info);
    } else {
        c.emit_inline_callee_deopt_check(info, 2, 64);
    }
    c.buf.as_slice()[start..].to_vec()
}

#[test]
fn statically_bound_sites_carry_the_lean_word_only_when_switched_on() {
    if !matches!(crate::sp_ic_deopt_check_mode(), crate::SpIcDeoptCheck::On) {
        return;
    }
    let lean_count = imm64(crate::service_args_word(Some(2), true));
    let lean_owned = imm64(crate::service_args_word(None, true));
    let plain_count = imm32(2);
    let plain_owned = imm32(crate::SERVICE_ARGS_OWNED_BY_CALLEE);
    for kind in [1u8, 3] {
        let on = emit(true, info(kind), false);
        assert!(contains(&on, &lean_count), "kind {kind}: lean count word");
        // No "no plain word" check: the plain `MOV ECX, 2` bytes
        // (`B9 02 00 00 00`) are a substring of the lean word's own
        // `MOV RCX, imm64` encoding, so a byte search cannot tell them
        // apart. The off arm below proves the lean word is the switch's.
        let on = emit(true, info(kind), true);
        assert!(contains(&on, &lean_owned), "kind {kind}: lean owned word");
        let off = emit(false, info(kind), false);
        assert!(
            contains(&off, &plain_count),
            "kind {kind}: off keeps the count"
        );
        assert!(!contains(&off, &lean_count));
        let off = emit(false, info(kind), true);
        assert!(
            contains(&off, &plain_owned),
            "kind {kind}: off keeps the owned word"
        );
    }
    for kind in [0u8, 2] {
        let on = emit(true, info(kind), false);
        assert!(
            contains(&on, &plain_count),
            "kind {kind}: virtual keeps the count"
        );
        assert!(!contains(&on, &lean_count));
    }
    let on = emit(true, std::ptr::null(), false);
    assert!(contains(&on, &plain_count), "a null info keeps the count");
}
