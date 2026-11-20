#[allow(unused_imports)]
use super::*;


// -- to_reg / to_fpreg tests (C11) --------------------------------------
//
// Note: `to_reg` / `to_fpreg` return `Option` and neither asserts. The
// `debug_assert!`s they used to carry fired on exactly the inputs the
// refusal path exists for, which made that path untestable and made a
// debug build abort where a release build merely declined the method.

#[test]
pub(in crate::aarch64_backend::tests) fn every_gpr_encoding_converts_and_thirty_one_is_not_a_gpr() {
    // 0..=30 are general-purpose registers through every conversion.
    for n in 0..=30u8 {
        assert!(crate::aarch64::Reg::from_u8(n).is_some(), "X{n} is a GPR");
        assert_eq!(r_zr(Arm64Register(n)).enc(), u32::from(n), "r_zr X{n}");
        assert_eq!(r(Arm64Register(n)).enc(), u32::from(n), "r X{n}");
    }
    // 31 is not a GPR. It is SP to `r` and XZR to `r_zr` -- the same bits,
    // and before A8 the same VALUE, which is the whole reason for the split.
    assert!(crate::aarch64::Reg::from_u8(31).is_none());
    assert_eq!(r(Arm64Register(31)), crate::aarch64::SP);
    assert_eq!(r_zr(Arm64Register(31)), crate::aarch64::XZR);
    assert_eq!(r(Arm64Register(31)).enc(), 31);
    assert_eq!(r_zr(Arm64Register(31)).enc(), 31);
    assert!(!take_invalid_register(), "none of the above is a violation");
}

#[test]
pub(in crate::aarch64_backend::tests) fn to_fpreg_accepts_v0_through_v7() {
    for n in 32..=39u8 {
        assert!(
            to_fpreg(Arm64Register(n)).is_some(),
            "to_fpreg({n}) should be Some for valid FP reg"
        );
    }
}

/// A5. A register the allocator should never have produced must cost the
/// METHOD its compiled body, not the process.
///
/// `r` and `fp` used to `.expect()`. The FP one has already been a live
/// panic once, when `D8`-`D15` arrived as `Arm64Register(40..=47)`.
#[test]
pub(in crate::aarch64_backend::tests) fn an_unencodable_register_refuses_the_body_instead_of_panicking() {
    assert_eq!(
        crate::aarch64::Reg::from_u8(32),
        None,
        "32 is not a GPR encoding"
    );
    // 40 is D8, which IS encodable: `FpReg` models D0..D31, so the FP half
    // of the register file runs 32..=63. An earlier revision asserted
    // `None` here on the strength of `to_fpreg`'s doc comment, which said
    // "V0=32..V7=39" — that describes the register ALLOCATOR's pool, not
    // the conversion's range, and the comment is corrected with this test.
    assert_eq!(
        to_fpreg(Arm64Register(40)),
        Some(crate::aarch64::FpReg::D8),
        "40 is D8 and is a valid encoding"
    );
    assert_eq!(to_fpreg(Arm64Register(64)), None, "64 is past D31");

    // The shorthands must not panic, and must leave the sticky flag set.
    let _ = take_invalid_register();
    let _ = r(Arm64Register(32));
    assert!(
        take_invalid_register(),
        "`r` must record an unencodable register"
    );
    let _ = fp(Arm64Register(64));
    assert!(
        take_invalid_register(),
        "`fp` must record an unencodable register"
    );
    // ...and reading it clears it, so one bad method does not poison the
    // next one compiled on this thread.
    assert!(!take_invalid_register());
}

/// A8. A shifted-register operand field must refuse encoding 31.
///
/// `r` and `r_gp` disagree about exactly one input, and that disagreement is
/// the contract at `aarch64::XZR` made checkable. `Arm64Register::SP` and
/// `Arm64Register::XZR` are the same value, so neither function can tell
/// which one a caller meant — but the CALL SITE knows which encoding it is
/// about to emit, and in a shifted-register form 31 is never SP.
///
/// A legitimate zero operand is written as the literal `aarch64::XZR`, which
/// is a constant and therefore cannot be a forwarded parameter. The `Neg`
/// arm is the one production instance and it does exactly that.
#[test]
pub(in crate::aarch64_backend::tests) fn a_shifted_register_operand_refuses_encoding_thirty_one() {
    let _ = take_invalid_register();

    // `r` accepts it, because `SUB SP, SP, #imm` and `STP FP, LR, [SP, #-16]!`
    // are real instructions this prologue emits.
    assert_eq!(
        r(Arm64Register::SP),
        crate::aarch64::SP,
        "the immediate and load/store arms still need SP"
    );
    assert!(
        !take_invalid_register(),
        "and accepting it must not record a violation"
    );

    // `r_gp` refuses it, and records, so `emit_machine_code_inner` discards
    // the body rather than emitting `ADD Xd, XZR, Xm` where the caller
    // meant `ADD Xd, SP, Xm`.
    let _ = r_gp(Arm64Register::SP);
    assert!(
        take_invalid_register(),
        "a 31 reaching a shifted-register field must cost the method its body"
    );
    // `XZR` is the same value, which is why this is a statement about the
    // path and not about the register.
    let _ = r_gp(Arm64Register::XZR);
    assert!(take_invalid_register());

    // Everything the allocator can actually produce passes through
    // unchanged: X19-X28 is `regalloc::ARM64_LOCAL_GPRS`.
    for n in 0..=30u8 {
        // `r_gp` answers a `Reg`, `r` the SP-capable `RegSp`; on X0..X30
        // they must name the same register.
        assert_eq!(
            crate::aarch64::RegSp::X(r_gp(Arm64Register(n))),
            r(Arm64Register(n)),
            "X{n} must be unaffected"
        );
    }
    assert!(
        !take_invalid_register(),
        "and none of them may record a violation"
    );
}

/// A4. A far `[base + offset]` store whose stored VALUE is IP0 writes the
/// address instead of the value.
///
/// `emit_addr_into_ip0` leaves the effective address in X16 and the `STUR`
/// then reads `rt`. If `rt` is X16 the value is already gone. The `AndImm`
/// arm guards the sibling case (`rn.0 == 16`) and three arms did not; the
/// claim that "IP0 is never a regalloc output" is true of the ALLOCATOR and
/// not of this backend's own X16 uses (the stack-bang probe, the
/// `tableswitch` scratch, the poll and frame-record address
/// materialisation).
#[test]
pub(in crate::aarch64_backend::tests) fn a_far_store_refuses_when_its_value_or_base_is_ip0() {
    let far = 100_000i32; // past both the scaled and the imm9 forms
    let with = |inst: Arm64Instruction| {
        let mut result = make_backend_with_method(1, 1, &[0xb1]); // return
        result.instructions.push(inst);
        emit_machine_code(&result)
    };

    // The control: a far store with ordinary registers still encodes.
    assert!(
        with(Arm64Instruction::Str {
            rt: Arm64Register::X0,
            rn: Arm64Register::X1,
            offset: far,
        })
        .is_some(),
        "a far store off an ordinary base must still encode"
    );

    // The value is IP0: the address materialisation destroyed it.
    assert!(
        with(Arm64Instruction::Str {
            rt: Arm64Register(16),
            rn: Arm64Register::X1,
            offset: far,
        })
        .is_none(),
        "STR with rt = IP0 and a far offset would store the ADDRESS"
    );

    // The base is IP0: `mov_imm64` overwrote it before the ADD read it.
    assert!(
        with(Arm64Instruction::Str {
            rt: Arm64Register::X0,
            rn: Arm64Register(16),
            offset: far,
        })
        .is_none(),
        "STR with rn = IP0 and a far offset computes offset + offset"
    );

    // The load has only the base hazard -- writing rt after the address is
    // consumed is fine -- but the base hazard is the same one.
    assert!(
        with(Arm64Instruction::Ldr {
            rt: Arm64Register::X0,
            rn: Arm64Register(16),
            offset: far,
        })
        .is_none(),
        "LDR with rn = IP0 and a far offset computes offset + offset"
    );
}

/// A3, end to end: a pair offset the encoder cannot carry must discard the
/// body rather than encode a different offset.
#[test]
pub(in crate::aarch64_backend::tests) fn a_pair_offset_outside_the_signed_imm7_discards_the_body() {
    let with_offset = |offset: i32| {
        let mut result = make_backend_with_method(1, 1, &[0xb1]);
        result.instructions.push(Arm64Instruction::Stp {
            rt1: Arm64Register::X0,
            rt2: Arm64Register::X1,
            rn: Arm64Register::SP,
            offset,            });
        emit_machine_code(&result)
    };

    assert!(with_offset(504).is_some(), "504 is encodable");
    assert!(with_offset(-512).is_some(), "-512 is encodable");

    for offset in [512i32, 1024, -520, -12, 70_000] {
        assert!(
            with_offset(offset).is_none(),
            "STP offset {offset} is not encodable and must discard the body"
        );
    }
}

/// The flag has to be consulted, not merely set: a body containing an
/// unencodable register must not be published.
#[test]
pub(in crate::aarch64_backend::tests) fn emit_machine_code_discards_a_body_with_an_unencodable_register() {
    let mut result = make_backend_with_method(1, 1, &[0xb1]); // return
    assert!(
        emit_machine_code(&result).is_some(),
        "the control: this method encodes"
    );
    // Plant the shape a regalloc bug would produce: a "GPR" of 32.
    result.instructions.push(Arm64Instruction::Mov {
        rd: Arm64Register(32),
        rm: Arm64Register::X0,
    });
    assert!(
        emit_machine_code(&result).is_none(),
        "a body containing an unencodable register must be discarded"
    );
    // And the refusal is not sticky across methods.
    let clean = make_backend_with_method(1, 1, &[0xb1]);
    assert!(emit_machine_code(&clean).is_some());
}

// -- Arm64Register tests ------------------------------------------------

#[test]
pub(in crate::aarch64_backend::tests) fn register_callee_saved_x19_through_x28() {
    for i in 19..=28u8 {
        assert!(
            Arm64Register(i).is_callee_saved(),
            "X{} should be callee-saved",
            i
        );
    }
    assert!(!Arm64Register::X0.is_callee_saved());
    assert!(!Arm64Register::FP.is_callee_saved());
    assert!(!Arm64Register::LR.is_callee_saved());
}

#[test]
pub(in crate::aarch64_backend::tests) fn register_arg_regs() {
    for i in 0..=7u8 {
        assert!(Arm64Register(i).is_arg_reg(), "X{} should be arg reg", i);
    }
    assert!(!Arm64Register::X8.is_arg_reg());
    assert!(!Arm64Register::X19.is_arg_reg());
}

#[test]
pub(in crate::aarch64_backend::tests) fn register_index() {
    assert_eq!(Arm64Register::X0.index(), 0);
    assert_eq!(Arm64Register::FP.index(), 29);
    assert_eq!(Arm64Register::SP.index(), 31);
}

// -- Arm64Condition tests -----------------------------------------------

#[test]
pub(in crate::aarch64_backend::tests) fn condition_all_variants_defined() {
    // Verify all 11 variants are distinct and constructible.
    let conds = [
        Arm64Condition::Eq,
        Arm64Condition::Ne,
        Arm64Condition::Lt,
        Arm64Condition::Le,
        Arm64Condition::Gt,
        Arm64Condition::Ge,
        Arm64Condition::Hi,
        Arm64Condition::Ls,
        Arm64Condition::Cs,
        Arm64Condition::Cc,
        Arm64Condition::Al,
    ];
    assert_eq!(conds.len(), 11);
    // Check they're not all equal.
    assert_ne!(conds[0], conds[1]);
}

// -- Entry convention tests ---------------------------------------------

/// A `double` argument arrives in an X register, not in `D0`.
///
/// This is the fact that makes the type NOT AAPCS64 (finding A9), and it
/// was invisible: the type was named `Arm64CallingConvention` and
/// documented as "AAPCS64 calling convention constants and helpers", so the
/// obvious reading — first FP argument in `V0` — was wrong and nothing
/// said so. The prologue is the authority, so the prologue is what this
/// asserts: `f(double)` must home `X0`, and nothing may read `V0`.
#[test]
pub(in crate::aarch64_backend::tests) fn the_entry_convention_passes_every_argument_in_an_x_register() {
    let mut b = Arm64Backend::new();
    // One `double` parameter (two JVM slots) in a frame-homed local, and a
    // body that does nothing: only the prologue's homing is under test.
    b.local_regs = vec![None, None];
    b.float_local_regs = vec![None, None];
    b.install_frame(
        2,
        Arm64SpillArea {
            locals: 2,
            operands: 2,
            ..Arm64SpillArea::default()
        },
        &[],
    );
    b.emit_argument_homing(&[0]);
    assert!(!b.failed, "one argument must home");

    let ops = b.buffer.instructions();
    assert!(
        ops.iter().any(|i| matches!(
            i,
            Arm64Instruction::Str { rt, .. } if *rt == Arm64Register::X0
        )),
        "the argument is read out of X0, whatever its Java type: {ops:?}"
    );
    assert!(
        !ops.iter().any(|i| matches!(
            i,
            Arm64Instruction::FpStr { vt, .. } if *vt == Arm64Register::V0
        )),
        "nothing may read V0 — AAPCS64 would put the double there and this \
         prologue would never see it"
    );

    // And the ninth argument is refused rather than read from the caller's
    // frame: this convention has no stack argument area.
    let mut b2 = Arm64Backend::new();
    b2.local_regs = vec![None; 9];
    b2.float_local_regs = vec![None; 9];
    b2.install_frame(
        9,
        Arm64SpillArea {
            locals: 9,
            ..Arm64SpillArea::default()
        },
        &[],
    );
    b2.emit_argument_homing(&[0, 1, 2, 3, 4, 5, 6, 7, 8]);
    assert!(b2.failed, "a ninth argument has nowhere to come from");
}

#[test]
pub(in crate::aarch64_backend::tests) fn calling_convention_int_arg_regs() {
    assert_eq!(Arm64EntryConvention::INT_ARG_REGS.len(), 8);
    assert_eq!(
        Arm64EntryConvention::int_arg_reg(0),
        Some(Arm64Register::X0)
    );
    assert_eq!(
        Arm64EntryConvention::int_arg_reg(7),
        Some(Arm64Register::X7)
    );
    assert_eq!(Arm64EntryConvention::int_arg_reg(8), None);
}

#[test]
pub(in crate::aarch64_backend::tests) fn calling_convention_callee_saved_count() {
    assert_eq!(Arm64EntryConvention::CALLEE_SAVED.len(), 10);
}

#[test]
pub(in crate::aarch64_backend::tests) fn calling_convention_local_reg_mapping() {
    assert_eq!(Arm64EntryConvention::local_reg(0), Some(Arm64Register::X19));
    assert_eq!(Arm64EntryConvention::local_reg(9), Some(Arm64Register::X28));
    assert_eq!(Arm64EntryConvention::local_reg(10), None);
}

#[test]
pub(in crate::aarch64_backend::tests) fn calling_convention_stack_alignment_is_16() {
    assert_eq!(Arm64EntryConvention::STACK_ALIGNMENT, 16);
}

#[test]
pub(in crate::aarch64_backend::tests) fn calling_convention_no_red_zone() {
    assert_eq!(Arm64EntryConvention::RED_ZONE, 0);
}

// -- FrameLayout tests --------------------------------------------------

#[test]
pub(in crate::aarch64_backend::tests) fn frame_layout_zero_locals() {
    let frame = Arm64FrameLayout::compute(0, 0, &[]);
    assert_eq!(frame.frame_size % 16, 0, "frame must be 16-byte aligned");
    assert_eq!(frame.num_spills, 0);
    assert_eq!(frame.num_reg_locals, 0);
}

#[test]
pub(in crate::aarch64_backend::tests) fn frame_layout_five_locals() {
    let saved: Vec<_> = (0..5)
        .map(|i| Arm64EntryConvention::CALLEE_SAVED[i])
        .collect();
    let frame = Arm64FrameLayout::compute(5, 2, &saved);
    assert_eq!(frame.num_reg_locals, 5);
    assert_eq!(frame.num_spills, 2);
    assert_eq!(frame.saved_regs.len(), 5);
}

#[test]
pub(in crate::aarch64_backend::tests) fn frame_layout_16_byte_alignment() {
    for n in 0..20 {
        let num_saved = n.min(Arm64EntryConvention::CALLEE_SAVED.len());
        let saved: Vec<_> = (0..num_saved)
            .map(|i| Arm64EntryConvention::CALLEE_SAVED[i])
            .collect();
        let frame = Arm64FrameLayout::compute(n, n, &saved);
        assert_eq!(
            frame.frame_size % 16,
            0,
            "frame_size {} not aligned for n={}",
            frame.frame_size,
            n
        );
    }
}

// -- CodeBuffer tests ---------------------------------------------------

#[test]
pub(in crate::aarch64_backend::tests) fn code_buffer_emit_and_count() {
    let mut buf = Arm64CodeBuffer::new();
    assert_eq!(buf.instruction_count(), 0);
    buf.emit(Arm64Instruction::Nop);
    buf.emit(Arm64Instruction::Ret);
    assert_eq!(buf.instruction_count(), 2);
}

#[test]
pub(in crate::aarch64_backend::tests) fn code_buffer_label_creation_and_binding() {
    let mut buf = Arm64CodeBuffer::new();
    let l1 = buf.new_label();
    let l2 = buf.new_label();
    assert_ne!(l1, l2);
    buf.emit(Arm64Instruction::Nop);
    buf.bind_label(l1);
    assert!(buf.labels.contains_key(&l1));
    assert_eq!(*buf.labels.get(&l1).unwrap(), 1); // after the Nop
}

#[test]
pub(in crate::aarch64_backend::tests) fn code_buffer_estimated_size_4_bytes_per_instr() {
    let mut buf = Arm64CodeBuffer::new();
    for _ in 0..10 {
        buf.emit(Arm64Instruction::Nop);
    }
    assert_eq!(buf.estimated_size(), 40);
}

// -- Backend: prologue / epilogue tests ---------------------------------

pub(in crate::aarch64_backend::tests) fn make_backend_with_method(
    num_locals: usize,
    num_params: usize,
    bytecode: &[u8],
) -> Arm64CompileResult {
    let mut backend = Arm64Backend::new();
    backend.compile_method(num_locals, num_params, 4, bytecode)
}

#[test]
pub(in crate::aarch64_backend::tests) fn backend_prologue_emits_stp_fp_lr() {
    let result = make_backend_with_method(0, 0, &[0xb1]); // return void
                                                          // Bug-fix (ARM64 BUG #2): the prologue now uses the writeback
                                                          // `STP FP, LR, [SP, #-16]!` form (StpPre) so SP is decremented as part
                                                          // of the save, instead of the old non-writeback `Stp` with an unsound
                                                          // "16 already consumed" SUB fudge.
    let has_stp_fp_lr = result.instructions.iter().any(|inst| {
        matches!(
            inst,
            Arm64Instruction::StpPre { rt1, rt2, offset: -16, .. }
                if *rt1 == Arm64Register::FP && *rt2 == Arm64Register::LR
        )
    });
    assert!(
        has_stp_fp_lr,
        "prologue must save FP/LR with writeback STP (StpPre, #-16)"
    );
}

#[test]
pub(in crate::aarch64_backend::tests) fn backend_epilogue_emits_ldp_fp_lr_and_ret() {
    let result = make_backend_with_method(0, 0, &[0xb1]);
    // Bug-fix (ARM64 BUG #2): the epilogue now restores FP/LR with the
    // matching post-index `LDP FP, LR, [SP], #16` (LdpPost).
    let has_ldp = result.instructions.iter().any(|inst| {
        matches!(
            inst,
            Arm64Instruction::LdpPost { rt1, rt2, offset: 16, .. }
                if *rt1 == Arm64Register::FP && *rt2 == Arm64Register::LR
        )
    });
    let has_ret = result
        .instructions
        .iter()
        .any(|inst| matches!(inst, Arm64Instruction::Ret));
    assert!(
        has_ldp,
        "epilogue must restore FP/LR with post-index LDP (LdpPost, #16)"
    );
    assert!(has_ret, "epilogue must emit RET");
}

// -- Backend: instruction emission tests --------------------------------

#[test]
pub(in crate::aarch64_backend::tests) fn backend_iconst_emits_movimm() {
    // iconst_5 (opcode 0x08) then ireturn (0xac)
    let result = make_backend_with_method(0, 0, &[0x08, 0xac]);
    let _has_mov = result
        .instructions
        .iter()
        .any(|inst| matches!(inst, Arm64Instruction::MovImm { imm: 2, .. }));
    // iconst_5 = opcode 0x08 - 3 = 5
    let has_mov5 = result
        .instructions
        .iter()
        .any(|inst| matches!(inst, Arm64Instruction::MovImm { imm: 5, .. }));
    assert!(has_mov5, "iconst_5 should emit MovImm with imm=5");
}

#[test]
pub(in crate::aarch64_backend::tests) fn backend_int_add_emits_add() {
    // iconst_1, iconst_2, iadd, ireturn
    let result = make_backend_with_method(0, 0, &[0x04, 0x05, 0x60, 0xac]);
    let has_add = result
        .instructions
        .iter()
        .any(|inst| matches!(inst, Arm64Instruction::AddW { .. }));
    assert!(has_add, "iadd should emit the 32-bit AddW");
}

#[test]
pub(in crate::aarch64_backend::tests) fn backend_int_sub_emits_sub() {
    let result = make_backend_with_method(0, 0, &[0x04, 0x05, 0x64, 0xac]);
    let has_sub = result
        .instructions
        .iter()
        .any(|inst| matches!(inst, Arm64Instruction::SubW { .. }));
    assert!(has_sub, "isub should emit the 32-bit SubW");
}

#[test]
pub(in crate::aarch64_backend::tests) fn backend_int_mul_emits_mul() {
    let result = make_backend_with_method(0, 0, &[0x04, 0x05, 0x68, 0xac]);
    let has_mul = result
        .instructions
        .iter()
        .any(|inst| matches!(inst, Arm64Instruction::MulW { .. }));
    assert!(has_mul, "imul should emit the 32-bit MulW");
}

