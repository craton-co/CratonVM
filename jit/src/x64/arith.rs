// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Integer and floating-point arithmetic: peepholes, division, comparisons.
//!
//! Three layers, in increasing order of how much they are allowed to assume:
//!
//! * the constant-operand peepholes (`try_const_arith_peephole` and friends),
//!   which recognise `local op constant` before the generic path spills;
//! * strength reduction for division and remainder — power-of-two shifts and
//!   the magic-multiply sequences, with their per-compile caches;
//! * the FP binops and the three-way `fcmp`/`dcmp` lowering, including the
//!   NaN fixup that the x86 `cvttsd2si` needs and that Java `d2i` semantics
//!   do not tolerate being skipped.

use super::*;

impl Compiler {
    // -----------------------------------------------------------------------
    // OSR exit maps
    // -----------------------------------------------------------------------
    //
    // Moved to `x64/osr.rs`. OSR *entry* publication still lives in
    // `compile_with_param_slots` below.

    // -----------------------------------------------------------------------
    // Safepoints, shadow stack and oop maps
    // -----------------------------------------------------------------------
    //
    // Moved to `x64/safepoint.rs`: the polls, the live-oop publication, and the
    // two elision proofs that let a poll skip publishing.

    /// Emit IEEE 754 NaN/overflow fixup after a CVTT instruction.
    ///
    /// x86 CVTTSS2SI/CVTTSD2SI returns the "indefinite integer" (0x80000000 for
    /// 32-bit, 0x8000000000000000 for 64-bit) for NaN AND overflow. The JVM
    /// spec requires: NaN→0, +overflow→MAX_VALUE, -overflow→MIN_VALUE.
    ///
    /// Call this immediately after the CVTT while XMM0 still holds the source.
    pub(super) fn emit_fp_to_int_nan_fixup(&mut self, is_double: bool, is_long: bool) {
        if !is_long {
            // CMP EAX, 0x80000000
            self.buf.emit_byte(0x3D);
            self.buf.emit(&0x80000000u32.to_le_bytes());
            // JNE .done (short)
            self.buf.emit_byte(0x75);
            let jne_patch = self.buf.pos();
            self.buf.emit_byte(0x00);

            // UCOMI XMM0, XMM0 — PF=1 if NaN
            if is_double {
                self.buf.emit(&[0x66, 0x0F, 0x2E, 0xC0]);
            } else {
                self.buf.emit(&[0x0F, 0x2E, 0xC0]);
            }
            // JP .nan
            self.buf.emit_byte(0x7A);
            let jp_patch = self.buf.pos();
            self.buf.emit_byte(0x00);

            // Not NaN — overflow. Check sign of source.
            // PXOR XMM1, XMM1
            self.buf.emit(&[0x66, 0x0F, 0xEF, 0xC9]);
            if is_double {
                self.buf.emit(&[0x66, 0x0F, 0x2E, 0xC1]); // UCOMISD XMM0, XMM1
            } else {
                self.buf.emit(&[0x0F, 0x2E, 0xC1]); // UCOMISS XMM0, XMM1
            }
            // JBE .done (negative overflow — 0x80000000 already correct)
            self.buf.emit_byte(0x76);
            let jbe_patch = self.buf.pos();
            self.buf.emit_byte(0x00);

            // Positive overflow: MOV EAX, 0x7FFFFFFF
            self.buf.emit_byte(0xB8);
            self.buf.emit(&0x7FFFFFFFu32.to_le_bytes());
            // JMP .done
            self.buf.emit_byte(0xEB);
            let jmp_patch = self.buf.pos();
            self.buf.emit_byte(0x00);

            // .nan: XOR EAX, EAX
            let nan_off = self.buf.pos();
            // Widening: usize offset -> i64 (no truncation; for displacement math)
            Self::patch_rel8_or_bail(
                &mut self.buf,
                jp_patch,
                nan_off as i64 - jp_patch as i64 - 1,
            );
            self.buf.emit(&[0x31, 0xC0]);

            // .done:
            let done_off = self.buf.pos();
            // Widening: usize offsets -> i64 (no truncation; for displacement math)
            Self::patch_rel8_or_bail(
                &mut self.buf,
                jne_patch,
                done_off as i64 - jne_patch as i64 - 1,
            );
            Self::patch_rel8_or_bail(
                &mut self.buf,
                jbe_patch,
                done_off as i64 - jbe_patch as i64 - 1,
            );
            Self::patch_rel8_or_bail(
                &mut self.buf,
                jmp_patch,
                done_off as i64 - jmp_patch as i64 - 1,
            );
        } else {
            // 64-bit: CMP RAX with 0x8000000000000000
            // MOV RCX, 0x8000000000000000
            self.buf.emit(&[0x48, 0xB9]);
            self.buf.emit(&0x8000000000000000u64.to_le_bytes());
            // CMP RAX, RCX
            self.buf.emit(&[0x48, 0x39, 0xC8]);
            // JNE .done
            self.buf.emit_byte(0x75);
            let jne_patch = self.buf.pos();
            self.buf.emit_byte(0x00);

            // UCOMI XMM0, XMM0
            if is_double {
                self.buf.emit(&[0x66, 0x0F, 0x2E, 0xC0]);
            } else {
                self.buf.emit(&[0x0F, 0x2E, 0xC0]);
            }
            // JP .nan
            self.buf.emit_byte(0x7A);
            let jp_patch = self.buf.pos();
            self.buf.emit_byte(0x00);

            // Not NaN — overflow. Check sign.
            self.buf.emit(&[0x66, 0x0F, 0xEF, 0xC9]); // PXOR XMM1, XMM1
            if is_double {
                self.buf.emit(&[0x66, 0x0F, 0x2E, 0xC1]);
            } else {
                self.buf.emit(&[0x0F, 0x2E, 0xC1]);
            }
            // JBE .done (negative overflow)
            self.buf.emit_byte(0x76);
            let jbe_patch = self.buf.pos();
            self.buf.emit_byte(0x00);

            // Positive overflow: MOV RAX, 0x7FFFFFFFFFFFFFFF
            self.buf.emit(&[0x48, 0xB8]);
            self.buf.emit(&0x7FFFFFFFFFFFFFFFu64.to_le_bytes());
            // JMP .done
            self.buf.emit_byte(0xEB);
            let jmp_patch = self.buf.pos();
            self.buf.emit_byte(0x00);

            // .nan: XOR RAX, RAX (48 31 C0)
            let nan_off = self.buf.pos();
            // Widening: usize offset -> i64 (no truncation; for displacement math)
            Self::patch_rel8_or_bail(
                &mut self.buf,
                jp_patch,
                nan_off as i64 - jp_patch as i64 - 1,
            );
            self.buf.emit(&[0x48, 0x31, 0xC0]);

            // .done:
            let done_off = self.buf.pos();
            // Widening: usize offsets -> i64 (no truncation; for displacement math)
            Self::patch_rel8_or_bail(
                &mut self.buf,
                jne_patch,
                done_off as i64 - jne_patch as i64 - 1,
            );
            Self::patch_rel8_or_bail(
                &mut self.buf,
                jbe_patch,
                done_off as i64 - jbe_patch as i64 - 1,
            );
            Self::patch_rel8_or_bail(
                &mut self.buf,
                jmp_patch,
                done_off as i64 - jmp_patch as i64 - 1,
            );
        }
    }

    /// LICM: emit a hoisted loop-invariant integer-arithmetic expression.
    ///
    /// The RPN `steps` program is evaluated using the dedicated arith-LICM
    /// scratch slot pool as a value stack; the final (single) result is left
    /// in RAX. Machine-code sequences for each binary op match the main
    /// emitter byte-for-byte (32-bit ALU op + `movsxd rax,eax` for the
    /// sign-extending ops, plain 32-bit for `iushr`), so the hoisted value is
    /// bit-identical to recomputing the expression in place.
    ///
    /// `PushLocal` reads the local via its register assignment if it has one,
    /// otherwise from its frame slot — the local is loop-invariant so its
    /// value is the same here (pre-header) as on every iteration.
    pub(super) fn emit_arith_hoist_into_rax(&mut self, steps: &[ArithStep]) {
        let scratch_base = self.arith_scratch_base;
        let slot = |k: usize| scratch_base + (k as i32) * 8; // Cast: x86-64 immediate encoding
        let mut depth: usize = 0;
        for step in steps {
            match *step {
                ArithStep::PushConst(v) => {
                    self.emit_mov_imm32_sx(RAX, v);
                    let off = slot(depth);
                    self.emit_store_local(off, RAX);
                    depth += 1;
                }
                ArithStep::PushLocal(l) => {
                    if let Some(reg) = self.reg_for_local(l) {
                        self.emit_mov_reg_reg(RAX, reg);
                    } else {
                        self.emit_load_local(RAX, self.local_offset(l));
                    }
                    let off = slot(depth);
                    self.emit_store_local(off, RAX);
                    depth += 1;
                }
                ArithStep::BinOp(op) => {
                    // depth >= 2 guaranteed by `match_invariant_iarith`.
                    depth -= 1;
                    let off_b = slot(depth);
                    depth -= 1;
                    let off_a = slot(depth);
                    self.emit_load_local(RCX, off_b); // b → RCX
                    self.emit_load_local(RAX, off_a); // a → RAX
                    match op {
                        0x60 => {
                            // iadd: ADD eax,ecx ; movsxd rax,eax
                            self.buf.emit(&[0x01, 0xC8]);
                            self.rex_w();
                            self.buf.emit(&[0x63, 0xC0]);
                        }
                        0x64 => {
                            // isub: SUB eax,ecx ; movsxd
                            self.buf.emit(&[0x29, 0xC8]);
                            self.rex_w();
                            self.buf.emit(&[0x63, 0xC0]);
                        }
                        0x68 => {
                            // imul: IMUL eax,ecx ; movsxd
                            self.buf.emit(&[0x0F, 0xAF, 0xC1]);
                            self.rex_w();
                            self.buf.emit(&[0x63, 0xC0]);
                        }
                        0x78 => {
                            // ishl: SHL eax,cl ; movsxd
                            self.buf.emit(&[0xD3, 0xE0]);
                            self.rex_w();
                            self.buf.emit(&[0x63, 0xC0]);
                        }
                        0x7a => {
                            // ishr: SAR eax,cl ; movsxd
                            self.buf.emit(&[0xD3, 0xF8]);
                            self.rex_w();
                            self.buf.emit(&[0x63, 0xC0]);
                        }
                        0x7c => {
                            // iushr: SHR eax,cl ; movsxd (a zero shift keeps bit 31)
                            self.buf.emit(&[0xD3, 0xE8]);
                            self.rex_w();
                            self.buf.emit(&[0x63, 0xC0]);
                        }
                        0x7e => {
                            // iand: AND eax,ecx ; movsxd
                            self.buf.emit(&[0x21, 0xC8]);
                            self.rex_w();
                            self.buf.emit(&[0x63, 0xC0]);
                        }
                        0x80 => {
                            // ior: OR eax,ecx ; movsxd
                            self.buf.emit(&[0x09, 0xC8]);
                            self.rex_w();
                            self.buf.emit(&[0x63, 0xC0]);
                        }
                        0x82 => {
                            // ixor: XOR eax,ecx ; movsxd
                            self.buf.emit(&[0x31, 0xC8]);
                            self.rex_w();
                            self.buf.emit(&[0x63, 0xC0]);
                        }
                        _ => unreachable!("non-hoistable binop reached emit"),
                    }
                    let off_res = slot(depth);
                    self.emit_store_local(off_res, RAX);
                    depth += 1;
                }
            }
        }
        // Result is the single value left in scratch slot 0.
        self.emit_load_local(RAX, slot(0));
    }

