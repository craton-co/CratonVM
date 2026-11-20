#[allow(unused_imports)]
use super::*;

#[test]
pub(in crate::x64::tests) fn p87_xmm_intermediate_chaining_float() {
    // float f(float a, float b) { return (a + b) * (a - b); }
    let code: Vec<u8> = vec![
        0x22, // fload_0
        0x23, // fload_1
        0x62, // fadd
        0x22, // fload_0
        0x23, // fload_1
        0x66, // fsub
        0x6a, // fmul
        0xae, // freturn
        0, 0,
    ];
    let code_len = 8;
    let compiled = compile(
        &code,
        code_len,
        2,
        2,
        false,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout — not needed for this test
    )
    .unwrap();

    let a = 5.0f32;
    let b = 3.0f32;
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe {
        compiled
            // Cast: float/double bit pattern to i64 (bit-preserving, VM all-GPR ABI)
            .try_call(&[a.to_bits() as i64, b.to_bits() as i64])
            .expect("test JIT call")
    }; // Cast: JIT ABI convention
    let expected = (a + b) * (a - b);
    assert_eq!(f32::from_bits(result as u32), expected); // Cast: JIT ABI convention
}

#[test]
pub(in crate::x64::tests) fn p87_xmm_multiple_intermediates() {
    // double f(double a, double b, double c) { return a*b + a*c + b*c; }
    // Tests multiple FP intermediates on the stack simultaneously
    // dload_0, dload_1, dmul,     -> (a*b)
    // dload_0, dload_2, dmul,     -> (a*c)
    // dadd,                       -> (a*b + a*c)
    // dload_1, dload_2, dmul,     -> (b*c)
    // dadd,                       -> (a*b + a*c + b*c)
    // dreturn
    let code: Vec<u8> = vec![
        0x26, // 0: dload_0 (a)
        0x27, // 1: dload_1 (b)
        0x6b, // 2: dmul
        0x26, // 3: dload_0 (a)
        0x28, // 4: dload_2 (c)
        0x6b, // 5: dmul
        0x63, // 6: dadd
        0x27, // 7: dload_1 (b)
        0x28, // 8: dload_2 (c)
        0x6b, // 9: dmul
        0x63, // 10: dadd
        0xaf, // 11: dreturn
        0, 0,
    ];
    let code_len = 12;
    let compiled = compile(
        &code,
        code_len,
        3,
        3,
        false,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout — not needed for this test
    )
    .unwrap();

    let a = 2.0f64;
    let b = 3.0f64;
    let c = 4.0f64;
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe {
        compiled
            .try_call(&[
                a.to_bits() as i64, // Cast: JIT ABI convention
                b.to_bits() as i64, // Cast: JIT ABI convention
                c.to_bits() as i64, // Cast: JIT ABI convention
            ])
            .expect("test JIT call")
    };
    let expected = a * b + a * c + b * c; // 6 + 8 + 12 = 26
    assert_eq!(f64::from_bits(result as u64), expected); // Cast: JIT ABI convention
}

#[test]
pub(in crate::x64::tests) fn p87_xmm_local_allocation_verified() {
    // Verify that the register allocator allocates XMM registers for FP locals
    // Simple: dload_0, dstore_1, dload_1, dreturn
    // Locals 0 and 1 are both FP — should both get XMM allocations
    let code: Vec<u8> = vec![0x26, 0x48, 0x27, 0xaf, 0, 0];
    let code_len = 4;
    let loops = detect_loops(&code, code_len);
    let alloc = crate::regalloc::allocate_registers(&code, code_len, 2, 1, &loops);
    // Both locals should have XMM assignments (not GPR)
    assert!(
        alloc.assignments[0].is_none(),
        "FP local 0 should not have GPR"
    );
    assert!(
        alloc.assignments[1].is_none(),
        "FP local 1 should not have GPR"
    );
    // At least one should get an XMM
    let xmm_count = alloc
        .xmm_assignments
        .iter()
        .filter(|a: &&Option<u8>| a.is_some())
        .count();
    assert!(
        xmm_count > 0,
        "At least one FP local should get XMM register"
    );
}

/// **The XMM-homed-local emitter, exercised on a System V host.**
///
/// The test above proves the ALLOCATOR hands a float/double local an XMM
/// register. Nothing proved the EMITTER then does anything with it, on the
/// platform that runs the suite: `Compiler::new` gates the allocation behind
/// `xmm_local_homes_enabled`, which is Win64-only for the ABI reason stated
/// there, so on Linux `xmm_for_local` answered `None` for every local and the
/// fifteen sites that read it never ran.
///
/// This is the prologue's three of those, forced on and off over one fixture:
///
///   * a register-passed float/double parameter is moved GPR -> XMM, rather
///     than stored to its frame home;
///   * an XMM-mapped local past the parameter span is zeroed with `PXOR`,
///     rather than by storing a zeroed GPR to its frame slot;
///   * neither instruction is emitted when the homes are off.
///
/// **Bytes only, never executed.** Forcing the homes on does not change the
/// ABI: code compiled this way would lose a local across any call on System V,
/// which is the whole reason the default is what it is. This test inspects the
/// buffer and calls nothing in it.
#[test]
pub(in crate::x64::tests) fn an_xmm_homed_local_is_loaded_and_zeroed_by_the_prologue() {
    // MOVQ xmm8, rax — 66 REX.W+R 0F 6E /r, ModRM 11 000 000.
    const MOVQ_XMM8_RAX: [u8; 5] = [0x66, 0x4C, 0x0F, 0x6E, 0xC0];
    // PXOR xmm9, xmm9 — 66 REX.R+B 0F EF /r, ModRM 11 001 001.
    const PXOR_XMM9: [u8; 5] = [0x66, 0x45, 0x0F, 0xEF, 0xC9];

    // Local 0 is the incoming parameter and is XMM-homed; local 2 is an
    // XMM-homed local past the parameter span, so the prologue must zero it.
    // XMM8/XMM9 rather than XMM2..7 because `regalloc` allocates single-pass
    // local homes out of the high half — the same range the test above asserts.
    let prologue_bytes = |homes: bool| -> Vec<u8> {
        let _force = if homes {
            XmmLocalHomesForce::on()
        } else {
            XmmLocalHomesForce::off()
        };
        let alloc_result = crate::regalloc::RegAllocResult {
            assignments: vec![None; 4],
            xmm_assignments: vec![Some(8), None, Some(9), None],
            used_callee_saved: Vec::new(),
            used_xmm_regs: vec![8, 9],
            block_live_in: Vec::new(),
        };
        let mut compiler = Compiler::new(
            "xmm-home-prologue-test".to_string(),
            ExecutableBuffer::new(4096).expect("test executable buffer"),
            4,
            1,
            8,
            false,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            alloc_result,
            false,
            test_helpers(),
            0,
            false,
            false,
            false,
            false,
            false,
            Vec::new(),
        );
        compiler.emit_prologue();
        assert!(
            !compiler.buf.overflowed(),
            "the test buffer must hold the prologue",
        );
        compiler.buf.as_slice().to_vec()
    };

    let win = prologue_bytes(true);
    let sysv = prologue_bytes(false);
    let has = |b: &[u8], pat: &[u8; 5]| b.windows(5).any(|w| w == pat);

    assert!(
        has(&win, &MOVQ_XMM8_RAX),
        "with XMM homes on, the parameter must be moved into XMM8 — this is \
         `frames::emit_prologue`'s register-argument arm, and it is the arm \
         that never runs on System V",
    );
    assert!(
        has(&win, &PXOR_XMM9),
        "with XMM homes on, an XMM-mapped local past the parameter span must \
         be zeroed in its REGISTER; zeroing only its frame word would leave \
         the register holding the previous frame's value",
    );
    assert!(
        !has(&sysv, &MOVQ_XMM8_RAX) && !has(&sysv, &PXOR_XMM9),
        "with XMM homes off, every float/double local uses its frame home and \
         no XMM instruction belongs in the prologue at all",
    );
    assert_ne!(
        win, sysv,
        "the two arms emitted identical bytes, so the gate is being ignored \
         and both assertions above are about the same compile",
    );
}

/// gcd d2/f: under `CRATONVM_JIT_SP_ZERO_SPILL_BAND=1` the single-pass
/// prologue stores zero into every operand-spill slot, `[base_spill_offset,
/// spill_limit_offset)`, so a word the activation has not written reads null
/// rather than a returned frame's leftover reference; without the flag the
/// prologue is byte-identical to the build before it. Bytes only, never
/// executed.
/// **The ten bytecode arms, exercised on a System V host.**
///
/// The prologue test above covers `frames`' three sites. These are the other
/// ten: every `dload`/`dstore`/`fload`/`fstore` and accumulator arm in
/// `bytecode_walk` that reads `xmm_for_local`, reached the way production
/// reaches them -- a whole compile through `driver::compile`, with the
/// allocator choosing the homes rather than a fixture asserting them.
///
/// The assertion is an A/B rather than a byte pattern on purpose. Which
/// instruction each arm picks is the emitter's business and changes with it;
/// that the two arms of the gate produce DIFFERENT code, and that the forced-on
/// one keeps the allocator's XMM registers out of the frame, is the property
/// this is here to hold.
///
/// **Bytes only, never executed** -- see `XmmLocalHomesForce`.
#[test]
pub(in crate::x64::tests) fn the_bytecode_arms_honour_an_xmm_home_the_allocator_gave() {
    // static double f(int n) { double a = n; return a + a; }
    //   0: iload_0  1: i2d  2: dstore_1  3: dload_1  4: dload_1  5: dadd
    //   6: dreturn
    // One store and two loads of local 1, which is what earns it a register.
    let code = [0x1a, 0x87, 0x48, 0x27, 0x27, 0x63, 0xaf, 0, 0];
    let code_len = 7;

    // Precondition: the ALLOCATOR gives local 1 an XMM home on this fixture.
    // Without it the two arms below are the same compile and the A/B proves
    // nothing -- the failure mode this fixture is one edit away from.
    let loops = detect_loops(&code, code_len);
    let alloc = crate::regalloc::allocate_registers(&code, code_len, 3, 1, &loops);
    assert!(
        alloc.xmm_assignments.iter().any(|a| a.is_some()),
        "the allocator gave this fixture no XMM home, so the gate below has \
         nothing to gate: repair the fixture rather than believe the A/B",
    );

    let compiled = |homes: bool| -> Vec<u8> {
        let _force = if homes {
            XmmLocalHomesForce::on()
        } else {
            XmmLocalHomesForce::off()
        };
        let cm = crate::x64::driver::compile(
            &code,
            code_len,
            1,
            3,
            false,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            std::collections::HashMap::new(),
            std::collections::HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            std::collections::HashMap::new(),
            None,
        )
        .expect("the fixture compiles through the single-pass backend");
        cm.code_bytes().to_vec()
    };

    let win = compiled(true);
    let sysv = compiled(false);

    assert_ne!(
        win, sysv,
        "the two arms of `xmm_local_homes_enabled` emitted identical code for \
         a method whose double local the allocator put in an XMM register — \
         the gate is being ignored, and every `xmm_for_local` arm in \
         `bytecode_walk` is unreachable on this host",
    );
}

// --- 87.2: FP Loop Optimization ---

#[test]
pub(in crate::x64::tests) fn p87_fp_loop_hoist_detection() {
    // Loop: dload_0; dload 4; dadd; dstore_0; iinc 2 1; iload 2; iconst_5;
    //       if_icmplt -11; dload_0; dreturn
    //
    // The invariant operand is `dload 4`, NOT the `dload_1` this test used to
    // carry. `dstore_0` stores a `double`, which occupies slots 0 AND 1, so
    // `dload_1` reads a slot the loop body writes and hoisting it out is
    // unsound. The old fixture asserted that hoist was legal — it was written
    // from what `find_modified_locals` did, which recorded only the lower slot
    // of a category-2 store until 2026-09-05. `dload 4` (slots 4, 5) is
    // disjoint from the accumulator (0, 1) and from the counter (2), so the
    // property the test was written for is preserved and now stated on a
    // fixture where it holds.
    let code: Vec<u8> = vec![
        0x26, // 0:  dload_0  (modified — acc, slots 0+1)
        0x18, 0x04, // 1:  dload 4 (invariant — slots 4+5)
        0x63, // 3:  dadd
        0x47, // 4:  dstore_0
        0x84, 0x02, 0x01, // 5: iinc 2, 1
        0x15, 0x02, // 8:  iload 2
        0x08, // 10: iconst_5
        0xa1, 0xFF, 0xF5, // 11: if_icmplt -11 → target=0
        0x26, // 14: dload_0
        0xaf, // 15: dreturn
        0, 0,
    ];
    let code_len = 16;
    let loops = detect_loops(&code, code_len);
    assert!(!loops.is_empty(), "Should detect a loop");

    let hoists = find_fp_loop_hoists(&code, code_len, &loops);
    // dload 4 at PC=1 should be hoistable (locals 4 and 5 are invariant)
    let hoisted_pcs: Vec<usize> = hoists.iter().map(|h| h.load_pc).collect();
    assert!(
        hoisted_pcs.contains(&1),
        "dload 4 at PC=1 should be hoistable"
    );
    // dload_0 at PC=0 should NOT be hoistable (local 0 is modified by dstore_0)
    assert!(!hoisted_pcs.contains(&0), "dload_0 should not be hoistable");

    // And the shape the old fixture asserted was legal must be refused: swap
    // the invariant operand back to `dload_1`, whose slot 1 is the dead high
    // half of the `dstore_0` in the body. Two bytes shorter, so the branch
    // offset moves with it.
    let overlapping: Vec<u8> = vec![
        0x26, // 0:  dload_0
        0x27, // 1:  dload_1  <- reads slots 1+2, and the body writes both
        0x63, // 2:  dadd
        0x47, // 3:  dstore_0 (writes slots 0+1)
        0x84, 0x02, 0x01, // 4: iinc 2, 1  (writes slot 2)
        0x15, 0x02, // 7:  iload 2
        0x08, // 9:  iconst_5
        0xa1, 0xFF, 0xF6, // 10: if_icmplt -10 → target=0
        0x26, // 13: dload_0
        0xaf, // 14: dreturn
        0, 0,
    ];
    let overlapping_loops = detect_loops(&overlapping, 15);
    assert!(
        find_fp_loop_hoists(&overlapping, 15, &overlapping_loops)
            .iter()
            .all(|h| h.load_pc != 1),
        "`dload_1` overlaps the `dstore_0` in the body and must not hoist"
    );
}

#[test]
pub(in crate::x64::tests) fn p87_fp_loop_hoist_empty_for_no_loops() {
    // No loops → no hoists
    let code: Vec<u8> = vec![0x26, 0xaf, 0, 0];
    let loops = detect_loops(&code, 2);
    let hoists = find_fp_loop_hoists(&code, 2, &loops);
    assert!(hoists.is_empty());
}

#[test]
pub(in crate::x64::tests) fn p87_strength_reduction_detects_dmul_by_2() {
    // Loop with ldc2_w (2.0), dmul → should be detected
    // Simulated: ldc2_w at PC=5, dmul at PC=8
    let code: Vec<u8> = vec![
        0x15, 0x03, // 0: iload 3 (iv)
        0x08, // 2: iconst_5 (bound)
        0xa2, 0x00, 0x0E, // 3: if_icmpge +14 → target=17
        0x26, // 6: dload_0 (value)
        0x14, 0x00, 0x01, // 7: ldc2_w #1
        0x6b, // 10: dmul
        0x47, // 11: dstore_0
        0x84, 0x03, 0x01, // 12: iinc 3, 1
        0xa7, 0xFF, 0xF1, // 15: goto -15 → target=0
        0x26, // 18: dload_0
        0xaf, // 19: dreturn
        0, 0,
    ];
    let code_len = 20;
    let loops = detect_loops(&code, code_len);
    assert!(!loops.is_empty());

    // ldc2_w at PC=7, value = 2.0
    let ldc2w_info = vec![(7usize, 2.0f64.to_bits() as i64)]; // Cast: JIT ABI convention
    let pcs = find_fp_strength_reductions(&code, code_len, &loops, &ldc2w_info);
    // dmul at PC=10 should be strength-reduced
    assert!(
        pcs.contains(&10),
        "dmul at PC=10 should be strength-reduced"
    );
}

#[test]
pub(in crate::x64::tests) fn p87_strength_reduction_ignores_non_2() {
    // Same pattern but ldc2_w loads 3.0 instead of 2.0
    let code: Vec<u8> = vec![
        0x15, 0x03, // 0: iload 3
        0x08, // 2: iconst_5
        0xa2, 0x00, 0x0E, // 3: if_icmpge +14
        0x26, // 6: dload_0
        0x14, 0x00, 0x01, // 7: ldc2_w #1
        0x6b, // 10: dmul
        0x47, // 11: dstore_0
        0x84, 0x03, 0x01, // 12: iinc 3, 1
        0xa7, 0xFF, 0xF0, // 15: goto -16
        0x26, // 18: dload_0
        0xaf, // 19: dreturn
        0, 0,
    ];
    let code_len = 20;
    let loops = detect_loops(&code, code_len);
    let ldc2w_info = vec![(7usize, 3.0f64.to_bits() as i64)]; // NOT 2.0 // Cast: JIT ABI convention
    let pcs = find_fp_strength_reductions(&code, code_len, &loops, &ldc2w_info);
    assert!(pcs.is_empty(), "Should not reduce dmul by non-2.0");
}

#[test]
pub(in crate::x64::tests) fn p87_strength_reduction_emits_dadd_self() {
    // Verify that strength-reduced dmul-by-2.0 emits ADDSD XMM0, XMM0
    // double f(double x) { return x * 2.0; }
    // dload_0, ldc2_w #1 (2.0), dmul, dreturn
    let code: Vec<u8> = vec![
        0x26, // 0: dload_0
        0x14, 0x00, 0x01, // 1: ldc2_w #1 (resolves to 2.0)
        0x6b, // 4: dmul
        0xaf, // 5: dreturn
        0, 0,
    ];
    let code_len = 6;

    // Wrap in a loop so strength reduction triggers
    // Actually, strength reduction only triggers inside loops.
    // Let's test without a loop first — dmul should still work normally.
    let ldc2w_info = vec![(1usize, 2.0f64.to_bits() as i64)]; // Cast: JIT ABI convention
    let compiled = compile(
        &code,
        code_len,
        1,
        1,
        false,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        ldc2w_info,
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout — not needed for this test
    )
    .unwrap();

    let x = 7.5f64;
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe {
        compiled
            // Cast: float/double bit pattern to i64 (bit-preserving, VM all-GPR ABI)
            .try_call(&[x.to_bits() as i64])
            .expect("test JIT call")
    }; // Cast: JIT ABI convention
    assert_eq!(f64::from_bits(result as u64), 15.0); // 7.5 * 2.0 = 15.0 // Cast: JIT ABI convention
}

// --- 87.3: SIMD FP Operations ---

#[test]
pub(in crate::x64::tests) fn p87_double_binop_chain_preserves_precision() {
    // Verify FP chaining doesn't lose precision
    // double f(double a) { return a + a + a + a; }
    let code: Vec<u8> = vec![
        0x26, 0x26, 0x63, // dload_0, dload_0, dadd
        0x26, 0x63, // dload_0, dadd
        0x26, 0x63, // dload_0, dadd
        0xaf, // dreturn
        0, 0,
    ];
    let code_len = 8;
    let compiled = compile(
        &code,
        code_len,
        1,
        1,
        false,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout — not needed for this test
    )
    .unwrap();

    let a = std::f64::consts::PI;
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe {
        compiled
            // Cast: float/double bit pattern to i64 (bit-preserving, VM all-GPR ABI)
            .try_call(&[a.to_bits() as i64])
            .expect("test JIT call")
    }; // Cast: JIT ABI convention
    assert_eq!(f64::from_bits(result as u64), a + a + a + a); // Cast: JIT ABI convention
}

#[test]
pub(in crate::x64::tests) fn p87_float_binop_chain_preserves_precision() {
    // float f(float a) { return a * a * a; }
    let code: Vec<u8> = vec![
        0x22, 0x22, 0x6a, // fload_0, fload_0, fmul
        0x22, 0x6a, // fload_0, fmul
        0xae, // freturn
        0, 0,
    ];
    let code_len = 6;
    let compiled = compile(
        &code,
        code_len,
        1,
        1,
        false,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout — not needed for this test
    )
    .unwrap();

    let a = 2.5f32;
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe {
        compiled
            // Cast: float/double bit pattern to i64 (bit-preserving, VM all-GPR ABI)
            .try_call(&[a.to_bits() as i64])
            .expect("test JIT call")
    }; // Cast: JIT ABI convention
    assert_eq!(f32::from_bits(result as u32), a * a * a); // 15.625 // Cast: JIT ABI convention
}

