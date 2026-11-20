use super::*;

fn local0(code: &[u8], cpc: usize) -> bool {
    let targets = crate::bytecode_analysis::branch_target_map(code, code.len());
    spliced_putfield_receiver_is_local0(code, code.len(), cpc, &targets)
}

#[test]
fn the_receiver_proof_admits_this_dot_f_equals_one_push() {
    // aload_0; iload_1; putfield #2; return
    assert!(local0(&[0x2a, 0x1b, 0xb5, 0, 2, 0xb1], 2));
    // aload 0; bipush 7; putfield
    assert!(local0(&[0x19, 0, 0x10, 7, 0xb5, 0, 2, 0xb1], 4));
    // aload_0; sipush; putfield
    assert!(local0(&[0x2a, 0x11, 1, 2, 0xb5, 0, 2, 0xb1], 4));
    // aload_0; aconst_null; putfield, after an earlier store.
    let code = [0x2a, 0x1b, 0xb5, 0, 2, 0x2a, 0x01, 0xb5, 0, 3, 0xb1];
    assert!(local0(&code, 2));
    assert!(local0(&code, 7));
}

#[test]
fn the_receiver_proof_refuses_everything_else() {
    // aload_1 is not local 0.
    assert!(!local0(&[0x2b, 0x1b, 0xb5, 0, 2, 0xb1], 2));
    // aload 1 (wide-less form) is not local 0.
    assert!(!local0(&[0x19, 1, 0x1b, 0xb5, 0, 2, 0xb1], 3));
    // Two-instruction value: aload_0; iload_1; iconst_1; iadd; putfield.
    assert!(!local0(&[0x2a, 0x1b, 0x04, 0x60, 0xb5, 0, 2, 0xb1], 4));
    // ldc is not admitted as the value push.
    assert!(!local0(&[0x2a, 0x12, 1, 0xb5, 0, 2, 0xb1], 3));
    // Not a putfield at cpc.
    assert!(!local0(&[0x2a, 0x1b, 0xb4, 0, 2, 0xb1], 2));
    // cpc not an instruction start.
    assert!(!local0(&[0x2a, 0x1b, 0xb5, 0, 2, 0xb1], 3));
    // The value push is a branch target:
    //   0: iload_1; 1: ifeq +5 (-> 6); 4: aload_0; 5: aload_2;
    //   6: iload_1 (target); 7: putfield
    // (an ill-typed join, but the textual rule must refuse it anyway).
    let code = [0x1b, 0x99, 0, 5, 0x2a, 0x2c, 0x1b, 0xb5, 0, 2, 0xb1];
    assert!(!local0(&code, 7));
    // The putfield is a branch target: 0: aload_0; 1: iload_1;
    // 2: goto +3 (-> 5); 5: putfield.
    let code = [0x2a, 0x1b, 0xa7, 0, 3, 0xb5, 0, 2, 0xb1];
    assert!(!local0(&code, 5));
    // A `wide` before the site refuses.
    let code = [0xc4, 0x15, 0, 1, 0x2a, 0x1b, 0xb5, 0, 2, 0xb1];
    assert!(!local0(&code, 6));
}


/// Word 3 of the fake object: where the helper stub records that it ran
/// (no process-global state: `jit/tests/process_global_statics_ratchet.rs`).
const HELPER_MARK_WORD: usize = 3;

// SAFETY: extern "C" test stub; `obj` is null or the test's live 32-byte
// fake object, whose word 3 is outside the stored field.
unsafe extern "C" fn stub_putfield_int(obj: i64, _idx: i64, val: i64) {
    if obj != 0 {
        *(obj as *mut u64).add(HELPER_MARK_WORD) = 1000 + val as u64;
    }
}

