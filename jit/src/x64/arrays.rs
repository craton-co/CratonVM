// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Array element access, bounds checks and null checks.
//!
//! The register-form array accessors and the checks that must precede them.
//! Every one of these bakes a header offset into an instruction displacement,
//! which is why they are inventoried by
//! `header_offset_emission_site_inventory_matches_the_doc` — the planned
//! 32-to-16-byte object-header shrink has to visit every one of them.
//!
//! The null checks come in `_at` and non-`_at` pairs: the `_at` forms carry
//! the bci the stub has to report, for the sites where the throwing bci is not
//! the one currently being walked.

use super::*;

impl Compiler {
    // -----------------------------------------------------------------------
    // Inline array access emitters
    // -----------------------------------------------------------------------

    /// Inline int element load (compact: 4 bytes/element). RAX=array, RCX=index.
    /// Result in RAX (sign-extended to 64-bit).
    pub(super) fn emit_int_aload_regs(&mut self) {
        // MOVSXD RAX, DWORD [RAX + RCX*4 + ARRAY_DATA_OFFSET]
        // Encoding: REX.W + 0x63 + ModRM(mod=01, reg=RAX, r/m=100) + SIB(scale=2, idx=RCX, base=RAX) + disp8
        self.rex_w();
        self.buf.emit_byte(0x63);
        self.buf.emit_byte(0x44); // ModRM: mod=01, reg=RAX(000), r/m=SIB(100)
        self.buf.emit_byte(0x88); // SIB: scale=2(10=*4), index=RCX(001), base=RAX(000)
        self.buf.emit_byte(ARRAY_DATA_OFFSET as u8); // disp8 // Cast: x86-64 immediate encoding
    }

    /// Inline byte element load (compact: 1 byte/element). RAX=array, RCX=index.
    /// Result in RAX (sign-extended to 32-bit, then to 64-bit).
    pub(super) fn emit_byte_aload_regs(&mut self) {
        // MOVSX EAX, BYTE [RAX + RCX*1 + ARRAY_DATA_OFFSET]
        // Encoding: 0x0F 0xBE + ModRM(mod=01, reg=EAX, r/m=SIB) + SIB(scale=0, idx=RCX, base=RAX) + disp8
        self.buf.emit(&[0x0F, 0xBE]);
        self.buf.emit_byte(0x44); // ModRM: mod=01, reg=EAX(000), r/m=SIB(100)
        self.buf.emit_byte(0x08); // SIB: scale=0(00=*1), index=RCX(001), base=RAX(000)
        self.buf.emit_byte(ARRAY_DATA_OFFSET as u8); // disp8 // Cast: x86-64 immediate encoding
                                                     // Sign-extend EAX to RAX
        self.rex_w();
        self.buf.emit(&[0x63, 0xC0]);
    }

    /// Inline int element store (compact: 4 bytes/element). RAX=array, RCX=index, RDX=value.
    pub(super) fn emit_int_astore_regs(&mut self) {
        // MOV DWORD [RAX + RCX*4 + ARRAY_DATA_OFFSET], EDX
        self.buf.emit_byte(0x89);
        self.buf.emit_byte(0x54); // ModRM: mod=01, reg=EDX(010), r/m=SIB(100)
        self.buf.emit_byte(0x88); // SIB: scale=2(10=*4), index=RCX(001), base=RAX(000)
        self.buf.emit_byte(ARRAY_DATA_OFFSET as u8); // disp8 // Cast: x86-64 immediate encoding
    }

    /// Inline byte element store (compact: 1 byte/element). RAX=array, RCX=index, RDX=value.
    pub(super) fn emit_byte_astore_regs(&mut self) {
        // MOV BYTE [RAX + RCX*1 + ARRAY_DATA_OFFSET], DL
        self.buf.emit_byte(0x88);
        self.buf.emit_byte(0x54); // ModRM: mod=01, reg=DL(010), r/m=SIB(100)
        self.buf.emit_byte(0x08); // SIB: scale=0(00=*1), index=RCX(001), base=RAX(000)
        self.buf.emit_byte(ARRAY_DATA_OFFSET as u8); // disp8 // Cast: x86-64 immediate encoding
    }

