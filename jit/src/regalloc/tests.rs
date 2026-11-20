use super::*;

#[test]
fn a_wide_local_read_in_a_handler_is_decoded() {
    // wide iload 5; ireturn -- slot 5 is not safe on entry.
    let code = [0xc4, 0x15, 0x00, 0x05, 0xac];
    assert!(handler_has_unsafe_local_read(&code, code.len(), 0, 0));
    assert!(!handler_has_unsafe_local_read(&code, code.len(), 0, 1 << 5));
    // wide istore 5; wide iinc 5 1; wide iload 5; ireturn -- the store
    // makes the later reads safe.
    let code = [
        0x03, 0xc4, 0x36, 0x00, 0x05, 0xc4, 0x84, 0x00, 0x05, 0x00, 0x01, 0xc4, 0x15, 0x00,
        0x05, 0xac,
    ];
    assert!(!handler_has_unsafe_local_read(&code, code.len(), 0, 0));
    // wide iinc 70 1 reads a slot the mask cannot name.
    let code = [0xc4, 0x84, 0x00, 70, 0x00, 0x01, 0xb1];
    assert!(handler_has_unsafe_local_read(
        &code,
        code.len(),
        0,
        u64::MAX
    ));
}

/// Round 11: a parameter the method reassigns is not restored by the
/// params-only handler frame (it restores the INCOMING value), so a
/// handler reading it is unsafe.
///
/// `static int f(int x) { try { x = 42; thrower(); } catch (E e) { return x; } return -1; }`
#[test]
fn a_handler_reading_a_reassigned_parameter_is_unsafe() {
    //  0: bipush 42   2: istore_0   3: invokestatic #2   6: iconst_m1   7: ireturn
    //  8: astore_1  (handler)       9: iload_0          10: ireturn
    let code = [
        0x10, 0x2a, 0x3b, 0xb8, 0x00, 0x02, 0x02, 0xac, 0x4c, 0x1a, 0xac,
    ];
    assert!(
        handler_has_unsafe_local_read(&code, code.len(), 8, 1),
        "the catch reads x, which the try body overwrote"
    );
    // Control: the same method without the store keeps x safe.
    let code = [
        0x00, 0x00, 0x00, 0xb8, 0x00, 0x02, 0x02, 0xac, 0x4c, 0x1a, 0xac,
    ];
    assert!(!handler_has_unsafe_local_read(&code, code.len(), 8, 1));
    // A handler that stores the parameter itself before reading it is
    // still safe: the dataflow sees the handler-local definition.
    //  8: astore_1  9: iconst_5  10: istore_0  11: iload_0  12: ireturn
    let code = [
        0x10, 0x2a, 0x3b, 0xb8, 0x00, 0x02, 0x02, 0xac, 0x4c, 0x08, 0x3b, 0x1a, 0xac,
    ];
    assert!(!handler_has_unsafe_local_read(&code, code.len(), 8, 1));
}

// T1.1.22-25 — invariant checker unit tests.
//
// These exercise `regalloc_invariants_hold` directly so any
// future change to the graph-coloring allocator is immediately
// cross-checked against the three structural invariants.

#[test]
fn invariants_accept_clean_assignment() {
    // 2 non-interfering GPR locals in different registers.
    let gpr = vec![Some(12), Some(13), None];
    let xmm = vec![None, None, Some(8)];
    let interference = vec![0u64, 0, 0];
    let float_mask = 0b100; // local 2 is float
    assert!(regalloc_invariants_hold(
        &gpr,
        &xmm,
        &interference,
        float_mask,
        3
    ));
}

#[test]
fn invariants_reject_gpr_xmm_overlap() {
    let gpr = vec![Some(12)];
    let xmm = vec![Some(8)]; // same local in both!
    let interference = vec![0u64];
    assert!(!regalloc_invariants_hold(&gpr, &xmm, &interference, 0, 1));
}

#[test]
fn invariants_reject_float_in_gpr() {
    let gpr = vec![Some(12)];
    let xmm = vec![None];
    let interference = vec![0u64];
    let float_mask = 0b1; // local 0 is float but assigned a GPR
    assert!(!regalloc_invariants_hold(
        &gpr,
        &xmm,
        &interference,
        float_mask,
        1
    ));
}

#[test]
fn invariants_reject_int_in_xmm() {
    let gpr = vec![None];
    let xmm = vec![Some(8)];
    let interference = vec![0u64];
    let float_mask = 0; // local 0 is int but assigned an XMM
    assert!(!regalloc_invariants_hold(
        &gpr,
        &xmm,
        &interference,
        float_mask,
        1
    ));
}

// CM-FASTMATH — `ldc`/`ldc_w`/`ldc2_w` were missing from `bytecode_analysis::step` (the
// x64.rs `bytecode_analysis::step` twin had them; this copy didn't), so liveness
// walks read constant-pool index operand bytes as opcodes. Pin the
// constant-load lengths so the tables cannot drift apart again.
#[test]
fn bc_len_constant_loads() {
    // opcode byte + dummy operand bytes; `bytecode_analysis::step` only looks at code[pc]
    // for these arms.
    assert_eq!(bytecode_analysis::step(&[0x12, 0xBB], 0), 2, "ldc");
    assert_eq!(bytecode_analysis::step(&[0x13, 0x00, 0xBB], 0), 3, "ldc_w");
    assert_eq!(bytecode_analysis::step(&[0x14, 0x00, 0xBB], 0), 3, "ldc2_w");
    assert_eq!(bytecode_analysis::step(&[0x10, 0x7F], 0), 2, "bipush");
    assert_eq!(bytecode_analysis::step(&[0x11, 0x12, 0x34], 0), 3, "sipush");
    assert_eq!(bytecode_analysis::step(&[0xa8, 0x00, 0x10], 0), 3, "jsr");
    assert_eq!(bytecode_analysis::step(&[0xa9, 0x04], 0), 2, "ret");
}

// Defense-in-depth (same class as the missing-`ldc` CM-FASTMATH bug): the
// 5-byte instructions invokeinterface (0xb9), invokedynamic (0xba), goto_w
// (0xc8) and jsr_w (0xc9). Only invokeinterface is reachable today (the
// other three are rejected by `jit_scan`), but a missing length entry
// under-counts the instruction by 4 bytes and desyncs every PC-stepping
// walk — so all four must read 5, and must match the x64.rs
// `bytecode_analysis::step` twin. Operand bytes are dummies; `bytecode_analysis::step` only reads
// `code[pc]` for these arms.
#[test]
fn bc_len_five_byte_ops() {
    assert_eq!(
        bytecode_analysis::step(&[0xb9, 0x00, 0x10, 0x02, 0x00], 0),
        5,
        "invokeinterface"
    );
    assert_eq!(
        bytecode_analysis::step(&[0xba, 0x00, 0x10, 0x00, 0x00], 0),
        5,
        "invokedynamic"
    );
    assert_eq!(
        bytecode_analysis::step(&[0xc8, 0x00, 0x00, 0x00, 0x10], 0),
        5,
        "goto_w"
    );
    assert_eq!(
        bytecode_analysis::step(&[0xc9, 0x00, 0x00, 0x00, 0x10], 0),
        5,
        "jsr_w"
    );
}