#[test]
pub(in crate::x64::tests) fn p87_double_division_chain() {
    // double f(double a, double b) { return (a / b) / b; }
    // Tests non-commutative FP ops work correctly with XMM chaining
    let code: Vec<u8> = vec![
        0x26, 0x27, 0x6f, // dload_0, dload_1, ddiv
        0x27, 0x6f, // dload_1, ddiv
        0xaf, // dreturn
        0, 0,
    ];
    let code_len = 6;
    let compiled = compile(
        &code,
        code_len,
        2,
        2,
        false,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout — not needed for this test
    )
    .unwrap();

    let a = 100.0f64;
    let b = 5.0f64;
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe {
        compiled
            // Cast: float/double bit pattern to i64 (bit-preserving, VM all-GPR ABI)
            .try_call(&[a.to_bits() as i64, b.to_bits() as i64])
            .expect("test JIT call")
    }; // Cast: JIT ABI convention
    assert_eq!(f64::from_bits(result as u64), (a / b) / b); // 4.0 // Cast: JIT ABI convention
}

#[test]
pub(in crate::x64::tests) fn p87_mixed_fp_and_int_computation() {
    // int f(double a, double b) { double c = a + b; return (int)c; }
    // dload_0, dload_1, dadd, d2i (0x8e), ireturn
    let code: Vec<u8> = vec![
        0x26, 0x27, 0x63, // dload_0, dload_1, dadd
        0x8e, // d2i
        0xac, // ireturn
        0, 0,
    ];
    let code_len = 5;
    let compiled = compile(
        &code,
        code_len,
        2,
        2,
        false,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout — not needed for this test
    )
    .unwrap();

    let a = 3.7f64;
    let b = 2.1f64;
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe {
        compiled
            // Cast: float/double bit pattern to i64 (bit-preserving, VM all-GPR ABI)
            .try_call(&[a.to_bits() as i64, b.to_bits() as i64])
            .expect("test JIT call")
    }; // Cast: JIT ABI convention
    assert_eq!(result, (a + b) as i32 as i64); // 5 // Cast: JIT ABI convention
}

#[test]
pub(in crate::x64::tests) fn p87_fp_hoist_double_and_float() {
    // Verify hoist detection for both float and double types.
    //
    // The invariant `double` is `dload 4`, not the `dload_1` this test used to
    // carry: `dstore_0` at PC=4 writes slots 0 AND 1, so `dload_1` reads a
    // slot the body writes. The old comment said `dstore_0` "modifies 0, but
    // doesn't affect 1", which is not true of a category-2 store — see
    // `p87_fp_loop_hoist_detection`, which carried the same mistake.
    let code: Vec<u8> = vec![
        0x22, // 0: fload_0 (float — local 0, which the dstore_0 below writes)
        0x18, 0x04, // 1: dload 4 (double, invariant — slots 4+5)
        0x63, // 3: dadd (type mismatch at runtime, but tests analysis)
        0x47, // 4: dstore_0 (writes slots 0 AND 1)
        0x84, 0x03, 0x01, // 5: iinc 3, 1
        0x15, 0x03, // 8: iload 3
        0x08, // 10: iconst_5
        0xa1, 0xFF, 0xF5, // 11: if_icmplt -11 → target=0
        0x26, 0xaf, 0, 0,
    ];
    let code_len = 16;
    let loops = detect_loops(&code, code_len);
    let hoists = find_fp_loop_hoists(&code, code_len, &loops);

    // fload_0 at PC=0: local 0 IS modified (dstore_0 at PC=4) → NOT hoistable
    // dload 4 at PC=1: locals 4 and 5 are NOT modified → hoistable
    let hoisted_locals: Vec<(usize, bool)> =
        hoists.iter().map(|h| (h.local_idx, h.is_double)).collect();
    assert!(
        hoisted_locals.contains(&(4, true)),
        "dload 4 should be hoistable"
    );
    assert!(
        !hoisted_locals.iter().any(|(l, _)| *l == 0),
        "local 0 should not be hoistable"
    );
}

#[test]
pub(in crate::x64::tests) fn p87_extract_dload_local_variants() {
    // Test dload extraction helpers
    assert_eq!(extract_dload_local(&[0x26], 0), Some(0)); // dload_0
    assert_eq!(extract_dload_local(&[0x27], 0), Some(1)); // dload_1
    assert_eq!(extract_dload_local(&[0x28], 0), Some(2)); // dload_2
    assert_eq!(extract_dload_local(&[0x29], 0), Some(3)); // dload_3
    assert_eq!(extract_dload_local(&[0x18, 0x05], 0), Some(5)); // dload 5
    assert_eq!(extract_dload_local(&[0x1a], 0), None); // iload_0 — not dload
}

#[test]
pub(in crate::x64::tests) fn p87_extract_dstore_local_variants() {
    assert_eq!(extract_dstore_local(&[0x47], 0), Some(0)); // dstore_0
    assert_eq!(extract_dstore_local(&[0x48], 0), Some(1)); // dstore_1
    assert_eq!(extract_dstore_local(&[0x49], 0), Some(2)); // dstore_2
    assert_eq!(extract_dstore_local(&[0x4a], 0), Some(3)); // dstore_3
    assert_eq!(extract_dstore_local(&[0x39, 0x05], 0), Some(5)); // dstore 5
    assert_eq!(extract_dstore_local(&[0x3b], 0), None); // istore_0 — not dstore
}

// =========================================================================
// Session 32: Scalar replacement plan tests
// =========================================================================

#[test]
pub(in crate::x64::tests) fn s32_plan_scalar_replacement_basic() {
    // Pattern: new; dup; invokespecial <init>()V; astore_1; aload_1; iconst_1; putfield
    let code: Vec<u8> = vec![
        0xbb, 0x00, 0x01, // 0: new #1
        0x59, // 3: dup
        0xb7, 0x00, 0x02, // 4: invokespecial #2 <init>()V
        0x4c, // 7: astore_1
        0x2b, // 8: aload_1
        0x04, // 9: iconst_1
        0xb5, 0x00, 0x03, // 10: putfield #3
        0x2b, // 13: aload_1
        0xb4, 0x00, 0x03, // 14: getfield #3
        0xac, // 17: ireturn
        0, 0,
    ];
    let code_len = 18;
    let mut non_escaping = std::collections::HashSet::new();
    non_escaping.insert(0usize);
    // CRIT-2 tuple: (pc, class_id, num_fields, needs_post_alloc_init, has_finalizer).
    let new_info = vec![(0usize, 1u32, 2usize, true, true)]; // 2 fields
                                                             // Create invoke_info for <init>()V at PC=4
                                                             // LEAK(intentional): the compiled method references this JitInvokeInfo by raw
                                                             // pointer, so it must be 'static and outlive the JIT code; owned by the test
                                                             // process for its lifetime.
    let init_info = Box::leak(Box::new(JitInvokeInfo {
        // LEAK(intentional): test-only; JitInvokeInfo must outlive JIT-compiled code pointer
        class_name: Box::leak("Test".to_string().into_boxed_str()), // LEAK(intentional): test-only; string field of leaked JitInvokeInfo
        method_name: Box::leak("<init>".to_string().into_boxed_str()), // LEAK(intentional): test-only; string field of leaked JitInvokeInfo
        descriptor: Box::leak("()V".to_string().into_boxed_str()), // LEAK(intentional): test-only; string field of leaked JitInvokeInfo
        num_jit_args: 1,
        return_type: b'V',
        invoke_kind: 0xb7,
        declaring_class_id: 0,
        owner_class_id: 0,
    }));
    let invoke_info = vec![(4usize, init_info as *const JitInvokeInfo)]; // Cast: address arithmetic
    let scalar_base = 4; // some offset

    let plan = plan_scalar_replacement(
        &code,
        code_len,
        &non_escaping,
        &new_info,
        &invoke_info,
        scalar_base,
    );

    assert!(
        plan.objects.contains_key(&0),
        "new at PC=0 should be scalar-replaced"
    );
    assert_eq!(plan.objects[&0].num_fields, 2);
    assert!(
        plan.init_skips.contains(&4),
        "invokespecial at PC=4 should be skipped"
    );
    assert!(
        plan.field_ops.contains_key(&10),
        "putfield at PC=10 should be scalar"
    );
    assert!(
        plan.field_ops.contains_key(&14),
        "getfield at PC=14 should be scalar"
    );
    // Two heap fields × two 8-byte words per `Value` slot (`SLOT_SIZE` = 16).
    assert_eq!(plan.total_slots, 4);

    // Phase B: the resolved class id is captured on the object descriptor.
    assert_eq!(plan.objects[&0].class_id, 1);

    // Phase B: per-PC local provenance records local 1 holding the scalar
    // object (new_pc 0) from `astore_1` (PC 7 stores it; first observable at
    // the PC-8 entry) through the rest of the straight-line region. It is NOT
    // recorded before the store (PCs 0..=7 have local 1 empty at entry).
    assert_eq!(
        plan.local_prov_at.get(&8).map(|v| v.as_slice()),
        Some([(1usize, 0usize)].as_slice()),
        "local 1 holds scalar obj 0 at PC 8 (aload_1)"
    );
    for &pc in &[9usize, 10, 13, 14] {
        assert_eq!(
            plan.local_prov_at.get(&pc).map(|v| v.as_slice()),
            Some([(1usize, 0usize)].as_slice()),
            "local 1 holds scalar obj 0 at PC {pc}"
        );
    }
    for &pc in &[0usize, 3, 4, 7] {
        assert!(
            !plan.local_prov_at.contains_key(&pc),
            "no scalar local recorded at PC {pc} (before astore_1)"
        );
    }
}

#[test]
pub(in crate::x64::tests) fn s32_phase_b_sr_field_values_typed_by_tag() {
    use crate::deopt::FrameValue;
    // 5 fields, base offset 80 (positive `[rbp - off]`), SLOT_SIZE = 16.
    // Field tags: 0=ref(L), 1=long(J), 2=double(D), 3=float(F), 4=unaccessed.
    let tags = [Some(b'L'), Some(b'J'), Some(b'D'), Some(b'F'), None];
    let fvs = sr_field_values(5, 80, |k| tags[k]);
    // Offsets: field k at `[rbp - (80 + k*16)]` → encoded `-(80 + k*16)`.
    assert_eq!(fvs[0], FrameValue::StackSlotRef(-80));
    assert_eq!(fvs[1], FrameValue::StackSlotLong(-(80 + 16)));
    assert_eq!(fvs[2], FrameValue::StackSlotDouble(-(80 + 32)));
    assert_eq!(fvs[3], FrameValue::StackSlotFloat(-(80 + 48)));
    // Never-accessed field ⇒ zero default, NOT a slot read.
    assert_eq!(fvs[4], FrameValue::Int(0));
    // An int-family tag (b'I'/b'B'/b'C'/b'S'/b'Z') maps to the cat-1 StackSlot.
    let ints = sr_field_values(1, 16, |_| Some(b'I'));
    assert_eq!(ints[0], FrameValue::StackSlot(-16));
}

#[test]
pub(in crate::x64::tests) fn s32_phase_c_plan_tracks_scalar_monitors() {
    // `Foo f = new Foo(); synchronized(f) { f.x = 1; }` (straight-line, no
    // exception handler) — a `synchronized` block over a scalar object.
    let code: Vec<u8> = vec![
        0xbb, 0x00, 0x01, // 0: new #1
        0x59, // 3: dup
        0xb7, 0x00, 0x02, // 4: invokespecial #2 <init>()V
        0x4c, // 7: astore_1            -> f in local 1
        0x2b, // 8: aload_1             -> [f]
        0x59, // 9: dup                 -> [f, f]
        0x4d, // 10: astore_2           -> local 2 = f, [f]
        0xc2, // 11: monitorenter       -> lock f (mon_depth[0]=1)
        0x2b, // 12: aload_1            -> [f]
        0x04, // 13: iconst_1           -> [f, 1]
        0xb5, 0x00, 0x03, // 14: putfield #3
        0x2c, // 17: aload_2            -> [f]
        0xc3, // 18: monitorexit        -> unlock (mon_depth[0]=0)
        0xb1, // 19: return
        0, 0,
    ];
    let code_len = 20;
    let mut non_escaping = std::collections::HashSet::new();
    non_escaping.insert(0usize);
    let new_info = vec![(0usize, 7u32, 1usize, true, true)]; // class_id 7, 1 field
                                                             // LEAK(intentional): this test JitInvokeInfo is referenced by raw
                                                             // pointer from generated code, so it and its string fields must remain
                                                             // valid for the process lifetime.
    let init_info = Box::leak(Box::new(JitInvokeInfo {
        // LEAK(intentional): field of leaked JitInvokeInfo.
        class_name: Box::leak("Foo".to_string().into_boxed_str()),
        // LEAK(intentional): field of leaked JitInvokeInfo.
        method_name: Box::leak("<init>".to_string().into_boxed_str()),
        // LEAK(intentional): field of leaked JitInvokeInfo.
        descriptor: Box::leak("()V".to_string().into_boxed_str()),
        num_jit_args: 1,
        return_type: b'V',
        invoke_kind: 0xb7,
        declaring_class_id: 0,
        owner_class_id: 0,
    }));
    let invoke_info = vec![(4usize, init_info as *const JitInvokeInfo)];
    let plan = plan_scalar_replacement(&code, code_len, &non_escaping, &new_info, &invoke_info, 0);

    assert!(
        plan.objects.contains_key(&0),
        "Foo should be scalar-replaced"
    );
    // Both monitor ops are over the scalar object (relockable, not blocking).
    assert!(
        plan.monitor_scalar_ops.contains(&11),
        "monitorenter@11 is scalar"
    );
    assert!(
        plan.monitor_scalar_ops.contains(&18),
        "monitorexit@18 is scalar"
    );
    // The lock is held (depth 1) across the block body and AT the monitorexit
    // entry (it executes the unlock), but NOT before the enter / after the exit.
    assert_eq!(
        plan.monitor_at.get(&12).map(|v| v.as_slice()),
        Some([(0usize, 1u32)].as_slice())
    );
    assert_eq!(
        plan.monitor_at.get(&14).map(|v| v.as_slice()),
        Some([(0usize, 1u32)].as_slice())
    );
    assert_eq!(
        plan.monitor_at.get(&18).map(|v| v.as_slice()),
        Some([(0usize, 1u32)].as_slice())
    );
    assert!(
        !plan.monitor_at.contains_key(&11),
        "not held before the enter executes"
    );
    assert!(
        !plan.monitor_at.contains_key(&19),
        "released after the exit"
    );
}

#[test]
pub(in crate::x64::tests) fn s32_plan_scalar_replacement_escaping_not_replaced() {
    // Pattern: new; dup; invokespecial; areturn — object escapes, should NOT be replaced
    let code: Vec<u8> = vec![
        0xbb, 0x00, 0x01, // 0: new #1
        0x59, // 3: dup
        0xb7, 0x00, 0x02, // 4: invokespecial <init>
        0xb0, // 7: areturn
        0, 0,
    ];
    // NOT in non_escaping_new → should produce empty plan
    let non_escaping = std::collections::HashSet::new();
    // CRIT-2 tuple shape: see compiler struct doc.
    let new_info = vec![(0usize, 1u32, 2usize, true, true)];
    let plan = plan_scalar_replacement(&code, 8, &non_escaping, &new_info, &[], 0);
    assert!(plan.objects.is_empty());
    assert!(plan.field_ops.is_empty());
    assert!(plan.init_skips.is_empty());
}

#[test]
pub(in crate::x64::tests) fn s32_plan_nonvoid_init_not_skipped() {
    // invokespecial with non-()V descriptor should NOT be skipped
    let code: Vec<u8> = vec![
        0xbb, 0x00, 0x01, // 0: new #1
        0x59, // 3: dup
        0x04, // 4: iconst_1
        0xb7, 0x00, 0x02, // 5: invokespecial <init>(I)V
        0xac, // 8: ireturn (dummy)
        0, 0,
    ];
    let mut non_escaping = std::collections::HashSet::new();
    non_escaping.insert(0usize);
    // CRIT-2 tuple shape: see compiler struct doc.
    let new_info = vec![(0usize, 1u32, 2usize, true, true)];
    // LEAK(intentional): the compiled method references this JitInvokeInfo by raw
    // pointer, so it must be 'static and outlive the JIT code; owned by the test
    // process for its lifetime.
    let init_info = Box::leak(Box::new(JitInvokeInfo {
        // LEAK(intentional): test-only; JitInvokeInfo must outlive JIT-compiled code pointer
        class_name: Box::leak("Test".to_string().into_boxed_str()), // LEAK(intentional): test-only; string field of leaked JitInvokeInfo
        method_name: Box::leak("<init>".to_string().into_boxed_str()), // LEAK(intentional): test-only; string field of leaked JitInvokeInfo
        descriptor: Box::leak("(I)V".to_string().into_boxed_str()), // LEAK(intentional): test-only; string field of leaked JitInvokeInfo
        num_jit_args: 2,
        return_type: b'V',
        invoke_kind: 0xb7,
        declaring_class_id: 0,
        owner_class_id: 0,
    }));
    let invoke_info = vec![(5usize, init_info as *const JitInvokeInfo)]; // Cast: address arithmetic
    let plan = plan_scalar_replacement(&code, 9, &non_escaping, &new_info, &invoke_info, 0);
    // init should NOT be skipped (non-void-init has args)
    assert!(!plan.init_skips.contains(&5));
}

// ===================================================================
// Session 34: JIT Switch Compilation tests
// ===================================================================

/// Helper: build bytecode for a method that uses tableswitch.
/// Layout: iload_0 at PC 0, tableswitch at PC 1 (padded to 4-byte align),
/// then case bodies that push sipush val; ireturn.
pub(in crate::x64::tests) fn build_tableswitch_bytecode(
    low: i32,
    cases: &[i32],
    default_val: i32,
) -> (Vec<u8>, usize) {
    let mut code = Vec::new();
    // PC 0: iload_0
    code.push(0x1A);
    // PC 1: tableswitch opcode
    code.push(0xAA);
    // Padding to align to 4-byte boundary from start of method (PC 0)
    while (code.len()) % 4 != 0 {
        code.push(0);
    }
    let table_base = 1; // base_pc of the tableswitch opcode
    let high = low + cases.len() as i32 - 1; // Cast: x86-64 immediate encoding
    let num_cases = cases.len();

    // Each case body: sipush val (3 bytes) + ireturn (1 byte) = 4 bytes
    let switch_data_size = 12 + num_cases * 4;
    let aligned_start = code.len();
    let body_start_pc = aligned_start + switch_data_size;
    let default_body_pc = body_start_pc + num_cases * 4;
    let default_offset = default_body_pc as i32 - table_base as i32; // Cast: x86-64 immediate encoding

    code.extend_from_slice(&default_offset.to_be_bytes());
    code.extend_from_slice(&low.to_be_bytes());
    code.extend_from_slice(&high.to_be_bytes());

    for i in 0..num_cases {
        let case_body_pc = body_start_pc + i * 4;
        let off = case_body_pc as i32 - table_base as i32; // Cast: x86-64 immediate encoding
        code.extend_from_slice(&off.to_be_bytes());
    }

    // Case bodies: sipush val; ireturn
    for &val in cases {
        code.push(0x11); // sipush
        code.extend_from_slice(&(val as i16).to_be_bytes()); // Cast: x86-64 immediate encoding
        code.push(0xAC); // ireturn
    }

    // Default body
    code.push(0x11);
    code.extend_from_slice(&(default_val as i16).to_be_bytes()); // Cast: x86-64 immediate encoding
    code.push(0xAC);

    let code_len = code.len();
    code.push(0);
    code.push(0);
    (code, code_len)
}

/// Helper: build bytecode for a method that uses lookupswitch.
pub(in crate::x64::tests) fn build_lookupswitch_bytecode(
    pairs: &[(i32, i32)],
    default_val: i32,
) -> (Vec<u8>, usize) {
    let mut code = Vec::new();
    // PC 0: iload_0
    code.push(0x1A);
    // PC 1: lookupswitch opcode
    code.push(0xAB);
    while (code.len()) % 4 != 0 {
        code.push(0);
    }
    let table_base = 1;
    let npairs = pairs.len();

    let switch_data_size = 8 + npairs * 8;
    let aligned_start = code.len();
    let body_start_pc = aligned_start + switch_data_size;
    let default_body_pc = body_start_pc + npairs * 4;
    let default_offset = default_body_pc as i32 - table_base as i32; // Cast: x86-64 immediate encoding

    code.extend_from_slice(&default_offset.to_be_bytes());
    code.extend_from_slice(&(npairs as i32).to_be_bytes()); // Cast: x86-64 immediate encoding

    for (i, &(key, _val)) in pairs.iter().enumerate() {
        let case_body_pc = body_start_pc + i * 4;
        let off = case_body_pc as i32 - table_base as i32; // Cast: x86-64 immediate encoding
        code.extend_from_slice(&key.to_be_bytes());
        code.extend_from_slice(&off.to_be_bytes());
    }

    for &(_key, val) in pairs {
        code.push(0x11);
        code.extend_from_slice(&(val as i16).to_be_bytes()); // Cast: x86-64 immediate encoding
        code.push(0xAC);
    }

    code.push(0x11);
    code.extend_from_slice(&(default_val as i16).to_be_bytes()); // Cast: x86-64 immediate encoding
    code.push(0xAC);

    let code_len = code.len();
    code.push(0);
    code.push(0);
    (code, code_len)
}