/// `idiv` is REFUSED (see the module header and
/// `backend_idiv_bails_to_interpreter`), and the refusal must survive
/// compile-time-constant operands too: a constant-folding or peephole path
/// that resolved `iconst_1 / iconst_2` before the opcode arm ran would
/// reintroduce the `BRK #1`/SIGTRAP lowering for the shape that looks
/// safest. Written as "no `SDiv` is emitted" rather than only
/// "`!success`", so it still fails if the arm is re-wired.
#[test]
pub(in crate::aarch64_backend::tests) fn backend_int_div_bails_with_constant_operands() {
    // iconst_1, iconst_2, idiv, ireturn
    let result = make_backend_with_method(0, 0, &[0x04, 0x05, 0x6c, 0xac]);
    assert!(
        !result.success,
        "idiv must bail, constant operands included"
    );
    let has_div = result
        .instructions
        .iter()
        .any(|inst| matches!(inst, Arm64Instruction::SDiv { .. }));
    assert!(!has_div, "no SDiv may be emitted for a refused idiv");
}

#[test]
pub(in crate::aarch64_backend::tests) fn backend_int_neg_emits_neg() {
    let result = make_backend_with_method(0, 0, &[0x04, 0x74, 0xac]);
    let has_neg = result
        .instructions
        .iter()
        .any(|inst| matches!(inst, Arm64Instruction::NegW { .. }));
    assert!(has_neg, "ineg should emit the 32-bit NegW");
}

#[test]
pub(in crate::aarch64_backend::tests) fn backend_iload_from_register() {
    // iload_0 (0x1a) then ireturn (0xac), with 1 local
    let result = make_backend_with_method(1, 0, &[0x1a, 0xac]);
    // iload from a register local emits Mov from a callee-saved register.
    let has_mov_from_callee_saved = result.instructions.iter().any(|inst| {
        matches!(
            inst,
            Arm64Instruction::Mov { rm, .. } if rm.is_callee_saved()
        )
    });
    assert!(
        has_mov_from_callee_saved,
        "iload_0 with register local should emit Mov from callee-saved reg"
    );
}

#[test]
pub(in crate::aarch64_backend::tests) fn backend_iload_from_spill_slot() {
    // 12 locals all loaded simultaneously to force spills (ARM64 has 10 callee-saved GPRs).
    // Load all 12, then return.
    let mut code = Vec::new();
    for i in 0..12u8 {
        code.push(0x15); // iload
        code.push(i);
    }
    code.push(0xac); // ireturn
    let result = make_backend_with_method(12, 12, &code);
    // At least 2 locals must be spilled → at least 2 Ldr from [FP + offset]
    let ldr_from_fp = result
        .instructions
        .iter()
        .filter(|inst| {
            matches!(
                inst,
                Arm64Instruction::Ldr { rn, .. } if *rn == Arm64Register::FP
            )
        })
        .count();
    assert!(
        ldr_from_fp >= 2,
        "with 12 simultaneous locals, at least 2 should spill, got {ldr_from_fp} LDRs from FP"
    );
}

#[test]
pub(in crate::aarch64_backend::tests) fn backend_istore_to_register() {
    // iconst_1, istore_0, return
    let result = make_backend_with_method(1, 0, &[0x04, 0x3b, 0xb1]);
    let has_mov_to_callee_saved = result.instructions.iter().any(|inst| {
        matches!(
            inst,
            Arm64Instruction::Mov { rd, .. } if rd.is_callee_saved()
        )
    });
    assert!(
        has_mov_to_callee_saved,
        "istore_0 with register local should emit Mov to callee-saved reg"
    );
}

#[test]
pub(in crate::aarch64_backend::tests) fn backend_return_int_uses_x0() {
    // iconst_3, ireturn
    let result = make_backend_with_method(0, 0, &[0x06, 0xac]);
    let has_mov_x0 = result.instructions.iter().any(|inst| {
        matches!(
            inst,
            Arm64Instruction::Mov { rd, .. } if *rd == Arm64Register::X0
        )
    });
    assert!(has_mov_x0, "ireturn should move result to X0");    }

/// JVMS §6.5 `ireturn` narrowing parity with x64 (`emit_narrow_int_return`,
/// `x64/emit.rs`): a `Z`/`B`/`C`/`S` method narrows X0 right after the
/// result lands there, an `I` method does not, and the narrowing encodes to
/// the exact X-form words (so the sign-extended `int` convention holds).
#[test]
pub(in crate::aarch64_backend::tests) fn r9w9_ireturn_narrows_boolean_byte_char_short_like_x64() {
    // iconst_2; ireturn
    let code = [0x05u8, 0xac];
    let cases: [(&str, Option<u32>); 5] = [
        ("()Z", Some(0x9240_0000)), // AND  X0, X0, #1
        ("()B", Some(0x9340_1C00)), // SXTB X0, W0
        ("()C", Some(0x9240_3C00)), // AND  X0, X0, #0xFFFF
        ("()S", Some(0x9340_3C00)), // SXTH X0, W0
        ("()I", None),
    ];
    for (desc, word) in cases {
        let mut b = Arm64Backend::new();
        b.set_safepoints_enabled(false);
        b.set_method_descriptor(desc, true);
        let result = b.compile_method(0, 0, 4, &code);
        assert!(result.success, "{desc} must compile");
        let insts = result.instructions.clone();
        let mov_at = insts
            .iter()
            .position(
                |i| matches!(i, Arm64Instruction::Mov { rd, .. } if *rd == Arm64Register::X0),
            )
            .expect("ireturn moves its value into X0");
        let next = insts
            .get(mov_at + 1)
            .expect("a branch to the epilogue follows");
        let narrowed = match (desc, next) {
            ("()Z", Arm64Instruction::AndImm { rd, rn, imm: 1 })
            | (
                "()C",
                Arm64Instruction::AndImm {
                    rd,
                    rn,
                    imm: 0xFFFF,
                },
            )
            | ("()B", Arm64Instruction::Sxtb { rd, rn })
            | ("()S", Arm64Instruction::Sxth { rd, rn }) => {
                *rd == Arm64Register::X0 && *rn == Arm64Register::X0
            }
            _ => false,
        };
        assert_eq!(
            narrowed,
            word.is_some(),
            "{desc}: narrowing after the X0 move, got {next:?}"
        );
        let bytes = emit_machine_code(&result).expect("encodes");
        if let Some(w) = word {
            let found = bytes
                .chunks_exact(4)
                .any(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]) == w);
            assert!(found, "{desc}: expected word {w:#010x} in the body");
        }
    }
}

// -- Round 9 wave 10: `getstatic`, `MemLoad`/`MemStore`/`DmbIsh` --------