/// `live_locals_per_pc_all` must be a strict SUPERSET of what the 64-local
/// analysis could answer: window 0 identical, and a window above it that
/// the old code could not express at all.
///
/// Two assertions, and only the second one can fail without the change —
/// the first would pass against the old code too, which is exactly why it
/// is not on its own.
#[test]
fn live_locals_per_pc_all_answers_above_slot_63() {
    // iload 70; istore 70; iload 70; return
    //  0: 0x15 0x46   iload 70
    //  2: 0x36 0x46   istore 70
    //  4: 0x15 0x46   iload 70
    //  6: 0xb1        return
    let code = [0x15, 0x46, 0x36, 0x46, 0x15, 0x46, 0xb1];
    let code_len = code.len();
    let (rows, covered, words) = live_locals_per_pc_all(&code, code_len, 0, &[], &[], 71);
    assert_eq!(words, 2, "71 locals needs two 64-slot windows");
    assert!(covered[0] && covered[2] && covered[4]);

    // Window 0 is bit-for-bit the pre-existing answer, so a method with 64
    // locals or fewer cannot have changed.
    let (old_rows, old_covered) =
        live_locals_per_pc_with_handlers(&code, code_len, 0, &[], &[]);
    for pc in 0..=code_len {
        assert_eq!(
            rows[pc * words],
            old_rows[pc],
            "window 0 must equal the 64-local analysis at pc {pc}"
        );
        assert_eq!(covered[pc], old_covered[pc], "coverage differs at pc {pc}");
    }

    // Slot 70 lives in window 1, bit 6. It is USED at pc 0 and pc 4 and
    // DEFINED at pc 2, so it is live at 0 (a use), dead at 2 (a def whose
    // old value nothing reads) and live again at 4. The old analysis could
    // express none of this: every row was a single `u64` and slot 70 had
    // no bit at all, so the snapshot had to assume it live everywhere.
    let bit = 1u64 << (70 - 64);
    assert_ne!(rows[0 * words + 1] & bit, 0, "slot 70 is live at its use");
    assert_eq!(
        rows[2 * words + 1] & bit,
        0,
        "slot 70 is dead at the store that overwrites it"
    );
    assert_ne!(
        rows[4 * words + 1] & bit,
        0,
        "slot 70 is live at its second use"
    );
}

/// The window form of the parameter seed must place a parameter slot in the
/// window that owns it, and nowhere else.
#[test]
fn param_live_in_mask_windowed_places_a_high_param_in_its_own_window() {
    assert_eq!(param_live_in_mask_windowed(0, &[70], 0), 0);
    assert_eq!(param_live_in_mask_windowed(0, &[70], 64), 1u64 << 6);
    // The `num_params`-only form (no explicit slot list) windows the same way.
    assert_eq!(param_live_in_mask_windowed(3, &[], 0), 0b111);
    assert_eq!(param_live_in_mask_windowed(3, &[], 64), 0);
    assert_eq!(param_live_in_mask_windowed(66, &[], 64), 0b11);
}

#[test]
fn local_access_decodes_wide_locals() {
    assert_eq!(
        local_access(&[0xc4, 0x15, 0x00, 0x3f], 0),
        Some((63, true, false)),
        "wide iload"
    );
    assert_eq!(
        local_access(&[0xc4, 0x39, 0x00, 0x3e], 0),
        Some((62, false, true)),
        "wide dstore"
    );
    assert_eq!(
        local_access(&[0xc4, 0x84, 0x00, 0x05, 0x00, 0x01], 0),
        Some((5, true, true)),
        "wide iinc"
    );
    assert_eq!(
        local_access(&[0xc4, 0xa9, 0x00, 0x05], 0),
        None,
        "wide ret is not a Java-value local access for register allocation"
    );
    assert_eq!(local_access(&[0x15], 0), None, "truncated iload");
}

#[test]
fn find_float_locals_decodes_wide_float_and_double_locals() {
    #[rustfmt::skip]
    let code = [
        0xc4, 0x17, 0x00, 0x05, // wide fload 5
        0xc4, 0x39, 0x00, 0x06, // wide dstore 6
        0xaf,                   // dreturn
    ];
    let mask = find_float_locals(&code, code.len(), 7);
    assert_ne!(
        mask & (1u64 << 5),
        0,
        "wide fload local should be float-classified"
    );
    assert_ne!(
        mask & (1u64 << 6),
        0,
        "wide dstore local should be float-classified"
    );
}

#[test]
fn find_non_float_locals_covers_every_gpr_category_encoding() {
    #[rustfmt::skip]
    let code = [
        0x1a,                   // iload_0
        0x1f,                   // lload_1
        0x2c,                   // aload_2
        0x3e,                   // istore_3
        0x15, 0x04,             // iload 4
        0x37, 0x05,             // lstore 5
        0x3a, 0x06,             // astore 6
        0x84, 0x07, 0x01,       // iinc 7, 1
        0xc4, 0x15, 0x00, 0x08, // wide iload 8
        0xc4, 0x84, 0x00, 0x09, 0x00, 0x01, // wide iinc 9, 1
        0xb1,                   // return
    ];
    let mask = find_non_float_locals(&code, code.len());
    for slot in 0..=9u32 {
        assert_ne!(
            mask & (1u64 << slot),
            0,
            "slot {slot} is accessed in the GPR category and must be flagged"
        );
    }
    assert_eq!(mask & (1u64 << 10), 0, "untouched slot must stay clear");
}

/// A slot javac reuses across type categories (an `int` loop counter, then
/// a `double` once the counter's scope ends) must get NEITHER a GPR nor an
/// XMM home: `iload` would read the frame slot while `dload` read the XMM,
/// and the OSR trampoline seeds only the register. See
/// `find_non_float_locals` for the measured miscompile.
#[test]
fn dual_category_slot_gets_no_register_home() {
    #[rustfmt::skip]
    let code = [
        0x0e,                   // dconst_0
        0x39, 0x00,             // dstore 0        (double sum -> slot 0)
        0x03,                   // iconst_0
        0x36, 0x02,             // istore 2        (int i    -> slot 2)
        // loop head @6
        0x15, 0x02,             // iload 2
        0x10, 0x64,             // bipush 100
        0xa2, 0x00, 0x0a,       // if_icmpge +10 -> 20
        0x84, 0x02, 0x01,       // iinc 2, 1
        0xa7, 0xff, 0xf9,       // goto -7 -> 6
        // @20: slot 2 is reused for a double
        0x18, 0x00,             // dload 0
        0x39, 0x02,             // dstore 2        (double tail -> slot 2)
        0x18, 0x02,             // dload 2
        0xaf,                   // dreturn
    ];
    let r = allocate_registers(&code, code.len(), 4, 0, &[]);
    assert_eq!(
        r.assignments[2], None,
        "a dual-category slot must not get a GPR home"
    );
    assert_eq!(
        r.xmm_assignments[2], None,
        "a dual-category slot must not get an XMM home either -- the OSR \
         trampoline would seed the XMM and leave the frame slot the int \
         accesses read uninitialised"
    );
    // The control: slot 0 is only ever a double, so it keeps its XMM home.
    assert!(
        r.xmm_assignments[0].is_some(),
        "a single-category double local must still be XMM-homed"
    );
}

#[test]
fn branch_target_decodes_wide_branches() {
    let goto_w = [0xc8, 0x00, 0x00, 0x00, 0x05, 0xb1];
    let jsr_w = [0xc9, 0x00, 0x00, 0x00, 0x05, 0xb1];
    assert_eq!(bytecode_analysis::offset_branch_target(&goto_w, 0), Some(5));
    assert_eq!(bytecode_analysis::offset_branch_target(&jsr_w, 0), Some(5));
    assert!(
        is_unconditional(0xc8),
        "goto_w is terminal for CFG fallthrough"
    );
    assert!(
        !is_unconditional(0xc9),
        "jsr_w still has a fallthrough return-address path"
    );
}