/// `compile_switch_method`'s shape with the locals a handler-body fixture
/// needs. The bytecode under test never runs; only the compiler's decision
/// about it is asserted.
pub(in crate::x64::tests) fn compile_probe_method(
    code: &[u8],
    num_params: usize,
    max_locals: usize,
) -> Option<CompiledMethod> {
    compile(
        code,
        code.len(),
        num_params,
        max_locals,
        false,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
}

/// A dead region containing a branch of its own must not refuse the method.
///
/// The backend has no in-method exception-handler dispatch, so a handler
/// body is dead code in the emitted image — but a branch INSIDE one still
/// marks its own targets, and the DCE walk revived at every branch target
/// regardless of whether anything reachable could get there. It came back
/// with no recorded operand-stack depth, rebuilt an empty stack, and then
/// underflowed on the merge's `iadd`, refusing the whole method.
///
/// Shape transcribed from `Rbc6FieldProbe.getfieldRefHandlerLocal`'s
/// handler — `catch (NPE e) { return scratch + (seen == null ? 0 : 1); }`
/// — whose ternary is the branch in question. See
/// `singlepass-codegen-refuses-handler-body-merge-FIXED-20260803.md`.
#[test]
pub(in crate::x64::tests) fn a_dead_region_with_an_internal_branch_does_not_refuse_the_method() {
    //  0: iload_0
    //  1: ireturn           <- everything below is unreachable
    //  2: iload_0
    //  3: aload_1
    //  4: ifnonnull 11
    //  7: iconst_0
    //  8: goto 12
    // 11: iconst_1
    // 12: iadd              <- the merge that underflowed
    // 13: ireturn
    let code = [
        0x1a, 0xac, 0x1a, 0x2b, 0xc7, 0x00, 0x07, 0x03, 0xa7, 0x00, 0x04, 0x04, 0x60, 0xac,
    ];
    assert!(
        compile_probe_method(&code, 2, 2).is_some(),
        "a branch inside an unreachable region must not refuse the method"
    );

    // The live prefix alone compiles, so the refusal really did come from
    // the dead tail and not from `iload_0; ireturn`.
    assert!(compile_probe_method(&code[..2], 2, 2).is_some());
}

/// The same shape reached through a `goto` over the dead region, which is
/// how a `try`/`catch` actually lays out: the live path jumps past the
/// handler, so the handler body sits between two live PCs.
#[test]
pub(in crate::x64::tests) fn a_dead_region_between_two_live_ones_is_skipped_whole() {
    //  0: goto 14           (over the "handler")
    //  3: iload_0           <- unreachable from here ...
    //  4: aload_1
    //  5: ifnonnull 12
    //  8: iconst_0
    //  9: goto 13
    // 12: iconst_1
    // 13: iadd              <- ... to here
    // 14: iconst_2          <- live again
    // 15: ireturn
    let code = [
        0xa7, 0x00, 0x0e, 0x1a, 0x2b, 0xc7, 0x00, 0x07, 0x03, 0xa7, 0x00, 0x04, 0x04, 0x60, 0x05,
        0xac,
    ];
    assert!(compile_probe_method(&code, 2, 2).is_some());
}

/// A refusal raised through the `failed` FLAG names the site that raised
/// it, not whatever opcode the walk happened to reach afterwards.
///
/// `dup2` at PC 0 has no preceding instruction, so its top-of-stack width
/// is unprovable and the arm raises the flag. The walk then runs on to the
/// `ireturn`, which is exactly the misattribution this records: the old
/// reason string was `singlepass-codegen(pc=1,op=0xac)`.
#[test]
pub(in crate::x64::tests) fn a_flag_refusal_names_the_site_that_raised_it() {
    let code = [0x5c, 0xac]; // dup2; ireturn
    let _ = crate::take_jit_bail_site(); // clear anything a prior test left
    assert!(compile_probe_method(&code, 0, 1).is_none());
    let (site, _, _) = crate::take_jit_bail_site().expect("a refusal records a site");
    assert_eq!(site, "singlepass-codegen/dup2-unprovable-top-width");
}

/// `return cond ? x : helper()` — the `else` arm's call sits immediately
/// before the shared `xreturn`, and the `then` arm's `goto` lands on it.
///
/// Both tail-call forms USED to swallow that `xreturn`: they emit no code for
/// its PC, so `pc_to_native` stayed -1 there, and `patch_branches` then
/// rejected the whole method with `branch-target-not-an-instruction-boundary`
/// — a reason that blames malformed bytecode for what is ordinary javac
/// output. See
/// `jit-tailcall-swallows-shared-return-FIXED-20260803.md`.
///
///     0: iload_0
///     1: ifeq 8
///     4: iconst_1
///     5: goto 12          <- the edge onto the return
///     8: iload_0
///     9: invokestatic
///    12: ireturn          <- swallowed by the tail form
pub(in crate::x64::tests) const TAILCALL_OVER_SHARED_RETURN: [u8; 13] = [
    0x1a, 0x99, 0x00, 0x07, 0x04, 0xa7, 0x00, 0x07, 0x1a, 0xb8, 0x00, 0x01, 0xac,
];

/// The self-recursive tail form (an `invokestatic` with no `invoke_info` and no
/// `direct_call` is a self-call here, as every other test in this file relies
/// on).
#[test]
pub(in crate::x64::tests) fn a_self_tail_call_may_not_swallow_a_branch_targeted_return() {
    assert!(
        compile_probe_method(&TAILCALL_OVER_SHARED_RETURN, 1, 1).is_some(),
        "the `goto`'s target is the `ireturn` the tail form consumes"
    );
}

/// The sibling tail form — reached only when the callee is ALREADY compiled,
/// which is why this never reproduced from a cold standalone probe and only
/// showed up inside a warm Spring context.
#[test]
pub(in crate::x64::tests) fn a_sibling_tail_call_may_not_swallow_a_branch_targeted_return() {
    assert!(
        compile_with_direct_call(&TAILCALL_OVER_SHARED_RETURN, 1, 1, 9, direct_callee_i())
            .is_some(),
        "a direct-callable callee must not let the tail form eat the shared return"
    );
}

/// Without the branch onto it, the same call/return pair is still the fusible
/// shape and must keep compiling — the guard is about the merge, not about
/// tail calls.
#[test]
pub(in crate::x64::tests) fn a_tail_call_over_an_unshared_return_still_compiles() {
    let code = [0x1a, 0xb8, 0x00, 0x01, 0xac]; // iload_0; invokestatic; ireturn
    assert!(compile_probe_method(&code, 1, 1).is_some());
    assert!(compile_with_direct_call(&code, 1, 1, 1, direct_callee_i()).is_some());
}

/// A one-`int`-arg callee returning `int`. `entry` is never executed — only
/// emitted as the JMP/CALL target.
pub(in crate::x64::tests) fn direct_callee_i() -> crate::JitDirectCall {
    crate::JitDirectCall {
        entry: 0x1000,
        needs_context: false,
        num_params: 1,
        return_type: b'I',
        guard_class_id: 0,
    }
}

/// `compile_probe_method` with one direct-callable callee wired at `at_pc`.
pub(in crate::x64::tests) fn compile_with_direct_call(
    code: &[u8],
    num_params: usize,
    max_locals: usize,
    at_pc: usize,
    callee: crate::JitDirectCall,
) -> Option<CompiledMethod> {
    compile(
        code,
        code.len(),
        num_params,
        max_locals,
        false,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        vec![(at_pc, callee)],
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
}

// -----------------------------------------------------------------------
// Regression: ecj-operandstack-corruption-jsp-compilation-500s-FIXED-20260821.md
// -----------------------------------------------------------------------
//
// A direct-call site that ALSO carries `invoke_info` reserves a cold-deopt
// copy of the arguments through `reserve_direct_call_service_slots`. That
// reservation has to sit ABOVE the argument slots `pop_stack` just handed
// back (they are still live sources for `emit_stack_arg_setup`), so it moves
// `next_spill_offset` past them — and the call's return value used to be
// pushed from there, one operand-stack slot per argument too deep.
//
// The shift is invisible inside a basic block: the linear walk keeps writing
// and reading the same shifted slots. It becomes wrong code at the first
// branch target after the call, whose depth is re-established from the
// bytecode — writer and reader then address different slots. Measured on
// ECJ's `OperandStack.pop(OperandCategory)`, whose `if_icmpeq` sits on a
// tableswitch merge and so compared `TypeBinding.id` against the expected
// category instead of `TypeIds.getCategory(id)`: every JSP compiled after
// that method tiered up threw `AssertionError: Unexpected operand at stack
// top`, surfacing as an HTTP 500 from Jasper.

/// A one-reference-arg callee returning a reference, direct-callable.
/// `entry` is never executed — only emitted as the CALL target.
pub(in crate::x64::tests) fn direct_callee_ref() -> crate::JitDirectCall {
    crate::JitDirectCall {
        entry: 0x1000,
        needs_context: false,
        num_params: 1,
        return_type: b'L',
        guard_class_id: 0,
    }
}

///     0: aload_0
///     1: invokestatic #1   <- direct-callable, returns a reference
///     4: invokestatic #2   <- helper dispatch: a safepoint with the pc-1
///                             result live on the operand stack
///     7: areturn
pub(in crate::x64::tests) const DIRECT_CALL_RESULT_LIVE_AT_SAFEPOINT: [u8; 8] =
    [0x2a, 0xb8, 0x00, 0x01, 0xb8, 0x00, 0x02, 0xb0];

/// Compile the shape above and return the oop-map frame slots recorded at the
/// pc-4 safepoint — i.e. where the pc-1 call parked its reference result.
/// `service_info_at_pc1` decides whether the direct-call site also carries
/// `invoke_info`, which is what makes it reserve the service-argument range.
pub(in crate::x64::tests) fn direct_call_result_slots_at_pc4(
    service_info_at_pc1: bool,
) -> Vec<i16> {
    // LEAK(intentional): compiled code stores raw pointers to these, so they
    // must outlive it; the test process owns them for its (short) lifetime.
    let sink = Box::leak(Box::new(JitInvokeInfo {
        class_name: "T",
        method_name: "sink",
        descriptor: "()V",
        num_jit_args: 0,
        return_type: b'V',
        invoke_kind: 3,
        declaring_class_id: 0,
        owner_class_id: 0,
    }));
    let mut invoke_info: Vec<(usize, *const JitInvokeInfo)> =
        vec![(4usize, sink as *const JitInvokeInfo)];
    if service_info_at_pc1 {
        let callee = Box::leak(Box::new(JitInvokeInfo {
            class_name: "T",
            method_name: "f",
            descriptor: "(Ljava/lang/Object;)Ljava/lang/Object;",
            num_jit_args: 1,
            return_type: b'L',
            invoke_kind: 3,
            declaring_class_id: 0,
            owner_class_id: 0,
        }));
        invoke_info.push((1usize, callee as *const JitInvokeInfo));
    }
    let compiled = compile(
        &DIRECT_CALL_RESULT_LIVE_AT_SAFEPOINT,
        DIRECT_CALL_RESULT_LIVE_AT_SAFEPOINT.len(),
        1,
        1,
        true,
        Vec::new(), // multianewarray_info
        Vec::new(), // field_info
        Vec::new(), // typecheck_info
        Vec::new(), // static_field_info
        Vec::new(), // new_info
        Vec::new(), // anewarray_info
        invoke_info,
        vec![(1usize, direct_callee_ref())],
        Vec::new(), // mic_slots
        Vec::new(), // pic_slots
        Vec::new(), // ldc_info
        Vec::new(), // ldc2w_info
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .expect("direct call followed by a dispatched call must compile");
    let mut slots = compiled
        .oop_maps
        .iter()
        .find(|m| m.bytecode_pc == 4)
        .map(|m| m.frame_slot_offsets.clone())
        .unwrap_or_default();
    slots.sort_unstable();
    slots
}

/// The return value's operand-stack depth must not depend on whether the site
/// reserved a service-argument range.
#[test]
pub(in crate::x64::tests) fn direct_call_result_slot_is_independent_of_service_arg_reservation() {
    let plain = direct_call_result_slots_at_pc4(false);
    let with_service_copy = direct_call_result_slots_at_pc4(true);
    assert!(
        !plain.is_empty(),
        "the pc-1 reference result must be a mapped live oop at the pc-4 safepoint"
    );
    assert_eq!(
        with_service_copy, plain,
        "the service-argument reservation moved the return value off its \
         operand-stack depth: the linear walk and every branch target after \
         this call now disagree about which slot holds it"
    );
}

pub(in crate::x64::tests) fn compile_switch_method(
    code: &[u8],
    code_len: usize,
) -> Option<CompiledMethod> {
    compile(
        code,
        code_len,
        1,
        1,
        false,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
}

#[test]
pub(in crate::x64::tests) fn test_jit_scan_rejects_tableswitch_payload_past_code_len() {
    let (code, code_len) = build_tableswitch_bytecode(0, &[10], -1);
    assert!(jit_scan(&code, code_len, "(I)I").is_some());

    let truncated_len = 4 + 12 + 2; // aligned operands + header + partial entry
    assert!(truncated_len < code_len);
    assert!(
        jit_scan(&code, truncated_len, "(I)I").is_none(),
        "tableswitch payload extending past code_len must be rejected by scan"
    );
}

#[test]
pub(in crate::x64::tests) fn test_jit_scan_rejects_lookupswitch_payload_past_code_len() {
    let (code, code_len) = build_lookupswitch_bytecode(&[(7, 10)], -1);
    assert!(jit_scan(&code, code_len, "(I)I").is_some());

    let truncated_len = 4 + 8 + 4; // aligned operands + header + partial pair
    assert!(truncated_len < code_len);
    assert!(
        jit_scan(&code, truncated_len, "(I)I").is_none(),
        "lookupswitch payload extending past code_len must be rejected by scan"
    );
}

#[test]
pub(in crate::x64::tests) fn s34_tableswitch_small_3_cases() {
    let (code, code_len) = build_tableswitch_bytecode(0, &[10, 20, 30], -1);
    let compiled = compile_switch_method(&code, code_len).expect("compilation failed");
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    unsafe {
        assert_eq!(compiled.try_call(&[0]).expect("test JIT call"), 10);
        assert_eq!(compiled.try_call(&[1]).expect("test JIT call"), 20);
        assert_eq!(compiled.try_call(&[2]).expect("test JIT call"), 30);
        assert_eq!(
            // Widening: i32 -> i64 (sign-extended)
            compiled.try_call(&[-1i32 as i64]).expect("test JIT call"),
            -1i64
        ); // Cast: JIT ABI convention
        assert_eq!(compiled.try_call(&[3]).expect("test JIT call"), -1i64);
        assert_eq!(compiled.try_call(&[100]).expect("test JIT call"), -1i64);
    }
}

#[test]
pub(in crate::x64::tests) fn s34_tableswitch_large_jump_table() {
    // 10 cases triggers jump table path (count > 4)
    let cases: Vec<i32> = (0..10).map(|i| (i + 1) * 100).collect();
    let (code, code_len) = build_tableswitch_bytecode(0, &cases, -999);
    let compiled = compile_switch_method(&code, code_len).expect("compilation failed");
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    unsafe {
        for i in 0..10 {
            assert_eq!(
                // Cast: test value to i64 for the JIT calling convention
                compiled.try_call(&[i as i64]).expect("test JIT call"),
                ((i + 1) * 100) as i64, // Cast: JIT ABI convention
                "case {} failed",
                i
            );
        }
        assert_eq!(
            // Widening: i32 -> i64 (sign-extended)
            compiled.try_call(&[-1i32 as i64]).expect("test JIT call"),
            -999i64
        ); // Cast: JIT ABI convention
        assert_eq!(compiled.try_call(&[10]).expect("test JIT call"), -999i64);
        assert_eq!(compiled.try_call(&[1000]).expect("test JIT call"), -999i64);
    }
}

#[test]
pub(in crate::x64::tests) fn s34_tableswitch_nonzero_low() {
    let cases: Vec<i32> = vec![50, 60, 70, 80, 90];
    let (code, code_len) = build_tableswitch_bytecode(5, &cases, 0);
    let compiled = compile_switch_method(&code, code_len).expect("compilation failed");
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    unsafe {
        assert_eq!(compiled.try_call(&[5]).expect("test JIT call"), 50);
        assert_eq!(compiled.try_call(&[6]).expect("test JIT call"), 60);
        assert_eq!(compiled.try_call(&[7]).expect("test JIT call"), 70);
        assert_eq!(compiled.try_call(&[8]).expect("test JIT call"), 80);
        assert_eq!(compiled.try_call(&[9]).expect("test JIT call"), 90);
        assert_eq!(compiled.try_call(&[4]).expect("test JIT call"), 0);
        assert_eq!(compiled.try_call(&[10]).expect("test JIT call"), 0);
    }
}

#[test]
pub(in crate::x64::tests) fn s34_tableswitch_negative_low() {
    let cases: Vec<i32> = vec![200, 201, 202, 203, 204];
    let (code, code_len) = build_tableswitch_bytecode(-2, &cases, -1);
    let compiled = compile_switch_method(&code, code_len).expect("compilation failed");
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    unsafe {
        assert_eq!(
            // Widening: i32 -> i64 (sign-extended)
            compiled.try_call(&[-2i32 as i64]).expect("test JIT call"),
            200
        ); // Cast: JIT ABI convention
        assert_eq!(
            compiled.try_call(&[-1i32 as i64]).expect("test JIT call"),
            201
        ); // Cast: JIT ABI convention
        assert_eq!(compiled.try_call(&[0]).expect("test JIT call"), 202);
        assert_eq!(compiled.try_call(&[1]).expect("test JIT call"), 203);
        assert_eq!(compiled.try_call(&[2]).expect("test JIT call"), 204);
        assert_eq!(
            // Widening: i32 -> i64 (sign-extended)
            compiled.try_call(&[-3i32 as i64]).expect("test JIT call"),
            -1i64
        ); // Cast: JIT ABI convention
        assert_eq!(compiled.try_call(&[3]).expect("test JIT call"), -1i64);
    }
}

#[test]
pub(in crate::x64::tests) fn s34_tableswitch_single_case() {
    let (code, code_len) = build_tableswitch_bytecode(0, &[42], -1);
    let compiled = compile_switch_method(&code, code_len).expect("compilation failed");
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    unsafe {
        assert_eq!(compiled.try_call(&[0]).expect("test JIT call"), 42);
        assert_eq!(compiled.try_call(&[1]).expect("test JIT call"), -1i64);
    }
}

#[test]
pub(in crate::x64::tests) fn s34_lookupswitch_small_linear() {
    let pairs = vec![(10, 100), (20, 200), (30, 300)];
    let (code, code_len) = build_lookupswitch_bytecode(&pairs, -1);
    let compiled = compile_switch_method(&code, code_len).expect("compilation failed");
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    unsafe {
        assert_eq!(compiled.try_call(&[10]).expect("test JIT call"), 100);
        assert_eq!(compiled.try_call(&[20]).expect("test JIT call"), 200);
        assert_eq!(compiled.try_call(&[30]).expect("test JIT call"), 300);
        assert_eq!(compiled.try_call(&[0]).expect("test JIT call"), -1i64);
        assert_eq!(compiled.try_call(&[15]).expect("test JIT call"), -1i64);
        assert_eq!(compiled.try_call(&[99]).expect("test JIT call"), -1i64);
    }
}

#[test]
pub(in crate::x64::tests) fn s34_lookupswitch_large_binary_search() {
    // 10 sparse pairs triggers binary search (npairs > 6)
    // Values must fit in i16 for sipush encoding
    let pairs: Vec<(i32, i32)> = vec![
        (5, 50),
        (10, 100),
        (20, 200),
        (50, 500),
        (100, 1000),
        (200, 2000),
        (500, 5000),
        (1000, 10000),
        (2000, 20000),
        (5000, 30000),
    ];
    let (code, code_len) = build_lookupswitch_bytecode(&pairs, -1);
    let compiled = compile_switch_method(&code, code_len).expect("compilation failed");
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    unsafe {
        for &(key, val) in &pairs {
            assert_eq!(
                // Cast: test value to i64 for the JIT calling convention
                compiled.try_call(&[key as i64]).expect("test JIT call"),
                val as i64, // Cast: JIT ABI convention
                "key {} should return {}",
                key,
                val
            );
        }
        assert_eq!(compiled.try_call(&[0]).expect("test JIT call"), -1i64);
        assert_eq!(compiled.try_call(&[7]).expect("test JIT call"), -1i64);
        assert_eq!(compiled.try_call(&[150]).expect("test JIT call"), -1i64);
        assert_eq!(compiled.try_call(&[99999]).expect("test JIT call"), -1i64);
    }
}

#[test]
pub(in crate::x64::tests) fn s34_lookupswitch_negative_keys() {
    // 8 pairs with negatives → binary search
    let pairs: Vec<(i32, i32)> = vec![
        (-100, 1),
        (-50, 2),
        (-10, 3),
        (0, 4),
        (10, 5),
        (50, 6),
        (100, 7),
        (1000, 8),
    ];
    let (code, code_len) = build_lookupswitch_bytecode(&pairs, -1);
    let compiled = compile_switch_method(&code, code_len).expect("compilation failed");
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    unsafe {
        for &(key, val) in &pairs {
            assert_eq!(
                // Cast: test value to i64 for the JIT calling convention
                compiled.try_call(&[key as i64]).expect("test JIT call"),
                // Cast: test value to i64 for the JIT calling convention
                val as i64
            ); // Cast: JIT ABI convention
        }
        assert_eq!(
            compiled.try_call(&[-200i32 as i64]).expect("test JIT call"),
            -1i64
        ); // Cast: JIT ABI convention
        assert_eq!(compiled.try_call(&[999]).expect("test JIT call"), -1i64);
    }
}

#[test]
pub(in crate::x64::tests) fn s34_lookupswitch_single_pair() {
    let pairs = vec![(42, 999)];
    let (code, code_len) = build_lookupswitch_bytecode(&pairs, 0);
    let compiled = compile_switch_method(&code, code_len).expect("compilation failed");
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    unsafe {
        assert_eq!(compiled.try_call(&[42]).expect("test JIT call"), 999);
        assert_eq!(compiled.try_call(&[0]).expect("test JIT call"), 0);
        assert_eq!(compiled.try_call(&[43]).expect("test JIT call"), 0);
    }
}

#[test]
pub(in crate::x64::tests) fn s34_tableswitch_all_same_target() {
    let cases = vec![77; 8];
    let (code, code_len) = build_tableswitch_bytecode(0, &cases, -1);
    let compiled = compile_switch_method(&code, code_len).expect("compilation failed");
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    unsafe {
        for i in 0..8 {
            // Cast: test value to i64 for the JIT calling convention
            assert_eq!(compiled.try_call(&[i as i64]).expect("test JIT call"), 77);
            // Cast: JIT ABI convention
        }
        assert_eq!(
            compiled.try_call(&[-1i32 as i64]).expect("test JIT call"),
            -1i64
        ); // Cast: JIT ABI convention
        assert_eq!(compiled.try_call(&[8]).expect("test JIT call"), -1i64);
    }
}

#[test]
pub(in crate::x64::tests) fn s34_lookupswitch_two_pairs() {
    let pairs = vec![(0, 111), (1000, 222)];
    let (code, code_len) = build_lookupswitch_bytecode(&pairs, -1);
    let compiled = compile_switch_method(&code, code_len).expect("compilation failed");
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    unsafe {
        assert_eq!(compiled.try_call(&[0]).expect("test JIT call"), 111);
        assert_eq!(compiled.try_call(&[1000]).expect("test JIT call"), 222);
        assert_eq!(compiled.try_call(&[500]).expect("test JIT call"), -1i64);
    }
}

#[test]
pub(in crate::x64::tests) fn s34_tableswitch_large_20_cases() {
    let cases: Vec<i32> = (0..20).map(|i| i * 11).collect();
    let (code, code_len) = build_tableswitch_bytecode(0, &cases, -1);
    let compiled = compile_switch_method(&code, code_len).expect("compilation failed");
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    unsafe {
        for i in 0..20 {
            assert_eq!(
                // Cast: test value to i64 for the JIT calling convention
                compiled.try_call(&[i as i64]).expect("test JIT call"),
                // Cast: loop counter expression to i64 (expected return value)
                (i * 11) as i64
            ); // Cast: JIT ABI convention
        }
        assert_eq!(compiled.try_call(&[20]).expect("test JIT call"), -1i64);
    }
}

// -----------------------------------------------------------------------
// S31 — JIT Method Inlining
// -----------------------------------------------------------------------

/// Helper: compile a method with inline sites.
pub(in crate::x64::tests) fn compile_with_inlines(
    code: &[u8],
    code_len: usize,
    num_params: usize,
    max_locals: usize,
    inline_sites: HashMap<usize, crate::InlineSite>,
) -> Option<CompiledMethod> {
    compile(
        code,
        code_len,
        num_params,
        max_locals,
        false,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(), // pic_slots
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        inline_sites,
        None, // string_layout
    )
}

/// Build an InlineSite from raw callee bytecode.
pub(in crate::x64::tests) fn make_inline_site(
    callee_bytecode: &[u8],
    callee_max_locals: usize,
    callee_num_args: usize,
    callee_is_static: bool,
    return_type: u8,
) -> crate::InlineSite {
    let mut callee_code = callee_bytecode.to_vec();
    callee_code.push(0); // padding
    callee_code.push(0);
    let declared_args = if callee_is_static {
        callee_num_args
    } else {
        callee_num_args.saturating_sub(1)
    };
    let descriptor = format!(
        "({}){}",
        "I".repeat(declared_args),
        if return_type == b'V' {
            "V".to_string()
        } else {
            "I".to_string()
        }
    );
    crate::InlineSite {
        callee_original_code: None,
        callee_original_handler_rows: Vec::new(),
        callee_code,
        callee_code_len: callee_bytecode.len(),
        callee_max_locals,
        callee_num_args,
        callee_is_static,
        return_type,
        field_info: Vec::new(),
        field_decl_info: Vec::new(),
        compact_field_info: Vec::new(),
        static_field_info: Vec::new(),
        ldc_info: Vec::new(),
        ldc2w_info: Vec::new(),
        ldc_fp_pcs: Vec::new(),
        needs_heap: false,
        class_name: "Test".to_string(),
        class_id: 0,
        method_name: "inlined".to_string(),
        descriptor,
        elided_invoke_pcs: Vec::new(),
        invoke_targets: Vec::new(),
        unloaded_static_call_pcs: Vec::new(),
        resolved_invoke_infos: Vec::new(),
        nested_sites: Default::default(),
        ir_new_info: Vec::new(),
        ir_anewarray_info: Vec::new(),
        ir_typecheck_info: Vec::new(),
        ir_typecheck_final: Vec::new(),
        volatile_field_pcs: Vec::new(),
        final_field_pcs: Vec::new(),
        byte_array_field_pcs: Vec::new(),
        referent_load_pcs: Vec::new(),
        ir_ldc_ref_rows: Vec::new(),
        ir_getclass_compares: Vec::new(),
        callee_is_synchronized: false,
        ir_ldc_class_info: Vec::new(),
        ir_handler_rows: Vec::new(),
        ir_indy_rows: Vec::new(),
        ir_indy_capture_rows: Vec::new(),
        ir_indy_capture_sites: Vec::new(),
        ir_lambda_impl_rows: Vec::new(),
        callee_receiver_profile: Vec::new(),
        callee_never_taken_pcs: Vec::new(),
        callee_always_taken_pcs: Vec::new(),
        callee_pruned_switch_edges: Vec::new(),
        callee_cold_arm_ranges: Vec::new(),
        callee_hot_path_pcs: Vec::new(),
        callee_invocations: 0,
        hidden: None,
        cha_class_set: Vec::new(),
        ir_caller_mirror_slot: 0,
        ir_security_walk_ignored: false,
    }
}

// -----------------------------------------------------------------------
// Steps 4 and 5 — a call inside a spliced body, and a splice inside a splice
// -----------------------------------------------------------------------

/// `compile_with_inlines` with the VM-context slot established.
///
/// A spliced call passes the context as `jit_invoke_dispatch`'s first
/// argument, and with `needs_heap == false` there is no slot holding it
/// (`heap_local_offset` is 0, i.e. the saved `rbp`). Harmless for a stub that
/// ignores the pointer, but the tests below should exercise the shape the VM
/// actually compiles.
pub(in crate::x64::tests) fn compile_with_inlines_heap(
    code: &[u8],
    code_len: usize,
    num_params: usize,
    max_locals: usize,
    inline_sites: HashMap<usize, crate::InlineSite>,
) -> Option<CompiledMethod> {
    compile(
        code,
        code_len,
        num_params,
        max_locals,
        true, // needs_heap — the dispatch helper takes the context
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(), // pic_slots
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        inline_sites,
        None, // string_layout
    )
}

/// A `JitInvokeInfo` with the lifetime the emitter's baked immediate needs.
///
/// `try_compile_inner` interns these into the compile's own arena; a unit test
/// at this layer has no arena, so it leaks. One per call, deliberately: the
/// address is what `resolved_invoke_infos` carries and two sites must not
/// alias.
pub(in crate::x64::tests) fn resolved_invoke(
    callee_pc: usize,
    class_name: &'static str,
    method_name: &'static str,
    descriptor: &'static str,
    num_jit_args: usize,
    return_type: u8,
    invoke_kind: u8,
) -> crate::ResolvedInlineInvoke {
    let info: &'static crate::JitInvokeInfo = Box::leak(Box::new(crate::JitInvokeInfo {
        class_name,
        method_name,
        descriptor,
        num_jit_args,
        return_type,
        invoke_kind,
        declaring_class_id: 0,
        owner_class_id: 0,
    }));
    crate::ResolvedInlineInvoke {
        callee_pc,
        info_addr: info as *const crate::JitInvokeInfo as usize, // Cast: address parked in a Send-able plan
        // 0 = "no direct bind", i.e. the dispatch-helper form. The direct form
        // needs a real compiled entry to CALL, which this layer has no way to
        // produce; `a_spliced_direct_call_is_registered_for_keep_alive` covers
        // that half on the data instead.
        direct_entry: 0,
        direct_needs_context: false,
        num_jit_args,
        return_type,
    }
}

pub(in crate::x64::tests) fn take_last_dispatch() -> Option<(String, String, String, Vec<i64>)> {
    LAST_DISPATCH.with(|slot| slot.borrow_mut().take())
}

/// STEP 4. A callee that CALLS is spliceable, and the call it makes reaches the
/// dispatch helper with the right target and the right arguments in the right
/// order.
///
/// This is the whole point of the step: every `invoke*` used to reject the
/// enclosing site outright, so a body like `leaf(a)` below was never a
/// candidate no matter how small it was. Nothing here is about making the CALL
/// cheaper — it is the same `jit_invoke_dispatch` an un-spliced body would use.
///
/// Self-proving three ways, each of which fails on a different mistake:
///   * the return value (15) is wrong if the call is not emitted, or if its
///     result is not pushed as the spliced body's value;
///   * the recorded argument vector `[5, 10]` is wrong — reversed — if the
///     args buffer is built in the wrong direction, which a symmetric argument
///     set would hide;
///   * the recorded name triple is wrong if the emitter baked a different
///     `JitInvokeInfo` than `resolved_invoke_infos` named.
#[test]
pub(in crate::x64::tests) fn a_call_inside_a_spliced_body_reaches_the_dispatch_helper() {
    // callee: `static int leaf(int a) { return target(a, 10); }`
    //   0: iload_0
    //   1: bipush 10
    //   3: invokestatic #3   -> resolved target
    //   6: ireturn
    let callee_body: [u8; 7] = [0x1a, 0x10, 0x0a, 0xb8, 0x00, 0x03, 0xac];
    let mut callee = make_inline_site(&callee_body, 1, 1, true, b'I');
    callee.resolved_invoke_infos = vec![resolved_invoke(
        3,
        "pkg/Target",
        "target",
        "(II)I",
        2,
        b'I',
        3,
    )];

    // caller: `static int f(int a) { return leaf(a); }`
    //   0: iload_0
    //   1: invokestatic #1   -> the inline site
    //   4: ireturn
    let caller: [u8; 7] = [0x1a, 0xb8, 0x00, 0x01, 0xac, 0, 0];
    let mut sites = HashMap::new();
    sites.insert(1, callee);

    let compiled = compile_with_inlines_heap(&caller, 5, 1, 1, sites)
        .expect("a callee containing a call must now splice");

    let _ = take_last_dispatch();
    // SAFETY: JIT-compiled code from valid bytecode in an executable mmap; the
    // dispatch helper is `stub_invoke_dispatch`, which only reads the argument
    // buffer the emitted code just built.
    let got = unsafe { compiled.call_with_heap(0, &[5]) };
    assert_eq!(
        got, 15,
        "the spliced call's result must be the body's value"
    );

    let (class_name, method_name, descriptor, args) =
        take_last_dispatch().expect("the spliced body must have called the dispatch helper");
    assert_eq!(
        (
            class_name.as_str(),
            method_name.as_str(),
            descriptor.as_str()
        ),
        ("pkg/Target", "target", "(II)I"),
        "the emitter must bake the JitInvokeInfo `resolved_invoke_infos` named",
    );
    assert_eq!(
        args,
        vec![5, 10],
        "arg[0] must be at the lowest address — a reversed buffer swaps these",
    );
}

/// Build a reference-returning `InlineSite` for `static Object id(Object o)`
/// — `aload_0; areturn`.
///
/// `make_inline_site` hard-codes an all-`I` descriptor, and the shape these
/// tests need is a REFERENCE result: only a reference is named in an oop map,
/// which is the observable that says which frame slot the splice parked it in.
pub(in crate::x64::tests) fn identity_ref_inline_site() -> crate::InlineSite {
    let mut site = make_inline_site(&[0x2a, 0xb0], 1, 1, true, b'L');
    site.descriptor = "(Ljava/lang/Object;)Ljava/lang/Object;".to_string();
    site
}

/// Compile [`DIRECT_CALL_RESULT_LIVE_AT_SAFEPOINT`] with the pc-1 call either
/// SPLICED or dispatched, and return the oop-map frame slots recorded at the
/// pc-4 safepoint — i.e. where the pc-1 call parked its reference result.
pub(in crate::x64::tests) fn spliced_result_slots_at_pc4(splice_pc1: bool) -> Vec<i16> {
    // LEAK(intentional): compiled code stores raw pointers to these, so they
    // must outlive it; the test process owns them for its (short) lifetime.
    let sink = Box::leak(Box::new(JitInvokeInfo {
        class_name: "T",
        method_name: "sink",
        descriptor: "()V",
        num_jit_args: 0,
        return_type: b'V',
        invoke_kind: 3,
        declaring_class_id: 0,
        owner_class_id: 0,
    }));
    let callee = Box::leak(Box::new(JitInvokeInfo {
        class_name: "T",
        method_name: "id",
        descriptor: "(Ljava/lang/Object;)Ljava/lang/Object;",
        num_jit_args: 1,
        return_type: b'L',
        invoke_kind: 3,
        declaring_class_id: 0,
        owner_class_id: 0,
    }));
    let invoke_info: Vec<(usize, *const JitInvokeInfo)> = vec![
        (1usize, callee as *const JitInvokeInfo),
        (4usize, sink as *const JitInvokeInfo),
    ];
    let mut inline_sites = HashMap::new();
    if splice_pc1 {
        inline_sites.insert(1usize, identity_ref_inline_site());
    }
    let compiled = compile(
        &DIRECT_CALL_RESULT_LIVE_AT_SAFEPOINT,
        DIRECT_CALL_RESULT_LIVE_AT_SAFEPOINT.len(),
        1,
        1,
        true,
        Vec::new(), // multianewarray_info
        Vec::new(), // field_info
        Vec::new(), // typecheck_info
        Vec::new(), // static_field_info
        Vec::new(), // new_info
        Vec::new(), // anewarray_info
        invoke_info,
        Vec::new(), // direct_calls
        Vec::new(), // mic_slots
        Vec::new(), // pic_slots
        Vec::new(), // ldc_info
        Vec::new(), // ldc2w_info
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        inline_sites,
        None, // string_layout
    )
    .expect("a reference-returning leaf callee must compile spliced and unspliced");
    let mut slots = compiled
        .oop_maps
        .iter()
        .find(|m| m.bytecode_pc == 4)
        .map(|m| m.frame_slot_offsets.clone())
        .unwrap_or_default();
    slots.sort_unstable();
    slots
}

/// A spliced callee's result must land at the operand-stack depth the CALLER's
/// bytecode gives it — not at the top of the callee's own frame region.
///
/// `try_emit_inline_body` carves the callee's locals AND a
/// `MAX_INLINE_MERGE_DEPTH` merge region out of the caller's spill area, and
/// does it BEFORE the arguments are popped (it has to — an argument slot stays
/// live until it is marshalled into a callee local). The cursor at the end of
/// the splice therefore sits `callee_locals + MAX_INLINE_MERGE_DEPTH` slots
/// above where the caller's operand stack actually tops out, and the `xreturn`
/// arms used to push the result from THERE.
///
/// Inside the caller's basic block that is invisible: the linear walk writes
/// and reads the same shifted slots and computes the right answer. It becomes
/// wrong code at the first merge point that re-establishes depth from the
/// bytecode — writer and reader then address different slots. That is the
/// `AssertionError: Unexpected operand at stack top` every JSP compile threw
/// once ECJ's `OperandStack.pop(OperandCategory)` tiered up
/// (tomcat/ecj-operandstack-*.md); the same defect had been fixed once in the
/// direct-call arms of `bytecode_walk.rs` on 2026-08-06 and came back through
/// the splicer when `0f55466d0` (2026-08-18) taught it to inline a
/// value-producing branch merge.
///
/// The oop map is the observable, exactly as in
/// [`direct_call_result_slot_is_independent_of_service_arg_reservation`]: the
/// slot a live reference is reported in IS the slot the result was pushed to.
/// Negative control (fix reverted, test kept) reports the spliced arm five
/// slots higher — one callee local plus the four-slot merge region.
#[test]
pub(in crate::x64::tests) fn a_spliced_callees_result_lands_at_the_callers_operand_depth() {
    let dispatched = spliced_result_slots_at_pc4(false);
    let spliced = spliced_result_slots_at_pc4(true);
    assert!(
        !dispatched.is_empty(),
        "the pc-1 reference result must be a mapped live oop at the pc-4 safepoint"
    );
    assert_eq!(
        spliced, dispatched,
        "the splice's callee-locals and merge-region reservations moved the \
         return value off its operand-stack depth: the linear walk and every \
         merge point after this call now disagree about which slot holds it"
    );
}

/// `xor r11d, r11d` (`45 31 DB`): the clear `emit_splice_return_clear_dead_words`
/// opens with. Counted, not located: other arms may emit it too, so the tests
/// below compare the switch-on and switch-off compiles of one shape.
pub(in crate::x64::tests) fn gci_i3il_count_xor_r11(code: &[u8]) -> usize {
    code.windows(3)
        .filter(|w| *w == [0x45u8, 0x31, 0xDB])
        .count()
}

/// Compile `caller` (one splice at pc 1) with
/// `CRATONVM_JIT_SPLICE_ZERO_DEAD_INLINE_REFS` set to `on`.

/// ECJ's `OperandStack.pop(OperandCategory)` reduced to its skeleton, EXECUTED.
///
/// A value produced by an inlined call, held on the operand stack across a
/// `tableswitch`, and compared against a constant pushed at a switch arm — the
/// exact shape whose compiled body compared the raw `TypeBinding.id` against
/// the expected category instead of `TypeIds.getCategory(id)`.
///
/// This one is satisfied by EITHER half of the fix (the splice pushing at the
/// caller's depth, or the switch arms canonicalising the operands that outlive
/// them), and that is deliberate: it asserts the end-to-end behaviour the
/// tomcat page is about, while the two tests above pin the splice half on its
/// own. `tableswitch`/`lookupswitch` were the only branch shapes in this walk
/// that did not establish the canonical layout their own arms are revived with,
/// which is why the shift reached the merge here and not through an `ifeq`.
#[test]
pub(in crate::x64::tests) fn an_inlined_result_held_across_a_tableswitch_reaches_the_merge_intact()
{
    // callee: `static int category(int a) { return a == 1 ? 2 : 1; }`
    //   0: iload_0
    //   1: iconst_1
    //   2: if_icmpne 7
    //   5: iconst_2
    //   6: ireturn
    //   7: iconst_1     <- branch target
    //   8: ireturn
    let callee = make_inline_site(
        &[0x1a, 0x04, 0xa0, 0x00, 0x05, 0x05, 0xac, 0x04, 0xac],
        1,
        1,
        true,
        b'I',
    );

    // caller: `static int f(int a, int sel) {
    //             int cat = category(a);
    //             int k = switch (sel) { case 0 -> 1; case 1 -> 2; default -> 3; };
    //             return cat == k ? 1 : 0; }`
    //
    //    0: iload_0                          [a]
    //    1: invokestatic #1                  [cat]        <- the splice
    //    4: iload_1                          [cat, sel]
    //    5: tableswitch low=0 high=1         [cat]
    //         (6,7 padding; 8 default; 12 low; 16 high; 20 case0; 24 case1)
    //   28: iconst_1                         [cat, 1]     <- case 0
    //   29: goto 37
    //   32: iconst_2                         [cat, 2]     <- case 1
    //   33: goto 37
    //   36: iconst_3                         [cat, 3]     <- default
    //   37: if_icmpeq 42                     []
    //   40: iconst_0
    //   41: ireturn
    //   42: iconst_1
    //   43: ireturn
    let mut caller: Vec<u8> = vec![
        0x1a, // 0  iload_0
        0xb8, 0x00, 0x01, // 1  invokestatic #1
        0x1b, // 4  iload_1
        0xaa, // 5  tableswitch
        0x00, 0x00, // 6,7 padding to the 4-byte boundary
    ];
    caller.extend_from_slice(&(36i32 - 5).to_be_bytes()); //  8 default -> 36
    caller.extend_from_slice(&0i32.to_be_bytes()); // 12 low
    caller.extend_from_slice(&1i32.to_be_bytes()); // 16 high
    caller.extend_from_slice(&(28i32 - 5).to_be_bytes()); // 20 case 0 -> 28
    caller.extend_from_slice(&(32i32 - 5).to_be_bytes()); // 24 case 1 -> 32
    caller.extend_from_slice(&[
        0x04, // 28 iconst_1
        0xa7, 0x00, 0x08, // 29 goto 37
        0x05, // 32 iconst_2
        0xa7, 0x00, 0x04, // 33 goto 37
        0x06, // 36 iconst_3
        0x9f, 0x00, 0x05, // 37 if_icmpeq 42
        0x03, // 40 iconst_0
        0xac, // 41 ireturn
        0x04, // 42 iconst_1
        0xac, // 43 ireturn
    ]);
    let caller_len = caller.len();
    assert_eq!(caller_len, 44, "hand-assembled caller length");
    caller.push(0); // padding, like every other method the JIT sees
    caller.push(0);

    let mut sites = HashMap::new();
    sites.insert(1, callee);
    let compiled = compile_with_inlines(&caller, caller_len, 2, 2, sites)
        .expect("a branchy leaf callee held across a tableswitch must compile");

    // SAFETY: JIT-compiled code from valid bytecode in an executable mmap. The
    // body is arithmetic and branches only — no helper is reached.
    unsafe {
        // a=1 -> cat=2; sel=1 -> k=2; equal.
        // Under the bug the merge reads canonical slot 0, which still holds the
        // `a` the caller pushed at pc 0: 1 vs 2 -> 0.
        assert_eq!(
            compiled.try_call(&[1, 1]).expect("test JIT call"),
            1,
            "the spliced result must be what the tableswitch merge compares"
        );
        // a=1 -> cat=2; sel=0 -> k=1; not equal. Under the bug: 1 vs 1 -> 1.
        assert_eq!(compiled.try_call(&[1, 0]).expect("test JIT call"), 0);
        // a=0 -> cat=1; sel=0 -> k=1; equal. Under the bug: 0 vs 1 -> 0.
        assert_eq!(compiled.try_call(&[0, 0]).expect("test JIT call"), 1);
        // a=3 -> cat=1; sel=9 -> default k=3; not equal.
        assert_eq!(compiled.try_call(&[3, 9]).expect("test JIT call"), 0);
        // a=1 -> cat=2; sel=9 -> default k=3; not equal. Under the bug: 1 vs 3
        // -> 0 (agrees) — kept so the default arm is covered on both callee arms.
        assert_eq!(compiled.try_call(&[1, 9]).expect("test JIT call"), 0);
    }
}

/// A spliced call site with no resolved target must REFUSE the splice.
///
/// The emitter cannot invent a dispatch target: `InlineSite::invoke_targets` is
/// resolved against the CALLEE's constant pool, and the enclosing method's
/// `invoke_info` is keyed by CALLER pc — a different bytecode space, where the
/// same integer names an unrelated call. Guessing there is how you get a body
/// that calls the wrong method.
///
/// Same site as the test above with `resolved_invoke_infos` emptied. Refusing
/// is not the same as failing: the compile still succeeds (the site falls back
/// to whatever the caller's own pc resolves to, which in this harness is
/// nothing), and what must NOT appear is a dispatch emitted against a target
/// the emitter does not have. A splice that ran anyway would have to bake some
/// address as `jit_invoke_dispatch`'s second argument, and every address
/// available to it here is wrong.
#[test]
pub(in crate::x64::tests) fn a_spliced_call_with_no_resolved_target_refuses_the_splice() {
    let callee_body: [u8; 7] = [0x1a, 0x10, 0x0a, 0xb8, 0x00, 0x03, 0xac];
    // resolved_invoke_infos deliberately left empty.
    let callee = make_inline_site(&callee_body, 1, 1, true, b'I');

    let caller: [u8; 7] = [0x1a, 0xb8, 0x00, 0x01, 0xac, 0, 0];
    let mut sites = HashMap::new();
    sites.insert(1, callee);

    let compiled = compile_with_inlines_heap(&caller, 5, 1, 1, sites)
        .expect("refusing the splice must not fail the whole compile");
    assert_eq!(
        calls_to(&compiled, test_helpers().invoke_dispatch),
        0,
        "the callee's call must not be emitted against an un-interned target",
    );
}

/// `invokeinterface` is FIVE bytes inside a spliced body, like everywhere else.
///
/// The other three invoke forms are three. Advancing by three over an
/// `invokeinterface` lands the walk on its `count` operand, and a `count` of 2
/// decodes as `iconst_m1` (0x02) — so the body would push -1, `nop` over the
/// trailing zero, and return -1 instead of the call's result. Silent wrong
/// answer, no bail, no diagnostic: exactly the failure mode the width guard
/// exists for.
#[test]
pub(in crate::x64::tests) fn an_invokeinterface_inside_a_splice_is_five_bytes_wide() {
    // callee: `static int leaf(int a) { return iface.m(a, 10); }` — shaped so
    // the operands are the same two the test above uses.
    //   0: iload_0
    //   1: bipush 10
    //   3: invokeinterface #3, count=2, 0
    //   8: ireturn
    let callee_body: [u8; 9] = [0x1a, 0x10, 0x0a, 0xb9, 0x00, 0x03, 0x02, 0x00, 0xac];
    let mut callee = make_inline_site(&callee_body, 1, 1, true, b'I');
    callee.resolved_invoke_infos = vec![resolved_invoke(3, "pkg/Iface", "m", "(II)I", 2, b'I', 2)];

    let caller: [u8; 7] = [0x1a, 0xb8, 0x00, 0x01, 0xac, 0, 0];
    let mut sites = HashMap::new();
    sites.insert(1, callee);

    let compiled = compile_with_inlines_heap(&caller, 5, 1, 1, sites)
        .expect("an invokeinterface-carrying callee must splice");

    let _ = take_last_dispatch();
    // SAFETY: as above.
    let got = unsafe { compiled.call_with_heap(0, &[5]) };
    assert_eq!(
        got, 15,
        "a 3-byte advance would decode the count operand as iconst_m1 and return -1",
    );
    assert!(
        take_last_dispatch().is_some(),
        "the interface call must still have gone through the dispatch helper",
    );
}

/// STEP 5. A call inside a spliced body that is itself spliced emits NO call.
///
/// The nested body computes `a - b`, while the dispatch stub returns `a + b`.
/// The two disagree for the arguments used, so the returned value alone says
/// which path ran — an arithmetic body that agreed with the stub (an `iadd`)
/// would have made this test pass whether nesting worked or not.
#[test]
pub(in crate::x64::tests) fn a_nested_splice_replaces_the_call_entirely() {
    // innermost: `static int inner(int a, int b) { return a - b; }`
    //   0: iload_0; 1: iload_1; 2: isub; 3: ireturn
    let inner = make_inline_site(&[0x1a, 0x1b, 0x64, 0xac], 2, 2, true, b'I');

    // middle: `static int leaf(int a) { return inner(a, 10); }`
    let callee_body: [u8; 7] = [0x1a, 0x10, 0x0a, 0xb8, 0x00, 0x03, 0xac];
    let mut callee = make_inline_site(&callee_body, 1, 1, true, b'I');
    // ADDITIVE by design: the pc carries both a nested body and a dispatch
    // target, so a nested bail falls back to the call instead of failing the
    // outer splice. The next test relies on exactly this.
    callee.resolved_invoke_infos = vec![resolved_invoke(
        3,
        "pkg/Inner",
        "inner",
        "(II)I",
        2,
        b'I',
        3,
    )];
    callee.nested_sites = vec![crate::NestedInlineSite {
        callee_pc: 3,
        guard_class_id: 0,
        site: inner,
        bimorphic_second: None,
    }]
    .into();

    let caller: [u8; 7] = [0x1a, 0xb8, 0x00, 0x01, 0xac, 0, 0];
    let mut sites = HashMap::new();
    sites.insert(1, callee);

    let compiled =
        compile_with_inlines_heap(&caller, 5, 1, 1, sites).expect("a nested splice must compile");

    let _ = take_last_dispatch();
    // SAFETY: as above.
    let got = unsafe { compiled.call_with_heap(0, &[5]) };
    assert_eq!(got, -5, "5 - 10; the dispatch stub would have answered 15");
    assert!(
        take_last_dispatch().is_none(),
        "a nested splice must emit no dispatch at all",
    );
    assert_eq!(
        calls_to(&compiled, test_helpers().invoke_dispatch),
        0,
        "and no CALL to the helper may be left in the code",
    );
}

/// A nested splice that BAILS falls back to the ordinary call.
///
/// This is why `nested_sites` and `resolved_invoke_infos` both carry the pc.
/// If a nested body could only succeed or fail the outer splice, one
/// unmodellable opcode three levels down would cost the whole chain.
///
/// `arraylength` (0xbe) is the bail: the inline mini-emitter has no arm for it
/// (bounds-checked array access is refused at this layer), so the nested
/// attempt rolls back and the pc takes its dispatch entry — answering 15 (the
/// stub's sum) rather than the nested body's -5.
#[test]
pub(in crate::x64::tests) fn a_nested_splice_that_bails_falls_back_to_the_call() {
    // innermost, but unspliceable: `arraylength` has no inline arm.
    let inner = make_inline_site(&[0x1a, 0x1b, 0xbe, 0xac], 2, 2, true, b'I');

    let callee_body: [u8; 7] = [0x1a, 0x10, 0x0a, 0xb8, 0x00, 0x03, 0xac];
    let mut callee = make_inline_site(&callee_body, 1, 1, true, b'I');
    callee.resolved_invoke_infos = vec![resolved_invoke(
        3,
        "pkg/Inner",
        "inner",
        "(II)I",
        2,
        b'I',
        3,
    )];
    callee.nested_sites = vec![crate::NestedInlineSite {
        callee_pc: 3,
        guard_class_id: 0,
        site: inner,
        bimorphic_second: None,
    }]
    .into();

    let caller: [u8; 7] = [0x1a, 0xb8, 0x00, 0x01, 0xac, 0, 0];
    let mut sites = HashMap::new();
    sites.insert(1, callee);

    let compiled = compile_with_inlines_heap(&caller, 5, 1, 1, sites)
        .expect("the outer splice must survive a nested bail");

    let _ = take_last_dispatch();
    // SAFETY: as above.
    let got = unsafe { compiled.call_with_heap(0, &[5]) };
    assert_eq!(
        got, 15,
        "the nested bail must fall back to the dispatch call"
    );
    let (_, _, _, args) = take_last_dispatch().expect("the fallback dispatch must have run");
    assert_eq!(
        args,
        vec![5, 10],
        "with the same arguments the nested body would have had"
    );
}

/// A VALUE-PRODUCING BRANCH MERGE splices, and both paths answer correctly.
///
/// This is the shape that blocked the whole netty inlining line of work.
/// `AssertionUtils.objectsAreEqual` — the first rung past `assertEquals` — is
/// exactly it:
///
/// ```text
///    8: iconst_1     9: goto 13    12: iconst_0    13: ireturn
/// ```
///
/// Two paths reach pc 13 with the same value in different places, and the
/// emitter's symbolic operand stack can only name one, so commit 419a6f5
/// refused every such body outright. Measured 2026-08-18, that refusal is what
/// made `nested-splice=0` and `outer-splice-rolled-back=2`: the call the
/// devirtualisation work was aimed at, one instruction later at pc 16, was
/// never emitted at all.
///
/// One compiled body, both paths, and the two answers differ — which is the
/// only way to show the merge carries a VALUE rather than happening to agree.
#[test]
pub(in crate::x64::tests) fn a_value_producing_branch_merge_splices_and_both_paths_are_right() {
    // callee: `static int f(int a) { return a != 0 ? 0 : 1; }`, emitted as the
    // objectsAreEqual diamond.
    //   0: iload_0
    //   1: ifne 8
    //   4: iconst_1
    //   5: goto 9
    //   8: iconst_0
    //   9: ireturn      <-- merge, one value live
    let callee = make_inline_site(
        &[0x1a, 0x9a, 0x00, 0x07, 0x04, 0xa7, 0x00, 0x04, 0x03, 0xac],
        1,
        1,
        true,
        b'I',
    );

    // caller: `static int g(int a) { return f(a); }`
    let caller: [u8; 7] = [0x1a, 0xb8, 0x00, 0x01, 0xac, 0, 0];
    let mut sites = HashMap::new();
    sites.insert(1, callee);

    let compiled = compile_with_inlines_heap(&caller, 5, 1, 1, sites)
        .expect("a diamond-shaped callee must now splice");

    // SAFETY: JIT-compiled code from valid bytecode in an executable mmap.
    unsafe {
        assert_eq!(
            compiled.call_with_heap(0, &[0]),
            1,
            "fall-through path: iconst_1 must survive the goto to the merge",
        );
        assert_eq!(
            compiled.call_with_heap(0, &[7]),
            0,
            "taken path: iconst_0 must be what the merge reads",
        );
    }
    assert_eq!(
        calls_to(&compiled, test_helpers().invoke_dispatch),
        0,
        "and the body must be spliced, not called",
    );
}

/// The two paths leave the value in DIFFERENT slots, which is what the merge
/// region is actually for — and what makes the spill's ORDER observable.
///
/// The other two merge tests do not discriminate that order, and it is worth
/// saying why rather than leaving it implied: in a constant-vs-constant
/// diamond both paths push into the same operand slot (each starts from an
/// empty callee stack and the emitter allocates deterministically from
/// `save_spill`), so re-running one path's store on the other path reads the
/// slot that path already wrote. Idempotent — correct by accident. Verified by
/// mutation: recording the label BEFORE the fall-through spill leaves both of
/// them passing.
///
/// `iload` is the discriminator. It pushes `StackSlot::Frame(local_offset)` —
/// the LOCAL's slot, not a fresh operand slot — so here the taken path's value
/// lives in the merge region's source at one address and the fall-through's at
/// another. If the fall-through's stores are emitted after the label, the
/// branch path re-runs them against a slot it never wrote.
#[test]
pub(in crate::x64::tests) fn a_merge_whose_paths_use_different_slots_pins_the_spill_order() {
    // callee: `static int f(int a) { return a != 0 ? a : 1; }`
    //   0: iload_0
    //   1: ifeq 8        (a == 0 -> 8)
    //   4: iload_0       <-- pushes the LOCAL's slot
    //   5: goto 9
    //   8: iconst_1      <-- pushes a fresh operand slot
    //   9: ireturn       <-- merge; the two sources are different addresses
    let callee = make_inline_site(
        &[0x1a, 0x99, 0x00, 0x07, 0x1a, 0xa7, 0x00, 0x04, 0x04, 0xac],
        1,
        1,
        true,
        b'I',
    );

    let caller: [u8; 7] = [0x1a, 0xb8, 0x00, 0x01, 0xac, 0, 0];
    let mut sites = HashMap::new();
    sites.insert(1, callee);

    let compiled = compile_with_inlines_heap(&caller, 5, 1, 1, sites)
        .expect("a merge over two different slots must splice");

    // SAFETY: JIT-compiled code from valid bytecode in an executable mmap.
    unsafe {
        assert_eq!(
            compiled.call_with_heap(0, &[5]),
            5,
            "the branch path must read the local it stored, not the \
             fall-through's operand slot",
        );
        assert_eq!(compiled.call_with_heap(0, &[0]), 1);
    }
}

/// Two values live across the merge, and two merges at different depths in one
/// body.
///
/// Depth 1 is the common case and could pass by accident — a single value that
/// both paths happen to leave in the same slot proves nothing about the
/// canonical-home machinery. Here `iconst_5` stays live UNDER the diamond, so
/// the `goto`'s merge carries two values while the branch target above it
/// carries one; the two depths must be tracked per target rather than globally.
#[test]
pub(in crate::x64::tests) fn two_merges_at_different_depths_in_one_body() {
    //   0: iconst_5          [5]
    //   1: iload_0
    //   2: ifne 9            -> target 9 with depth 1
    //   5: iconst_1          [5, 1]
    //   6: goto 10           -> target 10 with depth 2
    //   9: iconst_0          [5, 0]
    //  10: iadd              <-- merge, depth 2
    //  11: ireturn
    let callee = make_inline_site(
        &[
            0x08, 0x1a, 0x9a, 0x00, 0x07, 0x04, 0xa7, 0x00, 0x04, 0x03, 0x60, 0xac,
        ],
        1,
        1,
        true,
        b'I',
    );

    let caller: [u8; 7] = [0x1a, 0xb8, 0x00, 0x01, 0xac, 0, 0];
    let mut sites = HashMap::new();
    sites.insert(1, callee);

    let compiled =
        compile_with_inlines_heap(&caller, 5, 1, 1, sites).expect("a two-deep merge must splice");

    // SAFETY: as above.
    unsafe {
        assert_eq!(compiled.call_with_heap(0, &[0]), 6, "5 + 1");
        assert_eq!(compiled.call_with_heap(0, &[7]), 5, "5 + 0");
    }
}

/// DEVIRTUALISATION INSIDE A SPLICE. Both edges of the receiver guard, from one
/// compiled body.
///
/// The hot edge is a spliced body that returns 42; the cold edge is the
/// ordinary call, which this harness answers with the argument sum. So the
/// SAME machine code returns 42 for a receiver whose class id matches the
/// guard and takes the dispatch for one that does not — which is the only way
/// to show that the guard is a guard and not a constant.
///
/// What each assertion would catch:
///   * hit returning something other than 42 — the guard fell through to the
///     call, or the spliced body was never emitted;
///   * a recorded dispatch on the hit path — the JMP over the miss edge is
///     missing, so both arms run;
///   * miss NOT recording a dispatch — the guard is never false, i.e. the
///     class-id compare is against the wrong operand or the wrong offset;
///   * the miss dispatch seeing arguments other than `[receiver, 10]` — the
///     miss arm did not pop what the hit arm popped, which is the operand-stack
///     disagreement the emitter restores its symbolic state to prevent.
#[test]
pub(in crate::x64::tests) fn a_guarded_nested_splice_takes_the_body_on_a_hit_and_the_call_on_a_miss(
) {
    const GUARD_CLASS_ID: u32 = 0x4242;

    // The devirtualised target: `int m(int) { return 42; }` — a constant, so
    // the returned value alone says which arm ran.
    let inner = make_inline_site(&[0x10, 0x2a, 0xac], 2, 2, false, b'I');

    // The spliced body: `static int leaf(Object o) { return o.m(10); }`
    //   0: aload_0
    //   1: bipush 10
    //   3: invokevirtual #3
    //   6: ireturn
    let mut callee = make_inline_site(
        &[0x2a, 0x10, 0x0a, 0xb6, 0x00, 0x03, 0xac],
        1,
        1,
        true,
        b'I',
    );
    // A guarded pc KEEPS its dispatch entry: the miss edge has to go
    // somewhere, and a virtual site has no direct bind to send it to.
    callee.resolved_invoke_infos = vec![resolved_invoke(3, "pkg/T", "m", "(I)I", 2, b'I', 0)];
    callee.nested_sites = vec![crate::NestedInlineSite {
        callee_pc: 3,
        guard_class_id: GUARD_CLASS_ID,
        site: inner,
        bimorphic_second: None,
    }]
    .into();

    let caller: [u8; 7] = [0x2a, 0xb8, 0x00, 0x01, 0xac, 0, 0];
    let mut sites = HashMap::new();
    sites.insert(1, callee);

    let compiled = compile_with_inlines_heap(&caller, 5, 1, 1, sites)
        .expect("a guarded nested splice must compile");

    // Two receivers, one compiled body. Class id lives in the first four bytes
    // of the object header, which is what `CMP [rax+0], imm32` reads.
    let mut hit = Box::new([0u64; 8]);
    let mut miss = Box::new([0u64; 8]);
    hit[0] = GUARD_CLASS_ID as u64;
    miss[0] = (GUARD_CLASS_ID + 1) as u64;
    let hit_addr = hit.as_mut_ptr() as i64; // Cast: receiver address
    let miss_addr = miss.as_mut_ptr() as i64; // Cast: receiver address

    let _ = take_last_dispatch();
    // SAFETY: JIT-compiled code from valid bytecode in an executable mmap; the
    // receiver is a live 64-byte buffer shaped like an object header, and the
    // only helper reachable is `stub_invoke_dispatch`, which reads the argument
    // buffer and nothing else.
    let got_hit = unsafe { compiled.call_with_heap(0, &[hit_addr]) };
    assert_eq!(got_hit, 42, "a guard hit must run the spliced body");
    assert!(
        take_last_dispatch().is_none(),
        "and must not also fall into the miss edge",
    );

    // SAFETY: as above.
    let got_miss = unsafe { compiled.call_with_heap(0, &[miss_addr]) };
    let (_, _, _, args) = take_last_dispatch().expect("a guard miss must take the ordinary call");
    assert_eq!(
        args,
        vec![miss_addr, 10],
        "the miss arm must pop exactly the operands the hit arm popped",
    );
    assert_eq!(
        got_miss,
        miss_addr.wrapping_add(10),
        "and its result must be the call's, not the body's",
    );
}

/// A null receiver fails the guard rather than dereferencing it.
///
/// `CMP DWORD [RAX+0], guard` on a null receiver is a segfault, so the null
/// test has to come FIRST and branch to the same miss edge. Nothing else in
/// this construct would catch that: a null receiver is exactly the case a
/// profile never records, and the ordinary call reproduces the NPE correctly.
#[test]
pub(in crate::x64::tests) fn a_null_receiver_takes_the_guarded_splices_miss_edge() {
    const GUARD_CLASS_ID: u32 = 0x4242;
    let inner = make_inline_site(&[0x10, 0x2a, 0xac], 2, 2, false, b'I');
    let mut callee = make_inline_site(
        &[0x2a, 0x10, 0x0a, 0xb6, 0x00, 0x03, 0xac],
        1,
        1,
        true,
        b'I',
    );
    callee.resolved_invoke_infos = vec![resolved_invoke(3, "pkg/T", "m", "(I)I", 2, b'I', 0)];
    callee.nested_sites = vec![crate::NestedInlineSite {
        callee_pc: 3,
        guard_class_id: GUARD_CLASS_ID,
        site: inner,
        bimorphic_second: None,
    }]
    .into();

    let caller: [u8; 7] = [0x2a, 0xb8, 0x00, 0x01, 0xac, 0, 0];
    let mut sites = HashMap::new();
    sites.insert(1, callee);
    let compiled = compile_with_inlines_heap(&caller, 5, 1, 1, sites)
        .expect("a guarded nested splice must compile");

    let _ = take_last_dispatch();
    // SAFETY: JIT-compiled code from valid bytecode; a null receiver must not
    // be dereferenced by the guard, which is the property under test.
    let got = unsafe { compiled.call_with_heap(0, &[0]) };
    let (_, _, _, args) =
        take_last_dispatch().expect("a null receiver must reach the ordinary call, not the body");
    assert_eq!(args, vec![0, 10]);
    assert_eq!(got, 10);
}

/// The interning pass fills `resolved_invoke_infos` from `invoke_targets`, for
/// nested bodies as well as the top one.
///
/// A nested body whose calls were left uninterned would bail at the emitter's
/// "no resolved target" arm and quietly degrade to a dispatch — a regression
/// that costs only speed, and so would never fail a correctness test. Asserted
/// on the DATA rather than on emitted code, because that is where the bug
/// would live.
#[test]
pub(in crate::x64::tests) fn interning_reaches_nested_bodies_too() {
    let mut inner = make_inline_site(&[0x1a, 0x1b, 0x60, 0xac], 2, 2, true, b'I');
    inner.invoke_targets = vec![(
        1,
        crate::InlineInvokeTarget {
            class_name: "pkg/Deep".to_string(),
            method_name: "deep".to_string(),
            descriptor: "()I".to_string(),
            num_jit_args: 0,
            return_type: b'I',
            invoke_kind: 3,
            declaring_class_id: 7,
            target_class_id: 7,
            resolved_class_id: 0,
            owner_class_id: 0,
            direct_entry: None,
        },
    )];
    let mut outer = make_inline_site(&[0x1a, 0xac], 1, 1, true, b'I');
    outer.invoke_targets = vec![(
        0,
        crate::InlineInvokeTarget {
            class_name: "pkg/Inner".to_string(),
            method_name: "inner".to_string(),
            descriptor: "(II)I".to_string(),
            num_jit_args: 2,
            return_type: b'I',
            invoke_kind: 3,
            declaring_class_id: 9,
            target_class_id: 9,
            resolved_class_id: 0,
            owner_class_id: 0,
            direct_entry: Some((0xfeed_0000, true)),
        },
    )];
    outer.nested_sites = vec![crate::NestedInlineSite {
        callee_pc: 0,
        guard_class_id: 0,
        site: inner,
        bimorphic_second: None,
    }]
    .into();
    // Stale pointers from a hypothetical earlier compile must be REPLACED, not
    // appended to: an `InlineSite` can be cloned out of a cached plan.
    outer.resolved_invoke_infos = vec![crate::ResolvedInlineInvoke {
        callee_pc: 999,
        info_addr: 0xdead_beef,
        ..Default::default()
    }];

    let mut strings: Vec<Box<str>> = Vec::new();
    let mut infos: Vec<Box<crate::JitInvokeInfo>> = Vec::new();
    // `(entry, epoch)`: the pin gained an epoch when a baked direct call turned
    // out to be pinned by ADDRESS alone and the address had changed hands
    // (b56bb79b9). This call site was not updated with it, which left the whole
    // `cratonvm-jit` lib test binary failing to COMPILE on dev — so every test
    // in it, not just this one, was silently unrunnable.
    let mut direct_entries: Vec<(usize, u64)> = Vec::new();
    crate::intern_inline_invoke_targets(&mut outer, &mut strings, &mut infos, &mut direct_entries);

    assert_eq!(outer.resolved_invoke_infos.len(), 1);
    assert_eq!(outer.resolved_invoke_infos[0].callee_pc, 0);
    let deep = &outer.nested_sites[0].site;
    assert_eq!(
        deep.resolved_invoke_infos.len(),
        1,
        "a nested body's own calls must be interned too",
    );
    // SAFETY: the pointee is `infos[1]`, alive for the rest of this test.
    let deep_info =
        unsafe { &*(deep.resolved_invoke_infos[0].info_addr as *const crate::JitInvokeInfo) };
    assert_eq!(deep_info.class_name, "pkg/Deep");
    assert_eq!(deep_info.declaring_class_id, 7);
    // Two infos (`intern_jit_invoke_str` returns `&'static str` without pushing to `strings`).
    assert_eq!(infos.len(), 2);

    // THE KEEP-ALIVE. A baked direct-call address that never reaches
    // `_direct_callee_entries` is a use-after-free with no symptom until the
    // callee tiers up: nothing pins the callee artifact
    // (`prepare_for_publication`) and the invalidation reverse closure has no
    // way to find this caller. The outer target carries one bind, the nested
    // one carries none, so exactly one address must come out — and it must be
    // the one that was bound, not a placeholder.
    // The ADDRESS is what this test is about; the epoch beside it is whatever
    // `jit_entry_artifact_id` answers for a fabricated address, which is not
    // this test's business. Assert the addresses.
    assert_eq!(
        direct_entries.iter().map(|&(e, _)| e).collect::<Vec<_>>(),
        vec![0xfeed_0000],
        "every baked direct-call entry must be registered for keep-alive",
    );
    assert_eq!(
        outer.resolved_invoke_infos[0].direct_entry, 0xfeed_0000,
        "and the emitter must be handed the same address that was registered",
    );
    assert!(outer.resolved_invoke_infos[0].direct_needs_context);
    assert_eq!(
        deep.resolved_invoke_infos[0].direct_entry, 0,
        "a target with no bind stays on the dispatch form",
    );
}

#[test]
pub(in crate::x64::tests) fn inline_ctor_fresh_store_proof_rejects_repeated_and_non_ctor_writes() {
    let mut site = make_inline_site(
        &[
            0xb5, 0x00, 0x01, // pc 0: first write of field 0
            0xb5, 0x00, 0x01, // pc 3: repeated write of field 0
            0xb5, 0x00, 0x02, // pc 6: first write of field 1
            0xb1,
        ],
        3,
        3,
        false,
        b'V',
    );
    site.method_name = "<init>".to_string();
    site.field_info = vec![(0, 0, b'L'), (3, 0, b'L'), (6, 1, b'L')];

    assert!(inline_site_is_fresh_ctor_first_store(&site, 0, 0));
    assert!(!inline_site_is_fresh_ctor_first_store(&site, 3, 0));
    assert!(inline_site_is_fresh_ctor_first_store(&site, 6, 1));

    site.method_name = "setFields".to_string();
    assert!(!inline_site_is_fresh_ctor_first_store(&site, 0, 0));
}

/// `inline_ctor_store_starts_from_null` holds for `this.f = <local>` in a
/// branch-free constructor prefix and for nothing weaker: another receiver, an
/// intervening call, a branch, a reassigned `this`, or a second store.
#[test]
pub(in crate::x64::tests) fn a_ctor_store_starts_from_null_only_through_this_with_nothing_before_it(
) {
    use crate::x64::licm::inline_ctor_store_starts_from_null as from_null;
    let site_for = |code: &[u8], fields: Vec<(usize, usize, u8)>, elided: Vec<usize>| {
        let mut site = make_inline_site(code, 3, 3, false, b'V');
        site.method_name = "<init>".to_string();
        site.field_info = fields;
        site.elided_invoke_pcs = elided;
        site
    };
    // Node(Node l, Node r) { super(); left = l; right = r; }
    let node = [
        0x2a, 0xb7, 0x00, 0x01, // 0: aload_0; 1: invokespecial Object.<init> (elided)
        0x2a, 0x2b, 0xb5, 0x00, 0x07, // 4: aload_0; 5: aload_1; 6: putfield left
        0x2a, 0x2c, 0xb5, 0x00, 0x0d, // 9: aload_0; 10: aload_2; 11: putfield right
        0xb1,
    ];
    let fields = vec![(6, 0, b'L'), (11, 1, b'L')];
    let site = site_for(&node, fields.clone(), vec![1]);
    assert!(from_null(&site, 6, 0));
    assert!(from_null(&site, 11, 1));

    // The super call not proven a no-op: it could have written the field.
    assert!(!from_null(&site_for(&node, fields.clone(), vec![]), 6, 0));

    // A static callee is not a constructor body.
    let mut st = site_for(&node, fields.clone(), vec![1]);
    st.callee_is_static = true;
    assert!(!from_null(&st, 6, 0));

    // `l.left = null` — the receiver is not `this`.
    let other = [0x2b, 0x01, 0xb5, 0x00, 0x07, 0xb1];
    assert!(!from_null(
        &site_for(&other, vec![(2, 0, b'L')], vec![]),
        2,
        0
    ));
    // `this.left = null` — the same shape through `this` holds.
    let this_null = [0x2a, 0x01, 0xb5, 0x00, 0x07, 0xb1];
    assert!(from_null(
        &site_for(&this_null, vec![(2, 0, b'L')], vec![]),
        2,
        0
    ));
    // `aload 2` as the value (the two-byte form).
    let wide_load = [0x2a, 0x19, 0x02, 0xb5, 0x00, 0x07, 0xb1];
    assert!(from_null(
        &site_for(&wide_load, vec![(3, 0, b'L')], vec![]),
        3,
        0
    ));

    // A value that is not a single push: `this.left = l.left`.
    let chained = [0x2a, 0x2b, 0xb4, 0x00, 0x07, 0xb5, 0x00, 0x07, 0xb1];
    assert!(!from_null(
        &site_for(&chained, vec![(2, 0, b'L'), (5, 0, b'L')], vec![]),
        5,
        0
    ));

    // A call before the store — `helper(); left = l;` — may have set it.
    let call_first = [
        0x2a, 0xb6, 0x00, 0x03, // aload_0; invokevirtual this.helper()
        0x2a, 0x2b, 0xb5, 0x00, 0x07, 0xb1,
    ];
    assert!(!from_null(
        &site_for(&call_first, vec![(6, 0, b'L')], vec![]),
        6,
        0
    ));

    // A branch before the store.
    let branch = [
        0x2b, 0xc6, 0x00, 0x03, // aload_1; ifnull +3 (to pc 4)
        0x2a, 0x2b, 0xb5, 0x00, 0x07, 0xb1,
    ];
    assert!(!from_null(
        &site_for(&branch, vec![(6, 0, b'L')], vec![]),
        6,
        0
    ));

    // `this` reassigned: `aload_1; astore_0; aload_0; aload_2; putfield`.
    let reassigned = [0x2b, 0x4b, 0x2a, 0x2c, 0xb5, 0x00, 0x07, 0xb1];
    assert!(!from_null(
        &site_for(&reassigned, vec![(4, 0, b'L')], vec![]),
        4,
        0
    ));
    let reassigned_long = [0x2b, 0x3a, 0x00, 0x2a, 0x2c, 0xb5, 0x00, 0x07, 0xb1];
    assert!(!from_null(
        &site_for(&reassigned_long, vec![(5, 0, b'L')], vec![]),
        5,
        0
    ));

    // The second store to a field starts from the first one's value.
    let twice = [
        0x2a, 0x2b, 0xb5, 0x00, 0x07, // left = l
        0x2a, 0x01, 0xb5, 0x00, 0x07, // left = null
        0xb1,
    ];
    assert!(!from_null(
        &site_for(&twice, vec![(2, 0, b'L'), (7, 0, b'L')], vec![]),
        7,
        0
    ));
}

/// The G1 shape of an inlined constructor's reference store, with the caller's
/// half of the proof (a non-constructor method key) given and withheld.
///
/// Given it, the store takes the constructor arm behind a null test and a
/// null-value skip, and no longer reads the region table. Withheld — an empty
/// key, or a caller that is itself `<init>` — or with the switch off, the bytes
/// are exactly the old ones.
#[test]
pub(in crate::x64::tests) fn a_new_receivers_ctor_store_skips_null_and_the_region_table_under_g1() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    // Locals, not statics: every table is only read while compiling, inside
    // this function (and a `static` here would cost a line of
    // `process_global_statics_ratchet`).
    let g1_table: [AtomicUsize; 5] = Default::default();
    // The store-side table G1 never publishes: non-zero address, zero content.
    let bounds: [AtomicUsize; 6] = Default::default();
    // The read-side table the containment check bakes the address of.
    let read_bounds: [AtomicUsize; 6] = Default::default();
    g1_table[0].store(0x4000_0000, Ordering::Release);
    g1_table[1].store(0x80_0000, Ordering::Release);
    g1_table[2].store(!(0x10_0000usize - 1), Ordering::Release);

    let mut helpers = test_helpers();
    helpers.g1_barrier_addr = g1_table.as_ptr() as usize; // Cast: table address
    helpers.g1_post_write_barrier = 0x1234_5678;
    helpers.region_bounds_addr = bounds.as_ptr() as usize; // Cast: table address
    helpers.read_bounds_addr = read_bounds.as_ptr() as usize; // Cast: table address
    helpers.putfield_object = 0x2345_6789;

    // Caller: static void f(Object o, Object v) { o.<init>(v); }
    let caller: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0x2b, // 1: aload_1
        0xb7, 0x00, 0x01, // 2: invokespecial #1 → inline site
        0xb1, // 5: return
        0, 0,
    ];
    // Callee: <init>(Object v) { this.f = v; }
    let make_site = || {
        let mut site = make_inline_site(&[0x2a, 0x2b, 0xb5, 0x00, 0x01, 0xb1], 2, 2, false, b'V');
        site.method_name = "<init>".to_string();
        site.descriptor = "(Ljava/lang/Object;)V".to_string();
        site.field_info = vec![(2usize, 0usize, b'L')];
        site.compact_field_info = vec![(2usize, 8u32, true)];
        site.needs_heap = true;
        let mut sites = HashMap::new();
        sites.insert(2usize, site);
        sites
    };
    let compile_it = |key: &str| {
        compile_with_param_slots(
            &crate::compile_gate::CompileAdmission::for_backend_test(),
            &caller,
            6,
            2,
            2,
            true,
            Vec::new(),
            Vec::new(), // field_info
            Vec::new(), // typecheck_info
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),         // mic_slots
            Vec::new(),         // pic_slots
            Vec::new(),         // ldc_info
            Vec::new(),         // ldc_string_info
            Vec::new(),         // ldc_class_info
            Vec::new(),         // ldc2w_info
            Default::default(), // ldc_fp_pcs
            HashMap::new(),
            HashMap::new(),
            &helpers,
            std::collections::HashSet::new(),
            make_site(),
            HashMap::new(), // inline_guard_variants (PGO-02)
            None,           // string_layout
            &[],
            0,
            0b11, // param_oop_mask: both parameters are references
            Vec::new(),
            key,
            None, // despec: no VM
            Vec::new(),
            None, // elidable_init_pcs
            crate::x64::BackendRequest::default(),
        )
        .expect("inlined <init> reference putfield must compile")
        .code_bytes()
        .to_vec()
    };
    let has = |code: &[u8], needle: &[u8]| code.windows(needle.len()).any(|w| w == needle);
    let read_table = (read_bounds.as_ptr() as usize as u64).to_le_bytes(); // Cast: baked imm64

    crate::x64::licm::G1_INLINE_BARRIER_FORCED.with(|c| c.set(Some(true)));
    let key = "T.f:(Ljava/lang/Object;Ljava/lang/Object;)V";
    crate::x64::licm::CTOR_STORE_FROM_NULL_FORCE.with(|c| c.set(Some(false)));
    let off = compile_it(key);
    // The old arm for an unkeyed body: a body without a method label stamps
    // its throw sites without the method token (`emit_throw_stamp_arg`), so
    // its length is compared against an unkeyed old-arm compile.
    let off_unkeyed = compile_it("");
    crate::x64::licm::CTOR_STORE_FROM_NULL_FORCE.with(|c| c.set(Some(true)));
    let on = compile_it(key);
    let ctor_caller = compile_it("T.<init>:(Ljava/lang/Object;Ljava/lang/Object;)V");
    let unkeyed = compile_it("");
    crate::x64::licm::CTOR_STORE_FROM_NULL_FORCE.with(|c| c.set(None));
    crate::x64::licm::G1_INLINE_BARRIER_FORCED.with(|c| c.set(None));

    assert!(
        has(&off, &read_table),
        "the old arm checks containment against the read table"
    );
    assert!(
        !has(&on, &read_table),
        "a `new` receiver needs no containment check"
    );
    assert!(on.len() < off.len(), "on={} off={}", on.len(), off.len());
    // Not byte-compared: the prologue bakes a per-method counter address, so
    // two keys never produce identical bytes. The shape is what must match.
    for (other, old_arm, why) in [
        (
            &ctor_caller,
            &off,
            "a constructor caller's receiver may be `this`",
        ),
        (&unkeyed, &off_unkeyed, "an unknown caller proves nothing"),
    ] {
        assert!(has(other, &read_table), "{why}");
        assert_eq!(other.len(), old_arm.len(), "{why}");
    }
    // The null skip: the value loaded into RDX, `test rdx,rdx`, `jz`.
    let test_rdx_jz = [0x48, 0x85, 0xd2, 0x0f, 0x84];
    let count = |code: &[u8]| {
        code.windows(test_rdx_jz.len())
            .filter(|w| *w == test_rdx_jz)
            .count()
    };
    assert_eq!(
        count(&on),
        count(&off) + 1,
        "exactly one new `test rdx,rdx; jz`"
    );
}