pub(in crate::aarch64_backend::tests) fn words_of(bytes: &[u8]) -> Vec<u32> {
    bytes
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

pub(in crate::aarch64_backend::tests) fn static_field(
    class_id: u32,
    field_index: usize,
    type_tag: u8,
    is_volatile: bool,
) -> Arm64StaticField {
    Arm64StaticField {
        class_id,
        field_index,
        base_cell: 0x1000,
        type_tag,
        is_volatile,
    }
}

/// Compile `code` (a static `()`-method) with `fields` resolved.
pub(in crate::aarch64_backend::tests) fn compile_with_statics(
    descriptor: &str,
    code: &[u8],
    fields: &[(usize, Arm64StaticField)],
) -> Arm64CompileResult {
    let mut b = Arm64Backend::new();
    b.set_safepoints_enabled(false);
    b.set_method_descriptor(descriptor, true);
    b.set_static_field_info(fields.iter().copied().collect());
    b.compile_method(0, 0, 4, code)
}

/// Every width and ordering of the two new memory pseudo-ops, and the
/// barrier, encode to the ARM ARM words -- in particular a `release` store
/// is `STLR*` and an `acquire` load is `LDAR*`, never the plain form.
#[test]
pub(in crate::aarch64_backend::tests) fn r9w10_mem_load_store_pseudo_ops_encode_width_and_ordering() {
    use super::Arm64MemWidth::{B8, H16, W32, X64};
    let (rt, rn) = (Arm64Register::X9, Arm64Register::X10);
    let mut insts = Vec::new();
    for ordered in [false, true] {
        for width in [B8, H16, W32, X64] {
            insts.push(Arm64Instruction::MemLoad {
                rt,
                rn,
                width,
                acquire: ordered,
            });
        }
        for width in [B8, H16, W32, X64] {
            insts.push(Arm64Instruction::MemStore {
                rt,
                rn,
                width,
                release: ordered,
            });
        }
    }
    insts.push(Arm64Instruction::DmbIsh);
    let bytes = emit_machine_code(&result_from_instructions(insts)).expect("encodes");
    assert_eq!(
        words_of(&bytes),
        vec![
            0x3940_0149u32, // LDRB  W9, [X10]
            0x7940_0149,    // LDRH  W9, [X10]
            0xB940_0149,    // LDR   W9, [X10]
            0xF940_0149,    // LDR   X9, [X10]
            0x3900_0149,    // STRB  W9, [X10]
            0x7900_0149,    // STRH  W9, [X10]
            0xB900_0149,    // STR   W9, [X10]
            0xF900_0149,    // STR   X9, [X10]
            0x08DF_FD49,    // LDARB W9, [X10]
            0x48DF_FD49,    // LDARH W9, [X10]
            0x88DF_FD49,    // LDAR  W9, [X10]
            0xC8DF_FD49,    // LDAR  X9, [X10]
            0x089F_FD49,    // STLRB W9, [X10]
            0x489F_FD49,    // STLRH W9, [X10]
            0x889F_FD49,    // STLR  W9, [X10]
            0xC89F_FD49,    // STLR  X9, [X10]
            0xD503_3BBF,    // DMB ISH
        ]
    );
}

/// `getstatic` of a plain and of a `volatile` `int`: the x64 inline
/// `getstatic` shape (pointer cell -> statics block -> 16-byte cell's
/// 32-bit payload), a plain `LDR W` or an `LDAR W`, then `SXTW` into this
/// backend's int form.
#[test]
pub(in crate::aarch64_backend::tests) fn r9w10_getstatic_int_reads_the_cell_payload_with_ldar_when_volatile() {
    // getstatic #1; ireturn
    let code = [0xb2u8, 0x00, 0x01, 0xac];
    for is_volatile in [false, true] {
        let result =
            compile_with_statics("()I", &code, &[(0, static_field(7, 2, b'I', is_volatile))]);
        assert!(result.success, "volatile={is_volatile}: must compile");
        assert_eq!(result.static_init_classes, vec![7]);
        let load = result
            .instructions
            .iter()
            .find_map(|i| match i {
                Arm64Instruction::MemLoad { width, acquire, .. } => Some((*width, *acquire)),
                _ => None,
            })
            .expect("a MemLoad reads the static");
        assert_eq!(load, (Arm64MemWidth::W32, is_volatile));
        let words = words_of(&emit_machine_code(&result).expect("encodes"));
        let seq_ldr = [0xF940_0129u32, 0x9100_9129]; // LDR X9,[X9]; ADD X9,X9,#36
        let value_load = if is_volatile {
            0x88DF_FD29 // LDAR W9, [X9]
        } else {
            0xB940_0129 // LDR  W9, [X9]
        };
        let want = [seq_ldr[0], seq_ldr[1], value_load, 0x9340_7D29]; // .., SXTW X9, W9
        assert!(
            words.windows(4).any(|w| w == want),
            "volatile={is_volatile}: expected {want:08x?} in {words:08x?}"
        );
        // A volatile READ owes no barrier: no DMB anywhere in the body.
        assert!(
            !words.contains(&0xD503_3BBF),
            "volatile={is_volatile}: a read must not emit DMB ISH"
        );
    }
}

/// `long` reads the 64-bit payload (offset 8) with a 64-bit `LDAR` when
/// `volatile` (JLS §17.7 single-copy atomicity); `float`/`double` land in
/// an FP register through a bit move.
#[test]
pub(in crate::aarch64_backend::tests) fn r9w10_getstatic_long_float_double_widths() {
    let long = compile_with_statics(
        "()J",
        &[0xb2, 0x00, 0x01, 0xad],
        &[(0, static_field(3, 0, b'J', true))],
    );
    assert!(long.success);
    let words = words_of(&emit_machine_code(&long).expect("encodes"));
    // ADD X9, X9, #8; LDAR X9, [X9]
    assert!(
        words.windows(2).any(|w| w == [0x9100_2129u32, 0xC8DF_FD29]),
        "{words:08x?}"
    );

    for (desc, ret, tag, width) in [
        ("()F", 0xaeu8, b'F', Arm64MemWidth::W32),
        ("()D", 0xaf, b'D', Arm64MemWidth::X64),
    ] {
        let r = compile_with_statics(
            desc,
            &[0xb2, 0x00, 0x01, ret],
            &[(0, static_field(3, 1, tag, false))],
        );
        assert!(r.success, "{desc} must compile");
        let at = r
            .instructions
            .iter()
            .position(|i| matches!(i, Arm64Instruction::MemLoad { width: w, acquire: false, .. } if *w == width))
            .expect("a plain MemLoad of the payload width");
        let moved = match (tag, r.instructions.get(at + 1)) {
            (b'F', Some(Arm64Instruction::FmovToFpSingle { .. })) => true,
            (b'D', Some(Arm64Instruction::FmovToFp { .. })) => true,
            _ => false,
        };
        assert!(
            moved,
            "{desc}: the payload is bit-moved into an FP register"
        );
    }
}

/// What `getstatic` still refuses: no resolution for the site, an
/// unrecognised type tag, a zero base cell. And the refusal is by the
/// arm, not by a silently-compiled read.
///
/// h23 (2026-09-22): a REFERENCE static is no longer among them -- see
/// `h23_getstatic_lowers_a_reference_static_as_an_oop` just below. The
/// unresolved and mis-keyed cases still refuse a reference site, which is
/// what the first and last assertions here now pin.
#[test]
pub(in crate::aarch64_backend::tests) fn r9w10_getstatic_refuses_unresolved_reference_and_unbased_sites() {
    let code = [0xb2u8, 0x00, 0x01, 0xb0];
    let unresolved = compile_with_statics("()Ljava/lang/Object;", &code, &[]);
    assert!(!unresolved.success);
    // A tag no descriptor produces: still refused, by the same arm that
    // used to refuse `L`.
    let unknown = compile_with_statics(
        "()Ljava/lang/Object;",
        &code,
        &[(0, static_field(1, 0, b'Q', false))],
    );
    assert!(!unknown.success, "an unrecognised type tag is not lowered");
    let mut unbased = static_field(1, 0, b'I', false);
    unbased.base_cell = 0;
    let r = compile_with_statics("()I", &[0xb2, 0x00, 0x01, 0xac], &[(0, unbased)]);
    assert!(!r.success, "a zero base cell is not an address");
    // Resolved at the WRONG pc: still refused.
    let r = compile_with_statics(
        "()I",
        &[0x00, 0xb2, 0x00, 0x01, 0xac],
        &[(0, static_field(1, 0, b'I', false))],
    );
    assert!(!r.success, "the table is keyed by the getstatic's own pc");
}

/// h23 (2026-09-22): a REFERENCE static reads the same 64-bit payload
/// word `J`/`D` read and lands on the operand stack as an oop.
///
/// The three things that make it correct, each pinned separately:
/// the WIDTH (a reference is a whole word, not the 32-bit payload the int
/// category reads), the ORDERING (`LDAR` when volatile, a plain `LDR`
/// otherwise -- a reference static is as volatile as an `int` one), and
/// the OOP MARK, which is what a safepoint's map is built from and the
/// only reason this was refused for thirteen waves.
#[test]
pub(in crate::aarch64_backend::tests) fn h23_getstatic_lowers_a_reference_static_as_an_oop() {
    for (tag, desc) in [(b'L', "()Ljava/lang/Object;"), (b'[', "()[I")] {
        for is_volatile in [false, true] {
            let r = compile_with_statics(
                desc,
                &[0xb2, 0x00, 0x01, 0xb0],
                &[(0, static_field(3, 0, tag, is_volatile))],
            );
            let what = format!("tag={} volatile={is_volatile}", char::from(tag));
            assert!(r.success, "{what}: a reference static must compile");
            // A whole word, from the 64-bit payload, with acquire exactly
            // when the field is volatile.
            assert!(
                r.instructions.iter().any(|i| matches!(
                    i,
                    Arm64Instruction::MemLoad {
                        width: Arm64MemWidth::X64,
                        acquire,
                        ..
                    } if *acquire == is_volatile
                )),
                "{what}: a 64-bit load whose ordering matches the field"
            );
            // Never the int category's 32-bit read, which would truncate
            // the upper half of a pointer.
            assert!(
                !r.instructions.iter().any(|i| matches!(
                    i,
                    Arm64Instruction::MemLoad {
                        width: Arm64MemWidth::W32,
                        ..
                    }
                )),
                "{what}: a reference is never read 32 bits wide"
            );
            // A read owes no barrier, exactly as the primitive arms.
            assert!(
                !r.instructions
                    .iter()
                    .any(|i| matches!(i, Arm64Instruction::DmbIsh)),
                "{what}: a volatile READ emits no DMB"
            );
        }
    }

    // The mark itself, read off the model rather than inferred from the
    // instruction stream: `areturn` consumes it, so the check is made on
    // a body that leaves the value on the stack.
    let mut b = Arm64Backend::new();
    b.set_safepoints_enabled(false);
    b.set_method_descriptor("()Ljava/lang/Object;", true);
    b.set_static_field_info(
        [(0usize, static_field(3, 0, b'L', false))]
            .into_iter()
            .collect(),
    );
    b.cur_bytecode_pc = 0;
    assert!(
        b.emit_getstatic(static_field(3, 0, b'L', false)),
        "the arm lowers a reference static"
    );
    let top = b.operand_stack.last().expect("one entry pushed");
    assert_eq!(top.kind, OperandKind::Ref, "pushed as a reference");
    assert!(top.oop, "and marked for the collector");
}

/// Two reads of statics of one class record the class once; a read after
/// a merge point is described by the analysis the table feeds.
#[test]
pub(in crate::aarch64_backend::tests) fn r9w10_getstatic_records_declaring_classes_once() {
    // getstatic #1; getstatic #2; iadd; ireturn
    let code = [0xb2u8, 0x00, 0x01, 0xb2, 0x00, 0x02, 0x60, 0xac];
    let r = compile_with_statics(
        "()I",
        &code,
        &[
            (0, static_field(9, 0, b'I', false)),
            (3, static_field(9, 1, b'S', true)),
        ],
    );
    assert!(r.success);
    assert_eq!(r.static_init_classes, vec![9]);
}

/// The site walk decodes instructions, so a `0xb2` operand byte is not
/// mistaken for a `getstatic`.
#[test]
pub(in crate::aarch64_backend::tests) fn r9w10_static_field_sites_walk_instructions_not_bytes() {
    // bipush 0xb2; getstatic #5; ireturn
    let code = [0x10u8, 0xb2, 0xb2, 0x00, 0x05, 0xac];
    assert_eq!(static_field_sites(&code), vec![(2, 0xb2, 5)]);
}

// -- Round 9 wave 11: the `ArithmeticException` throw path -------------

pub(in crate::aarch64_backend::tests) const R9W11_THROW_ARITH: usize = 0x7FFF_0000_2000;

/// Compile a static method with the division throw path wired as the
/// caller would wire it: `helper` as `throw_arithmetic`, and the caller's
/// word on whether the exception table is empty.
pub(in crate::aarch64_backend::tests) fn compile_div(
    descriptor: &str,
    slots: usize,
    code: &[u8],
    helper: usize,
    table_empty: bool,
) -> Arm64CompileResult {
    let mut b = Arm64Backend::new();
    b.set_safepoints_enabled(false);
    // SAFETY: plain struct of `usize` addresses; all-zero is "nothing wired".
    let mut h: crate::JitRuntimeHelpers = unsafe { std::mem::zeroed() };
    h.throw_arithmetic = helper;
    b.set_helpers(h);
    b.set_method_descriptor(descriptor, true);
    b.set_exception_table_empty(table_empty);
    b.compile_method(slots, slots, 4, code)
}

/// The two new W-form pseudo-ops and the X forms they sit beside encode
/// to the ARM ARM words.
#[test]
pub(in crate::aarch64_backend::tests) fn r9w11_sdiv_msub_w_and_x_forms_encode() {
    let (d, n, m) = (Arm64Register::X11, Arm64Register::X9, Arm64Register::X10);
    let insts = vec![
        Arm64Instruction::SDivW {
            rd: d,
            rn: n,
            rm: m,
        },
        Arm64Instruction::MsubW {
            rd: d,
            rn: d,
            rm: m,
            ra: n,
        },
        Arm64Instruction::SDiv {
            rd: d,
            rn: n,
            rm: m,
        },
        Arm64Instruction::Msub {
            rd: d,
            rn: d,
            rm: m,
            ra: n,
        },
    ];
    let bytes = emit_machine_code(&result_from_instructions(insts)).expect("encodes");
    assert_eq!(
        words_of(&bytes),
        vec![
            0x1ACA_0D2Bu32, // SDIV W11, W9, W10
            0x1B0A_A56B,    // MSUB W11, W11, W10, W9
            0x9ACA_0D2B,    // SDIV X11, X9, X10
            0x9B0A_A56B,    // MSUB X11, X11, X10, X9
        ]
    );
}

/// With the throw path wired, `idiv`/`irem`/`ldiv`/`lrem` compile to a
/// zero test of the DIVISOR that branches to one out-of-line stub, which
/// calls `throw_arithmetic` through X16 and leaves through the shared
/// epilogue; then `SDIV` (and `MSUB` for a remainder), sign-extended back
/// to the int form for the `int` pair only. The `CBZ`'s encoded target is
/// checked against where the stub really landed.
#[test]
pub(in crate::aarch64_backend::tests) fn r9w11_div_rem_guard_the_divisor_and_branch_to_the_throw_stub() {
    let cases: [(u8, &str, usize, [u8; 4], bool, bool); 4] = [
        // iload_0; iload_1; idiv|irem; ireturn
        (0x6c, "(II)I", 2, [0x1a, 0x1b, 0x6c, 0xac], false, false),
        (0x70, "(II)I", 2, [0x1a, 0x1b, 0x70, 0xac], false, true),
        // lload_0; lload_2; ldiv|lrem; lreturn
        (0x6d, "(JJ)J", 4, [0x1e, 0x20, 0x6d, 0xad], true, false),
        (0x71, "(JJ)J", 4, [0x1e, 0x20, 0x71, 0xad], true, true),
    ];
    for (op, desc, slots, code, wide, rem) in cases {
        let r = compile_div(desc, slots, &code, R9W11_THROW_ARITH, true);
        assert!(
            r.success,
            "0x{op:02x}: must compile with the throw path wired"
        );
        let ops = &r.instructions;

        // The guard: a CBZ (X for long, W for int) on the divisor.
        let (guard_at, guard_rt, throw_label) = ops
            .iter()
            .enumerate()
            .find_map(|(i, inst)| match (wide, inst) {
                (true, Arm64Instruction::Cbz { rt, label }) => Some((i, *rt, *label)),
                (false, Arm64Instruction::CbzW { rt, label }) => Some((i, *rt, *label)),
                _ => None,
            })
            .unwrap_or_else(|| panic!("0x{op:02x}: no zero-divisor guard"));

        // The division reads the guarded register as its divisor.
        let (div_at, dst, dividend) = ops
            .iter()
            .enumerate()
            .find_map(|(i, inst)| match (wide, inst) {
                (true, Arm64Instruction::SDiv { rd, rn, rm }) if *rm == guard_rt => {
                    Some((i, *rd, *rn))
                }
                (false, Arm64Instruction::SDivW { rd, rn, rm }) if *rm == guard_rt => {
                    Some((i, *rd, *rn))
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("0x{op:02x}: no SDIV by the guarded divisor"));
        assert!(
            guard_at < div_at,
            "0x{op:02x}: the guard runs before the divide"
        );
        assert_ne!(
            dst, guard_rt,
            "0x{op:02x}: the result must not clobber the divisor"
        );
        assert_ne!(
            dst, dividend,
            "0x{op:02x}: the result must not clobber the dividend"
        );
        let mut next = div_at + 1;
        if rem {
            let msub_ok = match (wide, &ops[next]) {
                (true, Arm64Instruction::Msub { rd, rn, rm, ra })
                | (false, Arm64Instruction::MsubW { rd, rn, rm, ra }) => {
                    *rd == dst && *rn == dst && *rm == guard_rt && *ra == dividend
                }
                _ => false,
            };
            assert!(msub_ok, "0x{op:02x}: remainder is MSUB dst, dst, b, a");
            next += 1;
        }
        let sxtw_follows = matches!(
            &ops[next],
            Arm64Instruction::Sxtw { rd, rn } if *rd == dst && *rn == dst
        );
        assert_eq!(
            sxtw_follows, !wide,
            "0x{op:02x}: SXTW follows the int forms only"
        );
        assert!(
            !ops.iter()
                .any(|i| matches!(i, Arm64Instruction::Brk { .. })),
            "0x{op:02x}: the SIGTRAP guard must never come back"
        );

        // The stub: after the epilogue's RET, bound to the guard's label.
        let ret_at = ops
            .iter()
            .rposition(|i| matches!(i, Arm64Instruction::Ret))
            .expect("an epilogue RET");
        let stub_at = ops
            .iter()
            .position(|i| matches!(i, Arm64Instruction::Label(l) if *l == throw_label))
            .expect("the guard's label is bound");
        assert!(stub_at > ret_at, "0x{op:02x}: the stub is out of line");
        assert!(matches!(
            &ops[stub_at + 1],
            Arm64Instruction::MovImm { rd: Arm64Register::X16, imm }
                if *imm == R9W11_THROW_ARITH as i64
        ));
        assert!(matches!(
            &ops[stub_at + 2],
            Arm64Instruction::Blr {
                rn: Arm64Register::X16
            }
        ));
        // ...and leaves through the SAME epilogue the `*return` uses.
        let ret_label = ops[..ret_at]
            .iter()
            .find_map(|i| match i {
                Arm64Instruction::B { label } => Some(*label),
                _ => None,
            })
            .expect("the return branches to the epilogue");
        assert!(
            matches!(&ops[stub_at + 3], Arm64Instruction::B { label } if *label == ret_label),
            "0x{op:02x}: the stub must run the epilogue, not RET past it"
        );

        // Encoded: the CBZ's displacement lands exactly on the stub.
        let (bytes, offsets) = emit_machine_code_inner(&r).expect("encodes");
        let word_at = |off: usize| {
            u32::from_le_bytes([bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]])
        };
        let cbz = word_at(offsets[guard_at]);
        let want_base = if wide { 0xB400_0000u32 } else { 0x3400_0000 };
        assert_eq!(cbz & 0xFF00_001F, want_base | u32::from(guard_rt.0));
        // Cast: sign-extend the 19-bit word displacement.
        let imm19 = ((cbz >> 5) & 0x7FFFF) as i32;
        let disp = ((imm19 << 13) >> 13) * 4;
        assert_eq!(
            offsets[guard_at] as i64 + i64::from(disp),
            offsets[stub_at] as i64,
            "0x{op:02x}: CBZ must branch to the stub"
        );
        assert!(words_of(&bytes).contains(&0xD63F_0200), "BLR X16");
    }
}

/// Every division of a method shares ONE stub; a method with no division
/// emits none.
#[test]
pub(in crate::aarch64_backend::tests) fn r9w11_divisions_share_one_throw_stub_and_none_is_emitted_without_one() {
    // iload_0; iload_1; idiv; iload_1; irem; ireturn
    let r = compile_div(
        "(II)I",
        2,
        &[0x1a, 0x1b, 0x6c, 0x1b, 0x70, 0xac],
        R9W11_THROW_ARITH,
        true,
    );
    assert!(r.success);
    let labels: Vec<u32> = r
        .instructions
        .iter()
        .filter_map(|i| match i {
            Arm64Instruction::CbzW { label, .. } => Some(*label),
            _ => None,
        })
        .collect();
    assert_eq!(labels.len(), 2, "one guard per division");
    assert_eq!(labels[0], labels[1], "both guards share the stub");
    let blrs = r
        .instructions
        .iter()
        .filter(|i| matches!(i, Arm64Instruction::Blr { .. }))
        .count();
    assert_eq!(blrs, 1, "one stub, one call");

    // iload_0; ireturn -- wired, but nothing to guard.
    let plain = compile_div("(I)I", 1, &[0x1a, 0xac], R9W11_THROW_ARITH, true);
    assert!(plain.success);
    assert!(!plain
        .instructions
        .iter()
        .any(|i| matches!(i, Arm64Instruction::Blr { .. })));
}

/// The throw path is exact only with BOTH halves wired. A method whose
/// exception table is not known empty could have a handler for the
/// exception, which the stub's unknown throw pc cannot select correctly;
/// a missing helper is no throw path at all. Either refuses, by the arm,
/// before any division is emitted.
#[test]
pub(in crate::aarch64_backend::tests) fn r9w11_div_refuses_without_the_helper_or_the_empty_table_word() {
    let code = [0x1au8, 0x1b, 0x6c, 0xac];
    for (helper, table_empty, why) in [
        (R9W11_THROW_ARITH, false, "exception table not known empty"),
        (0, true, "no throw_arithmetic helper"),
        (0, false, "neither"),
    ] {
        let r = compile_div("(II)I", 2, &code, helper, table_empty);
        assert!(!r.success, "{why}: idiv must refuse");
        assert!(
            !r.instructions.iter().any(|i| matches!(
                i,
                Arm64Instruction::SDivW { .. } | Arm64Instruction::SDiv { .. }
            )),
            "{why}: no division may be emitted"
        );
        assert!(
            r.instructions.iter().any(|i| matches!(
                i,
                Arm64Instruction::Comment(c) if c.contains("no exact ArithmeticException path")
            )),
            "{why}: refused by the division arm"
        );
    }
}

// -- Round 9 wave 12: the NPE throw path and `arraylength` -------------

pub(in crate::aarch64_backend::tests) const R9W12_NPE: usize = 0x7FFF_0000_3000;

/// Compile a static method with the NPE throw path wired as the caller
/// would wire it.
pub(in crate::aarch64_backend::tests) fn compile_npe(
    descriptor: &str,
    slots: usize,
    code: &[u8],
    helper: usize,
    table_empty: bool,
) -> Arm64CompileResult {
    let mut b = Arm64Backend::new();
    b.set_safepoints_enabled(false);
    // SAFETY: plain struct of `usize` addresses; all-zero is "nothing wired".
    let mut h: crate::JitRuntimeHelpers = unsafe { std::mem::zeroed() };
    h.jit_npe_with_action = helper;
    b.set_helpers(h);
    b.set_method_descriptor(descriptor, true);
    b.set_exception_table_empty(table_empty);
    b.compile_method(slots, slots, 4, code)
}

/// `arraylength` null-checks its receiver against the per-action stub and
/// then reads the header's length word with a PLAIN load at
/// `ARRAY_LENGTH_OFFSET`, canonicalized by `SXTW`.
///
/// The plain load is the claim worth pinning: the length is written once,
/// before publication, and the load's address depends on the reference, so
/// ARMv8 orders it without `LDAR` and without a fence. If someone later
/// "fixes" this to an acquire load, the encoding assertion below is what
/// says the argument was reconsidered rather than forgotten.
#[test]
pub(in crate::aarch64_backend::tests) fn r9w12_arraylength_null_checks_then_loads_the_length_word() {
    // aload_0; arraylength; ireturn
    let r = compile_npe("([I)I", 1, &[0x2a, 0xbe, 0xac], R9W12_NPE, true);
    assert!(r.success, "a wired arraylength compiles");

    let at = r
        .instructions
        .iter()
        .position(|i| matches!(i, Arm64Instruction::Cbz { .. }))
        .expect("the receiver is null-checked with a 64-bit CBZ");
    let Arm64Instruction::Cbz { rt: recv, label } = r.instructions[at] else {
        unreachable!()
    };
    match (&r.instructions[at + 1], &r.instructions[at + 2]) {
        (
            Arm64Instruction::AddImm { rd, rn, imm },
            Arm64Instruction::MemLoad {
                rt,
                rn: base,
                width: Arm64MemWidth::W32,
                acquire: false,
            },
        ) => {
            assert_eq!(*rn, recv, "the length address is the receiver plus a disp");
            assert_eq!(
                *imm,
                // Cast: a small header constant, as the lowering passes it.
                cratonvm_types::ARRAY_LENGTH_OFFSET as i32,
                "the disp is the header's length offset"
            );
            assert_eq!(*rd, *base);
            assert_eq!(*rt, *base, "the length lands where its address was");
            assert_ne!(
                *rd, recv,
                "the address must not clobber the checked receiver"
            );
        }
        (a, b) => panic!("expected ADD #len_off then a plain 32-bit MemLoad, got {a:?} {b:?}"),
    }
    assert!(
        matches!(r.instructions[at + 3], Arm64Instruction::Sxtw { .. }),
        "an int producer is a W form plus SXTW on this backend"
    );

    // The stub: one per action, out of line after the epilogue's RET, and
    // it materializes the deopt sentinel ITSELF (the helper returns `()`).
    let stub = r
        .instructions
        .iter()
        .position(|i| matches!(i, Arm64Instruction::Label(l) if *l == label))
        .expect("the CBZ's label is bound");
    let ret = r
        .instructions
        .iter()
        .position(|i| matches!(i, Arm64Instruction::Ret))
        .expect("the epilogue returns");
    assert!(stub > ret, "the stub is out of line, after the RET");
    match &r.instructions[stub + 1..stub + 6] {
        [Arm64Instruction::MovImm {
            rd: a0,
            imm: action,
        }, Arm64Instruction::MovImm {
            rd: scratch,
            imm: helper,
        }, Arm64Instruction::Blr { rn: called }, Arm64Instruction::MovImm {
            rd: sentinel_reg,
            imm: sentinel,
        }, Arm64Instruction::B { .. }] => {
            assert_eq!(*a0, Arm64Register::X0);
            assert_eq!(*action, i64::from(npe_action::ARRAY_LENGTH));
            // Cast: the helper address as the pseudo-op carries it.
            assert_eq!(*helper, R9W12_NPE as i64);
            assert_eq!(*called, *scratch);
            assert_eq!(*sentinel_reg, Arm64Register::X0);
            assert_eq!(
                *sentinel,
                i64::MIN,
                "the stub owes the deopt sentinel: jit_npe_with_action returns ()"
            );
        }
        other => panic!("unexpected NPE stub shape: {other:?}"),
    }

    // No ordering instruction is emitted for this read.
    let words = words_of(&emit_machine_code(&r).expect("encodes"));
    assert!(
        !words.contains(&0xD503_3BBF),
        "an address-dependent length read owes no DMB ISH: {words:08x?}"
    );
    // The plain `LDR Wt, [Xn, #0]` is `0xB9400000 | ..`; `LDAR Wt, [Xn]`
    // would be `0x88DFFC00 | ..`. Check which one the encoder produced.
    assert!(
        words.iter().any(|w| w & 0xFFC0_0000 == 0xB940_0000),
        "the length load encodes as a plain LDR Wt, [Xn]: {words:08x?}"
    );
    assert!(
        !words.iter().any(|w| w & 0xFFFF_FC00 == 0x88DF_FC00),
        "no LDAR W is emitted for an array length: {words:08x?}"
    );
}

/// Two `arraylength`s in one method share ONE stub, and a method with no
/// null check emits none.
#[test]
pub(in crate::aarch64_backend::tests) fn r9w12_npe_stubs_are_one_per_action_and_none_without_one() {
    // aload_0; arraylength; aload_0; arraylength; iadd; ireturn
    let two = compile_npe(
        "([I)I",
        1,
        &[0x2a, 0xbe, 0x2a, 0xbe, 0x60, 0xac],
        R9W12_NPE,
        true,
    );
    assert!(two.success);
    let labels: Vec<u32> = two
        .instructions
        .iter()
        .filter_map(|i| match i {
            Arm64Instruction::Cbz { label, .. } => Some(*label),
            _ => None,
        })
        .collect();
    assert_eq!(labels.len(), 2, "each receiver is checked");
    assert_eq!(labels[0], labels[1], "both share the ARRAY_LENGTH stub");
    let calls = two
        .instructions
        .iter()
        .filter(|i| matches!(i, Arm64Instruction::Blr { .. }))
        .count();
    assert_eq!(calls, 1, "one stub, one call");

    // aload_0; areturn -- wired, but nothing to check.
    let plain = compile_npe("([I)[I", 1, &[0x2a, 0xb0], R9W12_NPE, true);
    assert!(plain.success);
    assert!(
        !plain
            .instructions
            .iter()
            .any(|i| matches!(i, Arm64Instruction::Blr { .. })),
        "no null check, no stub"
    );
}

/// The encoded `CBZ` displacement really lands on the stub, not merely on
/// a label the pseudo-op stream happens to contain.
#[test]
pub(in crate::aarch64_backend::tests) fn r9w12_the_encoded_null_check_branches_to_the_stub() {
    let r = compile_npe("([I)I", 1, &[0x2a, 0xbe, 0xac], R9W12_NPE, true);
    assert!(r.success);
    let words = words_of(&emit_machine_code(&r).expect("encodes"));
    // CBZ Xt, label: 0xB4000000 | (imm19 << 5) | Rt.
    let (at, word) = words
        .iter()
        .enumerate()
        .find(|(_, w)| *w & 0xFF00_0000 == 0xB400_0000)
        .map(|(i, w)| (i, *w))
        .expect("a 64-bit CBZ is encoded");
    // Sign-extend the 19-bit displacement, which counts INSTRUCTIONS.
    // Cast: a 19-bit field, then a small signed instruction count.
    let imm19 = ((word >> 5) & 0x7_FFFF) as i32;
    let disp = if imm19 & 0x4_0000 != 0 {
        imm19 - 0x8_0000
    } else {
        imm19
    };
    // Cast: an instruction index in a method of a handful of words.
    let target = at as i32 + disp;
    assert!(target > 0, "the branch goes forward, to the stub");
    // Cast: checked positive above.
    let target = target as usize;
    // The stub's first word is `MOVZ X0, #action`.
    assert_eq!(
        words[target],
        0xD280_0000 | (u32::from(npe_action::ARRAY_LENGTH) << 5),
        "the CBZ must land on MOVZ X0, #ARRAY_LENGTH: {words:08x?} target={target}"
    );
}

/// The NPE path is exact only with BOTH halves wired, exactly as the
/// `ArithmeticException` path is: a method whose exception table is not
/// known empty could catch the NPE, and the drain's unknown throw pc
/// cannot pick the handler. Either missing refuses by the arm, with
/// nothing emitted.
#[test]
pub(in crate::aarch64_backend::tests) fn r9w12_arraylength_refuses_without_the_helper_or_the_empty_table_word() {
    for (helper, table_empty, why) in [
        (R9W12_NPE, false, "exception table not known empty"),
        (0, true, "no jit_npe_with_action helper"),
        (0, false, "neither"),
    ] {
        let r = compile_npe("([I)I", 1, &[0x2a, 0xbe, 0xac], helper, table_empty);
        assert!(!r.success, "{why}: arraylength must refuse");
        assert!(
            !r.instructions
                .iter()
                .any(|i| matches!(i, Arm64Instruction::MemLoad { .. })),
            "{why}: no length load may be emitted"
        );
        assert!(
            r.instructions.iter().any(|i| matches!(
                i,
                Arm64Instruction::Comment(c)
                    if c.contains("no exact NullPointerException path")
            )),
            "{why}: refused by the arraylength arm"
        );
    }
}

// -- Round 9 wave 13: array elements, bounds checks and the AIOOBE path -

pub(in crate::aarch64_backend::tests) const R9W13_AIOOBE: usize = 0x7FFF_0000_4000;

/// Compile a static method with BOTH array throw paths wired as the caller
/// would wire them.
pub(in crate::aarch64_backend::tests) fn compile_array(
    descriptor: &str,
    slots: usize,
    code: &[u8],
    npe: usize,
    aioobe: usize,
    table_empty: bool,
) -> Arm64CompileResult {
    let mut b = Arm64Backend::new();
    b.set_safepoints_enabled(false);
    // SAFETY: plain struct of `usize` addresses; all-zero is "nothing wired".
    let mut h: crate::JitRuntimeHelpers = unsafe { std::mem::zeroed() };
    h.jit_npe_with_action = npe;
    h.throw_aioobe = aioobe;
    b.set_helpers(h);
    b.set_method_descriptor(descriptor, true);
    b.set_exception_table_empty(table_empty);
    b.compile_method(slots, slots, 4, code)
}

/// `ADD Xd, Xn, Xm, LSL #amount` encodes to the ARM ARM word for every
/// element scale a Java array has, and a scale the table cannot produce
/// refuses the method rather than truncating into the imm6.
#[test]
pub(in crate::aarch64_backend::tests) fn r9w13_add_lsl_encodes_every_array_scale() {
    let (rd, rn, rm) = (Arm64Register::X11, Arm64Register::X9, Arm64Register::X10);
    let insts: Vec<Arm64Instruction> = (0u8..=3)
        .map(|shift| Arm64Instruction::AddLsl { rd, rn, rm, shift })
        .collect();
    let bytes = emit_machine_code(&result_from_instructions(insts)).expect("encodes");
    assert_eq!(
        words_of(&bytes),
        vec![
            0x8B0A_012Bu32, // ADD X11, X9, X10           (byte[]/boolean[])
            0x8B0A_052B,    // ADD X11, X9, X10, LSL #1   (char[]/short[])
            0x8B0A_092B,    // ADD X11, X9, X10, LSL #2   (int[]/float[])
            0x8B0A_0D2B,    // ADD X11, X9, X10, LSL #3   (long[]/double[])
        ]
    );
    let bad = result_from_instructions(vec![Arm64Instruction::AddLsl {
        rd,
        rn,
        rm,
        shift: 4,
    }]);
    assert!(
        emit_machine_code(&bad).is_none(),
        "a scale outside 0..=3 is a lowering bug, not a wide immediate"
    );
}

/// Every primitive array LOAD: the guards, the scaled address, the
/// element-width load and the JVMS narrowing.
#[test]
pub(in crate::aarch64_backend::tests) fn r9w13_array_loads_guard_scale_and_narrow() {
    for (opcode, descriptor, ret, width, shift, action, extend) in [
        (
            0x2eu8,
            "([II)I",
            0xacu8,
            Arm64MemWidth::W32,
            2u8,
            npe_action::ALOAD_INT,
            None,
        ),
        (
            0x2f,
            "([JI)J",
            0xad,
            Arm64MemWidth::X64,
            3,
            npe_action::ALOAD_LONG,
            None,
        ),
        (
            0x33,
            "([BI)I",
            0xac,
            Arm64MemWidth::B8,
            0,
            npe_action::ALOAD_BYTE,
            Some("sxtb"),
        ),
        (
            0x34,
            "([CI)I",
            0xac,
            Arm64MemWidth::H16,
            1,
            npe_action::ALOAD_CHAR,
            None,
        ),
        (
            0x35,
            "([SI)I",
            0xac,
            Arm64MemWidth::H16,
            1,
            npe_action::ALOAD_SHORT,
            Some("sxth"),
        ),
    ] {
        // aload_0; iload_1; <load>; <return>
        let r = compile_array(
            descriptor,
            2,
            &[0x2a, 0x1b, opcode, ret],
            R9W12_NPE,
            R9W13_AIOOBE,
            true,
        );
        assert!(r.success, "0x{opcode:02x} must compile");

        let at = r
            .instructions
            .iter()
            .position(|i| matches!(i, Arm64Instruction::Cbz { .. }))
            .expect("the receiver is null-checked");
        let Arm64Instruction::Cbz {
            rt: arr,
            label: npe,
        } = r.instructions[at]
        else {
            unreachable!()
        };
        // The NPE stub names this opcode's own action.
        let stub = r
            .instructions
            .iter()
            .position(|i| matches!(i, Arm64Instruction::Label(l) if *l == npe))
            .expect("the NPE label is bound");
        assert!(
            matches!(
                r.instructions[stub + 1],
                Arm64Instruction::MovImm { imm, .. } if imm == i64::from(action)
            ),
            "0x{opcode:02x}: the null check reports its own JEP-358 action"
        );

        // The length load, then an UNSIGNED compare and a B.HS.
        let rest = &r.instructions[at + 1..];
        let len_at = rest
            .iter()
            .position(|i| matches!(i, Arm64Instruction::CmpW { .. }))
            .expect("the index is compared against the length");
        match (&rest[len_at - 2], &rest[len_at - 1], &rest[len_at]) {
            (
                Arm64Instruction::AddImm { rd, rn, imm },
                Arm64Instruction::MemLoad {
                    rt,
                    width: Arm64MemWidth::W32,
                    acquire: false,
                    ..
                },
                Arm64Instruction::CmpW { rm, .. },
            ) => {
                assert_eq!(*rn, arr, "the length is read off the array");
                // Cast: a small header constant, as the lowering passes it.
                assert_eq!(*imm, cratonvm_types::ARRAY_LENGTH_OFFSET as i32);
                assert_eq!(*rd, *rt);
                assert_eq!(*rm, *rt, "the compare's rhs IS the loaded length");
            }
            other => panic!("0x{opcode:02x}: unexpected bounds-check shape: {other:?}"),
        }
        assert!(
            matches!(
                rest[len_at + 1],
                Arm64Instruction::BCond {
                    cond: Arm64Condition::Cs,
                    ..
                }
            ),
            "0x{opcode:02x}: the bounds branch is the UNSIGNED >= that also catches idx < 0"
        );

        // The scaled address, then the element load at its own width.
        let scaled = match shift {
            0 => matches!(rest[len_at + 2], Arm64Instruction::Add { .. }),
            s => {
                matches!(rest[len_at + 2], Arm64Instruction::AddLsl { shift, .. } if shift == s)
            }
        };
        assert!(
            scaled,
            "0x{opcode:02x}: the index is scaled by the element size ({shift}): {:?}",
            rest[len_at + 2]
        );
        assert!(
            matches!(
                rest[len_at + 3],
                // Cast: a small header constant, as the lowering passes it.
                Arm64Instruction::AddImm { imm, .. } if imm == cratonvm_types::ARRAY_DATA_OFFSET as i32
            ),
            "0x{opcode:02x}: the data area offset is added"
        );
        assert!(
            matches!(
                rest[len_at + 4],
                Arm64Instruction::MemLoad { width: w, acquire: false, .. } if w == width
            ),
            "0x{opcode:02x}: a plain load of the element's own width: {:?}",
            rest[len_at + 4]
        );
        let narrowing = match extend {
            Some("sxtb") => matches!(rest[len_at + 5], Arm64Instruction::Sxtb { .. }),
            Some("sxth") => matches!(rest[len_at + 5], Arm64Instruction::Sxth { .. }),
            _ => true,
        };
        assert!(
            narrowing,
            "0x{opcode:02x}: expected {extend:?} after the load, got {:?}",
            rest[len_at + 5]
        );
    }
}

/// `faload`/`daload` land the raw bits in an FP register;
/// `fastore`/`dastore` take them back out.
#[test]
pub(in crate::aarch64_backend::tests) fn r9w13_fp_array_elements_move_through_a_bit_exact_fmov() {
    let f = compile_array(
        "([FI)F",
        2,
        &[0x2a, 0x1b, 0x30, 0xae],
        R9W12_NPE,
        R9W13_AIOOBE,
        true,
    );
    assert!(f.success);
    assert!(f
        .instructions
        .iter()
        .any(|i| matches!(i, Arm64Instruction::FmovToFpSingle { .. })));
    let d = compile_array(
        "([DI)D",
        2,
        &[0x2a, 0x1b, 0x31, 0xaf],
        R9W12_NPE,
        R9W13_AIOOBE,
        true,
    );
    assert!(d.success);
    assert!(d
        .instructions
        .iter()
        .any(|i| matches!(i, Arm64Instruction::FmovToFp { .. })));

    // aload_0; iload_1; fload_2 (0x24 -- 0x22 is fload_0); fastore; return
    let fs = compile_array(
        "([FIF)V",
        3,
        &[0x2a, 0x1b, 0x24, 0x51, 0xb1],
        R9W12_NPE,
        R9W13_AIOOBE,
        true,
    );
    assert!(fs.success);
    assert!(fs
        .instructions
        .iter()
        .any(|i| matches!(i, Arm64Instruction::FmovFromFpSingle { .. })));
    // aload_0; iload_1; dload_2 (0x28 -- 0x26 is dload_0); dastore; return
    let ds = compile_array(
        "([DID)V",
        4,
        &[0x2a, 0x1b, 0x28, 0x52, 0xb1],
        R9W12_NPE,
        R9W13_AIOOBE,
        true,
    );
    assert!(ds.success);
    assert!(ds
        .instructions
        .iter()
        .any(|i| matches!(i, Arm64Instruction::FmovFromFp { .. })));
}

/// Every primitive array STORE stores at the element's own width -- which
/// IS the JVMS narrowing for `castore`/`sastore`/`bastore` -- and only
/// `bastore` additionally masks, behind a runtime `boolean[]` test.
#[test]
pub(in crate::aarch64_backend::tests) fn r9w13_array_stores_truncate_and_only_bastore_masks() {
    for (opcode, descriptor, slots, value_op, width, masks) in [
        (0x4fu8, "([III)V", 3usize, 0x1cu8, Arm64MemWidth::W32, false),
        (0x50, "([JIJ)V", 4, 0x20, Arm64MemWidth::X64, false),
        (0x54, "([BII)V", 3, 0x1c, Arm64MemWidth::B8, true),
        (0x55, "([CII)V", 3, 0x1c, Arm64MemWidth::H16, false),
        (0x56, "([SII)V", 3, 0x1c, Arm64MemWidth::H16, false),
    ] {
        // aload_0; iload_1; <value load>; <store>; return
        let r = compile_array(
            descriptor,
            slots,
            &[0x2a, 0x1b, value_op, opcode, 0xb1],
            R9W12_NPE,
            R9W13_AIOOBE,
            true,
        );
        assert!(r.success, "0x{opcode:02x} must compile");
        assert!(
            r.instructions.iter().any(|i| matches!(
                i,
                Arm64Instruction::MemStore { width: w, release: false, .. } if *w == width
            )),
            "0x{opcode:02x}: stores at the element's own width"
        );
        let masked = r
            .instructions
            .iter()
            .any(|i| matches!(i, Arm64Instruction::AndImm { imm: 1, .. }));
        assert_eq!(
            masked, masks,
            "0x{opcode:02x}: only bastore narrows by `& 1`"
        );
        let probes = r.instructions.iter().any(
            |i| matches!(i, Arm64Instruction::Ldrb { offset, .. } if *offset == cratonvm_types::KIND_TAGS_BYTE_OFFSET as i32),
        );
        assert_eq!(
            probes, masks,
            "0x{opcode:02x}: only bastore reads the header's element-type byte"
        );
    }
}

/// The `bastore` mask is guarded by a compare against the exact `[Z`
/// kind/element byte, and is SKIPPED when the compare fails -- a `byte[]`
/// keeps its low-byte truncation.
#[test]
pub(in crate::aarch64_backend::tests) fn r9w13_the_bastore_mask_tests_the_boolean_array_tag_and_skips_otherwise() {
    let tag = cratonvm_types::primitive_array_kind_tags_byte("[Z").expect("[Z has a tag");
    let r = compile_array(
        "([BII)V",
        3,
        &[0x2a, 0x1b, 0x1c, 0x54, 0xb1],
        R9W12_NPE,
        R9W13_AIOOBE,
        true,
    );
    assert!(r.success);
    let at = r
        .instructions
        .iter()
        .position(
            |i| matches!(i, Arm64Instruction::Ldrb { offset, .. } if *offset == cratonvm_types::KIND_TAGS_BYTE_OFFSET as i32),
        )
        .expect("the element-type byte is read");
    match (&r.instructions[at + 1], &r.instructions[at + 2]) {
        (
            Arm64Instruction::CmpImmW { imm, .. },
            Arm64Instruction::BCond {
                cond: Arm64Condition::Ne,
                label,
            },
        ) => {
            assert_eq!(*imm, i32::from(tag), "compared against the `[Z` tag");
            // The AND is between the branch and the label it skips to.
            assert!(matches!(
                r.instructions[at + 3],
                Arm64Instruction::AndImm { imm: 1, .. }
            ));
            assert!(
                matches!(r.instructions[at + 4], Arm64Instruction::Label(l) if l == *label),
                "the B.NE skips exactly the AND"
            );
        }
        other => panic!("unexpected boolean-mask shape: {other:?}"),
    }
}

/// The AIOOBE stub is one per SITE, out of line, and marshals the index,
/// the length, the array and the bci into the helper's four argument
/// registers.
#[test]
pub(in crate::aarch64_backend::tests) fn r9w13_aioobe_stubs_are_per_site_and_marshal_four_arguments() {
    // aload_0; iload_1; iaload; aload_0; iload_1; iaload; iadd; ireturn
    let r = compile_array(
        "([II)I",
        2,
        &[0x2a, 0x1b, 0x2e, 0x2a, 0x1b, 0x2e, 0x60, 0xac],
        R9W12_NPE,
        R9W13_AIOOBE,
        true,
    );
    assert!(r.success);
    let bounds: Vec<(usize, u32)> = r
        .instructions
        .iter()
        .enumerate()
        .filter_map(|(i, inst)| match inst {
            Arm64Instruction::BCond {
                cond: Arm64Condition::Cs,
                label,                } => Some((i, *label)),
            _ => None,
        })
        .collect();
    assert_eq!(bounds.len(), 2, "each access is bounds-checked");
    assert_ne!(
        bounds[0].1, bounds[1].1,
        "two sites cannot share a stub: the bci and the registers differ"
    );

    let ret = r
        .instructions
        .iter()
        .position(|i| matches!(i, Arm64Instruction::Ret))
        .expect("the epilogue returns");
    let mut seen_bcis = Vec::new();
    for (_, label) in bounds {
        let stub = r
            .instructions
            .iter()
            .position(|i| matches!(i, Arm64Instruction::Label(l) if *l == label))
            .expect("the stub label is bound");
        assert!(stub > ret, "the stub is out of line, after the RET");
        match &r.instructions[stub + 1..stub + 8] {
            [Arm64Instruction::Mov { rd: a0, .. }, Arm64Instruction::Mov { rd: a1, .. }, Arm64Instruction::Mov { rd: a2, .. }, Arm64Instruction::MovImm { rd: a3, imm: bci }, Arm64Instruction::MovImm {
                rd: scratch,
                imm: helper,
            }, Arm64Instruction::Blr { rn: called }, Arm64Instruction::B { .. }] => {
                assert_eq!(
                    [*a0, *a1, *a2, *a3],
                    [
                        Arm64Register::X0,
                        Arm64Register::X1,
                        Arm64Register::X2,
                        Arm64Register::X3
                    ],
                    "index, length, array, bci"
                );
                // Cast: the helper address as the pseudo-op carries it.
                assert_eq!(*helper, R9W13_AIOOBE as i64);
                assert_eq!(*called, *scratch);
                seen_bcis.push(*bci);
            }
            other => panic!("unexpected AIOOBE stub shape: {other:?}"),
        }
    }
    assert_eq!(
        seen_bcis,
        vec![2, 5],
        "each stub carries its OWN site's bci"
    );
}

/// The encoded bounds branch really lands on its stub.
#[test]
pub(in crate::aarch64_backend::tests) fn r9w13_the_encoded_bounds_branch_reaches_the_stub() {
    let r = compile_array(
        "([II)I",
        2,
        &[0x2a, 0x1b, 0x2e, 0xac],
        R9W12_NPE,
        R9W13_AIOOBE,
        true,
    );
    assert!(r.success);
    let words = words_of(&emit_machine_code(&r).expect("encodes"));
    // B.cond: 0x54000000 | (imm19 << 5) | cond; HS/CS is cond 0b0010.
    let (at, word) = words
        .iter()
        .enumerate()
        .find(|(_, w)| *w & 0xFF00_001F == 0x5400_0002)
        .map(|(i, w)| (i, *w))
        .expect("a B.HS is encoded");
    // Cast: a 19-bit field, then a small signed instruction count.
    let imm19 = ((word >> 5) & 0x7_FFFF) as i32;
    let disp = if imm19 & 0x4_0000 != 0 {
        imm19 - 0x8_0000
    } else {
        imm19
    };
    // Cast: an instruction index in a method of a handful of words.
    let target = at as i32 + disp;
    assert!(target > 0, "the branch goes forward, to the stub");
    // Cast: checked positive above.
    let target = target as usize;
    // The stub's first word is `MOV X0, Xidx`, i.e. `ORR X0, XZR, Xidx`:
    // 0xAA0003E0 | (Rm << 16).
    assert_eq!(
        words[target] & 0xFFE0_FFFF,
        0xAA00_03E0,
        "the B.HS must land on MOV X0, Xindex: {words:08x?} target={target}"
    );
}

/// A reference element is not lowered, by either opcode, and the refusal
/// is by the array arm rather than by the shared-memory gate.
#[test]
pub(in crate::aarch64_backend::tests) fn r9w13_aaload_and_aastore_still_refuse() {
    // aload_0; iload_1; aaload; areturn
    let load = compile_array(
        "([Ljava/lang/Object;I)Ljava/lang/Object;",
        2,
        &[0x2a, 0x1b, 0x32, 0xb0],
        R9W12_NPE,
        R9W13_AIOOBE,
        true,
    );
    assert!(!load.success, "aaload has no aarch64 lowering");
    // aload_0; iload_1; aload_2; aastore; return
    let store = compile_array(
        "([Ljava/lang/Object;ILjava/lang/Object;)V",
        3,
        &[0x2a, 0x1b, 0x2c, 0x53, 0xb1],
        R9W12_NPE,
        R9W13_AIOOBE,
        true,
    );
    assert!(
        !store.success,
        "aastore owes an SATB barrier and a store check"
    );
    for r in [load, store] {
        assert!(
            r.instructions.iter().any(|i| matches!(
                i,
                Arm64Instruction::Comment(c) if c.contains("a reference element")
            )),
            "refused by the array arm, naming the reason"
        );
    }
}

/// An array access needs BOTH throw paths; either missing refuses with
/// nothing emitted.
#[test]
pub(in crate::aarch64_backend::tests) fn r9w13_array_access_refuses_without_both_throw_paths() {
    for (npe, aioobe, table_empty, why) in [
        (
            R9W12_NPE,
            R9W13_AIOOBE,
            false,
            "exception table not known empty",
        ),
        (0, R9W13_AIOOBE, true, "no NPE helper"),
        (R9W12_NPE, 0, true, "no AIOOBE helper"),
        (0, 0, true, "neither helper"),
    ] {
        let r = compile_array(
            "([II)I",
            2,
            &[0x2a, 0x1b, 0x2e, 0xac],
            npe,
            aioobe,
            table_empty,
        );
        assert!(!r.success, "{why}: iaload must refuse");
        assert!(
            !r.instructions
                .iter()
                .any(|i| matches!(i, Arm64Instruction::MemLoad { .. })),
            "{why}: no element load may be emitted"
        );
        assert!(
            r.instructions.iter().any(|i| matches!(
                i,
                Arm64Instruction::Comment(c) if c.contains("no exact array exception path")
            )),
            "{why}: refused by the array arm"
        );
    }
}

#[test]
pub(in crate::aarch64_backend::tests) fn backend_return_void_emits_branch_to_epilogue() {
    let result = make_backend_with_method(0, 0, &[0xb1]);
    let has_b = result
        .instructions
        .iter()
        .any(|inst| matches!(inst, Arm64Instruction::B { .. }));
    assert!(has_b, "return void should emit B to epilogue");
}

#[test]
pub(in crate::aarch64_backend::tests) fn compile_result_success_flag() {
    let good = make_backend_with_method(0, 0, &[0xb1]); // return void
    assert!(good.success, "simple void return should succeed");

    let bad = make_backend_with_method(0, 0, &[0xFF]); // invalid opcode
    assert!(!bad.success, "unsupported opcode should set success=false");
}

#[test]
pub(in crate::aarch64_backend::tests) fn arm64_stack_alignment_is_16() {
    assert_eq!(Arm64EntryConvention::STACK_ALIGNMENT, 16);
}

// -- Additional integration-level tests ---------------------------------

#[test]
pub(in crate::aarch64_backend::tests) fn compile_simple_add_method() {
    // int add(int a, int b) { return a + b; }
    // Bytecode: iload_0, iload_1, iadd, ireturn
    let result = make_backend_with_method(2, 2, &[0x1a, 0x1b, 0x60, 0xac]);
    assert!(result.success);
    let has_add = result
        .instructions
        .iter()
        .any(|inst| matches!(inst, Arm64Instruction::AddW { .. }));
    assert!(has_add);
}

#[test]
pub(in crate::aarch64_backend::tests) fn compile_bipush() {
    // bipush 42, ireturn
    let result = make_backend_with_method(0, 0, &[0x10, 42, 0xac]);
    assert!(result.success);
    let has_42 = result
        .instructions
        .iter()
        .any(|inst| matches!(inst, Arm64Instruction::MovImm { imm: 42, .. }));
    assert!(has_42);
}

// -- Float/double operation tests ------------------------------------------

/// `float` arithmetic lowers to the S forms and `double` to the D forms.
///
/// Both used to lower to the D forms: `float` was modelled as `double`
/// end to end, so rounding, overflow and the bits handed back to the VM
/// were a double's.
#[test]
pub(in crate::aarch64_backend::tests) fn float_and_double_arithmetic_use_their_own_widths() {
    fn single(op: u8) -> Arm64CompileResult {
        make_backend_with_method(0, 0, &[0x0c, 0x0c, op, 0xb1])
    }
    fn double(op: u8) -> Arm64CompileResult {
        make_backend_with_method(0, 0, &[0x0f, 0x0f, op, 0xb1])
    }
    fn has(r: &Arm64CompileResult, f: fn(&Arm64Instruction) -> bool) -> bool {
        r.success && r.instructions.iter().any(|i| f(i))
    }
    assert!(has(&single(0x62), |i| matches!(
        i,
        Arm64Instruction::FaddSingle { .. }
    )));
    assert!(has(&single(0x66), |i| matches!(
        i,
        Arm64Instruction::FsubSingle { .. }
    )));
    assert!(has(&single(0x6a), |i| matches!(
        i,
        Arm64Instruction::FmulSingle { .. }
    )));
    assert!(has(&single(0x6e), |i| matches!(
        i,
        Arm64Instruction::FdivSingle { .. }
    )));
    assert!(has(&double(0x63), |i| matches!(
        i,
        Arm64Instruction::FaddDouble { .. }
    )));
    assert!(has(&double(0x67), |i| matches!(
        i,
        Arm64Instruction::FsubDouble { .. }
    )));
    assert!(has(&double(0x6b), |i| matches!(
        i,
        Arm64Instruction::FmulDouble { .. }
    )));
    assert!(has(&double(0x6f), |i| matches!(
        i,
        Arm64Instruction::FdivDouble { .. }
    )));
    assert!(
        !single(0x62)
            .instructions
            .iter()
            .any(|i| matches!(i, Arm64Instruction::FaddDouble { .. })),
        "fadd must not use the double form"
    );
    // A float operand is not a double: `dadd` of two `fconst`s is refused.
    assert!(!single(0x63).success, "dadd over floats is ill-typed");
}

/// `i2f` converts from the W register into an S register, `i2d` into a D
/// register, and `l2f`/`l2d` from the X register.
#[test]
pub(in crate::aarch64_backend::tests) fn integer_to_fp_conversions_pick_source_and_destination_widths() {
    fn conv(code: &[u8]) -> Arm64CompileResult {
        make_backend_with_method(0, 0, code)
    }
    let i2f = conv(&[0x04, 0x86, 0xb1]);
    assert!(i2f.success);
    assert!(i2f
        .instructions
        .iter()
        .any(|i| matches!(i, Arm64Instruction::ScvtfSingle { .. })));
    let i2d = conv(&[0x04, 0x87, 0xb1]);
    assert!(i2d
        .instructions
        .iter()
        .any(|i| matches!(i, Arm64Instruction::ScvtfDoubleW { .. })));
    let l2f = conv(&[0x0a, 0x89, 0xb1]);
    assert!(l2f
        .instructions
        .iter()
        .any(|i| matches!(i, Arm64Instruction::ScvtfSingleX { .. })));
    let l2d = conv(&[0x0a, 0x8a, 0xb1]);
    assert!(l2d
        .instructions
        .iter()
        .any(|i| matches!(i, Arm64Instruction::ScvtfDouble { .. })));
    // f2d / d2f are real conversions now, not no-ops.
    let f2d = conv(&[0x0c, 0x8d, 0xb1]);
    assert!(f2d
        .instructions
        .iter()
        .any(|i| matches!(i, Arm64Instruction::FcvtSingleToDouble { .. })));
    let d2f = conv(&[0x0f, 0x90, 0xb1]);
    assert!(d2f
        .instructions
        .iter()
        .any(|i| matches!(i, Arm64Instruction::FcvtDoubleToSingle { .. })));
}

#[test]
pub(in crate::aarch64_backend::tests) fn backend_emit_machine_code_produces_bytes() {
    let result = make_backend_with_method(0, 0, &[0xb1]); // return void
    assert!(result.success);
    let code = emit_machine_code(&result);
    assert!(code.is_some(), "should produce machine code bytes");
    let bytes = code.unwrap();
    assert!(!bytes.is_empty(), "machine code should not be empty");
    // ARM64 instructions are 4 bytes each
    assert_eq!(bytes.len() % 4, 0, "machine code must be 4-byte aligned");
}

#[test]
pub(in crate::aarch64_backend::tests) fn backend_emit_simple_add_produces_code() {
    // int add(int a, int b) { return a + b; }
    let result = make_backend_with_method(2, 2, &[0x1a, 0x1b, 0x60, 0xac]);
    assert!(result.success);
    let code = emit_machine_code(&result);
    assert!(code.is_some());
    let bytes = code.unwrap();
    assert!(
        bytes.len() >= 16,
        "add method should produce at least a few instructions"
    );
}

/// `fneg`/`dneg` flip the sign bit with FNEG.
///
/// Formerly `backend_float_neg_emits_fsub`, which asserted the bug: the
/// lowering computed `0.0 - x`, and `0.0 - 0.0` is `+0.0`, so `-(0.0f)`
/// lost its sign.
#[test]
pub(in crate::aarch64_backend::tests) fn fneg_and_dneg_use_fneg_not_a_subtraction_from_zero() {
    for (code, single) in [
        (&[0x0c, 0x76, 0xb1][..], true),
        (&[0x0f, 0x77, 0xb1][..], false),
    ] {
        let result = make_backend_with_method(0, 0, code);
        assert!(result.success);
        let neg = result.instructions.iter().any(|i| {
            if single {
                matches!(i, Arm64Instruction::FnegSingle { .. })
            } else {
                matches!(i, Arm64Instruction::FnegDouble { .. })
            }
        });
        assert!(neg, "negation must be FNEG of the operand's own width");
        assert!(
            !result.instructions.iter().any(|i| matches!(
                i,
                Arm64Instruction::FsubDouble { .. } | Arm64Instruction::FsubSingle { .. }
            )),
            "0.0 - x turns -0.0 into +0.0"
        );
    }
}

#[test]
pub(in crate::aarch64_backend::tests) fn backend_float_rem_bails_to_interpreter() {
    // fconst_1, fconst_1, frem (0x72), return void.
    //
    // frem/drem have no EXACT ARM64 lowering in this backend (the
    // truncating FCVTZS round-trip saturates for large operands), so the
    // dispatch bails to the interpreter rather than emit a silently-wrong
    // result. The compile must report failure and emit_machine_code must
    // return None.
    let result = make_backend_with_method(0, 0, &[0x0c, 0x0c, 0x72, 0xb1]);
    assert!(
        !result.success,
        "frem should bail (success=false), not emit an inexact remainder"
    );
    assert!(
        emit_machine_code(&result).is_none(),
        "a failed compile must not produce machine code"
    );
}

#[test]
pub(in crate::aarch64_backend::tests) fn backend_fconst_bit_exact_for_non_integral() {
    // A constant must move the IEEE-754 bit pattern, NOT perform an
    // integer→float conversion, which would round 2.5 down to 2.0. And a
    // `float` constant must be the SINGLE pattern into an S register.
    let mut single = Arm64Backend::new();
    single.emit_fconst(2.5);
    assert!(single.buffer.instructions().iter().any(
        |i| matches!(i, Arm64Instruction::MovImm { imm, .. } if *imm == i64::from(2.5f32.to_bits()))
    ));
    assert!(single
        .buffer
        .instructions()
        .iter()
        .any(|i| matches!(i, Arm64Instruction::FmovToFpSingle { .. })));
    assert_eq!(single.operand_stack[0].kind, OperandKind::F32);

    let mut backend = Arm64Backend::new();
    backend.emit_dconst(2.5);
    let expected_bits = 2.5f64.to_bits() as i64;
    let has_movimm_bits = backend.buffer.instructions().iter().any(
        |inst| matches!(inst, Arm64Instruction::MovImm { imm, .. } if *imm == expected_bits),
    );
    assert!(
        has_movimm_bits,
        "emit_fconst should materialize the exact f64 bit pattern {:#018x}",
        expected_bits as u64
    );
    let has_fmov = backend
        .buffer
        .instructions()
        .iter()
        .any(|inst| matches!(inst, Arm64Instruction::FmovToFp { .. }));
    assert!(
        has_fmov,
        "emit_fconst should bit-move the pattern into FP via FmovToFp"
    );
    let has_scvtf = backend
        .buffer
        .instructions()
        .iter()
        .any(|inst| matches!(inst, Arm64Instruction::ScvtfDouble { .. }));
    assert!(
        !has_scvtf,
        "emit_fconst must NOT use ScvtfDouble (integer→float conversion)"
    );
}

#[test]
pub(in crate::aarch64_backend::tests) fn backend_ldc_bails_to_interpreter() {
    // ldc (0x12) #1, return void. The backend has no constant pool, so it
    // must bail (success=false) rather than guess at the constant.
    let result = make_backend_with_method(0, 0, &[0x12, 0x01, 0xb1]);
    assert!(
        !result.success,
        "ldc should bail to the interpreter (no constant pool available)"
    );
    // ldc2_w (0x14) #1, return void — same reasoning (long/double constant).
    let result2 = make_backend_with_method(0, 0, &[0x14, 0x00, 0x01, 0xb1]);
    assert!(!result2.success, "ldc2_w should bail to the interpreter");
}

#[test]
pub(in crate::aarch64_backend::tests) fn addsub_imm_safe_no_truncation_for_wide_immediate() {
    use crate::aarch64::{Aarch64Emitter, Reg};

    // Small immediate (fits 12 bits): one ADD-immediate instruction.
    let mut e_small = Aarch64Emitter::new();
    assert!(emit_addsub_imm_safe(
        &mut e_small,
        Reg::X9,
        Reg::X9,
        5,
        false
    ));
    assert_eq!(
        e_small.code().len(),
        4,
        "a 12-bit immediate should lower to a single ADD-imm"
    );

    // Wide immediate (> 0xFFF, not 4 KiB-aligned): must NOT be truncated to
    // a single ADD-imm. It is materialized into X16 (one or more MOV-wide)
    // then added by register, so the sequence is longer than one
    // instruction and never encodes the (wrong) masked immediate.
    let wide = 5000i32; // 0x1388 — low 12 bits 0x388 != 0, > 0xFFF
    let mut e_wide = Aarch64Emitter::new();
    assert!(emit_addsub_imm_safe(
        &mut e_wide,
        Reg::X9,
        Reg::X9,
        wide,
        false
    ));
    assert!(
        e_wide.code().len() > 4,
        "a >12-bit immediate must not collapse into one (truncated) ADD-imm"
    );

    // Compare against what a (buggy) single truncated ADD-imm would encode,
    // and assert the safe lowering's first 4 bytes are NOT that instruction.
    let mut e_trunc = Aarch64Emitter::new();
    // The old buggy path: add_imm with the value masked to 12 bits.
    e_trunc.add_imm(Reg::X9, Reg::X9, (wide as u16) & 0xFFF, false);
    let trunc_first = &e_trunc.code()[0..4];
    assert_ne!(
        &e_wide.code()[0..4],
        trunc_first,
        "safe lowering must not begin with the truncated ADD-imm encoding"
    );
}

#[test]
pub(in crate::aarch64_backend::tests) fn backend_fcmp_emits_fcmpdouble() {
    // fconst_1, fconst_1, fcmpl (0x95), return void
    let result = make_backend_with_method(0, 0, &[0x0c, 0x0c, 0x95, 0xb1]);
    assert!(result.success, "fcmpl should succeed");
    let has_fcmp = result
        .instructions
        .iter()
        .any(|inst| matches!(inst, Arm64Instruction::FcmpSingle { .. }));
    assert!(has_fcmp, "fcmpl of two floats should emit FcmpSingle");
}

#[test]
pub(in crate::aarch64_backend::tests) fn backend_emit_machine_code_with_float_ops() {
    // fconst_1, fconst_1, fadd, return void
    let result = make_backend_with_method(0, 0, &[0x0c, 0x0c, 0x62, 0xb1]);
    assert!(result.success);
    let code = emit_machine_code(&result);
    assert!(
        code.is_some(),
        "float operations should produce machine code"
    );
    let bytes = code.unwrap();
    assert_eq!(bytes.len() % 4, 0, "machine code must be 4-byte aligned");
}

// --- M28 fix tests: comparison opcodes, lcmp, idiv ---

#[test]
pub(in crate::aarch64_backend::tests) fn backend_iflt_opcode() {
    // iconst_1, iflt +3 (offset to return), return
    // 0x04=iconst_1, 0x9b=iflt, 00 06=offset +6 (to return), 0xb1=return
    let result = make_backend_with_method(0, 0, &[0x04, 0x9b, 0x00, 0x06, 0xb1, 0xb1]);
    assert!(result.success, "iflt should compile successfully");
}

#[test]
pub(in crate::aarch64_backend::tests) fn backend_ifge_opcode() {
    let result = make_backend_with_method(0, 0, &[0x04, 0x9c, 0x00, 0x06, 0xb1, 0xb1]);
    assert!(result.success, "ifge should compile successfully");
}

#[test]
pub(in crate::aarch64_backend::tests) fn backend_ifgt_opcode() {
    let result = make_backend_with_method(0, 0, &[0x04, 0x9d, 0x00, 0x06, 0xb1, 0xb1]);
    assert!(result.success, "ifgt should compile successfully");
}

#[test]
pub(in crate::aarch64_backend::tests) fn backend_ifle_opcode() {
    let result = make_backend_with_method(0, 0, &[0x04, 0x9e, 0x00, 0x06, 0xb1, 0xb1]);
    assert!(result.success, "ifle should compile successfully");
}

#[test]
pub(in crate::aarch64_backend::tests) fn backend_lcmp_opcode() {
    // lconst_0, lconst_1, lcmp, ireturn
    // 0x09=lconst_0, 0x0a=lconst_1, 0x94=lcmp, 0xac=ireturn
    let result = make_backend_with_method(0, 0, &[0x09, 0x0a, 0x94, 0xac]);
    assert!(result.success, "lcmp should compile successfully");
}

// `backend_idiv_opcode` (iconst_2, iconst_1, idiv, ireturn — asserted
// `success`) was removed with the 82a9d08fc div/rem refusal: inverted it
// would assert strictly less than
// `backend_int_div_bails_with_constant_operands` above, on the same shape.

#[test]
pub(in crate::aarch64_backend::tests) fn backend_ifeq_uses_label_not_raw_target() {
    // iconst_0, ifeq +5 (to second return), return, return
    // 0x03=iconst_0, 0x99=ifeq, 0x00 0x05=branch offset
    let result = make_backend_with_method(0, 0, &[0x03, 0x99, 0x00, 0x05, 0xb1, 0xb1]);
    assert!(
        result.success,
        "ifeq with label should compile successfully"
    );
}

#[test]
pub(in crate::aarch64_backend::tests) fn backend_bounds_check_on_truncated_bytecode() {
    // Truncated ifeq: only 2 bytes instead of 3 (opcode + 2 offset bytes)
    let result = make_backend_with_method(0, 0, &[0x99, 0x00]);
    // Should fail gracefully, not panic
    assert!(
        !result.success || result.instructions.is_empty(),
        "truncated branch should not produce valid code"
    );
}

// ===================================================================
// Phase 95 — ARM64 graph-coloring register allocator integration
// ===================================================================

#[test]
pub(in crate::aarch64_backend::tests) fn p95_backend_uses_graph_coloring_for_locals() {
    // int f(int a, int b) { int c = a + b; return c; }
    // iload_0, iload_1, iadd, istore_2, iload_2, ireturn
    let result = make_backend_with_method(3, 2, &[0x1a, 0x1b, 0x60, 0x3d, 0x1c, 0xac]);
    assert!(result.success);
    // Graph coloring may assign 2 or 3 regs — local 2 might share a register
    // with local 0 or 1 since their live ranges don't fully overlap.
    let saved_count = result.frame.saved_regs.len();
    assert!(
        saved_count >= 2 && saved_count <= 3,
        "graph coloring should identify 2-3 used callee-saved regs, got {}",
        saved_count
    );
    for reg in &result.frame.saved_regs {
        assert!(
            reg.is_callee_saved(),
            "saved reg X{} should be callee-saved",
            reg.0
        );
    }
}

#[test]
pub(in crate::aarch64_backend::tests) fn p95_backend_non_interfering_locals_can_share_nothing_extra() {
    // Locals used in sequence (no simultaneous live ranges):
    // iload_0; istore_2; iload_1; ireturn
    // Locals 0, 1 are params (live at entry), local 2 is temp.
    let result = make_backend_with_method(3, 2, &[0x1a, 0x3d, 0x1b, 0xac]);
    assert!(result.success);
    // All 3 locals should get registers (no spills needed).
    let saved_count = result.frame.saved_regs.len();
    assert!(
        saved_count <= 3 && saved_count >= 2,
        "expected 2-3 callee-saved regs, got {saved_count}"
    );
}

#[test]
pub(in crate::aarch64_backend::tests) fn p95_backend_spill_slot_for_11th_local() {
    // 11 locals, all loaded → 10 get regs, 1 spills.
    // Load local 10 (spilled) and return it.
    let result = make_backend_with_method(11, 0, &[0x15, 10, 0xac]);
    assert!(result.success);
    // Local 10 should be loaded from a frame spill slot (Ldr from FP).
    let has_spill_load = result.instructions.iter().any(|inst| {
        matches!(
            inst,
            Arm64Instruction::Ldr { rn, .. } if *rn == Arm64Register::FP
        )
    });
    assert!(has_spill_load, "11th local should spill to stack");    }

/// Float locals must NOT be homed in `D8`–`D15`.
///
/// Replaces `p95_backend_float_local_uses_fp_reg`, which asserted the
/// opposite. `regalloc::ARM64_LOCAL_FPS` is `D8..D15`, which AAPCS64 makes
/// **callee-saved**, and this backend's prologue/epilogue save only GPRs —
/// so homing a float local there destroyed the caller's copy. See the
/// comment on the `float_local_regs` loop in `compile_pass`.
///
/// The observable consequence is that a spilled float local goes through a
/// frame slot (`FpStr`/`FpLdr`) or a GPR bit-move, never `FmovFp` from a
/// callee-saved D register.
#[test]
pub(in crate::aarch64_backend::tests) fn float_locals_never_use_callee_saved_fp_regs() {
    // fconst_1 (0x0c), fstore_0 (0x43), fload_0 (0x22), return void (0xb1)
    let result = make_backend_with_method(1, 0, &[0x0c, 0x43, 0x22, 0xb1]);
    assert!(result.success);

    // No instruction may name an FP register outside the caller-saved
    // scratch set V0-V7 (encoded 32..=39). D8-D15 would appear as 40..=47.
    let mut offenders: Vec<u8> = Vec::new();
    fn note(reg: Arm64Register, offenders: &mut Vec<u8>) {
        if reg.0 >= 40 {
            offenders.push(reg.0);
        }
    }
    for inst in &result.instructions {
        match inst {
            Arm64Instruction::FmovFp { vd, vn } => {
                note(*vd, &mut offenders);
                note(*vn, &mut offenders);
            }
            Arm64Instruction::FmovToFp { vd, .. } => note(*vd, &mut offenders),
            Arm64Instruction::FmovFromFp { vn, .. } => note(*vn, &mut offenders),
            Arm64Instruction::FpLdr { vt, .. } | Arm64Instruction::FpStr { vt, .. } => {
                note(*vt, &mut offenders)
            }
            _ => {}
        }
    }
    assert!(
        offenders.is_empty(),
        "float locals must not be homed in callee-saved D8-D15 \
         (this backend never saves them); saw register encodings {offenders:?}"
    );

    // And the value really does round-trip through a frame slot, i.e. the
    // spill path — not silently dropped.
    let has_fp_spill = result
        .instructions
        .iter()
        .any(|i| matches!(i, Arm64Instruction::FpStr { .. }));
    let has_gpr_home = result
        .instructions
        .iter()
        .any(|i| matches!(i, Arm64Instruction::FmovFromFp { .. }));
    assert!(
        has_fp_spill || has_gpr_home,
        "fstore_0 must store the float somewhere (frame slot or GPR)"
    );
}

/// The FP frame-slot lowering must never use the scaled unsigned-offset
/// form for a negative displacement.
///
/// `Arm64FrameLayout::spill_offset` is always negative, and the previous
/// lowering did `offset as u16` — turning −24 into 65512, which the encoder
/// then scales by 8. That is a load/store ~64 KiB *above* FP, inside the
/// caller's frame. This checks the encoded bytes directly: an unscaled
/// LDUR/STUR (bit 24 clear) rather than the scaled form (bit 24 set).
#[test]
pub(in crate::aarch64_backend::tests) fn fp_frame_slot_access_uses_unscaled_form_for_negative_offsets() {
    let result = result_from_instructions(vec![
        Arm64Instruction::FpStr {
            vt: Arm64Register::V0,
            rn: Arm64Register::FP,
            offset: -24,
            is_double: true,
        },
        Arm64Instruction::FpLdr {
            vt: Arm64Register::V1,
            rn: Arm64Register::FP,
            offset: -24,
            is_double: true,
        },
        Arm64Instruction::Ret,
    ]);
    let bytes = emit_machine_code(&result).expect("FP frame access must encode");
    assert_eq!(bytes.len(), 3 * 4);

    // STUR D0, [X29, #-24] = 0xFC1E83A0; LDUR D1, [X29, #-24] = 0xFC5E83A1.
    // (Derived from the GPR STUR/LDUR words with the V bit — 1<<26 — set;
    // imm9 = -24 & 0x1FF = 0x1E8.)
    let w0 = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    let w1 = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    assert_eq!(w0, 0xFC1E_83A0, "STUR D0, [X29, #-24]");
    assert_eq!(w1, 0xFC5E_83A1, "LDUR D1, [X29, #-24]");

    for (name, w) in [("store", w0), ("load", w1)] {
        assert_eq!(
            (w >> 24) & 1,
            0,
            "{name} must NOT be the scaled unsigned-offset form (that form \
             cannot encode a negative displacement — it reads -24 as 65512)"
        );
        assert_eq!(
            (w >> 10) & 0x3,
            0b00,
            "{name} must not write back to the base register (FP)"
        );
    }
}

/// A positive, correctly-scaled FP offset must still take the compact
/// scaled form, so the fix above is a routing change and not a blanket
/// switch to the (shorter-range) unscaled encoding.
#[test]
pub(in crate::aarch64_backend::tests) fn fp_positive_aligned_offset_still_uses_scaled_form() {
    let result = result_from_instructions(vec![
        Arm64Instruction::FpLdr {
            vt: Arm64Register::V0,
            rn: Arm64Register::X1,
            offset: 16,
            is_double: true,
        },
        Arm64Instruction::Ret,
    ]);
    let bytes = emit_machine_code(&result).expect("must encode");
    let w = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    // LDR D0, [X1, #16] — scaled unsigned form has bit 24 set, imm12 = 2.
    assert_eq!(
        (w >> 24) & 1,
        1,
        "positive aligned offset uses the scaled form"
    );
    assert_eq!((w >> 10) & 0xFFF, 2, "imm12 must be 16/8 == 2");
}

/// An FP offset outside the imm9 range must materialize the address rather
/// than truncate. 4 words: MOVN/MOVZ(+MOVK) into IP0, ADD, then the access.
#[test]
pub(in crate::aarch64_backend::tests) fn fp_far_offset_materializes_address() {
    let result = result_from_instructions(vec![
        Arm64Instruction::FpStr {
            vt: Arm64Register::V0,
            rn: Arm64Register::FP,
            offset: -100_000,
            is_double: true,
        },
        Arm64Instruction::Ret,
    ]);
    let bytes = emit_machine_code(&result).expect("must encode");
    assert!(
        bytes.len() > 2 * 4,
        "a far FP offset must expand into an address materialization, \
         not a single truncated access"
    );
    // Last instruction before RET is the zero-offset access off IP0 (X16).
    let n = bytes.len();
    let access = u32::from_le_bytes([bytes[n - 8], bytes[n - 7], bytes[n - 6], bytes[n - 5]]);
    assert_eq!((access >> 5) & 0x1F, 16, "access base must be IP0 (X16)");
    assert_eq!(
        (access >> 12) & 0x1FF,
        0,
        "materialized access uses offset 0"
    );
}

// ===================================================================
// Phase 95.2 — New bytecode compilation tests
// ===================================================================

#[test]
pub(in crate::aarch64_backend::tests) fn p95_aload_astore_compiles() {
    // aload_0, astore_1, aload_1, areturn
    let result = make_backend_with_method(2, 1, &[0x2a, 0x4c, 0x2b, 0xb0]);
    assert!(result.success, "aload/astore should compile");
}

#[test]
pub(in crate::aarch64_backend::tests) fn p95_iinc_compiles() {
    // iload_0, iinc 0 5, iload_0, ireturn
    let result = make_backend_with_method(1, 1, &[0x1a, 0x84, 0x00, 0x05, 0x1a, 0xac]);
    assert!(result.success, "iinc should compile");
    // Should emit AddImm for the increment
    let has_add = result
        .instructions
        .iter()
        .any(|inst| matches!(inst, Arm64Instruction::AddImmW { imm: 5, .. }));
    assert!(has_add, "iinc +5 should emit the 32-bit AddImmW with imm=5");
}

#[test]
pub(in crate::aarch64_backend::tests) fn p95_iinc_negative_compiles() {
    // iinc 0 -1
    let result = make_backend_with_method(1, 1, &[0x84, 0x00, 0xFF, 0x1a, 0xac]);
    assert!(result.success, "iinc -1 should compile");
    let has_sub = result
        .instructions
        .iter()
        .any(|inst| matches!(inst, Arm64Instruction::SubImmW { imm: 1, .. }));
    assert!(has_sub, "iinc -1 should emit the 32-bit SubImmW with imm=1");
}

#[test]
pub(in crate::aarch64_backend::tests) fn p95_iushr_compiles() {
    // iconst_1, iconst_1, iushr, ireturn
    let result = make_backend_with_method(0, 0, &[0x04, 0x04, 0x7c, 0xac]);
    assert!(result.success, "iushr should compile");
    let has_lsr = result
        .instructions
        .iter()
        .any(|inst| matches!(inst, Arm64Instruction::LsrW { .. }));
    assert!(
        has_lsr,
        "iushr should emit the 32-bit LsrW, which shifts by the amount mod 32"
    );
}

/// Constant-operand counterpart of
/// `backend_irem_and_lrem_bail_to_interpreter`. The old `a - (a/b)*b`
/// lowering is what made `x % 0` return `x` (AArch64 `SDIV` by zero yields
/// 0 instead of trapping), so the assertion is that no `Msub` is emitted,
/// not merely that the method bailed.
#[test]
pub(in crate::aarch64_backend::tests) fn p95_irem_bails_with_constant_operands() {
    // iconst_5, iconst_2, irem, ireturn
    let result = make_backend_with_method(0, 0, &[0x08, 0x05, 0x70, 0xac]);
    assert!(
        !result.success,
        "irem must bail, constant operands included"
    );
    let has_msub = result
        .instructions
        .iter()
        .any(|inst| matches!(inst, Arm64Instruction::Msub { .. }));
    assert!(
        !has_msub,
        "no Msub may be emitted: `a - (a/b)*b` is exactly the lowering that \
         returned `a` for `a % 0`"
    );
}

#[test]
pub(in crate::aarch64_backend::tests) fn p95_dup_x1_compiles() {
    // iconst_1, iconst_2, dup_x1, pop, pop, pop, return
    let result = make_backend_with_method(0, 0, &[0x04, 0x05, 0x5a, 0x57, 0x57, 0x57, 0xb1]);
    assert!(result.success, "dup_x1 should compile");
}

#[test]
pub(in crate::aarch64_backend::tests) fn p95_dup2_compiles() {
    // iconst_1, iconst_2, dup2, pop, pop, pop, pop, return
    let result =
        make_backend_with_method(0, 0, &[0x04, 0x05, 0x5c, 0x57, 0x57, 0x57, 0x57, 0xb1]);
    assert!(result.success, "dup2 should compile");
}

#[test]
pub(in crate::aarch64_backend::tests) fn p95_tableswitch_compiles() {
    // iconst_1, tableswitch { low=0, high=2, default→8, 0→8, 1→8, 2→8 }, return
    // PC 0: iconst_1 (opcode 0x04)
    // PC 1: tableswitch (0xaa)
    // Padding to align to 4: 2 bytes (pc 2, 3)
    // PC 4: default offset: +7 → target PC 8
    // PC 8: low = 0
    // PC 12: high = 2
    // PC 16: offset[0] = +7 → target PC 8
    // PC 20: offset[1] = +7
    // PC 24: offset[2] = +7
    // PC 28: return
    let bytecode = &[
        0x04, // 0: iconst_1
        0xaa, // 1: tableswitch
        0x00, 0x00, // 2-3: padding
        0x00, 0x00, 0x00, 0x1b, // 4: default → +27 → PC 28
        0x00, 0x00, 0x00, 0x00, // 8: low = 0
        0x00, 0x00, 0x00, 0x02, // 12: high = 2
        0x00, 0x00, 0x00, 0x1b, // 16: case 0 → +27 → PC 28
        0x00, 0x00, 0x00, 0x1b, // 20: case 1 → +27
        0x00, 0x00, 0x00, 0x1b, // 24: case 2 → +27
        0xb1, // 28: return
    ];
    let result = make_backend_with_method(0, 0, bytecode);
    assert!(result.success, "tableswitch should compile");
}

#[test]
pub(in crate::aarch64_backend::tests) fn p95_lookupswitch_compiles() {
    // iconst_1, lookupswitch { npairs=2, default→done, 1→done, 42→done }, return
    let _bytecode = &[
        0x04, // 0: iconst_1
        0xab, // 1: lookupswitch
        0x00, 0x00, // 2-3: padding
        0x00, 0x00, 0x00, 0x1c, // 4: default → +28 → PC 29
        0x00, 0x00, 0x00, 0x02, // 8: npairs = 2
        0x00, 0x00, 0x00, 0x01, // 12: match 1
        0x00, 0x00, 0x00, 0x1c, // 16: → +28
        0x00, 0x00, 0x00, 0x2a, // 20: match 42
        0x00, 0x00, 0x00, 0x1c, // 24: → +28
        0xb1, // 28: return (at PC 28 but target is 29, let me fix)
    ];
    // Target PC should be 1 + 28 = 29, but that's past the bytecode. Let me make it target PC 28.
    let bytecode2 = &[
        0x04, // 0: iconst_1
        0xab, // 1: lookupswitch
        0x00, 0x00, // 2-3: padding
        0x00, 0x00, 0x00, 0x1b, // 4: default → +27 → PC 28
        0x00, 0x00, 0x00, 0x02, // 8: npairs = 2
        0x00, 0x00, 0x00, 0x01, // 12: match 1
        0x00, 0x00, 0x00, 0x1b, // 16: → +27 → PC 28
        0x00, 0x00, 0x00, 0x2a, // 20: match 42
        0x00, 0x00, 0x00, 0x1b, // 24: → +27 → PC 28
        0xb1, // 28: return
    ];
    let result = make_backend_with_method(0, 0, bytecode2);
    assert!(result.success, "lookupswitch should compile");
}

#[test]
pub(in crate::aarch64_backend::tests) fn p95_if_acmpeq_compiles() {
    // aconst_null, aconst_null, if_acmpeq +5, return, return
    let result = make_backend_with_method(0, 0, &[0x01, 0x01, 0xa5, 0x00, 0x05, 0xb1, 0xb1]);
    assert!(result.success, "if_acmpeq should compile");
}

#[test]
pub(in crate::aarch64_backend::tests) fn p95_lshl_lushr_compiles() {
    // lconst_1, iconst_1, lshl, iconst_1, lushr, lreturn. The shift amount
    // is an `int` (JVMS 6.5 `lshl`); an earlier version of this test shifted
    // by a `long`, which the typed operand stack now refuses.
    let result = make_backend_with_method(0, 0, &[0x0a, 0x04, 0x79, 0x04, 0x7d, 0xad]);
    assert!(result.success, "lshl/lushr should compile");
}

#[test]
pub(in crate::aarch64_backend::tests) fn p95_nop_compiles() {
    let result = make_backend_with_method(0, 0, &[0x00, 0xb1]);
    assert!(result.success, "nop should compile");
}

/// A loop must be REFUSED, because this backend emits no safepoint poll.
///
/// Formerly `p95_backend_fibonacci_compiles`, which asserted the opposite.
/// See [`Arm64Backend::label_for_pc`]: x86-64 polls
/// `helpers.safepoint_flag_addr` at every back-edge
/// (`x64::Backend::emit_safepoint_poll`); this backend has no poll and no
/// way to emit one, so a compiled loop is a region a thread can sit in
/// forever without ever reaching a stop-the-world request — the GC then
/// hangs the VM. Refusing the method and interpreting it is the only sound
/// option, and interpretation restores the interpreter's own polls.
#[test]
pub(in crate::aarch64_backend::tests) fn loop_method_bails_no_safepoint_poll() {
    // Fibonacci-like: int fib(int n) with loop
    // local 0 = n (param), local 1 = a = 0, local 2 = b = 1, local 3 = tmp
    // istore_1(a=0), iconst_1, istore_2(b=1), iload_0, ifle done,
    // loop: iload_2, iload_1, iadd, istore_3, iload_2, istore_1, iload_3, istore_2,
    //       iinc 0 -1, iload_0, ifgt loop, done: iload_1, ireturn
    let bytecode: &[u8] = &[
        0x03, // 0: iconst_0
        0x3c, // 1: istore_1  (a = 0)
        0x04, // 2: iconst_1
        0x3d, // 3: istore_2  (b = 1)
        0x1a, // 4: iload_0   (n)
        0x9e, 0x00, 0x14, // 5: ifle +20 → 25
        // loop body at PC 8:
        0x1c, // 8: iload_2   (b)
        0x1b, // 9: iload_1   (a)
        0x60, // 10: iadd     (a+b)
        0x3e, // 11: istore_3 (tmp = a+b)
        0x1c, // 12: iload_2  (b)
        0x3c, // 13: istore_1 (a = b)
        0x1d, // 14: iload_3  (tmp)
        0x3d, // 15: istore_2 (b = tmp)
        0x84, 0x00, 0xff, // 16: iinc 0, -1  (n--)
        0x1a, // 19: iload_0  (n)
        0x9d, 0xff, 0xf3, // 20: ifgt -13 → 8
        // done at PC 23:
        0x1b, // 23: iload_1  (a)
        0xac, // 24: ireturn
    ];
    let result = make_backend_with_method(4, 1, bytecode);
    assert!(
        !result.success,
        "a method with a loop back-edge must bail: there is no safepoint \
         poll to place on the back-edge, so a thread in this loop would \
         never reach a stop-the-world request"
    );
    assert!(
        emit_machine_code(&result).is_none(),
        "the encoder must also refuse a bailed result"
    );

    // Negative control: the same shape of body with only a FORWARD branch
    // still compiles, so the bail is specific to the back-edge and is not a
    // blanket refusal of branching methods.
    let straight_line: &[u8] = &[
        0x03, // 0: iconst_0
        0x3c, // 1: istore_1
        0x04, // 2: iconst_1
        0x3d, // 3: istore_2
        0x1a, // 4: iload_0
        0x9e, 0x00, 0x0a, // 5: ifle +10 → pc 15 (forward)
        0x1c, // 8: iload_2
        0x1b, // 9: iload_1
        0x60, // 10: iadd
        0x3e, // 11: istore_3
        0x1b, // 12: iload_1
        0xac, // 13: ireturn
        0x00, // 14: nop
        0x1b, // 15: iload_1
        0xac, // 16: ireturn
    ];
    let ok = make_backend_with_method(4, 1, straight_line);
    assert!(
        ok.success,
        "the same body with only a forward branch must still compile"
    );
}

/// A single backward `goto` — the minimal back-edge — must bail, including
/// the degenerate `goto 0` self-loop.
#[test]
pub(in crate::aarch64_backend::tests) fn backward_goto_and_self_loop_both_bail() {
    // nop; goto -1 from pc 1 → target pc 0, the tightest possible loop.
    let self_loop = make_backend_with_method(0, 0, &[0x00, 0xa7, 0xff, 0xff]);
    assert!(!self_loop.success, "a one-instruction loop must bail");

    // nop; nop; goto -2 (back to pc 0)
    let back = make_backend_with_method(0, 0, &[0x00, 0x00, 0xa7, 0xff, 0xfe]);
    assert!(!back.success, "a backward goto must bail");

    // Forward goto over a return — still fine.
    let fwd = make_backend_with_method(0, 0, &[0xa7, 0x00, 0x04, 0xb1, 0xb1]);
    assert!(fwd.success, "a forward goto must still compile");
}

/// A backward `tableswitch` case target is a back-edge too, and switch
/// targets take a different code path (`label_for_pc` from the switch arm
/// rather than from an `if*` arm), so it gets its own guard.
#[test]
pub(in crate::aarch64_backend::tests) fn backward_switch_case_target_bails() {
    // pc 0: nop
    // pc 1: nop
    // pc 2: nop
    // pc 3: iconst_1
    // pc 4: tableswitch, padding to pc 8, default +12 (→ pc 16), low=0,
    //       high=0, case 0 offset = -4 (→ pc 0, a back-edge)
    let bytecode: &[u8] = &[
        0x00, 0x00, 0x00, // 0-2: nop nop nop
        0x04, // 3: iconst_1
        0xaa, // 4: tableswitch
        0x00, 0x00, 0x00, // 5-7: padding to 4-byte boundary
        0x00, 0x00, 0x00, 0x14, // 8: default → +20 → pc 24 (forward)
        0x00, 0x00, 0x00, 0x00, // 12: low = 0
        0x00, 0x00, 0x00, 0x00, // 16: high = 0
        0xff, 0xff, 0xff, 0xfc, // 20: case 0 → -4 → pc 0 (BACK-EDGE)
        0xb1, // 24: return
    ];
    let result = make_backend_with_method(0, 0, bytecode);
    assert!(
        !result.success,
        "a backward switch case target is a back-edge and must bail too"
    );
}

// ===================================================================
// Phase 95.3 — NEON vectorization tests
// ===================================================================

// NOTE: the `p95_neon_pattern_detection_*` tests were removed alongside the
// dead `detect_neon_patterns` / `NeonVectorizablePattern` scanner (2026-06-10
// JIT cleanup). That scanner was never wired into codegen and emitted
// placeholder local indices; see the removal note near the top of this file.

#[test]
pub(in crate::aarch64_backend::tests) fn p95_neon_machine_code_emission() {
    let mut backend = Arm64Backend::new();
    backend.buffer = Arm64CodeBuffer::new();
    backend.frame = Some(Arm64FrameLayout::compute(0, 16, &[]));
    backend.local_regs = Vec::new();
    backend.float_local_regs = Vec::new();

    // Emit a small NEON sequence
    backend.buffer.emit(Arm64Instruction::NeonLd1_4s {
        vt: Arm64Register::V0,
        rn: Arm64Register::X0,
    });
    backend.buffer.emit(Arm64Instruction::NeonAdd4s {
        vd: Arm64Register::V0,
        vn: Arm64Register::V0,
        vm: Arm64Register::V1,
    });
    backend.buffer.emit(Arm64Instruction::NeonMul4s {
        vd: Arm64Register::V2,
        vn: Arm64Register::V0,
        vm: Arm64Register::V1,
    });
    backend.buffer.emit(Arm64Instruction::NeonSt1_4s {
        vt: Arm64Register::V2,
        rn: Arm64Register::X1,
    });
    backend.buffer.emit(Arm64Instruction::Ret);

    let result = Arm64CompileResult {
        safepoint_count: 0,
        incomplete_oop_maps: 0,
        static_init_classes: Vec::new(),
        sp_id_slot_off: 0,
        instructions: backend.buffer.instructions().to_vec(),
        frame: Arm64FrameLayout::compute(0, 0, &[]),
        labels: backend.buffer.labels.clone(),
        success: true,
        pending_oop_maps: Vec::new(),
        needs_context: false,
        polls_enabled: false,
        frame_base_published: false,
    };

    let code = emit_machine_code(&result);
    assert!(
        code.is_some(),
        "NEON instructions should produce machine code"
    );
    let bytes = code.unwrap();
    assert_eq!(
        bytes.len(),
        5 * 4,
        "5 instructions × 4 bytes each = 20 bytes"
    );
    // All bytes should be non-zero (valid ARM64 encodings)
    assert!(
        bytes.iter().any(|&b| b != 0),
        "encoded bytes should be non-trivial"
    );
}

// -----------------------------------------------------------------------
// aarch64-coverage audit (2026-07-26): `emit_machine_code` soundness gates
// -----------------------------------------------------------------------

/// Build a minimal `Arm64CompileResult` around a caller-supplied
/// instruction sequence, with `success = true` so the only thing under
/// test is `emit_machine_code`'s own validation.
pub(in crate::aarch64_backend::tests) fn result_from_instructions(instructions: Vec<Arm64Instruction>) -> Arm64CompileResult {
    Arm64CompileResult {
        instructions,
        frame: Arm64FrameLayout::compute(0, 0, &[]),
        labels: HashMap::new(),
        success: true,
        pending_oop_maps: Vec::new(),
        sp_id_slot_off: 0,
        safepoint_count: 0,
        incomplete_oop_maps: 0,
        static_init_classes: Vec::new(),
        needs_context: false,
        polls_enabled: false,
        frame_base_published: false,
    }
}

#[test]
pub(in crate::aarch64_backend::tests) fn emit_machine_code_bails_on_unbound_branch_label() {
    // `B <label 7>` where label 7 is never bound. The old patch loop left
    // the displacement-0 placeholder in place — a branch to itself, i.e.
    // an infinite loop inside successfully-"compiled" code.
    let result = result_from_instructions(vec![
        Arm64Instruction::B { label: 7 },
        Arm64Instruction::Ret,
    ]);
    assert!(
        emit_machine_code(&result).is_none(),
        "an unbound branch label must bail the method, not emit `B .`"
    );
}

#[test]
pub(in crate::aarch64_backend::tests) fn emit_machine_code_bails_on_unbound_conditional_branch_label() {
    let result = result_from_instructions(vec![
        Arm64Instruction::Cbz {
            rt: Arm64Register::X0,
            label: 3,
        },
        Arm64Instruction::Ret,
    ]);
    assert!(
        emit_machine_code(&result).is_none(),
        "an unbound CBZ label must bail the method"
    );
}

#[test]
pub(in crate::aarch64_backend::tests) fn emit_machine_code_bails_on_unbound_ldr_literal_label() {
    // An unpatched LDR (literal) reads at `pc + 0` — the instruction
    // itself — rather than a constant-pool entry.
    let result = result_from_instructions(vec![
        Arm64Instruction::LdrLiteral {
            rt: Arm64Register::X0,
            label: 11,
        },
        Arm64Instruction::Ret,
    ]);
    assert!(
        emit_machine_code(&result).is_none(),
        "an unbound LDR-literal label must bail the method"
    );
}

#[test]
pub(in crate::aarch64_backend::tests) fn emit_machine_code_accepts_bound_labels() {
    // Companion to the three bail tests: a correctly bound forward branch
    // must still encode, so the new checks cannot be satisfied by a
    // blanket refusal.
    let result = result_from_instructions(vec![
        Arm64Instruction::B { label: 1 },
        Arm64Instruction::Nop,
        Arm64Instruction::Label(1),
        Arm64Instruction::Ret,
    ]);
    let bytes = emit_machine_code(&result).expect("bound labels must encode");
    // 3 real instructions (Label emits nothing) × 4 bytes.
    assert_eq!(bytes.len(), 3 * 4);
    // The `B` at offset 0 targets offset 8 → imm26 == 2.
    let b = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    assert_eq!(b >> 26, 0b000101, "opcode must be unconditional B");
    assert_eq!(b & 0x03ff_ffff, 2, "B displacement must be +2 instructions");
}

/// `B.cond`/`CBZ` carry a signed imm19 → ±1 MiB. A conditional branch
/// over more than 2^18 instructions cannot be encoded.
///
/// `Aarch64Emitter::mark_branch_overflow` does two things: trips a
/// `debug_assert!` (active in debug/test builds) AND sets the sticky
/// `overflowed` flag (always). `debug_assert!` is compiled out under
/// `--release`, so the assertion this test can make differs per profile —
/// same split as `aarch64::tests::assert_branch_overflow_detected`.
#[test]
pub(in crate::aarch64_backend::tests) fn emit_machine_code_bails_on_out_of_range_conditional_branch() {
    const SPAN: usize = (1 << 18) + 4; // > 2^18 instructions ⇒ > ±1 MiB

    let build = || {
        let mut insts = Vec::with_capacity(SPAN + 3);
        insts.push(Arm64Instruction::Cbz {
            rt: Arm64Register::X0,
            label: 1,
        });
        insts.resize(SPAN + 1, Arm64Instruction::Nop);
        insts.push(Arm64Instruction::Label(1));
        insts.push(Arm64Instruction::Ret);
        result_from_instructions(insts)
    };

    if cfg!(debug_assertions) {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            emit_machine_code(&build())
        }));
        std::panic::set_hook(prev);
        assert!(
            outcome.is_err(),
            "an out-of-range CBZ patch must trip the debug_assert! in debug builds"
        );
    } else {
        assert!(
            emit_machine_code(&build()).is_none(),
            "an out-of-range CBZ patch must set the sticky overflow flag and bail"
        );
    }
}