#[test]
fn build_cfg_goto_w_has_target_successor_without_fallthrough() {
    // goto_w +6 -> return at pc 6. The byte at pc 5 is dead fallthrough.
    let code = [0xc8, 0x00, 0x00, 0x00, 0x06, 0x03, 0xb1];
    let blocks = build_cfg(&code, code.len());
    let entry = blocks
        .iter()
        .find(|block| block.start_pc == 0)
        .expect("entry block");
    assert_eq!(
        entry.successors.len(),
        1,
        "goto_w should not add fallthrough"
    );
    let succ = &blocks[entry.successors[0]];
    assert_eq!(succ.start_pc, 6);
}

// CM-FASTMATH end-to-end regression: the exact bytecode shape of
// commons-math3 `FastMath.polySine(D)D` as compiled into the published
// 3.6.1 jar, where the `ldc2_w` constant-pool indices are large enough
// that their operand bytes decode as multi-byte opcodes / `athrow`
// (0xBB `new`, 0xBF `athrow`, ...). With the broken length table the
// liveness walk hit a phantom block terminator inside an operand, saw no
// further uses of local 0 (`x`), and coalesced it with local 2 (`x2`)
// onto one XMM register — so `p * x2 * x` compiled as `p * x2 * x2`
// (the FastMath.sin(3π/4)=1.2252 transform-suite miscompile). Locals 0
// and 2 interfere (both live at pc 41-43) and must never share a register.
#[test]
fn polysine_high_cp_indices_do_not_coalesce_live_doubles() {
    #[rustfmt::skip]
    let code: Vec<u8> = vec![
        0x26,             // 0:  dload_0        x
        0x26,             // 1:  dload_0        x
        0x6b,             // 2:  dmul           x*x
        0x49,             // 3:  dstore_2       x2 =
        0x14, 0x00, 0xBB, // 4:  ldc2_w #187    (0xBB = `new` if misread)
        0x39, 0x04,       // 7:  dstore 4       p =
        0x18, 0x04,       // 9:  dload 4
        0x28,             // 11: dload_2
        0x6b,             // 12: dmul
        0x14, 0x00, 0xBD, // 13: ldc2_w #189    (0xBD = `anewarray`)
        0x63,             // 16: dadd
        0x39, 0x04,       // 17: dstore 4
        0x18, 0x04,       // 19: dload 4
        0x28,             // 21: dload_2
        0x6b,             // 22: dmul
        0x14, 0x00, 0xBF, // 23: ldc2_w #191    (0xBF = `athrow`!)
        0x63,             // 26: dadd
        0x39, 0x04,       // 27: dstore 4
        0x18, 0x04,       // 29: dload 4
        0x28,             // 31: dload_2
        0x6b,             // 32: dmul
        0x14, 0x00, 0xC1, // 33: ldc2_w #193    (0xC1 = `instanceof`)
        0x63,             // 36: dadd
        0x39, 0x04,       // 37: dstore 4
        0x18, 0x04,       // 39: dload 4
        0x28,             // 41: dload_2        x2 — still live
        0x6b,             // 42: dmul
        0x26,             // 43: dload_0        x  — still live!
        0x6b,             // 44: dmul
        0x39, 0x04,       // 45: dstore 4
        0x18, 0x04,       // 47: dload 4
        0xaf,             // 49: dreturn
    ];
    let result = allocate_registers(&code, code.len(), 6, 2, &[]);
    let x = result.xmm_assignments[0];
    let x2 = result.xmm_assignments[2];
    if let (Some(rx), Some(rx2)) = (x, x2) {
        assert_ne!(
            rx, rx2,
            "locals 0 (x) and 2 (x2) are simultaneously live across pc 41-44 \
             and must not share an XMM register"
        );
    }
}

#[test]
fn invariants_reject_interfering_locals_in_same_gpr() {
    let gpr = vec![Some(12), Some(12)]; // same reg, interfere
    let xmm = vec![None, None];
    let interference = vec![0b10, 0b01]; // mutual interference
    assert!(!regalloc_invariants_hold(&gpr, &xmm, &interference, 0, 2));
}

#[test]
fn invariants_accept_non_interfering_same_gpr() {
    // Two locals sharing a register is fine iff they don't interfere.
    let gpr = vec![Some(12), Some(12)];
    let xmm = vec![None, None];
    let interference = vec![0u64, 0]; // no interference
    assert!(regalloc_invariants_hold(&gpr, &xmm, &interference, 0, 2));
}

#[test]
fn invariants_reject_interfering_locals_in_same_xmm() {
    let gpr = vec![None, None];
    let xmm = vec![Some(8), Some(8)];
    let interference = vec![0b10, 0b01];
    let float_mask = 0b11;
    assert!(!regalloc_invariants_hold(
        &gpr,
        &xmm,
        &interference,
        float_mask,
        2
    ));
}

#[test]
fn trivial_method_passes_invariants() {
    // iload_0; ireturn; + padding
    let code = [0x1a, 0xac, 0, 0];
    let result = allocate_registers(&code, 2, 1, 1, &[]);
    // The trivial method should not trip the invariant guard.
    assert!(result.assignments.iter().any(|a| a.is_some()));
}

#[test]
fn test_trivial_method() {
    // iload_0; ireturn; + padding
    let code = [0x1a, 0xac, 0, 0];
    let result = allocate_registers(&code, 2, 1, 1, &[]);
    assert_eq!(result.assignments.len(), 1);
    assert!(
        result.assignments[0].is_some(),
        "param 0 should get a register"
    );
}

#[test]
fn test_two_non_interfering_locals() {
    // Two locals used in sequence (no overlap):
    // iload_0; istore_2; iload_1; istore_3; iload_2; iload_3; iadd; ireturn
    let code = [
        0x1a, // iload_0
        0x3d, // istore_2
        0x1b, // iload_1
        0x3e, // istore_3
        0x1c, // iload_2
        0x1d, // iload_3
        0x60, // iadd
        0xac, // ireturn
        0, 0, // padding
    ];
    let result = allocate_registers(&code, 8, 4, 2, &[]);
    assert_eq!(result.assignments.len(), 4);
    // All 4 locals should get registers since K >= 4
    for i in 0..4 {
        assert!(
            result.assignments[i].is_some(),
            "local {i} should get a register"
        );
    }
}

#[test]
fn test_interference_detected() {
    // Both locals used simultaneously:
    // iload_0; iload_1; iadd; ireturn
    let code = [0x1a, 0x1b, 0x60, 0xac, 0, 0];
    let result = allocate_registers(&code, 4, 2, 2, &[]);
    // Both should get registers but DIFFERENT ones
    assert!(result.assignments[0].is_some());
    assert!(result.assignments[1].is_some());
    assert_ne!(
        result.assignments[0], result.assignments[1],
        "interfering locals must get different registers"
    );
}

#[test]
fn test_loop_uses_weighted() {
    // Simple loop: local 2 used in loop body gets higher weight
    let code = [
        0x1a, // 0: iload_0
        0x3d, // 1: istore_2
        0x1c, // 2: iload_2 (loop body start)
        0x04, // 3: iconst_1
        0x60, // 4: iadd
        0x3d, // 5: istore_2
        0xa7, 0xff, 0xfb, // 6: goto -5 → 2
        0x1c, // 9: iload_2
        0xac, // 10: ireturn
        0, 0, // padding
    ];
    let loops = vec![(2, 6)]; // loop from PC 2 to PC 6
    let counts = count_uses(&code, 11, 3, &loops);
    // local 0: 1 use outside loop
    assert_eq!(counts[0], 1);
    // local 2: used in loop (iload_2 @ 2, istore_2 @ 5) = 2 * 10 = 20, plus outside (iload_0→istore_2 @1, iload_2 @9) = 2
    assert!(
        counts[2] >= 20,
        "loop uses should be weighted higher: got {}",
        counts[2]
    );
}

