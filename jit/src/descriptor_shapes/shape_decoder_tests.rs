//! The two decoding rules a scan cannot get right by looking at one byte:
//! which `)` closes the parameter list, and which opcode a `wide` prefix
//! hides. Both are pure functions of a `&str` / `&[u8]`, so these need no
//! fixture at all — the argument for this file, applied to it.
use super::*;

/// inc 25: like `method_uses_category2`, but flags only **double**. It
/// admitted long-only methods to the IR path while double stayed on
/// single-pass, and has had no production caller since the FP tier moved
/// admission to `method_uses_fp` / `fp_in_body`. Round 11 (irlower) moved
/// it here, beside the decoder tests that are its only readers, rather
/// than leave dead code in the production module.
fn method_uses_double(code: &[u8], code_len: usize, descriptor: &str) -> bool {
    let params_have_double = DescriptorParamIter::new(descriptor).any(|tag| tag == b'D');
    // `return_type`, not a scan for the first `)` -- see `method_uses_category2`.
    if params_have_double || return_type(descriptor) == b'D' {
        return true;
    }
    let mut pc = 0;
    while pc < code_len {
        if is_double_opcode(executing_opcode(code, pc)) {
            return true;
        }
        pc += crate::bytecode_analysis::step(code, pc);
    }
    false
}

/// A class name may contain `)` (JVMS 4.2.2 bars only `. ; [ /`), so the
/// return tag is found by PARSING the parameters, not by scanning.
#[test]
fn a_parameter_class_name_containing_a_paren_does_not_hide_the_return_type() {
    // One argument of class `A)B`, returning `long`.
    assert_eq!(return_type("(LA)B;)J"), b'J');
    // ...and the same with a reference return, which is the case that
    // matters: read as `J` the call's result is a number, so nothing marks
    // it as a live oop.
    assert_eq!(return_type("(LA)B;)Ljava/lang/Object;"), b'L');
    assert_eq!(
        static_call_shape("(LA)B;)Ljava/lang/Object;"),
        Some((1, b'L'))
    );
}

/// Round 11 (irlower): the two tier gates read the return tag through
/// `return_type` too. They used to take the byte after the FIRST `)`, so a
/// `long`/`double`-returning method whose parameter class name holds a `)`
/// read as category-2-free.
#[test]
fn r11_a_paren_in_a_class_name_does_not_hide_a_wide_return_from_the_gates() {
    // iconst_0 ; pop ; return -- no category-2 opcode anywhere in the body.
    let code = [0x03, 0x57, 0xb1];
    assert!(method_uses_category2(&code, code.len(), "(LA)B;)J"));
    assert!(method_uses_category2(&code, code.len(), "(LA)B;)D"));
    assert!(method_uses_double(&code, code.len(), "(LA)B;)D"));
    // ...and the controls: an int return is still category-1.
    assert!(!method_uses_category2(&code, code.len(), "(LA)B;)I"));
    assert!(!method_uses_double(&code, code.len(), "(LA)B;)J"));
}

/// The ordinary shapes, unchanged.
#[test]
fn the_ordinary_descriptors_still_decode() {
    assert_eq!(return_type("()V"), b'V');
    assert_eq!(return_type("(II)I"), b'I');
    assert_eq!(return_type("(J)D"), b'D');
    assert_eq!(return_type("([[I)[Ljava/lang/String;"), b'[');
    assert_eq!(return_type("(Ljava/lang/String;)Z"), b'Z');
    // Malformed / truncated answers `V`, as the scan did.
    assert_eq!(return_type(""), b'V');
    assert_eq!(return_type("(I)"), b'V');
    assert_eq!(return_type("I"), b'V');
}

/// `wide lload` / `wide lstore` is the one way a method can hold a `long`
/// with no un-widened category-2 opcode anywhere, and `step` skips the
/// whole instruction — so the gate must look past the prefix.
#[test]
fn a_widened_lload_is_a_category2_opcode() {
    // wide lload 300 ; wide lstore 302 ; return
    let code = [0xc4, 0x16, 0x01, 0x2C, 0xc4, 0x37, 0x01, 0x2E, 0xb1];
    assert!(
        method_uses_category2(&code, code.len(), "()V"),
        "a `wide lload` body reads as category-2-free"
    );
    // wide dload 300 ; return  — the double-only gate sees it too.
    let dcode = [0xc4, 0x18, 0x01, 0x2C, 0xb1];
    assert!(method_uses_double(&dcode, dcode.len(), "()V"));
    assert!(fp_in_body(&dcode, dcode.len()));
    // wide fload 300 ; return  — float, for the FP gate.
    let fcode = [0xc4, 0x17, 0x01, 0x2C, 0xb1];
    assert!(fp_in_body(&fcode, fcode.len()));
    assert!(method_uses_fp(&fcode, fcode.len(), "()V"));
    assert!(!method_uses_double(&fcode, fcode.len(), "()V"));
}