    /// Inline ref element load from Object[] array (compact 8-byte pointers).
    /// RAX=array, RCX=index. Result in RAX (raw pointer, 0 for null).
    ///
    /// Emits: MOV RAX, QWORD [RAX + RCX*8 + ARRAY_DATA_OFFSET]
    pub(super) fn emit_ref_aload_regs(&mut self) {
        if narrow_oops_enabled() {
            self.emit_narrow_ref_aload_regs();
            return;
        }
        // MOV RAX, QWORD [RAX + RCX*8 + ARRAY_DATA_OFFSET]
        // REX.W + 0x8B + ModRM(mod=01, reg=RAX, r/m=SIB) + SIB(scale=3, idx=RCX, base=RAX) + disp8
        self.rex_w();
        self.buf.emit_byte(0x8B); // MOV r64, r/m64
        self.buf.emit_byte(0x44); // ModRM: mod=01(disp8), reg=000(RAX), r/m=100(SIB)
        self.buf.emit_byte(0xC8); // SIB: scale=11(*8), index=001(RCX), base=000(RAX)
        self.buf.emit_byte(ARRAY_DATA_OFFSET as u8); // Cast: x86-64 immediate encoding
    }

    /// Compressed-oops reference element load. RAX=array, RCX=index; result in
    /// RAX as a full 64-bit pointer, so every consumer downstream is unchanged.
    ///
    /// The element is a 4-byte `(addr - base) >> 3`, with 0 meaning null, so the
    /// decode is `base + (narrow << 3)` — except for null, which must stay 0
    /// rather than becoming `base`. `SHL` sets ZF from its result (the count is
    /// a non-zero literal), so the null test is free: branch over the rebase
    /// when the shifted value is zero.
    ///
    /// R11 is the scratch: it is neither an `ARG_REGS` nor a `SCRATCH_REGS`
    /// member, so the operand-stack register cache never parks a value there.
    fn emit_narrow_ref_aload_regs(&mut self) {
        // MOV EAX, DWORD [RAX + RCX*4 + ARRAY_DATA_OFFSET]   (32-bit dst zero-extends)
        self.buf.emit_byte(0x8B); // MOV r32, r/m32
        self.buf.emit_byte(0x44); // ModRM: mod=01(disp8), reg=000(EAX), r/m=100(SIB)
        self.buf.emit_byte(0x88); // SIB: scale=10(*4), index=001(RCX), base=000(RAX)
        self.buf.emit_byte(ARRAY_DATA_OFFSET as u8); // Cast: x86-64 immediate encoding
        self.buf.emit(&[0x48, 0xC1, 0xE0, 0x03]); // SHL RAX, 3
        self.buf.emit(&[0x74, 0x0D]); // JZ +13 (past the rebase: null stays 0)
        self.buf.emit(&[0x49, 0xBB]); // MOV R11, imm64
        self.buf.emit(&narrow_base().to_le_bytes()); // ... = heap base
        self.buf.emit(&[0x4C, 0x01, 0xD8]); // ADD RAX, R11
    }

    /// Inline ref element store to Object[] array (compact 8-byte pointers).
    /// RAX=array, RCX=index, RDX=value (raw pointer, 0 for null).
    ///
    /// Emits: MOV QWORD [RAX + RCX*8 + ARRAY_DATA_OFFSET], RDX
    ///
    /// Wired into the `aastore` opcode arm; the GC write-barrier is emitted
    /// separately as a call to `self.helpers.write_barrier` after the store.
    pub(super) fn emit_ref_astore_regs(&mut self) {
        if narrow_oops_enabled() {
            self.emit_narrow_ref_astore_regs();
            return;
        }
        // MOV QWORD [RAX + RCX*8 + ARRAY_DATA_OFFSET], RDX
        // REX.W + 0x89 + ModRM(mod=01, reg=RDX, r/m=SIB) + SIB(scale=3, idx=RCX, base=RAX) + disp8
        self.rex_w();
        self.buf.emit_byte(0x89); // MOV r/m64, r64
        self.buf.emit_byte(0x54); // ModRM: mod=01(disp8), reg=010(RDX), r/m=100(SIB)
        self.buf.emit_byte(0xC8); // SIB: scale=11(*8), index=001(RCX), base=000(RAX)
        self.buf.emit_byte(ARRAY_DATA_OFFSET as u8); // Cast: x86-64 immediate encoding
    }