// -----------------------------------------------------------------------
// Division / remainder are refused while the throw path is UNWIRED (no
// `throw_arithmetic` helper, or no word that the exception table is
// empty) — see the module header and the `r9w11_*` tests for the wired
// lowering. `make_backend_with_method` wires neither.
// -----------------------------------------------------------------------

#[test]
pub(in crate::aarch64_backend::tests) fn backend_idiv_bails_to_interpreter() {
    // iload_0; iload_1; idiv; ireturn
    let result = make_backend_with_method(2, 2, &[0x1a, 0x1b, 0x6c, 0xac]);
    assert!(
        !result.success,
        "idiv must bail with no ArithmeticException path wired (the pre-round-7 \
         guard was BRK #1, i.e. SIGTRAP)"
    );
    assert!(emit_machine_code(&result).is_none());
}

#[test]
pub(in crate::aarch64_backend::tests) fn backend_ldiv_bails_to_interpreter() {
    // lload_0; lload_1; ldiv; lreturn
    let result = make_backend_with_method(2, 2, &[0x1e, 0x1f, 0x6d, 0xad]);
    assert!(
        !result.success,
        "ldiv must bail for the same reason as idiv"
    );
}

#[test]
pub(in crate::aarch64_backend::tests) fn backend_irem_and_lrem_bail_to_interpreter() {
    // AArch64 SDIV by zero yields 0 (it does not trap), so the old
    // `a - (a/b)*b` lowering returned `a` for `a % 0` instead of throwing.
    for &(op, name) in &[(0x70u8, "irem"), (0x71u8, "lrem")] {
        let result = make_backend_with_method(2, 2, &[0x1a, 0x1b, op, 0xac]);
        assert!(
            !result.success,
            "{name} must bail: SDIV-by-zero silently yields 0 on AArch64"
        );
    }
}

