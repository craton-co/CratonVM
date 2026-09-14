// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Raw x86-64 instruction emitters.
//!
//! The bottom layer of the backend: one method per instruction form, each
//! appending encoded bytes to the buffer and deciding nothing. REX/VEX
//! prefixes, ModRM/SIB, the `mov`/`cmp`/`test`/`cmov`/ALU forms, absolute and
//! rel32 calls and jumps, and the two branch-patching primitives.
//!
//! Nothing here reads compiler policy. If a method decides *whether* to emit
//! something, or emits a shape rather than an instruction, it belongs in one of
//! the sibling modules instead — that boundary is what makes this file safe for
//! several lanes to touch at once, which is the whole point of SEAM-01.

use super::*;

impl Compiler {
    // -----------------------------------------------------------------------
    // x86-64 instruction emitters
    // -----------------------------------------------------------------------

    /// REX prefix for 64-bit operand size.
    pub(super) fn rex_w(&mut self) {
        self.buf.emit_byte(0x48);
    }

    /// REX.W prefix with R bit (extended reg in reg field).
    pub(super) fn rex_w_r(&mut self, reg: u8) {
        let r = if reg >= 8 { 0x4C } else { 0x48 };
        self.buf.emit_byte(r);
    }

    /// REX.W prefix with B bit (extended reg in r/m field).
    #[allow(dead_code)]
    fn rex_w_b(&mut self, rm: u8) {
        // 0x48 = REX.W; 0x49 = REX.W + REX.B (needed for r8–r15 in r/m field).
        let b = if rm >= 8 { 0x49 } else { 0x48 };
        self.buf.emit_byte(b);
    }

    /// REX.W prefix with R and B bits.
    #[allow(dead_code)]
    pub(super) fn rex_w_rb(&mut self, reg: u8, rm: u8) {
        let mut rex: u8 = 0x48;
        if reg >= 8 {
            rex |= 0x04;
        } // R bit
        if rm >= 8 {
            rex |= 0x01;
        } // B bit
        self.buf.emit_byte(rex);
    }

    /// ModRM byte: mod=11 (register), reg, r/m
    pub(super) fn modrm_reg(&mut self, reg: u8, rm: u8) {
        self.buf.emit_byte(0xC0 | ((reg & 7) << 3) | (rm & 7));
    }

    /// ModRM byte for [rbp - disp] addressing.
    ///
    /// `disp` is the positive depth-from-RBP (i.e. the actual displacement
    /// is `-disp`). The byte stores `-disp` as i8/i32, so for disp8 we
    /// need `-disp` to fit in i8 (-128..=127), i.e. `disp` in `-127..=128`.
    /// The old check `(-128..=127)` was off-by-one: it wasted 3 bytes on
    /// the common depth-128 spill and would mis-encode disp=-128 as 0x80
    /// garbage (round-8 jit #4).
    ///
    /// That hand-written range test is now [`Disp::encode_for_base`]: it picks
    /// the same disp8/disp32 split, keeps the `mod` field and the emitted
    /// width in lock-step, and applies the RBP rule (no `mod=00` form — a
    /// zero displacement must still emit an explicit `disp8` of 0, which the
    /// `(-127..=128)` range happened to cover only by including 0). The
    /// negation is widened to `i64` first so a `disp` of `i32::MIN` cannot
    /// overflow before it is range-checked.
    ///
    /// A depth that does not fit disp32 has no encoding at all; `mark_overflowed`
    /// is the existing "codegen invariant broken, discard the method" channel
    /// for emitters that cannot return a `Result`.
    pub(super) fn modrm_rbp_disp(&mut self, reg: u8, disp: i32) {
        let Ok(d) = Disp::encode_for_base(-(disp as i64), RBP) else {
            self.buf
                .mark_codegen_unencodable("frame-displacement-unencodable");
            return;
        };
        self.buf.emit_byte(d.modrm(reg, RBP));
        let (bytes, len) = d.bytes();
        self.buf.emit(&bytes[..len]);
    }

    /// MOVQ XMMn, GPR — move 64-bit integer from a GPR into an XMM register.
    pub(super) fn emit_movq_xmm_from_gpr(&mut self, xmm: u8, gpr: u8) {
        // Encoding: 66 REX(W, R if xmm>=8, B if gpr>=8) 0F 6E /r
        let rex_r = if xmm >= 8 { 0x04u8 } else { 0u8 };
        let rex_b = if gpr >= 8 { 0x01u8 } else { 0u8 };
        self.buf.emit_byte(0x66);
        self.buf.emit_byte(0x48 | rex_r | rex_b); // REX.W + optional REX.R/B
        self.buf.emit_byte(0x0F);
        self.buf.emit_byte(0x6E);
        self.buf.emit_byte(0xC0 | ((xmm & 7) << 3) | (gpr & 7)); // ModRM
    }

    /// MOVQ XMMn, RAX — move 64-bit integer in RAX into an XMM register.
    pub(super) fn emit_movq_xmm_from_rax(&mut self, xmm: u8) {
        self.emit_movq_xmm_from_gpr(xmm, RAX);
    }

    /// MOVQ GPR, XMMn — move 64-bit value from XMM register into a GPR.
    pub(super) fn emit_movq_gpr_from_xmm(&mut self, gpr: u8, xmm: u8) {
        // Encoding: 66 REX(W, R if xmm>=8, B if gpr>=8) 0F 7E /r
        let rex_r = if xmm >= 8 { 0x04u8 } else { 0u8 };
        let rex_b = if gpr >= 8 { 0x01u8 } else { 0u8 };
        self.buf.emit_byte(0x66);
        self.buf.emit_byte(0x48 | rex_r | rex_b); // REX.W + optional REX.R/B
        self.buf.emit_byte(0x0F);
        self.buf.emit_byte(0x7E);
        self.buf.emit_byte(0xC0 | ((xmm & 7) << 3) | (gpr & 7)); // ModRM
    }

    /// MOVQ RAX, XMMn — move 64-bit value from XMM register into RAX.
    pub(super) fn emit_movq_rax_from_xmm(&mut self, xmm: u8) {
        self.emit_movq_gpr_from_xmm(RAX, xmm);
    }

    /// MOVQ [rbp - offset], XMMn — direct 64-bit XMM spill to a frame slot.
    ///
    /// Equivalent to (and replaces) the two-instruction sequence
    /// `MOVQ RAX, XMMn ; MOV [rbp-off], RAX` used by the scratch flusher
    /// and similar XMM-spill sites. Saves ~3 bytes per spill and frees
    /// RAX (allowing it to keep holding the function return value
    /// across an epilogue restore).
    ///
    /// Encoding: `66 [REX] 0F D6 /r` — MOVQ r/m64, xmm.
    ///   - REX.W is NOT required (the opcode is 64-bit by definition).
    ///   - REX.R is set when `xmm >= 8`.
    ///   - REX.B is NOT required: r/m base is RBP (5), low 3 bits.
    /// ModRM:
    ///   - disp8 form (mod=01) when `-128 <= -off <= 127`.
    ///   - disp32 form (mod=10) otherwise.
    /// disp is the signed offset from RBP; callers pass `offset` as a
    /// positive frame depth (matching `emit_store_local`'s convention),
    /// so we encode `-offset`.
    ///
    /// Round-9 LOW fix: this used to carry its own copy of the
    /// `(-127..=128)` guard (and before that an off-by-one `(-128..=127)`
    /// one). Both are now [`Disp::encode_for_base`], the single checked
    /// encoder — see `modrm_rbp_disp`. The displacement is resolved *before*
    /// any byte is emitted so an unencodable depth bails without leaving a
    /// truncated instruction behind.
    pub(super) fn emit_movq_mem_rbp_from_xmm(&mut self, offset: i32, xmm: u8) {
        let Ok(d) = Disp::encode_for_base(-(offset as i64), RBP) else {
            self.buf
                .mark_codegen_unencodable("frame-displacement-unencodable");
            return;
        };
        self.buf.emit_byte(0x66);
        if xmm >= 8 {
            // REX.R only (no .W, no .B — RBP is the base, low 3 bits).
            self.buf.emit_byte(0x44);
        }
        self.buf.emit_byte(0x0F);
        self.buf.emit_byte(0xD6);
        // ModRM r/m=101 (RBP), reg=xmm&7.
        self.buf.emit_byte(d.modrm(xmm, RBP));
        let (bytes, len) = d.bytes();
        self.buf.emit(&bytes[..len]);
    }

    /// MOVQ XMMn, [rbp - offset] — direct 64-bit load from a frame slot
    /// into an XMM register. Pair to `emit_movq_mem_rbp_from_xmm` for
    /// the epilogue restore path.
    ///
    /// Encoding: `F3 [REX] 0F 7E /r` — MOVQ xmm, r/m64.
    ///   - F3 is the mandatory prefix that selects MOVQ-from-mem.
    ///   - REX.R is set when `xmm >= 8`.
    ///   - REX.B is NOT required (base is RBP).
    pub(super) fn emit_movq_xmm_from_mem_rbp(&mut self, xmm: u8, offset: i32) {
        // Round-9 LOW fix: see `emit_movq_mem_rbp_from_xmm` — the hand-written
        // `(-127..=128)` guard is now the shared checked encoder.
        let Ok(d) = Disp::encode_for_base(-(offset as i64), RBP) else {
            self.buf
                .mark_codegen_unencodable("frame-displacement-unencodable");
            return;
        };
        self.buf.emit_byte(0xF3);
        if xmm >= 8 {
            self.buf.emit_byte(0x44);
        }
        self.buf.emit_byte(0x0F);
        self.buf.emit_byte(0x7E);
        self.buf.emit_byte(d.modrm(xmm, RBP));
        let (bytes, len) = d.bytes();
        self.buf.emit(&bytes[..len]);
    }

    /// MOVUPS [rbp - offset], XMMn — all 128 bits into a 16-byte frame slot.
    ///
    /// The callee-saved XMM save. Win64 preserves the whole of XMM6-XMM15, so
    /// the 64-bit `emit_movq_mem_rbp_from_xmm` is not a save at all for a
    /// caller that holds a vector there. Encoding: `[REX.R] 0F 11 /r`; no
    /// alignment requirement, so the slot needs no padding.
    pub(super) fn emit_movups_mem_rbp_from_xmm(&mut self, offset: i32, xmm: u8) {
        let Ok(d) = Disp::encode_for_base(-(offset as i64), RBP) else {
            self.buf
                .mark_codegen_unencodable("frame-displacement-unencodable");
            return;
        };
        if xmm >= 8 {
            self.buf.emit_byte(0x44); // REX.R (RBP needs no REX.B)
        }
        self.buf.emit_byte(0x0F);
        self.buf.emit_byte(0x11);
        self.buf.emit_byte(d.modrm(xmm, RBP));
        let (bytes, len) = d.bytes();
        self.buf.emit(&bytes[..len]);
    }