    /// Compressed-oops reference element store. RAX=array, RCX=index,
    /// RDX=value (raw 64-bit pointer, 0 for null).
    ///
    /// Encodes to `(addr - base) >> 3` in R11 and stores 4 bytes; a null value
    /// stores 0. **RDX is preserved** — the `aastore` arm hands it to the write
    /// barrier after this store — so the subtraction is done as
    /// `R11 = (-base) + RDX` rather than in place.
    fn emit_narrow_ref_astore_regs(&mut self) {
        self.buf.emit(&[0x4D, 0x31, 0xDB]); // XOR R11, R11 (the null encoding)
        self.buf.emit(&[0x48, 0x85, 0xD2]); // TEST RDX, RDX
        self.buf.emit(&[0x74, 0x11]); // JZ +17 (store the zero already in R11)
        self.buf.emit(&[0x49, 0xBB]); // MOV R11, imm64
        self.buf.emit(&narrow_base().wrapping_neg().to_le_bytes()); // ... = -base
        self.buf.emit(&[0x49, 0x01, 0xD3]); // ADD R11, RDX -> addr - base
        self.buf.emit(&[0x49, 0xC1, 0xEB, 0x03]); // SHR R11, 3
        self.buf.emit_byte(0x44); // MOV DWORD [..], R11D: REX.R (R11 as reg field)
        self.buf.emit_byte(0x89); // MOV r/m32, r32
        self.buf.emit_byte(0x5C); // ModRM: mod=01(disp8), reg=011(R11), r/m=100(SIB)
        self.buf.emit_byte(0x88); // SIB: scale=10(*4), index=001(RCX), base=000(RAX)
        self.buf.emit_byte(ARRAY_DATA_OFFSET as u8); // Cast: x86-64 immediate encoding
    }

    /// Inline short/char element load (compact: 2 bytes/element). RAX=array, RCX=index.
    /// For saload: sign-extends to 32-bit then to 64-bit.
    pub(super) fn emit_short_aload_regs(&mut self) {
        // MOVSX EAX, WORD [RAX + RCX*2 + ARRAY_DATA_OFFSET]
        // Encoding: 0x0F 0xBF + ModRM(mod=01, reg=EAX, r/m=SIB) + SIB(scale=1, idx=RCX, base=RAX) + disp8
        self.buf.emit(&[0x0F, 0xBF]);
        self.buf.emit_byte(0x44); // ModRM: mod=01, reg=EAX(000), r/m=SIB(100)
        self.buf.emit_byte(0x48); // SIB: scale=1(01=*2), index=RCX(001), base=RAX(000)
        self.buf.emit_byte(ARRAY_DATA_OFFSET as u8); // Cast: x86-64 immediate encoding
                                                     // Sign-extend EAX to RAX
        self.rex_w();
        self.buf.emit(&[0x63, 0xC0]);
    }

    /// Inline char element load (compact: 2 bytes/element). RAX=array, RCX=index.
    /// Zero-extends to 32-bit then sign-extends to 64-bit.
    pub(super) fn emit_char_aload_regs(&mut self) {
        // MOVZX EAX, WORD [RAX + RCX*2 + ARRAY_DATA_OFFSET]
        // Encoding: 0x0F 0xB7 + ModRM(mod=01, reg=EAX, r/m=SIB) + SIB(scale=1, idx=RCX, base=RAX) + disp8
        self.buf.emit(&[0x0F, 0xB7]);
        self.buf.emit_byte(0x44); // ModRM: mod=01, reg=EAX(000), r/m=SIB(100)
        self.buf.emit_byte(0x48); // SIB: scale=1(01=*2), index=RCX(001), base=RAX(000)
        self.buf.emit_byte(ARRAY_DATA_OFFSET as u8); // Cast: x86-64 immediate encoding
                                                     // MOVZX already zero-extends to EAX, upper 32 bits of RAX auto-zeroed
    }

