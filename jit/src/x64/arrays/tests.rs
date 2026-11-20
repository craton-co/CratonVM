//! Round 9 wave 6 (simd6): the operand-home array load
//! (`emit_array_load_via_regs`) and the single-instruction `byte`/`short`
//! loads, by their bytes and by running them.

use super::*;

/// The element base displacement, through the same checked narrowing the
/// emitter uses.
fn data_disp() -> u8 {
    crate::x64::disp::disp8_const(ARRAY_DATA_OFFSET as i64) as u8
}

fn emitted(f: impl FnOnce(&mut Compiler) -> bool) -> (bool, Vec<u8>) {
    let mut c = super::super::emit::tests::test_compiler();
    let start = c.buf.pos();
    let ok = f(&mut c);
    (ok, c.buf.as_slice()[start..].to_vec())
}

#[test]
fn a_load_through_the_homes_encodes_every_register_and_element_kind() {
    let d = data_disp();
    let cases: [(u8, u8, u8, u8, Vec<u8>); 5] = [
        // movsxd r8, dword [r14 + r12*4 + d]: REX.WRXB, and R12 as INDEX
        (0x2e, R8, R14, R12, vec![0x4F, 0x63, 0x44, 0xA6, d]),
        // mov rax, qword [r13 + rbx*8 + d]: R13 base needs mod=01 (it has)
        (0x2f, RAX, R13, RBX, vec![0x49, 0x8B, 0x44, 0xDD, d]),
        // movsx r9, byte [rsi + r15*1 + d]
        (0x33, R9, RSI, R15, vec![0x4E, 0x0F, 0xBE, 0x4C, 0x3E, d]),
        // movzx eax, word [rax + rcx*2 + d]: no REX at all, and exactly
        // the bytes of the RAX/RCX form `emit_char_aload_regs` emits
        (0x34, RAX, RAX, RCX, vec![0x0F, 0xB7, 0x44, 0x48, d]),
        // movsx rdx, word [rbp + rdi*2 + d]
        (0x35, RDX, RBP, RDI, vec![0x48, 0x0F, 0xBF, 0x54, 0x7D, d]),
    ];
    for (op, dst, base, index, want) in cases {
        let (ok, bytes) = emitted(|c| c.emit_array_load_via_regs(op, dst, base, index));
        assert!(ok, "opcode {op:#x} must be encodable");
        assert_eq!(
            bytes, want,
            "opcode {op:#x} dst {dst} base {base} index {index}"
        );
    }
    let (_, char_form) = emitted(|c| {
        c.emit_char_aload_regs();
        true
    });
    assert_eq!(char_form, vec![0x0F, 0xB7, 0x44, 0x48, d]);

    // Refusals emit nothing: an opcode it does not lower (faload keeps
    // its XMM push), and RSP as the SIB index (which means "no index").
    for (op, index) in [(0x30u8, RCX), (0x32, RCX), (0x2e, RSP)] {
        let (ok, bytes) = emitted(|c| c.emit_array_load_via_regs(op, R8, RAX, index));
        assert!(
            !ok && bytes.is_empty(),
            "opcode {op:#x} index {index} must be refused"
        );
    }
}

#[test]
fn byte_and_short_loads_are_one_rex_w_instruction() {
    let d = data_disp();
    let (_, b) = emitted(|c| {
        c.emit_byte_aload_regs();
        true
    });
    assert_eq!(
        b,
        vec![0x48, 0x0F, 0xBE, 0x44, 0x08, d],
        "movsx rax, byte [rax+rcx+d]"
    );
    let (_, s) = emitted(|c| {
        c.emit_short_aload_regs();
        true
    });
    assert_eq!(
        s,
        vec![0x48, 0x0F, 0xBF, 0x44, 0x48, d],
        "movsx rax, word [rax+rcx*2+d]"
    );
}

/// Round 9 wave 8 (arr8): the `bastore` element-type test, by its bytes.
#[test]
fn the_bastore_boolean_mask_is_a_kind_byte_compare_and_an_and() {
    let tag = cratonvm_types::primitive_array_kind_tags_byte("[Z").expect("[Z has a kind byte");
    let kind =
        crate::x64::disp::disp8_const(cratonvm_types::KIND_TAGS_BYTE_OFFSET as i64) as u8; // Cast: checked disp8
    let (_, bytes) = emitted(|c| {
        c.emit_bastore_boolean_mask_regs();
        true
    });
    assert_eq!(
        bytes,
        vec![0x80, 0x78, kind, tag, 0x75, 0x03, 0x83, 0xE2, 0x01],
        "cmp byte [rax+kind], tag(\"[Z\") ; jne +3 ; and edx, 1"
    );
}

