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

use super::bytecode_walk::*;
use super::*;

impl Compiler {
    /// Lower one bytecode of this family at `pc`. The result is the pc the walk
    /// continues at, or the value `compile_bytecode` returns.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn walk_arith(
        &mut self,
        _code: &[u8],
        _code_len: usize,
        op: u8,
        mut pc: usize,
        _dead: &mut bool,
        branch_targets: &[bool],
    ) -> WalkStep {
        match op {

            // iadd
            0x60 => {
                self.pop_to_rcx(); // b
                self.pop_to_rax(); // a
                                   // ADD eax, ecx (32-bit, wrapping)
                self.buf.emit(&[0x01, 0xC8]); // add eax, ecx
                                              // Sign-extend eax to rax for consistency
                self.rex_w();
                self.buf.emit(&[0x63, 0xC0]); // movsxd rax, eax
                self.push_from_rax();
                pc += 1;
            }

            // ladd
            0x61 => {
                self.pop_to_rcx();
                self.pop_to_rax();
                // ADD rax, rcx (64-bit)
                self.rex_w();
                self.buf.emit(&[0x01, 0xC8]); // add rax, rcx
                self.push_from_rax();
                pc += 1;
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
                self.pop_to_rcx(); // b
                self.pop_to_rax(); // a
                                   // SUB eax, ecx
                self.buf.emit(&[0x29, 0xC8]); // sub eax, ecx
                self.rex_w();
                self.buf.emit(&[0x63, 0xC0]); // movsxd rax, eax
                self.push_from_rax();
                pc += 1;
            }

            // lsub
            0x65 => {
                self.pop_to_rcx();
                self.pop_to_rax();
                self.rex_w();
                self.buf.emit(&[0x29, 0xC8]); // sub rax, rcx
                self.push_from_rax();
                pc += 1;
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
                self.pop_to_rcx();
                self.pop_to_rax();
                // IMUL eax, ecx
                self.buf.emit(&[0x0F, 0xAF, 0xC1]); // imul eax, ecx
                self.rex_w();
                self.buf.emit(&[0x63, 0xC0]); // movsxd rax, eax
                self.push_from_rax();
                pc += 1;
            }

            // lmul
            0x69 => {
                self.pop_to_rcx();
                self.pop_to_rax();
                // IMUL rax, rcx
                self.rex_w();
                self.buf.emit(&[0x0F, 0xAF, 0xC1]); // imul rax, rcx
                self.push_from_rax();
                pc += 1;
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
                    self.flush_xmm0_slots();
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
                self.emit_call_absolute(helper);
                self.emit_movq_rax_from_xmm(0);
                self.push_from_rax_as_xmm0();
                pc += 1;
            }

            // ineg
            0x74 => {
                self.pop_to_rax();
                // NEG eax
                self.buf.emit(&[0xF7, 0xD8]);
                self.rex_w();
                self.buf.emit(&[0x63, 0xC0]); // movsxd rax, eax
                self.push_from_rax();
                pc += 1;
            }

            // lneg
            0x75 => {
                self.pop_to_rax();
                self.rex_w();
                self.buf.emit(&[0xF7, 0xD8]); // NEG rax
                self.push_from_rax();
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
                self.pop_to_rcx(); // shift count (low 5 bits)
                self.pop_to_rax();
                // SHL eax, cl
                self.buf.emit(&[0xD3, 0xE0]);
                self.rex_w();
                self.buf.emit(&[0x63, 0xC0]); // movsxd
                self.push_from_rax();
                pc += 1;
            }

            // lshl
            0x79 => {
                self.pop_to_rcx();
                self.pop_to_rax();
                self.rex_w();
                self.buf.emit(&[0xD3, 0xE0]); // SHL rax, cl
                self.push_from_rax();
                pc += 1;
            }

            // ishr
            0x7a => {
                self.pop_to_rcx();
                self.pop_to_rax();
                // SAR eax, cl
                self.buf.emit(&[0xD3, 0xF8]);
                self.rex_w();
                self.buf.emit(&[0x63, 0xC0]);
                self.push_from_rax();
                pc += 1;
            }

            // lshr
            0x7b => {
                self.pop_to_rcx();
                self.pop_to_rax();
                self.rex_w();
                self.buf.emit(&[0xD3, 0xF8]); // SAR rax, cl
                self.push_from_rax();
                pc += 1;
            }

            // iushr
            0x7c => {
                self.pop_to_rcx();
                self.pop_to_rax();
                // SHR eax, cl
                self.buf.emit(&[0xD3, 0xE8]);
                // MOVSXD rax, eax: an int is held sign-extended. A shift
                // by 0 (mod 32) leaves bit 31 set, and the 32-bit SHR
                // zero-extends it, so `-1 >>> 0` would read as 2^32 - 1
                // to any 64-bit consumer (a compare, an index, i2l).
                self.rex_w();
                self.buf.emit(&[0x63, 0xC0]);
                self.push_from_rax();
                pc += 1;
            }

            // lushr
            0x7d => {
                self.pop_to_rcx();
                self.pop_to_rax();
                self.rex_w();
                self.buf.emit(&[0xD3, 0xE8]); // SHR rax, cl
                self.push_from_rax();
                pc += 1;
            }

            // iand
            0x7e => {
                self.pop_to_rcx();
                self.pop_to_rax();
                self.buf.emit(&[0x21, 0xC8]); // AND eax, ecx
                self.rex_w();
                self.buf.emit(&[0x63, 0xC0]); // movsxd
                self.push_from_rax();
                pc += 1;
            }

            // land
            0x7f => {
                self.pop_to_rcx();
                self.pop_to_rax();
                self.rex_w();
                self.buf.emit(&[0x21, 0xC8]); // AND rax, rcx
                self.push_from_rax();
                pc += 1;
            }

            // ior
            0x80 => {
                self.pop_to_rcx();
                self.pop_to_rax();
                self.buf.emit(&[0x09, 0xC8]); // OR eax, ecx
                self.rex_w();
                self.buf.emit(&[0x63, 0xC0]);
                self.push_from_rax();
                pc += 1;
            }

            // lor
            0x81 => {
                self.pop_to_rcx();
                self.pop_to_rax();
                self.rex_w();
                self.buf.emit(&[0x09, 0xC8]); // OR rax, rcx
                self.push_from_rax();
                pc += 1;
            }

            // ixor
            0x82 => {
                self.pop_to_rcx();
                self.pop_to_rax();
                self.buf.emit(&[0x31, 0xC8]); // XOR eax, ecx
                self.rex_w();
                self.buf.emit(&[0x63, 0xC0]);
                self.push_from_rax();
                pc += 1;
            }

            // lxor
            0x83 => {
                self.pop_to_rcx();
                self.pop_to_rax();
                self.rex_w();
                self.buf.emit(&[0x31, 0xC8]); // XOR rax, rcx
                self.push_from_rax();
                pc += 1;
            }

            // i2l — sign-extend int to long
            0x85 => {
                self.pop_to_rax();
                // movsxd rax, eax (sign-extend 32→64)
                self.rex_w();
                self.buf.emit(&[0x63, 0xC0]);
                self.push_from_rax();
                pc += 1;
            }

            // i2f — int to float
            0x86 => {
                self.flush_xmm0_slots();
                self.pop_to_rax();
                // CVTSI2SS XMM0, EAX: F3 0F 2A C0
                self.buf.emit(&[0xF3, 0x0F, 0x2A, 0xC0]);
                self.stack_push(StackSlot::Xmm(0), false);
                pc += 1;
            }

            // i2d — int to double
            0x87 => {
                self.flush_xmm0_slots();
                self.pop_to_rax();
                // CVTSI2SD XMM0, EAX: F2 0F 2A C0
                self.buf.emit(&[0xF2, 0x0F, 0x2A, 0xC0]);
                self.stack_push(StackSlot::Xmm(0), false);
                pc += 1;
            }

            // l2i — truncate long to int
            0x88 => {
                self.pop_to_rax();
                // Just keep lower 32 bits, sign-extend
                self.rex_w();
                self.buf.emit(&[0x63, 0xC0]); // movsxd rax, eax
                self.push_from_rax();
                pc += 1;
            }

            // l2f — long to float
            0x89 => {
                self.flush_xmm0_slots();
                self.pop_to_rax();
                // CVTSI2SS XMM0, RAX: F3 48 0F 2A C0
                self.buf.emit(&[0xF3, 0x48, 0x0F, 0x2A, 0xC0]);
                self.stack_push(StackSlot::Xmm(0), false);
                pc += 1;
            }

            // l2d — long to double
            0x8a => {
                self.flush_xmm0_slots();
                self.pop_to_rax();
                // CVTSI2SD XMM0, RAX: F2 48 0F 2A C0
                self.buf.emit(&[0xF2, 0x48, 0x0F, 0x2A, 0xC0]);
                self.stack_push(StackSlot::Xmm(0), false);
                pc += 1;
            }

            // f2i — float to int (truncate toward zero, NaN→0, overflow→MAX/MIN)
            0x8b => {
                // The MOVD below writes XMM0: move any pending XMM0 value
                // deeper on the stack out first, as every other conversion
                // arm does, or it is silently replaced by this operand.
                self.flush_xmm0_slots();
                self.pop_to_rax();
                // MOVD XMM0, EAX: 66 0F 6E C0
                self.buf.emit(&[0x66, 0x0F, 0x6E, 0xC0]);
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
                self.flush_xmm0_slots(); // the MOVD below writes XMM0; see f2i
                self.pop_to_rax();
                // MOVD XMM0, EAX: 66 0F 6E C0
                self.buf.emit(&[0x66, 0x0F, 0x6E, 0xC0]);
                // CVTTSS2SI RAX, XMM0: F3 48 0F 2C C0
                self.buf.emit(&[0xF3, 0x48, 0x0F, 0x2C, 0xC0]);
                self.emit_fp_to_int_nan_fixup(false, true);
                self.push_from_rax();
                pc += 1;
            }

            // f2d — float to double
            0x8d => {
                self.flush_xmm0_slots();
                self.pop_to_rax();
                // MOVD XMM0, EAX: 66 0F 6E C0
                self.buf.emit(&[0x66, 0x0F, 0x6E, 0xC0]);
                // CVTSS2SD XMM0, XMM0: F3 0F 5A C0
                self.buf.emit(&[0xF3, 0x0F, 0x5A, 0xC0]);
                self.stack_push(StackSlot::Xmm(0), false);
                pc += 1;
            }

            // d2i — double to int (truncate toward zero, NaN→0, overflow→MAX/MIN)
            0x8e => {
                self.flush_xmm0_slots(); // the MOVQ below writes XMM0; see f2i
                self.pop_to_rax();
                // MOVQ XMM0, RAX: 66 48 0F 6E C0
                self.buf.emit(&[0x66, 0x48, 0x0F, 0x6E, 0xC0]);
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
                self.flush_xmm0_slots(); // the MOVQ below writes XMM0; see f2i
                self.pop_to_rax();
                // MOVQ XMM0, RAX: 66 48 0F 6E C0
                self.buf.emit(&[0x66, 0x48, 0x0F, 0x6E, 0xC0]);
                // CVTTSD2SI RAX, XMM0: F2 48 0F 2C C0
                self.buf.emit(&[0xF2, 0x48, 0x0F, 0x2C, 0xC0]);
                self.emit_fp_to_int_nan_fixup(true, true);
                self.push_from_rax();
                pc += 1;
            }

            // d2f — double to float
            0x90 => {
                self.flush_xmm0_slots();
                self.pop_to_rax();
                // MOVQ XMM0, RAX: 66 48 0F 6E C0
                self.buf.emit(&[0x66, 0x48, 0x0F, 0x6E, 0xC0]);
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
                self.buf.emit(&[0x0F, 0xB6, 0xC0]); // MOVZX EAX, AL
                self.buf.emit(&[0x0F, 0xB6, 0xC9]); // MOVZX ECX, CL
                self.buf.emit(&[0x29, 0xC8]); // SUB EAX, ECX
                self.rex_w();
                self.buf.emit(&[0x63, 0xC0]); // movsxd rax, eax
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
}