    /// MOVUPS XMMn, [rbp - offset] — the restore paired with
    /// [`Self::emit_movups_mem_rbp_from_xmm`]. Encoding: `[REX.R] 0F 10 /r`.
    pub(super) fn emit_movups_xmm_from_mem_rbp(&mut self, xmm: u8, offset: i32) {
        let Ok(d) = Disp::encode_for_base(-(offset as i64), RBP) else {
            self.buf
                .mark_codegen_unencodable("frame-displacement-unencodable");
            return;
        };
        if xmm >= 8 {
            self.buf.emit_byte(0x44);
        }
        self.buf.emit_byte(0x0F);
        self.buf.emit_byte(0x10);
        self.buf.emit_byte(d.modrm(xmm, RBP));
        let (bytes, len) = d.bytes();
        self.buf.emit(&bytes[..len]);
    }

    /// MOVSD XMMdst, XMMsrc — move scalar double between XMM registers.
    pub(super) fn emit_movsd_xmm_xmm(&mut self, dst: u8, src: u8) {
        // F2 [REX] 0F 10 modrm — MOVSD dst, src
        let rex_r = if dst >= 8 { 0x04u8 } else { 0 };
        let rex_b = if src >= 8 { 0x01u8 } else { 0 };
        self.buf.emit_byte(0xF2);
        if rex_r != 0 || rex_b != 0 {
            self.buf.emit_byte(0x40 | rex_r | rex_b);
        }
        self.buf.emit_byte(0x0F);
        self.buf.emit_byte(0x10);
        self.buf.emit_byte(0xC0 | ((dst & 7) << 3) | (src & 7));
    }

    /// MOVSS XMMdst, XMMsrc — move scalar float between XMM registers.
    pub(super) fn emit_movss_xmm_xmm(&mut self, dst: u8, src: u8) {
        // F3 [REX] 0F 10 modrm — MOVSS dst, src
        let rex_r = if dst >= 8 { 0x04u8 } else { 0 };
        let rex_b = if src >= 8 { 0x01u8 } else { 0 };
        self.buf.emit_byte(0xF3);
        if rex_r != 0 || rex_b != 0 {
            self.buf.emit_byte(0x40 | rex_r | rex_b);
        }
        self.buf.emit_byte(0x0F);
        self.buf.emit_byte(0x10);
        self.buf.emit_byte(0xC0 | ((dst & 7) << 3) | (src & 7));
    }

    /// SQRTSD XMM0, XMM0 — compute double square root in-place.
    /// Encoding: F2 0F 51 C0
    pub(super) fn emit_sqrtsd_xmm0(&mut self) {
        self.buf.emit_byte(0xF2);
        self.buf.emit_byte(0x0F);
        self.buf.emit_byte(0x51);
        self.buf.emit_byte(0xC0); // ModRM: 11 000 000 (XMM0, XMM0)
    }

    /// PXOR XMMn, XMMn — zero an XMM register.
    pub(super) fn emit_pxor_xmm_self(&mut self, xmm: u8) {
        // Encoding: 66 [REX if xmm>=8] 0F EF /r (ModRM: 11 reg reg)
        if xmm >= 8 {
            self.buf.emit_byte(0x66);
            self.buf.emit_byte(0x45); // REX with R and B set for extended XMM
            self.buf.emit_byte(0x0F);
            self.buf.emit_byte(0xEF);
            self.buf.emit_byte(0xC0 | ((xmm & 7) << 3) | (xmm & 7));
        } else {
            self.buf.emit_byte(0x66);
            self.buf.emit_byte(0x0F);
            self.buf.emit_byte(0xEF);
            self.buf.emit_byte(0xC0 | (xmm << 3) | xmm);
        }
    }

    /// MOV dst_r64, src_r64
    pub(super) fn emit_mov_reg_reg(&mut self, dst: u8, src: u8) {
        // Peephole: `mov rN, rN` is a no-op; emit nothing. Common after
        // regalloc when a coalesced live range produces a self-move at a
        // copy point (e.g. prologue param shuffles where the ABI register
        // already matches the assigned local).
        if dst == src {
            return;
        }
        self.rex_w_rb(dst, src);
        self.buf.emit_byte(0x8B); // MOV r64, r/m64
        self.modrm_reg(dst, src);
    }

    /// XOR r32, r32 — zeroes register (32-bit op zero-extends to 64-bit).
    pub(super) fn emit_xor_reg_self(&mut self, reg: u8) {
        if reg >= 8 {
            // REX prefix with R and B bits for extended registers
            self.buf.emit_byte(0x40 | 0x04 | 0x01); // 0x45
        }
        self.buf.emit_byte(0x31); // XOR r/m32, r32
        self.modrm_reg(reg, reg);
    }

    /// JVMS §6.5 `ireturn`: narrow the value in RAX to the method's declared
    /// int-category return type, as if by `value & 1` for `boolean` and by
    /// truncation plus sign/zero extension for `byte`/`char`/`short`.
    ///
    /// `tag` is [`crate::narrowed_int_return_tag`]'s answer; `None` emits
    /// nothing. The results keep this backend's int convention — an `int` is
    /// sign-extended through all 64 bits of RAX — so `B`/`S` sign-extend to 64
    /// and `Z`/`C`, which are never negative, zero-extend.
    pub(super) fn emit_narrow_int_return(&mut self, tag: Option<u8>) {
        match tag {
            // AND EAX, 1 (83 /4 ib) — the 32-bit op zero-extends into RAX.
            Some(b'Z') => self.buf.emit(&[0x83, 0xE0, 0x01]),
            // MOVSX RAX, AL (REX.W 0F BE /r).
            Some(b'B') => self.buf.emit(&[0x48, 0x0F, 0xBE, 0xC0]),
            // MOVZX EAX, AX (0F B7 /r) — zero-extends into RAX.
            Some(b'C') => self.buf.emit(&[0x0F, 0xB7, 0xC0]),
            // MOVSX RAX, AX (REX.W 0F BF /r).
            Some(b'S') => self.buf.emit(&[0x48, 0x0F, 0xBF, 0xC0]),
            _ => {}
        }
    }

    // ── CMOV helpers (round-8 perf, round-7 jit #7) ──────────────────
    //
    // CMOVcc r64, r/m64 lets us implement small-value selects (Math.min,
    // Math.max, ternary `a < b ? x : y`) without a branch. Encoding is
    // `REX.W 0F 4cc /r` where the condition codes match the Jcc family:
    //   0x44 = CMOVE   (ZF=1)        0x45 = CMOVNE
    //   0x4C = CMOVL   (SF≠OF)       0x4D = CMOVGE
    //   0x4E = CMOVLE  (ZF=1 or SF≠OF) 0x4F = CMOVG
    //   0x42 = CMOVB   (CF=1, unsigned <)  0x43 = CMOVAE
    //   0x46 = CMOVBE  (CF=1 or ZF=1)      0x47 = CMOVA
    // These helpers are emit-time primitives; the IR/lower passes have
    // not yet been taught to detect the patterns that should use them.
    //
    // Round-8 Bug 8: the Math.min(I,I)/Math.max(I,I)/(J,J)/(J,J) intrinsics
    // now lower directly to `CMP + CMOVL/CMOVG` (see the
    // `MATH_MIN_INT_INTRINSIC` / `MATH_MAX_INT_INTRINSIC` arms in the
    // invokestatic dispatch). The bytecode-peephole patterns below are
    // still TODO — they catch user-written ternaries that the JIT cannot
    // recognise as Math.min/max:
    //   * `if_icmplt; ldc small; goto K; L: ldc small; K:` → CMOVL
    //   * `if_acmpne L; aconst_null; goto K; L: aload x; K:` → CMOVNE
    // The current emitter performs those selects via compare+conditional
    // jump+move, which mispredicts on hard-to-predict data (e.g. random
    // array element comparisons in sorting kernels).

    /// Emit `CMOVcc dst, src` (64-bit) with the given condition opcode byte
    /// (0x40..0x4F). dst/src are encoded register-direct (mod=11).
    #[allow(dead_code)]
    pub(super) fn emit_cmov_cc_reg_reg(&mut self, cc: u8, dst: u8, src: u8) {
        debug_assert!(
            (0x40..=0x4F).contains(&cc),
            "CMOV cc opcode must be in 0x40..0x4F"
        );
        // REX.W with R (dst extended) and B (src extended).
        let mut rex: u8 = 0x48;
        if dst >= 8 {
            rex |= 0x04;
        }
        if src >= 8 {
            rex |= 0x01;
        }
        self.buf.emit_byte(rex);
        self.buf.emit_byte(0x0F);
        self.buf.emit_byte(cc);
        self.modrm_reg(dst, src);
    }

    /// CMOVL r64, r64 — move src into dst if SF≠OF (signed `<`).
    #[allow(dead_code)]
    fn emit_cmov_l_reg_reg(&mut self, dst: u8, src: u8) {
        self.emit_cmov_cc_reg_reg(0x4C, dst, src);
    }

    /// CMOVG r64, r64 — move src into dst if ZF=0 and SF=OF (signed `>`).
    #[allow(dead_code)]
    fn emit_cmov_g_reg_reg(&mut self, dst: u8, src: u8) {
        self.emit_cmov_cc_reg_reg(0x4F, dst, src);
    }

    /// CMOVLE r64, r64 — move src into dst if ZF=1 or SF≠OF (signed `<=`).
    #[allow(dead_code)]
    fn emit_cmov_le_reg_reg(&mut self, dst: u8, src: u8) {
        self.emit_cmov_cc_reg_reg(0x4E, dst, src);
    }

    /// CMOVGE r64, r64 — move src into dst if SF=OF (signed `>=`).
    #[allow(dead_code)]
    fn emit_cmov_ge_reg_reg(&mut self, dst: u8, src: u8) {
        self.emit_cmov_cc_reg_reg(0x4D, dst, src);
    }

    /// CMOVE r64, r64 — move src into dst if ZF=1 (equal).
    #[allow(dead_code)]
    fn emit_cmov_e_reg_reg(&mut self, dst: u8, src: u8) {
        self.emit_cmov_cc_reg_reg(0x44, dst, src);
    }

    /// CMOVNE r64, r64 — move src into dst if ZF=0 (not equal).
    #[allow(dead_code)]
    fn emit_cmov_ne_reg_reg(&mut self, dst: u8, src: u8) {
        self.emit_cmov_cc_reg_reg(0x45, dst, src);
    }

    /// MOV reg, imm64
    #[allow(dead_code)]
    pub(super) fn emit_mov_imm64(&mut self, reg: u8, imm: i64) {
        // Optimize: if value fits in sign-extended 32 bits, use shorter MOV r/m64, imm32
        if imm == 0 {
            self.emit_xor_reg_self(reg);
            return;
        }
        // Widening: i32 bound -> i64 (range comparison)
        if imm >= i32::MIN as i64 && imm <= i32::MAX as i64 {
            // Widening: always safe
            self.emit_mov_imm32_sx(reg, imm as i32); // Cast: x86-64 immediate encoding
            return;
        }
        self.rex_w_b(reg);
        self.buf.emit_byte(0xB8 + (reg & 7)); // MOV r64, imm64
        self.buf.emit(&imm.to_le_bytes());
    }