#[test]
fn test_empty_method() {
    let code = [0xb1, 0, 0]; // return; padding
    let result = allocate_registers(&code, 1, 0, 0, &[]);
    assert!(result.assignments.is_empty());
    assert!(result.used_callee_saved.is_empty());
}

#[test]
fn test_many_locals_spill() {
    // More locals than registers — some must spill
    let k = LOCAL_REGS.len();
    // Create bytecode that uses k+2 locals simultaneously
    let mut code = Vec::new();
    for i in 0..=(k + 1) {
        code.push(0x15); // iload
        code.push(i as u8);
    }
    code.push(0xac); // ireturn
    code.push(0);
    code.push(0);

    let code_len = code.len() - 2;
    let num_locals = k + 2;
    let result = allocate_registers(&code, code_len, num_locals, num_locals, &[]);

    // At most K locals should get registers
    let assigned_count = result.assignments.iter().filter(|a| a.is_some()).count();
    assert!(
        assigned_count <= k,
        "should assign at most K={k} registers, got {assigned_count}"
    );
    // At least some should spill
    let spill_count = result.assignments.iter().filter(|a| a.is_none()).count();
    assert!(spill_count >= 2, "should have at least 2 spills");
}

#[test]
fn test_build_cfg_linear() {
    // Linear bytecode: iload_0; iconst_1; iadd; ireturn
    let code = [0x1a, 0x04, 0x60, 0xac, 0, 0];
    let blocks = build_cfg(&code, 4);
    assert_eq!(blocks.len(), 1, "linear code = 1 block");
    assert_eq!(blocks[0].start_pc, 0);
    assert_eq!(blocks[0].end_pc, 4);
}

#[test]
fn test_liveness_iteration_limit() {
    // Just verify that solve_liveness terminates even with loops.
    let code = [
        0x1a, // 0: iload_0
        0xa7, 0xff, 0xfe, // 1: goto -2 → 0 (infinite loop)
        0xac, // 4: ireturn (unreachable)
        0, 0,
    ];
    let mut blocks = build_cfg(&code, 5);
    for block in &mut blocks {
        compute_gen_kill(&code, block);
    }
    // Should terminate without hanging due to MAX_LIVENESS_ITERATIONS.
    assert!(
        solve_liveness(&mut blocks, 1),
        "a two-block loop converges well inside MAX_LIVENESS_ITERATIONS"
    );
}

/// A chain of `n` blocks in which block `i`'s only successor is block
/// `i - 1`, and only block 0 uses local 0.
///
/// `solve_liveness` sweeps block indices in REVERSE order, so a successor
/// with a LOWER index is always read from the previous sweep: the live bit
/// advances exactly one block per iteration. That makes the number of
/// sweeps needed equal to the chain length, which is the cheapest way to
/// build a CFG that genuinely exhausts MAX_LIVENESS_ITERATIONS. Real
/// bytecode reaches the same shape through back-edge chains in
/// obfuscated or decompiler-generated methods.
fn descending_liveness_chain(n: usize) -> Vec<BasicBlock> {
    (0..n)
        .map(|i| BasicBlock {
            start_pc: i,
            end_pc: i + 1,
            successors: if i == 0 { Vec::new() } else { vec![i - 1] },
            handler_successors: Vec::new(),
            gen: if i == 0 { 1 } else { 0 },
            kill: 0,
            live_in: 0,
            live_out: 0,
        })
        .collect()
}

#[test]
fn solve_liveness_reports_convergence_when_the_fixpoint_is_reached() {
    let mut blocks = descending_liveness_chain(16);
    assert!(solve_liveness(&mut blocks, 0));
    for (i, b) in blocks.iter().enumerate() {
        assert_eq!(b.live_in, 1, "local 0 must be live-in at block {i}");
    }
}

#[test]
fn solve_liveness_reports_failure_when_the_iteration_cap_truncates_it() {
    // One block more than the cap can propagate through, plus the extra
    // sweep a converged run needs to observe "nothing changed".
    let n = MAX_LIVENESS_ITERATIONS + 8;
    let mut blocks = descending_liveness_chain(n);
    assert!(
        !solve_liveness(&mut blocks, 0),
        "a chain longer than MAX_LIVENESS_ITERATIONS cannot have converged"
    );
    // The point of the flag: what is left behind is not "approximate", it
    // is a strict SUBSET of the truth. Local 0 IS live-in at the top of
    // the chain, and the truncated run says it is not — which as an
    // interference graph means a missing edge.
    assert_eq!(
        blocks[n - 1].live_in,
        0,
        "the truncated result under-states liveness, which is why the \
         caller must not use it"
    );
}

/// Bytecode whose liveness fixpoint cannot converge inside
/// `MAX_LIVENESS_ITERATIONS`: `iload_0; ireturn` followed by a chain of
/// `goto`s each branching to the previous one. Block indices follow pc
/// order, every `goto` block's successor has a lower index, and only the
/// first block uses local 0 — the `descending_liveness_chain` shape,
/// expressed as real bytecode so it reaches `allocate_registers_with`.
fn non_converging_goto_chain(gotos: usize) -> Vec<u8> {
    let mut code = vec![0x1a, 0xac]; // 0: iload_0  1: ireturn
    for k in 0..gotos {
        // The goto at pc `2 + 3k` targets pc `2 + 3(k-1)`, i.e. -3; the
        // first one targets pc 0, i.e. -2.
        let off: i16 = if k == 0 { -2 } else { -3 };
        code.push(0xa7);
        code.extend_from_slice(&off.to_be_bytes());
    }
    code
}

#[test]
fn allocate_registers_hands_out_no_homes_when_liveness_does_not_converge() {
    let code = non_converging_goto_chain(MAX_LIVENESS_ITERATIONS + 8);
    let code_len = code.len();

    // Sanity: this really is the shape the fixpoint cannot chew through.
    let mut blocks = build_cfg(&code, code_len);
    for block in &mut blocks {
        compute_gen_kill(&code, block);
    }
    assert!(
        blocks.len() > MAX_LIVENESS_ITERATIONS,
        "expected one block per goto, got {}",
        blocks.len()
    );
    assert!(!solve_liveness(&mut blocks, 1));

    // The allocator must refuse rather than colour a graph that is missing
    // interference edges. Every local keeps its canonical frame slot.
    let result = allocate_registers_with(
        &code,
        code_len,
        4, // num_locals
        1, // num_params
        &[],
        &[],
        &LOCAL_REGS,
        &LOCAL_XMMS,
        &[],
    );
    assert!(
        result.assignments.iter().all(|a| a.is_none()),
        "no local may receive a register home from a non-converged \
         liveness solution: {:?}",
        result.assignments
    );
    assert!(result.xmm_assignments.iter().all(|a| a.is_none()));
    assert!(result.used_callee_saved.is_empty());
    assert!(result.used_xmm_regs.is_empty());
}