/// A widened INT instruction is not category-2, and neither is `wide iinc`
/// (six bytes, the one widened form with a second operand).
#[test]
fn a_widened_int_instruction_is_not_category2() {
    // wide iload 300 ; wide iinc 300, 1 ; wide istore 300 ; return
    let code = [
        0xc4, 0x15, 0x01, 0x2C, 0xc4, 0x84, 0x01, 0x2C, 0x00, 0x01, 0xc4, 0x36, 0x01, 0x2C,
        0xb1,
    ];
    assert!(!method_uses_category2(&code, code.len(), "()V"));
    assert!(!fp_in_body(&code, code.len()));
}

/// Round 11 wave 2 (lane lower,
/// `r11-irlower-descriptor-walkers-disagree-on-malformed-tags`): every
/// parameter walker agrees with the ABI argument count
/// (`cratonvm_jit_api::count_param_slots`) and with each other about how
/// many parameters there are and which JVM slot each occupies — for
/// well-formed descriptors and for a stray byte, which is no parameter.
#[test]
fn every_parameter_walker_agrees_on_count_and_layout() {
    for d in [
        "()V",
        "(I)V",
        "(JI)J",
        "([[JLx;D)V",
        "(LA)B;)J",
        "(Q)V",
        "(ZQJ[Ljava/lang/String;)V",
    ] {
        let abi_args = cratonvm_jit_api::count_param_slots(d);
        let (slots, span) = compute_param_jvm_slots(d, true);
        assert_eq!(slots.len(), abi_args, "{d}: prologue slots vs ABI args");
        assert_eq!(
            ir_param_types(d, true).len(),
            abi_args,
            "{d}: IR param types"
        );
        assert_eq!(indy_arg_type_tags(d).len(), abi_args, "{d}: indy tags");
        assert_eq!(param_spec_slot_indices(d), slots, "{d}: spec slot indices");
        assert_eq!(count_param_slots_jvm_spec(d), span, "{d}: spec slot span");
        // The oop mask names exactly the reference parameters' slots.
        let mut want = 0u64;
        let refs = DescriptorParamIter::new(d).filter(|t| param_tag_width(*t).is_some());
        for (tag, &slot) in refs.zip(slots.iter()) {
            if matches!(tag, b'L' | b'[') {
                want |= 1u64 << slot;
            }
        }
        assert_eq!(compute_param_oop_mask(d, true), want, "{d}: oop mask");
        // The per-slot kind tags cover the span, and their non-zero
        // positions are the prologue's slots (interpreter round i1 wave 22).
        let tags = param_slot_kind_tags(d, true);
        assert_eq!(tags.len(), span, "{d}: kind tags span");
        let tagged: Vec<usize> = (0..tags.len()).filter(|&s| tags[s] != 0).collect();
        assert_eq!(tagged, slots, "{d}: kind tag slots");
        // The call-shape decoder is the one that REFUSES a stray byte.
        let malformed = d.contains('Q');
        assert_eq!(static_call_shape(d).is_none(), malformed, "{d}: call shape");
    }
    // An instance method adds `this` at slot 0, as a reference.
    assert_eq!(compute_param_jvm_slots("(JI)V", false), (vec![0, 1, 3], 4));
    assert_eq!(compute_param_oop_mask("(JLx;)V", false), 0b1001);
    assert_eq!(
        param_slot_kind_tags("(JZF[ID)V", false),
        vec![b'L', b'J', 0, b'I', b'F', b'L', b'D', 0]
    );
}

/// A trailing `wide` with nothing after it is malformed bytecode; it must
/// answer like the prefix itself rather than read past the body.
#[test]
fn a_truncated_wide_prefix_is_not_a_category2_opcode() {
    let code = [0xc4];
    assert!(!method_uses_category2(&code, code.len(), "()V"));
    assert!(!fp_in_body(&code, code.len()));
}