    /// MOV `reg`, `jit_getfield`'s third argument for this field.
    ///
    /// The argument is a slot index plus flag bits, and one of those flags —
    /// [`GETFIELD_EXPECT_REFERENCE`](cratonvm_jit_api::GETFIELD_EXPECT_REFERENCE)
    /// — is a safety contract, not an optimisation: without it the helper
    /// returns the payload of whichever `Value` variant the slot holds, and
    /// compiled code, which has already emitted the dereference, follows a
    /// type-punned primitive as a pointer.
    ///
    /// Every `getfield` helper call site in this backend goes through here, so
    /// the flag cannot be forgotten at one arm.
    ///
    /// `bc_pc` is the trapping bytecode index — this method's own for a
    /// top-level `getfield`, the CALLEE's inside a splice; the separation is
    /// `record_npe_trap_site`'s job, not the call site's. It is recorded as an
    /// NPE trap site and its key rides in the same argument, which is what lets
    /// the helper's null arm raise a MESSAGED `NullPointerException`: the
    /// helper has the receiver (null) and the slot index, and neither names the
    /// field or the bci. See `cratonvm_jit_api::GETFIELD_NPE_SITE_SHIFT`.
    ///
    /// The key costs the argument its short imm32 encoding on a primitive load
    /// (a reference load already carried a flag at bit 61 and was imm64
    /// anyway), i.e. three bytes at a HELPER call site — the arm that is
    /// already paying a call. `record_npe_trap_site` answers `0` when the
    /// feature is switched off, and a zero key restores the previous encoding
    /// byte for byte.
    pub(super) fn emit_getfield_index_arg(
        &mut self,
        reg: u8,
        field_index: usize,
        type_tag: u8,
        bc_pc: usize,
    ) {
        let is_ref = type_tag == b'L' || type_tag == b'[';
        let key = crate::x64::inlining::record_npe_trap_site(bc_pc);
        let arg = cratonvm_jit_api::getfield_index_arg(field_index as u32, is_ref, false, key);
        // Cast: the flag bits sit at 61/62, so the value stays positive in i64.
        self.emit_mov_imm64(reg, arg as i64);
    }

    /// MOV reg, imm32 (sign-extended to 64-bit). Uses XOR for zero.
    pub(super) fn emit_mov_imm32_sx(&mut self, reg: u8, imm: i32) {
        if imm == 0 {
            self.emit_xor_reg_self(reg);
            return;
        }
        // C7 /0: destination is in the R/M field, so use REX.B for extended regs
        self.rex_w_b(reg);
        self.buf.emit_byte(0xC7); // MOV r/m64, imm32
        self.modrm_reg(0, reg);
        self.buf.emit(&imm.to_le_bytes());
    }

    /// LEA reg, [RBP - offset] — compute address of a frame slot.
    /// Same encoding as emit_load_local but with LEA opcode (0x8D) instead of MOV (0x8B).
    pub(super) fn emit_lea_frame_slot(&mut self, dst: u8, offset: i32) {
        self.rex_w_r(dst);
        self.buf.emit_byte(0x8D); // LEA r64, m
        self.modrm_rbp_disp(dst, offset);
    }

    // -----------------------------------------------------------------------
    // AVX2 / VEX instruction helpers
    // -----------------------------------------------------------------------

    /// Emit a 2-byte VEX prefix: C5 [R~vvvvLpp]
    /// - `r`: REX.R complement (set if dest reg < 8)
    /// - `vvvv`: complement of source register (or 0b1111 for none)
    /// - `l`: 0 for 128-bit (XMM), 1 for 256-bit (YMM)
    /// - `pp`: opcode prefix (0=none, 1=66, 2=F3, 3=F2)
    pub(super) fn emit_vex2(&mut self, r: bool, vvvv: u8, l: bool, pp: u8) {
        self.buf.emit_byte(0xC5);
        let byte = (if r { 0x80 } else { 0 })
            | ((!vvvv & 0x0F) << 3)
            | (if l { 0x04 } else { 0 })
            | (pp & 0x03);
        self.buf.emit_byte(byte);
    }

    /// Emit a 3-byte VEX prefix: C4 [R~X~B~mmmmm] [W~vvvvLpp]
    #[allow(clippy::too_many_arguments)]
    pub(super) fn emit_vex3(
        &mut self,
        r: bool,
        x: bool,
        b: bool,
        mmmmm: u8,
        w: bool,
        vvvv: u8,
        l: bool,
        pp: u8,
    ) {
        self.buf.emit_byte(0xC4);
        let byte1 = (if r { 0x80 } else { 0 })
            | (if x { 0x40 } else { 0 })
            | (if b { 0x20 } else { 0 })
            | (mmmmm & 0x1F);
        self.buf.emit_byte(byte1);
        let byte2 = (if w { 0x80 } else { 0 })
            | ((!vvvv & 0x0F) << 3)
            | (if l { 0x04 } else { 0 })
            | (pp & 0x03);
        self.buf.emit_byte(byte2);
    }

    /// VPXOR YMM_dst, YMM_src1, YMM_src2 — XOR 256-bit registers
    /// VEX.256.66.0F.WIG EF /r
    pub(super) fn emit_vpxor_ymm(&mut self, dst: u8, src1: u8, src2: u8) {
        self.emit_vex2(dst < 8, src1, true, 1); // L=1(256-bit), pp=01(66)
        self.buf.emit_byte(0xEF);
        self.buf.emit_byte(0xC0 | ((dst & 7) << 3) | (src2 & 7));
    }

    /// VMOVDQU YMM, [reg + disp32] — unaligned 256-bit load
    /// VEX.256.F3.0F.WIG 6F /r
    #[allow(dead_code)]
    pub(super) fn emit_vmovdqu_load_ymm(&mut self, dst_ymm: u8, base_reg: u8, disp: i32) {
        let r = dst_ymm < 8;
        let b = base_reg < 8;
        if !b {
            self.emit_vex3(r, true, b, 0x01, false, 0, true, 2); // vvvv=0 → unused
        } else {
            self.emit_vex2(r, 0, true, 2); // vvvv=0 → unused, L=1, pp=10(F3)
        }
        self.buf.emit_byte(0x6F);
        // ModRM: mod=10 (disp32), reg=dst_ymm, rm=base_reg
        self.buf
            .emit_byte(0x80 | ((dst_ymm & 7) << 3) | (base_reg & 7));
        // SIB byte needed if base is RSP(4) or R12(12)
        if (base_reg & 7) == 4 {
            self.buf.emit_byte(0x24); // SIB: no index, base=RSP
        }
        self.buf.emit(&disp.to_le_bytes());
    }

    /// VMOVDQU [reg + disp32], YMM — unaligned 256-bit store
    /// VEX.256.F3.0F.WIG 7F /r
    #[allow(dead_code)]
    pub(super) fn emit_vmovdqu_store_ymm(&mut self, base_reg: u8, disp: i32, src_ymm: u8) {
        let r = src_ymm < 8;
        let b = base_reg < 8;
        if !b {
            self.emit_vex3(r, true, b, 0x01, false, 0, true, 2); // vvvv=0 → unused
        } else {
            self.emit_vex2(r, 0, true, 2); // vvvv=0 → unused
        }
        self.buf.emit_byte(0x7F);
        self.buf
            .emit_byte(0x80 | ((src_ymm & 7) << 3) | (base_reg & 7));
        if (base_reg & 7) == 4 {
            self.buf.emit_byte(0x24);
        }
        self.buf.emit(&disp.to_le_bytes());
    }

    /// VPADDD YMM_dst, YMM_src1, YMM_src2 — packed 32-bit integer add
    /// VEX.256.66.0F.WIG FE /r
    #[allow(dead_code)]
    fn emit_vpaddd_ymm(&mut self, dst: u8, src1: u8, src2: u8) {
        self.emit_vex2(dst < 8, src1, true, 1);
        self.buf.emit_byte(0xFE);
        self.buf.emit_byte(0xC0 | ((dst & 7) << 3) | (src2 & 7));
    }

    /// VPADDD YMM_dst, YMM_src1, [reg + disp32] — packed add from memory
    /// VEX.256.66.0F.WIG FE /r
    pub(super) fn emit_vpaddd_ymm_mem(&mut self, dst: u8, src1: u8, base_reg: u8, disp: i32) {
        let r = dst < 8;
        let b = base_reg < 8;
        if !b {
            self.emit_vex3(r, true, b, 0x01, false, src1, true, 1);
        } else {
            self.emit_vex2(r, src1, true, 1);
        }
        self.buf.emit_byte(0xFE);
        self.buf.emit_byte(0x80 | ((dst & 7) << 3) | (base_reg & 7));
        if (base_reg & 7) == 4 {
            self.buf.emit_byte(0x24);
        }
        self.buf.emit(&disp.to_le_bytes());
    }

    /// VEXTRACTI128 XMM, YMM, imm8 — extract high 128-bit lane
    /// VEX.256.66.0F3A.W0 39 /r imm8
    fn emit_vextracti128(&mut self, dst_xmm: u8, src_ymm: u8, lane: u8) {
        self.emit_vex3(src_ymm < 8, true, dst_xmm < 8, 0x03, false, 0, true, 1); // vvvv=0 → 1111 (unused)
        self.buf.emit_byte(0x39);
        self.buf
            .emit_byte(0xC0 | ((src_ymm & 7) << 3) | (dst_xmm & 7));
        self.buf.emit_byte(lane);
    }

    /// VPADDD XMM_dst, XMM_src1, XMM_src2 — packed 32-bit add (128-bit)
    /// VEX.128.66.0F.WIG FE /r
    fn emit_vpaddd_xmm(&mut self, dst: u8, src1: u8, src2: u8) {
        self.emit_vex2(dst < 8, src1, false, 1); // L=0 for 128-bit
        self.buf.emit_byte(0xFE);
        self.buf.emit_byte(0xC0 | ((dst & 7) << 3) | (src2 & 7));
    }

    /// VPSHUFD XMM_dst, XMM_src, imm8 — shuffle 32-bit integers
    /// VEX.128.66.0F.WIG 70 /r imm8
    fn emit_vpshufd_xmm(&mut self, dst: u8, src: u8, imm: u8) {
        self.emit_vex2(dst < 8, 0, false, 1); // vvvv=0 → 1111 (unused)
        self.buf.emit_byte(0x70);
        self.buf.emit_byte(0xC0 | ((dst & 7) << 3) | (src & 7));
        self.buf.emit_byte(imm);
    }

    /// VMOVD r32, XMM — move low 32-bit of XMM to GPR
    /// VEX.128.66.0F.W0 7E /r
    fn emit_vmovd_to_gpr(&mut self, dst_gpr: u8, src_xmm: u8) {
        self.emit_vex2(src_xmm < 8, 0, false, 1); // vvvv=0 → 1111 (unused)
        self.buf.emit_byte(0x7E);
        self.buf
            .emit_byte(0xC0 | ((src_xmm & 7) << 3) | (dst_gpr & 7));
    }

    /// VZEROUPPER — clear upper 128 bits of all YMM registers (required after AVX)
    /// VEX.128.0F.WIG 77
    pub(super) fn emit_vzeroupper(&mut self) {
        self.emit_vex2(true, 0, false, 0); // vvvv=0 → 1111 (unused)
        self.buf.emit_byte(0x77);
    }