#[test]
fn the_per_pc_liveness_map_widens_to_all_live_when_it_does_not_converge() {
    // The map builders cannot simply bail — their callers need a row for
    // every pc — so they take the other fail-closed direction: over-state
    // liveness. Under-stating it would publish a live local as
    // `Undefined` in a deopt frame.
    let code = non_converging_goto_chain(MAX_LIVENESS_ITERATIONS + 8);
    let live = live_locals_per_pc(&code, code.len(), 1);
    assert_eq!(
        live[2],
        u64::MAX,
        "a goto block deep in the chain must be reported all-live, not \
         all-dead, once the fixpoint is known to be truncated"
    );
}

#[test]
fn test_loop_weight_validation() {
    // Loop with header > back_edge (invalid) should not apply weight.
    let code = [0x1a, 0xac, 0, 0];
    let invalid_loops = vec![(5, 2)]; // header > back_edge
    let counts = count_uses(&code, 2, 1, &invalid_loops);
    assert_eq!(counts[0], 1, "invalid loop range should not apply weight");
}

#[test]
fn test_branch_target_negative_returns_none() {
    // Branch at pc=0 with a large negative offset should return None.
    let code = [0xa7, 0x80, 0x00]; // goto with offset -32768
    assert!(bytecode_analysis::offset_branch_target(&code, 0).is_none());
}

#[test]
fn test_build_cfg_branch() {
    // iload_0; ifne +3; iconst_0; ireturn; iconst_1; ireturn
    let code = [
        0x1a, // 0: iload_0
        0x9a, 0x00, 0x05, // 1: ifne +5 → 6
        0x03, // 4: iconst_0
        0xac, // 5: ireturn
        0x04, // 6: iconst_1
        0xac, // 7: ireturn
        0, 0,
    ];
    let blocks = build_cfg(&code, 8);
    assert!(
        blocks.len() >= 3,
        "should have at least 3 blocks, got {}",
        blocks.len()
    );
}

// ===================================================================
// ARM64 register allocator tests (Phase 95)
// ===================================================================

#[test]
fn p95_arm64_simple_method() {
    // iload_0; ireturn — single param, single local
    let code = [0x1a, 0xac, 0, 0];
    let result = allocate_registers_arm64(&code, 2, 1, 1, &[]);
    assert_eq!(result.assignments.len(), 1);
    assert!(
        result.assignments[0].is_some(),
        "param 0 should get ARM64 register"
    );
    // Should be one of X19-X28
    let reg = result.assignments[0].unwrap();
    assert!((19..=28).contains(&reg), "should be callee-saved X{reg}");
}

#[test]
fn p95_arm64_many_locals_use_all_callee_saved() {
    // 10 locals all alive simultaneously — should fill X19-X28 exactly.
    // Create bytecode that loads all 10 locals:
    let mut code = Vec::new();
    for i in 0..10u8 {
        code.push(0x15); // iload
        code.push(i);
    }
    code.push(0xac); // ireturn
    let code_len = code.len() - 0; // no padding needed for this test
    let result = allocate_registers_arm64(&code, code_len, 10, 10, &[]);
    let assigned_count = result.assignments.iter().filter(|a| a.is_some()).count();
    assert_eq!(assigned_count, 10, "all 10 locals should fit in X19-X28");
    // All should be distinct
    let mut regs: Vec<u8> = result.assignments.iter().filter_map(|a| *a).collect();
    regs.sort();
    regs.dedup();
    assert_eq!(regs.len(), 10, "all registers should be distinct");
    // All should be in 19..=28
    for r in &regs {
        assert!(
            (19..=28).contains(r),
            "register X{r} not in callee-saved range"
        );
    }
}

#[test]
fn p95_arm64_spill_with_12_locals() {
    // 12 locals all alive — 10 get registers, 2 must spill (ARM64 has 10 callee-saved GPRs).
    let mut code = Vec::new();
    for i in 0..12u8 {
        code.push(0x15); // iload
        code.push(i);
    }
    code.push(0xac); // ireturn
    let code_len = code.len();
    let result = allocate_registers_arm64(&code, code_len, 12, 12, &[]);
    let assigned_count = result.assignments.iter().filter(|a| a.is_some()).count();
    assert!(
        assigned_count <= 10,
        "at most 10 ARM64 callee-saved GPRs, got {assigned_count}"
    );
    let spill_count = result.assignments.iter().filter(|a| a.is_none()).count();
    assert!(
        spill_count >= 2,
        "should have at least 2 spills, got {spill_count}"
    );
}

#[test]
fn p95_arm64_fp_registers() {
    // Float locals should get D8-D15 (callee-saved FP regs).
    // fload_0; fload_1; fadd; freturn
    let code = [
        0x22, // fload_0
        0x23, // fload_1
        0x62, // fadd
        0xae, // freturn
        0, 0,
    ];
    let result = allocate_registers_arm64(&code, 4, 2, 2, &[]);
    // Both locals should be recognized as float and get FP assignments
    assert!(
        result.xmm_assignments[0].is_some(),
        "float local 0 should get FP register"
    );
    assert!(
        result.xmm_assignments[1].is_some(),
        "float local 1 should get FP register"
    );
    // FP regs should be in 8..=15 (D8-D15)
    let fp0 = result.xmm_assignments[0].unwrap();
    let fp1 = result.xmm_assignments[1].unwrap();
    assert!((8..=15).contains(&fp0), "FP reg {fp0} not in D8-D15 range");
    assert!((8..=15).contains(&fp1), "FP reg {fp1} not in D8-D15 range");
    assert_ne!(
        fp0, fp1,
        "interfering float locals must get different FP regs"
    );
    // GPR assignments should be None for float locals
    assert!(
        result.assignments[0].is_none(),
        "float local 0 should NOT get GPR"
    );
    assert!(
        result.assignments[1].is_none(),
        "float local 1 should NOT get GPR"
    );
}

#[test]
fn p95_arm64_mixed_int_float_locals() {
    // local 0: int param, local 1: float computed from local 0
    // iload_0; i2f; fstore_1; fload_1; freturn
    let code = [
        0x1a, // iload_0
        0x86, // i2f
        0x44, // fstore_1
        0x23, // fload_1
        0xae, // freturn
        0, 0,
    ];
    let result = allocate_registers_arm64(&code, 5, 2, 1, &[]);
    // local 0 = int → GPR
    assert!(
        result.assignments[0].is_some(),
        "int local 0 should get GPR"
    );
    assert!((19..=28).contains(&result.assignments[0].unwrap()));
    // local 1 = float → FP reg
    assert!(
        result.xmm_assignments[1].is_some(),
        "float local 1 should get FP register"
    );
    assert!((8..=15).contains(&result.xmm_assignments[1].unwrap()));
}

#[test]
fn p95_arm64_used_callee_saved_tracking() {
    // 3 locals alive — should report exactly 3 callee-saved regs used.
    let code = [
        0x1a, // iload_0
        0x1b, // iload_1
        0x60, // iadd
        0x1c, // iload_2
        0x60, // iadd
        0xac, // ireturn
        0, 0,
    ];
    let result = allocate_registers_arm64(&code, 6, 3, 3, &[]);
    assert_eq!(
        result.used_callee_saved.len(),
        3,
        "should use exactly 3 callee-saved GPRs"
    );
    for &r in &result.used_callee_saved {
        assert!((19..=28).contains(&r), "callee-saved reg X{r} not in range");
    }
}