    // -----------------------------------------------------------------------
    // Peephole: constant + arithmetic fusion
    // -----------------------------------------------------------------------

    /// Try to fuse a known constant with the immediately following arithmetic
    /// opcode (imul/idiv/irem). The constant is the RIGHT operand (top of stack).
    /// If the peephole fires, the following opcode is consumed and `true` is returned.
    ///
    /// `branch_targets` is the per-PC branch-target map of the enclosing
    /// `compile_bytecode` pass: fusing is only sound when `next_op_pc` is NOT
    /// a branch target. The fused sequence binds `pc_to_native[next_op_pc]`
    /// to code that hardcodes THIS path's constant and pops only the left
    /// operand; another predecessor branching to `next_op_pc` arrives with
    /// its own right operand on the canonical stack (ternary-in-step merge:
    /// `i += i == 0 ? 2 : 1` — both `iconst` arms feed one `iadd`), so it
    /// would have that operand silently dropped and the fall-through
    /// constant used instead (gap-jit-ternary-in-loop-increment).
    pub(super) fn try_const_arith_peephole(
        &mut self,
        const_val: i32,
        next_op_pc: usize,
        code: &[u8],
        code_len: usize,
        branch_targets: &[bool],
    ) -> bool {
        if next_op_pc >= code_len {
            return false;
        }
        // Never fuse across a merge point (see doc comment).
        if branch_targets.get(next_op_pc).copied().unwrap_or(true) {
            return false;
        }
        let next_op = code[next_op_pc];
        match next_op {
            // imul: left × const_val — always optimizable
            0x68 => {
                self.pc_to_native[next_op_pc] = self.buf.pos() as i32; // Cast: x86-64 immediate encoding
                self.pop_to_rax();
                self.emit_imul_const(const_val);
                self.push_from_rax();
                true
            }
            // idiv: left / const_val — power-of-2 only
            0x6c if const_val > 0 && (const_val & (const_val - 1)) == 0 => {
                self.pc_to_native[next_op_pc] = self.buf.pos() as i32; // Cast: x86-64 immediate encoding
                self.pop_to_rax();
                self.emit_idiv_pow2(const_val);
                self.push_from_rax();
                true
            }
            // irem: left % const_val — power-of-2 only
            0x70 if const_val > 0 && (const_val & (const_val - 1)) == 0 => {
                self.pc_to_native[next_op_pc] = self.buf.pos() as i32; // Cast: x86-64 immediate encoding
                self.pop_to_rax();
                self.emit_irem_pow2(const_val);
                self.push_from_rax();
                true
            }
            // idiv: left / const_val — non-power-of-2 (magic number method)
            0x6c if const_val >= 2 => {
                let (magic, shift) = self.magic_div_cached(const_val);
                self.pc_to_native[next_op_pc] = self.buf.pos() as i32; // Cast: x86-64 immediate encoding
                self.pop_to_rax();
                self.emit_idiv_magic(magic, shift);
                self.push_from_rax();
                true
            }
            // irem: left % const_val — non-power-of-2 (magic number method)
            0x70 if const_val >= 2 => {
                let (magic, shift) = self.magic_div_cached(const_val);
                self.pc_to_native[next_op_pc] = self.buf.pos() as i32; // Cast: x86-64 immediate encoding
                self.pop_to_rax();
                self.emit_irem_magic(magic, shift, const_val);
                self.push_from_rax();
                true
            }
            // iadd: left + const_val
            0x60 => {
                self.pc_to_native[next_op_pc] = self.buf.pos() as i32; // Cast: x86-64 immediate encoding
                self.pop_to_rax();
                if const_val == 0 {
                    // no-op
                } else if (-128..=127).contains(&const_val) {
                    self.buf.emit(&[0x83, 0xC0, const_val as u8]); // ADD EAX, imm8 // Cast: x86-64 immediate encoding
                } else {
                    self.buf.emit(&[0x81, 0xC0]); // ADD EAX, imm32
                    self.buf.emit(&const_val.to_le_bytes());
                }
                self.rex_w();
                self.buf.emit(&[0x63, 0xC0]); // MOVSXD RAX, EAX
                self.push_from_rax();
                true
            }
            // isub: left - const_val
            0x64 => {
                self.pc_to_native[next_op_pc] = self.buf.pos() as i32; // Cast: x86-64 immediate encoding
                self.pop_to_rax();
                if const_val == 0 {
                    // no-op
                } else if (-128..=127).contains(&const_val) {
                    self.buf.emit(&[0x83, 0xE8, const_val as u8]); // SUB EAX, imm8 // Cast: x86-64 immediate encoding
                } else {
                    self.buf.emit(&[0x81, 0xE8]); // SUB EAX, imm32
                    self.buf.emit(&const_val.to_le_bytes());
                }
                self.rex_w();
                self.buf.emit(&[0x63, 0xC0]); // MOVSXD RAX, EAX
                self.push_from_rax();
                true
            }
            // iand: left & const_val
            0x7e => {
                self.pc_to_native[next_op_pc] = self.buf.pos() as i32; // Cast: x86-64 immediate encoding
                self.pop_to_rax();
                if (-128..=127).contains(&const_val) {
                    self.buf.emit(&[0x83, 0xE0, const_val as u8]); // AND EAX, imm8 // Cast: x86-64 immediate encoding
                } else {
                    self.buf.emit(&[0x25]); // AND EAX, imm32
                    self.buf.emit(&const_val.to_le_bytes());
                }
                self.rex_w();
                self.buf.emit(&[0x63, 0xC0]); // MOVSXD RAX, EAX
                self.push_from_rax();
                true
            }
            // ior: left | const_val
            0x80 => {
                self.pc_to_native[next_op_pc] = self.buf.pos() as i32; // Cast: x86-64 immediate encoding
                self.pop_to_rax();
                if (-128..=127).contains(&const_val) {
                    self.buf.emit(&[0x83, 0xC8, const_val as u8]); // OR EAX, imm8 // Cast: x86-64 immediate encoding
                } else {
                    self.buf.emit(&[0x0D]); // OR EAX, imm32
                    self.buf.emit(&const_val.to_le_bytes());
                }
                self.rex_w();
                self.buf.emit(&[0x63, 0xC0]); // MOVSXD RAX, EAX
                self.push_from_rax();
                true
            }
            // ixor: left ^ const_val
            0x82 => {
                self.pc_to_native[next_op_pc] = self.buf.pos() as i32; // Cast: x86-64 immediate encoding
                self.pop_to_rax();
                if (-128..=127).contains(&const_val) {
                    self.buf.emit(&[0x83, 0xF0, const_val as u8]); // XOR EAX, imm8 // Cast: x86-64 immediate encoding
                } else {
                    self.buf.emit(&[0x35]); // XOR EAX, imm32
                    self.buf.emit(&const_val.to_le_bytes());
                }
                self.rex_w();
                self.buf.emit(&[0x63, 0xC0]); // MOVSXD RAX, EAX
                self.push_from_rax();
                true
            }
            // ishl: left << const_val (constant shift count)
            0x78 if (0..=31).contains(&const_val) => {
                self.pc_to_native[next_op_pc] = self.buf.pos() as i32; // Cast: x86-64 immediate encoding
                self.pop_to_rax();
                self.buf.emit(&[0xC1, 0xE0, const_val as u8]); // SHL EAX, imm8 // Cast: x86-64 immediate encoding
                self.rex_w();
                self.buf.emit(&[0x63, 0xC0]); // MOVSXD RAX, EAX
                self.push_from_rax();
                true
            }
            // ishr: left >> const_val (arithmetic, constant shift count)
            0x7a if (0..=31).contains(&const_val) => {
                self.pc_to_native[next_op_pc] = self.buf.pos() as i32; // Cast: x86-64 immediate encoding
                self.pop_to_rax();
                self.buf.emit(&[0xC1, 0xF8, const_val as u8]); // SAR EAX, imm8 // Cast: x86-64 immediate encoding
                self.rex_w();
                self.buf.emit(&[0x63, 0xC0]); // MOVSXD RAX, EAX
                self.push_from_rax();
                true
            }
            // iushr: left >>> const_val (logical, constant shift count)
            0x7c if (0..=31).contains(&const_val) => {
                self.pc_to_native[next_op_pc] = self.buf.pos() as i32; // Cast: x86-64 immediate encoding
                self.pop_to_rax();
                self.buf.emit(&[0xC1, 0xE8, const_val as u8]); // SHR EAX, imm8 // Cast: x86-64 immediate encoding
                self.rex_w();
                self.buf.emit(&[0x63, 0xC0]); // MOVSXD RAX, EAX
                self.push_from_rax();
                true
            }
            _ => false,
        }
    }