    /// Emit a horizontal reduction of 8 packed int32 in YMM0 → scalar int32 in EAX.
    /// Uses YMM0 as source, YMM1 as temp. Destroys YMM0/YMM1.
    /// Result: EAX = sum of all 8 lanes of YMM0.
    pub(super) fn emit_horizontal_sum_ymm0_to_eax(&mut self) {
        // VEXTRACTI128 XMM1, YMM0, 1  — get high 128 bits
        self.emit_vextracti128(1, 0, 1);
        // VPADDD XMM0, XMM0, XMM1     — add high to low
        self.emit_vpaddd_xmm(0, 0, 1);
        // VPSHUFD XMM1, XMM0, 0x4E    — swap high/low 64-bit halves
        self.emit_vpshufd_xmm(1, 0, 0x4E);
        // VPADDD XMM0, XMM0, XMM1
        self.emit_vpaddd_xmm(0, 0, 1);
        // VPSHUFD XMM1, XMM0, 0xB1    — swap adjacent 32-bit elements
        self.emit_vpshufd_xmm(1, 0, 0xB1);
        // VPADDD XMM0, XMM0, XMM1
        self.emit_vpaddd_xmm(0, 0, 1);
        // VMOVD EAX, XMM0
        self.emit_vmovd_to_gpr(RAX, 0);
    }

    // -----------------------------------------------------------------------
    // T17.Β.2 — SIMD element-wise emission (out[i] = a[i] OP b[i])
    // -----------------------------------------------------------------------

    /// Emit an AVX2 packed 3-operand YMM instruction of the form
    /// `op YMM_dst, YMM_src1, [base_reg + disp32]`.
    ///
    /// All element-wise integer ops (VPADDD, VPSUBD, VPMULLD, VPAND,
    /// VPOR, VPXOR) share the VEX.256.66.0F(.38).WIG encoding skeleton
    /// — only the opcode byte and the 0F38 vs 0F leading-opcode map
    /// differ. Centralizing the encoding avoids 6× duplication.
    ///
    /// `mm` selects the leading-opcode map:
    /// - 1 = 0F (covers VPADDD/VPSUBD/VPAND/VPOR/VPXOR)
    /// - 2 = 0F38 (covers VPMULLD only)
    fn emit_avx2_ymm_mem_66(
        &mut self,
        opcode: u8,
        mm: u8,
        dst: u8,
        src1: u8,
        base_reg: u8,
        disp: i32,
    ) {
        let r = dst < 8;
        let b = base_reg < 8;
        // 0F38 map always needs the 3-byte VEX prefix (C4). For the
        // 0F map we can use the compact 2-byte VEX only when
        // REX.B==0 (base reg is r0..r7).
        if mm != 1 || !b {
            self.emit_vex3(r, true, b, mm & 0x1F, false, src1, true, 1);
        } else {
            self.emit_vex2(r, src1, true, 1);
        }
        self.buf.emit_byte(opcode);
        // ModRM: mod=10 (disp32), reg=dst, rm=base_reg
        self.buf.emit_byte(0x80 | ((dst & 7) << 3) | (base_reg & 7));
        // SIB needed when base_reg encodes to RSP(4) or R12(12).
        if (base_reg & 7) == 4 {
            self.buf.emit_byte(0x24);
        }
        self.buf.emit(&disp.to_le_bytes());
    }

    /// VPSUBD YMM_dst, YMM_src1, [base + disp32]
    /// VEX.256.66.0F.WIG FA /r
    fn emit_vpsubd_ymm_mem(&mut self, dst: u8, src1: u8, base_reg: u8, disp: i32) {
        self.emit_avx2_ymm_mem_66(0xFA, 1, dst, src1, base_reg, disp);
    }

    /// VPMULLD YMM_dst, YMM_src1, [base + disp32]
    /// VEX.256.66.0F38.WIG 40 /r  (AVX2 only)
    fn emit_vpmulld_ymm_mem(&mut self, dst: u8, src1: u8, base_reg: u8, disp: i32) {
        self.emit_avx2_ymm_mem_66(0x40, 2, dst, src1, base_reg, disp);
    }

    /// VPAND YMM_dst, YMM_src1, [base + disp32]
    /// VEX.256.66.0F.WIG DB /r
    fn emit_vpand_ymm_mem(&mut self, dst: u8, src1: u8, base_reg: u8, disp: i32) {
        self.emit_avx2_ymm_mem_66(0xDB, 1, dst, src1, base_reg, disp);
    }

    /// VPOR YMM_dst, YMM_src1, [base + disp32]
    /// VEX.256.66.0F.WIG EB /r
    fn emit_vpor_ymm_mem(&mut self, dst: u8, src1: u8, base_reg: u8, disp: i32) {
        self.emit_avx2_ymm_mem_66(0xEB, 1, dst, src1, base_reg, disp);
    }

    /// VPXOR YMM_dst, YMM_src1, [base + disp32]
    /// VEX.256.66.0F.WIG EF /r
    fn emit_vpxor_ymm_mem(&mut self, dst: u8, src1: u8, base_reg: u8, disp: i32) {
        self.emit_avx2_ymm_mem_66(0xEF, 1, dst, src1, base_reg, disp);
    }

    /// Dispatch table from [`ElementWiseOp`] to the matching
    /// `op YMM_dst, YMM_src1, [mem]` emitter. Keeps
    /// `emit_simd_int_array_element_wise` short.
    pub(super) fn emit_ewise_ymm_mem(
        &mut self,
        op: ElementWiseOp,
        dst: u8,
        src1: u8,
        base_reg: u8,
        disp: i32,
    ) {
        match op {
            ElementWiseOp::Add => self.emit_vpaddd_ymm_mem(dst, src1, base_reg, disp),
            ElementWiseOp::Sub => self.emit_vpsubd_ymm_mem(dst, src1, base_reg, disp),
            ElementWiseOp::Mul => self.emit_vpmulld_ymm_mem(dst, src1, base_reg, disp),
            ElementWiseOp::And => self.emit_vpand_ymm_mem(dst, src1, base_reg, disp),
            ElementWiseOp::Or => self.emit_vpor_ymm_mem(dst, src1, base_reg, disp),
            ElementWiseOp::Xor => self.emit_vpxor_ymm_mem(dst, src1, base_reg, disp),
        }
    }

    /// Encode the scalar (single-lane) version of an [`ElementWiseOp`]
    /// as a 32-bit integer op with the standard x86-64 ModRM encoding
    /// `op EAX, ECX` (`EAX = EAX OP ECX`). Used in the remainder loop.
    pub(super) fn emit_ewise_scalar_eax_ecx(&mut self, op: ElementWiseOp) {
        match op {
            // ADD EAX, ECX — 01 C8
            ElementWiseOp::Add => self.buf.emit(&[0x01, 0xC8]),
            // SUB EAX, ECX — 29 C8
            ElementWiseOp::Sub => self.buf.emit(&[0x29, 0xC8]),
            // IMUL EAX, ECX — 0F AF C1
            ElementWiseOp::Mul => self.buf.emit(&[0x0F, 0xAF, 0xC1]),
            // AND EAX, ECX — 21 C8
            ElementWiseOp::And => self.buf.emit(&[0x21, 0xC8]),
            // OR  EAX, ECX — 09 C8
            ElementWiseOp::Or => self.buf.emit(&[0x09, 0xC8]),
            // XOR EAX, ECX — 31 C8
            ElementWiseOp::Xor => self.buf.emit(&[0x31, 0xC8]),
        }
    }

    /// T5.2.16 — emit `JMP rax` via an absolute target, through RAX.
    ///
    /// Sequence: `MOV RAX, imm64 ; JMP RAX`. Used by sibling tail-call
    /// sites after `emit_epilogue_without_ret`. Because RAX is
    /// caller-saved and we've already torn down our frame, clobbering
    /// it here is safe.
    pub(super) fn emit_jmp_absolute(&mut self, addr: usize) {
        self.rex_w();
        self.buf.emit_byte(0xB8); // MOV rax, imm64
        self.buf.emit(&(addr as i64).to_le_bytes()); // Cast: address arithmetic
                                                     // JMP RAX (FF /4)
        self.buf.emit(&[0xFF, 0xE0]);
    }

    /// Emit a CALL to `addr` choosing the shortest valid encoding.
    ///
    /// If `addr` lies within ±2GB of the byte after this call (the rel32
    /// reference point — `current_pc + 5`), emit `E8 <rel32>` (5 bytes).
    /// Otherwise fall back to the 12-byte `MOV RAX, imm64 ; CALL RAX`
    /// sequence (`emit_call_imm64_via_rax`). Helper targets (registered
    /// runtime functions in `JitRuntimeHelpers`) are typically within
    /// ±2GB of the JIT code cache, so the rel32 form dominates and
    /// saves 7 bytes per call site.
    ///
    /// Safety / correctness notes:
    /// - The JIT buffer is allocated with a stable base for its entire
    ///   lifetime (`JitBuf::reserve` does not relocate after `as_ptr()`
    ///   is observed). The address `buf.as_ptr() + buf.pos()` is
    ///   therefore the final runtime PC of the call site, and the
    ///   rel32 displacement computed here remains valid after
    ///   `finalize`.
    /// - Oop maps record `native_pc_offset = buf.pos()` which is the PC
    ///   *after* the call. Switching encodings changes the absolute PC
    ///   of subsequent instructions, but the oop map is captured at the
    ///   correct post-emission position, so the map stays consistent.
    /// - The rel32 path does NOT clobber RAX. No current call site
    ///   depends on the imm64-via-RAX side effect — every helper site
    ///   materializes its ABI args explicitly before calling.
    pub(super) fn emit_call_absolute(&mut self, addr: usize) {
        // Reference point for the rel32 displacement is the byte after
        // the 5-byte E8 cd encoding.
        // Cast: non-negative index/count to usize
        let call_pc = self.buf.as_ptr() as usize + self.buf.pos();
        let next_pc = call_pc.wrapping_add(5);
        // Signed delta from next_pc to target. Compute in i128 to keep
        // the comparison free of usize-subtraction wrap concerns.
        // Widening: i64/usize -> i128 (no truncation, for range check)
        let delta: i128 = (addr as i128) - (next_pc as i128);
        // Widening: i64/usize -> i128 (no truncation, for range check)
        if delta >= i32::MIN as i128 && delta <= i32::MAX as i128 {
            // E8 cd: CALL rel32 (5 bytes).
            self.buf.emit_byte(0xE8);
            // Task #60 (Design B): record the rel32 patch offset so the
            // unroll duplicator can re-resolve helper calls per-copy.
            // Duplicating the verbatim bytes would land the copied rel32
            // at `helper + shift` — the N-Body Body.x SIGSEGV pattern from
            // CHANGELOG. The duplicator reads `original_rel32` here,
            // reconstructs `helper_addr = orig_call_pc + 5 + rel32`, then
            // rewrites the copy's rel32 to `helper_addr - (copy_pc + 5)`.
            self.helper_call_patches.push(self.buf.pos());
            self.buf.emit(&(delta as i32).to_le_bytes()); // Cast: rel32 displacement
        } else {
            // Out of ±2GB reach — fall back to the 12-byte form.
            // The fallback bakes an absolute imm64 into the instruction
            // stream so byte-copy duplication is shift-safe automatically
            // (no patch tracking required).
            self.emit_call_imm64_via_rax(addr);
        }
    }