/// The refusal must be specific to div/rem — the surrounding integer
/// arithmetic still compiles.
#[test]
pub(in crate::aarch64_backend::tests) fn backend_imul_still_compiles_after_div_refusal() {
    // iload_0; iload_1; imul; ireturn
    let result = make_backend_with_method(2, 2, &[0x1a, 0x1b, 0x68, 0xac]);
    assert!(
        result.success,
        "imul must be unaffected by the div/rem bail"
    );
    assert!(emit_machine_code(&result).is_some());
}

/// Coverage guard for the module header's census: every opcode of the
/// object model must refuse the method WHEN NOTHING IS WIRED FOR IT.
///
/// What this asserts has narrowed as the backend grew, and it is worth
/// being precise about what is left. Most of these opcodes now HAVE a
/// lowering; `make_backend_with_method` wires no helper, no site table and
/// no throw path, so each refuses in its own arm. The test still earns its
/// place: a lowering that quietly stopped requiring what it needs -- a
/// resolved site, an exact exception path, an empty exception table --
/// would compile here, and this is what says so. The header's table of
/// CONDITIONAL rows is the other half of the same rule.
#[test]
pub(in crate::aarch64_backend::tests) fn object_model_opcodes_are_all_unsupported() {
    // One representative per documented unsupported area, plus every
    // field/dispatch/allocation opcode (the ones whose absence defines
    // this backend's scope).
    let unsupported: &[u8] = &[
        0x2e, 0x32, 0x35, // iaload / aaload / saload
        0x4f, 0x53, 0x56, // iastore / aastore / sastore
        0xb2, 0xb3, 0xb4, 0xb5, // getstatic / putstatic / getfield / putfield
        0xb6, 0xb7, 0xb9, 0xba, // invokevirtual/special/interface/dynamic
        0xbb, 0xbc, 0xbd, 0xbe, // new / newarray / anewarray / arraylength
        0xbf, // athrow
        0xc0, 0xc1, // checkcast / instanceof
        // `monitorenter`/`monitorexit` (0xc2/0xc3) left this list at h23:
        // they have a lowering now, and it refuses only for an unwired
        // helper -- which is the `every_shared_memory_opcode_...` test's
        // business, not this one's.
        // `multianewarray` (0xc5) left this list at h23c: it has a
        // lowering now, and refuses only for an unresolved site or an
        // unwired helper -- which is the
        // `every_shared_memory_opcode_...` test's business.
        0xc4, 0xc8, // wide / goto_w
    ];
    for &op in unsupported {
        // Two operand bytes cover the widest of these; trailing bytes are
        // irrelevant because the arm bails before consuming them.
        let result = make_backend_with_method(2, 2, &[op, 0x00, 0x01, 0xb1]);
        assert!(
            !result.success,
            "opcode 0x{op:02x} unexpectedly compiled — update the coverage \
             table in this module's header before landing that"
        );
    }
}