#[test]
pub(in crate::x64::tests) fn s31_inline_ctor_with_elided_super_call() {
    // Constructor-shaped callee: `void <init>() { super(); }` —
    //   aload_0 (0x2a); invokespecial #2 (0xb7, pc=1, ELIDED); return (0xb1)
    // Caller: `int f(ref obj) { obj.<init>(); return 5; }` —
    //   aload_0; invokespecial #1 (pc=1, inline site); iconst_5; ireturn
    //
    // Self-proving: with the elided-super-call support the site inlines
    // to nothing (pop the receiver, no code) and f returns 5. Without it,
    // try_emit_inline bails and the site falls back to the dispatch
    // helper — which in these tests is the panicking stub, so try_call
    // would abort.
    let caller_code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0xb7, 0x00, 0x01, // 1: invokespecial #1
        0x08, // 4: iconst_5
        0xac, // 5: ireturn
        0, 0, // padding
    ];
    let caller_len = 6;

    let mut callee = make_inline_site(
        &[0x2a, 0xb7, 0x00, 0x02, 0xb1], // aload_0; invokespecial #2; return
        1,                               // max_locals (receiver)
        1,                               // num_args (receiver)
        false,                           // instance method
        b'V',                            // void return
    );
    callee.elided_invoke_pcs = vec![1];

    let mut sites = HashMap::new();
    sites.insert(1, callee);

    let compiled = compile_with_inlines(&caller_code, caller_len, 1, 1, sites)
        .expect("ctor-shaped callee with elided super call must compile");

    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    unsafe {
        assert_eq!(compiled.try_call(&[0x1000]).expect("test JIT call"), 5);
    }
}