    /// Step 1 (inline frame-record) — emit the single segment-relative `MOV`
    /// that stores RBP straight into the precise-maps innermost-RBP mirror TLS
    /// slot, replacing `call jit_frame_record`. Windows uses `gs:` (`0x65`);
    /// Linux uses `fs:` (`0x64`). `disp32` comes from the sentinel-probed
    /// [`inline_rbp_tls_disp`].
    ///
    /// Encoding (9 bytes): `<seg> 48 89 2C 25 <disp32-le>`
    ///   * `<seg>`    — GS (`65`) or FS (`64`) segment override prefix.
    ///   * `48`       — REX.W (64-bit operand).
    ///   * `89`       — MOV r/m64, r64.
    ///   * `2C`       — ModRM mod=00 reg=RBP(5) r/m=100(SIB).
    ///   * `25`       — SIB scale=0 index=none(4) base=none(5) → [disp32].
    ///   * `disp32`   — displacement; effective address = segment base + disp.
    /// RAX = the current `JvmThread*`: the `JIT_THREAD` mirror load where the
    /// VM publishes it, the `get_current_thread` helper call otherwise. The two
    /// return the same pointer by construction (`publish_jit_thread_mirror` is
    /// called at every site that sets `JIT_THREAD`).
    pub(super) fn emit_fetch_current_thread_into_rax(&mut self) {
        let tls_disp = jit_thread_tls_disp();
        if tls_disp != 0 {
            self.emit_mov_rax_tls_disp32(tls_disp as u32);
        } else {
            self.emit_call_absolute(self.helpers.get_current_thread);
        }
    }
    /// `MOV RAX, gs:[disp32]` (`fs:` on Linux): the one-instruction thread
    /// fetch through the `JIT_THREAD` mirror (`jit_thread_tls_disp`).
    pub(super) fn emit_mov_rax_tls_disp32(&mut self, disp32: u32) {
        self.buf.emit_byte(inline_rbp_tls_segment_prefix());
        self.buf.emit_byte(0x48); // REX.W
        self.buf.emit_byte(0x8B); // MOV r64, r/m64
        self.buf.emit_byte(0x04); // ModRM: reg=RAX, r/m=SIB
        self.buf.emit_byte(0x25); // SIB: [disp32] absolute
        self.buf.emit(&disp32.to_le_bytes());
    }
    pub(super) fn emit_mov_tls_disp32_rbp(&mut self, disp32: u32) {
        self.buf.emit_byte(inline_rbp_tls_segment_prefix());
        self.buf.emit_byte(0x48); // REX.W
        self.buf.emit_byte(0x89); // MOV r/m64, r64
        self.buf.emit_byte(0x2C); // ModRM: reg=RBP, r/m=SIB
        self.buf.emit_byte(0x25); // SIB: [disp32] absolute
        self.buf.emit(&disp32.to_le_bytes());
    }

    /// The identity half of the frame record — store this method's compile id
    /// into the mirror slot beside the one `emit_mov_tls_disp32_rbp` writes, so
    /// the GC can name the method owning the innermost RBP instead of decoding
    /// the call that created the frame (`jit::reserve_compile_id`).
    ///
    /// **32-bit store, no scratch register.** That is the whole reason the id
    /// is a dense `u32` rather than a `CompiledMethod` pointer: the post-call
    /// republish sites run with the callee's return value live in RAX and
    /// document that they must not disturb it. A 64-bit immediate would need a
    /// register and put a push/pop back on every JIT->JIT call site.
    ///
    /// Encoding (11 bytes): `<seg> C7 04 25 <disp32-le> <imm32-le>`
    ///   * `<seg>`    — GS (`65`) or FS (`64`) segment override prefix.
    ///   * `C7 /0`    — MOV r/m32, imm32. No REX: the store is 32-bit and the
    ///                  upper half of the slot is never read.
    ///   * `04`       — ModRM mod=00 reg=/0 r/m=100(SIB).
    ///   * `25`       — SIB scale=0 index=none(4) base=none(5) → [disp32].
    pub(super) fn emit_mov_tls_disp32_imm32(&mut self, disp32: u32, imm32: u32) {
        self.buf.emit_byte(inline_rbp_tls_segment_prefix());
        self.buf.emit_byte(0xC7); // MOV r/m32, imm32
        self.buf.emit_byte(0x04); // ModRM: /0, r/m=SIB
        self.buf.emit_byte(0x25); // SIB: [disp32] absolute
        self.buf.emit(&disp32.to_le_bytes());
        self.buf.emit(&imm32.to_le_bytes());
    }

    /// Task #60 — emit `MOV r64, imm64` in the fixed-length 10-byte form
    /// regardless of `imm` value.
    ///
    /// `emit_mov_imm64` opportunistically shrinks to `XOR r,r` (imm == 0) or
    /// the 7-byte `MOV r/m64, imm32` (sign-extendable imm) — both fine for
    /// general use, but they make the imm64 location non-deterministic
    /// inside the instruction. The unroll duplicator needs to patch a
    /// fresh IC slot pointer into duplicated `MOV R10, imm64`s, which
    /// requires a known imm64 offset. This emitter always emits:
    ///
    /// ```text
    ///   REX.W [+ REX.B if r>=8]   (1 byte)
    ///   0xB8 + (reg & 7)          (1 byte)
    ///   imm64                     (8 bytes — little-endian)
    /// ```
    ///
    /// for a total of 10 bytes, with the imm64 starting at offset +2 from
    /// the instruction's first byte. Only the IC-site emission paths
    /// (`pic_inline` / `mic_inline`) need this — everywhere else the
    /// shrinking form remains optimal.
    pub(super) fn emit_mov_imm64_full(&mut self, reg: u8, imm: i64) {
        self.rex_w_b(reg);
        self.buf.emit_byte(0xB8 + (reg & 7));
        self.buf.emit(&imm.to_le_bytes());
    }

    /// Emit the 12-byte absolute call: `MOV RAX, imm64 ; CALL RAX`.
    ///
    /// Direct emission helper — `emit_call_absolute` is the preferred
    /// entry point and will dispatch here only when the rel32 path is
    /// out of reach. Kept as a separate function so the fallback is
    /// explicit at the one call site that needs it (inside
    /// `emit_call_absolute` itself).
    fn emit_call_imm64_via_rax(&mut self, addr: usize) {
        // MOV RAX, imm64 (REX.W + B8+rd)
        self.rex_w();
        self.buf.emit_byte(0xB8); // MOV rax, imm64
        self.buf.emit(&(addr as i64).to_le_bytes()); // Cast: address arithmetic
                                                     // CALL RAX (FF /2)
        self.buf.emit(&[0xFF, 0xD0]);
    }

    // -----------------------------------------------------------------------
    // Inline TLAB bump-pointer helpers (HIGH-6 JIT audit, object_allocation)
    // -----------------------------------------------------------------------

    /// Emit `MOV r64, [base + disp32]` using the full disp32 encoding so the
    /// caller does not have to special-case small displacements (the
    /// TLAB cursor/end offsets reach into JvmThread which can be hundreds of
    /// bytes from the struct base).
    ///
    /// `dst` and `base` are register numbers 0..=15 (e.g. RAX=0, R10=10).
    pub(super) fn emit_mov_r64_mem_disp32(&mut self, dst: u8, base: u8, disp: i32) {
        // REX.W + REX.R for dst >= 8 + REX.B for base >= 8.
        let mut rex = 0x48u8;
        if dst >= 8 {
            rex |= 0x04;
        }
        if base >= 8 {
            rex |= 0x01;
        }
        self.buf.emit_byte(rex);
        self.buf.emit_byte(0x8B); // MOV r64, r/m64
                                  // ModRM: mod=10 (disp32), reg=dst&7, r/m=base&7.
                                  // base==RSP/R12 would require a SIB byte; neither is used here.
        self.buf.emit_byte(0x80 | ((dst & 7) << 3) | (base & 7));
        self.buf.emit(&disp.to_le_bytes());
    }

    /// Emit `MOVSXD dst32→r64, [base + disp32]` — a 32-bit load from memory
    /// sign-extended into the full 64-bit `dst`. Used by inline `getfield` for
    /// `int`-category fields so the result matches `jit_getfield`'s
    /// `Value::Int(i) => i as i64` (sign-extending) ABI exactly.
    ///
    /// `dst` and `base` are register numbers 0..=15. `base` must not be
    /// RSP/R12 (would need a SIB byte — not emitted here).
    pub(super) fn emit_movsxd_r64_mem_disp32(&mut self, dst: u8, base: u8, disp: i32) {
        // REX.W (0x48) + REX.R for dst>=8 + REX.B for base>=8.
        let mut rex = 0x48u8;
        if dst >= 8 {
            rex |= 0x04;
        }
        if base >= 8 {
            rex |= 0x01;
        }
        self.buf.emit_byte(rex);
        self.buf.emit_byte(0x63); // MOVSXD r64, r/m32
                                  // ModRM: mod=10 (disp32), reg=dst&7, r/m=base&7.
        self.buf.emit_byte(0x80 | ((dst & 7) << 3) | (base & 7));
        self.buf.emit(&disp.to_le_bytes());
    }

    /// Emit a sign- or zero-extending 8/16-bit load into a 64-bit register.
    pub(super) fn emit_movx_r64_mem_disp32(
        &mut self,
        dst: u8,
        base: u8,
        disp: i32,
        source_bits: u8,
        signed: bool,
    ) {
        let mut rex = 0x48u8;
        if dst >= 8 {
            rex |= 0x04;
        }
        if base >= 8 {
            rex |= 0x01;
        }
        self.buf.emit_byte(rex);
        self.buf.emit_byte(0x0F);
        let opcode = match (source_bits, signed) {
            (8, true) => 0xBE,   // MOVSX r64, r/m8
            (8, false) => 0xB6,  // MOVZX r64, r/m8
            (16, true) => 0xBF,  // MOVSX r64, r/m16
            (16, false) => 0xB7, // MOVZX r64, r/m16
            _ => {
                self.fail("singlepass-codegen/movsx-source-width-unsupported");
                return;
            }
        };
        self.buf.emit_byte(opcode);
        self.buf.emit_byte(0x80 | ((dst & 7) << 3) | (base & 7));
        self.buf.emit(&disp.to_le_bytes());
    }

    /// Emit `MOV dst32, [base + disp32]` — a 32-bit load that zero-extends
    /// into the full 64-bit `dst` (implicit on x86-64 for any 32-bit GPR
    /// write). Used by inline `getfield` for `float` fields so the result
    /// matches `jit_getfield`'s `Value::Float(f) => f.to_bits() as i64`
    /// (zero-extending the 32-bit bit pattern) ABI exactly.
    ///
    /// `dst` and `base` are register numbers 0..=15. `base` must not be
    /// RSP/R12 (would need a SIB byte — not emitted here).
    pub(super) fn emit_mov_r32_mem_disp32(&mut self, dst: u8, base: u8, disp: i32) {
        // REX is only needed for extended (>=8) registers; no REX.W (32-bit op).
        if dst >= 8 || base >= 8 {
            let mut rex = 0x40u8;
            if dst >= 8 {
                rex |= 0x04;
            }
            if base >= 8 {
                rex |= 0x01;
            }
            self.buf.emit_byte(rex);
        }
        self.buf.emit_byte(0x8B); // MOV r32, r/m32
                                  // ModRM: mod=10 (disp32), reg=dst&7, r/m=base&7.
        self.buf.emit_byte(0x80 | ((dst & 7) << 3) | (base & 7));
        self.buf.emit(&disp.to_le_bytes());
    }