/// Every opcode the shared-memory predicate names must be refused **by the
/// gate**, not by the `_` arm -- unless it is in
/// [`opcode_has_ordered_lowering`], in which case its OWN arm must refuse
/// it while nothing is wired for it, and must say what is missing.
///
/// The distinction is the whole point. Before the gate, these opcodes were
/// refused because nobody had written a lowering for them -- an emergent
/// property, invisible to the next person to write one. The assertion on
/// the emitted comment is what makes it a rule: adding a `getfield` arm
/// without the ordering instructions leaves the method refused and this
/// test passing; removing the gate fails it.
///
/// The `expected` table is the second half of the rule, and the reason
/// this test grew rather than being deleted when the blanket
/// `ARM64_CAN_ORDER_MEMORY` went away (round 9 wave 14): an opcode cannot
/// join `opcode_has_ordered_lowering` without someone stating, here, what
/// its unwired refusal says.
#[test]
pub(in crate::aarch64_backend::tests) fn every_shared_memory_opcode_without_an_ordered_lowering_is_refused() {
    for op in 0u8..=0xff {
        if !opcode_touches_shared_memory(op) {
            continue;
        }
        // Three operand bytes cover the widest of these (multianewarray);
        // trailing bytes are irrelevant because the gate bails before
        // consuming any of them.
        let result = make_backend_with_method(2, 2, &[op, 0x00, 0x01, 0x01, 0xb1]);
        assert!(
            !result.success,
            "opcode 0x{op:02x} touches shared memory and must not compile \
             with nothing wired"
        );
        if opcode_has_ordered_lowering(op) {
            let expected = match op {
                0xb2 | 0xb3 => "no resolved static field",
                0xbb..=0xbd => "no resolved class, an unwired helper",
                0xbf => "an unwired helper, or a non-empty exception table",
                0xb4 | 0xb5 => "no resolved instance field",
                // Round 9 wave 22. `invokedynamic` shares the arm and the
                // comment, though its refusal is unconditional: the
                // dispatch helper has no `invoke_kind` for a call site
                // that is not a class.
                0xb6..=0xba => "unresolved site, unwired dispatch helper",
                0xbe => "no exact NullPointerException path",
                // Round 9 wave 23.
                0xc0 | 0xc1 => "unresolved target class, an unwired helper",
                // h23, 2026-09-22.
                0xc2 | 0xc3 => "an unwired helper, or no route for a pending exception",
                // h23c, 2026-09-22.
                0xc5 => "no resolved site, an unwired helper, an unsupported arity",
                0x2e..=0x35 | 0x4f..=0x56 => "no exact array exception path",
                _ => panic!(
                    "opcode 0x{op:02x} gained an ordered lowering without telling this \
                     test what its unwired refusal says"
                ),
            };
            let refused_by_arm = result.instructions.iter().any(
                |inst| matches!(inst, Arm64Instruction::Comment(c) if c.contains(expected)),
            );
            assert!(
                refused_by_arm,
                "opcode 0x{op:02x} has an ordered lowering and must be refused by its \
                 own arm when nothing is wired for it"
            );
            continue;
        }
        let gated = result.instructions.iter().any(|inst| {
            matches!(inst, Arm64Instruction::Comment(c) if c.contains("touches shared memory"))
        });
        assert!(
            gated,
            "opcode 0x{op:02x} was refused, but by the fallthrough arm rather than by \
             the shared-memory gate — the refusal must be the rule, not an accident"
        );
    }
}