#[test]
pub(in crate::x64::tests) fn s31_inline_ctor_non_elided_super_call_bails_cleanly() {
    // Same shape but the invokespecial pc is NOT in elided_invoke_pcs:
    // try_emit_inline must bail (rollback) without corrupting the
    // compile. The site then needs a dispatch fallback which this
    // harness does not provide — we only assert the compiler survives
    // (Some or None both acceptable shapes at this layer, but it must
    // not panic and must not emit the inline body).
    let caller_code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0xb7, 0x00, 0x01, // 1: invokespecial #1
        0x08, // 4: iconst_5
        0xac, // 5: ireturn
        0, 0,
    ];
    let caller_len = 6;

    let callee = make_inline_site(&[0x2a, 0xb7, 0x00, 0x02, 0xb1], 1, 1, false, b'V');
    // elided_invoke_pcs deliberately empty.

    let mut sites = HashMap::new();
    sites.insert(1, callee);

    let _ = compile_with_inlines(&caller_code, caller_len, 1, 1, sites);
}

#[test]
pub(in crate::x64::tests) fn s31_inline_getter_iload_ireturn() {
    // Caller: int f(int x) { return getX(x); }
    // callee: int getX(int x) { return x; }   → iload_0, ireturn
    //
    // Caller bytecode: iload_0, invokestatic #1 (pc=1), ireturn
    // We place the inline site at pc=1 (the invokestatic).
    let caller_code: Vec<u8> = vec![
        0x1a, // 0: iload_0
        0xb8, 0x00, 0x01, // 1: invokestatic #1
        0xac, // 4: ireturn
        0, 0, // padding
    ];
    let caller_len = 5;

    let callee = make_inline_site(
        &[0x1a, 0xac], // iload_0, ireturn
        1,             // max_locals
        1,             // num_args (one int param)
        true,          // static
        b'I',          // return type
    );

    let mut sites = HashMap::new();
    sites.insert(1, callee); // inline at pc=1

    let compiled =
        compile_with_inlines(&caller_code, caller_len, 1, 1, sites).expect("compilation failed");

    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    unsafe {
        assert_eq!(compiled.try_call(&[42]).expect("test JIT call"), 42);
        assert_eq!(compiled.try_call(&[0]).expect("test JIT call"), 0);
        assert_eq!(
            // Widening: i32 -> i64 (sign-extended)
            compiled.try_call(&[-7i32 as i64]).expect("test JIT call"),
            -7
        ); // Cast: JIT ABI convention
    }
}

