// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Vectorised and bulk loop bodies.
//!
//! Whole-loop replacements, not instruction emitters: each method here
//! recognises that a loop the walk is about to compile has a shape it can emit
//! as a strided AVX2 or bulk-store sequence, and emits the whole thing plus
//! its scalar tail.
//!
//! Admission is decided elsewhere (`x64/simd_analysis.rs`, `x64/licm.rs`) and
//! the raw VEX encodings live in `x64/emit.rs`. What is left here is the
//! shape: preheader, vector body, tail, and the reduction that joins them.

use super::*;

impl Compiler {
    // -----------------------------------------------------------------------
    // Arithmetic: peepholes, division, comparisons
    // -----------------------------------------------------------------------
    //
    // Moved to `x64/arith.rs`.

    /// Emit a vectorized int-array sum loop.
    /// Replaces the scalar loop with AVX2 code that processes 8 int elements at a time.
    /// Assumes: RCX = array base ptr, R10D = start index, R11D = bound (exclusive count).
    /// Result: accumulator value added to long local via frame slot.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn emit_simd_int_array_sum(&mut self, acc_local_offset: i32, acc_is_long: bool) {
        // RCX = array base address (already loaded)
        // R10D = current index (i)
        // R11D = loop bound (n)
        // Accumulator is in frame slot at acc_local_offset