/// h23 (2026-09-22): `monitorenter`/`monitorexit` lower, and a whole
/// `synchronized` body compiles -- the item that stood on five pages.
#[test]
pub(in crate::aarch64_backend::tests) fn h23_monitors_lower_through_the_helpers() {
    const MON_ENTER: usize = 0x7FFF_0000_C200;
    const MON_EXIT: usize = 0x7FFF_0000_C300;
    const SET_THROW_BCI: usize = 0x7FFF_0000_9100;

    fn compile_monitors(
        code: &[u8],
        enter: usize,
        exit: usize,
        table_empty: bool,
    ) -> Arm64CompileResult {
        let mut b = Arm64Backend::new();
        b.set_safepoints_enabled(false);
        // SAFETY: plain struct of `usize` addresses; all-zero is "nothing wired".
        let mut h: crate::JitRuntimeHelpers = unsafe { std::mem::zeroed() };
        h.monitor_enter = enter;
        h.monitor_exit = exit;
        h.set_throw_bci = SET_THROW_BCI;
        b.set_helpers(h);
        b.set_method_descriptor("(Ljava/lang/Object;)V", true);
        b.set_exception_table_empty(table_empty);
        b.compile_method(2, 1, 4, code)
    }

    // aload_0; monitorenter; aload_0; monitorexit; return -- the shape
    // javac's `synchronized (o) {}` compiles to, minus the handler (which
    // is exception-table data, not bytecode).
    let code = [0x2a, 0xc2, 0x2a, 0xc3, 0xb1];

    // A NON-EMPTY table, because every real `synchronized` block has one.
    let r = compile_monitors(&code, MON_ENTER, MON_EXIT, false);
    assert!(
        r.success,
        "a synchronized body must compile with a non-empty exception table"
    );
    for (addr, what) in [(MON_ENTER, "monitor_enter"), (MON_EXIT, "monitor_exit")] {
        let at = r
            .instructions
            .iter()
            .position(
                |i| matches!(i, Arm64Instruction::MovImm { imm, .. } if *imm == addr as i64),
            )
            .unwrap_or_else(|| panic!("{what} must be materialized"));
        assert!(
            matches!(
                r.instructions.get(at + 1),
                Some(Arm64Instruction::Blr { .. })
            ),
            "{what} must be CALLED, not merely loaded"
        );
    }
    // Both sentinel edges stamp their own bci: pc 1 and pc 3.
    for bci in [1i64, 3] {
        let stamps = r
            .instructions
            .windows(3)
            .filter(|w| {
                matches!(w[0], Arm64Instruction::MovImm { imm, .. } if imm == bci)
                    && matches!(w[1], Arm64Instruction::MovImm { imm, .. } if imm == SET_THROW_BCI as i64)
                    && matches!(w[2], Arm64Instruction::Blr { .. })
            })
            .count();
        assert_eq!(stamps, 1, "the monitor op at bci {bci} stamps its own bci");
    }
    // The frame carries the context word: both helpers take the VM
    // pointer, and without it `context_slot_offset` refuses.
    assert!(
        r.needs_context,
        "a monitor op needs the VM context in the frame"
    );

    // Each helper is required on its own: an unwired one refuses, by its
    // own arm, and says so.
    for (enter, exit) in [(0, MON_EXIT), (MON_ENTER, 0)] {
        let r = compile_monitors(&code, enter, exit, false);
        assert!(!r.success, "an unwired monitor helper must refuse");
        assert!(
            r.instructions.iter().any(|i| matches!(
                i,
                Arm64Instruction::Comment(c) if c.contains("monitor at pc=")
            )),
            "the refusal must name the monitor arm"
        );
    }

    // With an EMPTY table no stamp is emitted, so such a body stays
    // byte-identical to what it would have been.
    let empty = compile_monitors(&code, MON_ENTER, MON_EXIT, true);
    assert!(empty.success);
    assert!(
        !empty.instructions.iter().any(|i| matches!(
            i,
            Arm64Instruction::MovImm { imm, .. } if *imm == SET_THROW_BCI as i64
        )),
        "an empty table emits no stamp"
    );
}

/// h23c (2026-09-22): `multianewarray` lowers at ANY arity, through
/// `multianewarray_n`, with the operand area as its dimension buffer.
#[test]
pub(in crate::aarch64_backend::tests) fn h23c_multianewarray_lowers_at_any_arity() {
    const MNA_N: usize = 0x7FFF_0000_C500;
    const SET_THROW_BCI: usize = 0x7FFF_0000_9100;

    fn compile_mna(ndims: u8, site: Option<i64>) -> Arm64CompileResult {
        // iconst_1 x ndims; multianewarray #7, ndims; areturn
        let mut code: Vec<u8> = vec![0x04; usize::from(ndims)];
        code.extend_from_slice(&[0xc5, 0x00, 0x07, ndims, 0xb0]);
        let mna_pc = usize::from(ndims);
        let mut b = Arm64Backend::new();
        b.set_safepoints_enabled(false);
        // SAFETY: plain struct of `usize` addresses; all-zero is "nothing wired".
        let mut h: crate::JitRuntimeHelpers = unsafe { std::mem::zeroed() };
        h.multianewarray_n = MNA_N;
        h.set_throw_bci = SET_THROW_BCI;
        b.set_helpers(h);
        b.set_method_descriptor("()[[I", true);
        // NON-EMPTY: a real allocation site sits inside try/catch often
        // enough that this is the case worth pinning.
        b.set_exception_table_empty(false);
        if let Some(site) = site {
            b.set_multianewarray_site_info([(mna_pc, site)].into_iter().collect());
        }
        b.compile_method(0, 0, usize::from(ndims) + 2, &code)
    }

    // Every arity the scanner admits, not just 2 and 3.
    for ndims in 1..=crate::x64::MAX_JIT_MULTIANEWARRAY_DIMS {
        // Cast: MAX_JIT_MULTIANEWARRAY_DIMS is single-digit.
        let r = compile_mna(ndims as u8, Some(0x1234_5678));
        assert!(r.success, "ndims={ndims} must compile");
        let at = r
            .instructions
            .iter()
            .position(|i| {
                matches!(
                    i,
                    Arm64Instruction::MovImm { imm, .. } if *imm == MNA_N as i64
                )
            })
            .unwrap_or_else(|| panic!("ndims={ndims}: the helper must be materialized"));
        assert!(
            matches!(
                r.instructions.get(at + 1),
                Some(Arm64Instruction::Blr { .. })
            ),
            "ndims={ndims}: the helper must be CALLED"
        );
        // The arity travels as an immediate, so a site that lowered the
        // wrong count would be visible here rather than at run time.
        assert!(
            r.instructions.iter().any(|i| matches!(
                i,
                Arm64Instruction::MovImm { imm, .. } if *imm == ndims as i64
            )),
            "ndims={ndims}: the arity must be passed"
        );
        // The dimension buffer is an address off FP -- one SUB, not a
        // copy loop.
        assert!(
            r.instructions.iter().any(|i| matches!(
                i,
                Arm64Instruction::SubImm {
                    rn: Arm64Register::FP,
                    ..
                }
            )),
            "ndims={ndims}: the buffer address comes off FP"
        );
        // The frame carries the context word and a safepoint id, because
        // the helper takes the VM pointer and can collect.
        assert!(r.needs_context, "ndims={ndims}: needs the VM context");
        assert_ne!(r.sp_id_slot_off, 0, "ndims={ndims}: needs a safepoint id");
        // The 0-sentinel edge stamps its own bci.
        let bci = ndims as i64;
        assert!(
            r.instructions.windows(3).any(|w| {
                matches!(w[0], Arm64Instruction::MovImm { imm, .. } if imm == bci)
                    && matches!(w[1], Arm64Instruction::MovImm { imm, .. } if imm == SET_THROW_BCI as i64)
                    && matches!(w[2], Arm64Instruction::Blr { .. })
            }),
            "ndims={ndims}: the sentinel edge stamps the site's own bci"
        );
    }

    // An unresolved site refuses, by this arm, with a named reason.
    let r = compile_mna(3, None);
    assert!(!r.success, "an unresolved site must refuse");
    assert!(
        r.instructions.iter().any(|i| matches!(
            i,
            Arm64Instruction::Comment(c) if c.contains("multianewarray at pc=")
        )),
        "the refusal must name the multianewarray arm"
    );

    // Wider than the scanner admits: refused rather than mis-consumed.
    let too_wide = compile_mna(
        // Cast: single-digit + 1.
        (crate::x64::MAX_JIT_MULTIANEWARRAY_DIMS + 1) as u8,
        Some(0x1234_5678),
    );
    assert!(!too_wide.success, "an arity past the cap must refuse");
}

/// **The gate is INERT as of h23c (2026-09-22)**, and this records the day
/// and what would wake it.
///
/// This test used to be `the_shared_memory_gate_still_has_something_to_refuse`
/// and asserted the opposite: that some opcode in
/// [`opcode_touches_shared_memory`] still lacked an ordered lowering. Its
/// doc said "the day every one of them does, the gate can never fire and
/// should be deleted rather than left as a dead branch a reader has to
/// reason about. This test fails on that day, which is the only way anyone
/// would notice." `multianewarray` was the last one, and it fired.
///
/// # Why the gate was NOT deleted, against that note
///
/// A deliberate deviation, recorded so the next person can overrule it.
/// The two sets coinciding is a property of TODAY, not a theorem: the
/// gate's own doc calls [`opcode_touches_shared_memory`] "an ALLOW-LIST OF
/// THE HAZARD", so the state it defends against -- an opcode classified as
/// touching shared memory before anyone has written its ordered lowering
/// -- is exactly what a future wave produces on its way in. Deleting the
/// branch buys one less thing to read and sells the interlock that makes
/// the refusal a RULE rather than an accident, which is the distinction
/// the sibling test
/// (`every_shared_memory_opcode_without_an_ordered_lowering_is_refused`)
/// exists to preserve. It costs one predicate call per opcode.
///
/// So the gate stays and the assertion is inverted: the inert state is now
/// pinned, and any opcode that joins the hazard list without a lowering
/// fails HERE, naming itself, instead of silently re-arming a branch
/// everyone had stopped expecting to fire.
#[test]
pub(in crate::aarch64_backend::tests) fn the_shared_memory_gate_is_inert_and_every_hazard_opcode_is_lowered() {
    let ungated: Vec<u8> = (0u8..=0xff)
        .filter(|&op| opcode_touches_shared_memory(op) && !opcode_has_ordered_lowering(op))
        .collect();
    assert!(
        ungated.is_empty(),
        "opcode(s) {ungated:02x?} are classified as touching shared memory but have \
         no ordered lowering, so the `compile_pass` gate can fire again. That is not \
         a failure in itself -- it is the state the gate exists for -- but it must be \
         a DELIBERATE one: give each of them an arm that refuses by name with its \
         ordering stated, add it to `opcode_has_ordered_lowering`, and this goes green."
    );
}

/// The thread-local opcodes this backend exists to compile must NOT be
/// caught by the gate. A predicate that refused everything would pass the
/// test above and compile nothing.
#[test]
pub(in crate::aarch64_backend::tests) fn the_shared_memory_gate_does_not_catch_thread_local_work() {
    // Constants, local load/store, arithmetic, conversions, compares,
    // branches, the stack shuffles, `iinc`, the switches and the returns.
    for op in [
        0x00u8, 0x01, 0x03, 0x10, 0x11, 0x15, 0x1a, 0x36, 0x3b, 0x57, 0x59, 0x5f, 0x60, 0x64,
        0x68, 0x78, 0x7a, 0x7e, 0x84, 0x85, 0x91, 0x94, 0x99, 0x9f, 0xa7, 0xaa, 0xab, 0xac,
        0xb1,
    ] {
        assert!(
            !opcode_touches_shared_memory(op),
            "opcode 0x{op:02x} is thread-local work; gating it would compile nothing"
        );
    }
}

/// The two ordering constants are claims about what this file's PRODUCTION
/// code emits, so check them against the file.
///
/// Round 9 wave 9 added the ENCODERS (`ldar`/`stlr` and friends,
/// `ldaxr`/`stlxr`, `casal`, pinned by
/// `aarch64::tests::test_acquire_release_and_exclusive_encodings`). An
/// encoder is not a lowering: what these constants claim is that something
/// above this test module CALLS them.
///
/// Until round 9 wave 14 there was one constant for both halves,
/// `ARM64_CAN_ORDER_MEMORY`, and the gate read it as a blanket permission.
/// Splitting it is what let the acquire/release half tell the truth (it
/// has had call sites since wave 10) without waiting on an exclusive half
/// that cannot arrive -- see [`ARM64_LOWERS_ACQUIRE_RELEASE`].
#[test]
pub(in crate::aarch64_backend::tests) fn the_ordering_constants_agree_with_the_code() {
    let src = include_str!("../../../../jit-aarch64/src/lib.rs");
    let has = |name: &str| src.contains(&format!("pub fn {name}("));
    // A `volatile` field access needs acquire-load and release-store.
    let acquire_release = has("ldar") && has("stlr");
    // `monitorenter` needs a compare-and-swap on the lock word: either the
    // load/store-exclusive pair or the LSE single instruction.
    let exclusive = (has("ldaxr") && has("stlxr")) || has("casal");
    assert!(
        acquire_release && exclusive,
        "the acquire/release ({acquire_release}) and exclusive-access \
         ({exclusive}) encoders were added in round 9 wave 9 and must not \
         silently disappear"
    );

    // The production half of this file: everything before its test module.
    let backend = concat!(include_str!("../../aarch64_backend/mod.rs"), "\n", include_str!("../../aarch64_backend/arm64_register.rs"), "\n", include_str!("../../aarch64_backend/mark_top_operand_as_oop.rs"), "\n", include_str!("../../aarch64_backend/to_fpreg.rs"));
    let decl = format!("\nmod {}", "tests {");
    let production = backend.find(&decl).map_or(backend, |at| &backend[..at]);
    let calls = |name: &str| production.contains(&format!("emitter.{name}("));
    assert_eq!(
        acquire_release && calls("ldar") && calls("stlr"),
        ARM64_LOWERS_ACQUIRE_RELEASE,
        "ARM64_LOWERS_ACQUIRE_RELEASE disagrees with the code: encoders present = \
         {acquire_release}, production calls ldar = {}, stlr = {}",
        calls("ldar"),
        calls("stlr")
    );
    assert_eq!(
        exclusive && ((calls("ldaxr") && calls("stlxr")) || calls("casal")),
        ARM64_LOWERS_EXCLUSIVE_ACCESS,
        "ARM64_LOWERS_EXCLUSIVE_ACCESS disagrees with the code: encoders present = \
         {exclusive}, production calls ldaxr = {}, stlxr = {}, casal = {}",
        calls("ldaxr"),
        calls("stlxr"),
        calls("casal")
    );
}

/// The gate has no blanket escape hatch any more, and this is what keeps
/// it that way: `compile_pass`'s gate must read
/// `opcode_has_ordered_lowering` and NOTHING else.
///
/// A `bool` in front of the per-opcode list is exactly what a later change
/// flips to unblock itself, and flipping it un-gates every heap opcode at
/// once. The list cannot be flipped -- each entry has to be written, and
/// `every_shared_memory_opcode_without_an_ordered_lowering_is_refused`
/// makes the author state the arm's own refusal when they add one.
#[test]
pub(in crate::aarch64_backend::tests) fn the_gate_consults_only_the_per_opcode_list() {
    let backend = concat!(include_str!("../../aarch64_backend/mod.rs"), "\n", include_str!("../../aarch64_backend/arm64_register.rs"), "\n", include_str!("../../aarch64_backend/mark_top_operand_as_oop.rs"), "\n", include_str!("../../aarch64_backend/to_fpreg.rs"));
    let decl = format!("\nmod {}", "tests {");
    let production = backend.find(&decl).map_or(backend, |at| &backend[..at]);
    assert!(
        production.contains(
            "if opcode_touches_shared_memory(opcode) && !opcode_has_ordered_lowering(opcode) {"
        ),
        "the shared-memory gate is not the exact two-term condition this test pins; \
         if it grew a third term, that term is a blanket permission and the reason \
         `ARM64_CAN_ORDER_MEMORY` was removed applies to it"
    );
}

/// `invokestatic` has a match arm but `emit_invoke` always fails, so no
/// method containing a call of any kind compiles on this backend.
#[test]
pub(in crate::aarch64_backend::tests) fn invokestatic_arm_exists_but_always_bails() {
    let result = make_backend_with_method(1, 1, &[0xb8, 0x00, 0x01, 0xb1]);
    assert!(
        !result.success,
        "there is no call-target resolution on this backend; invokestatic must bail"
    );
}

/// A loop back-edge must resolve to a real target, not the `B .`
/// self-branch the lazy label discovery used to leave behind.
///
/// ```text
/// 0: iconst_0        0x03        // sum = 0
/// 1: istore_1        0x3c
/// 2: iload_1         0x1b   <-- back-edge target
/// 3: iconst_1        0x04
/// 4: iadd            0x60
/// 5: istore_1        0x3c
/// 6: goto -4         0xa7 ff fc  // back to pc 2
/// ```
///
/// (The `goto` is unconditional, so the loop never exits.)
///
/// **This method no longer compiles at all** — see
/// `loop_method_bails_no_safepoint_poll` and [`Arm64Backend::label_for_pc`]:
/// a back-edge is refused because there is no safepoint poll to place on it.
/// The original assertion ("a pure-arithmetic loop must compile") is
/// therefore inverted here.
///
/// The *encoder-level* guard the original test existed for — that a
/// backward branch resolves to a real target rather than being left as the
/// displacement-0 `B .` placeholder — is preserved below by driving
/// `emit_machine_code` with a hand-built pseudo-op sequence containing a
/// bound backward branch. That is a strictly stronger check on the piece
/// that can still regress (the patch loop), and it survives the front-end
/// policy change.
#[test]
pub(in crate::aarch64_backend::tests) fn backward_branch_resolves_to_its_target_not_itself() {
    // Front end: the loop is now refused outright.
    let code = [0x03, 0x3c, 0x1b, 0x04, 0x60, 0x3c, 0xa7, 0xff, 0xfc];
    let result = make_backend_with_method(2, 1, &code);
    assert!(
        !result.success,
        "a loop must bail — no safepoint poll exists for its back-edge"
    );

    // Encoder: a bound backward branch must still patch to a real negative
    // displacement, never to `B .`.
    let looped = result_from_instructions(vec![
        Arm64Instruction::Label(1),
        Arm64Instruction::Nop,
        Arm64Instruction::Nop,
        Arm64Instruction::B { label: 1 },
    ]);
    let bytes = emit_machine_code(&looped).expect("bound back-edge must encode");
    assert_eq!(bytes.len(), 3 * 4, "Label emits nothing; 3 real words");

    let mut last_b: Option<(usize, u32)> = None;
    for (i, w) in bytes.chunks_exact(4).enumerate() {
        let word = u32::from_le_bytes([w[0], w[1], w[2], w[3]]);
        if word >> 26 == 0b000101 {
            last_b = Some((i, word));
        }
    }
    let (b_index, b_word) = last_b.expect("an unconditional B must be present");
    let imm26 = b_word & 0x03ff_ffff;
    assert_ne!(
        imm26, 0,
        "displacement 0 is `B .` — an infinite self-branch, the bug this guards"
    );
    // Sign-extend imm26 and confirm it points backwards, at a real
    // instruction inside the buffer.
    let signed = ((imm26 << 6) as i32) >> 6;
    assert_eq!(
        signed, -2,
        "B at word 2 targeting word 0 is a -2 displacement"
    );
    let target = (b_index as i64 + signed as i64) * 4;
    assert_eq!(target, 0, "back-edge target must be the bound label");
    assert!(
        (target as usize) < bytes.len(),
        "back-edge target must land inside the emitted buffer"
    );
}