/// PGO-02 §3, tested by INJECTING the violation it exists to catch.
///
/// An inlined body is entered and left inside one frame — the caller's own — so
/// an inlined scope cannot be described as a separate activation. (The older
/// wording here, "`deopt::FrameState::caller` is populated by no producer", went
/// stale on 2026-08-18 and the claim that such a point "would name the caller's
/// method with the callee's bci" had the pair the wrong way round: the hazard
/// round 10 actually found is a point named for the CALLEE that carries the
/// CALLER's geometry.) A point published from inside a spliced body is therefore
/// a well-formed answer about a stack that never existed, and `try_emit_inline`
/// refuses such a splice; a point that does get built inside one is now built to
/// the callee's own geometry with every slot `Unsupported`, so the sinks refuse
/// it rather than resuming into a guess.
///
/// No production emitter reaches that state today, which is exactly why the
/// check needs a deliberate violation to prove it fires — asserting "no
/// production path publishes one" would pass vacuously forever, including
/// after the day it stopped being true.
#[test]
pub(in crate::x64::tests) fn inline_publishing_a_deopt_point_is_refused() {
    let caller_code: Vec<u8> = vec![
        0x1a, // 0: iload_0
        0xb8, 0x00, 0x01, // 1: invokestatic #1
        0xac, // 4: ireturn
        0, 0, // padding
    ];
    let caller_len = 5;
    let sites = || {
        let mut sites = HashMap::new();
        sites.insert(1, make_inline_site(&[0x1a, 0xac], 1, 1, true, b'I'));
        sites
    };

    // Control: the splice happens and the spliced code is correct.
    let inlined = compile_with_inlines(&caller_code, caller_len, 1, 1, sites())
        .expect("control compile must succeed");
    // SAFETY: JIT-compiled machine code produced by this compiler from valid
    // bytecode, in an executable mapping.
    unsafe {
        assert_eq!(inlined.try_call(&[42]).expect("test JIT call"), 42);
    }

    // Injected violation: the same body now publishes deopt metadata.
    super::INLINE_TEST_PUBLISHES_DEOPT.with(|f| f.set(true));
    let refused = compile_with_inlines(&caller_code, caller_len, 1, 1, sites());
    super::INLINE_TEST_PUBLISHES_DEOPT.with(|f| f.set(false));

    let refused = refused
        .expect("refusing the splice must fall back to a normal call, not bail the whole method");
    assert_ne!(
        refused.code_bytes(),
        inlined.code_bytes(),
        "a body that published deopt metadata must NOT have been spliced — identical \
         machine code means the postcondition did not fire and an unrepresentable \
         inlined scope was published"
    );
}