/// Round 9 wave 8 (arr8): the register-form array-load null check tests
/// the named register, is byte-identical to the RAX form for RAX, and
/// records one shared-stub entry whose patch site is its `JZ` rel32.
#[test]
fn the_null_check_through_a_home_tests_that_register() {
    // aload_0; iload_1; iaload
    let code = [0x2a, 0x1b, 0x2e];
    for (reg, test) in [
        (RAX, vec![0x48u8, 0x85, 0xC0]),
        (R14, vec![0x4D, 0x85, 0xF6]),
        (RBX, vec![0x48, 0x85, 0xDB]),
    ] {
        let mut c = super::super::emit::tests::test_compiler();
        let start = c.buf.pos();
        let stubs_before = c.null_check_store_stubs.len();
        c.emit_null_check_array_load_in(&code, 2, reg);
        let bytes = c.buf.as_slice()[start..].to_vec();
        let mut want = test.clone();
        want.extend_from_slice(&[0x0F, 0x84, 0, 0, 0, 0]);
        assert_eq!(bytes, want, "register {reg}");
        assert_eq!(c.null_check_store_stubs.len(), stubs_before + 1);
        let (action, patch, _, _) = c.null_check_store_stubs[stubs_before];
        assert_eq!(patch, start + test.len() + 2, "the patch is the JZ's rel32");
        assert_eq!(action, array_opcode_npe_action(&code, 2));
    }
}

/// Round 9 wave 8 (arr8): RUN the mask + store. A `boolean[]` keeps
/// `value & 1` (JVMS §6.5 `bastore`, what the interpreter's
/// `write_prim_element` does); a `byte[]` -- and a header carrying any
/// other tag -- keeps the low byte, exactly as before.
#[cfg(target_arch = "x86_64")]
#[test]
fn bastore_masks_a_boolean_array_and_truncates_a_byte_array() {
    let z = cratonvm_types::primitive_array_kind_tags_byte("[Z").expect("[Z has a kind byte");
    let b = cratonvm_types::primitive_array_kind_tags_byte("[B").expect("[B has a kind byte");
    let cases: [(u8, i32, i64); 9] = [
        (z, 0, 0),
        (z, 1, 1),
        (z, 2, 0),
        (z, 3, 1),
        (z, 0x102, 0),
        (z, -1, 1),
        (b, 2, 2),
        (b, 0x1FF, -1),
        (b, -128, -128),
    ];
    for (tag, value, want) in cases {
        let mut arr = array_of(&[vec![0x55u8], vec![0x55u8], vec![0x55u8]]);
        // SAFETY: the header byte lies inside the allocation, before the
        // element data.
        unsafe {
            (arr.as_mut_ptr() as *mut u8)
                .add(cratonvm_types::KIND_TAGS_BYTE_OFFSET)
                .write(tag);
        }
        let handle = arr.as_mut_ptr() as i64;
        let got = run_leaf(
            |c| {
                c.emit_mov_imm32_sx(RDX, value);
                c.emit_bastore_boolean_mask_regs();
                c.emit_byte_astore_regs();
                c.emit_byte_aload_regs();
            },
            handle,
            1,
        );
        assert_eq!(got, want, "tag {tag:#x} value {value:#x}");
    }
}

/// Emit `body` as a leaf taking `(array, index)` in RAX/RCX and
/// returning RAX, run it once, and return the result.
#[cfg(target_arch = "x86_64")]
fn run_leaf(body: impl FnOnce(&mut Compiler), array: i64, index: i64) -> i64 {
    let mut c = super::super::emit::tests::test_compiler();
    let entry = c.buf.pos();
    // Win64: ARG_REGS[0] is RCX, so read it before RCX is overwritten.
    c.emit_mov_r64_r64(RAX, ARG_REGS[0]);
    c.emit_mov_r64_r64(RCX, ARG_REGS[1]);
    body(&mut c);
    c.emit_ret();
    assert!(!c.buf.overflowed(), "the leaf must fit and encode");
    crate::platform::make_executable(c.buf.as_ptr() as *mut u8, c.buf.capacity())
        .expect("the test buffer must be flippable to RX");
    // SAFETY: `entry` is inside the live RX buffer `c` owns until this
    // function returns; the body reads one element of the caller's array
    // (in bounds) and touches only caller-saved registers.
    let f: extern "C" fn(i64, i64) -> i64 =
        unsafe { std::mem::transmute(c.buf.as_ptr().add(entry)) };
    f(array, index)
}

