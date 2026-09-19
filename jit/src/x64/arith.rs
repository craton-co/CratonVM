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

use super::operand_stack::operand_fold_enabled;
use super::*;

/// A two-operand integer ALU instruction of the `op r, r/m` family, as the
/// operand-stack lowering uses it ([`Compiler::emit_gpr_binop`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum GprAlu {
    Add,
    Sub,
    And,
    Or,
    Xor,
    Imul,
}

impl GprAlu {
    /// The `op r, r/m` opcode. ModRM.reg is the destination and ModRM.r/m the
    /// source, so a frame operand folds into the instruction as a memory
    /// operand.
    fn opcode(self) -> &'static [u8] {
        match self {
            GprAlu::Add => &[0x03],
            GprAlu::Sub => &[0x2B],
            GprAlu::And => &[0x23],
            GprAlu::Or => &[0x0B],
            GprAlu::Xor => &[0x33],
            GprAlu::Imul => &[0x0F, 0xAF],
        }
    }

    /// `a op b == b op a`, so the result may be computed in either operand's
    /// register.
    fn commutative(self) -> bool {
        !matches!(self, GprAlu::Sub)
    }
}

/// Where an ALU source operand is read from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AluSrc {
    /// A register that holds the operand's full 64-bit value.
    Reg(u8),
    /// `[rbp - offset]`, the operand's own frame word.
    Frame(i32),
}

impl AluSrc {
    /// The ModRM.r/m register: the register itself, or RBP for a frame word.
    fn rm(self) -> u8 {
        match self {
            AluSrc::Reg(r) => r,
            AluSrc::Frame(_) => RBP,
        }
    }
}

impl Compiler {
    // -----------------------------------------------------------------------
    // Operand-stack integer ALU lowering (round 9 wave 5)
    // -----------------------------------------------------------------------
    //
    // The single-pass backend used to lower every integer binop as
    // `pop_to_rcx; pop_to_rax; op eax, ecx; movsxd; push_from_rax`. Under the
    // pure-kernel operand cache that is four register copies around one ALU
    // instruction (`mov rcx, r8; mov rax, r13; add eax, ecx; mov r8, rax`),
    // and the scalar loop `s += a[i]` ran 21 instructions per element where 5
    // do (`perf-single-pass-scalar-loop-round-trips-every-operand-through-a-
    // scratch-register-20260918.md`). The helpers below read each operand
    // where it already is — its register, the register the slot mirror says
    // holds its frame word, or the frame word as a memory operand — and
    // compute in the register the result will live in. None of it changes
    // the operand-stack MODEL: the entries pushed are the same `Scratch` /
    // `Frame` kinds the old path pushed, so every merge, branch, call and
    // exception edge sees the layouts it always did.

    /// REX for a `reg, r/m` instruction: W when `wide`, R and B for the
    /// extended halves, and no prefix at all for a 32-bit op on two legacy
    /// registers.
    fn emit_rex_reg_rm(&mut self, wide: bool, reg: u8, rm: u8) {
        let mut rex = if wide { 0x48u8 } else { 0x40 };
        if reg >= 8 {
            rex |= 0x04;
        }
        if rm >= 8 {
            rex |= 0x01;
        }
        if rex != 0x40 {
            self.buf.emit_byte(rex);
        }
    }

    /// The ModRM (and displacement) for `reg` over `src`.
    fn emit_modrm_src(&mut self, reg: u8, src: AluSrc) {
        match src {
            AluSrc::Reg(r) => self.modrm_reg(reg, r),
            AluSrc::Frame(off) => self.modrm_rbp_disp(reg, off),
        }
    }

    /// `op dst, src` — 32-bit when `!wide` (the upper half of `dst` is then
    /// ZEROED, so an int caller re-sign-extends with
    /// [`Self::emit_movsxd_self`]).
    pub(super) fn emit_alu_reg_src(&mut self, op: GprAlu, dst: u8, src: AluSrc, wide: bool) {
        self.emit_rex_reg_rm(wide, dst, src.rm());
        self.buf.emit(op.opcode());
        self.emit_modrm_src(dst, src);
    }

    /// `MOVSXD dst, src32`.
    pub(super) fn emit_movsxd_src(&mut self, dst: u8, src: AluSrc) {
        self.emit_rex_reg_rm(true, dst, src.rm());
        self.buf.emit_byte(0x63);
        self.emit_modrm_src(dst, src);
    }

    /// `MOVSXD reg, reg32` — restore the backend's int convention (an `int`
    /// is held sign-extended through all 64 bits) after a 32-bit op.
    pub(super) fn emit_movsxd_self(&mut self, reg: u8) {
        self.emit_movsxd_src(reg, AluSrc::Reg(reg));
    }

    /// `CMP lhs32, src32` (`3B /r`): the flags of `lhs - src`.
    pub(super) fn emit_cmp_r32_src(&mut self, lhs: u8, src: AluSrc) {
        self.emit_rex_reg_rm(false, lhs, src.rm());
        self.buf.emit_byte(0x3B);
        self.emit_modrm_src(lhs, src);
    }

    /// Group-1 ALU op `/ext` on `reg` with a sign-extended immediate:
    /// `83 /ext ib` when `imm` fits `i8`, else `81 /ext id`. `ext`: 0 ADD,
    /// 1 OR, 4 AND, 5 SUB, 6 XOR, 7 CMP.
    pub(super) fn emit_alu_reg_imm(&mut self, ext: u8, reg: u8, imm: i32, wide: bool) {
        self.emit_rex_reg_rm(wide, 0, reg);
        if let Ok(imm8) = i8::try_from(imm) {
            self.buf.emit_byte(0x83);
            self.modrm_reg(ext, reg);
            self.buf.emit(&imm8.to_le_bytes());
        } else {
            self.buf.emit_byte(0x81);
            self.modrm_reg(ext, reg);
            self.buf.emit(&imm.to_le_bytes());
        }
    }

    /// A one-operand group op `opcode /ext` on `reg`: `F7 /3` NEG,
    /// `D3 /4|5|7` SHL/SHR/SAR by CL.
    pub(super) fn emit_group_reg(&mut self, opcode: u8, ext: u8, reg: u8, wide: bool) {
        self.emit_rex_reg_rm(wide, 0, reg);
        self.buf.emit_byte(opcode);
        self.modrm_reg(ext, reg);
    }

    /// `C1 /ext ib` — SHL (4), SHR (5) or SAR (7) `reg` by a constant.
    pub(super) fn emit_shift_reg_imm(&mut self, ext: u8, reg: u8, count: u8, wide: bool) {
        self.emit_group_reg(0xC1, ext, reg, wide);
        self.buf.emit_byte(count);
    }

    /// Where to read a popped operand from without copying it: its own
    /// register; the register the slot mirror says holds its frame word
    /// right now; or the frame word itself, as a memory operand. Only an
    /// `Xmm` operand costs a move, into `scratch`.
    ///
    /// Read the answer in the very next instruction. A `Frame` answer names a
    /// word the caller has just POPPED, which the next `push_stack` may hand
    /// out again; a mirror answer is only true until something else is
    /// emitted.
    pub(super) fn alu_src_for(&mut self, slot: StackSlot, scratch: u8) -> AluSrc {
        match slot {
            StackSlot::CalleeSaved(r) | StackSlot::Scratch(r, ..) => AluSrc::Reg(r),
            StackSlot::Frame(off) => match self.mirrored_reg(off) {
                Some(r) => AluSrc::Reg(r),
                None => {
                    self.dbg_note_slot_load(off);
                    AluSrc::Frame(off)
                }
            },
            StackSlot::Xmm(xmm) => {
                self.emit_movq_gpr_from_xmm(scratch, xmm);
                AluSrc::Reg(scratch)
            }
        }
    }

    /// `a op b` over the two operands on top of the stack — `int` when
    /// `!wide` (32-bit op, result re-sign-extended), `long` otherwise.
    ///
    /// Where the result is computed, in order of preference:
    ///
    /// 1. `store_home`, when the caller has proved the very next bytecode is
    ///    a store of the result to the register-homed local living in
    ///    `store_home`, one of the operands IS that local, and no other
    ///    entry reads its old value: `x = x + y` becomes `add home, y`.
    ///    Returns `true` and pushes nothing; the caller consumes the store.
    /// 2. The left operand's scratch register, in place (or the right one's
    ///    for a commutative op).
    /// 3. A free scratch register (cache on) or RAX, loaded with the left
    ///    operand — never the right operand's register, which is read after.
    ///
    /// The right operand is read where it is (`alu_src_for`). With a
    /// `store_home` that 1 could not use (the home is not an operand, or is
    /// the right operand of a `sub`) but that nothing else reads, the result
    /// of 2/3 is moved into the home (`movsxd home, r32` for an int) and
    /// `true` is returned, pushing nothing (round 9 wave 7). Otherwise
    /// returns `false` after pushing the result.
    ///
    /// Under `CRATONVM_JIT_NO_OPERAND_FOLD=1`: `b` into RCX, `a` into RAX,
    /// `op rax, rcx`, `push_from_rax` — the pre-wave-5 lowering.
    pub(super) fn emit_gpr_binop(
        &mut self,
        op: GprAlu,
        wide: bool,
        store_home: Option<u8>,
    ) -> bool {
        let b = self.pop_stack();
        let a = self.pop_stack();
        if !operand_fold_enabled() {
            self.load_slot_to_reg(RCX, b);
            self.load_slot_to_reg(RAX, a);
            self.emit_alu_reg_src(op, RAX, AluSrc::Reg(RCX), wide);
            if !wide {
                self.emit_movsxd_self(RAX);
            }
            self.push_from_rax();
            return false;
        }
        if let Some(home) = store_home {
            if self.reg_is_unshared(home) {
                let other = match (a, b) {
                    (StackSlot::CalleeSaved(r), _) if r == home => Some(b),
                    (_, StackSlot::CalleeSaved(r)) if r == home && op.commutative() => Some(a),
                    _ => None,
                };
                if let Some(other) = other {
                    let src = self.alu_src_for(other, RCX);
                    self.emit_alu_reg_src(op, home, src, wide);
                    if !wide {
                        self.emit_movsxd_self(home);
                    }
                    return true;
                }
            }
        }
        let (dst, other) = match (a, b) {
            (StackSlot::Scratch(r, ..), _) if self.reg_is_unshared(r) => (r, b),
            (_, StackSlot::Scratch(r, ..)) if op.commutative() && self.reg_is_unshared(r) => (r, a),
            _ => {
                let avoid = match b {
                    StackSlot::Scratch(r, ..) | StackSlot::CalleeSaved(r) => Some(r),
                    _ => None,
                };
                let dst = self.free_scratch_reg(avoid).unwrap_or(RAX);
                self.load_slot_to_reg(dst, a);
                (dst, b)
            }
        };
        let src = self.alu_src_for(other, RCX);
        self.emit_alu_reg_src(op, dst, src, wide);
        // 4. The store goes to a home that is not an operand (or is the right
        //    operand of a `sub`, read by the instruction above): the result
        //    moves there directly, the int re-sign-extension doubling as the
        //    move — `sub r8d, r13d; movsxd r13, r8d` for `s = a[i] - s`,
        //    where the push/pop pair left `movsxd r8, r8d; mov r13, r8`.
        //    Same condition as 1: no remaining entry reads the home's old
        //    value (so `invalidate_callee_saved` in the store would emit
        //    nothing either).
        if let Some(home) = store_home {
            if home != dst && self.reg_is_unshared(home) {
                if wide {
                    self.emit_mov_reg_reg(home, dst);
                } else {
                    self.emit_movsxd_src(home, AluSrc::Reg(dst));
                }
                return true;
            }
        }
        if !wide {
            self.emit_movsxd_self(dst);
        }
        self.push_work_reg(dst);
        false
    }