        // --- Compute number of SIMD iterations ---
        // R8D = (n - i) / 8 = number of full 8-element chunks
        // 0x44, 0x89: MOV EAX, R11D;  SUB EAX, R10D; SHR EAX, 3
        self.buf.emit(&[0x44, 0x89, 0xD8]); // MOV EAX, R11D
        self.buf.emit(&[0x44, 0x29, 0xD0]); // SUB EAX, R10D
        // Entered with i >= n the difference is zero or negative, and an
        // unsigned SHR of a negative difference is ~2^29 chunks — every one
        // of them past the array. Clamp to zero on the SIGNED flags of the
        // SUB (JGE honours OF, so a wrapped `n - i` still reads as n < i),
        // leaving the scalar loop's own `i < n` test to run zero iterations.
        self.buf.emit(&[0x7D, 0x02]); // JGE +2
        self.buf.emit(&[0x31, 0xC0]); // XOR EAX, EAX
        self.buf.emit(&[0xC1, 0xE8, 0x03]); // SHR EAX, 3
        self.buf.emit(&[0x41, 0x89, 0xC0]); // MOV R8D, EAX — chunk count
        self.buf.emit(&[0x45, 0x85, 0xC0]); // TEST R8D, R8D
                                            // JZ to scalar cleanup (patch later)
        self.buf.emit_byte(0x0F);
        self.buf.emit_byte(0x84);
        let simd_skip_patch = self.buf.pos();
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);

        // --- SIMD loop ---
        // VPXOR YMM0, YMM0, YMM0 — zero accumulator
        self.emit_vpxor_ymm(0, 0, 0);

        // Compute base address: RAX = RCX + R10 * 4 + HEADER_SIZE
        // (array elements start at RCX + HEADER_SIZE, each int is 4 bytes)
        self.buf.emit(&[0x4C, 0x89, 0xD0]); // MOV RAX, R10  (R10 is i)
        self.buf.emit(&[0xC1, 0xE0, 0x02]); // SHL EAX, 2  (i * 4)
        self.buf.emit(&[0x48, 0x01, 0xC8]); // ADD RAX, RCX  (array base + i*4)
                                            // ADD RAX, HEADER_SIZE
        self.buf.emit(&[0x48, 0x05]); // ADD RAX, imm32
        self.buf.emit(&(HEADER_SIZE as i32).to_le_bytes()); // Cast: x86-64 immediate encoding

        // The lanes are 64-bit. The detector admits the `long` accumulator
        // shape (`s = a[i] + s` after `i2l`), and 32-bit lanes plus a 32-bit
        // horizontal sum wrapped at 2^32 before the result was sign-extended
        // into the long: eight `Integer.MAX_VALUE` elements summed to -8.
        // Each 8-int chunk is widened as two 4-int halves (VPMOVSXDQ reads a
        // 128-bit memory operand), so every lane holds a sign-extended int and
        // the accumulator cannot wrap before 2^31 chunks. The low 32 bits of
        // the 64-bit total are exactly the wrapped `int` sum, so the int
        // accumulator arm below needs no separate path.
        let simd_loop_start = self.buf.pos();
        for half_disp in [0i32, 16] {
            // VPMOVSXDQ YMM1, [RAX + half_disp]   VEX.256.66.0F38.WIG 25 /r
            self.emit_vex3(true, true, true, 0x02, false, 0, true, 1);
            self.buf.emit_byte(0x25);
            if half_disp == 0 {
                self.buf.emit_byte(0x08); // mod=00 reg=YMM1 rm=RAX
            } else {
                self.buf.emit_byte(0x48); // mod=01 reg=YMM1 rm=RAX
                self.buf.emit_byte(half_disp as u8); // Cast: disp8 16, in range
            }
            // VPADDQ YMM0, YMM0, YMM1             VEX.256.66.0F.WIG D4 /r
            self.emit_vex2(true, 0, true, 1);
            self.buf.emit_byte(0xD4);
            self.buf.emit_byte(0xC1); // mod=11 reg=YMM0 rm=YMM1
        }
        // ADD RAX, 32  (advance by 8 ints × 4 bytes)
        self.buf.emit(&[0x48, 0x83, 0xC0, 0x20]);
        // DEC R8D
        self.buf.emit(&[0x41, 0xFF, 0xC8]);
        // JNZ simd_loop_start
        let rel = (simd_loop_start as i32) - (self.buf.pos() as i32 + 6); // Cast: x86-64 rel32 displacement
        self.buf.emit_byte(0x0F);
        self.buf.emit_byte(0x85);
        self.buf.emit(&rel.to_le_bytes());

        // --- Horizontal reduction: four qword lanes of YMM0 → RAX ---
        // VEXTRACTI128 XMM1, YMM0, 1           VEX.256.66.0F3A.W0 39 /r ib
        self.emit_vex3(true, true, true, 0x03, false, 0, true, 1);
        self.buf.emit(&[0x39, 0xC1, 0x01]); // mod=11 reg=YMM0 rm=XMM1, imm8=1
        // VPADDQ XMM0, XMM0, XMM1              VEX.128.66.0F.WIG D4 /r
        self.emit_vex2(true, 0, false, 1);
        self.buf.emit(&[0xD4, 0xC1]);
        // VPSHUFD XMM1, XMM0, 0x4E — high qword into the low lane
        self.emit_vex2(true, 0, false, 1);
        self.buf.emit(&[0x70, 0xC8, 0x4E]); // mod=11 reg=XMM1 rm=XMM0
        // VPADDQ XMM0, XMM0, XMM1
        self.emit_vex2(true, 0, false, 1);
        self.buf.emit(&[0xD4, 0xC1]);
        // VMOVQ RAX, XMM0                      VEX.128.66.0F.W1 7E /r
        self.emit_vex3(true, true, true, 0x01, true, 0, false, 1);
        self.buf.emit(&[0x7E, 0xC0]);

        // VZEROUPPER
        self.emit_vzeroupper();

        // Add SIMD result to accumulator
        if acc_is_long {
            // ADD [RBP + acc_offset], RAX (64-bit add to long local)
            self.rex_w();
            self.buf.emit_byte(0x01); // ADD r/m64, r64
            self.modrm_rbp_disp(RAX, acc_local_offset);
        } else {
            // ADD [RBP + acc_offset], EAX (32-bit add to int local)
            self.buf.emit_byte(0x01); // ADD r/m32, r32
            self.modrm_rbp_disp(RAX, acc_local_offset);
        }

        // Update induction variable: i += chunks_processed * 8
        // We need to know how many were processed. R10D was the start.
        // The scalar loop will pick up from the new i value.
        // Actually: the simd loop processed (original R8D) * 8 elements.
        // New i = old i + (original chunk_count * 8)
        // But R8D is now 0. Let's track differently.
        // Before SIMD loop, EAX had chunk_count. Let's save it.
        // Actually, let's just compute: new_i = old_i + (((n-old_i)/8)*8)
        // which is: new_i = n - (n - old_i) % 8
        // Simpler: after the simd loop pointer, compute i from pointer:
        //   bytes_consumed = (RAX_now - RAX_start) = chunks * 32
        //   elements_consumed = bytes_consumed / 4 = chunks * 8
        //   new_i = old_i + elements_consumed

        // Patch the skip jump target
        let after_simd = self.buf.pos();
        let skip_rel = (after_simd as i32) - (simd_skip_patch as i32 + 4); // Cast: x86-64 rel32 displacement
        let pos = simd_skip_patch;
        self.buf.try_patch_i32(pos, skip_rel).ok(); // on Err try_patch_i32 set buf.overflowed; compile bails

        // Now set up for scalar cleanup:
        // R10D needs to be updated to: old_i + num_simd_elements
        // Since we already did the pointer math, recalculate:
        // We computed chunk_count = (n - i) / 8 before the loop.
        // After SIMD: new_i = old_i + chunk_count * 8
        // Recompute: EAX = (R11D - R10D) >> 3 << 3; R10D += EAX
        self.buf.emit(&[0x44, 0x89, 0xD8]); // MOV EAX, R11D
        self.buf.emit(&[0x44, 0x29, 0xD0]); // SUB EAX, R10D
        // The same signed clamp as the chunk count: for i > n, `(n - i) & ~7`
        // is a NEGATIVE multiple of 8, which would restart the scalar tail
        // below i and read a[i - 8k] — before the array.
        self.buf.emit(&[0x7D, 0x02]); // JGE +2
        self.buf.emit(&[0x31, 0xC0]); // XOR EAX, EAX
        self.buf.emit(&[0x83, 0xE0, 0xF8]); // AND EAX, ~7 (round down to multiple of 8)
        self.buf.emit(&[0x41, 0x01, 0xC2]); // ADD R10D, EAX

        // --- Scalar cleanup loop ---
        // for (i = new_i; i < n; i++) sum += arr[i]
        let scalar_loop_start = self.buf.pos();
        // CMP R10D, R11D
        self.buf.emit(&[0x45, 0x39, 0xDA]); // CMP R10D, R11D
                                            // JGE end
        self.buf.emit_byte(0x0F);
        self.buf.emit_byte(0x8D);
        let scalar_end_patch = self.buf.pos();
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);

        // Load arr[i]: MOV EAX, [RCX + R10*4 + HEADER_SIZE]
        // SIB addressing: base=RCX, index=R10, scale=4
        self.buf.emit(&[0x42, 0x8B, 0x84, 0x91]); // MOV EAX, [RCX + R10*4 + disp32]
        self.buf.emit(&(HEADER_SIZE as i32).to_le_bytes()); // Cast: x86-64 immediate encoding

        // Add to accumulator
        if acc_is_long {
            self.rex_w();
            self.buf.emit(&[0x63, 0xC0]); // MOVSXD RAX, EAX
            self.rex_w();
            self.buf.emit_byte(0x01);
            self.modrm_rbp_disp(RAX, acc_local_offset);
        } else {
            self.buf.emit_byte(0x01);
            self.modrm_rbp_disp(RAX, acc_local_offset);
        }

        // INC R10D
        self.buf.emit(&[0x41, 0xFF, 0xC2]);
        // JMP scalar_loop_start
        let rel2 = (scalar_loop_start as i32) - (self.buf.pos() as i32 + 5); // Cast: x86-64 rel32 displacement
        self.buf.emit_byte(0xE9);
        self.buf.emit(&rel2.to_le_bytes());

        // Patch scalar end
        let scalar_end = self.buf.pos();
        let end_rel = (scalar_end as i32) - (scalar_end_patch as i32 + 4); // Cast: x86-64 rel32 displacement
        self.buf.try_patch_i32(scalar_end_patch, end_rel).ok(); // on Err try_patch_i32 set buf.overflowed; compile bails
    }

    /// Emit one checked matrix-dot element and advance R10D.
    ///
    /// The preheader keeps all Java-visible state in its original homes until
    /// the complete dot product succeeds, so a guard can safely restart the
    /// scalar bytecodes even when this element belongs to an unrolled batch.
    fn emit_matrix_dot_element(
        &mut self,
        scalar_fallbacks: &mut Vec<usize>,
        index_delta: i32,
        advance_iv: bool,
    ) {
        let b_disp = HEADER_SIZE as i32 + index_delta * if narrow_oops_enabled() { 4 } else { 8 };
        let a_disp = HEADER_SIZE as i32 + index_delta * 4;
        // Both displacements are baked under a hard-coded `mod=01` ModRM byte
        // AND grow with the unroll index — the only computed disp8s left in
        // this file. With the batch of 8 the widest is `HEADER_SIZE + 7*8`
        // (88 today), comfortably inside the byte; a wider unroll or a larger
        // object header walks them past 127, where the previous `as u8` would
        // have silently addressed memory BEFORE the array. Require the disp8
        // form explicitly and discard the method otherwise. Neither value can
        // be zero (both are at least `HEADER_SIZE`), so `Disp::None` — which
        // would need a mod=00 ModRM byte this emitter does not write — cannot
        // arise here.
        let b_disp8 = Disp::encode(b_disp as i64).ok().and_then(Disp::as_disp8);
        let a_disp8 = Disp::encode(a_disp as i64).ok().and_then(Disp::as_disp8);
        let (Some(b_disp8), Some(a_disp8)) = (b_disp8, a_disp8) else {
            self.buf.mark_codegen_unencodable("simd-disp8-unencodable");
            return;
        };
        if narrow_oops_enabled() {
            // EAX = narrow b[k], then decode it with the loop-invariant heap
            // base already in R11. A zero encoding is null -> scalar fallback.
            self.buf.emit(&[
                0x43,
                0x8B,
                0x44,
                0x95,
                b_disp8 as u8, // Cast: range-checked disp8 above, reinterpreted signed
            ]); // MOV EAX,[R13+R10*4+H]
            self.buf.emit(&[0x48, 0xC1, 0xE0, 0x03]); // SHL RAX,3
            scalar_fallbacks.push(self.emit_jcc_rel32_patch(0x84)); // JZ
            self.buf.emit(&[0x4C, 0x01, 0xD8]); // ADD RAX,R11
        } else {
            self.buf.emit(&[
                0x4B,
                0x8B,
                0x44,
                0xD5,
                b_disp8 as u8, // Cast: range-checked disp8 above, reinterpreted signed
            ]); // MOV RAX,[R13+R10*8+H]
            self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX,RAX
            scalar_fallbacks.push(self.emit_jcc_rel32_patch(0x84)); // JZ
        }

        // Each B row is independently mutable in Java, so its column check
        // remains per element. A failure restarts the exact scalar body, which
        // raises NPE/AIOOBE at the original bytecode.
        self.buf.emit(&[0x8B, 0x50, ARRAY_LENGTH_OFFSET as u8]); // MOV EDX,[RAX+len]
        self.buf.emit(&[0x41, 0x39, 0xD6]); // CMP R14D, EDX
        scalar_fallbacks.push(self.emit_jcc_rel32_patch(0x83)); // JAE

        self.buf.emit(&[
            0x43,
            0x8B,
            0x54,
            0x94,
            a_disp8 as u8, // Cast: range-checked disp8 above, reinterpreted signed
        ]); // MOV EDX,[R12+R10*4+H]
        self.buf
            .emit(&[0x42, 0x0F, 0xAF, 0x54, 0xB0, HEADER_SIZE as u8]); // IMUL EDX,[RAX+R14*4+H]
        self.buf.emit(&[0x41, 0x01, 0xD1]); // ADD R9D, EDX (Java int wrap)
        if advance_iv {
            self.buf.emit(&[0x41, 0xFF, 0xC2]); // INC R10D
        }
    }

    /// Emit the guarded tight loop for a [`MatrixDotLoop`].
    ///
    /// Register contract:
    /// - R12 = `a[row]`
    /// - R13 = outer `b`
    /// - R14D = column
    /// - R15D = exclusive bound
    /// - R10D = induction variable
    /// - R9D = wrapping int accumulator
    ///
    /// R12..R15 are reserved from Java-local allocation and saved by the
    /// method prologue.  RAX/RCX/RDX/R11 are ordinary emitter scratch.
    pub(super) fn emit_matrix_dot_preheader(&mut self, dot: &MatrixDotLoop) {
        let mut scalar_fallbacks = Vec::new();

        // Load the carried integer state.  Frame/register homes are left
        // untouched until successful completion, so every failing guard can
        // restart the original scalar loop without reconstructing state.
        if let Some(reg) = self.reg_for_local(dot.iv_local) {
            self.emit_mov_reg_reg(R10, reg);
        } else {
            self.emit_load_local(R10, self.local_offset(dot.iv_local));
        }
        if let Some(reg) = self.reg_for_local(dot.bound_local) {
            self.emit_mov_reg_reg(R15, reg);
        } else {
            self.emit_load_local(R15, self.local_offset(dot.bound_local));
        }
        if let Some(reg) = self.reg_for_local(dot.acc_local) {
            self.emit_mov_reg_reg(R9, reg);
        } else {
            self.emit_load_local(R9, self.local_offset(dot.acc_local));
        }

        // Zero trip: do not even inspect the arrays.
        self.buf.emit(&[0x45, 0x39, 0xFA]); // CMP R10D, R15D
        scalar_fallbacks.push(self.emit_jcc_rel32_patch(0x8D)); // JGE scalar header
        self.buf.emit(&[0x45, 0x85, 0xD2]); // TEST R10D, R10D
        scalar_fallbacks.push(self.emit_jcc_rel32_patch(0x88)); // JS scalar header

        // Resolve a[row]. Prefer the LICM result that was emitted immediately
        // before this preheader; fall back to a guarded direct load when LICM
        // is disabled or this header was de-specialized.
        let hoisted_a = self
            .hoist_info
            .iter()
            .enumerate()
            .find(|(_, h)| {
                h.loop_header == dot.header_pc
                    && h.array_local == dot.a_outer_local
                    && h.index_local == dot.a_row_local
            })
            .map(|(idx, _)| self.hoist_offsets[idx]);
        if let Some(off) = hoisted_a {
            self.emit_load_local(RAX, off);
        } else {
            if let Some(reg) = self.reg_for_local(dot.a_outer_local) {
                self.emit_mov_reg_reg(RAX, reg);
            } else {
                self.emit_load_local(RAX, self.local_offset(dot.a_outer_local));
            }
            if let Some(reg) = self.reg_for_local(dot.a_row_local) {
                self.emit_mov_reg_reg(RCX, reg);
            } else {
                self.emit_load_local(RCX, self.local_offset(dot.a_row_local));
            }
            self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
            scalar_fallbacks.push(self.emit_jcc_rel32_patch(0x84)); // JZ
            self.buf.emit(&[0x8B, 0x50, ARRAY_LENGTH_OFFSET as u8]); // MOV EDX,[RAX+len]
            self.buf.emit(&[0x39, 0xD1]); // CMP ECX, EDX
            scalar_fallbacks.push(self.emit_jcc_rel32_patch(0x83)); // JAE
            self.emit_ref_aload_regs();
        }
        self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX (a[row])
        scalar_fallbacks.push(self.emit_jcc_rel32_patch(0x84)); // JZ
        self.emit_mov_reg_reg(R12, RAX);

        // Resolve the outer B array and the invariant column.
        if let Some(reg) = self.reg_for_local(dot.b_outer_local) {
            self.emit_mov_reg_reg(R13, reg);
        } else {
            self.emit_load_local(R13, self.local_offset(dot.b_outer_local));
        }
        self.buf.emit(&[0x4D, 0x85, 0xED]); // TEST R13, R13
        scalar_fallbacks.push(self.emit_jcc_rel32_patch(0x84)); // JZ
        if let Some(reg) = self.reg_for_local(dot.b_column_local) {
            self.emit_mov_reg_reg(R14, reg);
        } else {
            self.emit_load_local(R14, self.local_offset(dot.b_column_local));
        }

        // A's selected row and the outer B array must both cover the complete
        // counted range.  JA (not JAE): length == bound is valid.
        self.buf
            .emit(&[0x41, 0x8B, 0x54, 0x24, ARRAY_LENGTH_OFFSET as u8]); // MOV EDX,[R12+len]
        self.buf.emit(&[0x41, 0x39, 0xD7]); // CMP R15D, EDX
        scalar_fallbacks.push(self.emit_jcc_rel32_patch(0x87)); // JA
        self.buf
            .emit(&[0x41, 0x8B, 0x55, ARRAY_LENGTH_OFFSET as u8]); // MOV EDX,[R13+len]
        self.buf.emit(&[0x41, 0x39, 0xD7]); // CMP R15D, EDX
        scalar_fallbacks.push(self.emit_jcc_rel32_patch(0x87)); // JA

        if narrow_oops_enabled() {
            self.emit_mov_imm64(R11, narrow_base() as i64);
        }

        // Eight-way unrolling amortizes the induction compare/branch while
        // retaining the original k-order for every multiply-add. Fixed
        // displacements let the CPU overlap independent row-pointer loads
        // without an INC dependency between elements. R8D is the
        // first k that cannot start a full batch (`bound - 7`).
        self.emit_mov_reg_reg(R8, R15);
        self.buf.emit(&[0x41, 0x83, 0xE8, 0x07]); // SUB R8D,7
        self.buf.emit(&[0x45, 0x39, 0xC2]); // CMP R10D,R8D
        let scalar_tail_patch = self.emit_jcc_rel32_patch(0x8D); // JGE scalar tail

        let batch_start = self.buf.pos();
        for index_delta in 0..8 {
            self.emit_matrix_dot_element(&mut scalar_fallbacks, index_delta, false);
        }
        self.buf.emit(&[0x41, 0x83, 0xC2, 0x08]); // ADD R10D,8
        self.buf.emit(&[0x45, 0x39, 0xC2]); // CMP R10D,R8D
        self.buf.emit(&[0x0F, 0x8C]); // JL batch_start
        let batch_patch = self.buf.pos();
        self.buf.emit(&[0u8; 4]);
        let batch_rel = (batch_start as i32) - (batch_patch as i32 + 4);
        self.buf.try_patch_i32(batch_patch, batch_rel).ok();

        self.patch_rel32_to_here(scalar_tail_patch);
        self.buf.emit(&[0x45, 0x39, 0xFA]); // CMP R10D,R15D
        let publish_patch = self.emit_jcc_rel32_patch(0x8D); // JGE publish

        let scalar_start = self.buf.pos();
        self.emit_matrix_dot_element(&mut scalar_fallbacks, 0, true);
        self.buf.emit(&[0x45, 0x39, 0xFA]); // CMP R10D, R15D
        self.buf.emit(&[0x0F, 0x8C]); // JL scalar_start
        let loop_patch = self.buf.pos();
        self.buf.emit(&[0u8; 4]);
        let loop_rel = (scalar_start as i32) - (loop_patch as i32 + 4);
        self.buf.try_patch_i32(loop_patch, loop_rel).ok();

        // Publish successful final state to whichever homes the scalar
        // continuation expects. MOVSXD restores the backend's signed-i32 local
        // representation after the wrapping 32-bit accumulator arithmetic.
        self.patch_rel32_to_here(publish_patch);
        self.buf.emit(&[0x4D, 0x63, 0xC9]); // MOVSXD R9,R9D
        if let Some(reg) = self.reg_for_local(dot.acc_local) {
            self.emit_mov_reg_reg(reg, R9);
        } else {
            self.emit_store_local(self.local_offset(dot.acc_local), R9);
        }
        if let Some(reg) = self.reg_for_local(dot.iv_local) {
            self.emit_mov_reg_reg(reg, R10);
        } else {
            self.emit_store_local(self.local_offset(dot.iv_local), R10);
        }

        // All failed guards land after the success stores, leaving their
        // original frame/register state untouched for the scalar bytecode.
        for patch in scalar_fallbacks {
            self.patch_rel32_to_here(patch);
        }
    }

    // -----------------------------------------------------------------------
    // T17.Β.3 — Loop unswitch emission
    // -----------------------------------------------------------------------

    /// Emit the preheader evaluation for a loop-unswitch candidate
    /// whose header starts at `header_pc`.
    ///
    /// The output is a single `MOV/LOAD + TEST`-style probe that
    /// evaluates the invariant local and sets flags according to the
    /// branch's semantics (ifeq / ifne / iflt / ifge / ifgt / ifle).
    /// Execution never mutates any Java-visible state, so semantics
    /// are strictly additive — the code compiled with unswitch
    /// detection enabled produces the same final state as the
    /// unmodified version.
    ///
    /// # Gate
    ///
    /// Detection already enforces `body_size <= MAX_UNSWITCH_BYTECODES`;
    /// we reassert here so emission silently falls back to the
    /// scalar path if the precondition is violated.
    pub(super) fn emit_loop_unswitch_preheader(&mut self, header_pc: usize) {
        // Clone to avoid aliasing self.
        let candidate = self
            .loop_unswitch_candidates
            .iter()
            .find(|c| c.header_pc == header_pc)
            .cloned();
        let Some(cand) = candidate else {
            return;
        };
        // Defensive gate — detection already checks this, but we
        // re-apply so the emission path is self-contained.
        let body_size = cand.back_edge_pc.saturating_sub(cand.header_pc);
        if body_size == 0 || body_size > MAX_UNSWITCH_BYTECODES {
            return;
        }

        // Load the invariant local into EAX.
        // Prefer a register-resident copy when the regalloc placed
        // the local in a caller-saved GPR; otherwise fall back to
        // the frame slot.
        if let Some(reg) = self.reg_for_local(cand.invariant_local) {
            self.emit_mov_reg_reg(RAX, reg);
        } else {
            self.emit_load_local(RAX, self.local_offset(cand.invariant_local));
        }

        // Set flags for the branch. For the unary-branch opcodes in
        // scope (0x99..=0x9E), TEST EAX, EAX covers eq/ne and a CMP
        // against 0 covers lt/ge/gt/le (TEST sets SF/ZF the same
        // way, so we can use a single TEST for all six).
        //
        // TEST EAX, EAX — 85 C0
        self.buf.emit(&[0x85, 0xC0]);

        // No jump is emitted. The per-iteration branch inside the
        // body will re-evaluate the predicate and take the correct
        // side; the pre-evaluation primes the branch predictor so
        // the in-loop check is ~always correctly predicted.
        //
        // The branch_op is captured for future body-duplication
        // emission variants; consume it here to silence unused
        // warnings and document the contract.
        debug_assert!(
            matches!(cand.branch_op, 0x99..=0x9E),
            "detector rejects non-unary branches (0x99..=0x9E)"
        );
    }

    /// Emit a vectorized int-array element-wise loop:
    ///
    /// ```text
    /// for (i = R10D; i < R11D; i++) OUT[i] = A[i] OP B[i]
    /// ```
    ///
    /// Input register allocation:
    /// - RAX = base of A
    /// - RCX = base of B
    /// - RDX = base of OUT
    /// - R10D = start index (i)
    /// - R11D = bound (n, exclusive)
    ///
    /// Strategy:
    /// - Phase 1 (AVX2 8-wide batch): process 8 elements per iteration
    ///   while `(n - i) >= 8`. Uses YMM0 as `A[i..i+8]` register, then
    ///   applies `OP` with `[RCX + i*4 + H]` straight from memory, and
    ///   stores the result via VMOVDQU to `[RDX + i*4 + H]`.
    /// - Phase 2 (scalar remainder): single-element loop for the final
    ///   `(n - i) % 8` elements — never reads past the array end.
    ///
    /// # Correctness invariants
    ///
    /// - The AVX2 batch loop *stops* strictly before `n - 8`, so the
    ///   256-bit load/store never crosses the end of the array.
    /// - The scalar tail is bytecode-equivalent to the original shape
    ///   (`iaload a; iaload b; i*op; iastore out`).
    /// - `VZEROUPPER` is emitted before the scalar path so legacy SSE
    ///   isn't penalized by a dirty AVX state.
    ///
    /// # Safety
    ///
    /// Call sites already clamped `i` to a 32-bit nonneg integer and
    /// ensured `n <= len(OUT), len(A), len(B)` via the existing bounds
    /// analysis (`bounds_safe_pcs`) or a speculative BCE guard.
    pub(super) fn emit_simd_int_array_element_wise(&mut self, op: ElementWiseOp) {
        // --- Compute chunk_count = (n - i) >> 3 into R8D ---
        //
        // This must NOT route through EAX: the caller leaves array A's base
        // pointer in RAX (B in RCX, OUT in RDX), and that base is read by
        // the preheader's `ADD R9, RAX` below and by the scalar-remainder
        // `MOV EAX, [RAX + R10*4 + H]`. Using EAX as scratch here would
        // overwrite A's base with the chunk count, so `&A[i]` degenerates to
        // `i*4 + chunk_count + H` and the first VMOVDQU faults.
        self.buf.emit(&[0x45, 0x89, 0xD8]); // MOV R8D, R11D
        self.buf.emit(&[0x45, 0x29, 0xD0]); // SUB R8D, R10D
        // Signed clamp for an i >= n entry; see `emit_simd_int_array_sum`.
        self.buf.emit(&[0x7D, 0x03]); // JGE +3
        self.buf.emit(&[0x45, 0x31, 0xC0]); // XOR R8D, R8D
        self.buf.emit(&[0x41, 0xC1, 0xE8, 0x03]); // SHR R8D, 3
        self.buf.emit(&[0x45, 0x85, 0xC0]); // TEST R8D, R8D
                                            // JZ to scalar remainder (patch later)
        self.buf.emit_byte(0x0F);
        self.buf.emit_byte(0x84);
        let simd_skip_patch = self.buf.pos();
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);

        // --- AVX2 batch loop ---
        // Compute byte offset into arrays: EAX = i*4, stays in RDI so
        // the SIB-style `[base + offset]` addressing is a plain disp32
        // (we recompute pointer-adjusted base regs instead).
        //
        // Strategy: materialize pointers once, advance them by 32 per
        // iteration so the inner loop body stays small.
        //
        //   R9  = &A[i]    (= RAX + R10*4 + H)
        //   R12 = &B[i]    (= RCX + R10*4 + H)
        //   R13 = &OUT[i]  (= RDX + R10*4 + H)
        //
        // R12/R13 are callee-saved; they were recorded as saved in the
        // prologue via `force_callee_saved_live` so regalloc doesn't
        // reuse them. But element-wise emission runs as a pre-header
        // before the scalar loop body, so we need to preserve them.
        //
        // To keep this self-contained we use R9 and scratch via push/pop
        // of R12/R13.

        // Save R12, R13 on the stack (callee-saved — must be restored).
        // PUSH R12 (41 54), PUSH R13 (41 55)
        self.buf.emit(&[0x41, 0x54]);
        self.buf.emit(&[0x41, 0x55]);

        // &A[i]: R9 = RAX + R10*4 + H
        // MOV R9, R10 (4D 89 D1)
        self.buf.emit(&[0x4D, 0x89, 0xD1]);
        // SHL R9, 2  (49 C1 E1 02) — R9 = i * 4
        self.buf.emit(&[0x49, 0xC1, 0xE1, 0x02]);
        // ADD R9, RAX  (49 01 C1) — R9 = RAX + i*4
        self.buf.emit(&[0x49, 0x01, 0xC1]);
        // ADD R9, HEADER_SIZE  (49 81 C1 imm32)
        self.buf.emit(&[0x49, 0x81, 0xC1]);
        // Cast: fixed struct/layout offset to i32 instruction displacement
        self.buf.emit(&(HEADER_SIZE as i32).to_le_bytes());

        // &B[i]: R12 = RCX + R10*4 + H
        self.buf.emit(&[0x4D, 0x89, 0xD4]); // MOV R12, R10
        self.buf.emit(&[0x49, 0xC1, 0xE4, 0x02]); // SHL R12, 2
        self.buf.emit(&[0x49, 0x01, 0xCC]); // ADD R12, RCX
        self.buf.emit(&[0x49, 0x81, 0xC4]); // ADD R12, imm32
                                            // Cast: fixed struct/layout offset to i32 instruction displacement
        self.buf.emit(&(HEADER_SIZE as i32).to_le_bytes());

        // &OUT[i]: R13 = RDX + R10*4 + H
        self.buf.emit(&[0x4D, 0x89, 0xD5]); // MOV R13, R10
        self.buf.emit(&[0x49, 0xC1, 0xE5, 0x02]); // SHL R13, 2
        self.buf.emit(&[0x49, 0x01, 0xD5]); // ADD R13, RDX
        self.buf.emit(&[0x49, 0x81, 0xC5]); // ADD R13, imm32
                                            // Cast: fixed struct/layout offset to i32 instruction displacement
        self.buf.emit(&(HEADER_SIZE as i32).to_le_bytes());

        let simd_loop_start = self.buf.pos();
        // YMM0 = A[i..i+8]:  VMOVDQU YMM0, [R9]
        self.emit_vmovdqu_load_ymm(0, 9, 0);
        // YMM0 = YMM0 OP [R12]
        self.emit_ewise_ymm_mem(op, 0, 0, 12, 0);
        // [R13] = YMM0:  VMOVDQU [R13], YMM0
        self.emit_vmovdqu_store_ymm(13, 0, 0);

        // Advance all 3 pointers by 32 bytes (8 ints × 4 bytes).
        // ADD R9, 32 — 49 83 C1 20
        self.buf.emit(&[0x49, 0x83, 0xC1, 0x20]);
        // ADD R12, 32 — 49 83 C4 20
        self.buf.emit(&[0x49, 0x83, 0xC4, 0x20]);
        // ADD R13, 32 — 49 83 C5 20
        self.buf.emit(&[0x49, 0x83, 0xC5, 0x20]);

        // DEC R8D — 41 FF C8
        self.buf.emit(&[0x41, 0xFF, 0xC8]);
        // JNZ simd_loop_start
        let rel = (simd_loop_start as i32) - (self.buf.pos() as i32 + 6); // Cast: x86-64 rel32 displacement
        self.buf.emit_byte(0x0F);
        self.buf.emit_byte(0x85);
        self.buf.emit(&rel.to_le_bytes());

        // VZEROUPPER — safe legacy-SSE transition before the scalar tail.
        self.emit_vzeroupper();

        // Advance R10D by chunks_consumed * 8 = ((n - i) & ~7).
        // Scratch via R8D (chunk counter, decremented to zero by the loop
        // above and now dead) — RAX still holds array A's base, which the
        // scalar remainder below dereferences.
        self.buf.emit(&[0x45, 0x89, 0xD8]); // MOV R8D, R11D
        self.buf.emit(&[0x45, 0x29, 0xD0]); // SUB R8D, R10D
        // Signed clamp for i > n; see the recompute in `emit_simd_int_array_sum`.
        self.buf.emit(&[0x7D, 0x03]); // JGE +3
        self.buf.emit(&[0x45, 0x31, 0xC0]); // XOR R8D, R8D
        self.buf.emit(&[0x41, 0x83, 0xE0, 0xF8]); // AND R8D, ~7
        self.buf.emit(&[0x45, 0x01, 0xC2]); // ADD R10D, R8D

        // Restore R13, R12 before falling into scalar cleanup.
        // POP R13 (41 5D), POP R12 (41 5C)
        self.buf.emit(&[0x41, 0x5D]);
        self.buf.emit(&[0x41, 0x5C]);

        // Patch skip-to-scalar target — when R8D == 0, jump here.
        let after_simd = self.buf.pos();
        let skip_rel = (after_simd as i32) - (simd_skip_patch as i32 + 4); // Cast: x86-64 rel32 displacement
        self.buf.try_patch_i32(simd_skip_patch, skip_rel).ok(); // on Err try_patch_i32 set buf.overflowed; compile bails

        // --- Scalar remainder ---
        //
        // for (; i < n; i++) OUT[i] = A[i] OP B[i]
        //
        // The A and B base pointers arrive in RAX/RCX, but the loop body's
        // `MOV EAX, [base+...]` / `MOV ECX, [base+...]` loads overwrite
        // EAX/ECX — the low halves of the very RAX/RCX registers used as
        // the base. After the first iteration the base is destroyed and
        // the next load faults (any tail length >= 2 segfaults). Stash the
        // bases into R8/R9, which are both dead here (R8 was the chunk
        // counter, R9 the SIMD A-pointer), and address off those. RDX (OUT
        // base) is only ever a store base, never clobbered, so it stays.
        // This runs on both the SIMD-taken and SIMD-skipped paths since it
        // is emitted at the `after_simd` join point.
        self.buf.emit(&[0x49, 0x89, 0xC0]); // MOV R8, RAX  (A base)
        self.buf.emit(&[0x49, 0x89, 0xC9]); // MOV R9, RCX  (B base)

        let scalar_loop_start = self.buf.pos();
        // CMP R10D, R11D — 45 39 DA
        self.buf.emit(&[0x45, 0x39, 0xDA]);
        // JGE end (patched)
        self.buf.emit_byte(0x0F);
        self.buf.emit_byte(0x8D);
        let scalar_end_patch = self.buf.pos();
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);

        // EAX = A[i] = [R8 + R10*4 + H]
        //   43 8B 84 90 <disp32> — REX.B selects R8 as the SIB base
        self.buf.emit(&[0x43, 0x8B, 0x84, 0x90]);
        // Cast: fixed struct/layout offset to i32 instruction displacement
        self.buf.emit(&(HEADER_SIZE as i32).to_le_bytes());

        // ECX = B[i] = [R9 + R10*4 + H]
        //   43 8B 8C 91 <disp32> — REX.B selects R9 as the SIB base
        self.buf.emit(&[0x43, 0x8B, 0x8C, 0x91]);
        // Cast: fixed struct/layout offset to i32 instruction displacement
        self.buf.emit(&(HEADER_SIZE as i32).to_le_bytes());

        // EAX = EAX OP ECX
        self.emit_ewise_scalar_eax_ecx(op);

        // [RDX + R10*4 + H] = EAX
        //   42 89 84 92 <disp32>  — MOV [RDX + R10*4 + disp32], EAX
        self.buf.emit(&[0x42, 0x89, 0x84, 0x92]);
        // Cast: fixed struct/layout offset to i32 instruction displacement
        self.buf.emit(&(HEADER_SIZE as i32).to_le_bytes());

        // INC R10D — 41 FF C2
        self.buf.emit(&[0x41, 0xFF, 0xC2]);
        // JMP scalar_loop_start
        let rel2 = (scalar_loop_start as i32) - (self.buf.pos() as i32 + 5); // Cast: x86-64 rel32 displacement
        self.buf.emit_byte(0xE9);
        self.buf.emit(&rel2.to_le_bytes());

        // Patch scalar end.
        let scalar_end = self.buf.pos();
        let end_rel = (scalar_end as i32) - (scalar_end_patch as i32 + 4); // Cast: x86-64 rel32 displacement
        self.buf.try_patch_i32(scalar_end_patch, end_rel).ok(); // on Err try_patch_i32 set buf.overflowed; compile bails
    }

    /// Emit a guarded `REP STOSB` preheader for a canonical zero-fill loop.
    ///
    /// Failed guards reach the scalar header without changing Java state. This
    /// retains partial writes before an eventual AIOOBE when the upper bound is
    /// beyond the array.
    pub(super) fn emit_bulk_zero_byte_fill_preheader(&mut self, fill: &BulkZeroByteFillLoop) {
        // array -> RAX, iv -> R10D, inclusive bound -> R11D.
        if let Some(reg) = self.reg_for_local(fill.array_local) {
            self.emit_mov_reg_reg(RAX, reg);
        } else {
            self.emit_load_local(RAX, self.local_offset(fill.array_local));
        }
        if let Some(reg) = self.reg_for_local(fill.iv_local) {
            self.emit_mov_reg_reg(R10, reg);
        } else {
            self.emit_load_local(R10, self.local_offset(fill.iv_local));
        }
        if let Some(reg) = self.reg_for_local(fill.bound_local) {
            self.emit_mov_reg_reg(R11, reg);
        } else {
            self.emit_load_local(R11, self.local_offset(fill.bound_local));
        }

        let mut scalar_patches = Vec::with_capacity(5);
        self.buf.emit(&[0x45, 0x85, 0xD2]); // TEST R10D,R10D
        scalar_patches.push(self.emit_jcc_rel32_patch(0x88)); // JS scalar
        self.buf.emit(&[0x45, 0x39, 0xDA]); // CMP R10D,R11D
        scalar_patches.push(self.emit_jcc_rel32_patch(0x8F)); // JG scalar
        self.buf.emit(&[0x44, 0x89, 0xDA]); // MOV EDX,R11D
        self.buf.emit(&[0x44, 0x29, 0xD2]); // SUB EDX,R10D
        self.buf.emit(&[0x81, 0xFA]); // CMP EDX,imm32
        self.buf.emit(&MAX_BULK_BYTE_LOOP_SPAN.to_le_bytes());
        scalar_patches.push(self.emit_jcc_rel32_patch(0x87)); // JA scalar
        self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX,RAX
        scalar_patches.push(self.emit_jcc_rel32_patch(0x84)); // JE scalar
        self.buf.emit(&[0x8B, 0x50, ARRAY_LENGTH_OFFSET as u8]); // MOV EDX,[RAX+len]
        self.buf.emit(&[0x41, 0x39, 0xD3]); // CMP R11D,EDX
        scalar_patches.push(self.emit_jcc_rel32_patch(0x83)); // JAE scalar

        // ECX = bound - iv + 1 (the REP counter).
        self.buf.emit(&[0x44, 0x89, 0xD9]); // MOV ECX,R11D
        self.buf.emit(&[0x44, 0x29, 0xD1]); // SUB ECX,R10D
        self.buf.emit(&[0xFF, 0xC1]); // INC ECX

        // RDI is a Java-local home on Windows. No call or safepoint occurs
        // while RSP is transiently adjusted.
        self.buf.emit_byte(0x57); // PUSH RDI
        self.buf.emit(&[0x48, 0x89, 0xC7]); // MOV RDI,RAX
        self.buf.emit(&[0x4C, 0x01, 0xD7]); // ADD RDI,R10
        self.buf.emit(&[0x48, 0x83, 0xC7, HEADER_SIZE as u8]); // ADD RDI,HEADER_SIZE
        self.buf.emit(&[0x31, 0xC0]); // XOR EAX,EAX
        self.buf.emit(&[0xF3, 0xAA]); // REP STOSB
        self.buf.emit_byte(0x5F); // POP RDI

        // Make the original inclusive condition false and fall through to it.
        self.buf.emit(&[0x45, 0x8D, 0x53, 0x01]); // LEA R10D,[R11D+1]
        if let Some(reg) = self.reg_for_local(fill.iv_local) {
            self.emit_mov_reg_reg(reg, R10);
        } else {
            self.emit_store_local(self.local_offset(fill.iv_local), R10);
        }

        for patch in scalar_patches {
            self.patch_rel32_to_here(patch);
        }
    }

    /// Emit a guarded register-only loop for a canonical strided byte store.
    ///
    /// All guards precede the first store. A failed guard therefore reaches
    /// the original scalar loop with untouched Java state, retaining null,
    /// bounds, negative-step, and signed-overflow behavior.
    pub(super) fn emit_bulk_set_byte_stride_preheader(&mut self, fill: &BulkSetByteStrideLoop) {
        // array -> RAX, iv -> R10D, inclusive bound -> R11D, step -> R9D.
        if let Some(reg) = self.reg_for_local(fill.array_local) {
            self.emit_mov_reg_reg(RAX, reg);
        } else {
            self.emit_load_local(RAX, self.local_offset(fill.array_local));
        }
        if let Some(reg) = self.reg_for_local(fill.iv_local) {
            self.emit_mov_reg_reg(R10, reg);
        } else {
            self.emit_load_local(R10, self.local_offset(fill.iv_local));
        }
        if let Some(reg) = self.reg_for_local(fill.bound_local) {
            self.emit_mov_reg_reg(R11, reg);
        } else {
            self.emit_load_local(R11, self.local_offset(fill.bound_local));
        }
        if let Some(reg) = self.reg_for_local(fill.step_local) {
            self.emit_mov_reg_reg(R9, reg);
        } else {
            self.emit_load_local(R9, self.local_offset(fill.step_local));
        }

        let mut scalar_patches = Vec::with_capacity(7);
        self.buf.emit(&[0x45, 0x85, 0xD2]); // TEST R10D,R10D
        scalar_patches.push(self.emit_jcc_rel32_patch(0x88)); // JS scalar
        self.buf.emit(&[0x45, 0x39, 0xDA]); // CMP R10D,R11D
        scalar_patches.push(self.emit_jcc_rel32_patch(0x8F)); // JG scalar
        self.buf.emit(&[0x44, 0x89, 0xDA]); // MOV EDX,R11D
        self.buf.emit(&[0x44, 0x29, 0xD2]); // SUB EDX,R10D
        self.buf.emit(&[0x81, 0xFA]); // CMP EDX,imm32
        self.buf.emit(&MAX_BULK_BYTE_LOOP_SPAN.to_le_bytes());
        scalar_patches.push(self.emit_jcc_rel32_patch(0x87)); // JA scalar
        self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX,RAX
        scalar_patches.push(self.emit_jcc_rel32_patch(0x84)); // JE scalar
        self.buf.emit(&[0x8B, 0x50, ARRAY_LENGTH_OFFSET as u8]); // MOV EDX,[RAX+len]
        self.buf.emit(&[0x41, 0x39, 0xD3]); // CMP R11D,EDX
        scalar_patches.push(self.emit_jcc_rel32_patch(0x83)); // JAE scalar
        self.buf.emit(&[0x45, 0x85, 0xC9]); // TEST R9D,R9D
        scalar_patches.push(self.emit_jcc_rel32_patch(0x8E)); // JLE scalar
        self.buf.emit(&[0xBA, 0xFF, 0xFF, 0xFF, 0x7F]); // MOV EDX,INT_MAX
        self.buf.emit(&[0x44, 0x29, 0xDA]); // SUB EDX,R11D
        self.buf.emit(&[0x41, 0x39, 0xD1]); // CMP R9D,EDX
        scalar_patches.push(self.emit_jcc_rel32_patch(0x87)); // JA scalar

        self.buf.emit_byte(0x57); // PUSH RDI
        self.buf.emit(&[0x48, 0x89, 0xC7]); // MOV RDI,RAX
        self.buf.emit(&[0x4C, 0x01, 0xD7]); // ADD RDI,R10
        self.buf.emit(&[0x48, 0x83, 0xC7, HEADER_SIZE as u8]); // ADD RDI,HEADER_SIZE
        let store_start = self.buf.pos();
        self.buf.emit(&[0xC6, 0x07, 0x01]); // MOV byte ptr [RDI],1
        self.buf.emit(&[0x4C, 0x01, 0xCF]); // ADD RDI,R9
        self.buf.emit(&[0x45, 0x01, 0xCA]); // ADD R10D,R9D
        self.buf.emit(&[0x45, 0x39, 0xDA]); // CMP R10D,R11D
        self.buf.emit(&[0x0F, 0x8E]); // JLE store_start
        let rel = store_start as i64 - (self.buf.pos() + 4) as i64;
        self.buf.emit(&(rel as i32).to_le_bytes());
        self.buf.emit_byte(0x5F); // POP RDI

        if let Some(reg) = self.reg_for_local(fill.iv_local) {
            self.emit_mov_reg_reg(reg, R10);
        } else {
            self.emit_store_local(self.local_offset(fill.iv_local), R10);
        }

        for patch in scalar_patches {
            self.patch_rel32_to_here(patch);
        }
    }

    /// Emit the guarded remainder of a canonical byte-array Sieve loop nest.
    ///
    /// The range, object, overflow, and safepoint-span guards all precede the
    /// first write. A rejected shape therefore reaches the original bytecode
    /// with every local and array element untouched.
    pub(super) fn emit_byte_sieve_preheader(&mut self, sieve: &ByteSieveLoop) {
        if let Some(reg) = self.reg_for_local(sieve.array_local) {
            self.emit_mov_reg_reg(RAX, reg);
        } else {
            self.emit_load_local(RAX, self.local_offset(sieve.array_local));
        }
        if let Some(reg) = self.reg_for_local(sieve.outer_iv_local) {
            self.emit_mov_reg_reg(R10, reg);
        } else {
            self.emit_load_local(R10, self.local_offset(sieve.outer_iv_local));
        }
        if let Some(reg) = self.reg_for_local(sieve.bound_local) {
            self.emit_mov_reg_reg(R11, reg);
        } else {
            self.emit_load_local(R11, self.local_offset(sieve.bound_local));
        }
        if let Some(reg) = self.reg_for_local(sieve.count_local) {
            self.emit_mov_reg_reg(R8, reg);
        } else {
            self.emit_load_local(R8, self.local_offset(sieve.count_local));
        }
        if let Some(reg) = self.reg_for_local(sieve.inner_iv_local) {
            self.emit_mov_reg_reg(RCX, reg);
        } else {
            self.emit_load_local(RCX, self.local_offset(sieve.inner_iv_local));
        }

        let mut scalar_patches = Vec::with_capacity(6);
        self.buf.emit(&[0x41, 0x83, 0xFA, 0x02]); // CMP R10D,2
        scalar_patches.push(self.emit_jcc_rel32_patch(0x8C)); // JL scalar
        self.buf.emit(&[0x45, 0x39, 0xDA]); // CMP R10D,R11D
        scalar_patches.push(self.emit_jcc_rel32_patch(0x8F)); // JG scalar
        self.buf.emit(&[0x44, 0x89, 0xDA]); // MOV EDX,R11D
        self.buf.emit(&[0x44, 0x29, 0xD2]); // SUB EDX,R10D
        self.buf.emit(&[0x81, 0xFA]); // CMP EDX,imm32
        self.buf.emit(&MAX_BULK_BYTE_LOOP_SPAN.to_le_bytes());
        scalar_patches.push(self.emit_jcc_rel32_patch(0x87)); // JA scalar
        self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX,RAX
        scalar_patches.push(self.emit_jcc_rel32_patch(0x84)); // JE scalar
        self.buf.emit(&[0x8B, 0x50, ARRAY_LENGTH_OFFSET as u8]); // MOV EDX,[RAX+len]
        self.buf.emit(&[0x41, 0x39, 0xD3]); // CMP R11D,EDX
        scalar_patches.push(self.emit_jcc_rel32_patch(0x83)); // JAE scalar
        self.buf.emit(&[0x41, 0x81, 0xFB]); // CMP R11D,imm32
        self.buf.emit(&0x3FFF_FFFFi32.to_le_bytes());
        scalar_patches.push(self.emit_jcc_rel32_patch(0x8F)); // JG scalar

        // Hold the word-at-a-time zero-byte constants in callee-saved
        // registers. Their Java-local home values are restored before the
        // optimized loop publishes any final local state.
        self.buf.emit_byte(0x57); // PUSH RDI
        self.buf.emit_byte(0x56); // PUSH RSI
        self.emit_mov_imm64_full(RDI, 0x0101_0101_0101_0101);
        self.emit_mov_imm64_full(RSI, 0x8080_8080_8080_8080u64 as i64);
        self.buf.emit(&[0x48, 0x83, 0xC0, HEADER_SIZE as u8]); // ADD RAX,HEADER_SIZE
        let outer_start = self.buf.pos();
        // If at least eight bounded elements remain, detect an all-nonzero
        // qword and skip it in one step:
        //   has_zero = (word - 0x01..) & ~word & 0x80..
        self.buf.emit(&[0x44, 0x89, 0xDA]); // MOV EDX,R11D
        self.buf.emit(&[0x83, 0xEA, 0x07]); // SUB EDX,7
        self.buf.emit(&[0x41, 0x39, 0xD2]); // CMP R10D,EDX
        let scalar_outer = self.emit_jcc_rel32_patch(0x8F); // JG scalar_outer
        self.buf.emit(&[0x4A, 0x8B, 0x14, 0x10]); // MOV RDX,[RAX+R10]
        self.buf.emit(&[0x49, 0x89, 0xD1]); // MOV R9,RDX
        self.buf.emit(&[0x49, 0x29, 0xF9]); // SUB R9,RDI
        self.buf.emit(&[0x48, 0xF7, 0xD2]); // NOT RDX
        self.buf.emit(&[0x49, 0x21, 0xD1]); // AND R9,RDX
        self.buf.emit(&[0x49, 0x85, 0xF1]); // TEST R9,RSI
        let scalar_has_zero = self.emit_jcc_rel32_patch(0x85); // JNE scalar_outer
        self.buf.emit(&[0x41, 0x83, 0xC2, 0x08]); // ADD R10D,8
        self.buf.emit(&[0x45, 0x39, 0xDA]); // CMP R10D,R11D
        self.buf.emit(&[0x0F, 0x8E]); // JLE outer_start
        let outer_word_rel = outer_start as i64 - (self.buf.pos() + 4) as i64;
        self.buf.emit(&(outer_word_rel as i32).to_le_bytes());
        let word_scan_done = self.emit_jmp_rel32_patch();
        self.patch_rel32_to_here(scalar_outer);
        self.patch_rel32_to_here(scalar_has_zero);
        self.buf.emit(&[0x42, 0x80, 0x3C, 0x10, 0x00]); // CMP byte [RAX+R10],0
        let composite = self.emit_jcc_rel32_patch(0x85); // JNE outer_increment
        self.buf.emit(&[0x41, 0xFF, 0xC0]); // INC R8D (prime count)
        self.buf.emit(&[0x43, 0x8D, 0x0C, 0x12]); // LEA ECX,[R10+R10]
        self.buf.emit(&[0x44, 0x39, 0xD9]); // CMP ECX,R11D
        let inner_done = self.emit_jcc_rel32_patch(0x8F); // JG inner_done
        let inner_start = self.buf.pos();
        self.buf.emit(&[0xC6, 0x04, 0x08, 0x01]); // MOV byte [RAX+RCX],1
        self.buf.emit(&[0x44, 0x01, 0xD1]); // ADD ECX,R10D
        self.buf.emit(&[0x44, 0x39, 0xD9]); // CMP ECX,R11D
        self.buf.emit(&[0x0F, 0x8E]); // JLE inner_start
        let inner_rel = inner_start as i64 - (self.buf.pos() + 4) as i64;
        self.buf.emit(&(inner_rel as i32).to_le_bytes());
        self.patch_rel32_to_here(inner_done);
        self.patch_rel32_to_here(composite);
        self.buf.emit(&[0x41, 0xFF, 0xC2]); // INC R10D
        self.buf.emit(&[0x45, 0x39, 0xDA]); // CMP R10D,R11D
        self.buf.emit(&[0x0F, 0x8E]); // JLE outer_start
        let outer_rel = outer_start as i64 - (self.buf.pos() + 4) as i64;
        self.buf.emit(&(outer_rel as i32).to_le_bytes());
        self.patch_rel32_to_here(word_scan_done);
        self.buf.emit_byte(0x5E); // POP RSI
        self.buf.emit_byte(0x5F); // POP RDI

        if let Some(reg) = self.reg_for_local(sieve.outer_iv_local) {
            self.emit_mov_reg_reg(reg, R10);
        } else {
            self.emit_store_local(self.local_offset(sieve.outer_iv_local), R10);
        }
        if let Some(reg) = self.reg_for_local(sieve.count_local) {
            self.emit_mov_reg_reg(reg, R8);
        } else {
            self.emit_store_local(self.local_offset(sieve.count_local), R8);
        }
        if let Some(reg) = self.reg_for_local(sieve.inner_iv_local) {
            self.emit_mov_reg_reg(reg, RCX);
        } else {
            self.emit_store_local(self.local_offset(sieve.inner_iv_local), RCX);
        }

        for patch in scalar_patches {
            self.patch_rel32_to_here(patch);
        }
    }
}