#[test]
pub(in crate::x64::tests) fn s31_inline_add_two_params() {
    // Caller: int f(int a, int b) { return add(a, b); }
    // Callee: int add(int a, int b) { return a + b; }  → iload_0, iload_1, iadd, ireturn
    let caller_code: Vec<u8> = vec![
        0x1a, // 0: iload_0
        0x1b, // 1: iload_1
        0xb8, 0x00, 0x01, // 2: invokestatic #1
        0xac, // 5: ireturn
        0, 0,
    ];
    let caller_len = 6;

    let callee = make_inline_site(
        &[0x1a, 0x1b, 0x60, 0xac], // iload_0, iload_1, iadd, ireturn
        2,
        2, // two params
        true,
        b'I',
    );

    let mut sites = HashMap::new();
    sites.insert(2, callee);

    let compiled =
        compile_with_inlines(&caller_code, caller_len, 2, 2, sites).expect("compilation failed");

    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    unsafe {
        assert_eq!(compiled.try_call(&[3, 4]).expect("test JIT call"), 7);
        assert_eq!(
            compiled
                // Widening: i32 -> i64 (sign-extended)
                .try_call(&[100, -50i32 as i64])
                .expect("test JIT call"),
            50
        ); // Cast: JIT ABI convention
        assert_eq!(compiled.try_call(&[0, 0]).expect("test JIT call"), 0);
    }
}

#[test]
pub(in crate::x64::tests) fn s31_inline_constant_return() {
    // Caller: int f() { return five(); }
    // Callee: int five() { return 5; }  → iconst_5, ireturn
    let caller_code: Vec<u8> = vec![
        0xb8, 0x00, 0x01, // 0: invokestatic #1
        0xac, // 3: ireturn
        0, 0,
    ];
    let caller_len = 4;

    let callee = make_inline_site(
        &[0x08, 0xac], // iconst_5, ireturn
        0,
        0, // no params
        true,
        b'I',
    );

    let mut sites = HashMap::new();
    sites.insert(0, callee);

    let compiled =
        compile_with_inlines(&caller_code, caller_len, 0, 0, sites).expect("compilation failed");

    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    unsafe {
        assert_eq!(compiled.try_call(&[]).expect("test JIT call"), 5);
    }
}

#[test]
pub(in crate::x64::tests) fn s31_inline_void_method() {
    // Caller: int f(int x) { noop(); return x; }
    // Callee: void noop() { return; }
    let caller_code: Vec<u8> = vec![
        0xb8, 0x00, 0x01, // 0: invokestatic #1 (void)
        0x1a, // 3: iload_0
        0xac, // 4: ireturn
        0, 0,
    ];
    let caller_len = 5;

    let callee = make_inline_site(
        &[0xb1], // return (void)
        0,
        0,
        true,
        b'V',
    );

    let mut sites = HashMap::new();
    sites.insert(0, callee);

    let compiled =
        compile_with_inlines(&caller_code, caller_len, 1, 1, sites).expect("compilation failed");

    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    unsafe {
        assert_eq!(compiled.try_call(&[99]).expect("test JIT call"), 99);
    }
}

#[test]
pub(in crate::x64::tests) fn s31_inline_with_branch() {
    // Callee: int abs(int x) { return x >= 0 ? x : -x; }
    // Bytecode: iload_0, ifge +5, iload_0, ineg, ireturn, iload_0, ireturn
    //           0       1        4       5     6        7       8
    let callee_bc: Vec<u8> = vec![
        0x1a, // 0: iload_0
        0x9c, 0x00, 0x06, // 1: ifge +6 → target=7
        0x1a, // 4: iload_0
        0x74, // 5: ineg
        0xac, // 6: ireturn
        0x1a, // 7: iload_0
        0xac, // 8: ireturn
    ];

    let caller_code: Vec<u8> = vec![
        0x1a, // 0: iload_0
        0xb8, 0x00, 0x01, // 1: invokestatic #1
        0xac, // 4: ireturn
        0, 0,
    ];
    let caller_len = 5;

    let callee = make_inline_site(&callee_bc, 1, 1, true, b'I');

    let mut sites = HashMap::new();
    sites.insert(1, callee);

    let compiled =
        compile_with_inlines(&caller_code, caller_len, 1, 1, sites).expect("compilation failed");

    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    unsafe {
        assert_eq!(compiled.try_call(&[5]).expect("test JIT call"), 5);
        assert_eq!(
            // Widening: i32 -> i64 (sign-extended)
            compiled.try_call(&[-5i32 as i64]).expect("test JIT call"),
            5
        ); // Cast: JIT ABI convention
        assert_eq!(compiled.try_call(&[0]).expect("test JIT call"), 0);
    }
}

#[test]
pub(in crate::x64::tests) fn s31_inline_with_if_icmp() {
    // Callee: int max(int a, int b) { return a >= b ? a : b; }
    // Exercises the restored if_icmp inline path: a forward conditional
    // branch (if_icmplt) with two return arms and an EMPTY operand stack at
    // the merge — the provably-safe subset that 419a6f5 over-bailed.
    let callee_bc: Vec<u8> = vec![
        0x1a, // 0: iload_0 (a)
        0x1b, // 1: iload_1 (b)
        0xa1, 0x00, 0x05, // 2: if_icmplt +5 -> 7 (if a<b, return b)
        0x1a, // 5: iload_0 (a)
        0xac, // 6: ireturn
        0x1b, // 7: iload_1 (b)   <- branch target (empty stack)
        0xac, // 8: ireturn
    ];
    let caller_code: Vec<u8> = vec![
        0x1a, // 0: iload_0
        0x1b, // 1: iload_1
        0xb8, 0x00, 0x01, // 2: invokestatic #1
        0xac, // 5: ireturn
        0, 0,
    ];
    let caller_len = 6;
    let callee = make_inline_site(&callee_bc, 2, 2, true, b'I');
    let mut sites = HashMap::new();
    sites.insert(2, callee);
    let compiled =
        compile_with_inlines(&caller_code, caller_len, 2, 2, sites).expect("compilation failed");
    // SAFETY: executing JIT-compiled machine code produced from valid
    // bytecode; the mmap region is executable.
    unsafe {
        assert_eq!(compiled.try_call(&[5, 3]).expect("test JIT call"), 5);
        assert_eq!(compiled.try_call(&[3, 5]).expect("test JIT call"), 5);
        assert_eq!(compiled.try_call(&[4, 4]).expect("test JIT call"), 4);
    }
}

#[test]
pub(in crate::x64::tests) fn s31_inline_arithmetic_chain() {
    // Callee: int triple(int x) { return x + x + x; }
    // iload_0, iload_0, iadd, iload_0, iadd, ireturn
    let callee_bc: Vec<u8> = vec![0x1a, 0x1a, 0x60, 0x1a, 0x60, 0xac];

    let caller_code: Vec<u8> = vec![
        0x1a, // 0: iload_0
        0xb8, 0x00, 0x01, // 1: invokestatic #1
        0xac, // 4: ireturn
        0, 0,
    ];
    let caller_len = 5;

    let callee = make_inline_site(&callee_bc, 1, 1, true, b'I');

    let mut sites = HashMap::new();
    sites.insert(1, callee);

    let compiled =
        compile_with_inlines(&caller_code, caller_len, 1, 1, sites).expect("compilation failed");

    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    unsafe {
        assert_eq!(compiled.try_call(&[7]).expect("test JIT call"), 21);
        assert_eq!(compiled.try_call(&[0]).expect("test JIT call"), 0);
        assert_eq!(
            // Widening: i32 -> i64 (sign-extended)
            compiled.try_call(&[-3i32 as i64]).expect("test JIT call"),
            -9
        ); // Cast: JIT ABI convention
    }
}

#[test]
pub(in crate::x64::tests) fn s31_inline_iinc_loop() {
    // Callee: int inc5(int x) { x++; x++; x++; x++; x++; return x; }
    // iinc 0,1 × 5, iload_0, ireturn
    let callee_bc: Vec<u8> = vec![
        0x84, 0x00, 0x01, // iinc local0, 1
        0x84, 0x00, 0x01, 0x84, 0x00, 0x01, 0x84, 0x00, 0x01, 0x84, 0x00, 0x01,
        0x1a, // iload_0
        0xac, // ireturn
    ];

    let caller_code: Vec<u8> = vec![0x1a, 0xb8, 0x00, 0x01, 0xac, 0, 0];
    let caller_len = 5;

    let callee = make_inline_site(&callee_bc, 1, 1, true, b'I');
    let mut sites = HashMap::new();
    sites.insert(1, callee);

    let compiled =
        compile_with_inlines(&caller_code, caller_len, 1, 1, sites).expect("compilation failed");

    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    unsafe {
        assert_eq!(compiled.try_call(&[10]).expect("test JIT call"), 15);
        assert_eq!(compiled.try_call(&[0]).expect("test JIT call"), 5);
    }
}

#[test]
pub(in crate::x64::tests) fn s31_inline_callee_uses_extra_locals() {
    // Callee: int swap_add(int a, int b) { int t = a; a = b; b = t; return a + b; }
    // iload_0, istore_2, iload_1, istore_0, iload_2, istore_1, iload_0, iload_1, iadd, ireturn
    let callee_bc: Vec<u8> = vec![
        0x1a, // 0: iload_0
        0x3d, // 1: istore_2 (local 2 = temp)
        0x1b, // 2: iload_1
        0x3b, // 3: istore_0
        0x1c, // 4: iload_2
        0x3c, // 5: istore_1
        0x1a, // 6: iload_0
        0x1b, // 7: iload_1
        0x60, // 8: iadd
        0xac, // 9: ireturn
    ];

    let caller_code: Vec<u8> = vec![
        0x1a, // 0: iload_0
        0x1b, // 1: iload_1
        0xb8, 0x00, 0x01, // 2: invokestatic
        0xac, // 5: ireturn
        0, 0,
    ];
    let caller_len = 6;

    let callee = make_inline_site(&callee_bc, 3, 2, true, b'I');
    let mut sites = HashMap::new();
    sites.insert(2, callee);

    let compiled =
        compile_with_inlines(&caller_code, caller_len, 2, 2, sites).expect("compilation failed");

    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    unsafe {
        // swap_add(a,b) = a + b regardless of swap, so always sum
        assert_eq!(compiled.try_call(&[3, 7]).expect("test JIT call"), 10);
        assert_eq!(compiled.try_call(&[100, 200]).expect("test JIT call"), 300);
    }
}

#[test]
pub(in crate::x64::tests) fn s31_inline_bailout_unsupported_opcode() {
    // Callee has monitorenter (0xC2) which is unsupported — should bail out.
    // The caller should still compile (the invokestatic falls through to normal call path).
    let callee_bc: Vec<u8> = vec![
        0x1a, // iload_0
        0xC2, // monitorenter — unsupported in inline
        0xb1, // return
    ];

    let caller_code: Vec<u8> = vec![
        0x1a, // 0: iload_0
        0xb8, 0x00, 0x01, // 1: invokestatic — will try inline but bail
        0xac, // 4: ireturn
        0, 0,
    ];
    let caller_len = 5;

    let callee = make_inline_site(&callee_bc, 1, 1, true, b'V');
    let mut sites = HashMap::new();
    sites.insert(1, callee);

    // This should either compile successfully (falling back to a call stub)
    // or return None if the fallback isn't available. Either way, no crash.
    let _result = compile_with_inlines(&caller_code, caller_len, 1, 1, sites);
    // Main assertion: no panic or crash
}

#[test]
pub(in crate::x64::tests) fn s31_inline_long_arithmetic() {
    // Callee: long double_it(long x) { return x + x; }  → lload_0, lload_0, ladd, lreturn
    // We test through int path since our JIT uses i64 uniformly.
    // Caller: int f(int x) { return double_it(x); }
    let callee_bc: Vec<u8> = vec![0x1a, 0x1a, 0x61, 0xac]; // iload_0, iload_0, ladd, ireturn

    let caller_code: Vec<u8> = vec![
        0x1a, // 0: iload_0
        0xb8, 0x00, 0x01, // 1: invokestatic
        0xac, // 4: ireturn
        0, 0,
    ];
    let caller_len = 5;

    let callee = make_inline_site(&callee_bc, 1, 1, true, b'I');
    let mut sites = HashMap::new();
    sites.insert(1, callee);

    let compiled =
        compile_with_inlines(&caller_code, caller_len, 1, 1, sites).expect("compilation failed");

    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    unsafe {
        assert_eq!(compiled.try_call(&[50]).expect("test JIT call"), 100);
        assert_eq!(
            // Widening: i32 -> i64 (sign-extended)
            compiled.try_call(&[-7i32 as i64]).expect("test JIT call"),
            -14
        ); // Cast: JIT ABI convention
    }
}

#[test]
pub(in crate::x64::tests) fn s31_inline_category2_params_use_jvm_local_slots() {
    // Mirrors Bouncy Castle Bits.bitPermuteStep(JJI)J:
    //   long x -> local 0/1, long m -> local 2/3, int s -> local 4.
    // The inline emitter used to deposit the three JIT args compactly into
    // locals 0, 1, 2, so `lload_2` read the shift count and `iload 4` read
    // zero. That miscompiled Interleave.expand64To128 after tiered inlining.
    let callee_bc: Vec<u8> = vec![
        0x1e, // 0: lload_0
        0x1e, // 1: lload_0
        0x15, 0x04, // 2: iload 4
        0x7d, // 4: lushr
        0x83, // 5: lxor
        0x20, // 6: lload_2
        0x7f, // 7: land
        0x37, 0x05, // 8: lstore 5
        0x16, 0x05, // 10: lload 5
        0x16, 0x05, // 12: lload 5
        0x15, 0x04, // 14: iload 4
        0x79, // 16: lshl
        0x83, // 17: lxor
        0x1e, // 18: lload_0
        0x83, // 19: lxor
        0xad, // 20: lreturn
    ];

    let caller_code: Vec<u8> = vec![
        0x1a, // 0: iload_0 (x, represented as i64 in this JIT ABI)
        0x1b, // 1: iload_1 (m)
        0x1c, // 2: iload_2 (s)
        0xb8, 0x00, 0x01, // 3: invokestatic
        0xad, // 6: lreturn
        0, 0,
    ];
    let caller_len = 7;

    let mut callee = make_inline_site(&callee_bc, 7, 3, true, b'J');
    callee.descriptor = "(JJI)J".to_string();
    let mut sites = HashMap::new();
    sites.insert(3, callee);

    let compiled = compile_with_inlines(&caller_code, caller_len, 3, 3, sites)
        .expect("category-2 inline callee must compile");

    let expect = |x: i64, m: i64, s: i64| -> i64 {
        let x = x as u64;
        let m = m as u64;
        let s = (s as u32) & 0x3f;
        let t = (x ^ (x >> s)) & m;
        (x ^ t ^ (t << s)) as i64
    };

    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    unsafe {
        let cases = [
            (0x0123_4567_89ab_cdef_i64, 0x0000_ffff_0000_0000_i64, 16_i64),
            (
                0x8000_0000_0000_0001_u64 as i64,
                0x2222_2222_2222_2222_i64,
                1_i64,
            ),
            (
                0xfedc_ba98_7654_3210_u64 as i64,
                0x0c0c_0c0c_0c0c_0c0c_i64,
                2_i64,
            ),
        ];
        for (x, m, s) in cases {
            assert_eq!(
                compiled.try_call(&[x, m, s]).expect("test JIT call"),
                expect(x, m, s)
            );
        }
    }
}

#[test]
pub(in crate::x64::tests) fn s31_inline_bipush_sipush() {
    // Callee: int f() { return 100; }  → bipush 100, ireturn
    let callee_bc: Vec<u8> = vec![0x10, 100, 0xac]; // bipush 100, ireturn

    let caller_code: Vec<u8> = vec![
        0xb8, 0x00, 0x01, // 0: invokestatic
        0xac, // 3: ireturn
        0, 0,
    ];
    let caller_len = 4;

    let callee = make_inline_site(&callee_bc, 0, 0, true, b'I');
    let mut sites = HashMap::new();
    sites.insert(0, callee);

    let compiled =
        compile_with_inlines(&caller_code, caller_len, 0, 0, sites).expect("compilation failed");

    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    unsafe {
        assert_eq!(compiled.try_call(&[]).expect("test JIT call"), 100);
    }
}

#[test]
pub(in crate::x64::tests) fn s31_inline_multiple_sites() {
    // Caller: int f(int x) { return inc(inc(x)); }
    // Two invokestatics, each inlining "int inc(int x) { return x + 1; }"
    // Callee: iload_0, iconst_1, iadd, ireturn
    let callee_bc: Vec<u8> = vec![0x1a, 0x04, 0x60, 0xac];

    let caller_code: Vec<u8> = vec![
        0x1a, // 0: iload_0
        0xb8, 0x00, 0x01, // 1: invokestatic #1 (first inc)
        0xb8, 0x00, 0x02, // 4: invokestatic #2 (second inc)
        0xac, // 7: ireturn
        0, 0,
    ];
    let caller_len = 8;

    let site1 = make_inline_site(&callee_bc, 1, 1, true, b'I');
    let site2 = make_inline_site(&callee_bc, 1, 1, true, b'I');

    let mut sites = HashMap::new();
    sites.insert(1, site1);
    sites.insert(4, site2);

    let compiled =
        compile_with_inlines(&caller_code, caller_len, 1, 1, sites).expect("compilation failed");

    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    unsafe {
        assert_eq!(compiled.try_call(&[10]).expect("test JIT call"), 12); // 10 + 1 + 1
        assert_eq!(compiled.try_call(&[0]).expect("test JIT call"), 2);
    }
}