    /// Inline short/char element store (compact: 2 bytes/element). RAX=array, RCX=index, RDX=value.
    pub(super) fn emit_short_astore_regs(&mut self) {
        // MOV WORD [RAX + RCX*2 + ARRAY_DATA_OFFSET], DX
        // Encoding: 0x66 prefix + 0x89 + ModRM + SIB + disp8
        self.buf.emit_byte(0x66); // operand size prefix (16-bit)
        self.buf.emit_byte(0x89);
        self.buf.emit_byte(0x54); // ModRM: mod=01, reg=DX(010), r/m=SIB(100)
        self.buf.emit_byte(0x48); // SIB: scale=1(01=*2), index=RCX(001), base=RAX(000)
        self.buf.emit_byte(ARRAY_DATA_OFFSET as u8); // Cast: x86-64 immediate encoding
    }

    /// Inline long/double element load (compact: 8 bytes/element). RAX=array, RCX=index.
    /// Result in RAX.
    pub(super) fn emit_long_aload_regs(&mut self) {
        // MOV RAX, QWORD [RAX + RCX*8 + ARRAY_DATA_OFFSET]
        // Encoding: REX.W + 0x8B + ModRM(mod=01, reg=RAX, r/m=SIB) + SIB(scale=3, idx=RCX, base=RAX) + disp8
        self.rex_w();
        self.buf.emit_byte(0x8B);
        self.buf.emit_byte(0x44); // ModRM: mod=01, reg=RAX(000), r/m=SIB(100)
        self.buf.emit_byte(0xC8); // SIB: scale=3(11=*8), index=RCX(001), base=RAX(000)
        self.buf.emit_byte(ARRAY_DATA_OFFSET as u8); // Cast: x86-64 immediate encoding
    }

    /// Inline long/double element store (compact: 8 bytes/element). RAX=array, RCX=index, RDX=value.
    pub(super) fn emit_long_astore_regs(&mut self) {
        // MOV QWORD [RAX + RCX*8 + ARRAY_DATA_OFFSET], RDX
        // Encoding: REX.W + 0x89 + ModRM(mod=01, reg=RDX, r/m=SIB) + SIB(scale=3, idx=RCX, base=RAX) + disp8
        self.rex_w();
        self.buf.emit_byte(0x89);
        self.buf.emit_byte(0x54); // ModRM: mod=01, reg=RDX(010), r/m=SIB(100)
        self.buf.emit_byte(0xC8); // SIB: scale=3(11=*8), index=RCX(001), base=RAX(000)
        self.buf.emit_byte(ARRAY_DATA_OFFSET as u8); // Cast: x86-64 immediate encoding
    }

    /// Inline arraylength. Assumes RAX=array ptr. Result in RAX.
    pub(super) fn emit_arraylength_regs(&mut self) {
        // Assumes RAX = array ptr. MOV EAX, DWORD [RAX + ARRAY_LENGTH_OFFSET]
        self.buf.emit(&[0x8B, 0x40, ARRAY_LENGTH_OFFSET as u8]); // Cast: x86-64 register encoding
    }