/// A synthetic array whose element `i` holds the little-endian bytes
/// `elems[i]`.
#[cfg(target_arch = "x86_64")]
fn array_of(elems: &[Vec<u8>]) -> Vec<u64> {
    let size = elems.first().map_or(1, Vec::len);
    let bytes = ARRAY_DATA_OFFSET + (elems.len() + 1) * size;
    let mut words = vec![0u64; bytes.div_ceil(8)];
    let base = words.as_mut_ptr() as *mut u8;
    // SAFETY: the allocation holds the header and every element.
    unsafe {
        std::ptr::write_unaligned(
            base.add(ARRAY_LENGTH_OFFSET) as *mut i32,
            elems.len() as i32,
        );
        for (i, e) in elems.iter().enumerate() {
            std::ptr::copy_nonoverlapping(
                e.as_ptr(),
                base.add(ARRAY_DATA_OFFSET + i * size),
                size,
            );
        }
    }
    words
}

/// Every element kind, read through extended base/index/destination
/// registers (R10/R11/R9: REX.B/X/R all set) and through the RAX/RCX
/// forms, extends to 64 bits exactly as Java's value does.
#[cfg(target_arch = "x86_64")]
#[test]
fn loads_through_the_homes_extend_like_java() {
    let ints: [i32; 4] = [i32::MIN, -1, 7, i32::MAX];
    let longs: [i64; 3] = [i64::MIN, -2, i64::MAX];
    let bytes: [i8; 4] = [i8::MIN, -1, 5, i8::MAX];
    let shorts: [i16; 4] = [i16::MIN, -1, 9, i16::MAX];
    let chars: [u16; 4] = [0, 0x7FFF, 0x8000, 0xFFFF];
    let kinds: Vec<(u8, Vec<Vec<u8>>, Vec<i64>)> = vec![
        (
            0x2e,
            ints.iter().map(|v| v.to_le_bytes().to_vec()).collect(),
            ints.iter().map(|&v| i64::from(v)).collect(),
        ),
        (
            0x2f,
            longs.iter().map(|v| v.to_le_bytes().to_vec()).collect(),
            longs.to_vec(),
        ),
        (
            0x33,
            bytes.iter().map(|v| v.to_le_bytes().to_vec()).collect(),
            bytes.iter().map(|&v| i64::from(v)).collect(),
        ),
        (
            0x34,
            chars.iter().map(|v| v.to_le_bytes().to_vec()).collect(),
            chars.iter().map(|&v| i64::from(v)).collect(),
        ),
        (
            0x35,
            shorts.iter().map(|v| v.to_le_bytes().to_vec()).collect(),
            shorts.iter().map(|&v| i64::from(v)).collect(),
        ),
    ];
    for (op, elems, want) in kinds {
        let mut arr = array_of(&elems);
        let handle = arr.as_mut_ptr() as i64;
        for (i, &w) in want.iter().enumerate() {
            let via = run_leaf(
                |c| {
                    c.emit_mov_r64_r64(R10, RAX);
                    c.emit_mov_r64_r64(R11, RCX);
                    assert!(c.emit_array_load_via_regs(op, R9, R10, R11));
                    c.emit_mov_r64_r64(RAX, R9);
                },
                handle,
                i as i64,
            );
            assert_eq!(via, w, "via homes: opcode {op:#x} element {i}");
            let general = run_leaf(
                |c| match op {
                    0x2e => c.emit_int_aload_regs(),
                    0x2f => c.emit_long_aload_regs(),
                    0x33 => c.emit_byte_aload_regs(),
                    0x34 => c.emit_char_aload_regs(),
                    _ => c.emit_short_aload_regs(),
                },
                handle,
                i as i64,
            );
            assert_eq!(general, w, "RAX/RCX form: opcode {op:#x} element {i}");
        }
    }
}

// -----------------------------------------------------------------------
// Round 9 wave 9 (arr9): bounds checks against the operand homes.
// -----------------------------------------------------------------------