// ===================================================================
// Inline array-access codegen tests
//
// These exercise the inline machine code emitted for `arraylength`
// (0xbe) and the primitive `Xaload` family (`iaload` 0x2e, `laload`
// 0x2f, `baload` 0x33, `caload` 0x34, `saload` 0x35). The JIT emits
// raw loads from the array header/data region with no helper `CALL`
// on the fast path:
//   - length:  MOV EAX, [array + ARRAY_LENGTH_OFFSET(12)]
//   - element: load from [array + HEADER_SIZE + idx*scale]
//     with scale 1/2/4/8 and the correct sign/zero extension.
// The two exception edges (null-array NPE, out-of-bounds AIOOBE)
// still funnel through the shared deopt stubs, which call the
// `bastore` / `throw_aioobe` runtime helpers respectively.
//
// Heap arrays are allocated via the `cratonvm-gc` dev-dependency
// (`GenerationalHeap`), matching the existing `test_getfield_*`
// pattern — no `SharedVm` / `vm-tests` gating needed because the
// fast path takes no VM-context argument.
// ===================================================================

thread_local! {
    /// Set by [`flagging_throw_aioobe`] so an inline out-of-bounds
    /// access can be observed from a test: `(index, length, bytecode_pc)`.
    pub(in crate::x64::tests) static TEST_AIOOBE_HIT: std::cell::Cell<Option<(i64, i64, i64)>> =
        const { std::cell::Cell::new(None) };
    /// Set by [`flagging_npe_with_action`] when an inline null-check deopt
    /// stub fires.
    pub(in crate::x64::tests) static TEST_NPE_HIT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// JEP 358 — the action code the firing null-check stub passed to
    /// `jit_npe_with_action`, DECODED out of the packed word. `-1` = no stub
    /// fired. Lets the null-NPE tests assert the per-opcode action is threaded
    /// correctly.
    pub(in crate::x64::tests) static TEST_NPE_ACTION: std::cell::Cell<i64> = const { std::cell::Cell::new(-1) };
    /// The trap-site key from the same packed word: the id
    /// `inlining::record_npe_trap_site` issued for THIS null check, which is
    /// what lets the stack walk recover the bci of the frame that trapped.
    /// `-1` = no stub fired, `0` = the site was not recorded (which is what
    /// `CRATONVM_JIT_NO_NPE_TRAP_LINES=1` produces).
    pub(in crate::x64::tests) static TEST_NPE_TRAP_KEY: std::cell::Cell<i64> = const { std::cell::Cell::new(-1) };
    /// The bytecode position the null-check deopt stub publishes before it
    /// raises the pending NPE. `-1` means that the stub skipped the stamp.
    pub(in crate::x64::tests) static TEST_THROW_BCI: std::cell::Cell<i64> = const { std::cell::Cell::new(-1) };
}

/// Test stand-in for `jit_throw_aioobe`: records the failure payload
/// and returns the `i64::MIN` deopt sentinel, exactly like the real
/// helper. The bounds-check stub calls this when `idx >= len`.
///
/// SAFETY: plain `extern "C"` callback invoked by JIT code with four
/// `i64` arguments; touches only a thread-local.
pub(in crate::x64::tests) unsafe extern "C" fn flagging_throw_aioobe(
    index: i64,
    length: i64,
    _array_ptr: i64,
    bytecode_pc: i64,
) -> i64 {
    // Wave 102 (L2): the stamp without the packed compile id, as the setter.
    let (bytecode_pc, _owner) = crate::implicit_null::split_throw_stamp_owner(bytecode_pc);
    TEST_AIOOBE_HIT.with(|c| c.set(Some((index, length, bytecode_pc))));
    i64::MIN
}

/// Test stand-in for `jit_npe_with_action`: each inline null-check deopt
/// stub calls `helpers.jit_npe_with_action(packed)` to flag a pending NPE.
/// Records the hit AND both halves of the packed word; the stub itself loads
/// the `i64::MIN` sentinel.
///
/// The argument is NOT the bare action code. Since the trap-site side channel
/// landed, each described null check gets its own ten-byte trampoline that
/// passes `action | (trap_key << 8)`, so the walk that builds the trace can
/// ask which check fired and recover the bci of the frame that trapped. A test
/// that asserts on the raw word is asserting on the low byte AND the site id
/// at once, and would move every time a new check is emitted ahead of it —
/// so split it here, once, rather than in each test.
///
/// SAFETY: plain `extern "C"` callback invoked by JIT code with one `i64`
/// argument; touches only thread-locals.
pub(in crate::x64::tests) unsafe extern "C" fn flagging_npe_with_action(packed: i64) {
    TEST_NPE_HIT.with(|c| c.set(true));
    TEST_NPE_ACTION.with(|c| c.set(packed & 0xff));
    TEST_NPE_TRAP_KEY.with(|c| c.set((packed >> 8) & 0x00ff_ffff));
}

/// Test stand-in for `jit_set_throw_bci`, used to make the exception-location
/// contract observable at the generated-code boundary.
pub(in crate::x64::tests) unsafe extern "C" fn flagging_set_throw_bci(bci: i64) {
    // Interpreter round i5 wave 102, lane L2: the real setter splits off the
    // compile id an exit packs above the stamp
    // (`CRATONVM_JIT_THROW_STAMP_OWNER`); so does this stand-in.
    let (bci, _owner) = crate::implicit_null::split_throw_stamp_owner(bci);
    TEST_THROW_BCI.with(|c| c.set(bci));
}

/// `test_helpers()` with the `throw_aioobe` and `jit_npe_with_action`
/// slots wired to the flagging stubs above so the exception-edge tests
/// can take the deopt path without panicking on an unimplemented stub.
pub(in crate::x64::tests) fn array_test_helpers() -> JitRuntimeHelpers {
    let mut h = test_helpers();
    // Cast: fn pointer to usize helper address
    h.throw_aioobe = flagging_throw_aioobe as *const () as usize;
    // Cast: fn pointer to usize helper address
    h.jit_npe_with_action = flagging_npe_with_action as *const () as usize;
    h.set_throw_bci = flagging_set_throw_bci as *const () as usize;
    h
}

/// Compile a single-method body with `array_test_helpers()`. Mirrors
/// the argument convention of the `test_getfield_*` helpers — the
/// `compile` positional args after `code_len` are `num_params` then
/// `max_locals` (there is no separate `max_stack` parameter).
pub(in crate::x64::tests) fn compile_array_test(
    code: &[u8],
    code_len: usize,
    num_params: usize,
    max_locals: usize,
) -> CompiledMethod {
    compile(
        code,
        code_len,
        num_params,
        max_locals,
        // needs_heap = false: inline array access (length + Xaload) is
        // pure machine code with no hidden VM-context argument, so the
        // tests call `CompiledMethod::call` directly with just the Java
        // args. The deopt stubs call `bastore`/`throw_aioobe` helpers
        // but those take no context. Mirrors the `test_getfield_*`
        // pattern; see the note in `test_bounds_check_iaload_in_bounds`.
        false, // needs_heap
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(), // pic_slots — no PIC sites
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &array_test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .unwrap()
}

#[test]
pub(in crate::x64::tests) fn test_inline_arraylength() {
    // int f(int[] arr) { return arr.length; }
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0      (array)
        0xbe, // 1: arraylength
        0xac, // 2: ireturn
        0, 0,
    ];
    let compiled = compile_array_test(&code, 3, 1, 1);

    use cratonvm_gc::gen_heap::GenerationalHeap;
    use cratonvm_gc::heap::ArrayElementType;
    use cratonvm_types::ClassId;
    let heap = GenerationalHeap::new();

    let arr0 = heap.alloc_array(ClassId::new(0), ArrayElementType::Int, 0);
    let arr5 = heap.alloc_array(ClassId::new(0), ArrayElementType::Int, 5);
    let arr257 = heap.alloc_array(ClassId::new(0), ArrayElementType::Int, 257);

    // SAFETY: JIT-compiled code from valid bytecode; mmap region executable.
    unsafe {
        assert_eq!(
            compiled
                // Cast: object/array pointer to i64 for the JIT calling convention
                .try_call(&[arr0.as_ptr() as i64])
                .expect("test JIT call"),
            0
        );
        assert_eq!(
            compiled
                // Cast: object/array pointer to i64 for the JIT calling convention
                .try_call(&[arr5.as_ptr() as i64])
                .expect("test JIT call"),
            5
        );
        assert_eq!(
            compiled
                // Cast: object/array pointer to i64 for the JIT calling convention
                .try_call(&[arr257.as_ptr() as i64])
                .expect("test JIT call"),
            257
        );
    }
}

#[test]
pub(in crate::x64::tests) fn test_inline_iaload() {
    // int f(int[] arr, int i) { return arr[i]; }
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0x1b, // 1: iload_1
        0x2e, // 2: iaload
        0xac, // 3: ireturn
        0, 0,
    ];
    let compiled = compile_array_test(&code, 4, 2, 2);

    use cratonvm_gc::gen_heap::GenerationalHeap;
    use cratonvm_gc::heap::ArrayElementType;
    use cratonvm_types::ClassId;
    let heap = GenerationalHeap::new();

    let arr = heap.alloc_array(ClassId::new(0), ArrayElementType::Int, 4);
    // Include a negative element to confirm 32-bit values round-trip.
    let vals = [7i32, -100_000, i32::MAX, i32::MIN];
    for (i, v) in vals.iter().enumerate() {
        heap.set_array_element(arr, i, Value::Int(*v)).unwrap();
    }

    // SAFETY: JIT-compiled code from valid bytecode; mmap region executable.
    unsafe {
        for (i, v) in vals.iter().enumerate() {
            // iaload sign-extends the 32-bit element to the 64-bit
            // return register, so the expected value is `*v as i64`.
            assert_eq!(
                compiled
                    // Cast: object/array pointer to i64 for the JIT calling convention
                    .try_call(&[arr.as_ptr() as i64, i as i64])
                    .expect("test JIT call"),
                // Cast: test value to i64 for the JIT calling convention
                *v as i64,
                "iaload mismatch at index {i}"
            );
        }
    }
}

#[test]
pub(in crate::x64::tests) fn test_inline_baload_sign_extends() {
    // int f(byte[] arr, int i) { return arr[i]; }
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0x1b, // 1: iload_1
        0x33, // 2: baload
        0xac, // 3: ireturn
        0, 0,
    ];
    let compiled = compile_array_test(&code, 4, 2, 2);

    use cratonvm_gc::gen_heap::GenerationalHeap;
    use cratonvm_gc::heap::ArrayElementType;
    use cratonvm_types::ClassId;
    let heap = GenerationalHeap::new();

    let arr = heap.alloc_array(ClassId::new(0), ArrayElementType::Byte, 4);
    // Store raw byte patterns via Value::Int (heap narrows to i8).
    // 0xFF -> -1, 0x80 -> -128 prove the sign extension.
    heap.set_array_element(arr, 0, Value::Int(0x7F)).unwrap();
    heap.set_array_element(arr, 1, Value::Int(0xFF)).unwrap();
    heap.set_array_element(arr, 2, Value::Int(0x80)).unwrap();
    heap.set_array_element(arr, 3, Value::Int(0x01)).unwrap();

    // SAFETY: JIT-compiled code from valid bytecode; mmap region executable.
    unsafe {
        assert_eq!(
            compiled
                // Cast: object/array pointer to i64 for the JIT calling convention
                .try_call(&[arr.as_ptr() as i64, 0])
                .expect("test JIT call"),
            127
        );
        assert_eq!(
            compiled
                // Cast: object/array pointer to i64 for the JIT calling convention
                .try_call(&[arr.as_ptr() as i64, 1])
                .expect("test JIT call"),
            -1
        );
        assert_eq!(
            compiled
                // Cast: object/array pointer to i64 for the JIT calling convention
                .try_call(&[arr.as_ptr() as i64, 2])
                .expect("test JIT call"),
            -128
        );
        assert_eq!(
            compiled
                // Cast: object/array pointer to i64 for the JIT calling convention
                .try_call(&[arr.as_ptr() as i64, 3])
                .expect("test JIT call"),
            1
        );
    }
}

#[test]
pub(in crate::x64::tests) fn test_inline_caload_zero_extends() {
    // int f(char[] arr, int i) { return arr[i]; }
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0x1b, // 1: iload_1
        0x34, // 2: caload
        0xac, // 3: ireturn
        0, 0,
    ];
    let compiled = compile_array_test(&code, 4, 2, 2);

    use cratonvm_gc::gen_heap::GenerationalHeap;
    use cratonvm_gc::heap::ArrayElementType;
    use cratonvm_types::ClassId;
    let heap = GenerationalHeap::new();

    let arr = heap.alloc_array(ClassId::new(0), ArrayElementType::Char, 3);
    // 0xFFFF must zero-extend to 65535 (not -1).
    heap.set_array_element(arr, 0, Value::Int(0x0041)).unwrap(); // 'A'
    heap.set_array_element(arr, 1, Value::Int(0xFFFF)).unwrap();
    heap.set_array_element(arr, 2, Value::Int(0x8000)).unwrap();

    // SAFETY: JIT-compiled code from valid bytecode; mmap region executable.
    unsafe {
        assert_eq!(
            compiled
                // Cast: object/array pointer to i64 for the JIT calling convention
                .try_call(&[arr.as_ptr() as i64, 0])
                .expect("test JIT call"),
            65
        );
        assert_eq!(
            compiled
                // Cast: object/array pointer to i64 for the JIT calling convention
                .try_call(&[arr.as_ptr() as i64, 1])
                .expect("test JIT call"),
            65535
        );
        assert_eq!(
            compiled
                // Cast: object/array pointer to i64 for the JIT calling convention
                .try_call(&[arr.as_ptr() as i64, 2])
                .expect("test JIT call"),
            32768
        );
    }
}

#[test]
pub(in crate::x64::tests) fn test_inline_saload_sign_extends() {
    // int f(short[] arr, int i) { return arr[i]; }
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0x1b, // 1: iload_1
        0x35, // 2: saload
        0xac, // 3: ireturn
        0, 0,
    ];
    let compiled = compile_array_test(&code, 4, 2, 2);

    use cratonvm_gc::gen_heap::GenerationalHeap;
    use cratonvm_gc::heap::ArrayElementType;
    use cratonvm_types::ClassId;
    let heap = GenerationalHeap::new();

    let arr = heap.alloc_array(ClassId::new(0), ArrayElementType::Short, 3);
    // 0xFFFF must sign-extend to -1, 0x8000 to -32768.
    heap.set_array_element(arr, 0, Value::Int(0x7FFF)).unwrap();
    heap.set_array_element(arr, 1, Value::Int(0xFFFF)).unwrap();
    heap.set_array_element(arr, 2, Value::Int(0x8000)).unwrap();

    // SAFETY: JIT-compiled code from valid bytecode; mmap region executable.
    unsafe {
        assert_eq!(
            compiled
                // Cast: object/array pointer to i64 for the JIT calling convention
                .try_call(&[arr.as_ptr() as i64, 0])
                .expect("test JIT call"),
            32767
        );
        assert_eq!(
            compiled
                // Cast: object/array pointer to i64 for the JIT calling convention
                .try_call(&[arr.as_ptr() as i64, 1])
                .expect("test JIT call"),
            -1
        );
        assert_eq!(
            compiled
                // Cast: object/array pointer to i64 for the JIT calling convention
                .try_call(&[arr.as_ptr() as i64, 2])
                .expect("test JIT call"),
            -32768
        );
    }
}

#[test]
pub(in crate::x64::tests) fn test_inline_laload() {
    // long f(long[] arr, int i) { return arr[i]; }
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0x1b, // 1: iload_1
        0x2f, // 2: laload
        0xad, // 3: lreturn
        0, 0,
    ];
    let compiled = compile_array_test(&code, 4, 2, 2);

    use cratonvm_gc::gen_heap::GenerationalHeap;
    use cratonvm_gc::heap::ArrayElementType;
    use cratonvm_types::ClassId;
    let heap = GenerationalHeap::new();

    let arr = heap.alloc_array(ClassId::new(0), ArrayElementType::Long, 3);
    let vals = [0x0123_4567_89AB_CDEFi64, i64::MIN, -1i64];
    for (i, v) in vals.iter().enumerate() {
        heap.set_array_element(arr, i, Value::Long(*v)).unwrap();
    }

    // SAFETY: JIT-compiled code from valid bytecode; mmap region executable.
    unsafe {
        for (i, v) in vals.iter().enumerate() {
            assert_eq!(
                compiled
                    // Cast: object/array pointer to i64 for the JIT calling convention
                    .try_call(&[arr.as_ptr() as i64, i as i64])
                    .expect("test JIT call"),
                *v,
                "laload mismatch at index {i}"
            );
        }
    }
}

#[test]
pub(in crate::x64::tests) fn test_inline_arraylength_null_throws_npe() {
    // int f(int[] arr) { return arr.length; }  with arr == null.
    // The inline `MOV EAX, [arr + 12]` is guarded by a TEST/JZ that
    // branches to the per-action null-check deopt stub. That stub calls
    // `helpers.jit_npe_with_action(ARRAY_LENGTH)` (flagging our NPE
    // thread-locals) and returns the `i64::MIN` deopt sentinel.
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0xbe, // 1: arraylength
        0xac, // 2: ireturn
        0, 0,
    ];
    let compiled = compile_array_test(&code, 3, 1, 1);

    TEST_NPE_HIT.with(|c| c.set(false));
    TEST_NPE_ACTION.with(|c| c.set(-1));
    TEST_NPE_TRAP_KEY.with(|c| c.set(-1));
    TEST_THROW_BCI.with(|c| c.set(-1));
    // SAFETY: JIT-compiled code from valid bytecode; mmap region executable.
    let result = unsafe { compiled.try_call(&[0]).expect("test JIT call") }; // null array
    assert_eq!(
        result,
        i64::MIN,
        "null arraylength must deopt with sentinel"
    );
    assert!(
        TEST_NPE_HIT.with(|c| c.get()),
        "null arraylength must take the NPE deopt stub"
    );
    assert_eq!(
        TEST_THROW_BCI.with(|c| c.get()),
        1,
        "null arraylength must publish its trapping bytecode position before raising NPE"
    );
    // JEP 358: the stub must thread the `arraylength` action code.
    assert_eq!(
        TEST_NPE_ACTION.with(|c| c.get()),
        // Widening: narrower int -> i64
        npe_action::ARRAY_LENGTH as i64,
        "null arraylength must report the ARRAY_LENGTH action"
    );
    // The other half of the same word: the trap-site id. A zero here means the
    // stub fired but named no site, which is exactly what the trace shows as
    // `Method:-1` — the frame survives and its line does not.
    if super::inlining::npe_trap_lines_enabled() {
        assert!(
            TEST_NPE_TRAP_KEY.with(|c| c.get()) > 0,
            "the null-check trampoline must carry a recorded trap-site id \
             beside the action; got {}",
            TEST_NPE_TRAP_KEY.with(|c| c.get())
        );
    }
}

#[test]
pub(in crate::x64::tests) fn test_inline_iaload_null_throws_npe() {
    // int f(int[] arr, int i) { return arr[i]; }  with arr == null.
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0x1b, // 1: iload_1
        0x2e, // 2: iaload
        0xac, // 3: ireturn
        0, 0,
    ];
    let compiled = compile_array_test(&code, 4, 2, 2);

    TEST_NPE_HIT.with(|c| c.set(false));
    TEST_NPE_ACTION.with(|c| c.set(-1));
    TEST_NPE_TRAP_KEY.with(|c| c.set(-1));
    // SAFETY: JIT-compiled code from valid bytecode; mmap region executable.
    let result = unsafe { compiled.try_call(&[0, 0]).expect("test JIT call") }; // null array
    assert_eq!(result, i64::MIN, "null iaload must deopt with sentinel");
    assert!(
        TEST_NPE_HIT.with(|c| c.get()),
        "null iaload must take the NPE deopt stub"
    );
    // JEP 358: the stub must thread the `iaload` (int load) action code —
    // proving the per-opcode threading, not the old fixed byte-store code.
    assert_eq!(
        TEST_NPE_ACTION.with(|c| c.get()),
        // Widening: narrower int -> i64
        npe_action::ALOAD_INT as i64,
        "null iaload must report the ALOAD_INT action"
    );
}

#[test]
pub(in crate::x64::tests) fn test_inline_castore_null_threads_char_action() {
    // void f(char[] arr) { arr[0] = 'x'; }  with arr == null.
    // Proves a *store* of a *char[]* threads ASTORE_CHAR — i.e. both the
    // direction (store) and the element type (char) are now precise,
    // where the old single shared stub fabricated ASTORE_BYTE for every
    // inline array opcode.
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0      (array)
        0x03, // 1: iconst_0     (index)
        0x10, 0x78, // 2: bipush 'x'
        0x55, // 4: castore
        0xb1, // 5: return
        0, 0,
    ];
    let compiled = compile_array_test(&code, 6, 1, 1);

    TEST_NPE_HIT.with(|c| c.set(false));
    TEST_NPE_ACTION.with(|c| c.set(-1));
    TEST_NPE_TRAP_KEY.with(|c| c.set(-1));
    // SAFETY: JIT-compiled code from valid bytecode; mmap region executable.
    let result = unsafe { compiled.try_call(&[0]).expect("test JIT call") }; // null array
    assert_eq!(result, i64::MIN, "null castore must deopt with sentinel");
    assert!(
        TEST_NPE_HIT.with(|c| c.get()),
        "null castore must take the NPE deopt stub"
    );
    assert_eq!(
        TEST_NPE_ACTION.with(|c| c.get()),
        // Widening: narrower int -> i64
        npe_action::ASTORE_CHAR as i64,
        "null castore must report the ASTORE_CHAR action"
    );
}