    /// Try to fuse a constant with a following if_icmp* opcode.
    /// The constant is value2 (top of stack); value1 is already on the simulated stack.
    /// If the peephole fires, the if_icmp opcode is consumed and the new PC is returned.
    ///
    /// `branch_targets`: same soundness precondition as
    /// `try_const_arith_peephole` — a fused `const; if_icmp*` binds
    /// `pc_to_native[next_op_pc]` to code that compares against THIS path's
    /// constant; a predecessor branching to the if_icmp expects its own
    /// value2 on the stack. Never fuse across a merge point.
    pub(super) fn try_const_compare_peephole(
        &mut self,
        const_val: i32,
        next_op_pc: usize,
        code: &[u8],
        code_len: usize,
        branch_targets: &[bool],
    ) -> Option<usize> {
        if next_op_pc + 2 >= code_len {
            return None;
        }
        if branch_targets.get(next_op_pc).copied().unwrap_or(true) {
            return None;
        }
        let next_op = code[next_op_pc];
        let cc = match next_op {
            0x9f => 0x84u8, // if_icmpeq → JE
            0xa0 => 0x85,   // if_icmpne → JNE
            0xa1 => 0x8C,   // if_icmplt → JL
            0xa2 => 0x8D,   // if_icmpge → JGE
            0xa3 => 0x8F,   // if_icmpgt → JG
            0xa4 => 0x8E,   // if_icmple → JLE
            _ => return None,
        };

        self.pc_to_native[next_op_pc] = self.buf.pos() as i32; // Cast: x86-64 immediate encoding

        let offset = i16::from_be_bytes([code[next_op_pc + 1], code[next_op_pc + 2]]) as i32; // Widening: always safe
        let target_pc = (next_op_pc as i32 + offset) as usize; // Cast: x86-64 immediate encoding

        // Mirror the regular `if_icmp*` arm: flush scratch (including the
        // callee-saved-oop flush), then poll on a backward branch. The fusion
        // skipped both, so `do { work(); } while (i++ < 1000);` — or kotlinc's
        // `for (i in 0 until 100)` over a call-free body — had a back edge with
        // no safepoint poll at all, and a stop-the-world collection waited for
        // the whole loop.
        self.flush_scratch_registers();
        if target_pc <= next_op_pc {
            self.emit_safepoint_poll();
        }

        // Values left below value1 must be flushed to canonical frame slots
        // for the taken edge (the merge-target revival reconstructs them from
        // canonical offsets; the regular if_icmp handler does the same).
        // Canonicalize BEFORE popping value1: a register-resident slot below
        // it would otherwise be stored to a canonical offset that can collide
        // with value1's own frame slot (register slots occupy a stack
        // position but no frame slot, shifting the slots above them down).
        // With value1 still on the simulated stack it is relocated above
        // every store target, so the CMP below reads the preserved value.
        if target_pc > next_op_pc && self.stack.len() > 1 {
            self.canonicalize_stack();
        }

        // Pop value1 (already on stack before the constant was pushed)
        let val1 = self.pop_stack();
        match val1 {
            StackSlot::CalleeSaved(reg) | StackSlot::Scratch(reg, ..) => {
                // CMP reg32, imm — direct compare without loading to RAX
                if reg >= 8 {
                    self.buf.emit_byte(0x41); // REX.B
                }
                if (-128..=127).contains(&const_val) {
                    self.buf.emit_byte(0x83); // CMP r/m32, imm8
                    self.buf.emit_byte(0xF8 | (reg & 7));
                    self.buf.emit_byte(const_val as u8); // Cast: x86-64 immediate encoding
                } else {
                    self.buf.emit_byte(0x81); // CMP r/m32, imm32
                    self.buf.emit_byte(0xF8 | (reg & 7));
                    self.buf.emit(&const_val.to_le_bytes());
                }
            }
            StackSlot::Frame(off) => {
                self.emit_load_local(RAX, off);
                if (-128..=127).contains(&const_val) {
                    self.buf.emit(&[0x83, 0xF8, const_val as u8]); // Cast: x86-64 immediate encoding
                } else {
                    self.buf.emit_byte(0x3D); // CMP EAX, imm32
                    self.buf.emit(&const_val.to_le_bytes());
                }
            }
            StackSlot::Xmm(xmm) => {
                self.emit_movq_rax_from_xmm(xmm);
                if (-128..=127).contains(&const_val) {
                    self.buf.emit(&[0x83, 0xF8, const_val as u8]); // Cast: x86-64 immediate encoding
                } else {
                    self.buf.emit_byte(0x3D); // CMP EAX, imm32
                    self.buf.emit(&const_val.to_le_bytes());
                }
            }
        }

        // Emit Jcc rel32
        self.buf.emit_byte(0x0F);
        self.buf.emit_byte(cc);
        let patch_offset = self.buf.pos();
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);

        self.forward_patches.push((patch_offset, target_pc));
        // Record the taken-edge stack depth so the merge-target revival
        // rebuilds the canonicalized slots (mirrors the regular handler).
        self.record_branch_target_depth(target_pc);
        self.reset_spills();

