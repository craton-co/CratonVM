// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Arithmetic, conversion and comparison bytecodes in the single-pass backend's bytecode walk.
//!
//! Split out of `Compiler::compile_bytecode` (`bytecode_walk.rs`), which keeps
//! the walk loop and one dispatch `match` that routes each opcode to its
//! family (`jit-god-functions-and-request-side-channels-FIXED-20260912.md`).
//! The arms are the walk's own, moved unchanged except for how they leave the
//! walk: `continue` became `return WalkStep::Next(pc)` and `return x` became
//! `return WalkStep::Return(x)`.

use super::arith::GprAlu;
use super::bytecode_walk::*;
use super::operand_stack::operand_fold_enabled;
use super::*;

impl Compiler {
    /// Lower one bytecode of this family at `pc`. The result is the pc the walk
    /// continues at, or the value `compile_bytecode` returns.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn walk_arith(
        &mut self,
        code: &[u8],
        code_len: usize,
        op: u8,
        mut pc: usize,
        _dead: &mut bool,
        branch_targets: &[bool],
    ) -> WalkStep {
        match op {
            // iadd
            0x60 => {
                pc = self.walk_gpr_binop(code, code_len, pc, branch_targets, GprAlu::Add, false);
            }

            // ladd
            0x61 => {
                pc = self.walk_gpr_binop(code, code_len, pc, branch_targets, GprAlu::Add, true);
            }

            // fadd
            0x62 => {
                self.emit_float_binop(0x58); // ADDSS
                pc += 1;
            }

            // dadd
            0x63 => {
                self.emit_double_binop(0x58); // ADDSD
                pc += 1;
            }

            // isub
            0x64 => {
                pc = self.walk_gpr_binop(code, code_len, pc, branch_targets, GprAlu::Sub, false);
            }

            // lsub
            0x65 => {
                pc = self.walk_gpr_binop(code, code_len, pc, branch_targets, GprAlu::Sub, true);
            }

            // fsub
            0x66 => {
                self.emit_float_binop(0x5C); // SUBSS
                pc += 1;
            }

            // dsub
            0x67 => {
                self.emit_double_binop(0x5C); // SUBSD
                pc += 1;
            }

            // imul
            0x68 => {
                pc = self.walk_gpr_binop(code, code_len, pc, branch_targets, GprAlu::Imul, false);
            }

            // lmul
            0x69 => {
                pc = self.walk_gpr_binop(code, code_len, pc, branch_targets, GprAlu::Imul, true);
            }

            // fmul
            0x6a => {
                self.emit_float_binop(0x59); // MULSS
                pc += 1;
            }

            // dmul — with strength reduction: dmul by 2.0 → dadd self
            0x6b => {
                // A `dmul` that is itself a branch target merges operand
                // stacks from more than one predecessor, so the constant
                // 2.0 the analysis saw textually before it is only one of
                // the possible right operands (`x * (c ? y : 2.0)` puts
                // `L: ldc2_w 2.0` directly before `M: dmul`). Only the
                // fall-through-only shape may be strength-reduced.
                if self.fp_strength_reduction_pcs.contains(&pc)
                    && !branch_targets.get(pc).copied().unwrap_or(true)
                {
                    // Pattern: <value>, ldc2_w 2.0, dmul
                    // Stack: [value, 2.0] → pop 2.0, emit ADDSD value, value
                    let _two = self.pop_stack(); // discard the 2.0 constant
                    let val = self.pop_stack();
                    self.flush_xmm0_slots_keeping(&[val]);
                    // Load value into XMM0
                    match val {
                        StackSlot::Xmm(xmm) if xmm != 0 => {
                            self.emit_movsd_xmm_xmm(0, xmm);
                        }
                        StackSlot::Xmm(0) => {} // already there
                        _ => {
                            self.load_slot_to_reg(RAX, val);
                            self.buf.emit(&[0x66, 0x48, 0x0F, 0x6E, 0xC0]); // MOVQ XMM0, RAX
                        }
                    }
                    // ADDSD XMM0, XMM0 — doubles the value
                    self.buf.emit(&[0xF2, 0x0F, 0x58, 0xC0]);
                    self.stack_push(StackSlot::Xmm(0), false);
                } else {
                    self.emit_double_binop(0x59); // MULSD
                }
                pc += 1;
            }

            // idiv — JVMS-compliant: guards divide-by-zero (→ deopt to
            // throw ArithmeticException) and INT_MIN / -1 (→ INT_MIN).
            // See `emit_safe_idiv` for the guard sequence.
            0x6c => {
                self.pop_to_rcx(); // divisor
                self.pop_to_rax(); // dividend
                self.emit_safe_idiv(pc, /*is_64bit*/ false, /*is_rem*/ false);
                self.push_from_rax();
                pc += 1;
            }

            // ldiv — JVMS-compliant guards; LONG_MIN / -1 returns LONG_MIN.
            0x6d => {
                self.pop_to_rcx();
                self.pop_to_rax();
                self.emit_safe_idiv(pc, /*is_64bit*/ true, /*is_rem*/ false);
                self.push_from_rax();
                pc += 1;
            }

            // fdiv
            0x6e => {
                self.emit_float_binop(0x5E); // DIVSS
                pc += 1;
            }

            // ddiv
            0x6f => {
                self.emit_double_binop(0x5E); // DIVSD
                pc += 1;
            }

            // irem — JVMS-compliant guards; INT_MIN % -1 returns 0.
            0x70 => {
                self.pop_to_rcx();
                self.pop_to_rax();
                self.emit_safe_idiv(pc, /*is_64bit*/ false, /*is_rem*/ true);
                self.push_from_rax();
                pc += 1;
            }

            // lrem — JVMS-compliant guards; LONG_MIN % -1 returns 0.
            0x71 => {
                self.pop_to_rcx();
                self.pop_to_rax();
                self.emit_safe_idiv(pc, /*is_64bit*/ true, /*is_rem*/ true);
                self.push_from_rax();
                pc += 1;
            }

            // frem / drem — JVM `%` on floating point is the truncated,
            // dividend-signed remainder (C `fmod`), which has no single
            // instruction. Call the same `jit_frem` / `jit_drem` helpers
            // the IR tier lowers to: `extern "C" fn(x, y) -> x` with the
            // operands in XMM0/XMM1 and the result in XMM0 on both ABIs.
            // Without this arm one `%` on a float kept the whole method
            // out of the single-pass tier.
            0x72 | 0x73 => {
                self.flush_xmm0_slots();
                self.flush_scratch_registers();
                let b_slot = self.pop_stack();
                let a_slot = self.pop_stack();
                self.load_slot_to_reg(RAX, a_slot);
                self.emit_movq_xmm_from_rax(0);
                self.load_slot_to_reg(RAX, b_slot);
                self.emit_movq_xmm_from_rax(1);
                let helper = if op == 0x72 {
                    self.helpers.jit_frem
                } else {
                    self.helpers.jit_drem
                };
                // ABI-3, deliberately NOT `assert_helper_call_shape!`. That macro
                // asserts `float_args == 0`, because every site it guards marshals
                // through the integer ARG_REGS file. These two helpers are the
                // opposite case: `jit_frem` is `(f32, f32) -> f32` and `jit_drem` is
                // `(f64, f64) -> f64`, and the sequence above deliberately stages both
                // operands into XMM0/XMM1 instead. Asserting the wrong shape here
                // would be worse than asserting nothing, so what is pinned instead is
                // the fact the site actually depends on: two float arguments, a float
                // result, and nothing in the integer file.
                const _: () = {
                    let frem = cratonvm_jit_api::helpers_abi::helper_sig_of("jit_frem");
                    let drem = cratonvm_jit_api::helpers_abi::helper_sig_of("jit_drem");
                    assert!(
                        frem.arity == 2 && frem.float_args == 2 && frem.returns_value,
                        "`jit_frem` no longer takes two floating-point operands; this \
                         site stages them into XMM0/XMM1 and would now be \
                         loading the wrong register file",
                    );
                    assert!(
                        drem.arity == 2 && drem.float_args == 2 && drem.returns_value,
                        "`jit_drem` no longer takes two floating-point operands; this \
                         site stages them into XMM0/XMM1 and would now be \
                         loading the wrong register file",
                    );
                };
                self.emit_call_absolute(helper);
                self.emit_movq_rax_from_xmm(0);
                self.push_from_rax_as_xmm0();
                pc += 1;
            }

            // ineg
            0x74 => {
                let reg = self.pop_to_work_reg();
                self.emit_group_reg(0xF7, 3, reg, false); // NEG r32
                self.emit_movsxd_self(reg);
                self.push_work_reg(reg);
                pc += 1;
            }

            // lneg
            0x75 => {
                let reg = self.pop_to_work_reg();
                self.emit_group_reg(0xF7, 3, reg, true); // NEG r64
                self.push_work_reg(reg);
                pc += 1;
            }

            // fneg — flip sign bit of float (bit 31)
            0x76 => {
                self.pop_to_rax();
                // XOR EAX, 0x80000000 (flip sign bit, zeroes upper 32 bits)
                self.buf.emit_byte(0x35); // XOR EAX, imm32
                self.buf.emit(&0x8000_0000u32.to_le_bytes());
                self.push_from_rax();
                pc += 1;
            }

            // dneg — flip sign bit of double (bit 63)
            0x77 => {
                self.pop_to_rax();
                // BTC RAX, 63 — complement bit 63
                self.rex_w();
                self.buf.emit(&[0x0F, 0xBA, 0xF8, 63]); // BTC r/m64, imm8
                self.push_from_rax();
                pc += 1;
            }

            // ishl
            0x78 => {
                self.pop_to_rcx(); // shift count, read from CL
                let reg = self.pop_to_work_reg();
                self.emit_group_reg(0xD3, 4, reg, false); // SHL r32, cl
                self.emit_movsxd_self(reg);
                self.push_work_reg(reg);
                pc += 1;
            }

            // lshl
            0x79 => {
                self.pop_to_rcx(); // shift count, read from CL
                let reg = self.pop_to_work_reg();
                self.emit_group_reg(0xD3, 4, reg, true); // SHL r64, cl
                self.push_work_reg(reg);
                pc += 1;
            }

            // ishr
            0x7a => {
                self.pop_to_rcx(); // shift count, read from CL
                let reg = self.pop_to_work_reg();
                self.emit_group_reg(0xD3, 7, reg, false); // SAR r32, cl
                self.emit_movsxd_self(reg);
                self.push_work_reg(reg);
                pc += 1;
            }

            // lshr
            0x7b => {
                self.pop_to_rcx(); // shift count, read from CL
                let reg = self.pop_to_work_reg();
                self.emit_group_reg(0xD3, 7, reg, true); // SAR r64, cl
                self.push_work_reg(reg);
                pc += 1;
            }

            // iushr
            0x7c => {
                self.pop_to_rcx();
                let reg = self.pop_to_work_reg();
                self.emit_group_reg(0xD3, 5, reg, false); // SHR r32, cl
                                                          // MOVSXD reg, reg32: an int is held sign-extended. A shift
                                                          // by 0 (mod 32) leaves bit 31 set, and the 32-bit SHR
                                                          // zero-extends it, so `-1 >>> 0` would read as 2^32 - 1
                                                          // to any 64-bit consumer (a compare, an index, i2l).
                self.emit_movsxd_self(reg);
                self.push_work_reg(reg);
                pc += 1;
            }

            // lushr
            0x7d => {
                self.pop_to_rcx(); // shift count, read from CL
                let reg = self.pop_to_work_reg();
                self.emit_group_reg(0xD3, 5, reg, true); // SHR r64, cl
                self.push_work_reg(reg);
                pc += 1;
            }

            // iand
            0x7e => {
                pc = self.walk_gpr_binop(code, code_len, pc, branch_targets, GprAlu::And, false);
            }

            // land
            0x7f => {
                pc = self.walk_gpr_binop(code, code_len, pc, branch_targets, GprAlu::And, true);
            }

            // ior
            0x80 => {
                pc = self.walk_gpr_binop(code, code_len, pc, branch_targets, GprAlu::Or, false);
            }

            // lor
            0x81 => {
                pc = self.walk_gpr_binop(code, code_len, pc, branch_targets, GprAlu::Or, true);
            }

            // ixor
            0x82 => {
                pc = self.walk_gpr_binop(code, code_len, pc, branch_targets, GprAlu::Xor, false);
            }

            // lxor
            0x83 => {
                pc = self.walk_gpr_binop(code, code_len, pc, branch_targets, GprAlu::Xor, true);
            }

            // i2l — sign-extend int to long. In place on a cached operand
            // (`movsxd r8, r8d`), else straight from the operand's register or
            // frame word into the result register (`emit_sext32_top`).
            0x85 => {
                self.emit_sext32_top();
                pc += 1;
            }

            // i2f — int to float
            0x86 => {
                // CVTSI2SS XMM0, r/m32 — from the operand's own home.
                self.emit_int_to_fp_xmm0(0xF3, false);
                self.stack_push(StackSlot::Xmm(0), false);
                pc += 1;
            }

            // i2d — int to double
            0x87 => {
                // CVTSI2SD XMM0, r/m32 — from the operand's own home.
                self.emit_int_to_fp_xmm0(0xF2, false);
                self.stack_push(StackSlot::Xmm(0), false);
                pc += 1;
            }

            // l2i — truncate long to int: keep the low 32 bits, sign-extended
            // (the same MOVSXD as `i2l`).
            0x88 => {
                self.emit_sext32_top();
                pc += 1;
            }

            // l2f — long to float
            0x89 => {
                // CVTSI2SS XMM0, r/m64 — from the operand's own home.
                self.emit_int_to_fp_xmm0(0xF3, true);
                self.stack_push(StackSlot::Xmm(0), false);
                pc += 1;
            }

            // l2d — long to double
            0x8a => {
                // CVTSI2SD XMM0, r/m64 — from the operand's own home.
                self.emit_int_to_fp_xmm0(0xF2, true);
                self.stack_push(StackSlot::Xmm(0), false);
                pc += 1;
            }

            // f2i — float to int (truncate toward zero, NaN→0, overflow→MAX/MIN)
            0x8b => {
                // The MOVD below writes XMM0: move any pending XMM0 value
                // deeper on the stack out first, as every other conversion
                // arm does, or it is silently replaced by this operand.
                // (`pop_fp_operand_to_xmm0` runs that flush, and reads an
                // XMM-resident operand in place.)
                self.pop_fp_operand_to_xmm0(false);
                // CVTTSS2SI EAX, XMM0: F3 0F 2C C0
                self.buf.emit(&[0xF3, 0x0F, 0x2C, 0xC0]);
                self.emit_fp_to_int_nan_fixup(false, false);
                // Sign-extend EAX to RAX
                self.rex_w();
                self.buf.emit(&[0x63, 0xC0]); // movsxd rax, eax
                self.push_from_rax();
                pc += 1;
            }

            // f2l — float to long (truncate toward zero, NaN→0, overflow→MAX/MIN)
            0x8c => {
                self.pop_fp_operand_to_xmm0(false); // flushes XMM0 first; see f2i
                                                    // CVTTSS2SI RAX, XMM0: F3 48 0F 2C C0
                self.buf.emit(&[0xF3, 0x48, 0x0F, 0x2C, 0xC0]);
                self.emit_fp_to_int_nan_fixup(false, true);
                self.push_from_rax();
                pc += 1;
            }

            // f2d — float to double
            0x8d => {
                self.pop_fp_operand_to_xmm0(false);
                // CVTSS2SD XMM0, XMM0: F3 0F 5A C0
                self.buf.emit(&[0xF3, 0x0F, 0x5A, 0xC0]);
                self.stack_push(StackSlot::Xmm(0), false);
                pc += 1;
            }

            // d2i — double to int (truncate toward zero, NaN→0, overflow→MAX/MIN)
            0x8e => {
                self.pop_fp_operand_to_xmm0(true); // flushes XMM0 first; see f2i
                                                   // CVTTSD2SI EAX, XMM0: F2 0F 2C C0
                self.buf.emit(&[0xF2, 0x0F, 0x2C, 0xC0]);
                self.emit_fp_to_int_nan_fixup(true, false);
                // Sign-extend EAX to RAX
                self.rex_w();
                self.buf.emit(&[0x63, 0xC0]); // movsxd rax, eax
                self.push_from_rax();
                pc += 1;
            }

            // d2l — double to long (truncate toward zero, NaN→0, overflow→MAX/MIN)
            0x8f => {
                self.pop_fp_operand_to_xmm0(true); // flushes XMM0 first; see f2i
                                                   // CVTTSD2SI RAX, XMM0: F2 48 0F 2C C0
                self.buf.emit(&[0xF2, 0x48, 0x0F, 0x2C, 0xC0]);
                self.emit_fp_to_int_nan_fixup(true, true);
                self.push_from_rax();
                pc += 1;
            }

            // d2f — double to float
            0x90 => {
                self.pop_fp_operand_to_xmm0(true);
                // CVTSD2SS XMM0, XMM0: F2 0F 5A C0
                self.buf.emit(&[0xF2, 0x0F, 0x5A, 0xC0]);
                self.stack_push(StackSlot::Xmm(0), false);
                pc += 1;
            }

            // i2b — truncate int to byte (sign-extend)
            0x91 => {
                self.pop_to_rax();
                // MOVSX EAX, AL — sign-extend byte to 32-bit
                self.buf.emit(&[0x0F, 0xBE, 0xC0]);
                // MOVSXD RAX, EAX — sign-extend to 64-bit
                self.rex_w();
                self.buf.emit(&[0x63, 0xC0]);
                self.push_from_rax();
                pc += 1;
            }

            // i2c — truncate int to char (zero-extend unsigned 16-bit)
            0x92 => {
                self.pop_to_rax();
                // MOVZX EAX, AX — zero-extend 16-bit to 32-bit
                self.buf.emit(&[0x0F, 0xB7, 0xC0]);
                // Upper 32 bits of RAX auto-zeroed by 32-bit op
                self.push_from_rax();
                pc += 1;
            }

            // i2s — truncate int to short (sign-extend)
            0x93 => {
                self.pop_to_rax();
                // MOVSX EAX, AX — sign-extend 16-bit to 32-bit
                self.buf.emit(&[0x0F, 0xBF, 0xC0]);
                // MOVSXD RAX, EAX — sign-extend to 64-bit
                self.rex_w();
                self.buf.emit(&[0x63, 0xC0]);
                self.push_from_rax();
                pc += 1;
            }

            // lcmp
            0x94 => {
                // `lcmp; if<cond>` — the loop test of every `for (long i ...)`
                // — as one `CMP r64, r64; Jcc` (see `try_fused_lcmp_branch`).
                if let Some(step) = self.try_fused_lcmp_branch(code, code_len, pc, branch_targets) {
                    return step;
                }
                self.pop_to_rcx(); // value2
                self.pop_to_rax(); // value1
                                   // CMP rax, rcx
                self.rex_w();
                self.buf.emit(&[0x39, 0xC8]); // cmp rax, rcx
                                              // Produce -1, 0, or 1 using SETG/SETL (avoids RBX).
                                              // lcmp is a SIGNED comparison: use SETG (0F 9F),
                                              // not SETA (0F 97, unsigned-above). With SETA, a
                                              // negative operand reads as a huge unsigned value,
                                              // so e.g. `ts == -6L` JIT-compiled as `lcmp; ifne`
                                              // returned "equal" for every ts >= 0 (sign-bit
                                              // clear) — Kafka ListOffsetsHandler computed
                                              // request version 11 instead of 1 for normal
                                              // timestamps. The other two lcmp sites already use
                                              // SETG; this one was the lone unsigned outlier.
                self.buf.emit(&[0x0F, 0x9F, 0xC0]); // SETG AL (signed)
                self.buf.emit(&[0x0F, 0x9C, 0xC1]); // SETL CL (signed)
                                                    // The -1/0/1 difference fits a byte: subtract the two flag
                                                    // bytes and sign-extend once, instead of two MOVZX, a 32-bit
                                                    // SUB and a MOVSXD (12 bytes where there were 17).
                self.buf.emit(&[0x28, 0xC8]); // SUB AL, CL
                self.buf.emit(&[0x48, 0x0F, 0xBE, 0xC0]); // MOVSX RAX, AL
                self.push_from_rax();
                pc += 1;
            }

            // fcmpl — float compare, NaN → -1
            0x95 => {
                self.emit_fcmp(false, false); // float, NaN→-1
                pc += 1;
            }

            // fcmpg — float compare, NaN → 1
            0x96 => {
                self.emit_fcmp(false, true); // float, NaN→1
                pc += 1;
            }

            // dcmpl — double compare, NaN → -1
            0x97 => {
                self.emit_fcmp(true, false); // double, NaN→-1
                pc += 1;
            }

            // dcmpg — double compare, NaN → 1
            0x98 => {
                self.emit_fcmp(true, true); // double, NaN→1
                pc += 1;
            }
            _ => {
                // `compile_bytecode` routed an opcode here that this family
                // does not lower: a dispatch table bug, refused rather than
                // emitted.
                self.fail("singlepass-codegen/walk-family-misdispatch");
                return WalkStep::Return(false);
            }
        }
        WalkStep::Next(pc)
    }

    /// One int/long ALU binop at `pc` (`iadd`, `lsub`, `imul`, `land`, ...),
    /// through `emit_gpr_binop` (`arith.rs`), and the pc the walk continues at.
    ///
    /// When the next bytecode stores the result to a register-homed local
    /// that is one of the operands (`s += x` is `iload s; <x>; iadd; istore
    /// s`), the binop computes straight into the local's register and the
    /// store is consumed: `add r13d, r8d; movsxd r13, r13d` where the walk
    /// used to emit `mov rcx, r8; mov rax, r13; add eax, ecx; movsxd;
    /// mov r8, rax; mov rax, r8; mov r13, rax`. A store to a home that is
    /// not an operand is consumed too (`sub r8d, r13d; movsxd r12, r8d`,
    /// wave 7), and an `i2l` after an int result is (`skip_redundant_i2l`).
    fn walk_gpr_binop(
        &mut self,
        code: &[u8],
        code_len: usize,
        pc: usize,
        branch_targets: &[bool],
        op: GprAlu,
        wide: bool,
    ) -> usize {
        let store = self.fusable_local_store(code, code_len, pc + 1, branch_targets, wide);
        let fused = self.emit_gpr_binop(op, wide, store.map(|(home, _)| home));
        match store {
            Some((_, next_pc)) if fused => {
                // The consumed store has no code of its own; it is "done" at
                // the end of the fused instruction. Nothing branches here
                // (checked), so this is metadata only.
                let native = self.buf.pos() as i32; // Cast: x86-64 immediate encoding
                if let Some(slot) = self.pc_to_native.get_mut(pc + 1) {
                    *slot = native;
                }
                next_pc
            }
            // An int result is pushed re-sign-extended (`op; movsxd`), so an
            // `i2l` right after it has nothing to do.
            _ if !wide => self.skip_redundant_i2l(code, code_len, pc + 1, branch_targets),
            _ => pc + 1,
        }
    }

    /// The register home and the following pc of an `istore`/`lstore` at
    /// `store_pc` that a binop may write directly (`walk_gpr_binop`), or
    /// `None`.
    ///
    /// Only a store that nothing else reaches: `store_pc` must not be a branch
    /// target, or an edge arriving there would bring its own value for the
    /// store, which the fused code never materialises. The width must match
    /// the binop's (`istore` after an int op, `lstore` after a long one — the
    /// verifier guarantees it, and anything else is left alone). The local
    /// must be register-homed; a frame-homed one keeps the ordinary store.
    /// `wide`-prefixed stores are not matched (the byte at `store_pc` is then
    /// `0xc4`).
    fn fusable_local_store(
        &self,
        code: &[u8],
        code_len: usize,
        store_pc: usize,
        branch_targets: &[bool],
        wide: bool,
    ) -> Option<(u8, usize)> {
        if store_pc >= code_len.min(code.len()) {
            return None;
        }
        if branch_targets.get(store_pc).copied().unwrap_or(true) {
            return None;
        }
        let (idx, len) = match (code[store_pc], wide) {
            (0x36, false) | (0x37, true) => (usize::from(*code.get(store_pc + 1)?), 2),
            (op @ 0x3b..=0x3e, false) => (usize::from(op - 0x3b), 1),
            (op @ 0x3f..=0x42, true) => (usize::from(op - 0x3f), 1),
            _ => return None,
        };
        let next_pc = store_pc + len;
        if next_pc > code_len {
            return None;
        }
        let home = self.reg_for_local(idx)?;
        Some((home, next_pc))
    }

    /// `lcmp` at `pc` immediately followed by `ifeq..ifle` at `pc + 1`, lowered
    /// as `CMP value1, value2; Jcc` with the SIGNED condition the pair means
    /// (`lcmp` is `sign(value1 - value2)`, and `if<cond>` tests that against
    /// zero, so `lcmp; ifge` is exactly `value1 >= value2`: JGE). The unfused
    /// pair is `CMP; SETG; SETL; SUB; MOVSX` + a push + `TEST; Jcc`.
    ///
    /// `None` — emit the ordinary `lcmp` — unless the next instruction is one
    /// of the six and is not a branch target (an edge landing on the `if`
    /// brings its own int, which this code never materialises). The emission
    /// mirrors the `if_icmp*` arm in `op_control.rs` step for step: flush the
    /// scratch registers, poll on a back edge (with the operands still on the
    /// stack, as there), canonicalise anything live below the two operands
    /// (both directions), pop, compare, PGO hint, `Jcc rel32`, record the
    /// target's depth. `Some(Return(false))` for a branch target the `if`
    /// cannot name.
    fn try_fused_lcmp_branch(
        &mut self,
        code: &[u8],
        code_len: usize,
        pc: usize,
        branch_targets: &[bool],
    ) -> Option<WalkStep> {
        let if_pc = pc + 1;
        if if_pc + 2 >= code_len || if_pc + 2 >= code.len() {
            return None;
        }
        let cc: u8 = match code[if_pc] {
            0x99 => 0x84, // ifeq -> JE
            0x9a => 0x85, // ifne -> JNE
            0x9b => 0x8C, // iflt -> JL
            0x9c => 0x8D, // ifge -> JGE
            0x9d => 0x8F, // ifgt -> JG
            0x9e => 0x8E, // ifle -> JLE
            _ => return None,
        };
        if branch_targets.get(if_pc).copied().unwrap_or(true) || self.stack.len() < 2 {
            return None;
        }
        let offset = i16::from_be_bytes([code[if_pc + 1], code[if_pc + 2]]) as isize; // Widening: always safe
        let Some(target_pc) = if_pc.checked_add_signed(offset) else {
            return Some(WalkStep::Return(false)); // invalid branch target
        };
        if let Some(slot) = self.pc_to_native.get_mut(if_pc) {
            *slot = self.buf.pos() as i32; // Cast: x86-64 immediate encoding
        }

        // A forward test whose two longs are the whole stack pops them before
        // the flush, which then has nothing to store (the `if_icmp*` arm's
        // `pair_forward` rule, `op_control.rs`).
        let popped_pair = if operand_fold_enabled() && target_pc > if_pc && self.stack.len() == 2 {
            let value2 = self.pop_stack();
            let value1 = self.pop_stack();
            Some((value1, value2))
        } else {
            None
        };
        self.flush_scratch_registers();
        if target_pc <= if_pc {
            self.emit_safepoint_poll();
        }
        if self.stack.len() > 2 {
            self.canonicalize_stack();
        }
        let (value1, value2) = match popped_pair {
            Some(pair) => pair,
            None => {
                let value2 = self.pop_stack();
                let value1 = self.pop_stack();
                (value1, value2)
            }
        };
        let r1 = self.slot_to_gpr(value1, RAX);
        let r2 = self.slot_to_gpr(value2, RCX);
        // CMP r1, r2 — REX.W [R = r2 >= 8] [B = r1 >= 8] 39 /r (r/m64 = r1).
        self.buf.emit_byte(0x48 | ((r2 >> 3) << 2) | (r1 >> 3));
        self.buf.emit(&[0x39, 0xC0 | ((r2 & 7) << 3) | (r1 & 7)]);
        // PGO branch prediction hint, keyed by the `if`'s pc as in op_control.
        if let Some(&is_taken) = self.branch_hints.get(&if_pc) {
            self.buf.emit_byte(if is_taken { 0x3E } else { 0x2E });
        }
        self.buf.emit(&[0x0F, cc]);
        let patch_offset = self.buf.pos();
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
        self.forward_patches.push((patch_offset, target_pc));
        self.record_branch_target_depth(target_pc);
        self.reset_spills();
        Some(WalkStep::Next(if_pc + 3))
    }
}