    /// Round-8 CRIT fix: emit an inline null check on the array receiver
    /// (assumed already in RAX) for an inline array-store opcode. On null,
    /// branches to the shared `null_check_store_stub` (emitted at method
    /// end by [`emit_null_check_store_stubs`]). On non-null, falls through
    /// to the caller's bounds check + inline store.
    ///
    /// Mirrors the structure of [`emit_bounds_check`]. Without this guard,
    /// the immediately-following `MOV R10D, [RAX + ARRAY_LENGTH_OFFSET]`
    /// in `emit_bounds_check` would dereference NULL and SIGSEGV — the
    /// signal handler at `vm/src/runtime/crash_handler.rs` only dumps an
    /// hs_err then re-raises, killing the VM instead of throwing NPE.
    /// The previous `process::abort()` in `vm/src/jit/helpers.rs`
    /// jit_iastore/bastore/aastore was a comment-level "fail loudly"
    /// theater because the helpers were never reached on the inline path.
    ///
    /// `trap_key` is the id `x64::inlining::record_npe_trap_site` issued for
    /// this site, or `0` when the site is not described. It travels with the
    /// action code so [`emit_null_check_store_stubs`] can give a described site
    /// its own cold trampoline; the two bytes of fast path emitted here are the
    /// same either way.
    fn emit_null_check_array_store(&mut self, action: u8, trap_key: u32) {
        // TEST RAX, RAX  (48 85 C0)
        self.buf.emit(&[0x48, 0x85, 0xC0]);
        // JZ rel32 -> null-store stub (patched later)
        self.buf.emit(&[0x0F, 0x84]);
        let patch_offset = self.buf.pos();
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]); // placeholder rel32
        self.null_check_store_stubs
            .push((action, patch_offset, trap_key));
    }

    /// Null check for `putfield` inside a protected range whose handler needs
    /// a precise frame. Unlike the generic array stub this records the frame
    /// at the trapping bytecode, then routes the NPE through that frame so a
    /// javac monitor-cleanup handler retains its synthetic monitor local.
    pub(super) fn emit_precise_null_check_field_store(&mut self) {
        let bci = self.dbg_last_pc;
        if !self.precise_exception_frames || !self.pc_is_protected(bci) {
            let key = crate::x64::inlining::record_npe_trap_site(bci);
            self.emit_null_check_array_store(npe_action::NONE, key);
            return;
        }
        if !self.exc_frame_box_ptr_by_bci.contains_key(&bci) {
            let box_ptr =
                self.build_and_record_deopt_point(bci, crate::deopt::DeoptReason::PendingException);
            self.exc_frame_box_ptr_by_bci.insert(bci, box_ptr);
        }
        self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX,RAX
        self.buf.emit(&[0x0F, 0x84]); // JZ rel32 -> precise NPE stub
        let patch_offset = self.buf.pos();
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
        // Reason 10 is a locally-detected pending NPE. The common reason-9
        // path already has an exception from a helper; this one creates it in
        // the out-of-line frame-deopt stub before the router consumes it.
        self.deopt_stubs.push((patch_offset, bci, 10));
    }

    /// Round-11 HIGH-2: variant of `emit_null_check_array_store` that
    /// elides the inline TEST/JZ entirely when the null-check
    /// elimination dataflow proves the array receiver came from a
    /// local that is known non-null at this bytecode PC. The decision
    /// is made by walking the bytecode 1-4 bytes back from `bc_pc` to
    /// find the `aload N; <index>; <arraystore>` pattern via
    /// [`array_receiver_local`]; if found AND the local is proven
    /// non-null, the entire TEST/JZ pair is skipped (5 bytes saved
    /// per occurrence + branch-predictor pressure reduction).
    ///
    /// Safe to call instead of `emit_null_check_array_store` at every
    /// inline array-store site; the conservative path is identical.
    pub(super) fn emit_null_check_array_store_at(&mut self, code: &[u8], bc_pc: usize) {
        if let Some(local) = array_receiver_local(code, bc_pc) {
            if self.is_local_nonnull(bc_pc, local) {
                // peephole-null-elim: dataflow proves non-null; skip
                // the 8-byte TEST/JZ sequence entirely.
                return;
            }
        }
        // JEP 358: the trapping opcode IS at `code[bc_pc]` (the array-store
        // arm passes its own pc), so derive the per-element-type action.
        let key = crate::x64::inlining::record_npe_trap_site(bc_pc);
        self.emit_null_check_array_store(array_opcode_npe_action(code, bc_pc), key);
    }

    /// Round-9 HIGH fix (asymmetric coverage): emit an inline null check
    /// on the array receiver (assumed already in RAX) for an inline
    /// array-LOAD opcode (iaload / aaload / baload / caload / saload /
    /// laload / faload / daload). The round-8 stub covered only stores;
    /// loads still relied on the page-fault path that
    /// `emit_null_check_array_store`'s doc rightly calls out as broken
    /// (the signal handler at `vm/src/runtime/crash_handler.rs` re-raises
    /// rather than throwing NPE, killing the VM).
    ///
    /// Loads have identical pre-state to stores (`RAX = array_ptr` at
    /// the bounds-check site) and the desired failure outcome is the
    /// same — set `JIT_PENDING_NPE`, deopt out with `RAX = i64::MIN`,
    /// run the epilogue. We therefore reuse the SAME shared stub by
    /// pushing the JZ patch offset into the same `null_check_store_stubs`
    /// vector; both loads and stores branch to it.
    /// `trap_key`: see [`Self::emit_null_check_array_store`].
    pub(super) fn emit_null_check_array_load(&mut self, action: u8, trap_key: u32) {
        // TEST RAX, RAX  (48 85 C0)
        self.buf.emit(&[0x48, 0x85, 0xC0]);
        // JZ rel32 -> shared null-check stub (patched later)
        self.buf.emit(&[0x0F, 0x84]);
        let patch_offset = self.buf.pos();
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]); // placeholder rel32
        self.null_check_store_stubs
            .push((action, patch_offset, trap_key));
    }

    /// Round-11 HIGH-2 (mirrors `emit_null_check_array_store_at`):
    /// elide the inline TEST/JZ null check on array loads when the
    /// receiver is proven non-null at `bc_pc` by the dataflow.
    pub(super) fn emit_null_check_array_load_at(&mut self, code: &[u8], bc_pc: usize) {
        if let Some(local) = array_receiver_local(code, bc_pc) {
            if self.is_local_nonnull(bc_pc, local) {
                // peephole-null-elim: dataflow proves non-null; skip
                // the 8-byte TEST/JZ sequence entirely.
                return;
            }
        }
        // JEP 358: derive the per-element-type action from the trapping opcode.
        let key = crate::x64::inlining::record_npe_trap_site(bc_pc);
        self.emit_null_check_array_load(array_opcode_npe_action(code, bc_pc), key);
    }

    /// Emit an inline null check on the `arraylength` receiver (assumed
    /// already in RAX). `arraylength` previously emitted a raw
    /// `MOV EAX, [RAX + ARRAY_LENGTH_OFFSET]` with no guard — a null
    /// receiver dereferenced low memory and SIGSEGV'd the VM (the crash
    /// handler re-raises rather than throwing NPE). This reuses the
    /// shared null-check stub (sets `JIT_PENDING_NPE`, deopts out) that
    /// array loads/stores already branch to.
    ///
    /// **This check is the one that survives in a counted `for (int i = 0; i <
    /// a.length; i++)` loop, and it costs a `TEST`/`JZ` per iteration.** The
    /// bound's own `arraylength` sits AT the loop header, and the null-check
    /// dataflow (`crate::null_check_elim`) meets over paths with a bitwise AND:
    /// the back edge arrives having just dereferenced the array, the pre-header
    /// does not, so the intersection at the header drops the fact and this
    /// helper emits. The elision is not wrong — the first iteration genuinely
    /// has no proof — but the second and every later one pays for it. Closing it
    /// needs a loop-header-aware proof (peel, or a pre-header null check that
    /// seeds the header's IN set), which lives in `null_check_elim`, not here.
    /// Measured at ~2 of the ~21 instructions the `char[]` scan body emits per
    /// element:
    /// `docs/known-issues/perf/array-element-load-baseline-codegen-20260901.md`.
    ///
    /// Unlike the load/store `_at` helpers, the dataflow elision keys on
    /// the directly-preceding `aload`/`aload_<n>` of the array receiver:
    /// for `arraylength` there is no index push between the `aload` and
    /// the opcode, so the load/store `array_receiver_local` parser (which
    /// expects an index push) would mis-decode the bytecode. We instead
    /// inspect the single instruction before `bc_pc`.
    pub(super) fn emit_null_check_arraylength(&mut self, code: &[u8], bc_pc: usize) {
        if bc_pc >= 1 {
            let prev = code[bc_pc - 1];
            // aload_0..aload_3 (0x2A..0x2D) — single-byte, receiver local n.
            if (0x2A..=0x2D).contains(&prev) {
                // Widening: u8 -> usize (opcode-relative local index, value fits)
                let local = (prev - 0x2A) as usize;
                if self.is_local_nonnull(bc_pc, local) {
                    return;
                }
            } else if bc_pc >= 2 && code[bc_pc - 2] == 0x19 {
                // aload <u8> — two-byte, receiver local code[bc_pc-1].
                // Widening: u8 -> wider int (bytecode operand byte, value fits)
                let local = code[bc_pc - 1] as usize;
                if self.is_local_nonnull(bc_pc, local) {
                    return;
                }
            }
        }
        // Reuse the shared null-check stub machinery; the action is the
        // `arraylength` JEP-358 code ("Cannot read the array length").
        let key = crate::x64::inlining::record_npe_trap_site(bc_pc);
        self.emit_null_check_array_load(npe_action::ARRAY_LENGTH, key);
    }

    /// Emit an array bounds check. RAX=array ptr, RCX=index (as i64).
    ///
    /// Loads the array length from the object header, compares the index
    /// (unsigned, so the compare catches negatives too) against it, and on
    /// `index >= length` jumps to an out-of-line stub that calls
    /// `jit_throw_aioobe`. The stub is emitted later by
    /// `emit_bounds_check_stubs()` after the main code.
    ///
    /// The displacement is the named constant, whose value is **4**. The
    /// "header offset 12" this comment used to state has been wrong since the
    /// header shrank to 16 bytes and `shape` moved up into `identity_hash_code`'s
    /// place (2026-08-07); the emitted bytes always took the constant, so only
    /// the prose was stale. Note that the constant's own comment in
    /// `types/src/heap_types.rs` says "8, not 12" above a value of 4 — that one
    /// is still wrong and is not this file's to fix.
    ///
    /// **R10D is a live OUTPUT of this sequence, not a scratch temporary.**
    /// `emit_bounds_check_stubs` (`x64/deopt_stubs.rs`) reads R10D as
    /// `jit_throw_aioobe`'s `length` argument — it is the number in "Index 5 out
    /// of bounds for length 3". The obvious peephole here is to fold the load
    /// into the compare (`CMP ECX, [RAX+len]`: one instruction and four bytes
    /// fewer, and it macro-fuses with the `JAE`), and it is WRONG on its own —
    /// it leaves the stub reporting whatever R10 last held. It is correct only
    /// together with a length load added to the cold stub, which makes it a
    /// two-file change and not a local peephole. Ruled out here for exactly
    /// that reason; sized in
    /// `docs/known-issues/perf/array-element-load-baseline-codegen-20260901.md`.
    pub(super) fn emit_bounds_check(&mut self, bc_pc: usize) {
        // Skip if loop analysis proved this access is safe.
        //
        // SECURITY INVARIANT (V16, per-array 2026-07-11): `bounds_safe_pcs`
        // only contains a PC when `analyze_bounds_elimination` established,
        // for the enclosing counted loop (exclusive comparator, IV stepped
        // only +1, array/bound locals loop-invariant), ONE of:
        //   * STATIC proof: the bound provably IS this access's array's own
        //     length (`find_bound_arraylength_provenance` — a bound taken from
        //     a DIFFERENT array's length proves nothing for this one, see
        //     docs/known-issues/jit-bce-multi-array-oob-store-20260711.md) and
        //     the IV provably starts non-negative (`find_iv_nonneg_start`); or
        //   * SPECULATIVE guard: a `SpeculativeBCEGuard` for exactly this
        //     access's array, emitted at the loop header (`iv >= 0` and
        //     `array.length >= bound` or deopt). If that guard is later
        //     dropped by per-bci de-spec, the guard's `covered_pcs` are
        //     removed from `bounds_safe_pcs` so this check comes back.
        // Do not add a PC to `bounds_safe_pcs` from any path that does not
        // establish one of the two.
        if self.bounds_safe_pcs.contains(&bc_pc) {
            return;
        }

        // MOV R10D, DWORD [RAX + ARRAY_LENGTH_OFFSET]  — load array_length from ObjectHeader
        // Encoding: 44 8B 50 xx (REX.R + MOV r32, r/m32 + ModRM(01, R10, RAX) + disp8)
        self.buf
            .emit(&[0x44, 0x8B, 0x50, ARRAY_LENGTH_OFFSET as u8]); // Cast: x86-64 register encoding

        // CMP ECX, R10D  — unsigned compare index vs length
        // If index >= length (unsigned), JAE to failure stub
        // Encoding: 41 3B CA (REX.B + CMP r32, r/m32 + ModRM(11, ECX, R10))
        self.buf.emit(&[0x41, 0x3B, 0xCA]);

        // JAE rel32 — jump if above-or-equal (unsigned >= means out of bounds)
        // The rel32 will be patched to point to the out-of-line stub
        self.buf.emit(&[0x0F, 0x83]);
        let patch_offset = self.buf.pos();
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]); // placeholder rel32
        self.bounds_check_stubs.push((patch_offset, bc_pc));
    }
}