    /// Emit `MOV [base + disp32], r64` (the bump-commit store).
    pub(super) fn emit_mov_mem_disp32_r64(&mut self, base: u8, src: u8, disp: i32) {
        let mut rex = 0x48u8;
        if src >= 8 {
            rex |= 0x04;
        }
        if base >= 8 {
            rex |= 0x01;
        }
        self.buf.emit_byte(rex);
        self.buf.emit_byte(0x89); // MOV r/m64, r64
        self.buf.emit_byte(0x80 | ((src & 7) << 3) | (base & 7));
        self.buf.emit(&disp.to_le_bytes());
    }

    /// Emit `MOV DWORD [base + disp32], imm32` — used to splat `class_id`
    /// into the freshly bumped object header at offset 0.
    pub(super) fn emit_mov_dword_mem_disp32_imm32(&mut self, base: u8, disp: i32, imm: i32) {
        // REX.B only (no .W: 32-bit op).
        if base >= 8 {
            self.buf.emit_byte(0x41);
        }
        self.buf.emit_byte(0xC7); // MOV r/m32, imm32 (with /0)
        self.buf.emit_byte(0x80 | (base & 7));
        self.buf.emit(&disp.to_le_bytes());
        self.buf.emit(&imm.to_le_bytes());
    }

    /// Emit `MOV BYTE [base + disp32], imm8`.
    ///
    /// Needed because the `gc_flags` bits now live inside a byte of the mark
    /// word rather than owning a dword of their own. A dword store at that
    /// offset would span past `HEADER_SIZE` into the object body -- which is
    /// precisely what `inline_tlab_header_writes_stay_inside_the_header`
    /// caught when the offsets were swept mechanically.
    pub(super) fn emit_mov_byte_mem_disp32_imm8(&mut self, base: u8, disp: i32, imm: u8) {
        // REX.B only when the base needs it; no .W (byte op).
        if base >= 8 {
            self.buf.emit_byte(0x41);
        }
        self.buf.emit_byte(0xC6); // MOV r/m8, imm8 (with /0)
        self.buf.emit_byte(0x80 | (base & 7));
        self.buf.emit(&disp.to_le_bytes());
        self.buf.emit_byte(imm);
    }

    /// Emit `LEA r64, [base + imm32]` — compute new cursor without
    /// touching the source register.
    pub(super) fn emit_lea_r64_mem_disp32(&mut self, dst: u8, base: u8, disp: i32) {
        let mut rex = 0x48u8;
        if dst >= 8 {
            rex |= 0x04;
        }
        if base >= 8 {
            rex |= 0x01;
        }
        self.buf.emit_byte(rex);
        self.buf.emit_byte(0x8D); // LEA r64, m
        self.buf.emit_byte(0x80 | ((dst & 7) << 3) | (base & 7));
        self.buf.emit(&disp.to_le_bytes());
    }

    /// Emit `CMP r64, [base + disp32]` — the TLAB-overflow check.
    pub(super) fn emit_cmp_r64_mem_disp32(&mut self, lhs: u8, base: u8, disp: i32) {
        let mut rex = 0x48u8;
        if lhs >= 8 {
            rex |= 0x04;
        }
        if base >= 8 {
            rex |= 0x01;
        }
        self.buf.emit_byte(rex);
        self.buf.emit_byte(0x3B); // CMP r64, r/m64
        self.buf.emit_byte(0x80 | ((lhs & 7) << 3) | (base & 7));
        self.buf.emit(&disp.to_le_bytes());
    }

    /// Emit `CMP r64, [base + disp]` choosing the **smallest legal**
    /// displacement form, for a `base` that is not RSP/R12.
    ///
    /// Split from [`Self::emit_cmp_r64_mem_disp32`] rather than folded into it
    /// because that one is named for the width it emits and several callers
    /// pin its bytes. The receiver-containment guard is the caller this exists
    /// for: it reads six table words at displacements 0..40, every one of which
    /// fits a `disp8`, and paid three wasted bytes on each.
    ///
    /// `base & 7 == 0b100` (RSP/R12) would need a SIB byte, which neither this
    /// nor the disp32 form emits; such a base falls back to the disp32 form so
    /// this function never becomes the place a SIB-less RSP operand is
    /// introduced. `base & 7 == 0b101` (RBP/R13) has no `mod=00` form, so a
    /// zero displacement there still takes the explicit `disp8` arm.
    pub(super) fn emit_cmp_r64_mem_disp(&mut self, lhs: u8, base: u8, disp: i32) {
        if base & 7 == 0b100 || !(i32::from(i8::MIN)..=i32::from(i8::MAX)).contains(&disp) {
            self.emit_cmp_r64_mem_disp32(lhs, base, disp);
            return;
        }
        let mut rex = 0x48u8;
        if lhs >= 8 {
            rex |= 0x04;
        }
        if base >= 8 {
            rex |= 0x01;
        }
        self.buf.emit_byte(rex);
        self.buf.emit_byte(0x3B); // CMP r64, r/m64
        if disp == 0 && base & 7 != 0b101 {
            // mod=00 — no displacement byte at all.
            self.buf.emit_byte(((lhs & 7) << 3) | (base & 7));
        } else {
            // mod=01 — disp8.
            self.buf.emit_byte(0x40 | ((lhs & 7) << 3) | (base & 7));
            // Cast: guarded by the range check above.
            self.buf.emit_byte(disp as u8);
        }
    }

    /// `MOVZX r32, byte [base + disp8]` — a one-BYTE header read.
    ///
    /// The reference-store gates test bits of the `GC_FLAGS_BYTE_OFFSET` byte,
    /// which sits 15 bytes into a 16-byte header. The pre-existing readers of
    /// it use `MOV r32, [base + disp]` — a FOUR-byte read starting at byte 15,
    /// so three of the four bytes come from the first instance field, or from
    /// whatever follows a field-less object. That works only because every
    /// consumer masks the low byte back out. This emitter reads the byte the
    /// callers actually want.
    pub(super) fn emit_movzx_r32_mem8(&mut self, dst: u8, base: u8, disp: i32) {
        let mut rex = 0u8;
        if dst >= 8 {
            rex |= 0x44;
        }
        if base >= 8 {
            rex |= 0x41;
        }
        if rex != 0 {
            self.buf.emit_byte(rex);
        }
        self.buf.emit(&[0x0F, 0xB6]); // MOVZX r32, r/m8
        self.emit_modrm_disp_for_base(dst, base, disp);
    }

    /// `CMP BYTE [base + disp], imm8`.
    pub(super) fn emit_cmp_mem8_imm8(&mut self, base: u8, disp: i32, imm: u8) {
        if base >= 8 {
            self.buf.emit_byte(0x41); // REX.B
        }
        self.buf.emit_byte(0x80); // CMP r/m8, imm8  (/7)
        self.emit_modrm_disp_for_base(7, base, disp);
        self.buf.emit_byte(imm);
    }

    /// `CMP r8, BYTE [base + disp]` — an unsigned byte compare against a
    /// memory-resident threshold. `lhs` must be one of the legacy byte
    /// registers (AL/CL/DL/BL); an extended register would need a REX prefix
    /// this encoding does not emit, so it is refused rather than mis-encoded.
    pub(super) fn emit_cmp_r8_mem8(&mut self, lhs: u8, base: u8, disp: i32) {
        debug_assert!(
            lhs < 4,
            "emit_cmp_r8_mem8: r{lhs} is not a legacy byte register"
        );
        if base >= 8 {
            self.buf.emit_byte(0x41); // REX.B
        }
        self.buf.emit_byte(0x3A); // CMP r8, r/m8
        self.emit_modrm_disp_for_base(lhs, base, disp);
    }

    /// `TEST r8, imm8` for a legacy byte register.
    pub(super) fn emit_test_r8_imm8(&mut self, reg: u8, imm: u8) {
        debug_assert!(
            reg < 4,
            "emit_test_r8_imm8: r{reg} is not a legacy byte register"
        );
        self.buf.emit(&[0xF6, 0xC0 | (reg & 7), imm]);
    }

    /// ModRM byte plus displacement for `[base + disp]`, smallest legal form.
    ///
    /// The two x86 base special cases both apply and both are silent
    /// mis-encodings if ignored: `base & 7 == 0b101` (RBP/R13) has no `mod=00`
    /// form, and `base & 7 == 0b100` (RSP/R12) needs a SIB byte, which this
    /// emits as the canonical `0x24`.
    fn emit_modrm_disp_for_base(&mut self, reg: u8, base: u8, disp: i32) {
        let needs_disp = disp != 0 || base & 7 == 0b101;
        let short = (i32::from(i8::MIN)..=i32::from(i8::MAX)).contains(&disp);
        let mode: u8 = if !needs_disp {
            0x00
        } else if short {
            0x40
        } else {
            0x80
        };
        self.buf.emit_byte(mode | ((reg & 7) << 3) | (base & 7));
        if base & 7 == 0b100 {
            self.buf.emit_byte(0x24); // SIB: scale=0, index=none, base=rsp/r12
        }
        if needs_disp {
            if short {
                // Cast: guarded by `short`.
                self.buf.emit_byte(disp as u8);
            } else {
                self.buf.emit(&disp.to_le_bytes());
            }
        }
    }

    /// Emit `MOV r64, r64` (register-to-register move).
    pub(super) fn emit_mov_r64_r64(&mut self, dst: u8, src: u8) {
        // Peephole: skip self-moves (no-op). Matches `emit_mov_reg_reg`.
        if dst == src {
            return;
        }
        let mut rex = 0x48u8;
        if src >= 8 {
            rex |= 0x04;
        }
        if dst >= 8 {
            rex |= 0x01;
        }
        self.buf.emit_byte(rex);
        self.buf.emit_byte(0x89); // MOV r/m64, r64
        self.buf.emit_byte(0xC0 | ((src & 7) << 3) | (dst & 7));
    }

    pub(super) fn emit_sub_r64_r64(&mut self, dst: u8, src: u8) {
        self.rex_w_rb(src, dst);
        self.buf.emit_byte(0x29); // SUB r/m64, r64
        self.modrm_reg(src, dst);
    }

    /// `SUB r64, [base + disp32]` (REX.W 2B /r).
    ///
    /// F-08 — the inline G1 barrier's `addr - arena_base`. A memory operand
    /// rather than a baked immediate on purpose: the arena base is a property
    /// of the collector INSTANCE, and compiled code outlives collector
    /// construction in embedding and in the unit suite, so the value has to be
    /// read at run time from the published table. That is the same rule
    /// `emit_guarded_getfield_receiver_check` follows for the bounds it
    /// compares against.
    pub(super) fn emit_sub_r64_mem_disp32(&mut self, dst: u8, base: u8, disp: i32) {
        let mut rex = 0x48u8;
        if dst >= 8 {
            rex |= 0x04;
        }
        if base >= 8 {
            rex |= 0x01;
        }
        self.buf.emit_byte(rex);
        self.buf.emit_byte(0x2B); // SUB r64, r/m64
        self.buf.emit_byte(0x80 | ((dst & 7) << 3) | (base & 7));
        self.buf.emit(&disp.to_le_bytes());
    }