/// A compiled method still carries NO oop maps -- and the reason is no
/// longer the writer.
///
/// `emit_oop_map_for_safepoint` is correct now (see
/// `the_oop_map_pc_is_the_encoders_byte_offset`), but it still has no
/// production call site, because this backend lowers no allocation, no call
/// and no monitor and refuses back edges -- so a compiled method contains no
/// GC-capable point to record a map AT. If this fires, a safepoint was
/// added: that is the good outcome, and the module header's
/// "Safety-critical gaps" section needs updating with it.
#[test]
pub(in crate::aarch64_backend::tests) fn compiled_methods_carry_no_oop_maps() {
    // aload_0; areturn — the reference path that *does* call
    // `mark_top_operand_as_oop`.
    let result = make_backend_with_method(1, 1, &[0x2a, 0xb0]);
    assert!(result.success);
    assert!(
        result.pending_oop_maps.is_empty(),
        "no safepoint is emitted on this backend, so nothing calls the map \
         writer; if this fires, a safepoint landed -- update the header"
    );
    // ...and the resolved side agrees, which is the half a GC would read.
    let (_code, maps) = emit_machine_code_with_oop_maps(&result).expect("the method encodes");
    assert!(maps.is_empty());
}

// -----------------------------------------------------------------------
// aarch64 parity audit (2026-08-01) — fail-closed gates
// -----------------------------------------------------------------------

// -----------------------------------------------------------------------
// Safepoint polls (opt-in, `CRATONVM_JIT_ARM64_SAFEPOINTS`)
// -----------------------------------------------------------------------

/// Build a backend with polls on and plausible helper addresses.
pub(in crate::aarch64_backend::tests) fn poll_backend() -> Arm64Backend {
    let mut b = Arm64Backend::new();
    b.set_safepoints_enabled(true);
    // SAFETY: plain struct of `usize` addresses.
    let mut h: crate::JitRuntimeHelpers = unsafe { std::mem::zeroed() };
    h.safepoint_flag_addr = 0x1234_5678_9AB0;
    h.safepoint_slow_path = 0x7FFF_0000_1000;
    // The frame-base publisher, wired because a real VM wires it: precise
    // maps are default-ON, so `build_helpers` fills this slot on every
    // ordinary start. Round 9 wave 21 made it a term of
    // `fully_oop_covered`, so a poll fixture without it would be a fixture
    // that can no longer make the claim it exists to test.
    h.frame_record = 0x7FFF_0000_2000;
    b.set_helpers(h);
    b
}

/// THE WIDTH OF THE FLAG READ, asserted as an exact instruction word.
///
/// The safepoint flag is a Rust `AtomicBool` -- ONE byte -- and the
/// `GcBarrier` fields that follow it (`gc_generation: AtomicU64`, ...) are
/// not zero. Reading it with the 64-bit `ldr_imm` would fold those bytes
/// into the `CBZ` and make the poll fire on every iteration once the
/// generation counter is nonzero. A host that cannot execute aarch64 has to
/// catch that by ENCODING, so this pins the literal word rather than
/// merely asserting "an Ldrb was emitted".
#[test]
pub(in crate::aarch64_backend::tests) fn the_poll_reads_one_byte_and_the_encoding_says_so() {
    use crate::aarch64::{Aarch64Emitter, Reg};
    let mut e = Aarch64Emitter::new();
    assert!(e.ldrb_imm(Reg::X17, Reg::X16, 0));
    let word = u32::from_le_bytes(e.code()[0..4].try_into().unwrap());
    // LDRB Wt, [Xn, #0] = 0x39400000 | (Rn << 5) | Rt
    assert_eq!(
        word, 0x3940_0211,
        "LDRB W17, [X16] must encode as 0x39400211; got {word:#010x}"
    );
    // The 64-bit form is a DIFFERENT instruction -- the control that makes
    // the assertion above mean something.
    let mut e64 = Aarch64Emitter::new();
    assert!(e64.ldr_imm(Reg::X17, Reg::X16, 0));
    let w64 = u32::from_le_bytes(e64.code()[0..4].try_into().unwrap());
    assert_ne!(word, w64, "byte and doubleword loads must differ");
}

/// The entry poll's shape, read off the pseudo-op stream.
#[test]
pub(in crate::aarch64_backend::tests) fn the_entry_poll_has_the_expected_shape() {
    let mut b = poll_backend();
    // `return void` -- no operand stack, so the poll spills nothing.
    let result = b.compile_method(0, 0, 4, &[0xb1]);
    assert!(result.success, "the method must still compile");
    let ops = &result.instructions;
    let ldrb = ops
        .iter()
        .position(|i| matches!(i, Arm64Instruction::Ldrb { .. }))
        .expect("the poll must read the flag with a BYTE load");
    assert!(
        matches!(ops[ldrb - 1], Arm64Instruction::MovImm { .. }),
        "the flag address must be materialized right before the load"
    );
    assert!(
        matches!(ops[ldrb + 1], Arm64Instruction::Cbz { .. }),
        "a clear flag must branch PAST the call, not into it"
    );
    assert!(
        ops[ldrb..]
            .iter()
            .any(|i| matches!(i, Arm64Instruction::Blr { .. })),
        "the poll must call the slow path"
    );
    // The CBZ target must be bound AFTER the BLR, or the poll skips
    // nothing -- or worse, branches backwards.
    let Arm64Instruction::Cbz { label, .. } = ops[ldrb + 1] else {
        unreachable!()
    };
    let blr = ops
        .iter()
        .position(|i| matches!(i, Arm64Instruction::Blr { .. }))
        .unwrap();
    let bound = ops
        .iter()
        .position(|i| matches!(i, Arm64Instruction::Label(l) if *l == label))
        .expect("the skip label must be bound");
    assert!(
        bound > blr,
        "the skip target must be past the call ({bound} vs {blr})"
    );
}

/// OFF is byte-identical to before, and still refuses loops.
///
/// The negative control for every assertion above: without it they would
/// pass just as well if the poll were emitted unconditionally.
#[test]
pub(in crate::aarch64_backend::tests) fn safepoints_off_emits_no_poll_and_still_refuses_loops() {
    let mut off = Arm64Backend::new();
    off.set_safepoints_enabled(false);
    let a = off.compile_method(0, 0, 4, &[0xb1]);
    assert!(a.success);
    assert!(
        !a.instructions
            .iter()
            .any(|i| matches!(i, Arm64Instruction::Ldrb { .. })),
        "no poll may be emitted with the switch off"
    );
    // The SAME loop `a_loop_compiles_with_a_poll_at_its_header` compiles
    // with the switch on, so this is a true A/B on one input: with polls
    // off the pre-existing safety gate still refuses it, because a compiled
    // loop containing no poll is a region a stop-the-world request can
    // never interrupt.
    //
    // (An earlier draft of this used a bare `goto -3` at pc 0. That target
    // is NEGATIVE, wraps when cast to `usize`, and so never looked like a
    // back edge at all -- the test passed for the wrong reason until the
    // loop above was written to compare against.)
    let code = [0x03, 0x3b, 0x84, 0x00, 0x01, 0xa7, 0xFF, 0xFD];
    let mut off2 = Arm64Backend::new();
    off2.set_safepoints_enabled(false);
    let b = off2.compile_method(1, 0, 4, &code);
    assert!(
        !b.success,
        "with polls off, a backward branch must still refuse the method"
    );
}

/// A helper table with no flag address emits nothing, switch or no switch.
///
/// The same optional-helper contract x64 has: an unwired build must not
/// call through a null pointer, and must be byte-identical to before.
#[test]
pub(in crate::aarch64_backend::tests) fn an_unwired_helper_table_emits_no_poll() {
    let mut b = Arm64Backend::new();
    b.set_safepoints_enabled(true); // switch ON, helpers absent
    let result = b.compile_method(0, 0, 4, &[0xb1]);
    assert!(result.success);
    assert!(
        !result
            .instructions
            .iter()
            .any(|i| matches!(i, Arm64Instruction::Ldrb { .. })),
        "no flag address means no poll code at all"
    );
}

/// With polls on, a LOOP compiles -- and gets a poll at its header.
///
/// This is the capability the refusal was trading away: `label_for_pc`
/// refused every backward branch precisely because a compiled loop with no
/// poll in it is a region a stop-the-world request can never interrupt.
#[test]
pub(in crate::aarch64_backend::tests) fn a_loop_compiles_with_a_poll_at_its_header() {
    // 0: iconst_0   1: istore_0   2: iinc 0,1   5: goto 2
    let code = [0x03, 0x3b, 0x84, 0x00, 0x01, 0xa7, 0xFF, 0xFD];
    let mut b = poll_backend();
    let result = b.compile_method(1, 0, 4, &code);
    assert!(
        result.success,
        "a loop must compile once its header can carry a poll"
    );
    let polls = result
        .instructions
        .iter()
        .filter(|i| matches!(i, Arm64Instruction::Ldrb { .. }))
        .count();
    assert_eq!(polls, 2, "expected an entry poll and a loop-header poll");
    // And it encodes: the backward branch patches to a NEGATIVE
    // displacement, which nothing on this backend had exercised before.
    assert!(
        emit_machine_code(&result).is_some(),
        "the loop must survive encoding, back edge and all"
    );
}

/// The poll leaves the compile-time operand model exactly as it found it.
///
/// Its stores and reloads sit INSIDE the `CBZ`-skipped block, so the model
/// must say the same thing on both paths: every operand in the register it
/// was in. Asserting the model is unchanged is how a non-executing host
/// checks that.
#[test]
pub(in crate::aarch64_backend::tests) fn the_poll_restores_the_operand_model() {
    let mut b = poll_backend();
    b.install_frame(
        0,
        Arm64SpillArea {
            operands: 8,
            ..Arm64SpillArea::default()
        },
        &[],
    );
    b.operand_stack = vec![
        Operand::in_reg(OperandKind::Ref, Arm64Register::X9),
        Operand::in_reg(OperandKind::I32, Arm64Register::X10),
        Operand::in_reg(OperandKind::F64, Arm64Register::V0),
    ];
    let before = b.operand_stack.clone();

    b.emit_safepoint_poll(false);

    assert!(!b.failed, "the poll must not refuse this frame");
    assert_eq!(
        b.operand_stack, before,
        "the poll must leave the model as it found it"
    );
    // The oop map DID record the reference operand's slot: the store is
    // what makes it nameable, and the map is taken at the call.
    assert_eq!(b.pending_oop_maps.len(), 1);
    assert_eq!(
        b.pending_oop_maps[0].frame_slot_offsets.len(),
        1,
        "only the reference operand belongs in the map"
    );
}

/// FLOATING-POINT OPERANDS SURVIVE THE POLL'S CALL.
///
/// V0-V7 are as caller-saved as X9-X15, and the poll used to store only the
/// GPR operand stack: a float or double live across a taken poll was
/// whatever the slow path left in its register.
#[test]
pub(in crate::aarch64_backend::tests) fn the_poll_stores_and_reloads_floating_point_operands() {
    let mut b = poll_backend();
    b.install_frame(
        0,
        Arm64SpillArea {
            operands: 8,
            ..Arm64SpillArea::default()
        },
        &[],
    );
    b.operand_stack = vec![
        Operand::in_reg(OperandKind::F32, Arm64Register::V1),
        Operand::in_reg(OperandKind::F64, Arm64Register::V2),
    ];
    b.emit_safepoint_poll(false);
    assert!(!b.failed);

    let ops = b.buffer.instructions();
    let blr = ops
        .iter()
        .position(|i| matches!(i, Arm64Instruction::Blr { .. }))
        .expect("the poll calls the slow path");
    let stored = |vt: Arm64Register, double: bool| {
        ops[..blr].iter().any(|i| {
            matches!(i, Arm64Instruction::FpStr { vt: v, rn, is_double, .. }
                     if *v == vt && *rn == Arm64Register::FP && *is_double == double)
        })
    };
    let reloaded = |vt: Arm64Register, double: bool| {
        ops[blr..].iter().any(|i| {
            matches!(i, Arm64Instruction::FpLdr { vt: v, rn, is_double, .. }
                     if *v == vt && *rn == Arm64Register::FP && *is_double == double)
        })
    };
    assert!(
        stored(Arm64Register::V1, false),
        "the float is stored at its own width"
    );
    assert!(stored(Arm64Register::V2, true), "the double is stored");
    assert!(
        reloaded(Arm64Register::V1, false),
        "the float is reloaded after the call"
    );
    assert!(
        reloaded(Arm64Register::V2, true),
        "the double is reloaded after the call"
    );
}

/// A poll whose store has no reserved slot refuses the method.
///
/// The alternative is a live value sitting in a caller-saved register
/// across a CALL, which is the exact hazard the store exists for.
#[test]
pub(in crate::aarch64_backend::tests) fn a_poll_that_cannot_place_its_spill_refuses() {
    let mut b = poll_backend();
    b.install_frame(0, Arm64SpillArea::default(), &[]);
    b.operand_stack = vec![Operand::in_reg(OperandKind::Ref, Arm64Register::X9)];
    b.emit_safepoint_poll(false);
    assert!(
        b.failed,
        "a spill with nowhere to go must fail the method closed"
    );
}

/// The operand spill area and the frame-homed locals must NOT overlap.
///
/// FIXED 2026-09-03; this failed as written.
///
/// `spill_index_for` numbers a frame-homed local by how many locals BEFORE
/// it also lack a register, so those locals occupy spill indices
/// `0..gpr_spills`. `alloc_scratch` numbers an operand slot
/// `frame.num_reg_locals + depth` -- and `num_reg_locals` is how many
/// locals got a REGISTER, which is the complement of that count, not the
/// end of it.
///
/// `num_spills = gpr_spills + max_stack` is the tell: the frame is sized as
/// if operands began at `gpr_spills`, which is exactly what makes the last
/// operand slot the last reserved word. Basing them at `num_reg_locals`
/// instead either ALIASES a local (fewer locals got registers) or runs PAST
/// the reserved area into the callee-save slots (more did).
#[test]
pub(in crate::aarch64_backend::tests) fn operand_spill_slots_do_not_alias_frame_homed_locals() {
    let mut b = Arm64Backend::new();
    // Four locals: local 0 register-homed, locals 1..3 frame-homed.
    b.local_regs = vec![Some(Arm64Register::X19), None, None, None];
    b.float_local_regs = vec![None, None, None, None];
    let gpr_spills = 3usize;
    let max_stack = 4usize;
    b.frame = Some(Arm64FrameLayout::compute(
        4,
        gpr_spills + max_stack,
        &[Arm64Register::X19],
    ));

    let local_slots: Vec<usize> = (1..4).map(|i| b.spill_index_for(i)).collect();
    assert_eq!(
        local_slots,
        vec![0, 1, 2],
        "frame-homed locals occupy 0..gpr_spills"
    );

    assert_eq!(b.frame.as_ref().unwrap().num_reg_locals, 1);
    // The base both spillers now share.
    let operand_slot_0 = b.local_spill_count();
    assert_eq!(
        operand_slot_0, gpr_spills,
        "the operand area must begin where the locals end"
    );

    assert!(
        !local_slots.contains(&operand_slot_0),
        "operand depth 0 landed on spill slot {operand_slot_0}, which is \
         also a frame-homed local's slot -- the operand stack and the \
         locals share frame words"
    );
    // The OTHER direction: with more locals register-homed than not, the
    // old base ran past the reserved area into the callee-save slots,
    // overwriting a saved register the epilogue restores to the caller.
    let mut b2 = Arm64Backend::new();
    b2.local_regs = vec![
        Some(Arm64Register::X19),
        Some(Arm64Register::X20),
        Some(Arm64Register::X21),
        None,
    ];
    b2.float_local_regs = vec![None; 4];
    let saved = [Arm64Register::X19, Arm64Register::X20, Arm64Register::X21];
    let num_spills = 1 + max_stack; // gpr_spills(1) + max_stack
    b2.frame = Some(Arm64FrameLayout::compute(4, num_spills, &saved));
    let last = b2.local_spill_count() + (max_stack - 1);
    let old_last = b2.frame.as_ref().unwrap().num_reg_locals + (max_stack - 1);
    assert!(
        last < num_spills,
        "the deepest operand slot ({last}) must stay inside the reserved \
         area ({num_spills}); the old base put it at {old_last}"
    );
    assert!(old_last >= num_spills, "the old base really did overrun");
}

/// The four spill regions tile the area exactly, and each accessor refuses
/// outside its OWN region rather than outside the grand total.
///
/// That distinction is the whole point of the type (finding A11). Every one
/// of the four accessors used to bound-check against `frame.num_spills`, so
/// an operand depth past `max_stack` did not refuse — it silently answered
/// the word belonging to a safepoint home, and a home index past the homes
/// answered the safepoint-id word. A refusal at the TOTAL's edge can never
/// catch a region mix-up, because a mix-up stays inside the total by
/// construction.
#[test]
pub(in crate::aarch64_backend::tests) fn each_spill_region_refuses_outside_itself_not_merely_outside_the_frame() {
    let area = Arm64SpillArea {
        locals: 3,
        operands: 4,
        safepoint_homes: 2,
        sp_id: 1,
        context: 0,
    };
    assert_eq!(area.total(), 10, "the total is derived, not restated");

    // The regions tile `0..total` with no gap and no overlap: collect every
    // word each accessor can produce and check the result is exactly
    // `0..total`. A shared word shows up as a duplicate and a gap as a
    // missing index — neither is visible from a total-sized bound.
    let mut words: Vec<usize> = Vec::new();
    words.extend((0..area.locals).filter_map(|i| area.local_word(i)));
    words.extend((0..area.operands).filter_map(|d| area.operand_word(d)));
    words.extend((0..area.safepoint_homes).filter_map(|k| area.safepoint_home_word(k)));
    words.extend(area.sp_id_word());
    words.sort_unstable();
    assert_eq!(words, (0..area.total()).collect::<Vec<_>>());

    // One past each region refuses, even though every one of these indices
    // is inside the frame and would have passed the old check.
    assert_eq!(area.local_word(area.locals), None);
    assert_eq!(area.operand_word(area.operands), None);
    assert_eq!(area.safepoint_home_word(area.safepoint_homes), None);

    // A compilation that does not poll reserves neither homes nor an id
    // word, and asking for either must answer "there is none" rather than
    // the first word past the operands.
    let no_poll = Arm64SpillArea {
        locals: 1,
        operands: 2,
        safepoint_homes: 0,
        sp_id: 0,
        context: 0,
    };
    assert_eq!(no_poll.total(), 3);
    assert_eq!(no_poll.safepoint_home_word(0), None);
    assert_eq!(no_poll.sp_id_word(), None);

    // The all-zero partition is what a backend holds before its prologue
    // runs: every accessor refuses, so an offset can never be computed
    // against a frame that does not exist yet.
    let unsized_area = Arm64SpillArea::default();
    assert_eq!(unsized_area.total(), 0);
    assert_eq!(unsized_area.local_word(0), None);
    assert_eq!(unsized_area.operand_word(0), None);
}

/// The id word is found by NAME, not by "the last one".
///
/// `sp_id_slot_off` used to be computed as `num_spills - 1`, which is the
/// right answer only while the id word happens to be laid out last. It IS
/// laid out last today; the point is that nothing said so, and the next
/// region appended to the area would have silently taken the id word's
/// offset with it.
#[test]
pub(in crate::aarch64_backend::tests) fn the_safepoint_id_word_is_named_rather_than_assumed_to_be_last() {
    let area = Arm64SpillArea {
        locals: 2,
        operands: 5,
        safepoint_homes: 3,
        sp_id: 1,
        context: 0,
    };
    assert_eq!(area.sp_id_word(), Some(10));
    assert_eq!(
        area.sp_id_word(),
        Some(area.total() - 1),
        "today it IS last — this pins the agreement rather than the guess"
    );
}

/// A frame for `n` locals of which `reg_homed` have registers.
pub(in crate::aarch64_backend::tests) fn locals_frame(b: &mut Arm64Backend, reg: &[Option<Arm64Register>], max_stack: usize) {
    b.local_regs = reg.to_vec();
    b.float_local_regs = vec![None; reg.len()];
    let saved: Vec<Arm64Register> = reg.iter().flatten().copied().collect();
    let gpr_spills = reg.iter().filter(|r| r.is_none()).count();
    // The partition production would build for this shape: the frame-homed
    // locals, the operand area, and one safepoint home per register-homed
    // local. No safepoint-id word — these fixtures drive the poll directly
    // rather than through `compile_pass`, which is what reserves it.
    b.install_frame(
        reg.len(),
        Arm64SpillArea {
            locals: gpr_spills,
            operands: max_stack,
            safepoint_homes: saved.len(),
            sp_id: 0,
            context: 0,
        },
        &saved,
    );
}

/// THE POINT OF THIS WHOLE CHANGE: a REGISTER-HOMED reference local is
/// stored to a frame home, named in the map, and reloaded after the call.
///
/// X19-X28 are callee-saved, so the value survives the call on its own --
/// but it survives inside the CALLEE's saved-register area, where only the
/// conservative walk can see it, and a conservative walk marks without
/// being able to REWRITE. A relocating collector therefore could not move
/// an object whose only root was a register local. The store makes it a
/// nameable, rewritable root; the reload is what carries a moved object's
/// new address back into the register.
#[test]
pub(in crate::aarch64_backend::tests) fn a_register_homed_reference_local_is_spilled_named_and_reloaded() {
    let mut b = poll_backend();
    locals_frame(&mut b, &[Some(Arm64Register::X19)], 2);
    // The dataflow says local 0 holds a reference at this pc.
    b.cur_bytecode_pc = 0;
    b.local_oop_masks = vec![0b1];
    b.local_oop_reached = vec![true];

    let home = b
        .safepoint_home_for_reg_local(0)
        .expect("a home must be reserved for a register-homed local");
    b.emit_safepoint_poll(false);
    assert!(!b.failed);

    let stored = b.buffer.instructions().iter().any(|i| {
        matches!(i, Arm64Instruction::Str { rt, rn, offset }
                 if *rt == Arm64Register::X19
                 && *rn == Arm64Register::FP
                 && *offset == home)
    });
    assert!(stored, "the register local must be stored to its home");

    assert_eq!(b.pending_oop_maps.len(), 1);
    let named: Vec<i16> = b.pending_oop_maps[0].frame_slot_offsets.clone();
    assert!(
        named.contains(&(home as i16)),
        "the map must name the home ({home}); named {named:?}"
    );

    let reloaded = b.buffer.instructions().iter().any(|i| {
        matches!(i, Arm64Instruction::Ldr { rt, rn, offset }
                 if *rt == Arm64Register::X19
                 && *rn == Arm64Register::FP
                 && *offset == home)
    });
    assert!(
        reloaded,
        "the register must be reloaded, or a relocated object's new address \
         never reaches the running code"
    );
}