/// Every (array, index) register pair encodes to the fixed 5-byte compare
/// plus `JAE rel32`, records its `JAE` displacement, and decodes back to
/// the same pair -- the decoder is what routes the site to its prologue,
/// so a pair it misread would reach the pad with the wrong RAX/RCX.
#[test]
fn a_home_bounds_check_encodes_and_decodes_every_register_pair() {
    if !jit_fused_bounds_load_enabled() {
        return; // the unfused configuration never takes this emitter
    }
    for array in 0u8..16 {
        for index in 0u8..16 {
            if array == RAX && index == RCX {
                continue;
            }
            let mut c = super::super::emit::tests::test_compiler();
            let start = c.buf.pos();
            c.emit_bounds_check_in(7, array, index);
            let bytes = c.buf.as_slice()[start..].to_vec();
            let rex = 0x40 | ((index >> 3) << 2) | (array >> 3);
            let want = vec![
                rex,
                0x3B,
                0x44 | ((index & 7) << 3),
                0x20 | (array & 7),
                BOUNDS_LEN_DISP,
                0x0F,
                0x83,
                0,
                0,
                0,
                0,
            ];
            assert_eq!(bytes, want, "array {array} index {index}");
            assert_eq!(c.bounds_check_stubs, vec![(start + 7, 7)]);
            assert_eq!(
                decode_home_bounds_check(c.buf.as_slice(), start + 7),
                Some((array, index)),
                "array {array} index {index}"
            );
        }
    }
}

/// The canonical RAX/RCX forms -- fused and unfused -- are never mistaken
/// for a home check, whatever precedes them; and RAX/RCX operands given
/// to the home emitter take the canonical bytes.
#[test]
fn the_canonical_bounds_checks_do_not_decode_as_home_checks() {
    for prefix in 0u8..=255 {
        let mut fused = vec![
            0x90,
            prefix,
            prefix,
            0x3B,
            0x48,
            BOUNDS_LEN_DISP,
            0x0F,
            0x83,
        ];
        fused.extend_from_slice(&[0; 4]);
        assert_eq!(
            decode_home_bounds_check(&fused, 8),
            None,
            "prefix {prefix:#x}"
        );
        let mut unfused = vec![
            prefix,
            0x44,
            0x8B,
            0x50,
            BOUNDS_LEN_DISP,
            0x41,
            0x3B,
            0xCA,
            0x0F,
            0x83,
        ];
        unfused.extend_from_slice(&[0; 4]);
        assert_eq!(
            decode_home_bounds_check(&unfused, 10),
            None,
            "prefix {prefix:#x}"
        );
    }
    if jit_fused_bounds_load_enabled() {
        let mut c = super::super::emit::tests::test_compiler();
        let start = c.buf.pos();
        c.emit_bounds_check_in(3, RAX, RCX);
        assert_eq!(
            c.buf.as_slice()[start..].to_vec(),
            vec![0x3B, 0x48, BOUNDS_LEN_DISP, 0x0F, 0x83, 0, 0, 0, 0]
        );
        assert_eq!(decode_home_bounds_check(c.buf.as_slice(), start + 5), None);
    }
    // Too close to the start of the buffer to hold the shape: no decode,
    // no panic.
    assert_eq!(decode_home_bounds_check(&[0x0F, 0x83, 0, 0, 0, 0], 2), None);
    assert_eq!(decode_home_bounds_check(&[], 0), None);
}

/// A check elided by `bounds_safe_pcs` emits nothing and records nothing.
#[test]
fn an_elided_home_bounds_check_emits_nothing() {
    let mut c = super::super::emit::tests::test_compiler();
    c.bounds_safe_pcs.insert(4);
    let start = c.buf.pos();
    c.emit_bounds_check_in(4, R10, R11);
    assert_eq!(c.buf.pos(), start);
    assert!(c.bounds_check_stubs.is_empty());
}