        Some(next_op_pc + 3)
    }

    // -----------------------------------------------------------------------
    // peephole-cmov — Round-11 HIGH-3
    //
    // Detect the user-written equivalent of `Math.min` / `Math.max`:
    //
    //     iload a            // val1
    //     iload b            // val2
    //     if_icmpXX L1       // 3 bytes
    //     iload a-or-b       // 1 byte   (INSTR_A: the "taken-false" value)
    //     goto    L2         // 3 bytes
    //   L1:
    //     iload b-or-a       // 1 byte   (INSTR_B: the "taken-true" value)
    //   L2:
    //
    // and lower it to `CMP / MOV / CMOV` instead of a branch. The
    // explicit `Math.min(a,b)` invokestatic intrinsic is handled
    // separately in the invoke dispatcher; this peephole catches the
    // inlined source-level pattern. Conservative: only the canonical
    // javac shape where INSTR_A and INSTR_B are 1-byte `iload_0..3`
    // of two *different* locals X and Y, and the immediately-
    // preceding operand loads (also `iload_0..3`) are the same two
    // locals in some order.
    // -----------------------------------------------------------------------

    /// Lookup `iload_0..3` opcode → local index. Returns `None` for
    /// any other opcode.
    fn iload_short_local(op: u8) -> Option<usize> {
        if (0x1A..=0x1D).contains(&op) {
            // Widening: u8 -> usize (opcode-relative local index, value fits)
            Some((op - 0x1A) as usize)
        } else {
            None
        }
    }

    /// Try to emit the if_icmp + iload + goto + iload min/max peephole
    /// as a branchless CMOV sequence. Called at the start of the
    /// if_icmp opcode handler at PC `pc`. If the peephole fires, all
    /// 4 source instructions (if_icmp, INSTR_A, goto, INSTR_B) are
    /// consumed; the function emits CMP+MOV+CMOV and returns
    /// `Some(new_pc)` — the PC to resume from (= the merge point L2).
    /// On `None` the caller emits the regular branch sequence.
    ///
    /// Pre-condition: `val1` and `val2` are the popped operands of
    /// the if_icmp (val1 is the deeper one). The caller MUST NOT
    /// have emitted the CMP or any output yet.
    pub(super) fn try_cmov_minmax_peephole(
        &mut self,
        code: &[u8],
        code_len: usize,
        branch_targets: &[bool],
        pc: usize,
        op: u8,
        val1: StackSlot,
        val2: StackSlot,
    ) -> Option<usize> {
        // Only handle if_icmplt / if_icmpge.
        if !matches!(op, 0xa1 | 0xa2) {
            return None;
        }
        if pc + 3 > code.len() {
            return None;
        }
        // if_icmp branch offset
        // Cast: value to i32 (encoding immediate/displacement)
        let off1 = i16::from_be_bytes([code[pc + 1], code[pc + 2]]) as i32;
        // Cast: value to i32 (encoding immediate/displacement)
        let l1_pc = (pc as i32).checked_add(off1)?;
        if l1_pc < 0 {
            return None;
        }
        // Cast: non-negative index/count to usize
        let l1_pc = l1_pc as usize;

        // INSTR_A at pc+3 must be iload_0..3 (1 byte).
        let a_pc = pc + 3;
        if a_pc >= code.len() {
            return None;
        }
        let a_local = Self::iload_short_local(code[a_pc])?;

        // Next instruction must be `goto` (0xa7) at pc+4.
        let goto_pc = a_pc + 1;
        if goto_pc + 3 > code.len() || code[goto_pc] != 0xa7 {
            return None;
        }
        // Cast: value to i32 (encoding immediate/displacement)
        let goto_off = i16::from_be_bytes([code[goto_pc + 1], code[goto_pc + 2]]) as i32;
        // Cast: value to i32 (encoding immediate/displacement)
        let l2_pc = (goto_pc as i32).checked_add(goto_off)?;
        if l2_pc < 0 {
            return None;
        }
        // Cast: non-negative index/count to usize
        let l2_pc = l2_pc as usize;

        // INSTR_B at the if_icmp branch target. Must be exactly the
        // byte after the `goto` (i.e. pc+7).
        let b_pc = l1_pc;
        if b_pc != goto_pc + 3 || b_pc >= code.len() {
            return None;
        }
        let b_local = Self::iload_short_local(code[b_pc])?;

        // L2 must point at the byte right after INSTR_B (b_pc + 1).
        if l2_pc != b_pc + 1 {
            return None;
        }

        // The two inner loads must be of two *different* locals.
        if a_local == b_local {
            return None;
        }

        // The two if_icmp operands must come from `iload_0..3` of the
        // same two locals (in some order). Canonical pattern:
        //   iload_X (1 byte) ; iload_Y (1 byte) ; if_icmpXX
        if pc < 2 {
            return None;
        }
        let v1_local = Self::iload_short_local(code[pc - 2])?;
        let v2_local = Self::iload_short_local(code[pc - 1])?;
        let mut pair = [v1_local, v2_local];
        pair.sort_unstable();
        let mut inner = [a_local, b_local];
        inner.sort_unstable();
        if pair != inner {
            return None;
        }

        // ---- Merge-point safety. ---------------------------------------
        //
        // The fusion consumes `pc..=l2_pc` and maps every PC in that span to
        // the native offset AFTER the merged result is pushed. That is only
        // sound when the span is entered through this `if_icmp` alone.
        // javac's short-circuit conditions break it routinely:
        // `(ok && a < b) ? a : b` makes the taken-side `iload` the target of
        // the `ifeq` as well, and `(ok || a < b) ? b : a` does the same to the
        // fall-through `iload`. A foreign edge into the span would skip the
        // CMOV's result store and read a stale slot at L2.
        //
        // The operand loads must also be the instructions that produced
        // `val1`/`val2`: `pc-2`/`pc-1` have to be instruction starts (a
        // `sipush 0x1a1b` operand reads as two `iload`s) and must not be
        // reachable from elsewhere, and neither may `pc` itself.
        let is_target = |p: usize| branch_targets.get(p).copied().unwrap_or(true);
        if is_target(pc - 1) || is_target(pc) || is_target(a_pc) || is_target(goto_pc) {
            return None;
        }
        let edges = super::bce::branch_edges(code, code_len)?;
        if edges.iter().any(|&(from, to)| to == b_pc && from != pc) {
            return None;
        }
        let mut start = 0usize;
        let mut starts_ok = (false, false);
        while start < pc && start < code_len {
            if start == pc - 2 {
                starts_ok.0 = true;
            }
            if start == pc - 1 {
                starts_ok.1 = true;
            }
            start += bytecode_analysis::step(code, start).max(1);
        }
        if start != pc || !starts_ok.0 || !starts_ok.1 {
            return None;
        }

        // ---- All checks passed. Emit branchless CMOV. ------------------
        let r1 = self.slot_to_gpr(val1, RAX);
        let r2 = self.slot_to_gpr(val2, RCX);
        self.emit_cmp_r32_r32(r1, r2);
        // Load INSTR_A's value (fall-through) into RAX and INSTR_B's value
        // (taken) into RCX. These must respect register allocation: when a
        // local is register-mapped its frame slot is never written, so a
        // raw `emit_load_local` would read uninitialized stack garbage.
        // (CMP above is already emitted, so clobbering RAX/RCX is safe; the
        // callee-saved home registers of the locals are never RAX/RCX.)
        match self.reg_for_local(a_local) {
            Some(reg) => self.emit_mov_reg_reg(RAX, reg),
            None => {
                let a_off = self.local_offset(a_local);
                self.emit_load_local(RAX, a_off);
            }
        }
        match self.reg_for_local(b_local) {
            Some(reg) => self.emit_mov_reg_reg(RCX, reg),
            None => {
                let b_off = self.local_offset(b_local);
                self.emit_load_local(RCX, b_off);
            }
        }
        // CMOVcc EAX, ECX (no REX.W; iload values are 32-bit ints):
        //   0xa1 → JL  → CMOVL  (0x4C)
        //   0xa2 → JGE → CMOVGE (0x4D)
        let cmov_cc = match op {
            0xa1 => 0x4Cu8,
            0xa2 => 0x4Du8,
            _ => return None,
        };
        // peephole-cmov: branchless lowering of user-written min/max.
        self.buf.emit(&[0x0F, cmov_cc, 0xC1]);
        // The 32-bit CMOV zero-extends into the upper 32 bits of RAX. The
        // JIT keeps `int` values sign-extended to 64 bits (see i2b/i2s/i2l),
        // so re-extend the selected value — otherwise a negative result
        // (e.g. min(-7, 4)) surfaces as a large positive. MOVSXD RAX, EAX.
        self.buf.emit(&[0x48, 0x63, 0xC0]);
        // Push RAX as the merged result.
        self.push_from_rax();

        // Map every consumed bytecode PC to the current native offset
        // so downstream PC-keyed lookups still find a valid destination.
        // Cast: buffer position/length to encoding offset (i32/u32)
        let native = self.buf.pos() as i32;
        for p in pc..=l2_pc {
            if p < self.pc_to_native.len() {
                self.pc_to_native[p] = native;
            }
        }
        // Record the merge-point stack depth so the dispatch loop's
        // merge-point canonicalization (if it kicks in at L2) sees a
        // consistent expectation. We just pushed one value.
        self.record_branch_target_depth(l2_pc);

        Some(l2_pc)
    }

    /// Emit optimized multiply by a known constant (result in EAX, sign-extended to RAX).
    fn emit_imul_const(&mut self, val: i32) {
        match val {
            0 => {
                self.buf.emit(&[0x31, 0xC0]); // XOR EAX, EAX
            }
            1 => { /* input already in EAX */ }
            -1 => {
                self.buf.emit(&[0xF7, 0xD8]); // NEG EAX
            }
            2 => {
                self.buf.emit(&[0x01, 0xC0]); // ADD EAX, EAX
            }
            3 => {
                // LEA EAX, [RAX + RAX*2]
                self.buf.emit(&[0x8D, 0x04, 0x40]);
            }
            4 => {
                self.buf.emit(&[0xC1, 0xE0, 0x02]); // SHL EAX, 2
            }
            5 => {
                // LEA EAX, [RAX + RAX*4]
                self.buf.emit(&[0x8D, 0x04, 0x80]);
            }
            8 => {
                self.buf.emit(&[0xC1, 0xE0, 0x03]); // SHL EAX, 3
            }
            9 => {
                // LEA EAX, [RAX + RAX*8]
                self.buf.emit(&[0x8D, 0x04, 0xC0]);
            }
            // round-7 fix (bug 6): power-of-2 fast path for val >= 16.
            // 2/4/8 are handled above; 16/32/.../2^30 fall through to IMUL
            // imm32 (5 bytes) when they could be a 3-byte SHL EAX, imm8.
            // Negative powers of two are intentionally left to the IMUL
            // path — SHL produces an unsigned shift, and emitting
            // SHL + NEG would not be smaller than IMUL imm8/imm32.
            // Cast: bytecode/native offset to u32 (non-negative, fits)
            _ if val > 0 && (val as u32).is_power_of_two() => {
                // Cast: bytecode/native offset to u32 (non-negative, fits)
                let k = (val as u32).trailing_zeros() as u8;
                // SHL EAX, k (32-bit shift; high bits zero anyway, then
                // the MOVSXD below sign-extends, matching Java imul
                // semantics for non-negative results).
                self.buf.emit(&[0xC1, 0xE0, k]); // SHL EAX, imm8
            }
            _ if (-128..=127).contains(&val) => {
                // IMUL EAX, EAX, imm8
                self.buf.emit(&[0x6B, 0xC0, val as u8]); // Cast: x86-64 immediate encoding
            }
            _ => {
                // IMUL EAX, EAX, imm32
                self.buf.emit(&[0x69, 0xC0]);
                self.buf.emit(&val.to_le_bytes());
            }
        }
        // Sign-extend result to 64 bits (safe no-op for val==0 which zeros RAX)
        if val != 0 {
            self.rex_w();
            self.buf.emit(&[0x63, 0xC0]); // MOVSXD RAX, EAX
        }
    }

    /// Emit a folded affine self-update `local = local*k + c` for an int local
    /// (the result of [`match_affine_chain`]). Reads the local into RAX,
    /// multiplies + adds the (sign-extended) constants, writes it back — a
    /// net-zero effect on the simulated operand stack (the matched bytecodes
    /// were a balanced load…store run), using RAX as the only scratch.
    pub(super) fn emit_affine_fold(&mut self, local: usize, k: i32, c: i32) {
        // RAX = local
        if let Some(reg) = self.reg_for_local(local) {
            self.emit_mov_reg_reg(RAX, reg);
        } else {
            self.emit_load_local(RAX, self.local_offset(local));
        }
        // RAX *= k  (emit_imul_const sign-extends the result for k != 0)
        self.emit_imul_const(k);
        // RAX += c  (matches the const-arith peephole's add encoding)
        if c != 0 {
            if (-128..=127).contains(&c) {
                // Truncation: wider int -> u8 (low 8 bits, intentional)
                self.buf.emit(&[0x83, 0xC0, c as u8]); // ADD EAX, imm8
            } else {
                self.buf.emit(&[0x81, 0xC0]); // ADD EAX, imm32
                self.buf.emit(&c.to_le_bytes());
            }
            self.rex_w();
            self.buf.emit(&[0x63, 0xC0]); // MOVSXD RAX, EAX
        }
        // local = RAX
        if let Some(reg) = self.reg_for_local(local) {
            self.emit_mov_reg_reg(reg, RAX);
        } else {
            self.emit_store_local(self.local_offset(local), RAX);
        }
    }

    /// Emit optimized signed division by power-of-2 constant.
    /// Result: EAX = EAX / 2^k (rounded toward zero), sign-extended to RAX.
    /// Long (cat-2) sibling of [`Self::try_const_arith_peephole`]: fuse a
    /// resolved `ldc2_w` long constant with the immediately following
    /// `lmul`/`ldiv`/`lrem`/`ladd`/`lsub`. Same merge-point rule: never fuse
    /// when the arith op is a branch target. The JVMS ArithmeticException
    /// guard is unnecessary — the constant divisor is known non-zero — and
    /// LONG_MIN / -1 cannot arise (only positive divisors fuse).
    pub(super) fn try_const_arith_peephole_long(
        &mut self,
        const_val: i64,
        next_op_pc: usize,
        code: &[u8],
        code_len: usize,
        branch_targets: &[bool],
    ) -> bool {
        if next_op_pc >= code_len {
            return false;
        }
        if branch_targets.get(next_op_pc).copied().unwrap_or(true) {
            return false;
        }
        let fits_i32 = (-0x8000_0000i64..=0x7FFF_FFFF).contains(&const_val);
        match code[next_op_pc] {
            // lmul: left * const
            0x69 => {
                self.pc_to_native[next_op_pc] = self.buf.pos() as i32; // Cast: x86-64 immediate encoding
                self.pop_to_rax();
                if fits_i32 {
                    self.rex_w();
                    self.buf.emit_byte(0x69); // IMUL RAX, RAX, imm32
                    self.modrm_reg(RAX, RAX);
                    self.buf.emit(&(const_val as i32).to_le_bytes()); // Cast: x86-64 immediate encoding
                } else {
                    self.emit_mov_imm64(RDX, const_val);
                    self.rex_w();
                    self.buf.emit(&[0x0F, 0xAF, 0xC2]); // IMUL RAX, RDX
                }
                self.push_from_rax();
                true
            }
            // ldiv: left / const — power-of-2
            0x6d if const_val > 0
                && (const_val & (const_val - 1)) == 0
                && const_val - 1 <= i32::MAX as i64 =>
            {
                self.pc_to_native[next_op_pc] = self.buf.pos() as i32; // Cast: x86-64 immediate encoding
                self.pop_to_rax();
                self.emit_ldiv_pow2(const_val);
                self.push_from_rax();
                true
            }
            // lrem: left % const — power-of-2
            0x71 if const_val > 0
                && (const_val & (const_val - 1)) == 0
                && const_val - 1 <= i32::MAX as i64 =>
            {
                self.pc_to_native[next_op_pc] = self.buf.pos() as i32; // Cast: x86-64 immediate encoding
                self.pop_to_rax();
                self.emit_lrem_pow2(const_val);
                self.push_from_rax();
                true
            }
            // ldiv: left / const — non-power-of-2 (64-bit magic, mulhi form)
            0x6d if const_val >= 2 => {
                let (magic, shift) = self.magic_div64_cached(const_val);
                self.pc_to_native[next_op_pc] = self.buf.pos() as i32; // Cast: x86-64 immediate encoding
                self.pop_to_rax();
                self.emit_ldiv_magic64(magic, shift);
                self.push_from_rax();
                true
            }
            // lrem: left % const — non-power-of-2
            0x71 if const_val >= 2 => {
                let (magic, shift) = self.magic_div64_cached(const_val);
                self.pc_to_native[next_op_pc] = self.buf.pos() as i32; // Cast: x86-64 immediate encoding
                self.pop_to_rax();
                self.emit_lrem_magic64(magic, shift, const_val);
                self.push_from_rax();
                true
            }
            // ladd: left + const (imm32 range only)
            0x61 if fits_i32 => {
                self.pc_to_native[next_op_pc] = self.buf.pos() as i32; // Cast: x86-64 immediate encoding
                self.pop_to_rax();
                if const_val != 0 {
                    self.rex_w();
                    self.buf.emit(&[0x81, 0xC0]); // ADD RAX, imm32
                    self.buf.emit(&(const_val as i32).to_le_bytes()); // Cast: x86-64 immediate encoding
                }
                self.push_from_rax();
                true
            }
            // lsub: left - const (imm32 range only)
            0x65 if fits_i32 => {
                self.pc_to_native[next_op_pc] = self.buf.pos() as i32; // Cast: x86-64 immediate encoding
                self.pop_to_rax();
                if const_val != 0 {
                    self.rex_w();
                    self.buf.emit(&[0x81, 0xE8]); // SUB RAX, imm32
                    self.buf.emit(&(const_val as i32).to_le_bytes()); // Cast: x86-64 immediate encoding
                }
                self.push_from_rax();
                true
            }
            _ => false,
        }
    }

    /// Signed 64-bit division by 2^k rounding toward zero (RAX in/out).
    fn emit_ldiv_pow2(&mut self, divisor: i64) {
        debug_assert!(divisor > 0 && (divisor & (divisor - 1)) == 0);
        let k = divisor.trailing_zeros();
        if k == 0 {
            return; // div by 1 = no-op
        }
        let mask = divisor - 1; // caller guarantees fits i32
        self.rex_w();
        self.buf.emit(&[0x89, 0xC1]); // MOV RCX, RAX
        self.rex_w();
        self.buf.emit(&[0xC1, 0xF9, 0x3F]); // SAR RCX, 63
        self.rex_w();
        if mask <= 127 {
            self.buf.emit(&[0x83, 0xE1, mask as u8]); // AND RCX, imm8 // Cast: x86-64 immediate encoding
        } else {
            self.buf.emit(&[0x81, 0xE1]); // AND RCX, imm32
            self.buf.emit(&(mask as i32).to_le_bytes()); // Cast: x86-64 immediate encoding
        }
        self.rex_w();
        self.buf.emit(&[0x01, 0xC8]); // ADD RAX, RCX
        self.rex_w();
        self.buf.emit(&[0xC1, 0xF8, k as u8]); // SAR RAX, k // Cast: x86-64 immediate encoding
    }

    /// Signed 64-bit remainder by 2^k (RAX in/out).
    fn emit_lrem_pow2(&mut self, divisor: i64) {
        debug_assert!(divisor > 0 && (divisor & (divisor - 1)) == 0);
        let k = divisor.trailing_zeros();
        if k == 0 {
            self.emit_xor_reg_self(RAX); // a % 1 == 0
            return;
        }
        let mask = divisor - 1; // caller guarantees fits i32
        self.rex_w();
        self.buf.emit(&[0x89, 0xC1]); // MOV RCX, RAX (save original)
        self.rex_w();
        self.buf.emit(&[0x89, 0xC2]); // MOV RDX, RAX
        self.rex_w();
        self.buf.emit(&[0xC1, 0xFA, 0x3F]); // SAR RDX, 63
        self.rex_w();
        if mask <= 127 {
            self.buf.emit(&[0x83, 0xE2, mask as u8]); // AND RDX, imm8 // Cast: x86-64 immediate encoding
        } else {
            self.buf.emit(&[0x81, 0xE2]); // AND RDX, imm32
            self.buf.emit(&(mask as i32).to_le_bytes()); // Cast: x86-64 immediate encoding
        }
        self.rex_w();
        self.buf.emit(&[0x01, 0xD0]); // ADD RAX, RDX
        self.rex_w();
        self.buf.emit(&[0xC1, 0xF8, k as u8]); // SAR RAX, k // Cast: x86-64 immediate encoding
        self.rex_w();
        self.buf.emit(&[0xC1, 0xE0, k as u8]); // SHL RAX, k // Cast: x86-64 immediate encoding
        self.rex_w();
        self.buf.emit(&[0x29, 0xC1]); // SUB RCX, RAX
        self.rex_w();
        self.buf.emit(&[0x89, 0xC8]); // MOV RAX, RCX
    }

    /// Memoized [`Self::magic_signed_div64`].
    fn magic_div64_cached(&mut self, d: i64) -> (i64, u32) {
        if let Some(&pair) = self.magic_div64_memo.get(&d) {
            return pair;
        }
        let pair = Self::magic_signed_div64(d);
        self.magic_div64_memo.insert(d, pair);
        pair
    }

    /// Compute the signed 64-bit magic number for division by constant
    /// `d >= 2` (Hacker's Delight 10-4, W = 64, exact u128 arithmetic).
    /// Returns `(magic, shift)` such that with `t = mulhi_signed(magic, n)`
    /// (plus `n` when `magic < 0`):  `n / d = (t >> shift) + (n >>> 63)`.
    pub(super) fn magic_signed_div64(d: i64) -> (i64, u32) {
        debug_assert!(d >= 2);
        let ad = d as u128;
        let two63: u128 = 1u128 << 63;
        let anc = two63 - 1 - two63 % ad;

        let mut p = 63u32;
        let mut q1 = two63 / anc;
        let mut r1 = two63 - q1 * anc;
        let mut q2 = two63 / ad;
        let mut r2 = two63 - q2 * ad;

        loop {
            p += 1;
            q1 *= 2;
            r1 *= 2;
            if r1 >= anc {
                q1 += 1;
                r1 -= anc;
            }
            q2 *= 2;
            r2 *= 2;
            if r2 >= ad {
                q2 += 1;
                r2 -= ad;
            }
            let delta = ad - 1 - r2;
            if q1 > delta || (q1 == delta && r1 == 0) {
                break;
            }
            if p >= 127 {
                break;
            }
        }

        let magic = (q2 + 1) as u64 as i64; // two's-complement wrap intended
        (magic, p - 64)
    }

    /// Signed 64-bit division by a non-power-of-2 constant via the mulhi
    /// magic method (RAX in/out; clobbers RCX/RDX like the 32-bit variant).
    fn emit_ldiv_magic64(&mut self, magic: i64, shift: u32) {
        self.rex_w();
        self.buf.emit(&[0x89, 0xC1]); // MOV RCX, RAX — save dividend
        self.emit_mov_imm64(RDX, magic);
        self.rex_w();
        self.buf.emit(&[0xF7, 0xEA]); // IMUL RDX — RDX:RAX = RAX * RDX (signed)
        if magic < 0 {
            // d > 0 with a wrapped (negative-as-i64) magic: t += n.
            self.rex_w();
            self.buf.emit(&[0x01, 0xCA]); // ADD RDX, RCX
        }
        if shift > 0 {
            self.rex_w();
            self.buf.emit(&[0xC1, 0xFA, shift as u8]); // SAR RDX, shift // Cast: x86-64 immediate encoding
        }
        self.rex_w();
        self.buf.emit(&[0x89, 0xD0]); // MOV RAX, RDX
        self.rex_w();
        self.buf.emit(&[0x89, 0xCA]); // MOV RDX, RCX
        self.rex_w();
        self.buf.emit(&[0xC1, 0xEA, 0x3F]); // SHR RDX, 63 — sign bit of n
        self.rex_w();
        self.buf.emit(&[0x01, 0xD0]); // ADD RAX, RDX — quotient
    }

    /// Signed 64-bit remainder by a non-power-of-2 constant (RAX in/out).
    fn emit_lrem_magic64(&mut self, magic: i64, shift: u32, divisor: i64) {
        self.emit_ldiv_magic64(magic, shift); // RAX = quotient; RCX = n
        if (-0x8000_0000i64..=0x7FFF_FFFF).contains(&divisor) {
            self.rex_w();
            self.buf.emit_byte(0x69); // IMUL RAX, RAX, imm32
            self.modrm_reg(RAX, RAX);
            self.buf.emit(&(divisor as i32).to_le_bytes()); // Cast: x86-64 immediate encoding
        } else {
            self.emit_mov_imm64(RDX, divisor);
            self.rex_w();
            self.buf.emit(&[0x0F, 0xAF, 0xC2]); // IMUL RAX, RDX
        }
        self.rex_w();
        self.buf.emit(&[0x29, 0xC1]); // SUB RCX, RAX — n - q*d
        self.rex_w();
        self.buf.emit(&[0x89, 0xC8]); // MOV RAX, RCX
    }

    fn emit_idiv_pow2(&mut self, divisor: i32) {
        debug_assert!(divisor > 0 && (divisor & (divisor - 1)) == 0);
        let k = divisor.trailing_zeros();
        if k == 0 {
            return; // div by 1 = no-op
        }
        // Signed division by 2^k rounding toward zero:
        // MOV ECX, EAX;  SAR ECX, 31;  AND ECX, (2^k - 1);
        // ADD EAX, ECX;  SAR EAX, k
        self.buf.emit(&[0x89, 0xC1]); // MOV ECX, EAX
        self.buf.emit(&[0xC1, 0xF9, 0x1F]); // SAR ECX, 31
        let mask = divisor - 1;
        if mask <= 127 {
            self.buf.emit(&[0x83, 0xE1, mask as u8]); // AND ECX, imm8 // Cast: x86-64 immediate encoding
        } else {
            self.buf.emit(&[0x81, 0xE1]);
            self.buf.emit(&mask.to_le_bytes()); // AND ECX, imm32
        }
        self.buf.emit(&[0x01, 0xC8]); // ADD EAX, ECX
        self.buf.emit(&[0xC1, 0xF8, k as u8]); // SAR EAX, k // Cast: x86-64 immediate encoding
        self.rex_w();
        self.buf.emit(&[0x63, 0xC0]); // MOVSXD RAX, EAX
    }

    /// Emit optimized signed remainder by power-of-2 constant.
    /// Result: EAX = EAX % 2^k, sign-extended to RAX.
    fn emit_irem_pow2(&mut self, divisor: i32) {
        debug_assert!(divisor > 0 && (divisor & (divisor - 1)) == 0);
        let k = divisor.trailing_zeros();
        if k == 0 {
            // a % 1 == 0
            self.buf.emit(&[0x31, 0xC0]); // XOR EAX, EAX
            return;
        }
        // remainder = dividend - (dividend / 2^k) * 2^k
        self.buf.emit(&[0x89, 0xC1]); // MOV ECX, EAX (save original)
                                      // Division sequence (clobbers EAX):
        self.buf.emit(&[0x89, 0xC2]); // MOV EDX, EAX
        self.buf.emit(&[0xC1, 0xFA, 0x1F]); // SAR EDX, 31
        let mask = divisor - 1;
        if mask <= 127 {
            self.buf.emit(&[0x83, 0xE2, mask as u8]); // AND EDX, imm8 // Cast: x86-64 immediate encoding
        } else {
            self.buf.emit(&[0x81, 0xE2]);
            self.buf.emit(&mask.to_le_bytes()); // AND EDX, imm32
        }
        self.buf.emit(&[0x01, 0xD0]); // ADD EAX, EDX
        self.buf.emit(&[0xC1, 0xF8, k as u8]); // SAR EAX, k // Cast: x86-64 immediate encoding
                                               // quotient * divisor:
        self.buf.emit(&[0xC1, 0xE0, k as u8]); // SHL EAX, k // Cast: x86-64 immediate encoding
                                               // remainder = original - quotient*divisor
        self.buf.emit(&[0x29, 0xC1]); // SUB ECX, EAX
        self.buf.emit(&[0x89, 0xC8]); // MOV EAX, ECX
        self.rex_w();
        self.buf.emit(&[0x63, 0xC0]); // MOVSXD RAX, EAX
    }

    /// Memoized wrapper around [`Self::magic_signed_div32`]. The magic-number
    /// derivation is a pure function of the divisor; caching it per constant
    /// avoids recomputing the Newton iteration for repeated `/ k` / `% k` in
    /// a loop body. The cached `(magic, shift)` is bit-identical to a fresh
    /// computation, so generated machine code is unchanged.
    fn magic_div_cached(&mut self, d: i32) -> (i64, u32) {
        if let Some(&pair) = self.magic_div_memo.get(&d) {
            return pair;
        }
        let pair = Self::magic_signed_div32(d);
        self.magic_div_memo.insert(d, pair);
        pair
    }

    /// Compute magic number for signed 32-bit division by constant d (d >= 2).
    /// Returns (magic, shift) such that:
    ///   n / d = ((n as i64) * magic) >> (32 + shift) + (n < 0 ? 1 : 0)
    fn magic_signed_div32(d: i32) -> (i64, u32) {
        debug_assert!(d >= 2);
        let ad = d as u64; // Cast: x86-64 immediate encoding
        let two31 = 1u64 << 31;
        let anc = two31 - 1 - two31 % ad;

        let mut p = 31u32;
        let mut q1 = two31 / anc;
        let mut r1 = two31 - q1 * anc;
        let mut q2 = two31 / ad;
        let mut r2 = two31 - q2 * ad;

        loop {
            p += 1;
            q1 *= 2;
            r1 *= 2;
            if r1 >= anc {
                q1 += 1;
                r1 -= anc;
            }
            q2 *= 2;
            r2 *= 2;
            if r2 >= ad {
                q2 += 1;
                r2 -= ad;
            }
            let delta = ad - 1 - r2;
            if q1 > delta || (q1 == delta && r1 == 0) {
                break;
            }
            if p >= 63 {
                break;
            }
        }

        let magic = (q2 + 1) as i64; // Cast: JIT ABI convention
        (magic, p - 32)
    }

    /// Emit optimized signed division by a non-power-of-2 constant using
    /// multiply-and-shift (magic number method from Hacker's Delight).
    /// Input: dividend in EAX. Output: quotient in EAX, sign-extended to RAX.
    fn emit_idiv_magic(&mut self, magic: i64, shift: u32) {
        // MOV ECX, EAX — save dividend for sign correction
        self.buf.emit(&[0x89, 0xC1]);
        // MOVSXD RAX, EAX — sign-extend to 64 bits
        self.rex_w();
        self.buf.emit(&[0x63, 0xC0]);
        // IMUL RAX, RAX, magic
        if (-0x8000_0000..=0x7FFF_FFFF).contains(&magic) {
            self.rex_w();
            self.buf.emit_byte(0x69); // IMUL r64, r/m64, imm32
            self.modrm_reg(RAX, RAX);
            self.buf.emit(&(magic as i32).to_le_bytes()); // Cast: x86-64 immediate encoding
        } else {
            self.emit_mov_imm64(RDX, magic);
            self.rex_w();
            self.buf.emit(&[0x0F, 0xAF, 0xC2]); // IMUL RAX, RDX
        }
        // SAR RAX, 32 + shift
        let total_shift = 32 + shift;
        self.rex_w();
        self.buf.emit(&[0xC1, 0xF8, total_shift as u8]); // SAR RAX, imm8 // Cast: x86-64 immediate encoding
                                                         // Sign correction: SHR ECX, 31; ADD EAX, ECX
        self.buf.emit(&[0xC1, 0xE9, 0x1F]); // SHR ECX, 31
        self.buf.emit(&[0x01, 0xC8]); // ADD EAX, ECX
                                      // MOVSXD RAX, EAX
        self.rex_w();
        self.buf.emit(&[0x63, 0xC0]);
    }

    /// Emit optimized signed remainder by a non-power-of-2 constant.
    /// Input: dividend in EAX. Output: remainder in EAX, sign-extended to RAX.
    fn emit_irem_magic(&mut self, magic: i64, shift: u32, divisor: i32) {
        // MOV ECX, EAX — save original dividend
        self.buf.emit(&[0x89, 0xC1]);
        // MOVSXD RAX, EAX
        self.rex_w();
        self.buf.emit(&[0x63, 0xC0]);
        // IMUL RAX, RAX, magic
        if (-0x8000_0000..=0x7FFF_FFFF).contains(&magic) {
            self.rex_w();
            self.buf.emit_byte(0x69);
            self.modrm_reg(RAX, RAX);
            self.buf.emit(&(magic as i32).to_le_bytes()); // Cast: x86-64 immediate encoding
        } else {
            self.emit_mov_imm64(RDX, magic);
            self.rex_w();
            self.buf.emit(&[0x0F, 0xAF, 0xC2]);
        }
        // SAR RAX, 32 + shift
        let total_shift = 32 + shift;
        self.rex_w();
        self.buf.emit(&[0xC1, 0xF8, total_shift as u8]); // Cast: x86-64 immediate encoding
                                                         // Sign correction: MOV EDX, ECX; SHR EDX, 31; ADD EAX, EDX
        self.buf.emit(&[0x89, 0xCA]); // MOV EDX, ECX
        self.buf.emit(&[0xC1, 0xEA, 0x1F]); // SHR EDX, 31
        self.buf.emit(&[0x01, 0xD0]); // ADD EAX, EDX — quotient in EAX
                                      // Remainder = n - quotient * divisor
        if (-128..=127).contains(&divisor) {
            self.buf.emit(&[0x6B, 0xC0, divisor as u8]); // IMUL EAX, EAX, imm8 // Cast: x86-64 immediate encoding
        } else {
            self.buf.emit(&[0x69, 0xC0]); // IMUL EAX, EAX, imm32
            self.buf.emit(&divisor.to_le_bytes());
        }
        self.buf.emit(&[0x29, 0xC1]); // SUB ECX, EAX (n - q*d)
        self.buf.emit(&[0x89, 0xC8]); // MOV EAX, ECX
                                      // MOVSXD RAX, EAX
        self.rex_w();
        self.buf.emit(&[0x63, 0xC0]);
    }

    /// Emit a JVMS-compliant signed integer division or remainder.
    ///
    /// Assumes the dividend is in RAX and the divisor in RCX. Leaves the
    /// result in RAX (sign-extended to 64 bits for the 32-bit forms so that
    /// the value is safe to push as a long-width stack slot).
    ///
    /// Guards required by JVMS §6.5.{idiv,irem,ldiv,lrem}:
    ///   * divisor == 0 → throw `ArithmeticException` (routed through the
    ///     uncommon-trap deopt stub with `DEOPT_REASON_DIV_BY_ZERO = 3`; the
    ///     interpreter materialises the exception from the i64::MIN sentinel).
    ///   * `INT_MIN / -1` (or `LONG_MIN / -1`) — the raw x86 IDIV faults with
    ///     #DE on this overflow. The Java spec says no exception is raised:
    ///     `idiv`/`ldiv` must return the dividend unchanged, and `irem`/`lrem`
    ///     must return 0. We special-case this with a CMP/CMP/branch pair and
    ///     synthesise the result without executing IDIV.
    ///
    /// `bci` is the bytecode pc used for the deopt-stub bookkeeping.
    pub(super) fn emit_safe_idiv(&mut self, bci: usize, is_64bit: bool, is_rem: bool) {
        // -------- Guard 1: divide-by-zero --------
        if is_64bit {
            // TEST RCX, RCX  (48 85 C9)
            self.buf.emit(&[0x48, 0x85, 0xC9]);
        } else {
            // TEST ECX, ECX  (85 C9)
            self.buf.emit(&[0x85, 0xC9]);
        }
        // JZ rel32 → deopt stub (DEOPT_REASON_DIV_BY_ZERO = 3)
        self.buf.emit(&[0x0F, 0x84]);
        let dz_patch = self.buf.pos();
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
        self.deopt_stubs.push((dz_patch, bci, 3));

        // -------- Guard 2: INT_MIN / -1  (or LONG_MIN / -1) --------
        // If dividend == MIN and divisor == -1, IDIV would raise #DE.
        // Materialise the JVMS-mandated result and skip the IDIV.
        //
        //     CMP   dividend, MIN
        //     JNE   :do_div
        //     CMP   divisor, -1
        //     JNE   :do_div
        //     <materialise result>      ; idiv → dividend (RAX already holds MIN)
        //                               ; irem → 0
        //     JMP   :after_div
        //   :do_div
        //     CDQ / CQO
        //     IDIV  ECX / RCX
        //     <move result into RAX, sign-extending for 32-bit>
        //   :after_div

        // CMP dividend, MIN
        if is_64bit {
            // CMP RAX, imm32 sign-extended — we need full i64::MIN which doesn't
            // fit in imm32. Load i64::MIN into R10 and CMP RAX, R10.
            // MOV R10, i64::MIN  (49 BA <imm64>)
            self.buf.emit(&[0x49, 0xBA]);
            // Cast: non-negative value to u64
            self.buf.emit(&(i64::MIN as u64).to_le_bytes());
            // CMP RAX, R10  (4C 39 D0)
            self.buf.emit(&[0x4C, 0x39, 0xD0]);
        } else {
            // CMP EAX, imm32  (3D <imm32>)
            self.buf.emit_byte(0x3D);
            // Cast: bytecode/native offset to u32 (non-negative, fits)
            self.buf.emit(&(i32::MIN as u32).to_le_bytes());
        }
        // JNE rel8 → :do_div (we'll patch after we know the size)
        self.buf.emit(&[0x75, 0x00]); // placeholder rel8
        let jne1_patch = self.buf.pos() - 1;

        // CMP divisor, -1
        if is_64bit {
            // CMP RCX, -1  (48 83 F9 FF)  — imm8 sign-extended to 64
            self.buf.emit(&[0x48, 0x83, 0xF9, 0xFF]);
        } else {
            // CMP ECX, -1  (83 F9 FF)     — imm8 sign-extended to 32
            self.buf.emit(&[0x83, 0xF9, 0xFF]);
        }
        // JNE rel8 → :do_div
        self.buf.emit(&[0x75, 0x00]);
        let jne2_patch = self.buf.pos() - 1;

        // Materialise the overflow result.
        if is_rem {
            // result = 0
            if is_64bit {
                // XOR EAX, EAX (zeros full RAX)
                self.buf.emit(&[0x31, 0xC0]);
            } else {
                self.buf.emit(&[0x31, 0xC0]);
            }
        } else {
            // result = dividend (RAX/EAX already holds MIN). For 32-bit, ensure
            // RAX is sign-extended like the IDIV path does.
            if !is_64bit {
                // MOVSXD RAX, EAX  (48 63 C0)
                self.buf.emit(&[0x48, 0x63, 0xC0]);
            }
        }
        // JMP rel8 → :after_div
        self.buf.emit(&[0xEB, 0x00]);
        let jmp_after_patch = self.buf.pos() - 1;

        // :do_div — patch JNE targets to here
        let do_div_off = self.buf.pos();
        // Widening: usize/u32 offset -> i64 (no truncation; for rel/displacement math)
        let rel1 = (do_div_off as i64) - (jne1_patch as i64 + 1);
        // Widening: usize/u32 offset -> i64 (no truncation; for rel/displacement math)
        let rel2 = (do_div_off as i64) - (jne2_patch as i64 + 1);
        // A rel8 displacement that does not fit in an i8 would silently
        // miscompile in release builds. `emit_safe_idiv` cannot signal a failure
        // (it returns `()`), so honor the no-panic contract: mark the buffer
        // overflowed (the driver's `if buf.overflowed() { return None; }`
        // discards the half-emitted method and falls back to the interpreter)
        // instead of asserting. The intervening block is fixed-size and small,
        // so this can only fire on a genuine codegen bug. The range check is
        // `patch_rel8_or_bail`'s (which marks the buffer on a miss), not a
        // second hand-rolled copy of it.
        Self::patch_rel8_or_bail(&mut self.buf, jne1_patch, rel1);
        Self::patch_rel8_or_bail(&mut self.buf, jne2_patch, rel2);
        if self.buf.overflowed() {
            return;
        }

        // Sign-extend RAX → RDX:RAX (or EAX → EDX:EAX), then IDIV.
        if is_64bit {
            // CQO  (48 99)
            self.buf.emit(&[0x48, 0x99]);
            // IDIV RCX  (48 F7 F9)
            self.buf.emit(&[0x48, 0xF7, 0xF9]);
        } else {
            // CDQ  (99)
            self.buf.emit_byte(0x99);
            // IDIV ECX  (F7 F9)
            self.buf.emit(&[0xF7, 0xF9]);
        }

        // Move the result (quotient in RAX/EAX, remainder in RDX/EDX) into RAX,
        // sign-extending 32-bit results so callers can treat RAX as i64.
        if is_rem {
            if is_64bit {
                // MOV RAX, RDX  (48 89 D0)
                self.buf.emit(&[0x48, 0x89, 0xD0]);
            } else {
                // MOVSXD RAX, EDX  (48 63 C2)
                self.buf.emit(&[0x48, 0x63, 0xC2]);
            }
        } else if !is_64bit {
            // MOVSXD RAX, EAX  (48 63 C0)
            self.buf.emit(&[0x48, 0x63, 0xC0]);
        }

        // :after_div — patch the JMP from the overflow path.
        let after_off = self.buf.pos();
        // Widening: usize/u32 offset -> i64 (no truncation; for rel/displacement math)
        let rel_jmp = (after_off as i64) - (jmp_after_patch as i64 + 1);
        // No-panic bail (see JNE patch checks above): a rel8 that does not fit in
        // an i8 would silently miscompile, so mark the buffer overflowed and let
        // the driver discard the method instead of asserting.
        Self::patch_rel8_or_bail(&mut self.buf, jmp_after_patch, rel_jmp);
    }

    // -----------------------------------------------------------------------
    // SSE float/double helpers
    // -----------------------------------------------------------------------

    /// SSE float binary op: pop two f32 values, apply SSE scalar op, push result.
    /// `sse_op`: 0x58=ADD, 0x59=MUL, 0x5C=SUB, 0x5E=DIV
    ///
    /// Optimized: if operands are already in XMM registers (from fload of XMM locals
    /// or prior float arithmetic), avoids the GPR→XMM round-trip. Mirrors
    /// emit_double_binop's XMM chaining for float values.
    pub(super) fn emit_float_binop(&mut self, sse_op: u8) {
        let slot2 = self.pop_stack(); // value2 (top)
        let slot1 = self.pop_stack(); // value1 (deeper)

        // See emit_double_binop: relocate any live XMM0 operand still on the
        // remaining stack before this op clobbers XMM0/XMM1 as scratch.
        self.flush_xmm0_slots();

        self.load_slot_to_reg(RCX, slot2);
        self.buf.emit(&[0x66, 0x0F, 0x6E, 0xC9]); // MOVD XMM1, ECX
        self.load_slot_to_reg(RAX, slot1);
        self.buf.emit(&[0x66, 0x0F, 0x6E, 0xC0]); // MOVD XMM0, EAX
                                                  // F3 0F <sse_op> C1 — XMM0 = XMM0 op XMM1
        self.buf.emit(&[0xF3, 0x0F, sse_op, 0xC1]);
        // MOVD EAX, XMM0
        self.buf.emit(&[0x66, 0x0F, 0x7E, 0xC0]);
        self.push_from_rax();
    }

    /// SSE double binary op: pop two f64 values, apply SSE scalar op, push result.
    /// `sse_op`: 0x58=ADD, 0x59=MUL, 0x5C=SUB, 0x5E=DIV
    ///
    /// Optimized: if operands are already in XMM registers (from dload of XMM locals),
    /// avoids the GPR→XMM round-trip. When slot1 is Xmm(0) and slot2 is a high XMM
    /// (8-15), emits the SSE op directly against that register, skipping XMM1 entirely.
    pub(super) fn emit_double_binop(&mut self, sse_op: u8) {
        let slot2 = self.pop_stack(); // value2 (top)
        let slot1 = self.pop_stack(); // value1 (deeper)

        // This op clobbers XMM0 (and XMM1) as scratch. A value still live DEEPER
        // on the operand stack that is parked in XMM0 (the deferred-FP cache, e.g.
        // a prior call result or `push_from_rax_as_xmm0`) would be destroyed by the
        // `load slot1 -> XMM0` below before it is ever consumed. Relocate any such
        // live XMM0 operand to a scratch XMM / frame first. slot1/slot2 are already
        // popped, so this only touches the *remaining* stack. (emit_fcmp already
        // does this; emit_double/float_binop did not — that gap silently corrupted
        // `f(x) + f(g(x))`-shaped code and the whole commons-math FastMath.sin family
        // under JIT, where a call result sat in XMM0 across the next arg's FP math.)
        self.flush_xmm0_slots();

        self.load_slot_to_reg(RCX, slot2);
        self.buf.emit(&[0x66, 0x48, 0x0F, 0x6E, 0xC9]); // MOVQ XMM1, RCX
        self.load_slot_to_reg(RAX, slot1);
        self.buf.emit(&[0x66, 0x48, 0x0F, 0x6E, 0xC0]); // MOVQ XMM0, RAX
                                                        // F2 0F <sse_op> C1 — XMM0 = XMM0 op XMM1
        self.buf.emit(&[0xF2, 0x0F, sse_op, 0xC1]);
        // MOVQ RAX, XMM0
        self.buf.emit(&[0x66, 0x48, 0x0F, 0x7E, 0xC0]);
        self.push_from_rax();
    }

    /// Float/double compare: pop two values, produce -1/0/1.
    /// `is_double`: true for dcmp*, false for fcmp*.
    /// `nan_positive`: true for *cmpg (NaN→1), false for *cmpl (NaN→-1).
    pub(super) fn emit_fcmp(&mut self, is_double: bool, nan_positive: bool) {
        let slot2 = self.pop_stack();
        let slot1 = self.pop_stack();
        self.flush_xmm0_slots();

        // Load slot2 into XMM1 (or use directly for UCOMISD XMM0, XMMn)
        let cmp_xmm2: u8; // register holding value2 for the UCOMI instruction
        match slot2 {
            StackSlot::Xmm(xmm) if xmm >= 2 => {
                // Can use directly in UCOMISD/UCOMISS
                cmp_xmm2 = xmm;
            }
            StackSlot::Xmm(xmm) => {
                if xmm != 1 {
                    if is_double {
                        self.emit_movsd_xmm_xmm(1, xmm);
                    } else {
                        self.emit_movss_xmm_xmm(1, xmm);
                    }
                }
                cmp_xmm2 = 1;
            }
            _ => {
                self.load_slot_to_reg(RCX, slot2);
                if is_double {
                    self.buf.emit(&[0x66, 0x48, 0x0F, 0x6E, 0xC9]); // MOVQ XMM1, RCX
                } else {
                    self.buf.emit(&[0x66, 0x0F, 0x6E, 0xC9]); // MOVD XMM1, ECX
                }
                cmp_xmm2 = 1;
            }
        }

        // Load slot1 into XMM0
        match slot1 {
            StackSlot::Xmm(xmm) if xmm != 0 => {
                if is_double {
                    self.emit_movsd_xmm_xmm(0, xmm);
                } else {
                    self.emit_movss_xmm_xmm(0, xmm);
                }
            }
            StackSlot::Xmm(0) => {} // already in XMM0
            _ => {
                self.load_slot_to_reg(RAX, slot1);
                if is_double {
                    self.buf.emit(&[0x66, 0x48, 0x0F, 0x6E, 0xC0]); // MOVQ XMM0, RAX
                } else {
                    self.buf.emit(&[0x66, 0x0F, 0x6E, 0xC0]); // MOVD XMM0, EAX
                }
            }
        }

        // UCOMISD/UCOMISS XMM0, XMMn
        if is_double {
            // 66 [REX.B] 0F 2E modrm
            let modrm = 0xC0 | (cmp_xmm2 & 7);
            if cmp_xmm2 >= 8 {
                self.buf.emit(&[0x66, 0x41, 0x0F, 0x2E, modrm]);
            } else {
                self.buf.emit(&[0x66, 0x0F, 0x2E, modrm]);
            }
        } else {
            // [REX.B] 0F 2E modrm
            let modrm = 0xC0 | (cmp_xmm2 & 7);
            if cmp_xmm2 >= 8 {
                self.buf.emit(&[0x41, 0x0F, 0x2E, modrm]);
            } else {
                self.buf.emit(&[0x0F, 0x2E, modrm]);
            }
        }

        if nan_positive {
            // *cmpg: NaN → 1
            // Extract all flags before any ALU ops (which clobber CF/ZF/PF)
            // SETA AL (above = value1 > value2)
            self.buf.emit(&[0x0F, 0x97, 0xC0]);
            // SETB CL (below = value1 < value2 or NaN)
            self.buf.emit(&[0x0F, 0x92, 0xC1]);
            // SETP DL (parity = NaN)
            self.buf.emit(&[0x0F, 0x9A, 0xC2]);
            // Now safe to use ALU ops
            // OR AL, DL — positive = above OR NaN
            self.buf.emit(&[0x08, 0xD0]); // OR AL, DL
                                          // XOR DL, 1 — !NaN
            self.buf.emit(&[0x80, 0xF2, 0x01]);
            // AND CL, DL — below AND !NaN
            self.buf.emit(&[0x20, 0xD1]); // AND CL, DL
                                          // MOVZX EAX, AL
            self.buf.emit(&[0x0F, 0xB6, 0xC0]);
            // MOVZX ECX, CL
            self.buf.emit(&[0x0F, 0xB6, 0xC9]);
            // SUB EAX, ECX
            self.buf.emit(&[0x29, 0xC8]);
        } else {
            // *cmpl: NaN → -1 (SETB naturally includes NaN)
            // SETA AL
            self.buf.emit(&[0x0F, 0x97, 0xC0]);
            // MOVZX EAX, AL
            self.buf.emit(&[0x0F, 0xB6, 0xC0]);
            // SETB CL
            self.buf.emit(&[0x0F, 0x92, 0xC1]);
            // MOVZX ECX, CL
            self.buf.emit(&[0x0F, 0xB6, 0xC9]);
            // SUB EAX, ECX
            self.buf.emit(&[0x29, 0xC8]);
        }

        // Sign-extend EAX to RAX (for -1)
        self.rex_w();
        self.buf.emit(&[0x63, 0xC0]); // movsxd rax, eax
        self.push_from_rax();
    }
}