/// Every parameter slot must be live-in at entry, including the ones a
/// category-2 parameter pushes past `num_params`.
///
/// `private void f(long, Object, Void)` on an instance: `this`=0,
/// `long`=1..2, `Object`=3, `Void`=4 — four parameters over five slots.
/// Seeding liveness with `(1 << 4) - 1` left slot 4 (the `Void`) dead on
/// entry, so the colouring was free to give it the same register as the
/// live `Object` in slot 3 — and the prologue, which stores EVERY incoming
/// argument into its home, then overwrote the `Object` with the always-null
/// `Void` on the way in. Observed as `mov r12, rcx ; mov r12, r8`.
#[test]
fn every_parameter_slot_is_live_in_even_past_a_category_two_parameter() {
    // aload_0; lload_1; aload_3; aconst_null; invokevirtual #1; return
    let code = [0x2a, 0x1f, 0x2d, 0x01, 0xb6, 0x00, 0x01, 0xb1];
    let param_slots = [0usize, 1, 3, 4];

    let result =
        allocate_registers_with_handlers(&code, code.len(), 5, 4, &param_slots, &[], &[]);
    let homes: Vec<Option<u8>> = param_slots
        .iter()
        .map(|&s| result.assignments.get(s).copied().flatten())
        .collect();
    for (i, a) in homes.iter().enumerate() {
        for (j, b) in homes.iter().enumerate() {
            if i != j {
                if let (Some(x), Some(y)) = (a, b) {
                    assert_ne!(
                        x, y,
                        "parameter slots {} and {} share register {x}; the prologue \
                         stores both and would clobber the live one",
                        param_slots[i], param_slots[j]
                    );
                }
            }
        }
    }

    // The mask helper itself: slot-derived, not count-derived.
    assert_eq!(param_live_in_mask(4, &param_slots), 0b1_1011);
    // Empty slot map keeps the historical identity layout.
    assert_eq!(param_live_in_mask(4, &[]), 0b1111);
}

/// Round 11: a parameter the entry block OVERWRITES before reading is not
/// live-in, so liveness gave it no edge to an earlier parameter that is
/// dead by the time of the overwrite — and the two shared a register the
/// prologue writes twice.
///
/// `static int f(int a, int b) { b = a * 2; return b; }`
#[test]
fn a_parameter_overwritten_before_it_is_read_still_interferes_with_the_others() {
    // 0: iload_0  1: iconst_2  2: imul  3: istore_1  4: iload_1  5: ireturn
    let code = [0x1a, 0x05, 0x68, 0x3c, 0x1b, 0xac];
    let result = allocate_registers(&code, code.len(), 2, 2, &[]);
    let a = result.assignments.first().copied().flatten();
    let b = result.assignments.get(1).copied().flatten();
    if let (Some(x), Some(y)) = (a, b) {
        assert_ne!(
            x, y,
            "parameters 0 and 1 share register {x}: the prologue stores arg1 over \
             arg0 before `iload_0` reads it"
        );
    }
}

// ── live_locals_per_pc (OSR-exit dead-local detection) ──────────────────

/// A local that ONLY the catch handler reads is live throughout the
/// protected range — the handler is a successor of every instruction in
/// it. Without exception edges the liveness scan says "dead", and the
/// consumer that acts on that (the x64 deopt/exceptional-frame snapshot)
/// then drops the value — and, for a reference, drops the GC's only view
/// of that object through this frame.
#[test]
fn live_locals_per_pc_sees_a_local_only_the_handler_reads() {
    // 0: iconst_0            9: istore_2
    // 1: istore_1           10: return
    // 2: invokestatic #2    11: return
    // 5: goto 11
    // 8: iload_1   <- handler entry, reads local 1
    // exception table: [2, 5) -> handler 8
    let code = [
        0x03, 0x3c, 0xb8, 0x00, 0x02, 0xa7, 0x00, 0x06, 0x1b, 0x3d, 0xb1, 0xb1,
    ];
    let len = code.len();
    let blind = live_locals_per_pc(&code, len, 1);
    assert_eq!(
        blind[2] & (1 << 1),
        0,
        "precondition: with no exception edges the handler's read is invisible"
    );

    let (aware, covered) = live_locals_per_pc_with_handlers(&code, len, 1, &[], &[(2, 5, 8)]);
    assert_ne!(
        aware[2] & (1 << 1),
        0,
        "local 1 must be live at the protected pc — the handler reads it"
    );
    assert!(
        covered[8],
        "the handler pc must become a covered block start"
    );
    assert_eq!(
        aware[1] & (1 << 1),
        0,
        "the exception edge must not make the local live BEFORE its store"
    );
}

#[test]
fn live_locals_per_pc_marks_loop_counter_dead_after_loop_exit() {
    // Mirrors the shape of `TestOutputBuffer.WritingServlet.doGet`'s
    // `for (int i = 0; i < writeCount; i++) w.write(...)` loop: a counter
    // local that is live throughout the loop body but genuinely dead once
    // control reaches the post-loop code (here, returning a DIFFERENT
    // local). Local 0 = returned after the loop, local 1 = loop counter
    // `i`, local 2 = the loop bound.
    #[rustfmt::skip]
    let code: [u8; 15] = [
        0x03,             // 0:  iconst_0
        0x3c,             // 1:  istore_1        i = 0
        0x1b,             // 2:  iload_1          <- LOOP HEADER
        0x1c,             // 3:  iload_2
        0xa2, 0x00, 0x09, // 4:  if_icmpge +9 -> pc13 (EXIT)
        0x84, 0x01, 0x01, // 7:  iinc 1, 1
        0xa7, 0xff, 0xf8, // 10: goto -8 -> pc2 (LOOP HEADER)
        0x1a,             // 13: iload_0          <- EXIT
        0xac,             // 14: ireturn
    ];
    let live_at = live_locals_per_pc(&code, code.len(), 3);

    // Inside the loop (the `iinc`), the counter is live: the loop
    // condition and the increment itself both still need it.
    assert_ne!(
        live_at[7] & (1 << 1),
        0,
        "loop counter (local 1) must be live at the iinc inside the loop"
    );

    // At the post-loop `iload_0` the counter is dead — nothing after this
    // point reads local 1 before the method returns.
    assert_eq!(
        live_at[13] & (1 << 1),
        0,
        "loop counter (local 1) must be dead once the loop has exited"
    );
    // The local the post-loop code actually reads IS live there.
    assert_ne!(
        live_at[13] & (1 << 0),
        0,
        "local 0 must be live at the iload_0 that reads it"
    );
}

#[test]
fn live_locals_per_pc_straight_line_no_locals_live_after_last_use() {
    let code: [u8; 6] = [
        0x1a, // 0: iload_0
        0x1b, // 1: iload_1
        0x60, // 2: iadd
        0x3c, // 3: istore_1   (local 1 redefined — its old value is dead)
        0x1c, // 4: iload_2
        0xac, // 5: ireturn (return value must be int on stack; irrelevant here)
    ];
    let live_at = live_locals_per_pc(&code, code.len(), 3);
    // At the final `iload_2`, locals 0 and 1 are both dead: local 0's only
    // use was at pc0, and local 1 was just overwritten at pc3 without a
    // subsequent read before the method returns.
    assert_eq!(live_at[4] & (1 << 0), 0, "local 0 dead after its only use");
    assert_eq!(
        live_at[4] & (1 << 1),
        0,
        "local 1 dead after being overwritten"
    );
    assert_ne!(
        live_at[4] & (1 << 2),
        0,
        "local 2 live at the iload_2 that reads it"
    );
}

// -----------------------------------------------------------------------
// CFG coverage after an unconditional terminator (handler-shaped region)
// -----------------------------------------------------------------------