/// Emit `try_emit_inline_primitive_putfield` (compact `I` field at byte 8,
/// receiver in frame slot 8, value in slot 16) and run it once. Returns
/// whether the emitter applied.
fn run(trusted: bool, obj: i64, val: i64) -> bool {
    // A READ bounds table that contains nothing: the containment check
    // rejects every receiver, the trusted guard does not consult it.
    let empty_bounds = [0usize; 6];
    let mut c = crate::x64::flag_and_header_contracts::bounds_check_test_compiler();
    c.helpers.read_bounds_addr = empty_bounds.as_ptr() as usize; // Cast: table address
    c.helpers.putfield_int = stub_putfield_int as *const () as usize; // Cast: fn address
    let entry_off = c.buf.pos();
    c.buf.emit(&[0x55]); // push rbp
    c.buf.emit(&[0x48, 0x89, 0xE5]); // mov rbp, rsp
    c.buf.emit(&[0x48, 0x83, 0xEC, 0x40]); // sub rsp, 0x40
    c.emit_store_local(8, ARG_REGS[0]);
    c.emit_store_local(16, ARG_REGS[1]);
    let applied = c.try_emit_inline_primitive_putfield(
        Some((8, false)),
        StackSlot::Frame(8),
        StackSlot::Frame(16),
        0,
        b'I',
        trusted,
    );
    c.buf.emit(&[0xC9, 0xC3]); // leave; ret
    assert!(!c.buf.overflowed());
    if !applied {
        return false;
    }
    crate::platform::make_executable(c.buf.as_ptr() as *mut u8, c.buf.capacity())
        .expect("the test buffer must be flippable to RX");
    // SAFETY: `entry_off` is inside the same allocation.
    let entry = unsafe { c.buf.as_ptr().add(entry_off) };
    // SAFETY: the snippet builds its own frame, writes only through `obj`
    // (a live test buffer or null, which the guard sends to the stub), and
    // calls only the stub above. `c` outlives the call.
    let f: extern "C" fn(i64, i64) = unsafe { std::mem::transmute(entry) };
    f(obj, val);
    drop(c);
    true
}

/// The trusted guard stores a compact receiver inline where the
/// containment guard (against a table that contains nothing) sends the
/// same store to the helper; a null receiver still reaches the helper.
#[test]
fn a_trusted_receiver_stores_inline_without_the_containment_check() {
    if !cratonvm_types::compact_ref_fields_enabled() {
        return; // no compact layout in this environment
    }
    let mut obj = [0u64; 4];
    // SAFETY: byte offset inside the 32-byte buffer.
    unsafe {
        *(obj.as_mut_ptr() as *mut u8).add(cratonvm_types::GC_FLAGS_BYTE_OFFSET) =
            cratonvm_types::GC_FLAG_COMPACT;
    }
    let p = obj.as_mut_ptr() as i64;

    if !run(false, p, 41) {
        return; // the inline store is switched off in this environment
    }
    assert_eq!(obj[HELPER_MARK_WORD], 1041, "untrusted: the helper ran");
    assert_eq!(obj[1] as u32, 0, "untrusted: nothing stored inline");

    // The site bakes the global layout-replacement epoch; a concurrent test
    // that re-registers a class layout bumps it between emission and the
    // call, and the epoch guard then (correctly) sends the store to the
    // helper. Not this test's question: retry while the epoch moves.
    let mut stable = false;
    for _ in 0..8 {
        obj[HELPER_MARK_WORD] = 0;
        obj[1] = 0;
        let epoch = cratonvm_types::layout_replace_epoch();
        assert!(run(true, p, 42));
        if cratonvm_types::layout_replace_epoch() == epoch {
            stable = true;
            break;
        }
    }
    if !stable {
        return; // the epoch never held still across one run
    }
    assert_eq!(obj[HELPER_MARK_WORD], 0, "trusted: no helper");
    assert_eq!(obj[1] as u32, 42, "trusted: the cell holds the value");

    // A null receiver must reach the helper (which ignores it here); a
    // missing null test would fault on the header read instead.
    assert!(run(true, 0, 43));
}
