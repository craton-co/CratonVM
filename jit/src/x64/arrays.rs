// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
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

/// The disp8 of the array-length word in every inline bounds-check compare
/// (`CMP r32, [array + len]`), the canonical RAX/RCX form in
/// [`Compiler::emit_bounds_check`] and the operand-home form in
/// [`Compiler::emit_bounds_check_in`] alike -- and the byte
/// `decode_home_bounds_check` matches. A `const` item, not an inline call:
/// `disp8_const` only rejects an over-127 layout constant at BUILD time when it
/// is evaluated in a const context. Inline it would be an ordinary runtime
/// panic, and codegen must never panic. (Hoisted out of `emit_bounds_check`
/// in round 9 wave 9 so the two emitters and the decoder share ONE spelling.)
const BOUNDS_LEN_DISP: u8 = crate::x64::disp::disp8_const(ARRAY_LENGTH_OFFSET as i64) as u8;

/// Byte length of the operand-home bounds compare
/// `REX 3B ModRM(01,idx,100) SIB(00,100,arr) disp8` that
/// [`Compiler::emit_bounds_check_in`] emits right before its `JAE rel32`.
const HOME_BOUNDS_CMP_LEN: usize = 5;



/// Which registers does the bounds check whose `JAE rel32` displacement starts
/// at `patch_off` compare, if it is an operand-home check
/// ([`Compiler::emit_bounds_check_in`])? `Some((array, index))` exactly when the
/// seven bytes before `patch_off` are `REX 3B ModRM SIB disp8 0F 83` in that
/// emitter's fixed shape; `None` for the canonical RAX/RCX forms
/// (`3B 48 len 0F 83` and `44 8B 50 len 41 3B CA 0F 83`) and for anything else.
///
/// Neither canonical form can match: the byte where this shape needs its ModRM
/// (`mod=01, r/m=100`, i.e. `& 0xC7 == 0x44`) is `0x3B` in the fused form and
/// `0x41` in the unfused one. The decode reads the code itself rather than a
/// side table, so the per-site stub prologue it drives is correct for every
/// entry `bounds_check_stubs` holds -- including the loop unroller's shifted
/// copies (byte-identical bodies) -- and needs no rollback bookkeeping of its
/// own. Round 9 wave 9 (arr9).
fn decode_home_bounds_check(code: &[u8], patch_off: usize) -> Option<(u8, u8)> {
    let start = patch_off.checked_sub(HOME_BOUNDS_CMP_LEN + 2)?;
    let &[rex, op, modrm, sib, disp, j0, j1] = code.get(start..patch_off)? else {
        return None;
    };
    // REX with W = 0 and X = 0 (the SIB names no index).
    let rex_ok = rex & 0xF0 == 0x40 && rex & 0x0A == 0;
    if !(rex_ok
        && op == 0x3B
        && modrm & 0xC7 == 0x44
        && sib & 0xF8 == 0x20
        && disp == BOUNDS_LEN_DISP
        && j0 == 0x0F
        && j1 == 0x83)
    {
        return None;
    }
    let index = (((rex >> 2) & 1) << 3) | ((modrm >> 3) & 7);
    let array = ((rex & 1) << 3) | (sib & 7);
    Some((array, index))
}

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
    /// Result in RAX (sign-extended to 64-bit).
    pub(super) fn emit_byte_aload_regs(&mut self) {
        // MOVSX EAX, BYTE [RAX + RCX*1 + ARRAY_DATA_OFFSET]
        // Encoding: 0x0F 0xBE + ModRM(mod=01, reg=EAX, r/m=SIB) + SIB(scale=0, idx=RCX, base=RAX) + disp8
        //
        // REX.W form (`MOVSX RAX, BYTE [...]`): one instruction gives the same
        // 64-bit value the old `MOVSX EAX, ... ; MOVSXD RAX, EAX` pair did
        // (round 9 wave 6, simd6).
        self.rex_w();
        self.buf.emit(&[0x0F, 0xBE]);
        self.buf.emit_byte(0x44); // ModRM: mod=01, reg=RAX(000), r/m=SIB(100)
        self.buf.emit_byte(0x08); // SIB: scale=0(00=*1), index=RCX(001), base=RAX(000)
        self.buf.emit_byte(ARRAY_DATA_OFFSET as u8); // disp8 // Cast: x86-64 immediate encoding
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

    /// JVMS §6.5 `bastore`: into a `boolean[]` the int is narrowed by
    /// `value & 1`; into a `byte[]` it is truncated to its low byte. One opcode
    /// serves both and the verifier accepts either, so the element type is not
    /// known statically. This is the runtime half, what HotSpot's templates do
    /// too: read the array header's kind/element byte and mask only when it
    /// names exactly `boolean[]`.
    ///
    /// RAX=array (already null- and bounds-checked, so the header byte is
    /// readable), EDX=value. Clobbers EDX (on the `boolean[]` path) and the
    /// flags; RAX and RCX are untouched, so `emit_byte_astore_regs` and the GPU
    /// barrier after it see what they expect.
    ///
    /// ```text
    /// CMP BYTE [RAX + KIND_TAGS_BYTE_OFFSET], tag("[Z")   ; 80 78 disp8 ib
    /// JNE +3                                              ; 75 03
    /// AND EDX, 1                                          ; 83 E2 01
    /// ```
    ///
    /// `primitive_array_kind_tags_byte("[Z")` is the exact predicate (kind
    /// `Array`, element `Boolean`); a `boolean[][]` carries element type
    /// `Reference` and is never a `bastore` target anyway. It cannot answer
    /// `None` for `"[Z"`; if it ever did, nothing is emitted and the store keeps
    /// its old low-byte behaviour rather than masking a `byte[]`.
    ///
    /// The `0x54` arm skips this when the stored value is provably `0` or `1`
    /// (an `iconst_0`/`iconst_1`/`bipush 0|1` immediately before a `bastore`
    /// that is not a merge point: javac's `a[i] = true`), where the two
    /// narrowings agree. Round 9 wave 8 (arr8),
    /// `compiled-bastore-to-boolean-array-is-not-masked-20260918.md`.
    pub(super) fn emit_bastore_boolean_mask_regs(&mut self) {
        // Cast: a checked signed disp8, reinterpreted as its encoding byte.
        const KIND_DISP: u8 =
            crate::x64::disp::disp8_const(cratonvm_types::KIND_TAGS_BYTE_OFFSET as i64) as u8; // Cast: checked disp8
        let Some(tag) = cratonvm_types::primitive_array_kind_tags_byte("[Z") else {
            return;
        };
        self.buf.emit(&[0x80, 0x78, KIND_DISP, tag]); // CMP BYTE [RAX+kind], tag
        self.buf.emit(&[0x75, 0x03]); // JNE +3 (not a boolean[]: keep the low byte)
        self.buf.emit(&[0x83, 0xE2, 0x01]); // AND EDX, 1
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

    /// Inline short element load (compact: 2 bytes/element). RAX=array, RCX=index.
    /// For saload: sign-extends to 64-bit.
    pub(super) fn emit_short_aload_regs(&mut self) {
        // MOVSX EAX, WORD [RAX + RCX*2 + ARRAY_DATA_OFFSET]
        // Encoding: 0x0F 0xBF + ModRM(mod=01, reg=EAX, r/m=SIB) + SIB(scale=1, idx=RCX, base=RAX) + disp8
        //
        // REX.W form (`MOVSX RAX, WORD [...]`): one instruction, the same
        // 64-bit value as the old `MOVSX EAX ; MOVSXD RAX, EAX` pair.
        self.rex_w();
        self.buf.emit(&[0x0F, 0xBF]);
        self.buf.emit_byte(0x44); // ModRM: mod=01, reg=RAX(000), r/m=SIB(100)
        self.buf.emit_byte(0x48); // SIB: scale=1(01=*2), index=RCX(001), base=RAX(000)
        self.buf.emit_byte(ARRAY_DATA_OFFSET as u8); // Cast: x86-64 immediate encoding
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

    /// Emit the GPU input-residency barrier after an inline primitive
    /// array store, if it is armed.
    ///
    /// Assumes **RAX still holds the array pointer**, which it does at
    /// every one of the seven store arms: the store itself is
    /// `[RAX + RCX*n + ARRAY_DATA_OFFSET]`, and nothing between the
    /// bounds check and here touches RAX.
    ///
    /// Nothing is emitted unless a `--gpu` run armed the barrier, so a
    /// CPU-only build and a `gpu-offload` build without `--gpu` both get
    /// byte-identical code to before it existed. See
    /// [`crate::gpu_barrier`] for the sequence, the register contract
    /// and why the compiled tier marks a bucket rather than calling
    /// `input_cache::invalidate`.
    pub(super) fn emit_gpu_input_cache_barrier(&mut self) {
        if let Some(bytes) = crate::gpu_barrier::barrier_bytes() {
            self.buf.emit(&bytes);
        }
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
    fn emit_null_check_array_store(&mut self, action: u8, trap_key: u32, bci: usize) {
        // TEST RAX, RAX  (48 85 C0)
        self.buf.emit(&[0x48, 0x85, 0xC0]);
        // JZ rel32 -> null-store stub (patched later)
        self.buf.emit(&[0x0F, 0x84]);
        let patch_offset = self.buf.pos();
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]); // placeholder rel32
        self.null_check_store_stubs
            .push((action, patch_offset, trap_key, self.orig_bci(bci)));
    }

    /// Null check for `putfield` inside a protected range whose handler needs
    /// a precise frame. Unlike the generic array stub this records the frame
    /// at the trapping bytecode, then routes the NPE through that frame so a
    /// javac monitor-cleanup handler retains its synthetic monitor local.
    pub(super) fn emit_precise_null_check_field_store(&mut self) {
        let bci = self.dbg_last_pc;
        // `may_file_by_bci` is the third arm of this refusal and belongs here
        // rather than on the insert below. This is one of exactly TWO publishers
        // `x64/inlining.rs` reaches (it calls this from two places, inside the
        // splice's `putfield` arms), and `bci` is `dbg_last_pc`, which only the
        // OUTER walk assigns — so inside a splice this would file the CALLER's pc
        // into a key space the enclosing method also owns, and `emit_deopt_stubs`
        // would later bake whichever entry survived. Refusing the whole precise
        // arm instead emits the shared null-store stub, which is what every
        // unprotected `putfield` in the tree already gets. See
        // `Compiler::may_file_by_bci` for why this cannot fire today and is
        // written anyway.
        if !self.precise_exception_frames
            || !self.pc_is_protected(bci)
            || !self.may_file_by_bci("exc_frame_box_ptr_by_bci", bci)
        {
            let key = crate::x64::inlining::record_npe_trap_site(self.orig_bci(bci));
            self.emit_null_check_array_store(npe_action::NONE, key, bci);
            return;
        }
        if !self.exc_frame_box_ptr_by_bci.contains_key(&bci) {
            let box_ptr =
                self.build_and_record_deopt_point(bci, crate::deopt::DeoptReason::PendingException);
            self.exc_frame_box_ptr_by_bci.insert(bci, box_ptr);
        }
        // JIT round 16 wave 14 (lane throw4, throw3's backlog B4): the site's
        // trap key, packed above `NONE` in the word the reason-10 stub hands
        // `jit_npe_with_action`. A `putfield` records no action, so without a
        // key nothing corroborated the safepoint-id slot's (stale) bci and the
        // NPE carried no message at all; with it the snapshot names this bci
        // exactly and `jit_npe_message` rebuilds `Cannot assign field ...`.
        // Filed only with a key, so `=0` leaves the map (and the stub's
        // `unwrap_or(NONE)`) exactly as before.
        let trap_key = self.precise_npe_trap_key(bci);
        if trap_key != 0 {
            let packed = u32::from(npe_action::NONE) | (trap_key << 8);
            self.precise_npe_action_by_bci.insert(bci, packed);
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
    /// is made by decoding `aload N; <index push>; <value push>;
    /// <arraystore>` back from `bc_pc`
    /// ([`super::null_check_elim::array_store_receiver_with_starts`]); if
    /// found, none of the three pcs after the `aload` is a merge point, AND
    /// the local is proven non-null, the TEST/JZ pair is skipped.
    ///
    /// Until round 11 wave 2 this asked the LOAD decoder, which looks two
    /// instructions back and so never matched a store: the elision this
    /// doc described had never run
    /// (`r11-x64gen-array-store-null-elision-never-fires-FIXED-20260923.md`).
    ///
    /// Safe to call instead of `emit_null_check_array_store` at every
    /// inline array-store site; the conservative path is identical.
    pub(super) fn emit_null_check_array_store_at(
        &mut self,
        code: &[u8],
        bc_pc: usize,
        starts: &[bool],
    ) {
        // The instruction-start map is built HERE rather than inside
        // `array_receiver`, because this is the outermost point in a function
        // this change is permitted to edit. Be honest about what that buys
        // today: nothing. This function is entered once per array-store site,
        // so one map per site is still exactly what happens. What it buys is
        // the shape — the map is a named local flowing into the decode, so the
        // follow-up that hoists it into the walk deletes this line and adds a
        // parameter instead of rewriting the decode. `REVIEW-NOTE` at the
        // bottom of `x64/null_check_elim.rs` names the function that has to
        // change for the cost to actually go away.
        //
        // `code.len()`, NOT the walk's `code_len` parameter: the map has to be
        // byte-for-byte the one `array_receiver` would have built, and that is
        // the length it uses. See
        // `null_check_elim::preceding_aload_nonnull_local_with_starts`.
        //
        // The decode answers `None` for `bc_pc == 0` and for
        // `bc_pc >= code.len()` without consulting the map at all, so building
        // the map first is wasted work at exactly those two pcs. Neither
        // reaches here: `bc_pc` is the array-store opcode the walk is
        // currently lowering, so it is always a real, in-range instruction
        // start.
        // The map arrives as a parameter, built once per method by
        // `compile_bytecode`. It used to be rebuilt here, per site.
        if let Some((local, idx_pc, val_pc)) =
            super::null_check_elim::array_store_receiver_with_starts(code, bc_pc, starts)
        {
            // The receiver is identified by textual adjacency; at a merge
            // point the array on the stack may have come from another path.
            // All THREE pcs after the `aload` must be merge-free (see the
            // decoder's doc for the two-of-three counterexample).
            if !self.null_check_info.is_merge_point(bc_pc)
                && !self.null_check_info.is_merge_point(val_pc)
                && !self.null_check_info.is_merge_point(idx_pc)
                && self.is_local_nonnull(bc_pc, local)
            {
                // peephole-null-elim: dataflow proves non-null; skip
                // the 8-byte TEST/JZ sequence entirely.
                return;
            }
        }
        // JEP 358: the trapping opcode IS at `code[bc_pc]` (the array-store
        // arm passes its own pc), so derive the per-element-type action.
        let action = array_opcode_npe_action(code, bc_pc);
        // PRECISE array NPE, on the same terms as the bounds check above: only
        // inside a protected range, where an unpublished frame is what RBC.6
        // refuses the whole method for. The ACTION travels with the bci -- see
        // `precise_npe_action_by_bci` -- because reason 10 was written for
        // `putfield`, whose action is always `NONE`, and baking that constant in
        // here would downgrade every array NPE message inside a try block.
        if self.emit_precise_array_npe_check_recorded(bc_pc, action) {
            return;
        }
        let key = crate::x64::inlining::record_npe_trap_site(self.orig_bci(bc_pc));
        self.emit_null_check_array_store(action, key, bc_pc);
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
    pub(super) fn emit_null_check_array_load(&mut self, action: u8, trap_key: u32, bci: usize) {
        // TEST RAX, RAX  (48 85 C0)
        self.buf.emit(&[0x48, 0x85, 0xC0]);
        // JZ rel32 -> shared null-check stub (patched later)
        self.buf.emit(&[0x0F, 0x84]);
        let patch_offset = self.buf.pos();
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]); // placeholder rel32
        self.null_check_store_stubs
            .push((action, patch_offset, trap_key, self.orig_bci(bci)));
    }

    /// The shared-stub array-load null check against the array in `reg`
    /// instead of RAX: `TEST reg, reg ; JZ <shared null stub>`.
    ///
    /// Sound because the shared stub (`emit_null_check_store_stubs`) reads no
    /// register the fast path set up: it loads its action code, calls
    /// `jit_npe_with_action` and returns the deopt sentinel. The array being
    /// in RAX was only ever a requirement of the BOUNDS check that follows the
    /// null check in the general arm, so a load whose bounds check is elided
    /// can test the array where it lives. The recorded entry is the same
    /// `(action, patch, trap_key)` triple, so the loop unroller's patch
    /// snapshot (`op_control.rs`) and the inliner's rollback truncation cover
    /// it unchanged.
    ///
    /// NOT for a site inside a protected range under precise frames: that
    /// check is the reason-10 deopt stub, whose frame is described with the
    /// general arm's register state. [`Self::array_load_null_check_via_reg_ok`]
    /// is the gate; the caller asks it first.
    ///
    /// Round 9 wave 8 (arr8),
    /// `perf-single-pass-checked-array-loads-copy-operands-into-rax-rcx-20260918.md`.
    pub(super) fn emit_null_check_array_load_in(&mut self, code: &[u8], bc_pc: usize, reg: u8) {
        let action = array_opcode_npe_action(code, bc_pc);
        let key = crate::x64::inlining::record_npe_trap_site(self.orig_bci(bc_pc));
        self.emit_test_r64_r64(reg); // TEST reg, reg (48 85 C0 for RAX)
        let patch_offset = self.emit_jcc_rel32_patch(0x84); // JZ rel32 -> shared stub
        self.null_check_store_stubs
            .push((action, patch_offset, key, self.orig_bci(bc_pc)));
    }

    /// May the null check of the array load at `bc_pc` be emitted against a
    /// register other than RAX ([`Self::emit_null_check_array_load_in`])? Yes
    /// exactly when [`Self::emit_null_check_array_load_at`] would take the
    /// shared stub: not inside a protected range under precise frames.
    pub(super) fn array_load_null_check_via_reg_ok(&self, bc_pc: usize) -> bool {
        !(self.precise_exception_frames && self.pc_is_protected(bc_pc))
    }

    /// Does [`Self::emit_null_check_array_load_at`] emit NOTHING at `bc_pc`?
    ///
    /// The elision decision on its own, as a query: the array operand of the
    /// load at `bc_pc` is an `aload` of a local the null-check dataflow proves
    /// non-null there, and neither the load nor that index push is a merge
    /// point. `emit_null_check_array_load_at` asks exactly this first, so the
    /// two cannot disagree. `op_array.rs` asks it to learn whether a load
    /// needs its array in RAX at all (the null-check stubs read RAX).
    pub(super) fn array_load_null_check_is_elided(
        &self,
        code: &[u8],
        bc_pc: usize,
        starts: &[bool],
    ) -> bool {
        // See `emit_null_check_array_store_at`: no elision at a merge point.
        match super::null_check_elim::array_receiver_with_starts(code, bc_pc, starts) {
            Some((local, idx_pc)) => {
                !self.null_check_info.is_merge_point(bc_pc)
                    && !self.null_check_info.is_merge_point(idx_pc)
                    && self.is_local_nonnull(bc_pc, local)
            }
            None => false,
        }
    }

    /// Emit the element load of `iaload`/`laload`/`baload`/`caload`/`saload`
    /// (opcode `op`) as ONE instruction addressing the operands where they
    /// already are: `dst = [base + index*size + array data offset]`, sign- or
    /// (for `char`) zero-extended to 64 bits exactly as the RAX/RCX forms
    /// above extend it. Returns `false`, emitting nothing, for any other
    /// opcode or an unencodable register (`index` may not be RSP: SIB
    /// index `100` without REX.X means "no index").
    ///
    /// ModRM is always `mod=01` with a SIB byte, which encodes every base,
    /// RBP/R13 (which need a displacement) and RSP/R12 (which need a SIB)
    /// included. The caller has already emitted whatever null and bounds
    /// checks the site keeps, against these same registers
    /// ([`Self::emit_null_check_array_load_in`], [`Self::emit_bounds_check_in`]).
    ///
    /// Round 9 wave 6 (simd6), page fix 3 of
    /// `perf-single-pass-scalar-loop-round-trips-every-operand-through-a-scratch-register-20260918.md`:
    /// `mov rax, r14 ; mov rcx, r12 ; movsxd rax, [rax+rcx*4+d] ; mov r8, rax`
    /// becomes `movsxd r8, [r14+r12*4+d]`.
    pub(super) fn emit_array_load_via_regs(
        &mut self,
        op: u8,
        dst: u8,
        base: u8,
        index: u8,
    ) -> bool {
        // Checked narrowing, evaluated at build time (see `emit_bounds_check`).
        // Cast: a checked signed disp8, reinterpreted as its encoding byte.
        const DATA_DISP: u8 = crate::x64::disp::disp8_const(ARRAY_DATA_OFFSET as i64) as u8;
        let (wide, opcode, scale): (bool, &[u8], u8) = match op {
            0x2e => (true, &[0x63u8][..], 2),        // MOVSXD r64, m32
            0x2f => (true, &[0x8Bu8][..], 3),        // MOV r64, m64
            0x33 => (true, &[0x0Fu8, 0xBE][..], 0),  // MOVSX r64, m8
            0x35 => (true, &[0x0Fu8, 0xBF][..], 1),  // MOVSX r64, m16
            0x34 => (false, &[0x0Fu8, 0xB7][..], 1), // MOVZX r32, m16 (clears 63..32)
            _ => return false,
        };
        if index == RSP || dst > 15 || base > 15 || index > 15 {
            return false;
        }
        let rex =
            0x40u8 | (u8::from(wide) << 3) | ((dst >> 3) << 2) | ((index >> 3) << 1) | (base >> 3);
        if rex != 0x40 {
            self.buf.emit_byte(rex);
        }
        self.buf.emit(opcode);
        self.buf.emit_byte(0x44 | ((dst & 7) << 3)); // ModRM: mod=01, reg=dst, r/m=SIB
        self.buf
            .emit_byte((scale << 6) | ((index & 7) << 3) | (base & 7)); // SIB
        self.buf.emit_byte(DATA_DISP);
        true
    }

    /// Round-11 HIGH-2 (mirrors `emit_null_check_array_store_at`):
    /// elide the inline TEST/JZ null check on array loads when the
    /// receiver is proven non-null at `bc_pc` by the dataflow.
    pub(super) fn emit_null_check_array_load_at(
        &mut self,
        code: &[u8],
        bc_pc: usize,
        starts: &[bool],
    ) {
        // Map built at the function's entry for the same reason, and with the
        // same caveats, as `emit_null_check_array_store_at` above — including
        // that `code.len()` is the required length and the walk's `code_len`
        // is not. `op_array.rs` has eight call sites into this helper, one per
        // element-typed load opcode, so a loop body doing `a[i]` pays a full
        // instruction-start map per access.
        // The map arrives as a parameter, built once per method by
        // `compile_bytecode`. It used to be rebuilt here, per site.
        if self.array_load_null_check_is_elided(code, bc_pc, starts) {
            // peephole-null-elim: dataflow proves non-null; skip
            // the 8-byte TEST/JZ sequence entirely.
            return;
        }
        // JEP 358: derive the per-element-type action from the trapping opcode.
        let action = array_opcode_npe_action(code, bc_pc);
        // PRECISE array-load NPE inside a protected range (r9-ea), exactly as
        // the store twin above and the bounds check already do. Without it a
        // load's NPE was the one exit of an array access that still took the
        // shared, frame-less pad, so `first_unsupported_precise_frame_site`
        // (lib.rs) has to keep refusing array loads in a protected range for
        // methods whose handler reads a later local.
        if self.emit_precise_array_npe_check_recorded(bc_pc, action) {
            return;
        }
        let key = crate::x64::inlining::record_npe_trap_site(self.orig_bci(bc_pc));
        self.emit_null_check_array_load(action, key, bc_pc);
    }

    /// Inside a protected range under `precise_exception_frames`, emit the
    /// receiver null check (receiver in RAX) as a reason-10 deopt stub that
    /// publishes this instruction's frame, and answer `true`. Anywhere else
    /// emit nothing and answer `false`, leaving the caller's shared-stub check.
    ///
    /// Shared by the array load, store and `arraylength` null checks (the
    /// store arm inlined this sequence before r9); the ACTION travels with the
    /// site through `precise_npe_action_by_bci` so the message still names the
    /// operation. Since JIT round 16 wave 12 (lane throw3) also the String
    /// access intrinsics' null receiver under `CRATONVM_JIT_STRING_INTRINSIC_NPE=1`
    /// (`npe_action::INVOKE_RECEIVER`; the receiver is in RAX there too, and
    /// the operands are popped first, as here).
    ///
    /// # Keyed by the EMITTER pc (r9-ea)
    ///
    /// `build_and_record_deopt_point` takes an emitter pc — it publishes
    /// `resume_bci_for(pc)` and indexes every per-pc analysis by it — and
    /// `emit_deopt_stubs` translates the stub's key with `orig_bci` itself.
    /// This arm used to translate FIRST (`orig_bci(bc_pc)`) and hand the result
    /// to both, so under a bytecode loop rewrite the frame was built from
    /// another instruction's analyses and published a doubly-translated bci
    /// (`precise-exception-stubs-double-map-bci-under-loop-rewrite-20260918.md`).
    /// Identity when nothing is rewritten. The reason-11 bounds-check arm in
    /// `emit_bounds_check` is keyed the same way (r9 wave 2), with its
    /// consumer passing the translated bci to `jit_throw_aioobe`.
    pub(super) fn emit_precise_array_npe_check(&mut self, bc_pc: usize, action: u8) -> bool {
        self.emit_precise_npe_check_keyed(bc_pc, action, 0)
    }

    /// [`Self::emit_precise_array_npe_check`] with the site's trap key: what
    /// the array load, store and `arraylength` arms call.
    ///
    /// JIT round 16 wave 14 (lane throw4, throw3's backlog B4): wave 13 keyed
    /// only the String arm, so a protected array NPE in a precise-frame method
    /// still took its bci from the frame's safepoint-id slot, which a null check
    /// never writes. A stale slot lands on another opcode, the action does not
    /// corroborate it, and the message degrades to the action-only text
    /// (`Cannot load from int array`, without `because ...`) -- under the load
    /// conditions that showed `msg=null` on the String arm. The key makes the
    /// snapshot name this bci exactly (`apply_npe_trap_site`).
    pub(super) fn emit_precise_array_npe_check_recorded(
        &mut self,
        bc_pc: usize,
        action: u8,
    ) -> bool {
        match self.precise_npe_trap_key(bc_pc) {
            0 => self.emit_precise_array_npe_check(bc_pc, action),
            trap_key => self.emit_precise_npe_check_keyed(bc_pc, action, trap_key),
        }
    }

    /// The trap key a reason-10 precise NPE stub at `bc_pc` packs above its
    /// action, or `0` (the bare action, the stub's historical word).
    ///
    /// Recorded only where the precise arm can be armed (precise frames, a
    /// protected pc); elsewhere the caller takes the shared stub and records
    /// that stub's own key. A row recorded for an arm `may_file_by_bci` then
    /// refuses is never named by any stub, like a rewound splice's rows
    /// (`record_npe_trap_site`). Under `CRATONVM_JIT_PRECISE_NPE_TRAP_KEY=0`
    /// nothing is recorded and the array and `putfield` precise stubs are
    /// byte-identical to wave 13; on (the default) only the stub's `imm32`
    /// changes, plus one row in the artifact's trap table. Read per site at
    /// compile time, no static.
    pub(super) fn precise_npe_trap_key(&self, bc_pc: usize) -> u32 {
        if !self.precise_exception_frames
            || !self.pc_is_protected(bc_pc)
        {
            return 0;
        }
        crate::x64::inlining::record_npe_trap_site(self.orig_bci(bc_pc))
    }

    /// [`Self::emit_precise_array_npe_check`] with an
    /// `inlining::record_npe_trap_site` key for the site (`0` for none), packed
    /// above the action into the word the reason-10 stub hands
    /// `jit_npe_with_action`, so the NPE's frame snapshot names THIS bci
    /// exactly (`apply_npe_trap_site`).
    ///
    /// JIT round 16 wave 13 (lane throw3): without a key the snapshot's bci is
    /// the frame's safepoint-id slot, which a null check never writes -- it
    /// holds the last CALL's id (in `R16ThrowpathPlain.npeInvoke`, the
    /// handler's `putstatic`), the message analysis then finds a `putstatic`
    /// where the action says `invoke` and declines, and the String intrinsic's
    /// NPE came out with `msg=null` (20/20 loaded runs, w12b).
    pub(super) fn emit_precise_npe_check_keyed(
        &mut self,
        bc_pc: usize,
        action: u8,
        trap_key: u32,
    ) -> bool {
        // The `may_file_by_bci` clause answers `false`, i.e. "no precise stub
        // emitted, use the shared one", which is this function's own documented
        // fail-closed answer and the only reason it returns a `bool` at all. It
        // also covers `precise_npe_action_by_bci` below, which is filed
        // UNCONDITIONALLY past the idempotence test and would otherwise overwrite
        // the enclosing method's action at a colliding key even when the pointer
        // was withheld. `x64/inlining.rs` does not call this arm at all today —
        // the splice walk contains no array lowering — so this is insurance; see
        // `Compiler::may_file_by_bci`.
        if !(self.precise_exception_frames
            && self.pc_is_protected(bc_pc)
            && self.may_file_by_bci("exc_frame_box_ptr_by_bci", bc_pc))
        {
            return false;
        }
        if !self.exc_frame_box_ptr_by_bci.contains_key(&bc_pc) {
            let box_ptr = self
                .build_and_record_deopt_point(bc_pc, crate::deopt::DeoptReason::PendingException);
            self.exc_frame_box_ptr_by_bci.insert(bc_pc, box_ptr);
        }
        // A key is below 2^24 by construction (`record_npe_trap_site`), so the
        // shift loses nothing; `0` leaves the bare action.
        let packed = u32::from(action) | (trap_key << 8);
        self.precise_npe_action_by_bci.insert(bc_pc, packed);
        self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
        self.buf.emit(&[0x0F, 0x84]); // JZ rel32 -> precise NPE stub
        let patch_offset = self.buf.pos();
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
        self.deopt_stubs.push((patch_offset, bc_pc, 10));
        true
    }

    /// Emit an inline null check on the `arraylength` receiver (assumed
    /// already in RAX). `arraylength` previously emitted a raw
    /// `MOV EAX, [RAX + ARRAY_LENGTH_OFFSET]` with no guard — a null
    /// receiver dereferenced low memory and SIGSEGV'd the VM (the crash
    /// handler re-raises rather than throwing NPE). This reuses the
    /// shared null-check stub (sets `JIT_PENDING_NPE`, deopts out) that
    /// array loads/stores already branch to.
    ///
    /// **This check used to be the one that survives in a counted
    /// `for (int i = 0; i < a.length; i++)` loop, at a `TEST`/`JZ` per
    /// iteration.** The bound's own `arraylength` sits AT the loop header, and
    /// the null-check dataflow (`crate::null_check_elim`) meets over paths with
    /// a bitwise AND: the back edge arrives having just dereferenced the array,
    /// the pre-header does not, so the intersection at the header drops the
    /// fact and this helper emitted. The elision was not wrong — the first
    /// iteration genuinely has no proof — but the second and every later one
    /// paid for it.
    ///
    /// Closed 2026-09-02 by moving the whole sequence instead of proving it
    /// away: `ArrayLenHoist` (`x64/licm.rs`) computes the invariant
    /// `arraylength` once in the pre-header and the body reads a frame slot, so
    /// this null check goes with it and the header no longer emits one at all.
    /// The dataflow reasoning above still describes what happens at a header
    /// the hoist declines (a variant receiver, a bypassable header, a site
    /// outside the header's straight-line prefix), which is why it is kept.
    /// Sized in array-element-load-baseline-codegen-20260901.
    ///
    /// Unlike the load/store `_at` helpers, the dataflow elision keys on
    /// the directly-preceding `aload`/`aload_<n>` of the array receiver:
    /// for `arraylength` there is no index push between the `aload` and
    /// the opcode, so the load/store `array_receiver_local` parser (which
    /// expects an index push) would mis-decode the bytecode. We instead ask
    /// for the single instruction before `bc_pc`.
    ///
    /// # SOUNDNESS FIX (2026-09-16): the backward byte read this replaced
    ///
    /// Until now this helper decoded its receiver by reading raw bytes
    /// *backwards* — `code[bc_pc - 1]` matched against `0x2A..=0x2D`, then
    /// `code[bc_pc - 2] == 0x19` — with no instruction-start validation. It
    /// was the one array helper that never received the fix both `SOUNDNESS
    /// FIX` blocks in `null_check_elim.rs` document, and the hazard is
    /// identical: a multi-byte instruction's trailing OPERAND byte can
    /// numerically equal an `aload` opcode.
    ///
    /// The concrete shape, with the same constant-pool index the recorded
    /// production instance used: `a.arr.length` where field `arr` sits at CP
    /// index 42 compiles to `getfield #42` (`B4 00 2A`) immediately followed
    /// by `arraylength` (`BE`). The byte at `bc_pc - 1` is `0x2A` — the low
    /// half of the index — and read as an opcode that is `aload_0`, i.e.
    /// `this`, which the null-check dataflow proves non-null in an instance
    /// method. The check would then be elided on `a.arr`, which can be null:
    /// a SIGSEGV where the JVMS requires a `NullPointerException`, and the
    /// crash handler re-raises rather than throwing. The two-byte arm has the
    /// same shape one byte further back — any 3-byte instruction whose first
    /// operand byte is `0x19` (e.g. `sipush 0x19xx`) names an arbitrary local.
    ///
    /// The fix is the one the other two sites already took: validate against
    /// a forward-walked instruction-start map, and accept only a decode whose
    /// candidate is a genuine boundary.
    /// [`super::null_check_elim::preceding_aload_nonnull_local_with_starts`]
    /// answers exactly this question — "is the instruction immediately
    /// preceding `pc` an `aload` of some local" — and is the same helper the
    /// `ifnull`/`ifnonnull` arms use. On any misalignment it answers `None`,
    /// so the runtime check is conservatively kept.
    pub(super) fn emit_null_check_arraylength(
        &mut self,
        code: &[u8],
        bc_pc: usize,
        starts: &[bool],
    ) {
        // Built from `code.len()`, not from any `code_len` parameter: the
        // helper's own contract is a map over the whole slice it is given, and
        // a short map silently turns provable sites into `None`. See the
        // REVIEW-NOTE in `null_check_elim.rs` for the trap.
        // The map arrives as a parameter, built once per method by
        // `compile_bytecode`. It used to be rebuilt here, per site.
        //
        // NOT at a merge point (r9-ea): the textually preceding `aload` is the
        // operand's source only when nothing jumps to `bc_pc`. In
        // `(c ? a : b).length` the `arraylength` IS the join, the preceding
        // push is `aload b`, and a proof about `b` said nothing about the `a`
        // path. Every sibling elision (`_load_at`, `_store_at`, the `getfield`
        // receiver arm, `ifnull`/`ifnonnull`) already refused there. See
        // `arraylength-null-check-elided-at-a-merge-point-20260918.md`.
        if !self.null_check_info.is_merge_point(bc_pc) {
            if let Some(local) = super::null_check_elim::preceding_aload_nonnull_local_with_starts(
                code, bc_pc, &starts,
            ) {
                if self.is_local_nonnull(bc_pc, local) {
                    return;
                }
            }
        }
        // Inside a protected range under precise frames: publish (r9-ea). This
        // is the exit that made `arraylength` a blocking opcode for
        // `first_unsupported_precise_frame_site` ("routes its NPE through the
        // SHARED null-check stub, which records no frame at the bci").
        if self.emit_precise_array_npe_check_recorded(bc_pc, npe_action::ARRAY_LENGTH) {
            return;
        }
        // Reuse the shared null-check stub machinery; the action is the
        // `arraylength` JEP-358 code ("Cannot read the array length").
        let key = crate::x64::inlining::record_npe_trap_site(self.orig_bci(bc_pc));
        self.emit_null_check_array_load(npe_action::ARRAY_LENGTH, key, bc_pc);
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
    /// **The length is loaded by the COLD STUB, not by this sequence.** This
    /// used to be `MOV R10D, [RAX+len] ; CMP ECX, R10D ; JAE stub` — seven
    /// bytes and three instructions — because `emit_bounds_check_stubs`
    /// (`x64/deopt_stubs.rs`) reads R10D as `jit_throw_aioobe`'s `length`
    /// argument: the number in "Index 5 out of bounds for length 3". Folding
    /// the load into the compare on its own would have left that stub
    /// reporting whatever R10 last held, which is why it stood as a
    /// deliberately-refused peephole with the reason written down.
    ///
    /// It is correct **together with** the same load added to the cold stub,
    /// where RAX still holds the array pointer and nothing is timing-critical.
    /// That is the pairing now in force, and the two halves must move
    /// together: if this compare ever stops dereferencing `[RAX+len]`, or the
    /// stub stops re-loading it, the exception message goes wrong silently.
    /// `bounds_check_length_is_reloaded_in_the_cold_stub` in
    /// `x64/flag_and_header_contracts.rs` is the tripwire on that pairing.
    ///
    /// The fast path is now `CMP ECX, [RAX+len] ; JAE stub` — three bytes and
    /// four saved, on **every** emitted bounds check, i.e. everywhere BCE does
    /// not fire. Faulting behaviour is unchanged: the compare still
    /// dereferences the same header word the load did, so a null array still
    /// traps at the same instruction boundary rather than reaching the stub.
    pub(super) fn emit_bounds_check(&mut self, bc_pc: usize) {
        // Skip if loop analysis proved this access is safe.
        //
        // SECURITY INVARIANT (V16, per-array 2026-07-11): `bounds_safe_pcs`
        // only contains a PC when `analyze_bounds_elimination_with_handlers`
        // established ONE of:
        //   * STATIC proof (`CountedLoop::prove_index_in_bounds_of` answered
        //     `Static`) for the enclosing recognised counted loop: e.g. the
        //     limit provably IS this access's array's own length
        //     (`find_bound_arraylength_provenance` rewrites it to
        //     `BoundSource::ArrayLength` — a DIFFERENT array's length proves
        //     nothing for this one, see
        //     docs/feature-designs/jit-ir-optimizations.md) and
        //     the IV's entry value is a proven non-negative constant
        //     (`loop_analysis::constant_iv_init`);
        //   * SPECULATIVE guard: a `SpeculativeBCEGuard` for exactly this
        //     access's array, emitted at the loop header (`iv >= 0` and
        //     `array.length >= bound` or deopt), every one of whose obligations
        //     `GuardShape::covers` accepts. If that guard is later dropped (de-spec,
        //     bypassable header), its `covered_pcs` are removed from
        //     `bounds_safe_pcs` so this check comes back; or
        //   * the guard-dominated range reason (`range_safe_pcs`): an explicit
        //     `0 <= i < a.length` test dominates the access, with the method's
        //     exception table known.
        // Do not add a PC to `bounds_safe_pcs` from any path that does not
        // establish one of these.
        if self.bounds_safe_pcs.contains(&bc_pc) {
            return;
        }

        // CMP ECX, DWORD [RAX + ARRAY_LENGTH_OFFSET]  — unsigned compare of the
        // index against array_length read straight out of the ObjectHeader.
        // If index >= length (unsigned, so a negative index compares as huge),
        // JAE to the failure stub, which re-loads the length for the message.
        // Encoding: 3B 48 xx (CMP r32, r/m32 + ModRM(01, ECX, RAX) + disp8).
        // The disp8 is const-asserted to fit in `types/src/heap_types.rs`; the
        // module-level `BOUNDS_LEN_DISP` is the checked narrowing (see there).
        const LEN_DISP: u8 = BOUNDS_LEN_DISP;
        if jit_fused_bounds_load_enabled() {
            self.buf.emit(&[0x3B, 0x48, LEN_DISP]);
        } else {
            // `CRATONVM_JIT_FUSED_BOUNDS_LOAD=0`: the pre-2026-09-02 pair.
            // MOV R10D, DWORD [RAX + len]  (44 8B 50 xx), then
            // CMP ECX, R10D               (41 3B CA).
            // The cold stub re-loads the length either way, so this arm is a
            // pure instruction-count difference on the fast path.
            self.buf.emit(&[0x44, 0x8B, 0x50, LEN_DISP]);
            self.buf.emit(&[0x41, 0x3B, 0xCA]);
        }

        // JAE rel32 — jump if above-or-equal (unsigned >= means out of bounds)
        // The rel32 will be patched to point to the out-of-line stub
        self.buf.emit(&[0x0F, 0x83]);
        let patch_offset = self.buf.pos();
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]); // placeholder rel32
                                                  // PRECISE AIOOBE. Outside a protected range the cheap shared pad stays
                                                  // -- a frame is only useful where this method's own exception table can
                                                  // catch. Inside one, the pad returns the sentinel through the epilogue
                                                  // and the handler is entered from the INTERPRETER's frame, which is
                                                  // exactly the unpublished-frame case RBC.6 refuses the whole method
                                                  // for. Reason 11 publishes the AIOOBE and then materialises the frame,
                                                  // the same shape reason 10 uses for a locally-detected NPE.
        if self.precise_exception_frames
            && self.pc_is_protected(bc_pc)
            && self.may_file_by_bci("exc_frame_box_ptr_by_bci", bc_pc)
        {
            // The third clause is about the KEY, not the coordinate: a splice
            // filing here would put a second method's pc into the same bare-bci
            // space the enclosing method owns, and `emit_deopt_stubs` bakes
            // whichever entry survived. Refusing falls through to
            // `bounds_check_stubs` below — the shared, frame-less AIOOBE pad,
            // which is what every unprotected bounds check in the tree uses.
            // `x64/inlining.rs` contains no call to this function, so the splice
            // walk cannot reach it and the clause is insurance; see
            // `Compiler::may_file_by_bci`.
            //
            // Keyed by the EMITTER pc, like `emit_precise_array_npe_check`:
            // `build_and_record_deopt_point` publishes `resume_bci_for(bc_pc)`
            // and indexes its analyses by `bc_pc`, and `emit_deopt_stubs`
            // translates the stub key (`orig_bci(site_pc)`) for the
            // `jit_throw_aioobe` pc argument itself. Translating here as well
            // double-mapped the bci under a loop rewrite (r9 wave 2,
            // `precise-exception-stubs-double-map-bci-under-loop-rewrite`).
            if !self.exc_frame_box_ptr_by_bci.contains_key(&bc_pc) {
                let box_ptr = self.build_and_record_deopt_point(
                    bc_pc,
                    crate::deopt::DeoptReason::PendingException,
                );
                self.exc_frame_box_ptr_by_bci.insert(bc_pc, box_ptr);
            }
            self.deopt_stubs.push((patch_offset, bc_pc, 11));
            return;
        }
        self.bounds_check_stubs.push((patch_offset, bc_pc));
    }

    /// May the array access at `bc_pc` be bounds-checked against its operands'
    /// own registers ([`Self::emit_bounds_check_in`])? Yes when the check is
    /// elided anyway (`bounds_safe_pcs`), or when it would take the SHARED
    /// per-site AIOOBE pad in its fused form: not inside a protected range
    /// under precise frames (the reason-11 deopt stub describes the general
    /// arm's RAX/RCX state) and not under `CRATONVM_JIT_FUSED_BOUNDS_LOAD=0`
    /// (whose `MOV R10D ; CMP ECX, R10D` pair is RAX/RCX-only).
    pub(super) fn array_bounds_check_via_regs_ok(&self, bc_pc: usize) -> bool {
        self.bounds_safe_pcs.contains(&bc_pc)
            || (jit_fused_bounds_load_enabled() && self.array_load_null_check_via_reg_ok(bc_pc))
    }

    /// [`Self::emit_bounds_check`] with the array in `array` and the index in
    /// `index` instead of RAX/RCX: `CMP index32, [array + len] ; JAE <pad>`.
    ///
    /// The fast path compares the operands where they live; the cold edge
    /// moves them. The `JAE` is recorded in `bounds_check_stubs` like any other
    /// site, and [`Self::emit_bounds_check_home_prologues`] (run just before
    /// `emit_bounds_check_stubs`) recognises this instruction's bytes
    /// (`decode_home_bounds_check`), retargets the `JAE` to a per-site
    /// `mov rax, <array> ; mov rcx, <index> ; jmp <pad>` and hands the pad
    /// that `jmp` instead. So the pad -- which reads RAX/RCX -- is unchanged,
    /// and the entry is covered by every existing rollback truncation and by
    /// the loop unroller's patch snapshot without a new side table.
    ///
    /// Always the fixed 5-byte `REX 3B ModRM(01,idx,100) SIB(00,100,arr)
    /// disp8` shape (REX even when it is `0x40`, SIB even when the base does
    /// not need one) so the decode is exact. RAX/RCX operands take the
    /// canonical emitter. The caller must have asked
    /// [`Self::array_bounds_check_via_regs_ok`]; a call it would have refused
    /// abandons the compile rather than route a precise or unfused site
    /// through the wrong pad.
    ///
    /// Round 9 wave 9 (arr9), the remaining half of
    /// `perf-single-pass-checked-array-loads-copy-operands-into-rax-rcx-20260918.md`.
    pub(super) fn emit_bounds_check_in(&mut self, bc_pc: usize, array: u8, index: u8) {
        if self.bounds_safe_pcs.contains(&bc_pc) {
            return;
        }
        if array == RAX && index == RCX {
            self.emit_bounds_check(bc_pc);
            return;
        }
        if array > 15 || index > 15 || !self.array_bounds_check_via_regs_ok(bc_pc) {
            self.buf
                .mark_codegen_unencodable("bounds-check-in-home-registers-refused");
            return;
        }
        let rex = 0x40u8 | ((index >> 3) << 2) | (array >> 3);
        self.buf.emit(&[
            rex,
            0x3B,                      // CMP r32, r/m32
            0x44 | ((index & 7) << 3), // ModRM: mod=01, reg=index, r/m=SIB
            0x20 | (array & 7),        // SIB: no index, base=array
            BOUNDS_LEN_DISP,           // disp8
        ]);
        let patch_offset = self.emit_jcc_rel32_patch(0x83); // JAE rel32 -> prologue
        self.bounds_check_stubs.push((patch_offset, bc_pc));
    }

    /// Give every operand-home bounds check ([`Self::emit_bounds_check_in`])
    /// its cold prologue: `L_k: mov rax, <array> ; mov rcx, <index> ; jmp
    /// <pad>`, the `JAE` retargeted to `L_k` and the `bounds_check_stubs` entry
    /// rewritten to the `jmp`'s displacement so `emit_bounds_check_stubs`
    /// lands it on the site's own pad (same bci, pad unchanged). Canonical
    /// RAX/RCX entries are left alone. MUST run after the last bytecode is
    /// walked (so every unrolled copy is in the table) and before
    /// `emit_bounds_check_stubs`.
    ///
    /// The two moves are a parallel copy into RAX/RCX: ordered so neither
    /// source is overwritten before it is read, and an exchange when the two
    /// operands sit in each other's target.
    pub(super) fn emit_bounds_check_home_prologues(&mut self) {
        for k in 0..self.bounds_check_stubs.len() {
            let Some(&(patch_off, bc_pc)) = self.bounds_check_stubs.get(k) else {
                break;
            };
            let Some((array, index)) = decode_home_bounds_check(self.buf.as_slice(), patch_off)
            else {
                continue;
            };
            if array == RAX && index == RCX {
                continue;
            }
            self.patch_rel32_to_here(patch_off);
            if array == RCX && index == RAX {
                self.buf.emit(&[0x48, 0x91]); // XCHG RAX, RCX
            } else if index == RAX {
                self.emit_mov_reg_reg(RCX, RAX);
                self.emit_mov_reg_reg(RAX, array);            } else {
                self.emit_mov_reg_reg(RAX, array);
                self.emit_mov_reg_reg(RCX, index);            }
            let jmp_patch = self.emit_jmp_rel32_patch();
            if let Some(entry) = self.bounds_check_stubs.get_mut(k) {
                *entry = (jmp_patch, bc_pc);
            }
        }
    }
}

#[cfg(test)]
mod tests;