/// The prologue: the `JAE` is retargeted to `mov rax, <array> ; mov rcx,
/// <index> ; jmp`, the table entry now names that `jmp`, and a canonical
/// site next to it is left exactly as it was.
#[test]
fn the_home_prologue_moves_the_operands_and_takes_over_the_stub_entry() {
    if !jit_fused_bounds_load_enabled() {
        return;
    }
    let mut c = super::super::emit::tests::test_compiler();
    c.emit_bounds_check(1); // canonical, RAX/RCX
    let canonical = c.bounds_check_stubs[0];
    let site = c.buf.pos();
    c.emit_bounds_check_in(9, RBX, R13);
    let jae_patch = site + 7;
    let prologue = c.buf.pos();
    c.emit_bounds_check_home_prologues();
    let bytes = c.buf.as_slice()[prologue..].to_vec();
    // mov rax, rbx ; mov rcx, r13 ; jmp rel32
    assert_eq!(
        bytes,
        vec![0x48, 0x8B, 0xC3, 0x49, 0x8B, 0xCD, 0xE9, 0, 0, 0, 0]
    );
    let mut rel = [0u8; 4];
    rel.copy_from_slice(&c.buf.as_slice()[jae_patch..jae_patch + 4]);
    assert_eq!(
        i32::from_le_bytes(rel),
        (prologue - (jae_patch + 4)) as i32,
        "the JAE lands on the prologue"
    );
    assert_eq!(
        c.bounds_check_stubs,
        vec![canonical, (prologue + 7, 9)],
        "the home entry now patches the prologue's jmp, same bci"
    );
}

/// The parallel copy into RAX/RCX when an operand already sits in the
/// other's target register.
#[test]
fn the_home_prologue_orders_its_moves() {
    if !jit_fused_bounds_load_enabled() {
        return;
    }
    let cases: [(u8, u8, Vec<u8>); 3] = [
        // xchg rax, rcx
        (RCX, RAX, vec![0x48, 0x91]),
        // mov rcx, rax ; mov rax, r8
        (R8, RAX, vec![0x48, 0x8B, 0xC8, 0x49, 0x8B, 0xC0]),
        // mov rax, rcx ; mov rcx, r9
        (RCX, R9, vec![0x48, 0x8B, 0xC1, 0x49, 0x8B, 0xC9]),
    ];
    for (array, index, moves) in cases {
        let mut c = super::super::emit::tests::test_compiler();
        c.emit_bounds_check_in(2, array, index);
        let prologue = c.buf.pos();
        c.emit_bounds_check_home_prologues();
        let bytes = c.buf.as_slice()[prologue..].to_vec();
        let mut want = moves.clone();
        want.extend_from_slice(&[0xE9, 0, 0, 0, 0]);
        assert_eq!(bytes, want, "array {array} index {index}");
    }
}

/// RUN it: the operands are moved out of RAX/RCX into other registers,
/// RAX/RCX are cleared, and the home check either falls through (in
/// bounds: the leaf returns -1) or reaches a stand-in pad through the
/// prologue. The pad returns `RAX - RCX`, which is `array - index` only if
/// the prologue really put the array in RAX and the index in RCX.
#[cfg(target_arch = "x86_64")]
#[test]
fn a_home_bounds_check_reaches_its_pad_with_rax_and_rcx_restored() {
    if !jit_fused_bounds_load_enabled() {
        return;
    }
    let elems: Vec<Vec<u8>> = (0..5i32).map(|v| v.to_le_bytes().to_vec()).collect();
    let mut arr = array_of(&elems);
    let handle = arr.as_mut_ptr() as i64;
    // Caller-saved on both ABIs only.
    let pairs: [(u8, u8); 6] = [
        (R10, R11),
        (RDX, R8),
        (R9, RDX),
        (RCX, RAX),
        (R8, RAX),
        (RCX, R9),
    ];
    for (array, index) in pairs {
        for idx in [0i64, 4, 5, 6, 1000, -1, i64::from(i32::MIN)] {
            let got = run_leaf(
                |c| {
                    c.emit_mov_r64_r64(R10, RAX);
                    c.emit_mov_r64_r64(R11, RCX);
                    c.emit_xor_reg_self(RAX);
                    c.emit_xor_reg_self(RCX);
                    c.emit_mov_r64_r64(array, R10);
                    c.emit_mov_r64_r64(index, R11);
                    c.emit_bounds_check_in(0, array, index);
                    c.emit_mov_imm32_sx(RAX, -1); // in bounds
                    let to_end = c.emit_jmp_rel32_patch();
                    c.emit_bounds_check_home_prologues();
                    let (pad_patch, _) = c.bounds_check_stubs[0];
                    c.patch_rel32_to_here(pad_patch);
                    c.buf.emit(&[0x48, 0x29, 0xC8]); // SUB RAX, RCX
                    c.patch_rel32_to_here(to_end);
                },
                handle,
                idx,
            );
            let want = if (0..5).contains(&idx) {
                -1
            } else {
                handle.wrapping_sub(idx)
            };
            assert_eq!(got, want, "array {array} index {index} idx {idx}");
        }
    }
}