    /// `AND r64, [base + disp32]` (REX.W 23 /r). Sets ZF from the result, so
    /// the caller can branch on it without a separate `TEST`.
    pub(super) fn emit_and_r64_mem_disp32(&mut self, dst: u8, base: u8, disp: i32) {
        let mut rex = 0x48u8;
        if dst >= 8 {
            rex |= 0x04;
        }
        if base >= 8 {
            rex |= 0x01;
        }
        self.buf.emit_byte(rex);
        self.buf.emit_byte(0x23); // AND r64, r/m64
        self.buf.emit_byte(0x80 | ((dst & 7) << 3) | (base & 7));
        self.buf.emit(&disp.to_le_bytes());
    }

    /// `XOR dst64, src64` (REX.W 31 /r).
    ///
    /// Distinct from [`Self::emit_xor_reg_self`], which is the zeroing idiom.
    pub(super) fn emit_xor_r64_r64(&mut self, dst: u8, src: u8) {
        self.rex_w_rb(src, dst);
        self.buf.emit_byte(0x31); // XOR r/m64, r64
        self.modrm_reg(src, dst);
    }

    pub(super) fn emit_shr_r64_imm8(&mut self, reg: u8, shift: u8) {
        self.rex_w_b(reg);
        self.buf.emit_byte(0xC1);
        self.buf.emit_byte(0xE8 | (reg & 7)); // /5 SHR, mod=11
        self.buf.emit_byte(shift);
    }

    /// `MOV byte ptr [base + index], imm8`.
    pub(super) fn emit_mov_mem8_indexed_imm8(&mut self, base: u8, index: u8, value: u8) {
        let mut rex = 0x40u8;
        if index >= 8 {
            rex |= 0x02; // X
        }
        if base >= 8 {
            rex |= 0x01; // B
        }
        if rex != 0x40 {
            self.buf.emit_byte(rex);
        }
        self.buf.emit_byte(0xC6);
        self.buf.emit_byte(0x04); // mod=00, /0, SIB
        self.buf.emit_byte(((index & 7) << 3) | (base & 7)); // scale=1
        self.buf.emit_byte(value);
    }

    /// Emit `ADD r64, imm8` (sign-extended). Used by the TLAB-align step
    /// (`cursor + 7` before AND with -8).
    pub(super) fn emit_add_r64_imm8(&mut self, reg: u8, imm: i8) {
        let mut rex = 0x48u8;
        if reg >= 8 {
            rex |= 0x01;
        }
        self.buf.emit_byte(rex);
        self.buf.emit_byte(0x83); // /0 = ADD
        self.buf.emit_byte(0xC0 | (reg & 7));
        self.buf.emit_byte(imm as u8); // Cast: x86-64 immediate encoding
    }

    /// Emit `AND r64, imm8` (sign-extended). Used by the TLAB-align step
    /// (`cursor & -8`).
    pub(super) fn emit_and_r64_imm8(&mut self, reg: u8, imm: i8) {
        let mut rex = 0x48u8;
        if reg >= 8 {
            rex |= 0x01;
        }
        self.buf.emit_byte(rex);
        self.buf.emit_byte(0x83); // /4 = AND
        self.buf.emit_byte(0xE0 | (reg & 7));
        self.buf.emit_byte(imm as u8); // Cast: x86-64 immediate encoding
    }

    /// Emit `OR r64, imm8` (sign-extended). Used to set the shadow-stack
    /// overflow tag bit in the saved-base slot.
    pub(super) fn emit_or_r64_imm8(&mut self, reg: u8, imm: i8) {
        let mut rex = 0x48u8;
        if reg >= 8 {
            rex |= 0x01;
        }
        self.buf.emit_byte(rex);
        self.buf.emit_byte(0x83); // /1 = OR
        self.buf.emit_byte(0xC8 | (reg & 7));
        self.buf.emit_byte(imm as u8); // Cast: x86-64 immediate encoding
    }

    /// Emit `TEST r64, imm32` (sign-extended) — sets ZF when no selected bit is
    /// set. Used to read the shadow-stack overflow tag out of a saved base.
    pub(super) fn emit_test_r64_imm32(&mut self, reg: u8, imm: i32) {
        let mut rex = 0x48u8;
        if reg >= 8 {
            rex |= 0x01;
        }
        self.buf.emit_byte(rex);
        self.buf.emit_byte(0xF7); // /0 = TEST r/m64, imm32
        self.buf.emit_byte(0xC0 | (reg & 7));
        self.buf.emit(&imm.to_le_bytes());
    }

    /// Emit `TEST r64, r64` — sets ZF if the register is zero.
    pub(super) fn emit_test_r64_r64(&mut self, reg: u8) {
        let mut rex = 0x48u8;
        if reg >= 8 {
            rex |= 0x05;
        } // REX.W + REX.R + REX.B
        self.buf.emit_byte(rex);
        self.buf.emit_byte(0x85); // TEST r/m64, r64
        self.buf.emit_byte(0xC0 | ((reg & 7) << 3) | (reg & 7));
    }

    /// `TEST BYTE [rip+disp32], imm8` — the RIP-relative sibling of
    /// [`Self::emit_test_mem8_imm8`], for a **fixed absolute address** that is
    /// within ±2GB of the instruction being emitted.
    ///
    /// x86-64 has no `TEST [m64], imm8` form taking a bare 64-bit absolute
    /// address, which is why the safepoint poll materialized its flag address
    /// into R11 first. It does have this one: `F6 /0 ib` with ModRM
    /// `mod=00, rm=101` addresses `[rip + disp32]`, so the whole poll is
    /// **7 bytes and one instruction** instead of `MOV R11, imm64`
    /// (10 bytes) + `TEST BYTE [R11+0], 0xFF` (5 bytes), and it needs no
    /// scratch register at all.
    ///
    /// The RIP the CPU adds `disp32` to is the address of the NEXT
    /// instruction — i.e. past the trailing `imm8`, not past the
    /// displacement. Getting that wrong reads the flag one byte early, which
    /// is a silent wrong answer rather than a fault, so the `+ LEN` below is
    /// load-bearing.
    ///
    /// Returns `false` **without emitting anything** when the target is out of
    /// rel32 reach (the JIT code cache and the VM's data segment are separate
    /// mappings and nothing guarantees they land within 2GB of each other), so
    /// the caller can fall back to the register-materializing form. The reach
    /// test is the same one [`Self::emit_call_absolute`] makes, and rests on
    /// the same fact: `ExecutableBuffer` is allocated once at a fixed capacity
    /// and never relocates, so `as_ptr() + pos()` is already this
    /// instruction's final runtime address.
    pub(super) fn emit_test_mem8_abs_imm8(&mut self, addr: usize, imm8: u8) -> bool {
        // F6 05 <disp32> <imm8>
        const LEN: usize = 7;
        // Cast: non-negative index/count to usize
        let here = self.buf.as_ptr() as usize + self.buf.pos();
        let next_pc = here.wrapping_add(LEN);
        // Widening: i64/usize -> i128 (no truncation, for range check)
        let delta: i128 = (addr as i128) - (next_pc as i128);
        // Widening: i64/usize -> i128 (no truncation, for range check)
        if delta < i32::MIN as i128 || delta > i32::MAX as i128 {
            return false;
        }
        self.buf.emit(&[0xF6, 0x05]); // TEST r/m8, imm8 with ModRM(00, /0, RIP)
        self.rip_abs_disp32_patches.push((self.buf.pos(), 1));
        self.buf.emit(&(delta as i32).to_le_bytes()); // Cast: rel32 displacement
        self.buf.emit_byte(imm8);
        true
    }

    /// `CMP DWORD [rip+disp32], imm32` — the RIP-relative compare against a
    /// **fixed absolute address**, for a 32-bit counter within ±2GB of the
    /// instruction being emitted.
    ///
    /// The epoch guard's whole comparison in **one 10-byte instruction and no
    /// register**, against `MOV R11, imm64` (10 bytes) + `MOV ECX, [R11]`
    /// (3) + `CMP ECX, imm32` (6) — three instructions, nineteen bytes, and
    /// two clobbered registers to read one `u32`. `81 /7 id` with ModRM
    /// `mod=00, rm=101` is the RIP-relative form (`0x3D`).
    ///
    /// The reference point is the end of the WHOLE instruction, past the
    /// trailing `imm32` — which is why `LEN` is 10 and why the
    /// `rip_abs_disp32_patches` entry declares a trail of **4**, not the
    /// poll's 1. Get either wrong and the guard compares an unrelated global
    /// against a baked epoch: no fault, no failing smoke test, just a check
    /// that answers about the wrong word forever.
    ///
    /// The load stays a single aligned 32-bit read, so it is as atomic as the
    /// `MOV ECX` it replaces.
    ///
    /// Returns `false` **without emitting anything** when the target is out of
    /// disp32 reach, so the caller can fall back to the register-materializing
    /// form — same contract, and same reason, as
    /// [`Self::emit_test_mem8_abs_imm8`] above.
    pub(super) fn emit_cmp_mem32_abs_imm32(&mut self, addr: usize, imm32: u32) -> bool {
        // 81 3D <disp32> <imm32>
        const LEN: usize = 10;
        // Cast: non-negative index/count to usize
        let here = self.buf.as_ptr() as usize + self.buf.pos();
        let next_pc = here.wrapping_add(LEN);
        // Widening: i64/usize -> i128 (no truncation, for range check)
        let delta: i128 = (addr as i128) - (next_pc as i128);
        // Widening: i64/usize -> i128 (no truncation, for range check)
        if delta < i32::MIN as i128 || delta > i32::MAX as i128 {
            return false;
        }
        self.buf.emit(&[0x81, 0x3D]); // CMP r/m32, imm32 with ModRM(00, /7, RIP)
        self.rip_abs_disp32_patches.push((self.buf.pos(), 4));
        self.buf.emit(&(delta as i32).to_le_bytes()); // Cast: rel32 displacement
        self.buf.emit(&(imm32 as i32).to_le_bytes()); // Cast: the baked epoch
        true
    }