/// The shape this fixes: a protected region that ends in `ireturn`,
/// followed by an exception handler that is not a branch target. Before
/// the fix, `build_cfg`'s second pass skipped every PC from the handler's
/// `astore` to the end of the method (nothing marked them as block
/// starts), so the handler's local accesses were invisible to liveness and
/// interference.
const HANDLER_AFTER_RETURN: [u8; 9] = [
    0x1a, // 0: iload_0
    0xac, // 1: ireturn                  <- protected region ends here
    0x4c, // 2: astore_1  (handler entry: store the pending exception)
    0x12, 0x01, // 3: ldc #1
    0x4d, // 5: astore_2
    0x2c, // 6: aload_2
    0x2b, // 7: aload_1
    0xbf, // 8: athrow
];

#[test]
fn cfg_covers_code_after_an_unconditional_terminator() {
    let code = HANDLER_AFTER_RETURN;
    let live = live_locals_per_pc(&code, code.len(), 1);
    // The handler reads locals 1 and 2 at pc7/pc6. If the region were
    // uncovered, every entry here would be the `0` default.
    assert_ne!(
        live[7] & (1 << 1),
        0,
        "local 1 must be live at the aload_1 inside the handler"
    );
    assert_ne!(
        live[6] & (1 << 2),
        0,
        "local 2 must be live at the aload_2 inside the handler"
    );
}

#[test]
fn interference_sees_locals_defined_after_a_return() {
    let code = HANDLER_AFTER_RETURN;
    let mut blocks = build_cfg(&code, code.len());
    for b in &mut blocks {
        compute_gen_kill(&code, b);
    }
    assert!(solve_liveness(&mut blocks, 1));
    let interference = build_interference(&code, &blocks, 3, &[]);
    // Locals 1 and 2 are simultaneously live at the `aload_2; aload_1`
    // pair, so they MUST interfere — otherwise the colourer is free to
    // give them the same register and the athrow rethrows the wrong ref.
    assert_ne!(
        interference[1] & (1 << 2),
        0,
        "locals 1 and 2 are simultaneously live in the handler and must interfere"
    );
    assert_ne!(
        interference[2] & (1 << 1),
        0,
        "interference must be symmetric"
    );
}

/// A local that ONLY the exception handler reads must not be given the
/// register of a local that is live across the protected range.
///
/// Without exception edges the handler block has no predecessor, so such a
/// local's live range is the handler alone: it interferes with nothing in
/// the try and the colorer hands both locals the same physical register.
/// The precise exceptional frame then reconstructs the handler's local from
/// that register and reads the OTHER local's value — an unrelated object
/// standing where a live one was, which is exactly the json-smart symptom
/// this whole family produced.
#[test]
fn handler_only_local_does_not_share_a_register_with_a_live_local() {
    // 0: aconst_null   1: astore_1        (local 1 — read ONLY by the handler)
    // 2: aconst_null   3: astore_2        (local 2 — read after the try)
    // 4: nop           5: nop             protected [4,6)
    // 6: goto +6 -> 12
    // 9: astore_3     10: aload_1  11: areturn      <- handler at 9
    // 12: aload_2     13: areturn
    let code: Vec<u8> = vec![
        0x01, 0x4c, 0x01, 0x4d, 0x00, 0x00, 0xa7, 0x00, 0x06, 0x4e, 0x2b, 0xb0, 0x2c, 0xb0,
    ];
    let code_len = code.len();
    let handlers = [(4usize, 6usize, 9usize)];

    // Assert the INTERFERENCE relation, not a coloring outcome: with four
    // locals and a full callee-saved file the colorer has no pressure to
    // alias, so "they got the same register" is not a reliable statement of
    // the hazard — "the allocator does not know they are simultaneously
    // live" is.
    let interference = |handlers: &[(usize, usize, usize)]| -> u64 {
        let mut blocks = build_cfg_with_handlers(&code, code_len, handlers);
        for block in &mut blocks {
            compute_gen_kill(&code, block);
        }
        assert!(solve_liveness(&mut blocks, 0));
        build_interference(&code, &blocks, 4, &[])[1]
    };

    assert_eq!(
        interference(&[]) & (1 << 2),
        0,
        "documents the hazard: without exception edges the handler-only local \
         does not interfere with the local live across the try, so the colorer \
         is free to give them the same register"
    );
    assert_ne!(
        interference(&handlers) & (1 << 2),
        0,
        "with exception edges the handler-only local is live across the \
         protected range and must interfere with the local live there"
    );

    // ...and the allocator built on that graph must keep them apart.
    let modelled = allocate_registers_with_handlers(&code, code_len, 4, 0, &[], &[], &handlers);
    assert!(
        modelled.assignments[1].is_some() && modelled.assignments[2].is_some(),
        "modelling handlers must not de-register-home either local"
    );
    assert_ne!(modelled.assignments[1], modelled.assignments[2]);
}

#[test]
fn find_reference_locals_sees_handler_locals_after_a_return() {
    // `find_reference_locals` walks raw bytecode, not the CFG, so it was
    // never affected — pin that, because the publish plan's soundness
    // rests on it covering handler code the CFG previously missed.
    let code = HANDLER_AFTER_RETURN;
    let refs = find_reference_locals(&code, code.len(), 3);
    assert_ne!(refs & (1 << 1), 0, "local 1 is astore_1/aload_1'd");
    assert_ne!(refs & (1 << 2), 0, "local 2 is astore_2/aload_2'd");
    assert_eq!(refs & 1, 0, "local 0 is only ever iload'ed");
}

// -----------------------------------------------------------------------
// Safepoint publication planning
// -----------------------------------------------------------------------

/// `int fib(int n) { return n < 2 ? n : fib(n-1) + fib(n-2); }`-shaped
/// body: one int local, two `invokestatic`s. Nothing here can hold an oop,
/// so a GC-capable call site has nothing to publish.
#[test]
fn publish_plan_is_empty_for_an_int_only_recursive_kernel() {
    let code: Vec<u8> = vec![
        0x1a, // 0: iload_0
        0x05, // 1: iconst_2
        0xa2, 0x00, 0x05, // 2: if_icmpge +5 -> 7
        0x1a, // 5: iload_0
        0xac, // 6: ireturn
        0x1a, // 7: iload_0
        0x04, // 8: iconst_1
        0x64, // 9: isub
        0xb8, 0x00, 0x01, // 10: invokestatic #1
        0x1a, // 13: iload_0
        0x05, // 14: iconst_2
        0x64, // 15: isub
        0xb8, 0x00, 0x01, // 16: invokestatic #1
        0x60, // 19: iadd
        0xac, // 20: ireturn
    ];
    let code_len = code.len();
    // Pretend the colourer gave local 0 a register home.
    let assignments = vec![Some(12u8)];
    let plan = plan_safepoint_publication(&code, code_len, 1, 1, &[], &assignments, 0, &[]);

    assert_eq!(plan.reference_locals, 0, "no aload/astore anywhere");
    assert_eq!(plan.register_homed_reference_locals, 0);
    assert_eq!(plan.publish_always, 0);
    assert!(
        plan.no_reference_in_registers(),
        "an int-only kernel must be allowed to skip local publication"
    );
    // At both call sites there is nothing to publish.
    assert_eq!(plan.publish_at_bci(10), 0);
    assert_eq!(plan.publish_at_bci(16), 0);
}