    /// `i2l` / `l2i`: the top operand's low 32 bits, sign-extended to 64.
    ///
    /// In place on an unshared scratch operand (`movsxd r8, r8d`); otherwise
    /// straight from the operand's register or frame word into the result
    /// register (`movsxd r8, r13d`, `movsxd rax, dword [rbp-x]`) instead of
    /// copying it into RAX first. Under `CRATONVM_JIT_NO_OPERAND_FOLD=1`:
    /// `pop_to_rax; movsxd rax, eax; push_from_rax`.
    pub(super) fn emit_sext32_top(&mut self) {
        if !operand_fold_enabled() {
            self.pop_to_rax();
            self.emit_movsxd_self(RAX);
            self.push_from_rax();
            return;
        }
        let slot = self.pop_stack();
        if let StackSlot::Scratch(reg, ..) = slot {
            if self.reg_is_unshared(reg) {
                self.emit_movsxd_self(reg);
                self.push_work_reg(reg);
                return;
            }
        }
        let dst = self.free_scratch_reg(None).unwrap_or(RAX);
        let src = self.alu_src_for(slot, dst);
        self.emit_movsxd_src(dst, src);
        self.push_work_reg(dst);
    }

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
    /// Clobbers XMM1 (and the flags) on the rare indefinite path only.
    ///
    /// The indefinite test is `CMP r, 1; JNO .done`: `r - 1` overflows exactly
    /// when `r` is the most negative value, which is the indefinite pattern.
    /// It replaced `MOV RCX, 0x8000000000000000; CMP RAX, RCX` (13 bytes and a
    /// clobbered RCX on every `f2l`/`d2l`) and `CMP EAX, imm32` (5 bytes);
    /// the positive-overflow result is `NOT` of the indefinite pattern
    /// (0x80..0 → 0x7F..F) instead of a 5- or 10-byte immediate load. The
    /// fast path — every in-range conversion — is now 4 or 5 bytes and one
    /// not-taken branch for both widths.
    pub(super) fn emit_fp_to_int_nan_fixup(&mut self, is_double: bool, is_long: bool) {
        // CMP EAX/RAX, 1  — OF=1 iff the value is i32::MIN / i64::MIN.
        if is_long {
            self.buf.emit(&[0x48, 0x83, 0xF8, 0x01]);
        } else {
            self.buf.emit(&[0x83, 0xF8, 0x01]);
        }
        // JNO .done (short) — not the indefinite pattern: already correct.
        self.buf.emit_byte(0x71);
        let jno_patch = self.buf.pos();
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
        // JBE .done (negative overflow — the MIN pattern is already correct)
        self.buf.emit_byte(0x76);
        let jbe_patch = self.buf.pos();
        self.buf.emit_byte(0x00);

        // Positive overflow: NOT turns the MIN pattern into MAX. The 32-bit
        // form zero-extends into RAX exactly as the `MOV EAX, imm32` it
        // replaced did.
        if is_long {
            self.buf.emit(&[0x48, 0xF7, 0xD0]); // NOT RAX
        } else {
            self.buf.emit(&[0xF7, 0xD0]); // NOT EAX
        }
        // JMP .done
        self.buf.emit_byte(0xEB);
        let jmp_patch = self.buf.pos();
        self.buf.emit_byte(0x00);

        // .nan: XOR EAX, EAX — zeroes all of RAX for both widths.
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
            jno_patch,
            done_off as i64 - jno_patch as i64 - 1,
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
                if operand_fold_enabled() && Self::imul_const_is_generic(const_val) {
                    // The three-operand IMUL reads the operand where it is and
                    // writes the work register: `imul r8d, r13d, 31; movsxd r8,
                    // r8d` where the RAX form is `mov rax, r13; imul eax, eax,
                    // 31; movsxd rax, eax; mov r8, rax` (round 9 wave 7). The
                    // strength-reduced constants keep their LEA/SHL/ADD forms.
                    let slot = self.pop_stack();
                    let dst = match slot {
                        StackSlot::Scratch(r, ..) if self.reg_is_unshared(r) => r,
                        _ => self.free_scratch_reg(None).unwrap_or(RAX),
                    };
                    let src = self.alu_src_for(slot, dst);
                    self.emit_imul_reg_src_imm(dst, src, const_val);
                    self.emit_movsxd_self(dst);
                    self.push_work_reg(dst);
                    return true;
                }
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
            // iadd / isub / iand / ior / ixor / ishl / ishr / iushr with a
            // constant right operand. The left operand is taken with
            // `pop_to_work_reg`: a cached scratch operand is updated IN PLACE
            // (`add r8d, 1; movsxd r8, r8d`) instead of being copied into RAX
            // and parked back (`mov rax, r8; add eax, 1; movsxd rax, eax;
            // mov r8, rax`). When the work register is RAX — every method
            // without the operand cache — the historical RAX encodings below
            // are emitted unchanged.
            0x60 | 0x64 | 0x7e | 0x80 | 0x82 => {
                self.pc_to_native[next_op_pc] = self.buf.pos() as i32; // Cast: x86-64 immediate encoding
                let reg = self.pop_to_work_reg();
                // Group-1 `/ext` and the RAX short forms of each op.
                let (ext, rax_imm32_opcode): (u8, &[u8]) = match next_op {
                    0x60 => (0, &[0x81u8, 0xC0][..]), // ADD EAX, imm32
                    0x64 => (5, &[0x81u8, 0xE8][..]), // SUB EAX, imm32
                    0x7e => (4, &[0x25u8][..]),       // AND EAX, imm32
                    0x80 => (1, &[0x0Du8][..]),       // OR EAX, imm32
                    _ => (6, &[0x35u8][..]),          // 0x82: XOR EAX, imm32
                };
                // `x + 0` / `x - 0` emit no ALU op (the MOVSXD stays).
                let skip_op = const_val == 0 && matches!(next_op, 0x60 | 0x64);
                if skip_op {
                    // no-op
                } else if reg != RAX {
                    self.emit_alu_reg_imm(ext, reg, const_val, false);
                } else if (-128..=127).contains(&const_val) {
                    // Cast: x86-64 immediate encoding
                    self.buf.emit(&[0x83, 0xC0 | (ext << 3), const_val as u8]); // op EAX, imm8
                } else {
                    self.buf.emit(rax_imm32_opcode);
                    self.buf.emit(&const_val.to_le_bytes());
                }
                // A non-negative AND mask clears bit 31, and the 32-bit AND
                // already zero-extended into the register -- which for a value
                // with bit 31 clear IS the sign extension. Only a negative mask
                // can leave bit 31 set and needs the MOVSXD. (`x & 0xFF`,
                // `x & 0x7FFFFFFF`, `hash & (n - 1)`-style masks are the common
                // case.) Every other op re-extends.
                if next_op != 0x7e || const_val < 0 {
                    self.emit_movsxd_self(reg);
                }
                self.push_work_reg(reg);
                true
            }
            // ishl / ishr / iushr: left SHIFT const_val (constant count)
            0x78 | 0x7a | 0x7c if (0..=31).contains(&const_val) => {
                self.pc_to_native[next_op_pc] = self.buf.pos() as i32; // Cast: x86-64 immediate encoding
                let reg = self.pop_to_work_reg();
                let ext = match next_op {
                    0x78 => 4, // SHL
                    0x7a => 7, // SAR
                    _ => 5,    // 0x7c: SHR
                };
                // Cast: 0..=31 checked above
                self.emit_shift_reg_imm(ext, reg, const_val as u8, false);
                // A logical shift by 1..=31 clears bit 31, so the zero-extension
                // the 32-bit SHR performed is already the sign extension. A
                // shift by 0 leaves bit 31 as it was (`-1 >>> 0 == -1`) and
                // still needs the MOVSXD — see the generic `iushr` arm. SHL and
                // SAR always re-extend.
                if next_op != 0x7c || const_val == 0 {
                    self.emit_movsxd_self(reg);
                }
                self.push_work_reg(reg);
                true
            }
            // lshl / lshr / lushr: a LONG left operand shifted by this int
            // constant. JVMS uses the count's low six bits; a zero count is
            // the value itself. One `shl/sar/shr r64, imm8` in the work
            // register, where the unfused walk materialised the count, parked
            // it, reloaded it into RCX and shifted by CL (round 9 wave 7).
            0x79 | 0x7b | 0x7d if operand_fold_enabled() => {
                self.pc_to_native[next_op_pc] = self.buf.pos() as i32; // Cast: x86-64 immediate encoding
                let reg = self.pop_to_work_reg();
                // Cast: masked to 0..=63 first.
                let count = (const_val & 0x3f) as u8;
                if count != 0 {
                    let ext = match next_op {
                        0x79 => 4, // SHL
                        0x7b => 7, // SAR
                        _ => 5,    // 0x7d: SHR
                    };
                    self.emit_shift_reg_imm(ext, reg, count, true);
                }
                self.push_work_reg(reg);
                true
            }
            _ => false,
        }
    }

    /// Does `emit_imul_const` fall through to a real IMUL for `val` (rather
    /// than a zero, a move, NEG, ADD, LEA or SHL)?
    fn imul_const_is_generic(val: i32) -> bool {
        // Cast: sign reinterpretation only under the `val > 0` guard.
        let pow2 = val > 0 && (val as u32).is_power_of_two();
        !(pow2 || matches!(val, 0 | 1 | -1 | 3 | 5 | 9))
    }

    /// `IMUL dst32, src32, imm` (`6B /r ib` or `69 /r id`). The upper half of
    /// `dst` is zeroed; an int caller re-sign-extends.
    fn emit_imul_reg_src_imm(&mut self, dst: u8, src: AluSrc, imm: i32) {
        self.emit_rex_reg_rm(false, dst, src.rm());
        if let Ok(imm8) = i8::try_from(imm) {
            self.buf.emit_byte(0x6B);
            self.emit_modrm_src(dst, src);
            self.buf.emit(&imm8.to_le_bytes());
        } else {
            self.buf.emit_byte(0x69);
            self.emit_modrm_src(dst, src);
            self.buf.emit(&imm.to_le_bytes());
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
        //
        // A FORWARD compare whose value1 is the whole stack pops it BEFORE the
        // flush (the `if_icmp*` arm's `pair_forward` rule): the CMP consumes
        // it, nothing is live across the edge, and the flush would only have
        // parked a scratch-cached value1 in a frame word nobody reads.
        let lone_val1 = if operand_fold_enabled() && target_pc > next_op_pc && self.stack.len() == 1
        {
            Some(self.pop_stack())
        } else {
            None
        };
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
        //
        // Both directions, the rule every `op_control.rs` branch arm follows
        // since round 9: a loop header with a live operand stack is
        // canonicalised when the walk reaches it, so a fused BACK edge must
        // arrive with the same `Frame(base + i*8)` layout, not with whatever
        // homes the body left (NOTES-baseline cross-lane request 2).
        if self.stack.len() > 1 {
            self.canonicalize_stack();
        }

        // Pop value1 (already on stack before the constant was pushed)
        let val1 = match lone_val1 {
            Some(slot) => slot,
            None => self.pop_stack(),
        };
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
                // SHL EAX, k — exact for EVERY input, negative ones
                // included: `x * 2^k` mod 2^32 is `x << k`, which is Java's
                // wrapping `imul`. The MOVSXD below restores the
                // sign-extended int convention.
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
            // An operand pushed BEFORE the folded run by `iload local` is a
            // `CalleeSaved(reg)` entry that reads the local's OLD value; the
            // `istore` arms materialise such entries before writing the
            // register, and so must this (it replaces an `istore`). A no-op
            // when nothing on the stack names the register — every javac
            // statement. RAX is not touched.
            self.invalidate_callee_saved(reg);
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
                                                                       // In place on a cached operand (see the int arms): `lload i;
                                                                       // lconst_1; ladd` is javac's `i++` on a `long`.
                let reg = self.pop_to_work_reg();
                if const_val != 0 {
                    self.emit_alu_long_imm(0, reg, const_val); // ADD reg, imm
                }
                self.push_work_reg(reg);
                true
            }
            // lsub: left - const (imm32 range only)
            0x65 if fits_i32 => {
                self.pc_to_native[next_op_pc] = self.buf.pos() as i32; // Cast: x86-64 immediate encoding
                let reg = self.pop_to_work_reg();
                if const_val != 0 {
                    self.emit_alu_long_imm(5, reg, const_val); // SUB reg, imm
                }
                self.push_work_reg(reg);
                true
            }
            _ => false,
        }
    }

    /// Group-1 ALU op `/ext` on `reg` with a sign-extended immediate, 64-bit:
    /// `REX.W 83 /ext ib` when `imm` fits `i8` (4 bytes), else
    /// `REX.W 81 /ext id` (7 bytes). `imm` must fit `i32` (callers check; one
    /// that does not emits nothing and fails the compile rather than
    /// truncating). `ext`: 0 = ADD, 5 = SUB. For RAX the bytes are the ones
    /// the RAX-only version of this emitter produced.
    fn emit_alu_long_imm(&mut self, ext: u8, reg: u8, imm: i64) {
        match i32::try_from(imm) {
            Ok(imm32) => self.emit_alu_reg_imm(ext, reg, imm32, true),
            Err(_) => self.fail("singlepass-codegen/long-alu-immediate-out-of-range"),
        }
    }

    /// `RCX = n < 0 ? 2^k - 1 : 0` for the dividend in RAX (1 <= k <= 63) —
    /// the rounding bias of a signed division by `2^k`. `SAR 63` makes the
    /// sign mask and `SHR 64-k` keeps its low `k` bits; for `k == 1` the
    /// `SHR 63` alone is the sign bit, one instruction fewer. RAX is kept.
    fn emit_pow2_bias64(&mut self, k: u32) {
        self.rex_w();
        self.buf.emit(&[0x89, 0xC1]); // MOV RCX, RAX
        if k > 1 {
            self.rex_w();
            self.buf.emit(&[0xC1, 0xF9, 0x3F]); // SAR RCX, 63
        }
        self.rex_w();
        self.buf.emit(&[0xC1, 0xE9, (64 - k) as u8]); // SHR RCX, 64-k // Cast: 1..=63
    }

    /// Signed 64-bit division by 2^k rounding toward zero (RAX in/out;
    /// clobbers RCX): `(n + bias) >> k`.
    fn emit_ldiv_pow2(&mut self, divisor: i64) {
        debug_assert!(divisor > 0 && (divisor & (divisor - 1)) == 0);
        let k = divisor.trailing_zeros();
        if k == 0 {
            return; // div by 1 = no-op
        }
        self.emit_pow2_bias64(k);
        self.rex_w();
        self.buf.emit(&[0x01, 0xC8]); // ADD RAX, RCX
        self.rex_w();
        self.buf.emit(&[0xC1, 0xF8, k as u8]); // SAR RAX, k // Cast: x86-64 immediate encoding
    }

    /// Signed 64-bit remainder by 2^k (RAX in/out; clobbers RCX):
    /// `((n + bias) & (2^k - 1)) - bias` — the remainder takes the dividend's
    /// sign, as Java's does (`-5 % 4 == -1`: `(-2 & 3) - 3`). Six instructions
    /// where the `n - ((n + bias) >> k << k)` form took ten.
    fn emit_lrem_pow2(&mut self, divisor: i64) {
        debug_assert!(divisor > 0 && (divisor & (divisor - 1)) == 0);
        let k = divisor.trailing_zeros();
        if k == 0 {
            self.emit_xor_reg_self(RAX); // a % 1 == 0
            return;
        }
        let mask = divisor - 1; // caller guarantees fits i32 (so the imm sign-extends to itself)
        self.emit_pow2_bias64(k);
        self.rex_w();
        self.buf.emit(&[0x01, 0xC8]); // ADD RAX, RCX
        self.rex_w();
        if mask <= 127 {
            self.buf.emit(&[0x83, 0xE0, mask as u8]); // AND RAX, imm8 // Cast: x86-64 immediate encoding
        } else {
            self.buf.emit(&[0x81, 0xE0]); // AND RAX, imm32
            self.buf.emit(&(mask as i32).to_le_bytes()); // Cast: x86-64 immediate encoding
        }
        self.rex_w();
        self.buf.emit(&[0x29, 0xC8]); // SUB RAX, RCX
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
        // quotient = t + (n >>> 63), built in RAX straight from the saved
        // dividend (one MOV fewer than routing the sign bit through RDX).
        // RDX is left holding `t`; RCX still holds `n` for `emit_lrem_magic64`.
        self.rex_w();
        self.buf.emit(&[0x89, 0xC8]); // MOV RAX, RCX
        self.rex_w();
        self.buf.emit(&[0xC1, 0xE8, 0x3F]); // SHR RAX, 63 — sign bit of n
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

    /// `ECX = n < 0 ? 2^k - 1 : 0` for the dividend in EAX (1 <= k <= 31);
    /// the 32-bit twin of [`Self::emit_pow2_bias64`]. EAX is kept.
    fn emit_pow2_bias32(&mut self, k: u32) {
        self.buf.emit(&[0x89, 0xC1]); // MOV ECX, EAX
        if k > 1 {
            self.buf.emit(&[0xC1, 0xF9, 0x1F]); // SAR ECX, 31
        }
        self.buf.emit(&[0xC1, 0xE9, (32 - k) as u8]); // SHR ECX, 32-k // Cast: 1..=31
    }

    /// Signed 32-bit division by 2^k rounding toward zero:
    /// `(n + bias) >> k`, sign-extended to RAX. Clobbers RCX.
    fn emit_idiv_pow2(&mut self, divisor: i32) {
        debug_assert!(divisor > 0 && (divisor & (divisor - 1)) == 0);
        let k = divisor.trailing_zeros();
        if k == 0 {
            return; // div by 1 = no-op
        }
        self.emit_pow2_bias32(k);
        self.buf.emit(&[0x01, 0xC8]); // ADD EAX, ECX
        self.buf.emit(&[0xC1, 0xF8, k as u8]); // SAR EAX, k // Cast: x86-64 immediate encoding
        self.rex_w();
        self.buf.emit(&[0x63, 0xC0]); // MOVSXD RAX, EAX
    }

    /// Emit optimized signed remainder by power-of-2 constant.
    /// Result: EAX = EAX % 2^k, sign-extended to RAX. Clobbers RCX.
    /// `((n + bias) & (2^k - 1)) - bias` (see [`Self::emit_lrem_pow2`]).
    fn emit_irem_pow2(&mut self, divisor: i32) {
        debug_assert!(divisor > 0 && (divisor & (divisor - 1)) == 0);
        let k = divisor.trailing_zeros();
        if k == 0 {
            // a % 1 == 0
            self.buf.emit(&[0x31, 0xC0]); // XOR EAX, EAX
            return;
        }
        let mask = divisor - 1;
        self.emit_pow2_bias32(k);
        self.buf.emit(&[0x01, 0xC8]); // ADD EAX, ECX
        if mask <= 127 {
            self.buf.emit(&[0x83, 0xE0, mask as u8]); // AND EAX, imm8 // Cast: x86-64 immediate encoding
        } else {
            self.buf.emit_byte(0x25); // AND EAX, imm32
            self.buf.emit(&mask.to_le_bytes());
        }
        self.buf.emit(&[0x29, 0xC8]); // SUB EAX, ECX
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
    ///     must return 0.
    ///
    /// The second guard tests the **divisor alone** (`CMP r, -1`), the way
    /// HotSpot does: for a divisor of -1 the quotient is `-dividend` with
    /// wrap-around (`NEG` of MIN is MIN, which is exactly the JVMS answer) and
    /// the remainder is always 0, so no IDIV is needed for ANY dividend. The
    /// sequence this replaced compared the dividend against MIN first, which
    /// for the 64-bit forms meant a 10-byte `MOV R10, imm64` (clobbering R10,
    /// which the bounds-check and SIMD paths use as a scratch) plus a
    /// `CMP RAX, R10` and a second branch on every `ldiv`/`lrem`.
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

        // -------- Guard 2: divisor == -1 --------
        // IDIV raises #DE for MIN / -1. Rather than detect that one pair, take
        // every divisor of -1 off the IDIV path: `x / -1` is `-x` with
        // wrap-around (so MIN / -1 == MIN, as JVMS requires) and `x % -1` is 0.
        //
        //     CMP   divisor, -1
        //     JNE   :do_div
        //     NEG   dividend            ; idiv/ldiv   (+ MOVSXD for 32-bit)
        //     XOR   EAX, EAX            ; irem/lrem
        //     JMP   :after_div
        //   :do_div
        //     CDQ / CQO
        //     IDIV  ECX / RCX
        //     <move result into RAX, sign-extending for 32-bit>
        //   :after_div
        if is_64bit {
            // CMP RCX, -1  (48 83 F9 FF)  — imm8 sign-extended to 64
            self.buf.emit(&[0x48, 0x83, 0xF9, 0xFF]);
        } else {
            // CMP ECX, -1  (83 F9 FF)     — imm8 sign-extended to 32
            self.buf.emit(&[0x83, 0xF9, 0xFF]);
        }
        // JNE rel8 → :do_div (patched once the short block's size is known)
        self.buf.emit(&[0x75, 0x00]);
        let jne_patch = self.buf.pos() - 1;

        // Materialise the divisor == -1 result.
        if is_rem {
            // result = 0. XOR EAX, EAX zeroes all of RAX for both widths.
            self.buf.emit(&[0x31, 0xC0]);
        } else if is_64bit {
            // NEG RAX  (48 F7 D8) — LONG_MIN stays LONG_MIN.
            self.buf.emit(&[0x48, 0xF7, 0xD8]);
        } else {
            // NEG EAX  (F7 D8) — INT_MIN stays INT_MIN — then re-establish the
            // sign-extended int convention the IDIV path below also produces.
            self.buf.emit(&[0xF7, 0xD8]);
            // MOVSXD RAX, EAX  (48 63 C0)
            self.buf.emit(&[0x48, 0x63, 0xC0]);
        }
        // JMP rel8 → :after_div
        self.buf.emit(&[0xEB, 0x00]);
        let jmp_after_patch = self.buf.pos() - 1;

        // :do_div — patch the JNE to here
        let do_div_off = self.buf.pos();
        // Widening: usize/u32 offset -> i64 (no truncation; for rel/displacement math)
        let rel1 = (do_div_off as i64) - (jne_patch as i64 + 1);
        // A rel8 displacement that does not fit in an i8 would silently
        // miscompile in release builds. `emit_safe_idiv` cannot signal a failure
        // (it returns `()`), so honor the no-panic contract: mark the buffer
        // overflowed (the driver's `if buf.overflowed() { return None; }`
        // discards the half-emitted method and falls back to the interpreter)
        // instead of asserting. The intervening block is fixed-size and small,
        // so this can only fire on a genuine codegen bug. The range check is
        // `patch_rel8_or_bail`'s (which marks the buffer on a miss), not a
        // second hand-rolled copy of it.
        Self::patch_rel8_or_bail(&mut self.buf, jne_patch, rel1);
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

    /// `flush_xmm0_slots` for a caller that has ALREADY popped `operands` and
    /// has yet to read them.
    ///
    /// `pop_stack` returns a popped scratch XMM (XMM2..) to the pool the moment
    /// no remaining entry names it, and `flush_xmm0_slots` relocates a
    /// still-live XMM0 value into the FIRST free scratch XMM. Run between the
    /// pops and the operand loads — which is where every FP binop, `fcmp` and
    /// the `dmul`-by-2.0 reduction ran it — that free register can be the one
    /// an operand is still sitting in: with `[Xmm(0)=A, Xmm(2)=B, Frame=C]`,
    /// `dadd` pops C and B (XMM2 freed), the flush writes A into XMM2, and the
    /// add computes `A + C`. An XMM0 value sits below a scratch one after any
    /// stack shuffle (`dup_x1`/`dup_x2`/`swap` on floats, `dup2_x1`/`dup2_x2` on
    /// doubles). Holding the operands' scratch registers across the flush
    /// closes it without the extra `MOVSD` that flushing before the pops would
    /// cost whenever an operand is itself in XMM0.
    ///
    /// The hold is released as soon as the flush returns: the callers read the
    /// operands next and allocate nothing in between.
    ///
    /// # The operands' FRAME words are held too (round 9 wave 6, x64misc6)
    ///
    /// The same hazard exists one level down. When every scratch XMM is busy,
    /// `flush_xmm0_slots` spills the XMM0 value to a fresh frame word from
    /// `reserve_spill_slots` — and `pop_stack` has just handed the popped
    /// operands' own words back to the spill cursor when they were the topmost
    /// ones. The spill then lands ON an operand the caller has not read yet.
    /// `s0*s1 + (s2*s3 + (s4*s5 + (s6*s7 + (s8*s9 + (s10*s11 + s12*s13)))))`
    /// over `double` statics parks five products (XMM2..XMM5 on Win64, then
    /// XMM0); the `dmul` of `s10*s11` pops two frame words, the flush spills
    /// the fifth product over `s10`'s, and the multiply reads it back as its
    /// left operand. The w5b binary returned 16390.0 where Java says 504.0
    /// (`FpNestProbe`, NOTES-w6-x64misc6.md). When the flush is about to take
    /// the frame route, the cursor is first raised past every popped `Frame`
    /// operand, so the reservation cannot be one of their words. That can only
    /// DELAY a reclaim (the words are free again at the next `reset_spills`),
    /// the trade `pop_stack`'s own live-slot scan documents.
    pub(super) fn flush_xmm0_slots_keeping(&mut self, operands: &[StackSlot]) {
        let mut held = 0u8;
        for slot in operands {
            if let StackSlot::Xmm(x) = *slot {
                if let Some(i) = SCRATCH_XMMS.iter().position(|&r| r == x) {
                    let bit = 1u8 << i;
                    if self.scratch_xmm_in_use & bit == 0 {
                        self.scratch_xmm_in_use |= bit;
                        held |= bit;
                    }
                }
            }
        }
        let xmm0_live = self.stack.iter().any(|s| matches!(s, StackSlot::Xmm(0)));
        let no_scratch_xmm =
            (0..SCRATCH_XMMS.len()).all(|i| self.scratch_xmm_in_use & (1u8 << i) != 0);
        if xmm0_live && no_scratch_xmm {
            for slot in operands {
                if let StackSlot::Frame(off) = *slot {
                    if let Some(end) = off.checked_add(8) {
                        if end > self.next_spill_offset {
                            self.next_spill_offset = end;
                        }
                    }
                }
            }
        }
        self.flush_xmm0_slots();
        self.scratch_xmm_in_use &= !held;
    }

    /// Bring an FP binop's RIGHT operand into a register the SSE op can name
    /// as its r/m operand, and return that register.
    ///
    /// A scratch or local-home XMM (`Xmm(n)`, `n >= 2`) is used where it is —
    /// no copy, no GPR crossing (the shape `emit_fcmp` already had). `Xmm(0)`
    /// (and, defensively, `Xmm(1)`) is copied into XMM1 with `MOVSD`/`MOVSS`
    /// because the left operand is about to be loaded into XMM0. Anything else
    /// goes through RCX: `MOVQ`/`MOVD XMM1, RCX`.
    ///
    /// Must run BEFORE `fp_binop_lhs_to_xmm0`: the left load overwrites XMM0.
    fn fp_binop_rhs_xmm(&mut self, slot: StackSlot, is_double: bool) -> u8 {
        match slot {
            StackSlot::Xmm(n) if n >= 2 => n,
            StackSlot::Xmm(n) => {
                if n != 1 {
                    if is_double {
                        self.emit_movsd_xmm_xmm(1, n);
                    } else {
                        self.emit_movss_xmm_xmm(1, n);
                    }
                }
                1
            }
            _ => {
                self.load_slot_to_reg(RCX, slot);
                if is_double {
                    self.buf.emit(&[0x66, 0x48, 0x0F, 0x6E, 0xC9]); // MOVQ XMM1, RCX
                } else {
                    self.buf.emit(&[0x66, 0x0F, 0x6E, 0xC9]); // MOVD XMM1, ECX
                }
                1
            }
        }
    }

    /// Bring an FP binop's LEFT operand into XMM0 (a no-op when it is already
    /// there). Register-to-register when it is XMM-resident; otherwise through
    /// RAX.
    fn fp_binop_lhs_to_xmm0(&mut self, slot: StackSlot, is_double: bool) {
        match slot {
            StackSlot::Xmm(0) => {}
            StackSlot::Xmm(n) => {
                if is_double {
                    self.emit_movsd_xmm_xmm(0, n);
                } else {
                    self.emit_movss_xmm_xmm(0, n);
                }
            }
            _ => {
                self.load_slot_to_reg(RAX, slot);
                if is_double {
                    self.buf.emit(&[0x66, 0x48, 0x0F, 0x6E, 0xC0]); // MOVQ XMM0, RAX
                } else {
                    self.buf.emit(&[0x66, 0x0F, 0x6E, 0xC0]); // MOVD XMM0, EAX
                }
            }
        }
    }

    /// Pop the floating-point operand of a conversion (`f2i` `f2l` `f2d`
    /// `d2i` `d2l` `d2f`) into XMM0, where the `CVT*` and the NaN fixup read
    /// it.
    ///
    /// The operand is read where it is: nothing to do when it already IS
    /// XMM0 (a `dmul` result), one `MOVSD`/`MOVSS` from any other XMM, and
    /// the old `MOVQ`/`MOVD` through RAX only for a frame or GPR operand. The
    /// arms used to flush first — which, for an `Xmm(0)` operand, copied it
    /// into a scratch XMM — then pop it through RAX and copy it back:
    /// `movsd xmm2, xmm0; movq rax, xmm2; movq xmm0, rax` in front of every
    /// `(int) (x * s)`. Under `CRATONVM_JIT_NO_OPERAND_FOLD=1` the old bytes.
    ///
    /// The flush runs after the pop with the operand held
    /// (`flush_xmm0_slots_keeping`), so a deeper XMM0 value is relocated
    /// without touching the operand's register or frame word.
    pub(super) fn pop_fp_operand_to_xmm0(&mut self, is_double: bool) {
        if !operand_fold_enabled() {
            self.flush_xmm0_slots();
            self.pop_to_rax();
            if is_double {
                self.buf.emit(&[0x66, 0x48, 0x0F, 0x6E, 0xC0]); // MOVQ XMM0, RAX
            } else {
                self.buf.emit(&[0x66, 0x0F, 0x6E, 0xC0]); // MOVD XMM0, EAX
            }
            return;
        }
        let slot = self.pop_stack();
        self.flush_xmm0_slots_keeping(&[slot]);
        self.fp_binop_lhs_to_xmm0(slot, is_double);
    }

    /// `i2f` `i2d` `l2f` `l2d`: pop the integer operand and convert it into
    /// XMM0 (`CVTSI2SS`/`CVTSI2SD`, `prefix` `F3`/`F2`, `wide` for a `long`),
    /// reading the operand straight from its register or frame word
    /// (`cvtsi2sd xmm0, r13d`, `cvtsi2sd xmm0, dword [rbp-x]`) instead of
    /// copying it into RAX first. The caller pushes `Xmm(0)`. Under
    /// `CRATONVM_JIT_NO_OPERAND_FOLD=1` the old `pop_to_rax` bytes.
    pub(super) fn emit_int_to_fp_xmm0(&mut self, prefix: u8, wide: bool) {
        // Flushed BEFORE the pop, as the arms always did: an integer operand
        // is never XMM0-homed, and a flush that spills can then never be
        // handed the popped operand's frame word.
        self.flush_xmm0_slots();
        if !operand_fold_enabled() {
            self.pop_to_rax();
            self.buf.emit_byte(prefix);
            if wide {
                self.buf.emit_byte(0x48);
            }
            self.buf.emit(&[0x0F, 0x2A, 0xC0]); // CVTSI2Sx XMM0, EAX/RAX
            return;
        }
        let slot = self.pop_stack();
        let src = self.alu_src_for(slot, RAX);
        // Mandatory prefix, then REX (W for a long source, B for R8..R15),
        // then `0F 2A /r` with ModRM.reg = XMM0.
        self.buf.emit_byte(prefix);
        self.emit_rex_reg_rm(wide, 0, src.rm());
        self.buf.emit(&[0x0F, 0x2A]);
        self.emit_modrm_src(0, src);
    }

    /// `<prefix> [REX.B] 0F <sse_op> ModRM(XMM0, rhs)` — `XMM0 = XMM0 op rhs`.
    /// `prefix` is `F2` (scalar double) or `F3` (scalar single); the mandatory
    /// prefix precedes REX.
    fn emit_sse_scalar_op_xmm0(&mut self, prefix: u8, sse_op: u8, rhs: u8) {
        self.buf.emit_byte(prefix);
        if rhs >= 8 {
            self.buf.emit_byte(0x41); // REX.B
        }
        self.buf.emit(&[0x0F, sse_op, 0xC0 | (rhs & 7)]);
    }

    /// SSE float binary op: pop two f32 values, apply SSE scalar op, push result.
    /// `sse_op`: 0x58=ADD, 0x59=MUL, 0x5C=SUB, 0x5E=DIV
    ///
    /// XMM-resident operands are used in place (`fp_binop_rhs_xmm`,
    /// `fp_binop_lhs_to_xmm0`); only frame/GPR operands cross through RAX/RCX.
    /// The result still leaves through `MOVD EAX, XMM0` (a clean zero-extended
    /// GPR value): a float left in XMM0 after `MOVSS`-merging operands would
    /// carry junk in bits 32..63 of any later `MOVQ` read, so only the double
    /// op keeps its result in XMM0
    /// (`single-pass-fp-binops-round-trip-through-gpr-20260918.md`).
    pub(super) fn emit_float_binop(&mut self, sse_op: u8) {
        let slot2 = self.pop_stack(); // value2 (top)
        let slot1 = self.pop_stack(); // value1 (deeper)

        // See emit_double_binop: relocate any live XMM0 operand still on the
        // remaining stack before this op clobbers XMM0/XMM1 as scratch.
        self.flush_xmm0_slots_keeping(&[slot1, slot2]);

        let rhs = self.fp_binop_rhs_xmm(slot2, false);
        self.fp_binop_lhs_to_xmm0(slot1, false);
        self.emit_sse_scalar_op_xmm0(0xF3, sse_op, rhs);
        // MOVD EAX, XMM0
        self.buf.emit(&[0x66, 0x0F, 0x7E, 0xC0]);
        self.push_from_rax();
    }

    /// SSE double binary op: pop two f64 values, apply SSE scalar op, push result.
    /// `sse_op`: 0x58=ADD, 0x59=MUL, 0x5C=SUB, 0x5E=DIV
    ///
    /// XMM-resident operands are used in place (a scratch or local-home XMM as
    /// the r/m operand, XMM0 as the left operand); the result is pushed as
    /// `Xmm(0)`. Only frame/GPR operands cross through RAX/RCX.
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
        self.flush_xmm0_slots_keeping(&[slot1, slot2]);

        // Operands in place (no GPR crossing for an XMM-resident one), and the
        // result STAYS in XMM0 — pushed as `Xmm(0)`, as `i2d`, the array loads
        // and the `dmul`-by-2.0 reduction already do — so a `dmul; dadd` chain
        // never leaves the XMM domain. The flush above guarantees no other
        // stack entry names XMM0, and every XMM0 writer flushes before it
        // writes (`flush_xmm0_slots` / `_keeping`).
        let rhs = self.fp_binop_rhs_xmm(slot2, true);
        self.fp_binop_lhs_to_xmm0(slot1, true);
        self.emit_sse_scalar_op_xmm0(0xF2, sse_op, rhs);
        self.stack_push(StackSlot::Xmm(0), false);
    }

    /// Float/double compare: pop two values, produce -1/0/1.
    /// `is_double`: true for dcmp*, false for fcmp*.
    /// `nan_positive`: true for *cmpg (NaN→1), false for *cmpl (NaN→-1).
    pub(super) fn emit_fcmp(&mut self, is_double: bool, nan_positive: bool) {
        let slot2 = self.pop_stack();
        let slot1 = self.pop_stack();
        self.flush_xmm0_slots_keeping(&[slot1, slot2]);

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
        } else {
            // *cmpl: NaN → -1 (SETB naturally includes NaN)
            // SETA AL
            self.buf.emit(&[0x0F, 0x97, 0xC0]);
            // SETB CL
            self.buf.emit(&[0x0F, 0x92, 0xC1]);
        }

        // AL and CL are each 0 or 1 (and never both 1), so their difference is
        // -1/0/1 and fits a byte: one byte SUB and one sign extension instead
        // of two MOVZX, a 32-bit SUB and a MOVSXD.
        self.buf.emit(&[0x28, 0xC8]); // SUB AL, CL
        self.buf.emit(&[0x48, 0x0F, 0xBE, 0xC0]); // MOVSX RAX, AL
        self.push_from_rax();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::x64::{ARG_REGS, RAX, RCX};

    /// A leaf `extern "C" fn(i64, i64) -> i64` whose body sees its first
    /// argument in RAX and its second in RCX, and returns RAX. Emitted after
    /// the prologue `Compiler::new` writes, then flipped to RX and executed —
    /// the same shape as `the_inline_g1_barrier_filter_...` in `x64/tests.rs`.
    #[cfg(target_arch = "x86_64")]
    struct Leaf {
        compiler: Compiler,
        entry: usize,
    }

    #[cfg(target_arch = "x86_64")]
    impl Leaf {
        fn new(body: impl FnOnce(&mut Compiler)) -> Leaf {
            let mut compiler = super::super::emit::tests::test_compiler();
            let entry = compiler.buf.pos();
            // Win64: ARG_REGS[0] is RCX, so read it before RCX is overwritten.
            compiler.emit_mov_r64_r64(RAX, ARG_REGS[0]);
            compiler.emit_mov_r64_r64(RCX, ARG_REGS[1]);
            body(&mut compiler);
            compiler.emit_ret();
            assert!(!compiler.buf.overflowed(), "the leaf must fit and encode");
            crate::platform::make_executable(
                compiler.buf.as_ptr() as *mut u8,
                compiler.buf.capacity(),
            )
            .expect("the test buffer must be flippable to RX");
            Leaf { compiler, entry }
        }

        fn call(&self, a: i64, b: i64) -> i64 {
            // SAFETY: `entry` is an offset inside the live, RX buffer owned by
            // `self.compiler`; the body touches only caller-saved registers
            // (RAX/RCX/RDX, XMM0/XMM1) and no memory, and returns in RAX.
            let f: extern "C" fn(i64, i64) -> i64 =
                unsafe { std::mem::transmute(self.compiler.buf.as_ptr().add(self.entry)) };
            f(a, b)
        }
    }

    /// `emit_safe_idiv` against Rust's wrapping semantics, which are Java's:
    /// `MIN / -1 == MIN`, `MIN % -1 == 0`, and every other divisor of -1 is a
    /// negation — now taken without an IDIV at all.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn safe_idiv_matches_java_semantics_for_every_divisor_class() {
        let div32 = Leaf::new(|c| c.emit_safe_idiv(0, false, false));
        let rem32 = Leaf::new(|c| c.emit_safe_idiv(0, false, true));
        let div64 = Leaf::new(|c| c.emit_safe_idiv(0, true, false));
        let rem64 = Leaf::new(|c| c.emit_safe_idiv(0, true, true));

        let ints = [i32::MIN, i32::MIN + 1, -7, -1, 1, 3, 7, i32::MAX];
        for &a in ints.iter() {
            for &b in ints.iter() {
                let (a64, b64) = (i64::from(a), i64::from(b));
                assert_eq!(
                    div32.call(a64, b64),
                    i64::from(a.wrapping_div(b)),
                    "{a} / {b}"
                );
                assert_eq!(
                    rem32.call(a64, b64),
                    i64::from(a.wrapping_rem(b)),
                    "{a} % {b}"
                );
            }
        }
        let longs = [i64::MIN, i64::MIN + 1, -7, -1, 1, 3, 7, i64::MAX, 1 << 40];
        for &a in longs.iter() {
            for &b in longs.iter() {
                assert_eq!(div64.call(a, b), a.wrapping_div(b), "{a} / {b}");
                assert_eq!(rem64.call(a, b), a.wrapping_rem(b), "{a} % {b}");
            }
        }
    }

    /// `CVTT*` + `emit_fp_to_int_nan_fixup` against Rust's `as` casts, which
    /// saturate and send NaN to 0 exactly as JVMS §2.8.3 requires.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn fp_to_int_fixup_matches_java_semantics() {
        // The source arrives as raw bits in RAX; MOVQ/MOVD it into XMM0.
        let d2i = Leaf::new(|c| {
            c.emit_movq_xmm_from_gpr(0, RAX);
            c.buf.emit(&[0xF2, 0x0F, 0x2C, 0xC0]); // CVTTSD2SI EAX, XMM0
            c.emit_fp_to_int_nan_fixup(true, false);
            c.buf.emit(&[0x48, 0x63, 0xC0]); // MOVSXD RAX, EAX
        });
        let d2l = Leaf::new(|c| {
            c.emit_movq_xmm_from_gpr(0, RAX);
            c.buf.emit(&[0xF2, 0x48, 0x0F, 0x2C, 0xC0]); // CVTTSD2SI RAX, XMM0
            c.emit_fp_to_int_nan_fixup(true, true);
        });
        let f2i = Leaf::new(|c| {
            c.buf.emit(&[0x66, 0x0F, 0x6E, 0xC0]); // MOVD XMM0, EAX
            c.buf.emit(&[0xF3, 0x0F, 0x2C, 0xC0]); // CVTTSS2SI EAX, XMM0
            c.emit_fp_to_int_nan_fixup(false, false);
            c.buf.emit(&[0x48, 0x63, 0xC0]); // MOVSXD RAX, EAX
        });
        let f2l = Leaf::new(|c| {
            c.buf.emit(&[0x66, 0x0F, 0x6E, 0xC0]); // MOVD XMM0, EAX
            c.buf.emit(&[0xF3, 0x48, 0x0F, 0x2C, 0xC0]); // CVTTSS2SI RAX, XMM0
            c.emit_fp_to_int_nan_fixup(false, true);
        });

        let doubles = [
            f64::NAN,
            -f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            0.0,
            -0.0,
            1.5,
            -1.5,
            2147483647.0,
            2147483647.9,
            2147483648.0,
            -2147483648.0,
            -2147483648.9,
            -2147483649.0,
            9.2e18,
            9.3e18,
            -9.223372036854775808e18,
            -9.3e18,
            f64::MAX,
            f64::MIN,
            f64::MIN_POSITIVE,
        ];
        for &x in doubles.iter() {
            // Cast: raw IEEE bits into the argument register.
            let bits = x.to_bits() as i64;
            assert_eq!(d2i.call(bits, 0), i64::from(x as i32), "d2i({x:e})");
            assert_eq!(d2l.call(bits, 0), x as i64, "d2l({x:e})");
            // Cast: f64 -> f32 rounding is the point of this sample set.
            let f = x as f32;
            let fbits = i64::from(f.to_bits());
            assert_eq!(f2i.call(fbits, 0), i64::from(f as i32), "f2i({f:e})");
            assert_eq!(f2l.call(fbits, 0), f as i64, "f2l({f:e})");
        }
    }

    /// The flush a popped-operand FP op runs must not relocate a live XMM0
    /// value INTO the scratch register an operand it has not read yet is in.
    ///
    /// `[Xmm(0)=A, Xmm(2)=B]`: popping B frees XMM2, and a bare
    /// `flush_xmm0_slots` then moves A there — the exact register B is read
    /// from next. `flush_xmm0_slots_keeping(&[B])` must pick another home.
    #[test]
    fn flushing_xmm0_after_a_pop_never_reuses_the_popped_operands_register() {
        let mut c = super::super::emit::tests::test_compiler();
        let b_reg = SCRATCH_XMMS[0];
        c.stack_push(StackSlot::Xmm(0), false);
        c.stack_push(StackSlot::Xmm(b_reg), false);
        // As if `flush_xmm0_slots` had allocated it for B earlier.
        c.scratch_xmm_in_use |= 1;
        let b = c.pop_stack();
        assert!(matches!(b, StackSlot::Xmm(r) if r == b_reg));
        assert_eq!(c.scratch_xmm_in_use & 1, 0, "pop_stack frees B's register");
        c.flush_xmm0_slots_keeping(&[b]);
        match c.stack[0] {
            StackSlot::Xmm(r) => {
                assert_ne!(r, b_reg, "A was relocated into B's register");
                assert_ne!(r, 0, "A must have left XMM0");
            }
            StackSlot::Frame(_) => {}
            other => panic!("A relocated to an unexpected home: {other:?}"),
        }
        assert_eq!(
            c.scratch_xmm_in_use & 1,
            0,
            "the hold on B's register is released once the flush returns"
        );
    }

    /// Every scratch XMM busy, so the flush spills the live XMM0 value to a
    /// FRAME word: that word must not be one a just-popped operand still
    /// occupies (`pop_stack` handed both back to the cursor).
    #[test]
    fn flushing_xmm0_to_the_frame_never_reuses_a_popped_operands_word() {
        let mut c = super::super::emit::tests::test_compiler();
        c.base_spill_offset = 8;
        c.next_spill_offset = 8;
        c.spill_limit_offset = 8 + 16 * 8;
        c.stack.clear();
        c.stack_oop_marks.clear();
        c.stack_push(StackSlot::Xmm(0), false);
        // Cast: at most six scratch XMMs, so the mask fits a u8.
        c.scratch_xmm_in_use = ((1u16 << SCRATCH_XMMS.len()) - 1) as u8;
        let Some(StackSlot::Frame(off_a)) = c.push_stack() else {
            panic!("the test frame has room for the left operand");
        };
        let Some(StackSlot::Frame(off_b)) = c.push_stack() else {
            panic!("the test frame has room for the right operand");
        };
        let b = c.pop_stack();
        let a = c.pop_stack();
        assert_eq!(
            c.next_spill_offset,
            off_a.min(off_b),
            "precondition: the two pops released both operand words"
        );
        c.flush_xmm0_slots_keeping(&[a, b]);
        match c.stack[0] {
            StackSlot::Frame(off) => {
                assert_ne!(off, off_a, "XMM0 was spilled over the left operand");
                assert_ne!(off, off_b, "XMM0 was spilled over the right operand");
            }
            other => panic!("with no scratch XMM free XMM0 must go to the frame: {other:?}"),
        }
    }

    /// The same shape executed: `P` parked in XMM0 below two frame-homed
    /// double operands with every scratch XMM taken, then `dmul`. The answer
    /// is `a * b`, not `P * b` (the w5b binary's `FpNestProbe` answer).
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn a_double_binop_with_no_scratch_xmm_reads_its_own_frame_operands() {
        let parked = 3.0f64;
        for op in [0x58u8, 0x59, 0x5C, 0x5E] {
            let leaf = Leaf::new(|c| {
                open_frame(c, false);
                c.emit_mov_r64_r64(R10, RAX); // a
                                              // Cast: raw IEEE bits into an immediate.
                c.emit_mov_imm64(RAX, parked.to_bits() as i64);
                c.emit_movq_xmm_from_gpr(0, RAX);
                c.stack_push(StackSlot::Xmm(0), false);
                // Cast: at most six scratch XMMs, so the mask fits a u8.
                c.scratch_xmm_in_use = ((1u16 << SCRATCH_XMMS.len()) - 1) as u8;
                place(c, Home::Frame, R10);
                place(c, Home::Frame, RCX);
                c.emit_double_binop(op);
                c.pop_to_rax();
                let _ = c.pop_stack(); // the parked value
                let _ = c.pop_stack(); // the sentinel
                assert!(c.stack.is_empty(), "the model must be balanced");
                c.buf.emit_byte(0xC9); // LEAVE
            });
            for (a, b) in [(5.0f64, 7.0f64), (-1.5, 0.25), (1e300, 1e-300)] {
                let want = match op {
                    0x58 => a + b,
                    0x59 => a * b,
                    0x5C => a - b,
                    _ => a / b,
                };
                // Cast: raw IEEE bits through the integer argument registers.
                let got = leaf.call(a.to_bits() as i64, b.to_bits() as i64);
                // Cast: the result's raw bits back to a double.
                assert_eq!(f64::from_bits(got as u64), want, "op {op:#x} over {a}, {b}");
            }
        }
    }

    /// The power-of-two and magic-number division/remainder sequences against
    /// Rust's `/` and `%` (truncating, remainder takes the dividend's sign —
    /// JVMS `idiv`/`irem`/`ldiv`/`lrem`). Executed, one leaf per divisor.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn constant_division_sequences_match_java_semantics() {
        let longs: [i64; 12] = [
            0,
            1,
            -1,
            5,
            -5,
            7,
            -8,
            1 << 40,
            -(1 << 40) - 3,
            i64::MAX,
            i64::MIN,
            i64::MIN + 1,
        ];
        for d in [1i64, 2, 4, 8, 1 << 20, 1 << 31] {
            let div = Leaf::new(|c| c.emit_ldiv_pow2(d));
            let rem = Leaf::new(|c| c.emit_lrem_pow2(d));
            for &n in longs.iter() {
                assert_eq!(div.call(n, 0), n / d, "{n} / {d}");
                assert_eq!(rem.call(n, 0), n % d, "{n} % {d}");
            }
        }
        for d in [3i64, 7, 10, 1000, 7919] {
            let (magic, shift) = Compiler::magic_signed_div64(d);
            let div = Leaf::new(|c| c.emit_ldiv_magic64(magic, shift));
            let rem = Leaf::new(|c| c.emit_lrem_magic64(magic, shift, d));
            for &n in longs.iter() {
                assert_eq!(div.call(n, 0), n / d, "{n} / {d}");
                assert_eq!(rem.call(n, 0), n % d, "{n} % {d}");
            }
        }
        let ints: [i32; 10] = [0, 1, -1, 5, -5, 7, -8, i32::MAX, i32::MIN, i32::MIN + 1];
        // (Not 1: `emit_idiv_pow2(1)` is a no-op on an already-canonical RAX.)
        for d in [2i32, 4, 16, 1 << 30] {
            let div = Leaf::new(|c| c.emit_idiv_pow2(d));
            let rem = Leaf::new(|c| c.emit_irem_pow2(d));
            for &n in ints.iter() {
                // Dirty upper half: the 32-bit sequences must read EAX only.
                let arg = i64::from(n) ^ (0x5A5A_5A5A_i64 << 32);
                assert_eq!(div.call(arg, 0), i64::from(n / d), "{n} / {d}");
                assert_eq!(rem.call(arg, 0), i64::from(n % d), "{n} % {d}");
            }
        }
    }

    // -------------------------------------------------------------------
    // Operand-stack integer ALU lowering (round 9 wave 5, lane spstack5)
    // -------------------------------------------------------------------
    //
    // Executed: every helper is driven through the real operand-stack model
    // with its operands homed in each place the walk can leave them — a
    // scratch register, a register standing in for a local's home, a frame
    // word (whose slot mirror is live when it was stored last) — with the
    // pure-kernel operand cache on and off, and the answer is compared with
    // Rust's wrapping arithmetic, which is Java's. A value parked UNDER the
    // operands (`SENTINEL` in R11) must come out untouched: the in-place
    // lowering may only overwrite registers it popped.

    use super::super::operand_stack::operand_fold_enabled as fold_on;

    /// Where a test operand lives. `Reg10` stands in for a callee-saved local
    /// home: R10 is caller-saved on both ABIs, so a leaf may clobber it, and
    /// the model does not care which register a `CalleeSaved` entry names.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Home {
        Scratch8,
        Scratch9,
        Reg10,
        Frame,
    }

    const HOMES: [Home; 4] = [Home::Scratch8, Home::Scratch9, Home::Reg10, Home::Frame];

    /// What R11 holds under the operands for the whole body.
    const SENTINEL: i64 = 0x5EED_0000_1234_5678;

    const OPS: [GprAlu; 6] = [
        GprAlu::Add,
        GprAlu::Sub,
        GprAlu::And,
        GprAlu::Or,
        GprAlu::Xor,
        GprAlu::Imul,
    ];

    /// `push rbp; mov rbp, rsp; sub rsp, 0x100`, and a spill window of 16
    /// words ([rbp-8] ..= [rbp-0x80]) for `push_stack`, inside that frame.
    #[cfg(target_arch = "x86_64")]
    fn open_frame(c: &mut Compiler, cache: bool) {
        c.buf.emit(&[
            0x55, 0x48, 0x89, 0xE5, 0x48, 0x81, 0xEC, 0x00, 0x01, 0x00, 0x00,
        ]);
        c.base_spill_offset = 8;
        c.next_spill_offset = 8;
        c.spill_limit_offset = 8 + 16 * 8;
        c.stack.clear();
        c.stack_oop_marks.clear();
        c.kernel_operand_cache = cache;
        c.emit_mov_imm64(R11, SENTINEL);
        c.stack_push(StackSlot::CalleeSaved(R11), false);
    }

    /// Pop the result (RAX) and the sentinel (RCX), fold them into one
    /// answer (`result ^ sentinel`), and tear the frame down.
    #[cfg(target_arch = "x86_64")]
    fn close_frame(c: &mut Compiler) {
        c.pop_to_rax();
        c.pop_to_rcx();
        c.emit_alu_reg_src(GprAlu::Xor, RAX, AluSrc::Reg(RCX), true);
        assert!(c.stack.is_empty(), "the model must be balanced");
        c.buf.emit_byte(0xC9); // LEAVE
    }

    /// Put the value in `src` on the operand stack, homed at `home`.
    #[cfg(target_arch = "x86_64")]
    fn place(c: &mut Compiler, home: Home, src: u8) {
        match home {
            Home::Scratch8 => {
                c.emit_mov_r64_r64(R8, src);
                c.stack_push(StackSlot::Scratch(R8), false);
            }
            Home::Scratch9 => {
                c.emit_mov_r64_r64(R9, src);
                c.stack_push(StackSlot::Scratch(R9), false);
            }
            Home::Reg10 => {
                c.emit_mov_r64_r64(R10, src);
                c.stack_push(StackSlot::CalleeSaved(R10), false);
            }
            Home::Frame => {
                let Some(StackSlot::Frame(off)) = c.push_stack() else {
                    panic!("the test frame has room for every operand");
                };
                c.emit_store_local(off, src);
            }
        }
    }

    /// Java's answer: wrapping 32-bit for an int op (sign-extended, as the
    /// backend holds ints), wrapping 64-bit for a long op.
    fn alu_ref(op: GprAlu, wide: bool, a: i64, b: i64) -> i64 {
        if wide {
            match op {
                GprAlu::Add => a.wrapping_add(b),
                GprAlu::Sub => a.wrapping_sub(b),
                GprAlu::And => a & b,
                GprAlu::Or => a | b,
                GprAlu::Xor => a ^ b,
                GprAlu::Imul => a.wrapping_mul(b),
            }
        } else {
            // Cast: truncation to the int operand is the point.
            let (x, y) = (a as i32, b as i32);
            i64::from(match op {
                GprAlu::Add => x.wrapping_add(y),
                GprAlu::Sub => x.wrapping_sub(y),
                GprAlu::And => x & y,
                GprAlu::Or => x | y,
                GprAlu::Xor => x ^ y,
                GprAlu::Imul => x.wrapping_mul(y),
            })
        }
    }

    const INTS: [i64; 8] = [
        0,
        1,
        -1,
        7,
        -8,
        0x1234_5678,
        i32::MAX as i64,
        i32::MIN as i64,
    ];
    const LONGS: [i64; 8] = [
        0,
        1,
        -1,
        1 << 40,
        -(1 << 33) + 5,
        i64::MAX,
        i64::MIN,
        0x0123_4567_89AB_CDEF,
    ];

    /// `emit_gpr_binop` over every pair of operand homes, both widths, with
    /// the operand cache on and off. `(Reg10, Reg10)` is `iload x; iload x`:
    /// two entries naming one register, the same value.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn gpr_binop_reads_every_operand_home_and_matches_java() {
        for op in OPS {
            for wide in [false, true] {
                for cache in [false, true] {
                    for a_home in HOMES {
                        for b_home in HOMES {
                            let same_reg = a_home == b_home && a_home != Home::Frame;
                            if same_reg && a_home != Home::Reg10 {
                                // Two entries in ONE scratch register is not a
                                // state the walk creates (`dup` gives the copy
                                // its own home).
                                continue;
                            }
                            let leaf = Leaf::new(|c| {
                                open_frame(c, cache);
                                place(c, a_home, RAX);
                                if same_reg {
                                    let top = c.peek_stack();
                                    c.stack_push(top, false);
                                } else {
                                    place(c, b_home, RCX);
                                }
                                assert!(!c.emit_gpr_binop(op, wide, None), "no store to fuse");
                                close_frame(c);
                            });
                            let vals = if wide { LONGS } else { INTS };
                            for &a in vals.iter() {
                                for &b in vals.iter() {
                                    let b = if same_reg { a } else { b };
                                    assert_eq!(
                                        leaf.call(a, b) ^ SENTINEL,
                                        alu_ref(op, wide, a, b),
                                        "{op:?} wide={wide} cache={cache} a@{a_home:?}={a} b@{b_home:?}={b}"
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    /// `x = x OP y` with `x` register-homed: the binop writes the home and the
    /// store is consumed — in place for the left operand always, for the right
    /// one when the op commutes, and (wave 7) for the right operand of a `sub`
    /// through a work register and a move. With a copy of `x`'s OLD value
    /// still on the stack (`x + (x = x OP y)`-shaped), it must NOT fuse, and
    /// the copy must still read the old value.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn a_store_to_the_left_operands_home_is_fused_only_when_nothing_else_reads_it() {
        for op in OPS {
            for wide in [false, true] {
                for cache in [false, true] {
                    for y_home in [Home::Scratch8, Home::Frame] {
                        for local_is_right in [false, true] {
                            for old_copy_below in [false, true] {
                                let mut fused = None;
                                let leaf = Leaf::new(|c| {
                                    open_frame(c, cache);
                                    // x lives in R10, its home.
                                    c.emit_mov_r64_r64(R10, RAX);
                                    if old_copy_below {
                                        c.stack_push(StackSlot::CalleeSaved(R10), false);
                                    }
                                    if local_is_right {
                                        place(c, y_home, RCX);
                                        c.stack_push(StackSlot::CalleeSaved(R10), false);
                                    } else {
                                        c.stack_push(StackSlot::CalleeSaved(R10), false);
                                        place(c, y_home, RCX);
                                    }
                                    let f = c.emit_gpr_binop(op, wide, Some(R10));
                                    if f {
                                        // What `istore x` would have left: the
                                        // new value in the home. Read it back.
                                        c.emit_mov_r64_r64(RAX, R10);
                                    } else {
                                        c.pop_to_rax();
                                    }
                                    if old_copy_below {
                                        // RDX = the copy of x's OLD value.
                                        let old = c.pop_stack();
                                        c.load_slot_to_reg(RDX, old);
                                    }
                                    c.pop_to_rcx(); // the sentinel
                                    c.emit_alu_reg_src(GprAlu::Xor, RAX, AluSrc::Reg(RCX), true);
                                    if old_copy_below {
                                        // Fold the old copy in too: rax ^= rdx.
                                        c.emit_alu_reg_src(
                                            GprAlu::Xor,
                                            RAX,
                                            AluSrc::Reg(RDX),
                                            true,
                                        );
                                    }
                                    assert!(c.stack.is_empty());
                                    c.buf.emit_byte(0xC9); // LEAVE
                                    fused = Some(f);
                                });
                                let fused = fused.expect("the body ran");
                                // Wave 7: the right operand of a `sub` is
                                // fused too (computed elsewhere, then moved
                                // into the home) -- still never with a copy.
                                let expect_fused = fold_on() && !old_copy_below;
                                assert_eq!(
                                    fused, expect_fused,
                                    "{op:?} wide={wide} right={local_is_right} copy={old_copy_below}"
                                );
                                let vals = if wide { LONGS } else { INTS };
                                for &x in vals.iter() {
                                    for &y in vals.iter() {
                                        let want = if local_is_right {
                                            alu_ref(op, wide, y, x)
                                        } else {
                                            alu_ref(op, wide, x, y)
                                        };
                                        let want = if old_copy_below { want ^ x } else { want };
                                        assert_eq!(
                                            leaf.call(x, y) ^ SENTINEL,
                                            want,
                                            "{op:?} wide={wide} cache={cache} y@{y_home:?} \
                                             right={local_is_right} copy={old_copy_below} x={x} y={y}"
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    /// Wave 7: `y = a OP b` with `y` register-homed (RDX here) and NEITHER
    /// operand: the result is computed where 2/3 of `emit_gpr_binop` put it
    /// and moved into the home (`movsxd home, r32` for an int), the store
    /// consumed. The home starts with a stale value whose upper half is
    /// dirty, so a 32-bit write to it would be caught.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn a_store_to_a_home_that_is_not_an_operand_takes_the_result_directly() {
        for op in OPS {
            for wide in [false, true] {
                for cache in [false, true] {
                    for a_home in HOMES {
                        for b_home in HOMES {
                            if a_home == b_home && a_home != Home::Frame {
                                continue;
                            }
                            let mut fused = None;
                            let leaf = Leaf::new(|c| {
                                open_frame(c, cache);
                                c.emit_mov_imm64(RDX, 0x7777_0000_8000_0001);
                                place(c, a_home, RAX);
                                place(c, b_home, RCX);
                                let f = c.emit_gpr_binop(op, wide, Some(RDX));
                                if f {
                                    c.emit_mov_r64_r64(RAX, RDX);
                                } else {
                                    c.pop_to_rax();
                                }
                                c.pop_to_rcx(); // the sentinel
                                c.emit_alu_reg_src(GprAlu::Xor, RAX, AluSrc::Reg(RCX), true);
                                assert!(c.stack.is_empty());
                                c.buf.emit_byte(0xC9); // LEAVE
                                fused = Some(f);
                            });
                            assert_eq!(
                                fused.expect("the body ran"),
                                fold_on(),
                                "{op:?} wide={wide} cache={cache} a@{a_home:?} b@{b_home:?}"
                            );
                            let vals = if wide { LONGS } else { INTS };
                            for &a in vals.iter() {
                                for &b in vals.iter() {
                                    assert_eq!(
                                        leaf.call(a, b) ^ SENTINEL,
                                        alu_ref(op, wide, a, b),
                                        "{op:?} wide={wide} cache={cache}                                          a@{a_home:?}={a} b@{b_home:?}={b}"
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    /// `i2l` / `l2i` (`emit_sext32_top`) from every home. The input's upper
    /// half is dirty on purpose: only the low 32 bits may be read.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn sign_extension_reads_only_the_low_half_from_every_home() {
        for cache in [false, true] {
            for home in HOMES {
                let leaf = Leaf::new(|c| {
                    open_frame(c, cache);
                    place(c, home, RAX);
                    c.emit_sext32_top();
                    close_frame(c);
                });
                for &v in LONGS.iter().chain(INTS.iter()) {
                    // Cast: truncation to the low half is the point.
                    let want = i64::from(v as i32);
                    assert_eq!(
                        leaf.call(v, 0) ^ SENTINEL,
                        want,
                        "cache={cache} {home:?} {v:#x}"
                    );
                }
            }
        }
    }

    /// The constant arms of `try_const_arith_peephole` (int) and
    /// `try_const_arith_peephole_long`, on an operand in each home: in place
    /// on a scratch operand, the historical RAX encodings otherwise.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn constant_alu_arms_compute_in_place_and_match_java() {
        let alu_consts = [0i32, 1, -1, 5, 127, 128, -129, 0x7FFF_FFFF, i32::MIN];
        for cache in [false, true] {
            for home in HOMES {
                let mut cases: Vec<(u8, i32)> = Vec::new();
                for o in [0x60u8, 0x64, 0x7e, 0x80, 0x82] {
                    for k in alu_consts {
                        cases.push((o, k));
                    }
                }
                for o in [0x78u8, 0x7a, 0x7c] {
                    for k in [0i32, 1, 5, 31] {
                        cases.push((o, k));
                    }
                }
                // imul: the three-operand form (wave 7) and, for 3/16/-1, the
                // strength-reduced RAX forms it leaves alone.
                for k in [31i32, -7, 6, 1000, 0x10001, i32::MIN, 3, 16, -1] {
                    cases.push((0x68, k));
                }
                for (opcode, k) in cases {
                    let leaf = Leaf::new(|c| {
                        open_frame(c, cache);
                        c.pc_to_native = vec![0; 2];
                        place(c, home, RAX);
                        assert!(c.try_const_arith_peephole(k, 0, &[opcode], 1, &[false]));
                        close_frame(c);
                    });
                    for &v in INTS.iter() {
                        // Cast: the int operand.
                        let x = v as i32;
                        let want = i64::from(match opcode {
                            0x60 => x.wrapping_add(k),
                            0x64 => x.wrapping_sub(k),
                            0x7e => x & k,
                            0x80 => x | k,
                            0x82 => x ^ k,
                            0x68 => x.wrapping_mul(k),
                            0x78 => x.wrapping_shl(k as u32), // Cast: 0..=31
                            0x7a => x.wrapping_shr(k as u32), // Cast: 0..=31
                            // Cast: logical shift on the unsigned bits.
                            _ => ((x as u32) >> (k as u32)) as i32,
                        });
                        assert_eq!(
                            leaf.call(v, 0) ^ SENTINEL,
                            want,
                            "op={opcode:#x} k={k} cache={cache} {home:?} x={x}"
                        );
                    }
                }
                let mut long_cases: Vec<(u8, i64)> = Vec::new();
                for o in [0x61u8, 0x65] {
                    for k in [0i64, 1, -1, 1000, -129, 0x7FFF_FFFF] {
                        long_cases.push((o, k));
                    }
                }
                // lshl / lshr / lushr by an int constant (wave 7): the count's
                // low six bits, a zero count leaving the value as it is.
                for opcode in [0x79u8, 0x7b, 0x7d] {
                    for k in [0i32, 1, 3, 31, 32, 63, 64, -1, 100] {
                        let leaf = Leaf::new(|c| {
                            open_frame(c, cache);
                            c.pc_to_native = vec![0; 2];
                            place(c, home, RAX);
                            assert_eq!(
                                c.try_const_arith_peephole(k, 0, &[opcode], 1, &[false]),
                                fold_on()
                            );
                            // Under the kill switch nothing is fused and the
                            // operand itself is the answer (checked below).
                            close_frame(c);
                        });
                        // Cast: the count's low six bits, as JVMS lshl/lshr/lushr.
                        let n = (k & 0x3f) as u32;
                        for &v in LONGS.iter() {
                            let want = if !fold_on() {
                                v
                            } else {
                                match opcode {
                                    0x79 => v.wrapping_shl(n),
                                    0x7b => v.wrapping_shr(n),
                                    // Cast: logical shift on the unsigned bits.
                                    _ => ((v as u64) >> n) as i64,
                                }
                            };
                            assert_eq!(
                                leaf.call(v, 0) ^ SENTINEL,
                                want,
                                "op={opcode:#x} k={k} cache={cache} {home:?} v={v:#x}"
                            );
                        }
                    }
                }
                for (opcode, k) in long_cases {
                    let leaf = Leaf::new(|c| {
                        open_frame(c, cache);
                        c.pc_to_native = vec![0; 2];
                        place(c, home, RAX);
                        assert!(c.try_const_arith_peephole_long(k, 0, &[opcode], 1, &[false]));
                        close_frame(c);
                    });
                    for &v in LONGS.iter() {
                        let want = if opcode == 0x61 {
                            v.wrapping_add(k)
                        } else {
                            v.wrapping_sub(k)
                        };
                        assert_eq!(
                            leaf.call(v, 0) ^ SENTINEL,
                            want,
                            "op={opcode:#x} k={k} cache={cache} {home:?} v={v}"
                        );
                    }
                }
            }
        }
    }

    /// The shapes the page asked for, byte for byte, with the cache on:
    /// `s += x` (s in R13, x cached in R8) is ONE `add r13, r8`, and an int
    /// add of a cached operand and a local home is `add r8d, r13d;
    /// movsxd r8, r8d` with the result left in R8 — no RAX/RCX round trip.
    #[test]
    fn the_scalar_loop_shapes_emit_no_round_trip() {
        if !fold_on() {
            return; // the kill switch is set in this process
        }
        let emit = |f: &dyn Fn(&mut Compiler)| {
            let mut c = super::super::emit::tests::test_compiler();
            c.kernel_operand_cache = true;
            c.stack.clear();
            c.stack_oop_marks.clear();
            let start = c.buf.pos();
            f(&mut c);
            (c.buf.as_slice()[start..].to_vec(), c.stack.clone())
        };
        // lload s (R13); <x in R8>; ladd; lstore s
        let (bytes, stack) = emit(&|c: &mut Compiler| {
            c.stack_push(StackSlot::CalleeSaved(R13), false);
            c.stack_push(StackSlot::Scratch(R8), false);
            assert!(c.emit_gpr_binop(GprAlu::Add, true, Some(R13)));
        });
        assert_eq!(bytes, vec![0x4D, 0x03, 0xE8], "add r13, r8");
        assert!(stack.is_empty());
        // <x in R8>; iload s (R13); iadd  -> in place in R8
        let (bytes, stack) = emit(&|c: &mut Compiler| {
            c.stack_push(StackSlot::Scratch(R8), false);
            c.stack_push(StackSlot::CalleeSaved(R13), false);
            assert!(!c.emit_gpr_binop(GprAlu::Add, false, None));
        });
        assert_eq!(
            bytes,
            vec![0x45, 0x03, 0xC5, 0x4D, 0x63, 0xC0],
            "add r8d, r13d; movsxd r8, r8d"
        );
        assert!(matches!(stack.as_slice(), [StackSlot::Scratch(r)] if *r == R8));
        // i2l of a cached operand: one movsxd in place.
        let (bytes, stack) = emit(&|c: &mut Compiler| {
            c.stack_push(StackSlot::Scratch(R9), false);
            c.emit_sext32_top();
        });
        assert_eq!(bytes, vec![0x4D, 0x63, 0xC9], "movsxd r9, r9d");
        assert!(matches!(stack.as_slice(), [StackSlot::Scratch(r)] if *r == R9));
        // A frame operand folds into the instruction as a memory operand.
        let (bytes, _) = emit(&|c: &mut Compiler| {
            c.stack_push(StackSlot::CalleeSaved(R12), false);
            c.stack_push(StackSlot::Frame(0x30), false);
            c.slot_mirror = None;
            assert!(!c.emit_gpr_binop(GprAlu::Sub, true, None));
        });
        // mov r8, r12; sub r8, [rbp-0x30]
        assert_eq!(bytes, vec![0x4D, 0x8B, 0xC4, 0x4C, 0x2B, 0x45, 0xD0]);
    }

    /// Wave 7: `skip_redundant_i2l` consumes the `i2l` after a producer only
    /// where nothing else can arrive at it, and never emits anything.
    #[test]
    fn a_redundant_i2l_is_consumed_only_where_nothing_else_arrives() {
        if !fold_on() {
            return; // the kill switch is set in this process
        }
        let run = |code: &[u8], targets: &[bool], push: bool, eager: Option<usize>| {
            let mut c = super::super::emit::tests::test_compiler();
            c.stack.clear();
            c.stack_oop_marks.clear();
            c.pc_to_native = vec![-1; 4];
            c.deopt_eager_bci = eager;
            if push {
                c.stack_push(StackSlot::Scratch(R8), false);
            }
            let start = c.buf.pos();
            let next = c.skip_redundant_i2l(code, code.len(), 1, targets);
            let native = c.pc_to_native[1];
            // Cast: a test buffer offset, far below i32::MAX.
            (next, c.buf.pos() - start, native == c.buf.pos() as i32)
        };
        // iaload; i2l; ladd
        let code = [0x2eu8, 0x85, 0x61];
        let none = [false; 3];
        assert_eq!(run(&code, &none, true, None), (2, 0, true), "consumed");
        let target = [false, true, false];
        assert_eq!(run(&code, &target, true, None).0, 1, "a branch target");
        assert_eq!(
            run(&[0x2e, 0x00, 0x61], &none, true, None).0,
            1,
            "not an i2l"
        );
        assert_eq!(run(&code, &none, false, None).0, 1, "nothing pushed");
        assert_eq!(
            run(&code, &none, true, Some(1)).0,
            1,
            "a debug deopt trigger"
        );
        assert_eq!(run(&code[..1], &none, true, None).0, 1, "past the end");
    }

    /// `emit_affine_fold` replaces a balanced `iload x ... istore x` run, so
    /// it must honour what the `istore` did: an entry pushed EARLIER by
    /// `iload x` (a `CalleeSaved` copy of the local's register) still reads
    /// the OLD value after the fold writes the register.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn the_affine_fold_keeps_an_earlier_copy_of_the_local() {
        let leaf = Leaf::new(|c| {
            open_frame(c, false);
            c.local_assignments = vec![Some(R10)];
            c.emit_mov_r64_r64(R10, RAX); // x = arg0
            c.stack_push(StackSlot::CalleeSaved(R10), false); // iload x (earlier)
            c.emit_affine_fold(0, 3, 5); // x = x * 3 + 5
            let old = c.pop_stack();
            c.load_slot_to_reg(RDX, old);
            c.emit_mov_r64_r64(RAX, R10);
            c.pop_to_rcx(); // the sentinel
            c.emit_alu_reg_src(GprAlu::Xor, RAX, AluSrc::Reg(RCX), true);
            c.emit_alu_reg_src(GprAlu::Xor, RAX, AluSrc::Reg(RDX), true);
            assert!(c.stack.is_empty());
            c.buf.emit_byte(0xC9); // LEAVE
        });
        for &x in INTS.iter() {
            // Cast: the int local.
            let xi = x as i32;
            let new = i64::from(xi.wrapping_mul(3).wrapping_add(5));
            assert_eq!(leaf.call(x, 0) ^ SENTINEL, new ^ x, "x={x}");
        }
    }

    // -------------------------------------------------------------------
    // Conversions read their operand in place (round 9 wave 6, x64misc6)
    // -------------------------------------------------------------------

    /// `i2f` `i2d` `l2f` `l2d` through `emit_int_to_fp_xmm0`, from every
    /// integer home, against Rust's `as` (round-to-nearest, as `CVTSI2Sx`
    /// under the default MXCSR). The int forms get a dirty upper half: only
    /// the low 32 bits may be read.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn int_to_fp_conversions_read_every_operand_home() {
        for cache in [false, true] {
            for home in HOMES {
                for (prefix, wide) in [(0xF3u8, false), (0xF2, false), (0xF3, true), (0xF2, true)] {
                    let leaf = Leaf::new(|c| {
                        open_frame(c, cache);
                        place(c, home, RAX);
                        c.emit_int_to_fp_xmm0(prefix, wide);
                        c.stack_push(StackSlot::Xmm(0), false);
                        close_frame(c);
                    });
                    let vals = if wide { LONGS } else { INTS };
                    for &v in vals.iter() {
                        let arg = if wide { v } else { v ^ (0x5A5A_5A5A_i64 << 32) };
                        let got = leaf.call(arg, 0) ^ SENTINEL;
                        // Cast: truncation to the int operand is the point.
                        let vi = v as i32;
                        let what = format!(
                            "prefix={prefix:#x} wide={wide} cache={cache} home={home:?} v={v}"
                        );
                        match (prefix, wide) {
                            // Cast: a float result is the low 32 bits.
                            (0xF3, false) => {
                                assert_eq!(got as u32, (vi as f32).to_bits(), "{what}")
                            }
                            // Cast: the double's raw bits.
                            (0xF2, false) => {
                                assert_eq!(got as u64, f64::from(vi).to_bits(), "{what}")
                            }
                            // Cast: a float result is the low 32 bits; `as` rounds to nearest.
                            (0xF3, true) => assert_eq!(got as u32, (v as f32).to_bits(), "{what}"),
                            // Cast: the double's raw bits; `as` rounds to nearest.
                            _ => assert_eq!(got as u64, (v as f64).to_bits(), "{what}"),
                        }
                    }
                }
            }
        }
    }

    /// Where a floating-point test operand lives.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum FpHome {
        /// XMM0 itself (a `dmul` result).
        Xmm0,
        /// The first scratch XMM.
        XmmScratch,
        /// A frame word.
        Frame,
        /// R8, the operand cache's first register.
        Scratch8,
    }

    /// `f2i` `f2l` `f2d` `d2i` `d2l` `d2f` through the real `walk_arith`
    /// arms, from every home, with and without an unrelated double parked in
    /// XMM0 under the operand. Checks the answer against Rust's `as` (which
    /// saturates and sends NaN to 0, JVMS §2.8.3) and, in a second leaf, that
    /// the parked value survived the conversion's use of XMM0.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn fp_conversions_read_every_operand_home_and_keep_a_parked_xmm0() {
        let parked = -12.75f64;
        let doubles = [
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            0.0,
            -0.0,
            1.5,
            -2.5,
            2147483648.0,
            -2147483649.0,
            9.3e18,
            -9.3e18,
            123456.789,
        ];
        for op in [0x8bu8, 0x8c, 0x8d, 0x8e, 0x8f, 0x90] {
            let src_is_double = matches!(op, 0x8e..=0x90);
            for home in [
                FpHome::Xmm0,
                FpHome::XmmScratch,
                FpHome::Frame,
                FpHome::Scratch8,
            ] {
                for with_parked in [false, true] {
                    if with_parked && home == FpHome::Xmm0 {
                        // Two entries naming XMM0 would be one value.
                        continue;
                    }
                    let build = |return_parked: bool| {
                        Leaf::new(|c| {
                            open_frame(c, true);
                            c.emit_mov_r64_r64(R10, RAX); // the operand's bits
                            if with_parked {
                                // Cast: raw IEEE bits into an immediate.
                                c.emit_mov_imm64(RAX, parked.to_bits() as i64);
                                c.emit_movq_xmm_from_gpr(0, RAX);
                                c.stack_push(StackSlot::Xmm(0), false);
                            }
                            match home {
                                FpHome::Xmm0 => {
                                    c.emit_movq_xmm_from_gpr(0, R10);
                                    c.stack_push(StackSlot::Xmm(0), false);
                                }
                                FpHome::XmmScratch => {
                                    c.emit_movq_xmm_from_gpr(SCRATCH_XMMS[0], R10);
                                    c.scratch_xmm_in_use |= 1;
                                    c.stack_push(StackSlot::Xmm(SCRATCH_XMMS[0]), false);
                                }
                                FpHome::Frame => place(c, Home::Frame, R10),
                                FpHome::Scratch8 => place(c, Home::Scratch8, R10),
                            }
                            let mut dead = false;
                            let step = c.walk_arith(&[op], 1, op, 0, &mut dead, &[false]);
                            assert!(
                                matches!(step, super::super::bytecode_walk::WalkStep::Next(1)),
                                "op {op:#x} lowers"
                            );
                            if return_parked {
                                c.pop_to_rcx(); // the result, discarded
                                c.pop_to_rax(); // the parked double
                            } else {
                                c.pop_to_rax(); // the result
                                if with_parked {
                                    let _ = c.pop_stack();
                                }
                            }
                            let _ = c.pop_stack(); // the sentinel
                            assert!(c.stack.is_empty(), "the model must be balanced");
                            c.buf.emit_byte(0xC9); // LEAVE
                        })
                    };
                    let result_leaf = build(false);
                    let parked_leaf = if with_parked { Some(build(true)) } else { None };
                    for &x in doubles.iter() {
                        // Cast: f64 -> f32 rounding is part of the sample set.
                        let f = x as f32;
                        // Cast: raw IEEE bits through the integer argument register.
                        let arg = if src_is_double {
                            x.to_bits() as i64
                        } else {
                            i64::from(f.to_bits())
                        };
                        let got = result_leaf.call(arg, 0);
                        let what = format!("op={op:#x} home={home:?} parked={with_parked} x={x:e}");
                        match op {
                            0x8b => assert_eq!(got, i64::from(f as i32), "{what}"),
                            0x8c => assert_eq!(got, f as i64, "{what}"),
                            0x8d => {
                                // Cast: the double result's raw bits.
                                let d = f64::from_bits(got as u64);
                                let want = f64::from(f);
                                assert!(d == want || (d.is_nan() && want.is_nan()), "{what}: {d}");
                            }
                            0x8e => assert_eq!(got, i64::from(x as i32), "{what}"),
                            0x8f => assert_eq!(got, x as i64, "{what}"),
                            _ => {
                                // Cast: a float result is the low 32 bits.
                                let r = f32::from_bits(got as u32);
                                assert!(r == f || (r.is_nan() && f.is_nan()), "{what}: {r}");
                            }
                        }
                        if let Some(leaf) = &parked_leaf {
                            // Cast: the parked double's raw bits.
                            let p = f64::from_bits(leaf.call(arg, 0) as u64);
                            assert_eq!(p, parked, "{what}: the parked XMM0 value was clobbered");
                        }
                    }
                }
            }
        }
    }
}