    /// `TEST BYTE [base+disp], imm8` -- checks a per-object header flag byte
    /// (e.g. `GC_FLAG_COMPACT`) without needing any scratch register: the
    /// memory operand is read and discarded by the CPU, `base` and the
    /// flags register are the only things touched.
    ///
    /// The displacement was previously narrowed with a bare `disp as u8` under
    /// a hard-coded `mod=01` ModRM byte. Every caller passes a header-field
    /// offset that is small today, but the cast is the P0 hazard: at 128 the
    /// operand silently becomes `[base - 128]`. It now goes through
    /// [`Disp::encode_for_base`], which widens to disp32 instead of wrapping
    /// and applies the RBP/R13 zero-displacement rule (`emit_safepoint_poll`
    /// calls this with `disp == 0`, and would mis-encode as RIP-relative if
    /// the flag address ever landed in RBP/R13).
    pub(super) fn emit_test_mem8_imm8(&mut self, base: u8, disp: i32, imm8: u8) {
        // r/m == 100 means "a SIB byte follows"; this form emits none, so an
        // RSP/R12 base has no encoding here. No caller uses one (RAX and R11
        // today) — bail rather than emit an instruction addressing via a
        // fabricated SIB.
        if base_requires_sib(base) {
            self.buf.mark_codegen_unencodable("mem8-base-requires-sib");
            return;
        }
        let Ok(d) = Disp::encode_for_base(disp as i64, base) else {
            self.buf
                .mark_codegen_unencodable("mem8-displacement-unencodable");
            return;
        };
        // This form has always emitted an explicit displacement byte, including
        // for `disp == 0` (the safepoint-flag poll). Keep the `mod=01` shape
        // there rather than shrinking to `mod=00`, so the conversion to the
        // checked encoder is byte-for-byte identical at every existing call
        // site and no instruction length moves under the branch patcher.
        let d = if matches!(d, Disp::None) {
            Disp::Disp8(0)
        } else {
            d
        };
        if base >= 8 {
            self.buf.emit(&[0x41]); // REX.B (extend ModRM.rm to r8-r15)
        }
        self.buf.emit(&[0xF6, d.modrm(0, base)]); // 0xF6 /0 = TEST r/m8, imm8
        let (bytes, len) = d.bytes();
        self.buf.emit(&bytes[..len]);
        self.buf.emit_byte(imm8);
    }

    /// Emit a 32-bit register-to-register ALU op `dst op= src` for the
    /// STRING_SEARCH intrinsics. `opcode` is the primary opcode of the
    /// `r/m32, r32` form (0x01 ADD, 0x29 SUB, 0x39 CMP, 0x89 MOV, 0x31
    /// XOR). ModRM uses mod=11, reg=src, r/m=dst (so the source is the
    /// ModRM.reg field). REX is emitted only when an extended register is
    /// involved (32-bit op, no REX.W).
    pub(super) fn emit_alu_r32_r32(&mut self, opcode: u8, dst: u8, src: u8) {
        if dst >= 8 || src >= 8 {
            let mut rex = 0x40u8;
            if src >= 8 {
                rex |= 0x04; // REX.R for the ModRM.reg (src)
            }
            if dst >= 8 {
                rex |= 0x01; // REX.B for the ModRM.r/m (dst)
            }
            self.buf.emit_byte(rex);
        }
        self.buf.emit_byte(opcode);
        self.buf.emit_byte(0xC0 | ((src & 7) << 3) | (dst & 7));
    }

    /// Emit `Jcc rel32` and return the byte offset of the 4-byte
    /// displacement so the caller can patch it once the branch target
    /// is known. `cc` is the condition-code suffix byte (e.g. 0x84 = JE,
    /// 0x87 = JA, 0x85 = JNE).
    pub(super) fn emit_jcc_rel32_patch(&mut self, cc: u8) -> usize {
        self.buf.emit_byte(0x0F);
        self.buf.emit_byte(cc);
        let patch = self.buf.pos();
        self.buf.emit(&[0u8; 4]);
        patch
    }

    /// Emit `JMP rel32` returning the patch site for the displacement.
    pub(super) fn emit_jmp_rel32_patch(&mut self) -> usize {
        self.buf.emit_byte(0xE9);
        let patch = self.buf.pos();
        self.buf.emit(&[0u8; 4]);
        patch
    }

    /// Patch a `rel8` branch displacement, discarding the whole compile when it
    /// does not fit.
    ///
    /// Truncating instead (`rel as u8`, which is what every one of these sites
    /// used to do) does not produce a wrong-but-harmless branch: it produces a
    /// branch to a DIFFERENT, attacker-irrelevant-but-arbitrary address, and on
    /// x86 that address is usually the middle of an earlier instruction. The
    /// inline-PIC cascade did exactly this — its inter-slot `JNE` wrapped to
    /// `-128` once the per-slot body grew past 127 bytes and landed inside the
    /// pre-call shadow-stack push, turning it into an unguarded infinite push
    /// loop that walked off the end of the thread's shadow buffer. A
    /// `debug_assert!` guarded it, which is to say nothing guarded it: release
    /// is the only build where it matters.
    ///
    /// `mark_overflowed` makes the driver drop the half-emitted method and fall
    /// back, which is always better than emitting the wrong branch.
    ///
    /// The body now lives on [`ExecutableBuffer::patch_rel8_or_bail`] so the
    /// *other* backend can reach it too: `ir_lower` carried eight raw
    /// `(a - b - 1) as u8` patches of its own, i.e. the identical defect one
    /// module over. This stays as the single-pass backend's spelling because
    /// ~60 call sites already use it.
    pub(super) fn patch_rel8_or_bail(buf: &mut ExecutableBuffer, patch: usize, rel: i64) {
        buf.patch_rel8_or_bail(patch, rel);
    }

    /// Patch a previously-emitted `rel32` displacement so it targets the
    /// current buffer position.
    pub(super) fn patch_rel32_to_here(&mut self, patch: usize) {
        let rel = (self.buf.pos() as i32) - (patch as i32 + 4); // Cast: x86-64 rel32 displacement
        self.buf.try_patch_i32(patch, rel).ok(); // on Err try_patch_i32 set buf.overflowed; compile bails
    }

    /// Emit the inline TLAB bump-pointer fast path for the `new` opcode
    /// (HIGH-6 JIT audit, object_allocation/1000 3-5× gap).
    ///
    /// Layout (all rel32 branches, no relocations):
    /// ```text
    ///   MOV  RAX, helpers.get_current_thread
    ///   CALL RAX                            ; RAX = JvmThread*
    ///   TEST RAX, RAX
    ///   JE   slow_path                       ; null thread (re-entrant)
    ///   MOV  R10, RAX                        ; R10 = thread
    ///   MOV  R11, [R10 + tlab_cursor_off]    ; R11 = cursor
    ///   LEA  RAX, [R11 + total_size]         ; RAX = new cursor
    ///   CMP  RAX, [R10 + tlab_end_off]
    ///   JA   slow_path                       ; TLAB full
    ///   MOV  DWORD [R11 + 0], class_id_imm   ; write class_id  (header FIRST)
    ///   MOV  DWORD [R11 + 4], 0              ; kind=Object/elem=Ref/pad
    ///   MOV  DWORD [R11 + 12], 0             ; array_length=0
    ///   MOV  DWORD [R11 + 16], num_fields    ; num_slots (walker stride)
    ///   MOV  [R10 + tlab_cursor_off], RAX    ; commit LAST (publish object)
    ///   ; Hand off to post-init helper which finishes header + primitive
    ///   ; defaults + finalizer registration.
    ///   MOV  ARG0, [RBP - heap_local_off]    ; vm_ptr
    ///   MOV  ARG1, R11                       ; obj_ptr
    ///   MOV  ARG2, class_id_imm
    ///   MOV  ARG3, num_fields_imm
    ///   CALL helpers.tlab_post_init
    ///   JMP  done
    /// slow_path:
    ///   MOV  ARG0, [RBP - heap_local_off]
    ///   MOV  ARG1, class_id_imm
    ///   MOV  ARG2, num_fields_imm
    ///   CALL helpers.new_object
    /// done:
    /// ```
    ///
    /// Returns the bumped object pointer (or the slow-path result) in RAX.
    /// Caller emits the safepoint oop map and pushes RAX.
    /// Guarded inline `getfield` receiver check (see
    /// [`guarded_inline_getfield_enabled`]). The receiver must already be in
    /// RAX. Emits, in order:
    ///
    ///   1. `TEST RAX,RAX; JZ slow` — null receiver takes the helper path so
    ///      the checked helper's NPE semantics (pending-NPE + `i64::MIN`
    ///      sentinel) are preserved bit-for-bit.
    ///   2. `MOV RCX,RAX; AND RCX,7; JNZ slow` — object headers are 8-byte
    ///      aligned; truncated/garbage receiver bits with low bits set can
    ///      never be a real object (mirrors `plausible_heap_pointer`).
    ///   3. Three `[base, end)` containment checks against the GC's
    ///      process-global `JIT_REGION_BOUNDS` table (young-from / young-to /
    ///      old), the same first gate `jit_getfield`'s `is_object_address`
    ///      applies. An empty or unpublished region is `[0, 0)` and matches
    ///      nothing, so a backend that doesn't publish bounds sends every
    ///      receiver down the helper path.
    ///
    /// On fall-through the receiver points inside a live arena, whose backing
    /// stays mapped for the heap's lifetime — a raw field-cell load cannot
    /// fault, so the caller may emit the direct MOV sequence. Returns the
    /// patch offsets that the caller MUST patch to its helper slow path.
    /// Clobbers RCX and RDX; preserves RAX (the receiver).
    /// Emit `CMP reg, [rbp - offset]` against a frame local (same rbp-relative
    /// addressing as [`Self::emit_load_local`]). Used by the inline self-call
    /// stack check (`CMP RSP, [rbp - floor_slot]`).
    pub(super) fn emit_cmp_r64_rbp_local(&mut self, reg: u8, offset: i32) {
        self.rex_w_r(reg);
        self.buf.emit_byte(0x3B); // CMP r64, r/m64
        self.modrm_rbp_disp(reg, offset);
    }

    /// CMP r32_a, r32_b — sets flags for signed integer comparison.
    pub(super) fn emit_cmp_r32_r32(&mut self, a: u8, b: u8) {
        // CMP r/m32, r32 → opcode 0x39, ModRM(11, b, a)
        // Need REX if either register >= 8
        let need_rex = a >= 8 || b >= 8;
        if need_rex {
            let mut rex = 0x40u8;
            if b >= 8 {
                rex |= 0x04;
            } // REX.R
            if a >= 8 {
                rex |= 0x01;
            } // REX.B
            self.buf.emit_byte(rex);
        }
        self.buf.emit_byte(0x39);
        self.buf.emit_byte(0xC0 | ((b & 7) << 3) | (a & 7));
    }

    /// CMP r64_a, r64_b — full-width unsigned/pointer comparison.
    pub(super) fn emit_cmp_r64_r64(&mut self, a: u8, b: u8) {
        let mut rex = 0x48u8; // REX.W
        if b >= 8 {
            rex |= 0x04;
        }
        if a >= 8 {
            rex |= 0x01;
        }
        self.buf.emit_byte(rex);
        self.buf.emit_byte(0x39);
        self.buf.emit_byte(0xC0 | ((b & 7) << 3) | (a & 7));
    }

    /// TEST r32, r32 — sets ZF and SF from the value.
    pub(super) fn emit_test_r32_r32(&mut self, reg: u8) {
        let need_rex = reg >= 8;
        if need_rex {
            let rex = 0x40u8 | if reg >= 8 { 0x04 | 0x01 } else { 0 };
            self.buf.emit_byte(rex);
        }
        self.buf.emit_byte(0x85); // TEST r/m32, r32
        self.buf.emit_byte(0xC0 | ((reg & 7) << 3) | (reg & 7));
    }
}