#[test]
fn publish_plan_keeps_register_homed_reference_locals() {
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0   (local 0 is a reference)
        0x4c, // 1: astore_1  (local 1 is a reference)
        0x1c, // 2: iload_2   (local 2 is an int)
        0xb8, 0x00, 0x01, // 3: invokestatic #1
        0x2b, // 6: aload_1   (keeps local 1 live across the call)
        0xb0, // 7: areturn
    ];
    let code_len = code.len();
    // Locals 0 and 1 are refs, local 2 is an int; all three get registers.
    let assignments = vec![Some(12u8), Some(13u8), Some(14u8)];
    let plan = plan_safepoint_publication(&code, code_len, 3, 1, &[], &assignments, 0, &[]);

    assert_eq!(plan.reference_locals, 0b011);
    assert_eq!(plan.register_homed_reference_locals, 0b011);
    assert!(
        !plan.no_reference_in_registers(),
        "a register-homed reference local must force publication"
    );
    // Local 1 is read after the call, so it must be published at the call.
    assert_ne!(
        plan.publish_at_bci(3) & (1 << 1),
        0,
        "a reference local live across the call must be published"
    );
    // Local 2 is an int — never published regardless of liveness.
    assert_eq!(
        plan.publish_at_bci(3) & (1 << 2),
        0,
        "a primitive local is never a GC root and must not be published"
    );
}

#[test]
fn publish_plan_narrows_by_liveness_but_never_below_a_dead_reference() {
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0x4c, // 1: astore_1
        0x2b, // 2: aload_1     <- local 1's LAST read
        0x57, // 3: pop
        0xb8, 0x00, 0x01, // 4: invokestatic #1  (local 1 is dead here)
        0xb1, // 7: return
    ];
    let code_len = code.len();
    let assignments = vec![Some(12u8), Some(13u8)];
    let plan = plan_safepoint_publication(&code, code_len, 2, 1, &[], &assignments, 0, &[]);
    assert_ne!(plan.register_homed_reference_locals & (1 << 1), 0);
    assert_eq!(
        plan.publish_at_bci(4) & (1 << 1),
        0,
        "a reference local whose last read is behind the call is not a live root"
    );
    // The bci-independent set is unaffected by liveness — a consumer that
    // does not want the exception-handler-CFG precondition uses this.
    assert_ne!(plan.publish_always & (1 << 1), 0);
}

/// Uncovered PCs (no basic block reaches them) read `0` from the liveness
/// table by default, which must NOT be mistaken for "nothing live".
#[test]
fn publish_plan_falls_back_to_conservative_set_on_uncovered_pcs() {
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0x4c, // 1: astore_1
        0xb1, // 2: return
    ];
    let code_len = code.len();
    let assignments = vec![Some(12u8), Some(13u8)];
    let plan = plan_safepoint_publication(&code, code_len, 2, 1, &[], &assignments, 0, &[]);
    // pc past the end of the code is definitionally uncovered.
    assert_eq!(
        plan.publish_at_bci(code_len + 32),
        plan.publish_always,
        "an out-of-range bci must fall back to the conservative set"
    );
}

#[test]
fn publish_plan_ignores_locals_without_a_register_home() {
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0x4c, // 1: astore_1
        0xb8, 0x00, 0x01, // 2: invokestatic #1
        0x2b, // 5: aload_1
        0xb0, // 6: areturn
    ];
    let code_len = code.len();
    // Local 1 spilled (no register home) — its canonical frame slot is
    // already authoritative, so it is not part of the publish set.
    let assignments = vec![Some(12u8), None];
    let plan = plan_safepoint_publication(&code, code_len, 2, 1, &[], &assignments, 0, &[]);
    assert_ne!(plan.reference_locals & (1 << 1), 0);
    assert_eq!(
        plan.register_homed_reference_locals & (1 << 1),
        0,
        "a frame-homed reference local needs no publication"
    );
}

#[test]
fn publish_plan_unions_the_caller_supplied_reference_parameter_mask() {
    // `void f(Object unused)` whose parameter is never read: no `aload`
    // taints slot 0, so only the caller's descriptor-derived mask knows.
    let code: Vec<u8> = vec![0xb1]; // return
    let assignments = vec![Some(12u8)];
    let plan = plan_safepoint_publication(&code, code.len(), 1, 1, &[], &assignments, 0b1, &[]);
    assert_ne!(plan.reference_locals & 1, 0);
    assert_ne!(plan.register_homed_reference_locals & 1, 0);
    assert!(!plan.no_reference_in_registers());
}

// -----------------------------------------------------------------------
// Save-area sizing — the historical spill-overflow defect
// -----------------------------------------------------------------------
//
// The x64 prologue sizes its callee-saved save area as
// `used_callee_saved.len() * 8` and writes register `used_callee_saved[i]`
// at `callee_saved_base + i*8`; the per-safepoint spill region is sized
// from the SAME vector. If `used_callee_saved` ever reported fewer
// registers than `assignments` actually uses, the prologue would under-
// reserve and the safepoint spill would write past its region into the
// caller's saved registers — the documented `Token.duplicate` corruption.
// Pin the contract from this side.

fn assert_save_area_contract(result: &RegAllocResult, pool: &[u8]) {
    let mut seen: Vec<u8> = result.assignments.iter().flatten().copied().collect();
    seen.sort_unstable();
    seen.dedup();
    let mut reported = result.used_callee_saved.clone();
    reported.sort_unstable();
    assert_eq!(
        seen, reported,
        "used_callee_saved must be exactly the deduped set of assigned registers"
    );
    for r in &result.used_callee_saved {
        assert!(
            pool.contains(r),
            "assigned register {r} is outside the allocation pool {pool:?}"
        );
    }
    let mut dedup = result.used_callee_saved.clone();
    dedup.dedup();
    assert_eq!(
        dedup.len(),
        result.used_callee_saved.len(),
        "used_callee_saved must not contain duplicates — each entry owns one 8-byte save slot"
    );
    assert!(
        result.used_callee_saved.len() <= pool.len(),
        "save area can never need more slots than the pool has registers"
    );
}

#[test]
fn save_area_contract_holds_for_a_register_pressured_method() {
    // 12 distinct, simultaneously-live locals against a 5/7-register pool:
    // forces spilling, which is exactly when the reported save-area set
    // and the actual assignments are most likely to drift apart.
    let mut code: Vec<u8> = Vec::new();
    for slot in 0..12u8 {
        code.push(0x15); // iload <slot>
        code.push(slot);
    }
    for slot in (0..12u8).rev() {
        code.push(0x36); // istore <slot>
        code.push(slot);
    }
    code.push(0xb1); // return
    let code_len = code.len();
    let result = allocate_registers_with(
        &code,
        code_len,
        12,
        12,
        &[],
        &[],
        &ARM64_LOCAL_GPRS,
        &ARM64_LOCAL_FPS,
        &[],
    );
    assert_save_area_contract(&result, &ARM64_LOCAL_GPRS);
}

#[test]
fn save_area_contract_holds_for_the_handler_shaped_method() {
    let code = HANDLER_AFTER_RETURN;
    let result = allocate_registers_with(
        &code,
        code.len(),
        3,
        1,
        &[],
        &[],
        &ARM64_LOCAL_GPRS,
        &ARM64_LOCAL_FPS,
        &[],
    );
    assert_save_area_contract(&result, &ARM64_LOCAL_GPRS);
}

#[test]
fn save_area_contract_holds_when_every_local_spills() {
    // Zero available registers → every local must spill and the save area
    // must be empty (a non-empty one would reserve slots the prologue
    // never writes and shift every later frame region).
    let code: Vec<u8> = vec![0x1a, 0x1b, 0x60, 0x3c, 0xb1];
    let result = allocate_registers_with(&code, code.len(), 2, 2, &[], &[], &[], &[], &[]);
    assert!(result.assignments.iter().all(Option::is_none));
    assert!(result.used_callee_saved.is_empty());
}
