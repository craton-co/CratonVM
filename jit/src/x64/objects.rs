// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Object headers, fields, allocation and the string layout.
//!
//! Everything that addresses an object interior: the inline TLAB bump
//! allocator and the header words it has to initialise, the compact-layout
//! reference `putfield` fast paths, the card-mark and receiver-check helpers,
//! and the `java.lang.String` accessors the string intrinsics are built on.
//!
//! Like `x64/arrays.rs`, every displacement here is a header-offset emission
//! site, and the inline TLAB path in particular writes header words directly
//! rather than through the allocator — `emit_inline_tlab_new` and
//! `Tlab::alloc_initialized` are two implementations of one contract.

use super::*;

/// Reference-store sites that received the GATED inline barrier sequence.
///
/// A count needs a denominator to be readable: zero here means either that no
/// collector published a barrier plan or that the workload compiles no
/// reference stores, and those are different facts. Reported by
/// `jit-method-stats` beside the declined count below.
static GATED_REF_STORE_SITES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Reference-store sites that asked for the gated sequence and were declined —
/// compiled with the full-helper path instead.
static UNGATED_REF_STORE_SITES: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

pub(crate) fn note_gated_ref_store() {
    GATED_REF_STORE_SITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

pub(crate) fn note_ungated_ref_store() {
    UNGATED_REF_STORE_SITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// `(pre, post, young_floor)` when a collector has published a usable
/// reference-store barrier plan.
///
/// Both gate bytes, plus EXACTLY ONE of the two post-barrier shapes. A
/// publisher that supplied neither has no way to rule the post barrier out; one
/// that supplied both would emit two independent skips for one question — and
/// for a mask publisher the floor is not merely redundant but WRONG (an old-gen
/// object with `gc_age == 0` sits below it), which is the whole reason the mask
/// shape exists.
///
/// A free function so the rule can be tested against a helper table alone,
/// without standing up a `Compiler`.
pub(crate) fn ref_store_gates_of(
    helpers: &cratonvm_jit_api::JitRuntimeHelpers,
) -> Option<(usize, usize, usize)> {
    let pre = helpers.ref_store_pre_gate;
    let post = helpers.ref_store_post_gate;
    let floor = helpers.ref_store_post_young_floor;
    let mask = helpers.ref_store_post_skip_mask;
    let one_post_shape = (floor != 0) ^ (mask != 0);
    (pre != 0 && post != 0 && one_post_shape).then_some((pre, post, floor))
}

/// The published post-barrier skip mask, when the plan uses that shape.
///
/// A VALUE, baked as an immediate: which collector is running cannot change
/// after start-up. See `JitRuntimeHelpers::ref_store_post_skip_mask`.
pub(crate) fn ref_store_post_skip_mask_of(
    helpers: &cratonvm_jit_api::JitRuntimeHelpers,
) -> Option<u8> {
    let mask = helpers.ref_store_post_skip_mask;
    // A mask wider than the flags byte would be a publisher bug, and baking it
    // would test bits that byte does not have.
    (mask != 0 && mask <= u8::MAX as usize).then_some(mask as u8)
}

/// `(gated, declined)` reference-store site counts.
pub fn ref_store_site_counts() -> (u64, u64) {
    (
        GATED_REF_STORE_SITES.load(std::sync::atomic::Ordering::Relaxed),
        UNGATED_REF_STORE_SITES.load(std::sync::atomic::Ordering::Relaxed),
    )
}

impl Compiler {
    // -----------------------------------------------------------------------
    // Vectorised and bulk loop bodies
    // -----------------------------------------------------------------------
    //
    // Moved to `x64/simd.rs`. Admission lives in `x64/simd_analysis.rs`; the
    // raw VEX encodings live in `x64/emit.rs`.

    pub(super) fn inline_card_mark_available(&self) -> bool {
        // Keep every old-receiver write on the helper-owned barrier path.
        //
        // The inline byte-store sequence is locally sound, but WildFly's real
        // JIT boot audit observed an old `org/jboss/modules/Module` reference
        // to a young child on a clean card. A clean card is a correctness
        // failure: the next minor collection can reclaim that reachable child.
        // Until the direct emitter has end-to-end coverage for every compiled
        // store form and card-table lifecycle, `jit_putfield_object` remains
        // the single source of truth for old-to-young post barriers. Young
        // receivers still retain their barrier-free direct stores.
        //
        // Keep the published metadata in `JitRuntimeHelpers` for a future
        // verified implementation; merely exposing it must not select the
        // unsafe fast path.
        false
    }

    /// Emit the generational post-write barrier using `source_reg` and
    /// `target_reg`, immediately after the reference-slot store.
    ///
    /// Protocol: slot store -> release dirty-byte store. x86-64 TSO preserves
    /// store-store order, so a plain byte store is the release implementation
    /// and needs no `SFENCE`; the STW consumer acquire-scans the atomic card
    /// bytes before following the old-to-young edge. G1/ZGC never expose this
    /// metadata and retain their helper-owned remembered-set barriers.
    pub(super) fn emit_inline_card_mark_regs(&mut self, source_reg: u8, target_reg: u8) {
        debug_assert!(self.inline_card_mark_available());
        debug_assert!(!matches!(source_reg, RCX | R10 | R11));
        debug_assert!(!matches!(target_reg, RCX | R10 | R11));

        let mut done = Vec::new();
        self.emit_test_r64_r64(target_reg);
        done.push(self.emit_jcc_rel32_patch(0x84)); // null target

        self.emit_mov_imm64_full(R10, self.helpers.jit_card_old_base as i64);
        self.emit_cmp_r64_r64(source_reg, R10);
        done.push(self.emit_jcc_rel32_patch(0x82)); // source below old
        self.emit_mov_imm64_full(R11, self.helpers.jit_card_old_end as i64);
        self.emit_cmp_r64_r64(source_reg, R11);
        done.push(self.emit_jcc_rel32_patch(0x83)); // source at/above old end

        self.emit_cmp_r64_r64(target_reg, R10);
        let target_below_old = self.emit_jcc_rel32_patch(0x82);
        self.emit_cmp_r64_r64(target_reg, R11);
        done.push(self.emit_jcc_rel32_patch(0x82)); // old -> old
        self.patch_rel32_to_here(target_below_old);

        self.emit_mov_r64_r64(RCX, source_reg);
        self.emit_sub_r64_r64(RCX, R10);
        self.emit_shr_r64_imm8(RCX, 9); // CARD_SIZE = 512
        self.emit_mov_imm64_full(R11, self.helpers.jit_card_table_addr as i64);
        self.emit_mov_mem8_indexed_imm8(R11, RCX, 1); // CARD_DIRTY

        for patch in done {
            self.patch_rel32_to_here(patch);
        }
    }

    /// Emit an inline "decode the String character at `idx`" sequence for
    /// the STRING_SEARCH `compareTo` / `indexOf` intrinsics.
    ///
    /// Reads the code unit at element index `idx_reg` of the backing
    /// `byte[]` whose payload starts at `val_reg + HEADER_SIZE`, branching
    /// on `coder_reg` (0 = LATIN1, one byte/char zero-extended; non-zero =
    /// UTF16, two little-endian bytes/char). The zero-extended `u16` result
    /// lands in the low 16 bits of `dst` (upper bits cleared). The four
    /// register operands are distinct 0..=15 GPR numbers; `idx_reg` is the
    /// SIB index and so must not be RSP (4) — and an index field of 100
    /// means "no index", so it must not be R12 (12) either. `val_reg` is
    /// the SIB base and may be any register (R12/RSP as a base is legal
    /// with the disp8 ModRM used here). No CALL; no memory beyond the array
    /// payload is touched.
    pub(super) fn emit_string_decode_char(
        &mut self,
        dst: u8,
        val_reg: u8,
        idx_reg: u8,
        coder_reg: u8,
    ) {
        // TEST coder_reg, coder_reg ; JNZ utf16
        let mut rex = 0x48u8;
        if coder_reg >= 8 {
            rex |= 0x05;
        }
        self.buf.emit_byte(rex);
        self.buf.emit_byte(0x85);
        self.buf
            .emit_byte(0xC0 | ((coder_reg & 7) << 3) | (coder_reg & 7));
        let utf16 = self.emit_jcc_rel32_patch(0x85); // JNZ

        // LATIN1: MOVZX dst32, BYTE [val_reg + idx_reg*1 + HEADER_SIZE].
        // 0F B6 /r with a SIB byte (scale=00 → *1).
        let mut rex = 0x40u8;
        if dst >= 8 {
            rex |= 0x04;
        }
        if idx_reg >= 8 {
            rex |= 0x02;
        }
        if val_reg >= 8 {
            rex |= 0x01;
        }
        if rex != 0x40 {
            self.buf.emit_byte(rex);
        }
        self.buf.emit(&[0x0F, 0xB6]);
        // ModRM: mod=01 (disp8), reg=dst, r/m=100 (SIB follows).
        self.buf.emit_byte(0x40 | ((dst & 7) << 3) | 0x04);
        // SIB: scale=00, index=idx_reg, base=val_reg.
        self.buf.emit_byte(((idx_reg & 7) << 3) | (val_reg & 7));
        // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
        self.buf.emit_byte(HEADER_SIZE as u8);
        let done = self.emit_jmp_rel32_patch();

        // UTF16: MOVZX dst32, WORD [val_reg + idx_reg*2 + HEADER_SIZE].
        self.patch_rel32_to_here(utf16);
        let mut rex = 0x40u8;
        if dst >= 8 {
            rex |= 0x04;
        }
        if idx_reg >= 8 {
            rex |= 0x02;
        }
        if val_reg >= 8 {
            rex |= 0x01;
        }
        if rex != 0x40 {
            self.buf.emit_byte(rex);
        }
        self.buf.emit(&[0x0F, 0xB7]);
        self.buf.emit_byte(0x40 | ((dst & 7) << 3) | 0x04);
        // SIB: scale=01 (*2), index=idx_reg, base=val_reg.
        self.buf
            .emit_byte(0x40 | ((idx_reg & 7) << 3) | (val_reg & 7));
        // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
        self.buf.emit_byte(HEADER_SIZE as u8);
        self.patch_rel32_to_here(done);
    }

    /// Load a String receiver's `value` field (the backing `byte[]`/`char[]`
    /// ref) from `base` into `dst`, correctly handling BOTH object layouts
    /// that can coexist for `java/lang/String` at runtime:
    ///
    ///   * compact-ref-field layout — the address is
    ///     `StringFieldLayout::value_compact_offset`;
    ///   * a LEGACY-laid-out instance of the same class — per the getfield
    ///     (opcode 0xb4) inline path's own comment, "a class with a
    ///     registered compact layout may still have LEGACY-laid-out
    ///     instances" (e.g. an allocation whose field count didn't match the
    ///     registered `CompactLayout` at alloc time). Its address is
    ///     `StringFieldLayout::value_legacy_offset`.
    ///
    /// Both offsets are exact payload addresses computed independently by
    /// `StringFieldLayout::new` (see BUG-STRING-CODER-COMPACT-20260726 there
    /// for why neither may be derived from the other). Dispatches per-object
    /// via the `GC_FLAG_COMPACT` header bit, exactly mirroring the getfield
    /// 0xb4 inline path. No scratch register needed.
    ///
    /// # Compressed oops
    ///
    /// Only the **compact** arm is affected: `cratonvm_types::narrow_oop`
    /// narrows compact reference *instance fields* and reference *array
    /// elements* and nothing else, so a LEGACY-laid-out instance still carries
    /// a full 64-bit pointer in its 16-byte tagged `Value` cell and its arm is
    /// unchanged. This was hole 1 of `gc/src/compressed_oops.rs`'s "two
    /// correctness holes": the compact arm used to be an unconditional 64-bit
    /// load, which under narrow oops read 4 bytes of narrow oop plus 4 bytes of
    /// the adjacent field and dereferenced the result.
    pub(super) fn emit_load_string_value_ptr(
        &mut self,
        dst: u8,
        base: u8,
        compact_offset: i32,
        legacy_offset: i32,
    ) {
        self.emit_test_mem8_imm8(
            base,
            cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32,
            cratonvm_types::GC_FLAG_COMPACT,
        );
        let legacy = self.emit_jcc_rel32_patch(0x84); // JZ (flag clear => legacy)
                                                      // The compact slot is narrow only when a `CompactLayout` was actually
                                                      // registered for this String class — `StringFieldLayout::new`'s
                                                      // fallback points `value_compact_offset` at the LEGACY cell payload,
                                                      // which stays 8 bytes wide. Matching the offset makes this
                                                      // self-checking rather than trusting the caller and the layout to
                                                      // agree.
        let narrow = narrow_oops_enabled()
            && self.string_layout.is_some_and(|l| {
                l.value_compact_offset == compact_offset && l.value_compact_is_narrow
            });
        if narrow {
            self.emit_load_narrow_ref_field(dst, base, compact_offset);
        } else {
            self.emit_mov_r64_mem_disp32(dst, base, compact_offset);
        }
        let done = self.emit_jmp_rel32_patch();
        self.patch_rel32_to_here(legacy);
        self.emit_mov_r64_mem_disp32(dst, base, legacy_offset);
        self.patch_rel32_to_here(done);
    }

    /// Load a **narrow** compact reference field at `[base + offset]` into
    /// `dst` as a full 64-bit pointer, so every consumer downstream is
    /// unchanged. `dst` may alias `base` (the field is read before the base is
    /// clobbered).
    ///
    /// The slot holds `(addr - narrow_base) >> narrow_shift`, with 0 reserved
    /// for null, so the decode is `narrow_base + (n << shift)` — except for
    /// null, which must stay 0 rather than becoming the base. `SHL` sets ZF
    /// from its result, so the null test is free at the shifts the live heap
    /// actually uses; a pinned shift of 0 needs an explicit `TEST`.
    ///
    /// Unlike [`Self::emit_narrow_ref_aload_regs`], this emitter runs at sites
    /// where the register allocator has already parked values in the extended
    /// registers, so it cannot claim R11 outright. It borrows R11 inside the
    /// non-null arm and restores it before falling through: the `PUSH`/`POP`
    /// pair is balanced, straddles no `CALL` and no RSP-relative access, and is
    /// only ever emitted when the (default-off) narrow-oop gate is on.
    fn emit_load_narrow_ref_field(&mut self, dst: u8, base: u8, offset: i32) {
        // MOV dst32, DWORD [base + offset] — writing a 32-bit GPR zero-extends.
        self.emit_mov_r32_mem_disp32(dst, base, offset);
        let shift = cratonvm_types::narrow_oop::narrow_shift();
        if shift > 0 {
            // SHL dst, shift — sets ZF from the result, so null stays testable.
            self.buf.emit_byte(0x48 | ((dst >= 8) as u8));
            self.buf.emit(&[0xC1, 0xE0 | (dst & 7)]);
            // Truncation: `narrow_shift()` is <= 3 (`narrow_oop::enable`).
            self.buf.emit_byte(shift as u8);
        } else {
            self.emit_test_r64_r64(dst);
        }
        let done = self.emit_jcc_rel32_patch(0x84); // JZ — a null oop stays 0.
        self.buf.emit(&[0x41, 0x53]); // PUSH R11
        self.buf.emit(&[0x49, 0xBB]); // MOV R11, imm64
        self.buf.emit(&narrow_base().to_le_bytes());
        // ADD dst, R11
        self.buf.emit_byte(0x4C | ((dst >= 8) as u8));
        self.buf.emit(&[0x01, 0xD8 | (dst & 7)]);
        self.buf.emit(&[0x41, 0x5B]); // POP R11
        self.patch_rel32_to_here(done);
    }

    /// Load `String.coder` / `String.hash` into `dst` with the same
    /// per-object compact/legacy dispatch as
    /// [`Self::emit_load_string_value_ptr`].
    ///
    /// `compact_is_byte` selects a zero-extending BYTE load for the compact
    /// arm: `CompactLayout` stores `coder` at its natural one-byte Java
    /// width, and the bytes that follow it are the class's padding — a
    /// 4-byte load there would fold that padding into the value. The legacy
    /// arm is always a sign-extended 4-byte `Value` payload load. `coder`
    /// and `hash` are non-negative in practice, so sign- vs zero-extension
    /// is behaviourally identical for the widths that do overlap.
    pub(super) fn emit_load_string_i32_field(
        &mut self,
        dst: u8,
        base: u8,
        compact_offset: i32,
        compact_is_byte: bool,
        legacy_offset: i32,
    ) {
        self.emit_test_mem8_imm8(
            base,
            cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32,
            cratonvm_types::GC_FLAG_COMPACT,
        );
        let legacy = self.emit_jcc_rel32_patch(0x84);
        if compact_is_byte {
            // MOVZX dst64, BYTE [base + compact_offset]
            self.emit_movx_r64_mem_disp32(dst, base, compact_offset, 8, false);
        } else {
            self.emit_movsxd_r64_mem_disp32(dst, base, compact_offset);
        }
        let done = self.emit_jmp_rel32_patch();
        self.patch_rel32_to_here(legacy);
        self.emit_movsxd_r64_mem_disp32(dst, base, legacy_offset);
        self.patch_rel32_to_here(done);
    }

    /// Emit a compiled `getstatic` as a direct load, with no helper `CALL`.
    ///
    /// Returns `false` when the site cannot be inlined, in which case the
    /// caller must keep the `jit_getstatic` path; `true` means the value has
    /// been pushed (and the oop mark / volatile fence emitted) already.
    ///
    /// # Shape
    ///
    /// ```text
    ///   MOV RAX, imm64          ; &statics_index[class].base  (the POINTER cell)
    ///   MOV RAX, [RAX]          ; the class's statics block base
    ///   MOV/MOVSXD RAX, [RAX + field_index*16 + payload_off]
    /// ```
    ///
    /// The first two are what replaces a helper round trip; the third is the
    /// same load the inline `getfield` arms emit, against the same 16-byte
    /// `Value` cell layout (`FIELD_CELL_PAYLOAD*_OFFSET`, pinned by
    /// `field_cell_layout_matches_value_enum`). Result conventions match
    /// `jit_getstatic` exactly: `MOVSXD` for the int category (`Value::Int(i)
    /// => i as i64`), a 32-bit zero-extending `MOV` for float (`f.to_bits() as
    /// i64`), a 64-bit `MOV` of the payload word for long/double/reference
    /// (`Object(None)` leaves that word zero, i.e. JVM null).
    ///
    /// # What is NOT emitted, and why that is safe
    ///
    /// * **No class-init check.** The resolver only answers for a class that is
    ///   already initialized, and initialization is monotonic.
    /// * **No exception check.** With no call there is no `i64::MIN` deopt
    ///   sentinel to disambiguate — which also removes a latent bug the helper
    ///   path still has, where a `static long` legitimately holding
    ///   `Long.MIN_VALUE` is indistinguishable from a thrown `<clinit>`.
    /// * **No `flush_scratch_registers`.** Nothing here clobbers a register the
    ///   operand-stack cache can hold: `SCRATCH_REGS` is `[R8, R9]` and this
    ///   sequence touches only RAX, which the helper path clobbers anyway.
    /// * **No plausibility check on a reference payload.** Same contract as the
    ///   inline `getfield` arms, which also raw-load the payload word.
    pub(super) fn try_emit_inline_getstatic(
        &mut self,
        class_id_raw: u32,
        field_index: usize,
        type_tag: u8,
        is_volatile: bool,
    ) -> bool {
        if !inline_getstatic_enabled() {
            return false;
        }
        let Some(base_cell) = resolve_static_base(class_id_raw, field_index) else {
            return false;
        };
        // Cast: cell byte offset within the class's statics block -> disp32.
        let Ok(cell_off) = i32::try_from(field_index.saturating_mul(SLOT_SIZE)) else {
            return false;
        };
        // Cast: the baked address of the never-freed base-pointer cell.
        self.emit_mov_imm64(RAX, base_cell as i64);
        self.emit_mov_r64_mem_disp32(RAX, RAX, 0);
        match type_tag {
            b'J' | b'D' | b'L' | b'[' => self.emit_mov_r64_mem_disp32(
                RAX,
                RAX,
                // Cast: fixed layout offset to i32 instruction displacement
                cell_off + FIELD_CELL_PAYLOAD64_OFFSET as i32,
            ),
            b'F' => self.emit_mov_r32_mem_disp32(
                RAX,
                RAX,
                // Cast: fixed layout offset to i32 instruction displacement
                cell_off + FIELD_CELL_PAYLOAD32_OFFSET as i32,
            ),
            _ => self.emit_movsxd_r64_mem_disp32(
                RAX,
                RAX,
                // Cast: fixed layout offset to i32 instruction displacement
                cell_off + FIELD_CELL_PAYLOAD32_OFFSET as i32,
            ),
        }
        // Volatile static: MFENCE after the read, exactly as the helper arm does
        // (x86-64 already gives acquire ordering for the load itself).
        if is_volatile {
            self.buf.emit(&[0x0F, 0xAE, 0xF0]); // MFENCE
        }
        self.push_from_rax();
        // A reference-typed static's loaded value is a live oop — same
        // obligation as the helper arm (T1.1.a).
        if type_tag == b'L' || type_tag == b'[' {
            self.mark_top_as_oop();
        }
        true
    }

    /// Null / alignment / three-region containment on the receiver in RAX,
    /// returning the patch sites the caller must route to its slow path.
    ///
    /// **Which table `bounds_addr` names is the caller's decision, and the two
    /// kinds of caller decide differently.** The six-word
    /// `[b0, e0, b1, e1, b2, e2]` layout is shared, so the emitted bytes are
    /// identical and only the baked immediate differs -- but the two tables
    /// answer different questions:
    ///
    /// * READ callers (the `getfield` arms, and `ir_lower`'s copy of this
    ///   sequence) pass `helpers.read_bounds_addr` -- `JIT_READ_BOUNDS`, the
    ///   "is this address mapped, so a raw load cannot fault" table, which
    ///   Generational and G1 both fill and ZGC deliberately does not;
    /// * STORE callers (the inline reference-`putfield` arms) pass
    ///   `helpers.region_bounds_addr` -- `JIT_REGION_BOUNDS`, whose EMPTINESS
    ///   under G1/ZGC is load-bearing: it is what stops an inline store from
    ///   skipping `post_write_barrier_rset` and losing the remembered-set edge
    ///   a JNI-pinned, CSet-excluded region is reachable only through
    ///   (`audits/g1-audit.md` 8.1). Those callers additionally gate on
    ///   [`region_bounds_are_live`], which reads that table's CONTENT.
    ///
    /// Handing the read table to a store caller would silently unblock exactly
    /// the fast path G1-2 exists to block. Two tables rather than one is what
    /// makes that mistake something you have to type out rather than inherit.
    pub(super) fn emit_guarded_getfield_receiver_check(
        &mut self,
        bounds_addr: usize,
    ) -> Vec<usize> {
        let mut slow: Vec<usize> = Vec::new();
        // 1. null → slow (helper throws the NPE).
        self.emit_test_r64_r64(RAX);
        slow.push(self.emit_jcc_rel32_patch(0x84)); // JZ
                                                    // 2. alignment: low 3 bits must be clear.
        self.emit_mov_r64_r64(RCX, RAX);
        self.emit_and_r64_imm8(RCX, 7);
        slow.push(self.emit_jcc_rel32_patch(0x85)); // JNZ
                                                    // 3. region containment. RDX = the caller's bounds table (six
                                                    //    usize words: [b0, e0, b1, e1, b2, e2]) -- READ callers pass
                                                    //    JIT_READ_BOUNDS, STORE callers JIT_REGION_BOUNDS; see above.
        self.emit_mov_imm64(RDX, bounds_addr as i64);
        // region 0: RAX >= b0 && RAX < e0 → ok
        self.emit_cmp_r64_mem_disp(RAX, RDX, 0);
        let below_b0 = self.emit_jcc_rel32_patch(0x82); // JB → try region 1
        self.emit_cmp_r64_mem_disp(RAX, RDX, 8);
        let ok0 = self.emit_jcc_rel32_patch(0x82); // JB → in region 0
        self.patch_rel32_to_here(below_b0);
        // region 1
        self.emit_cmp_r64_mem_disp(RAX, RDX, 16);
        let below_b1 = self.emit_jcc_rel32_patch(0x82); // JB → try region 2
        self.emit_cmp_r64_mem_disp(RAX, RDX, 24);
        let ok1 = self.emit_jcc_rel32_patch(0x82); // JB → in region 1
        self.patch_rel32_to_here(below_b1);
        // region 2 — last chance: outside → slow.
        self.emit_cmp_r64_mem_disp(RAX, RDX, 32);
        slow.push(self.emit_jcc_rel32_patch(0x82)); // JB → slow
        self.emit_cmp_r64_mem_disp(RAX, RDX, 40);
        slow.push(self.emit_jcc_rel32_patch(0x83)); // JAE → slow
                                                    // fall-through / ok: receiver is inside a published live region.
        self.patch_rel32_to_here(ok0);
        self.patch_rel32_to_here(ok1);
        slow
    }

    /// Cheaper receiver guard for a value whose operand-stack type is already
    /// proven to be an oop by the bytecode/type tracker. Such a value cannot be
    /// an unaligned integer or an arbitrary out-of-heap address without an
    /// earlier JIT/GC correctness failure, so repeating the six arena-bound
    /// comparisons at every field access is redundant. Null remains a real
    /// Java exceptional case and is routed to the existing checked helper.
    pub(super) fn emit_trusted_oop_receiver_check(&mut self) -> Vec<usize> {
        self.emit_test_r64_r64(RAX);
        vec![self.emit_jcc_rel32_patch(0x84)] // JZ -> checked helper
    }

    /// [`Self::emit_trusted_oop_receiver_check`], with the check DROPPED when
    /// the null-check dataflow already proves the receiver non-null at
    /// `bc_pc`. Returns an empty patch list in that case, which every caller
    /// already handles — the slow path is simply unreachable.
    ///
    /// # Only `getfield` may call this
    ///
    /// The proof is `preceding_aload_nonnull_local`: the local named by the
    /// `aload` that ends exactly at `bc_pc`. That local is the receiver only
    /// when the receiver is the value on TOP of the operand stack, which is
    /// true of `getfield` and of nothing else nearby:
    ///
    /// * `putfield`'s stack is `[…, objectref, value]` — the preceding push is
    ///   the stored VALUE, and attributing the receiver's fact to it is the
    ///   exact shape of the Tomcat `MessageBytes.setString` miscompile that
    ///   `opcode_dereferences_receiver` documents at length.
    /// * `checkcast` does not throw on null at all — a null cast succeeds —
    ///   so its `JZ` targets a legal null path, not an NPE. Eliding it would
    ///   let a null receiver fall into the `KIND_TAGS` byte compare and fault.
    ///
    /// # Why this cannot see a spliced callee's bytecode
    ///
    /// `self.null_check_info` is analysed from the caller's `code` and indexed
    /// by caller bci. `compile_bytecode` is called exactly once, on that same
    /// array (`driver.rs`), and the inline splicer emits callee bodies through
    /// its own emitters rather than re-entering the walk — so `code` and
    /// `bc_pc` here always denote the method the analysis actually ran on. A
    /// future splicer that DID re-enter `compile_bytecode` with a callee's
    /// bytecode would break that pairing silently, which is why it is written
    /// down rather than left to be rediscovered.
    pub(super) fn emit_trusted_oop_receiver_check_at(
        &mut self,
        code: &[u8],
        bc_pc: usize,
        implicit_ok: bool,
    ) -> Vec<usize> {
        if super::null_check_elim::receiver_null_elim_enabled() {
            if let Some(local) = super::null_check_elim::preceding_aload_nonnull_local(code, bc_pc)
            {
                if self.is_local_nonnull(bc_pc, local) {
                    super::null_check_elim::note_receiver_null_check_elided();
                    return Vec::new();
                }
            }
        }
        // No proof. The check can still leave the fast path, if the fault it
        // would have prevented is caught and translated instead — which is
        // what the implicit null check is. The instruction that will occupy
        // `self.buf.pos()` is the receiver dereference; the caller binds its
        // recovery address, and verifies its encoding, in
        // `bind_implicit_null_recovery`.
        //
        // `implicit_ok` is the caller asserting that it emits such a
        // dereference NEXT and unconditionally. Only the compact `getfield`
        // arm does: its `GC_FLAGS` read at `[RAX + 15]` always follows. The
        // second arm emits that read only under
        // `compact_ref_fields_enabled()`, so it passes `false` rather than
        // make the guarantee conditional — the verification would catch a
        // wrong answer, but as a failed compile on a live workload rather than
        // as a decision made here.
        if implicit_ok && crate::implicit_null::enabled() {
            self.implicit_null_pending.push((self.buf.pos(), bc_pc));
            super::null_check_elim::note_receiver_null_check_implicit();
            return Vec::new();
        }
        super::null_check_elim::note_receiver_null_check_emitted();
        self.emit_trusted_oop_receiver_check()
    }

    // -----------------------------------------------------------------
    // F-08 — the inline G1 post-write barrier
    // -----------------------------------------------------------------

    /// Byte offsets into the published `JIT_G1_BARRIER` table
    /// (`gc/src/gen_heap.rs::JitG1BarrierTable`). Five `usize` words:
    /// `[arena_base, arena_len, region_mask, card_table_base, card_shift]`.
    pub(super) const G1B_ARENA_BASE: i32 = 0;
    pub(super) const G1B_ARENA_LEN: i32 = 8;
    pub(super) const G1B_REGION_MASK: i32 = 16;

    /// F-08 — may this compile emit a real G1 post-write barrier inline,
    /// instead of routing every reference store to `jit_putfield_object`?
    ///
    /// Three things must hold, and each of them is a separate hazard:
    ///
    /// * the opt-in flag is set (`CRATONVM_G1_INLINE_BARRIER`, default OFF);
    /// * a G1 collector has published its geometry into `JIT_G1_BARRIER`, so
    ///   the arena base, length and region mask the sequence loads are real;
    /// * the lean barrier helper is wired, since the inline arm's slow path
    ///   CALLs it and a zero there would be a call to address 0. A hand-built
    ///   test helper table leaves it zero; that is the "not wired" contract
    ///   every optional slot in `JitRuntimeHelpers` carries.
    ///
    /// **This does not, and must not, re-open defect G1-2.** That defect is
    /// about an inline store that SKIPS the barrier; `region_bounds_are_live`
    /// stays false under G1 and every generational-style barrier-free arm stays
    /// unreachable there. What this enables is an arm that EMITS the barrier —
    /// the same remembered-set edge `post_write_barrier_rset` records, with the
    /// two cases in which that function provably does nothing filtered out
    /// inline. See `emit_g1_post_write_barrier_regs` for that argument in full.
    ///
    /// It is also not `inline_card_mark_available()`, which is a deliberate
    /// constant `false` and stays one. That is the GENERATIONAL card mark,
    /// disabled after a WildFly boot audit found an old `org/jboss/modules/
    /// Module` reference to a young child left on a CLEAN card. Different
    /// mechanism, different table, different collector; the two are kept
    /// separate so that re-enabling one never silently re-enables the other.
    /// # Which methods this can reach at all (F-08 residual, measured)
    ///
    /// Only the ones compiled by THIS tier. `ir_lower` — the IR tier — has no
    /// reference-store site whatsoever; its own `read_bounds_addr` doc says so
    /// in as many words ("this tier emits no inline reference STORE... The
    /// store question... has no site here to ask it"), and it asks only the
    /// read-side mapped-address question. A method the IR tier compiles
    /// therefore keeps the out-of-line `putfield_object` helper no matter what
    /// this predicate answers.
    ///
    /// That is visible from outside, and was measured rather than assumed. With
    /// `RUST_LOG=cratonvm_jit=info`, the ACTIVE line below appears exactly once
    /// on `apps/g1_probe/G1CardChurn` with the flag on and never with it off —
    /// and never on `probes/G1ChurnPauseProbe` in either arm, whose hot stores
    /// are constructor field writes in a method this tier does not own.
    ///
    /// So the barrier's reach is bounded by which tier compiles the storing
    /// method, and the workload that exhibits the barrier and the workload that
    /// exhibits pause behaviour are not the same one. That is the honest reason
    /// F-08 still has no pause-level number, and it is a `jit/` change to fix,
    /// not a `gc/` one.
    ///
    /// (A note on measuring this: a bare `RUST_LOG=info` shows nothing. The
    /// launcher builds its filter as `from_default_env().add_directive(WARN)`,
    /// and a global WARN ties with a global `info` on specificity, resolving
    /// last-added-wins. A target-scoped `RUST_LOG=cratonvm_jit=info` is more
    /// specific and wins. Getting that wrong reads as "the arm never engages".)
    pub(super) fn g1_inline_barrier_available(&self) -> bool {
        g1_inline_barrier_enabled()
            && g1_barrier_table_live(self.helpers.g1_barrier_addr)
            && self.helpers.g1_post_write_barrier != 0
    }

    /// F-08 — receiver guard for the inline G1 store arm: null, alignment and
    /// containment in a mapped arena, returning the patch sites the caller
    /// routes to its slow path.
    ///
    /// **Why this passes `read_bounds_addr` where the store arms pass
    /// `region_bounds_addr`, and why that is not the mistake the doc on
    /// [`Self::emit_guarded_getfield_receiver_check`] warns about.**
    ///
    /// That warning says handing the READ table to a STORE caller "would
    /// silently unblock exactly the fast path G1-2 exists to block". It is
    /// about a caller that uses containment as its LICENCE TO SKIP THE
    /// BARRIER: under G1 the store-side table is empty, every receiver is
    /// rejected, and that rejection is what forces the helper. Swapping in a
    /// table G1 does publish would let those receivers through with no barrier
    /// at all.
    ///
    /// This caller does not skip the barrier. It emits one
    /// ([`Self::emit_g1_post_write_barrier_regs`]) on the path this guard
    /// admits. Containment here is doing its ORIGINAL job and only that job —
    /// "is this address inside mapped arena memory, so the header reads and the
    /// 8-byte field store that follow cannot fault" — which is precisely the
    /// question `JIT_READ_BOUNDS` answers and which G1 publishes into. The
    /// store-side table is untouched and `region_bounds_are_live` still reads
    /// it and still says no.
    ///
    /// If this guard admitted a receiver it should not, the failure mode is a
    /// fault or a corrupt store, not a lost remembered-set edge; the barrier
    /// below runs for every admitted receiver regardless.
    pub(super) fn emit_g1_store_receiver_check(&mut self) -> Vec<usize> {
        self.emit_guarded_getfield_receiver_check(self.helpers.read_bounds_addr)
    }

    /// F-08 — G1's post-write barrier, inline.
    ///
    /// Preconditions: `obj_reg` holds the receiver and `val_reg` the stored
    /// reference, the inline store has already happened, and `scratch` is a
    /// register the caller does not need afterwards. All three are clobbered.
    /// `obj_slot` / `val_slot` are the stack slots the operands came from, so
    /// the slow arm can reload them into the ABI argument registers without
    /// depending on what the filter did to the originals.
    ///
    /// # The sequence
    ///
    /// ```text
    ///   test val, val                 ; a null store records nothing
    ///   jz   done
    ///   mov  scratch, imm64 &JIT_G1_BARRIER
    ///   sub  obj, [scratch + 0]       ; obj - arena_base
    ///   sub  val, [scratch + 0]       ; val - arena_base
    ///   xor  obj, val
    ///   and  obj, [scratch + 16]      ; & region_mask, sets ZF
    ///   jz   done                     ; same region: nothing to remember
    ///   <reload ABI args from slots>
    ///   call g1_post_write_barrier
    /// done:
    /// ```
    ///
    /// # Why eliding those two cases is sound
    ///
    /// `G1Collector::post_write_barrier_rset` opens with exactly the same two
    /// tests and returns without touching anything when either fires:
    ///
    /// * a null stored reference has `lookup_region_for_addr(0) == None`, so
    ///   the `(Some(s), Some(d)) if s != d` match arm cannot be taken;
    /// * two addresses in the same region take the same `_ => return` arm.
    ///
    /// So the inline filter removes calls whose callee would have returned, and
    /// never a call that would have recorded. The remaining cases — an address
    /// outside G1's arena, a destination region that is Free, an edge this
    /// thread already recorded — are all left to the callee, which already
    /// distinguishes them and which is where the F-05 card store lives.
    ///
    /// # Why the arena base is SUBTRACTED rather than the XOR taken raw
    ///
    /// `(obj ^ val) & region_mask == 0` asks whether the two addresses share an
    /// aligned `region_size` block of the address space, and G1's arena is not
    /// region-aligned, so an aligned block is NOT a region: exactly one region
    /// boundary falls inside each block, and two addresses straddling it would
    /// be called "same region", the barrier skipped, and a live cross-region
    /// edge lost. That is a use-after-free, so the two subtractions are
    /// load-bearing rather than tidy.
    ///
    /// The alignment the arena actually has has already changed once under this
    /// reasoning and the argument must not come to depend on it. It was a
    /// `Vec<u8>` (malloc-aligned) when this was written; since F-16 it is an
    /// `mmap` / `VirtualAlloc` reservation, so 4 KiB on Linux and 64 KiB on
    /// Windows. Neither is a region — the default region size is 1 MiB and the
    /// ergonomic can take it to 32 MiB — and neither is guaranteed by anything
    /// the collector promises. The subtractions are correct for ANY base, which
    /// is the property to preserve.
    ///
    /// An out-of-arena operand makes its subtraction wrap to a huge value; that
    /// can only make the XOR differ and send the store to the helper, which
    /// then no-ops. The failure direction is a wasted call, never a lost edge.
    ///
    /// # Why the card is not dirtied here
    ///
    /// It could be — the table carries the card base and shift — but it would
    /// buy nothing. The remembered-set ENTRY still has to be recorded, and that
    /// is a hash-map insert keyed on a (source region, target region) pair with
    /// no inline form. Dirtying the card inline and then calling anyway is
    /// duplicated work; the callee dirties it on the way through. What would
    /// change this is Phase 2 taking its source set from the card table instead
    /// of from the region-index remembered set, which is a collector policy
    /// change and not an emitter one.
    pub(super) fn emit_g1_post_write_barrier_regs(
        &mut self,
        obj_reg: u8,
        val_reg: u8,
        scratch: u8,
        obj_slot: StackSlot,
        val_slot: StackSlot,
    ) {
        let nothing_to_do = self.emit_g1_barrier_filter(obj_reg, val_reg, scratch);
        // Everything the filter could not dismiss: the collector's own barrier.
        // Both operands are reloaded from their stack slots, because the filter
        // destroyed the registers they were in.
        self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
        self.load_slot_to_reg(ARG_REGS[1], obj_slot);
        self.load_slot_to_reg(ARG_REGS[2], val_slot);
        self.emit_call_absolute(self.helpers.g1_post_write_barrier);
        for patch in nothing_to_do {
            self.patch_rel32_to_here(patch);
        }
    }

    /// F-08 — the two-test filter alone, without the call it guards.
    ///
    /// Returns the jump sites the caller must patch to "nothing to remember".
    /// Clobbers all three registers.
    ///
    /// Split from [`Self::emit_g1_post_write_barrier_regs`] so the filter can
    /// be EXECUTED in a unit test without a compiled frame — the call arm
    /// reloads its operands through `emit_load_local` / `load_slot_to_reg`,
    /// which need a real prologue and a real heap local, and that requirement
    /// would otherwise put the part of this sequence that can silently
    /// miscompile (three instruction encodings this file had no other user for,
    /// and an address-arithmetic argument) beyond the reach of any test that
    /// runs the code. Same motive as `RememberedSet::add_reference_in_generation_within`:
    /// make the risky half addressable on its own.
    pub(super) fn emit_g1_barrier_filter(
        &mut self,
        obj_reg: u8,
        val_reg: u8,
        scratch: u8,
    ) -> Vec<usize> {
        // Engagement, not assumption. The codebase's own rule ("never assume a
        // gated path was taken -- verify") is why `g1: parallel evacuation
        // ACTIVE` exists, and it applies twice over to a barrier that is opt-in
        // AND behind three conjoined conditions: a run whose checksum matches
        // HotSpot proves nothing about this arm unless something says the arm
        // was emitted. One line per process, at `info`.
        {
            static LOGGED: std::sync::atomic::AtomicBool =
                std::sync::atomic::AtomicBool::new(false);
            if !LOGGED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                tracing::info!(
                    "jit: G1 inline post-write barrier ACTIVE (F-08, CRATONVM_G1_INLINE_BARRIER)"
                );
            }
        }
        debug_assert!(
            self.helpers.g1_barrier_addr != 0,
            "F-08: emit_g1_barrier_filter called with no JIT_G1_BARRIER table — \
             the sequence would load through a null table address"
        );
        // 1. Null stored reference: `post_write_barrier_rset` returns.
        self.emit_test_r64_r64(val_reg);
        let done_null = self.emit_jcc_rel32_patch(0x84); // JZ
                                                         // 2. Same region: `post_write_barrier_rset` returns.
        self.emit_mov_imm64(scratch, self.helpers.g1_barrier_addr as i64);
        self.emit_sub_r64_mem_disp32(obj_reg, scratch, Self::G1B_ARENA_BASE);
        self.emit_sub_r64_mem_disp32(val_reg, scratch, Self::G1B_ARENA_BASE);
        self.emit_xor_r64_r64(obj_reg, val_reg);
        self.emit_and_r64_mem_disp32(obj_reg, scratch, Self::G1B_REGION_MASK);
        let done_same = self.emit_jcc_rel32_patch(0x84); // JZ
        vec![done_null, done_same]
    }

    /// The full-barrier route every inline reference-`putfield` arm falls back
    /// to: `jit_putfield_object(heap, obj, field_index, value)`, which performs
    /// the SATB pre-barrier and the collector's OWN post-write barrier — G1's
    /// `post_write_barrier_rset` included, which is the remembered-set edge a
    /// JNI-pinned (CSet-excluded) young region is reachable only through.
    ///
    /// Factored out for G1-2 so the "bounds are not live ⇒ take the helper"
    /// short-circuit is literally the same instruction sequence as the bail
    /// target the fast paths already patch to.
    fn emit_ref_putfield_helper_call(
        &mut self,
        obj_slot: StackSlot,
        val_slot: StackSlot,
        field_index: usize,
    ) {
        self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
        self.load_slot_to_reg(ARG_REGS[1], obj_slot);
        self.emit_mov_imm32_sx(ARG_REGS[2], field_index as i32); // Cast: x86-64 immediate encoding
        self.load_slot_to_reg(ARG_REGS[3], val_slot);
        self.emit_call_absolute(self.helpers.putfield_object);
    }

    /// Emit a compact reference-field store with a barrier-free fast path and
    /// the validated helper as its slow path.
    ///
    /// Small callees such as constructors are emitted by
    /// `try_emit_inline_body`, not the top-level bytecode loop. Keeping this
    /// emitter shared inside `Compiler` makes their field stores follow the
    /// same safety contract as top-level compact `putfield`: only a mapped,
    /// genuinely compact, young receiver whose old field is null is written
    /// directly. Every case requiring SATB/card barriers goes through
    /// `jit_putfield_object`.
    pub(super) fn emit_inline_body_compact_ref_putfield(
        &mut self,
        obj_slot: StackSlot,
        val_slot: StackSlot,
        field_index: usize,
        compact_body_offset: u32,
    ) {
        let cell_off = (HEADER_SIZE + compact_body_offset as usize) as i32;

        // F-08 — the G1 arm. Entered only when a G1 collector has published
        // its geometry AND the opt-in flag is set; it emits a REAL G1
        // post-write barrier after the store instead of borrowing the
        // generational arm's "a young receiver needs no barrier" premise,
        // which is false under G1 (see G1-2 below and `audits/g1-audit.md`
        // §10). `region_bounds_are_live` is deliberately NOT consulted for it
        // and stays false under G1 — the store-side table is untouched.
        let g1 = self.g1_inline_barrier_available();

        // G1-2: no published bounds ⇒ no generational card metadata ⇒ the
        // "young receiver needs no post barrier" premise does not hold (G1's
        // RSet edge into a JNI-pinned, CSet-excluded region would be lost).
        // The containment guard below would reject every receiver anyway with
        // an all-zero table, and with an unwired table it would bake a
        // `MOV RDX,0` + `CMP RAX,[RDX]` that faults — so take the helper
        // outright instead of emitting an inline path that can never run.
        if !g1 && !region_bounds_are_live(self.helpers.region_bounds_addr) {
            self.emit_ref_putfield_helper_call(obj_slot, val_slot, field_index);
            return;
        }

        let mut bail: Vec<usize> = Vec::new();

        self.load_slot_to_reg(RAX, obj_slot);
        if g1 {
            bail.extend(self.emit_g1_store_receiver_check());
        } else {
            bail.extend(self.emit_guarded_getfield_receiver_check(self.helpers.region_bounds_addr));
        }

        // A registered compact class may still have legacy instances when a
        // synthetic/native allocation used a mismatched slot count.
        self.emit_test_mem8_imm8(
            RAX,
            cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32,
            cratonvm_types::GC_FLAG_COMPACT,
        );
        bail.push(self.emit_jcc_rel32_patch(0x84)); // JZ legacy -> helper

        // Without direct generational card metadata, old receivers retain the
        // collector-specific helper. Otherwise the post-store mark is inline.
        //
        // F-08: the G1 arm skips this test entirely, and that is the point.
        // `GC_FLAG_OLD_GEN` is a GENERATIONAL bit; G1 stamps it (defect G1-1's
        // fix) but its own post barrier does not care about it, because a G1
        // remembered-set edge is cross-REGION, not old-to-young. Testing it
        // here would send every promoted receiver to the helper for no reason
        // while doing nothing for the young-into-pinned-region case that
        // actually needs the barrier.
        if !g1 && !self.inline_card_mark_available() {
            self.emit_test_mem8_imm8(
                RAX,
                cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32,
                cratonvm_types::GC_FLAG_OLD_GEN,
            );
            bail.push(self.emit_jcc_rel32_patch(0x85)); // JNZ old -> helper
        }

        // A non-null old value needs the SATB pre-barrier.
        self.emit_mov_r64_mem_disp32(RCX, RAX, cell_off);
        self.emit_test_r64_r64(RCX);
        bail.push(self.emit_jcc_rel32_patch(0x85)); // JNZ non-null -> helper

        // Match the interpreter/helper's silent out-of-bounds drop.
        self.emit_mov_r32_mem_disp32(RCX, RAX, cratonvm_types::NUM_SLOTS_OFFSET as i32);
        self.emit_mov_imm64(RDX, field_index as i64);
        self.emit_cmp_r32_r32(RDX, RCX);
        let oob = self.emit_jcc_rel32_patch(0x83); // JAE -> drop

        // Compact reference fields are bare 8-byte pointers.
        self.load_slot_to_reg(RDX, val_slot);
        self.emit_mov_mem_disp32_r64(RAX, RDX, cell_off);
        if g1 {
            // F-08. RCX is dead here (it last held the num_slots bound), so it
            // is the scratch; RAX and RDX are clobbered by the filter and the
            // slow arm reloads both from their slots.
            self.emit_g1_post_write_barrier_regs(RAX, RDX, RCX, obj_slot, val_slot);
        } else if self.inline_card_mark_available() {
            self.emit_inline_card_mark_regs(RAX, RDX);
        }
        let done = self.emit_jmp_rel32_patch();

        for b in bail {
            self.patch_rel32_to_here(b);
        }
        self.emit_ref_putfield_helper_call(obj_slot, val_slot, field_index);

        self.patch_rel32_to_here(oob);
        self.patch_rel32_to_here(done);
    }

    /// Constructor-only specialization for the first syntactic write to a
    /// compact reference field.
    ///
    /// JVM verification only permits `<init>` on a non-null uninitialized
    /// object produced by `new`. The inline resolver additionally admits only
    /// empty super-constructor chains and forward control flow. Therefore the
    /// first write to a given field starts from null and its resolved slot is
    /// in bounds. A young compact receiver needs no barrier; the only runtime
    /// checks retained are the per-object compact flag (synthetic allocations
    /// can still use legacy cells) and old-generation bit (allocation spill).
    ///
    /// G1-2 (`audits/g1-audit.md` §8.1): "a young compact receiver needs no
    /// barrier" is a GENERATIONAL claim. This emitter used to state it with no
    /// receiver guard whatsoever — not even the null test its two sibling
    /// emitters have — so on a backend that publishes no region bounds it wrote
    /// the reference inline and lost the collector's post-write barrier. Under
    /// G1 that is the JNI-pinned-young-region remembered-set edge (a pinned
    /// region is excluded from the CSet, so its rset is the ONLY way in), i.e. a
    /// use-after-free. It now takes the helper outright when
    /// [`region_bounds_are_live`] is false, and when it is true it emits the
    /// same null test the trusted-oop arms emit.
    pub(super) fn emit_inline_fresh_ctor_compact_ref_putfield(
        &mut self,
        obj_slot: StackSlot,
        val_slot: StackSlot,
        field_index: usize,
        compact_body_offset: u32,
    ) {
        let cell_off = (HEADER_SIZE + compact_body_offset as usize) as i32;

        // F-08 — the G1 arm, as in `emit_inline_body_compact_ref_putfield`.
        // §10 of the audit named THIS emitter as where closing G1-2 costs
        // measurable time: `n.left = newChild` inside `<init>` is the shape
        // that dominates allocation-heavy code, and it became a helper call.
        // The arm below stores inline and then runs a real G1 post barrier,
        // whose common case for that shape — parent and child allocated back
        // to back in one Eden region — is two instructions and a not-taken
        // branch.
        //
        // §10 also explains why the barrier cannot simply be ELIDED for a
        // freshly allocated receiver: G1 pinning is region-granular, the
        // allocator does not avoid pinned regions, and a pinned young region
        // is held out of the collection set and reached only through its
        // remembered set. So the store must run a barrier; it just does not
        // have to run a CALL.
        let g1 = self.g1_inline_barrier_available();

        // G1-2: bounds not live ⇒ not the generational backend ⇒ every
        // reference store must run the collector's own post-write barrier.
        if !g1 && !region_bounds_are_live(self.helpers.region_bounds_addr) {
            self.emit_ref_putfield_helper_call(obj_slot, val_slot, field_index);
            return;
        }

        let mut bail: Vec<usize> = Vec::new();

        self.load_slot_to_reg(RAX, obj_slot);
        // G1-2: receiver guard, consistent with the other two emitters. The
        // full containment check is deliberately NOT repeated here — with
        // bounds live the backend is Generational, and this receiver is the
        // `new`-produced uninitialized object the JVM verifier requires for
        // `<init>` (see the precondition above), so the remaining exceptional
        // case is null. It is unreachable in practice (the caller already
        // emitted `emit_precise_null_check_field_store`) and therefore costs a
        // perfectly-predicted not-taken branch; without it a null receiver
        // faulted on the `gc_flags` header read below instead of reaching the
        // helper's defined no-op semantics.
        // F-08: the G1 arm takes the FULL containment guard rather than the
        // bare null test. The trusted-oop substitution's premise is "with
        // bounds live the backend is Generational", which is exactly what this
        // arm falsifies, so it cannot inherit the cheaper check — and the
        // header reads and 8-byte store below need the receiver to be inside
        // mapped arena memory whatever the collector is.
        if g1 {
            bail.extend(self.emit_g1_store_receiver_check());
        } else {
            bail.extend(self.emit_trusted_oop_receiver_check());
        }
        self.emit_test_mem8_imm8(
            RAX,
            cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32,
            cratonvm_types::GC_FLAG_COMPACT,
        );
        bail.push(self.emit_jcc_rel32_patch(0x84)); // JZ legacy -> helper
        if !g1 && !self.inline_card_mark_available() {
            self.emit_test_mem8_imm8(
                RAX,
                cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32,
                cratonvm_types::GC_FLAG_OLD_GEN,
            );
            bail.push(self.emit_jcc_rel32_patch(0x85)); // JNZ old -> helper
        }

        self.load_slot_to_reg(RDX, val_slot);
        self.emit_mov_mem_disp32_r64(RAX, RDX, cell_off);
        if g1 {
            // F-08 — RCX is untouched by this emitter, so it is free scratch.
            self.emit_g1_post_write_barrier_regs(RAX, RDX, RCX, obj_slot, val_slot);
        } else if self.inline_card_mark_available() {
            self.emit_inline_card_mark_regs(RAX, RDX);
        }
        let done = self.emit_jmp_rel32_patch();

        for b in bail {
            self.patch_rel32_to_here(b);
        }
        self.emit_ref_putfield_helper_call(obj_slot, val_slot, field_index);

        self.patch_rel32_to_here(done);
    }

    pub(super) fn emit_inline_tlab_new(
        &mut self,
        class_id_raw: u32,
        num_fields: usize,
        // CRIT-2 — when both `has_nonzero_tag_primitive_init` and
        // `has_finalizer` are statically known false at the call site, the post-init
        // helper has nothing meaningful to do beyond writing the
        // identity-hash and num_slots header words. We can emit those
        // inline and skip the helper call (which otherwise costs a
        // class_manager.read() and a finalizer-queue lock). When
        // unknown (the conservative default in `try_compile`), we
        // still issue the helper call.
        skip_post_init_helper: bool,
    ) {
        // A raw compiled bump updates `Tlab::cursor` without going through
        // the allocator's publication protocol. In concurrent Elasticsearch
        // merge churn that left a malformed young-space span before the next
        // collection could obtain an exact object map. Route through the
        // checked runtime helper until the raw JIT path can share the same
        // atomic publication contract as `Tlab::alloc_initialized`.
        //
        // The helper retains TLAB allocation (and its fast path); it merely
        // removes the unsynchronised machine-code cursor writer.
        // Allocation spill sink (`alloc_spill_sink_enabled`) — the `new` arm
        // asked `emit_pre_safepoint_spill` to withhold every blind-spill
        // register this fast path does not clobber, and that emitter agreed.
        // Two obligations follow, both discharged below: the preamble must stay
        // within `ALLOC_FAST_PATH_CLOBBERS`, and every slow-path edge must emit
        // the withheld stores before it can reach `new_object`.
        let sink_spill = self.deferred_alloc_blind_spill;
        if !inline_tlab_new_enabled() {
            // Unreachable under the sink (the request checks this gate), but the
            // withheld stores are this arm's to emit if it ever becomes so.
            self.emit_deferred_alloc_blind_spill();
            self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
            self.emit_mov_imm32_sx(ARG_REGS[1], class_id_raw as i32);
            self.emit_mov_imm32_sx(ARG_REGS[2], num_fields as i32);
            self.emit_call_absolute(self.helpers.new_object);
            return;
        }

        // Compact reference-field layout: when a per-class layout is registered
        // for exactly this field count, allocate the packed body size and mark
        // the object compact (array_length = body bytes, GC_FLAG_COMPACT) inline
        // — no helper call, no per-alloc layout lookup. `class_layout` here runs
        // once at JIT-compile time, not per allocation.
        // GROOVY-CLUSTER-20260717 → GUARDED RESTORE (perf/halfgap-20260717):
        // class_layout(class_id_raw) is snapshotted ONCE at JIT-compile time
        // and its body_size gets baked as immediate constants below
        // (bump-allocation size, array_length header write). When a class's
        // registered compact layout is later REPLACED (class_manager.rs's
        // recompute_subclass_layouts — the synthetic-stub→real-bytecode
        // upgrade, the exact shape of ANTLR/Groovy-generated parser
        // classes), an already-compiled site would keep allocating at the
        // OLD size while field access (correctly, per-object) uses the
        // CURRENT layout — confirmed heap corruption; the interim fix
        // disabled this path entirely (compact_body = None).
        //
        // The restore bakes the ADDRESS + compile-time VALUE of the class's
        // layout-REPLACE counter (`types::field_layout::layout_replace_guard`
        // — fixed-capacity table, addresses stable for process life; new
        // class REGISTRATIONS don't bump it, only replacements do) and
        // emits a 3-instruction guard at the top of the inline path:
        //     mov r11, imm64(count_addr)
        //     mov eax, [r11]
        //     cmp eax, imm32(count_at_compile_time)  ;  jne slow_path
        // A replaced layout therefore permanently routes this site to the
        // always-correct `new_object` helper — for classes that never get
        // replaced (every benchmark and the overwhelming majority of real
        // classes), the full compact inline path is back.
        let compact_snapshot: Option<(usize, *const u32, u32)> =
            if cratonvm_types::compact_ref_fields_enabled() {
                cratonvm_types::class_layout(class_id_raw)
                    .filter(|l| l.field_count() == num_fields)
                    .map(|l| {
                        let (addr, expected) = cratonvm_types::layout_replace_guard(class_id_raw);
                        (l.body_size as usize, addr, expected)
                    })
            } else {
                None
            };
        let compact_body: Option<usize> = compact_snapshot.map(|(body, _, _)| body);
        // The inline allocator's OWN legacy-fallback census.
        //
        // `plan_object_alloc` prints `[compact-legacy]` when the helper path
        // cannot find a matching layout; this path never reaches it, so an
        // inline `new` that falls back to the uniform 16-byte-cell layout was
        // INVISIBLE to that census — a class could allocate legacy on the
        // hottest path in the program and still be absent from the only report
        // that names legacy allocations. That blind spot is how
        // `SHA256Digest` came to be 100% of the `jit_getfield` helper's
        // receivers on Generational while appearing in no legacy census at all.
        if compact_body.is_none() && cratonvm_types::flags().gc.dbg_compact_legacy {
            eprintln!(
                "[compact-legacy] JIT inline-new class_id={class_id_raw} num_fields={num_fields}                  registered_field_count={:?} compact_enabled={} -> LEGACY object",
                cratonvm_types::class_layout(class_id_raw).map(|l| l.field_count()),
                cratonvm_types::compact_ref_fields_enabled(),
            );
        }
        // Object total size (header + body). Computed at compile time.
        let total_size = HEADER_SIZE + compact_body.unwrap_or(num_fields * SLOT_SIZE);
        // Cast: value to i32 (encoding immediate/displacement)
        let cursor_off = self.helpers.tlab_cursor_offset_in_thread as i32;
        // Cast: value to i32 (encoding immediate/displacement)
        let end_off = self.helpers.tlab_end_offset_in_thread as i32;
        // Cast: value to i32 (encoding immediate/displacement)
        let class_id_off = self.helpers.class_id_offset_in_obj as i32;

        // Step 0: layout-replace guard (see the GUARDED RESTORE note above).
        // Runs before anything else so a stale-layout site diverts to the
        // helper with zero state to unwind. R11/RAX are scratch here.
        let layout_guard_patch = compact_snapshot.map(|(_, count_addr, expected)| {
            self.emit_mov_imm64_full(R11, count_addr as i64);
            self.emit_mov_r32_mem_disp32(RAX, R11, 0);
            // CMP EAX, imm32 (EAX-only short form 0x3D).
            self.buf.emit_byte(0x3D);
            self.buf.emit(&(expected as i32).to_le_bytes());
            self.emit_jcc_rel32_patch(0x85) // JNE slow_path
        });

        // Step 1: fetch the JvmThread*. Allocation-heavy methods cache it in
        // the prologue/OSR trampoline; otherwise use the small TLS helper.
        //
        // HIGH-2 / Fix 2 — direct `MOV reg, FS:[off]` TLS load is the
        // ideal sequence (saves ~5 ns per `new`). It is NOT applied
        // here in this round because it requires runtime cooperation
        // we do not yet have:
        //
        //   * Rust's `thread_local!` macro hides the TLS slot offset
        //     entirely — there is no portable API to extract the
        //     FS/GS-relative offset of `JIT_THREAD` at JIT-compile
        //     time. A `#[thread_local]` static (unstable on stable
        //     Rust) would still need a startup probe (inline asm
        //     `mov rax, fs:[OFFSET]` against a known sentinel) to
        //     recover the loader-assigned displacement.
        //   * On Windows the slot lives at GS:[0x58 + slot*8] where
        //     `slot` is allocated dynamically by `TlsAlloc`; the same
        //     probe machinery applies but with a different segment
        //     prefix and one extra indirection. Per task scope, this
        //     arm is intentionally left on the helper.
        //   * The current `JitRuntimeHelpers` table exposes only the
        //     helper function pointer; wiring an `Option<(SegPrefix,
        //     u32)>` field plus a startup probe in the VM is a
        //     cross-crate change outside the scope of this fix
        //     round.
        //
        // Until that plumbing lands, the helper call stays — see the
        // task notes for the planned approach.
        // Common case: one prologue/OSR helper call per invocation, not per `new`.
        if sink_spill {
            // Sink form: the cached slot is the ONLY thread source here. The
            // fallback below calls `get_current_thread`, and a CALL clobbers the
            // whole caller-saved file — which would invalidate the eleven
            // registers this site is about to spill at the slow-path label
            // instead of here. A null cached slot therefore diverts to
            // `new_object`, which resolves its own thread and allocates
            // correctly; `emit_prologue` writes this slot on every entry
            // (inherited on a proven self-call, fetched otherwise), so the
            // divert only happens for a genuinely non-Java thread, where the
            // fallback would have returned null and diverted anyway.
            self.emit_load_local(RAX, self.jit_thread_slot_off);
        } else if self.jit_thread_slot_off != 0 {
            self.emit_load_local(RAX, self.jit_thread_slot_off);
            self.emit_test_r64_r64(RAX);
            let have_cached_thread = self.emit_jcc_rel32_patch(0x85); // JNE have_thread
            self.emit_fetch_current_thread_into_rax();
            self.emit_store_local(self.jit_thread_slot_off, RAX);
            self.patch_rel32_to_here(have_cached_thread);
        } else {
            self.emit_fetch_current_thread_into_rax();
        }
        self.emit_test_r64_r64(RAX);
        let null_thread_patch = self.emit_jcc_rel32_patch(0x84); // JE slow_path

        // R10 = thread; R11 = cursor.
        self.emit_mov_r64_r64(R10, RAX);
        self.emit_mov_r64_mem_disp32(R11, R10, cursor_off);

        // Align cursor up to 8 bytes (matches `Tlab::alloc(_, 8)`'s
        // behaviour). Without this, an interleaved array allocation that
        // left the cursor misaligned would force this `new` object onto a
        // non-8-aligned address — the GC walker assumes 8-aligned object
        // headers and would mis-decode the layout. Total cost: 2
        // instructions (8 bytes encoded) — negligible vs the cache miss
        // the slow path would incur.
        self.emit_add_r64_imm8(R11, 7);
        self.emit_and_r64_imm8(R11, -8);

        // RAX = R11 + total_size (new cursor).
        self.emit_lea_r64_mem_disp32(RAX, R11, total_size as i32); // Cast: x86-64 immediate encoding

        // CMP RAX, [R10 + end_off]; JA slow_path (TLAB exhausted).
        self.emit_cmp_r64_mem_disp32(RAX, R10, end_off);
        let tlab_full_patch = self.emit_jcc_rel32_patch(0x87); // JA slow_path

        // JVM default initialization and TLAB-reuse safety. All refill
        // backends return zeroed TLAB ranges, including cells reused by a
        // non-moving sweep, so the default path does not repeat those stores
        // per object. The opt-out retains the older defensive clear. Both
        // layouts are qword-sized here (legacy fields are 16 bytes; compact
        // fields are 8 or 16 bytes).
        //
        // This also makes the all-zero-tag primitive family (int, boolean,
        // byte, char, short) fully initialized inline as `Value::Int(0)`.
        // Only long/float/double need the post-init helper to install a non-zero
        // Value discriminant; reference fields in the compact layout are null
        // bare pointers after this clear.
        let zero_elision = inline_tlab_zero_elision_enabled();
        if !zero_elision {
            debug_assert_eq!((total_size - HEADER_SIZE) % 8, 0);
            self.emit_mov_imm32_sx(RDX, 0);
            for body_off in (HEADER_SIZE..total_size).step_by(8) {
                self.emit_mov_mem_disp32_r64(R11, RDX, body_off as i32);
            }
        }

        // BinTrees-18 heap-corruption fix (jit/gc audit, 2026-06):
        // *** Write the full object header BEFORE committing the TLAB
        // cursor. ***
        //
        // The previous order committed the bump (published the object's
        // address into `thread.tlab.cursor`) and only THEN wrote the
        // header fields. That left a window in which the object region was
        // already part of the "used" portion of the TLAB / young arena but
        // its header was still the TLAB-zeroed pattern (class_id=0,
        // kind=Object, num_slots=0). Any heap walk that observed the object
        // during that window — the non-moving young sweep that runs while
        // JIT frames are active (`gc_quiescence`), the Cheney to-space
        // scan, or a background-thread STW collection that parks this
        // mutator at a poll inside the in-between helper — computed
        // `size = HEADER_SIZE + 0*SLOT_SIZE = HEADER_SIZE` and stepped 40
        // bytes into the object's own field region. There it decoded the
        // first `Value` field cell (discriminant word = 4 = `Object`) as a
        // bogus header: `class_id=4`, `array_length=1` (upper half of the
        // 8-byte object-pointer payload), `num_slots=384` (the next cell's
        // discriminant region) — exactly the
        // "kind=Object but array_length=1 (num_slots=384, class_id=4)"
        // inconsistency reported by `gen_object_total_size`, after which
        // the walker desynced / looped (rc=124 timeout on `bintrees18`).
        //
        // Writing the header first means the object is fully walker-coherent
        // at the instant its address becomes reachable via the committed
        // cursor: the store to `cursor` below is the single linearization
        // point, and on x86-64 it is not reordered ahead of the header
        // stores (TSO: stores are not reordered with older stores). So no
        // walker can ever see a committed-but-unheadered object.
        //
        //   class_id  → identifies the object's class (offset 0)
        //   off 4     → kind=Object(0) / elem=Reference(0) / padding(0)
        //   off 12    → array_length=0 (Object kind never sets this)
        //   num_slots → walker's stride: size = HEADER_SIZE + n*SLOT_SIZE
        //
        // identity_hash_code (offset 8) stays 0 (TLAB-zeroed); the lazy-
        // mint contract in `System.identityHashCode()` handles it on
        // demand. The `jit_post_tlab_init` helper below still runs for the
        // primitive-init / finalizer paths, but the header is already
        // walker-coherent before the object is ever published.
        self.emit_mov_dword_mem_disp32_imm32(
            R11,
            class_id_off,
            class_id_raw as i32, // Cast: ClassId immediate fits in 32 bits
        );
        // Defensively zero offset 4 (kind=Object=0, elem=Reference=0, pad=0)
        // and offset 12 (array_length=0). The historical assumption "TLAB
        // refill zeroes the region" was empirically violated on long runs
        // (BinTrees-18, ECJ HashtableOfInt /by-zero #23): the GC-ARRAY-GUARD
        // observed `kind=Object && array_length=0x01010101` on freshly-
        // bumped slots. The defensive walker in `bcd70d0` catches that
        // pattern as corruption, but the right place to enforce the
        // invariant is at the *allocator* — write the four header bytes
        // (and the four array_length bytes) explicitly. Two extra dwords
        // per `new` is negligible vs. the safety guarantee.
        // `KIND_TAGS_BYTE_OFFSET` (4) names the dword that packs
        // kind/element_type/gc_age/gc_flags. It was a bare literal until the
        // 2026-07-26 header-offset audit — see
        // `arch-2026-07-26/x64-flag-skew-and-contracts.md` §5.
        let mut compact_flag_pending = false;
        // NO separate quartet store any more, and removing it is a fix rather
        // than a tidy-up. `kind` / `element_type` / `gc_age` / `gc_flags` used
        // to be four bytes at offset 4, zeroed with one dword store. They are
        // now bits 48..62 of the mark word, and the mechanical offset sweep
        // pointed that same DWORD store at byte 14 -- which spans 14..18 and
        // runs two bytes PAST a 16-byte header, into the object body.
        //
        // `header_offset_emission_site_inventory_matches_the_doc` and
        // `inline_tlab_header_writes_stay_inside_the_header` both caught it,
        // which is exactly what that contract module is for.
        //
        // The mark-word zeroing below subsumes it: a plain object wants
        // kind=Object(0) and element_type=Reference(0), so the quartet bits it
        // needs are all zero anyway.
        // offset 12: the full 32-bit field count for Object kind.
        let shape = num_fields as u32;
        self.emit_mov_dword_mem_disp32_imm32(
            R11,
            cratonvm_types::NUM_SLOTS_OFFSET as i32,
            shape as i32,
        );
        // Compact object: set GC_FLAG_COMPACT (bit 2) in the gc_flags byte.
        if let Some(body) = compact_body {
            if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_COMPACT_INLINE").is_some() {
                eprintln!(
                    "[compact-inline] new class_id={class_id_raw} body={body} total={total_size}"
                );
            }
            // A BYTE store, not a dword. `gc_flags` occupies bits 0..4 of the
            // mark word's byte 7, so the flag constants are still the right
            // mask -- but a dword store at that offset would write 15..19 and
            // run past a 16-byte header into the body.
            //
            // Writing the whole byte is safe only because `gc_age` shares it
            // and is 0 at allocation. Deferred until after the mark-word
            // zeroing below, which would otherwise erase it.
            compact_flag_pending = true;
        }
        // UNCONDITIONAL, and `zero_elision` must never gate it again.
        //
        // This was `if !zero_elision` for one release and it broke the Spring
        // Boot suite outright: 178 of the first 184 classes died in JUnit
        // discovery with `gen_heap::read_slot: corrupt Value cell`.
        //
        // The reason is that the mark word stopped being defensive padding and
        // became CONTENT. It carries `kind`, `element_type`, `gc_age` and
        // `gc_flags` in bits 48..62 as of the 24 -> 16 shrink. Skipping the
        // write leaves an object wearing whatever the TLAB slot happened to
        // hold: a stale `GC_FLAG_COMPACT` makes every accessor read a legacy
        // tagged-`Value` object as bare compact pointers -- which is exactly
        // what that diagnostic reports -- and a stale `kind` turns an object
        // into an array or a region sentinel mid-walk.
        //
        // `zero_elision` is default-ON (opt-out only), so this was not a corner
        // case; it was every JIT-inline allocation in the process.
        //
        // The dword store this replaced -- `kind`/`element_type`/`gc_age`/
        // `gc_flags` at offset 4 -- was itself unconditional, and for exactly
        // this reason. The comment above it already said why: "the historical
        // assumption 'TLAB refill zeroes the region' was empirically violated
        // on long runs". Moving those four bytes into the mark word did not
        // move that argument with them; folding the write into the elision
        // branch silently dropped it. Two dwords per `new` is the same price
        // that comment already judged negligible.
        self.emit_mov_dword_mem_disp32_imm32(R11, cratonvm_types::MARK_WORD_OFFSET as i32, 0);
        self.emit_mov_dword_mem_disp32_imm32(R11, cratonvm_types::MARK_WORD_OFFSET as i32 + 4, 0);

        // AFTER the mark-word zeroing, which would otherwise erase it.
        if compact_flag_pending {
            self.emit_mov_byte_mem_disp32_imm8(
                R11,
                cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32,
                cratonvm_types::GC_FLAG_COMPACT,
            );
        }

        // Commit the bump LAST: [R10 + cursor_off] = RAX. This publishes the
        // object's end as the new cursor (and, transitively, the object's
        // address as a live allocation). x86-64 TSO preserves the required
        // header/body-before-cursor store order; the STW handshake provides
        // the acquire side. Do not add an SFENCE here: it is unnecessary on
        // this backend and would tax every fast-path allocation.
        self.emit_mov_mem_disp32_r64(R10, RAX, cursor_off);

        if skip_post_init_helper {
            // CRIT-2 fast path — no primitive defaults to apply and no
            // finalizer to register. With class_id + num_slots already
            // written inline above, the header is complete enough for
            // both the GC walker and the runtime; no helper call needed.
            //
            // Class, kind/flags, and shape are explicitly published above.
            // Body defaults, forwarding_ptr=null, and
            // mark_word=MARK_NEUTRAL come from the refill zeroing invariant
            // unless the conservative opt-out repeats those stores inline.
            //
            // RAX = obj_ptr — both arms converge with RAX holding the
            // freshly-allocated object pointer.
            self.emit_mov_r64_r64(RAX, R11);
        } else {
            // Hand off to post-init: tlab_post_init(vm_ptr, obj_ptr, cid, nf).
            // The helper now only does the cold work (identity-hash mint,
            // primitive-typed default values, finalizer registration); the
            // walker-coherent header bits (class_id + num_slots) are
            // already in place from the inline writes above.
            self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
            self.emit_mov_r64_r64(ARG_REGS[1], R11);
            self.emit_mov_imm32_sx(ARG_REGS[2], class_id_raw as i32); // Cast: ClassId fits in 32 bits
            self.emit_mov_imm32_sx(ARG_REGS[3], num_fields as i32); // Cast: x86-64 immediate encoding
            self.emit_call_absolute(self.helpers.tlab_post_init);
        }

        // Jump over the slow path; both arms converge with RAX = obj_ptr.
        let done_patch = self.emit_jmp_rel32_patch();

        // ----- slow_path -----
        self.patch_rel32_to_here(null_thread_patch);
        self.patch_rel32_to_here(tlab_full_patch);
        if let Some(patch) = layout_guard_patch {
            // Layout-replace guard mismatch: the baked compact size is stale;
            // the helper allocates per the CURRENT layout.
            self.patch_rel32_to_here(patch);
        }
        // Every edge that reaches `new_object` — and therefore a collection —
        // converges here, so this is the one place the withheld half of the
        // safepoint's blind spill has to be. Emitted before the argument setup
        // below for the same reason the deopt stub spills before its own: the
        // ARG_REGS are part of the spilled file.
        self.emit_deferred_alloc_blind_spill();
        self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
        self.emit_mov_imm32_sx(ARG_REGS[1], class_id_raw as i32); // Cast: ClassId fits in 32 bits
        self.emit_mov_imm32_sx(ARG_REGS[2], num_fields as i32); // Cast: x86-64 immediate encoding
        self.emit_call_absolute(self.helpers.new_object);

        // ----- done -----
        self.patch_rel32_to_here(done_patch);
    }

    // -----------------------------------------------------------------------
    // Array access, bounds checks and null checks
    // -----------------------------------------------------------------------
    //
    // Moved to `x64/arrays.rs`.

    /// Emit an `ldc <Class>` site: call `helpers.ldc_class_cp` and push the
    /// returned mirror as an oop. Returns `false` when this pc is not a
    /// class-`ldc` **or** the site cannot be served (the helper is unwired, or
    /// this artifact has no VM context to pass it), leaving the caller to fall
    /// through to the immediate/string arms or refuse the method.
    ///
    /// The mirror is re-fetched on every execution rather than baked, exactly
    /// as `helpers.ldc_string` re-interns its String: both are heap objects a
    /// relocating collector may move between two runs of this body.
    /// `ldc <String>` — the interned literal named at `cp_idx` in
    /// `holder_class_id`'s constant pool, materialised by
    /// `helpers.ldc_string_cp`.
    ///
    /// The exact twin of [`Self::emit_ldc_class`] below, down to the stub: the
    /// two helpers take the same `(vm_ptr, holder_class_id, cp_idx)` shape and
    /// share the same `0 = pending exception` convention, so they share the
    /// emitter. That is what keeps the two sites' ABIs from drifting apart.
    ///
    /// Returns `false` when the site is not a string `ldc`, when the helper is
    /// unwired (a hand-built test table) or when the artifact has no context
    /// slot — and then the caller bails the site exactly as it did before this
    /// helper existed.
    pub(super) fn emit_ldc_string(&mut self, pc: usize) -> bool {
        let Some(&idx) = self.ldc_string_info_idx.get(&pc) else {
            return false;
        };
        if self.helpers.ldc_string_cp == 0 || !self.needs_heap {
            return false;
        }
        let (_, holder_class_id, cp_idx) = self.ldc_string_info[idx];
        self.emit_pre_safepoint_spill();
        crate::runtime_lowering::emit_ldc_class_cp_stub(
            &mut self.buf,
            self.heap_local_offset,
            self.helpers.ldc_string_cp,
            holder_class_id,
            cp_idx,
            self.helpers.frame_record,
        );
        // Interning allocates, so this is a real safepoint. A `0` return is a
        // published pending exception (the constant-pool entry could not be
        // re-read), not a value — the same convention `emit_ldc_class` takes.
        self.emit_oop_map_for_safepoint();
        self.emit_post_alloc_oom_check();
        self.push_from_rax();
        self.mark_top_as_oop();
        true
    }

    pub(super) fn emit_ldc_class(&mut self, pc: usize) -> bool {
        let Some(&idx) = self.ldc_class_info_idx.get(&pc) else {
            return false;
        };
        if self.helpers.ldc_class_cp == 0 || !self.needs_heap {
            return false;
        }
        let (_, holder_class_id, cp_idx) = self.ldc_class_info[idx];
        self.emit_pre_safepoint_spill();
        crate::runtime_lowering::emit_ldc_class_cp_stub(
            &mut self.buf,
            self.heap_local_offset,
            self.helpers.ldc_class_cp,
            holder_class_id,
            cp_idx,
            self.helpers.frame_record,
        );
        // Resolution can load a class — arbitrary Java, hence a GC point — so
        // this is a real safepoint, and its `0` return is a published pending
        // exception (`NoClassDefFoundError` and friends), not a value.
        self.emit_oop_map_for_safepoint();
        self.emit_post_alloc_oom_check();
        self.push_from_rax();
        self.mark_top_as_oop();
        true
    }

    /// The three published reference-store gate addresses, or `None` when this
    /// process's collector did not publish a plan.
    ///
    /// All three or none: a plan with a live pre-gate and a zero post-gate
    /// would let compiled code skip the post barrier on the strength of a word
    /// nobody maintains, so the tuple is destructured as a unit and a single
    /// zero declines the whole fast path.
    pub(super) fn ref_store_gates(&self) -> Option<(usize, usize, usize)> {
        ref_store_gates_of(&self.helpers)
    }

    /// The published post-barrier skip mask, when the plan uses that shape.
    pub(super) fn ref_store_post_skip_mask(&self) -> Option<u8> {
        ref_store_post_skip_mask_of(&self.helpers)
    }

    /// `LOCK INC qword [counter]` when the single-pass reference-store path
    /// trace is on, and nothing at all otherwise.
    ///
    /// Uses R11, which is scratch at every call site here, and clobbers flags
    /// — so it is only ever emitted where the next instruction sets them again
    /// or does not read them.
    fn emit_ref_store_path_trace(&mut self, counter: &'static std::sync::atomic::AtomicU64) {
        if !crate::x64::sp_ref_store_trace_enabled() {
            return;
        }
        self.emit_mov_imm64_full(R11, counter as *const _ as i64);
        self.buf.emit_byte(0xF0); // LOCK
        self.buf.emit_byte(0x49); // REX.W + REX.B
        self.buf.emit_byte(0xFF); // INC r/m64 (/0)
        self.buf.emit_byte(0x03); // ModRM mod=00 reg=000 rm=011 (R11)
    }

    /// Emit a compact reference `putfield` whose barriers are **gated inline**
    /// rather than paid as a call.
    ///
    /// Returns `false` without emitting anything when the shape is not
    /// admitted, in which case the caller keeps whichever arm it has today.
    ///
    /// # What makes this sound
    ///
    /// Every gate below names a PREFIX of the barrier helper's own control
    /// flow, read from the word the helper itself reads:
    ///
    /// | inline test | the helper's own first act |
    /// |---|---|
    /// | `pre_active == 0` | `satb_pre_barrier` loads `mark_active` and returns |
    /// | `flags_byte < young_floor` | `note_ref_store_slow` compares `gc_age` to the promotion age and returns |
    /// | `post_active == 0` | `note_ref_store` loads `has_old_objects` and returns |
    ///
    /// So a skipped call is a call that would have returned having done
    /// nothing. On any other answer this path calls the collector's OWN
    /// `write_barrier`, which is the same code that records the edge today —
    /// no remembered-set contract is reimplemented here, which is the mistake
    /// the previous inline store path made and what
    /// `inline_card_mark_available` was hard-`false`d to stop.
    ///
    /// The gates are published conservatively: each may read "there may be
    /// work" while the truth is "no work" (a call that was not needed), and
    /// never the reverse. See `gc::gen_heap::JitRefStoreGates`.
    ///
    /// # What this path does NOT have to prove, and why that is the win
    ///
    /// The arm it replaces required the field's **old value to be null**, so
    /// every re-assignment of an already-set reference took the helper. That
    /// condition existed to make the SATB pre-barrier unnecessary; with
    /// `pre_active` read directly, the old value stops mattering and the
    /// ordinary `node.next = other` store stays inline.
    ///
    /// It also drops the published-region containment test, which under the
    /// default collector could never pass — the emitter laid down six compares
    /// against an all-zero table and then called the helper anyway. Receiver
    /// validity is still established, by the READ-side bounds table for an
    /// unproven receiver and by a null test for a type-tracker-proven oop.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn emit_gated_compact_ref_putfield(
        &mut self,
        obj_slot: StackSlot,
        val_slot: StackSlot,
        field_index: usize,
        cell_off: i32,
        receiver_is_trusted_oop: bool,
    ) -> bool {
        let Some((pre, post, floor)) = self.ref_store_gates() else {
            return false;
        };
        // The value has to survive to the store and, on the barriered path, to
        // the helper call — both of which read it out of its frame slot, so no
        // register constraint travels across the guards.
        // The bail sets are kept APART rather than in one vector so the
        // run-time trace can say which gate refused — see `SP_REF_STORE_BAIL`.
        // Without the trace they are concatenated and all land on the same
        // helper label, which is exactly what they did before this split.
        let mut bail_recv: Vec<usize> = Vec::new();
        let mut bail_pre: Vec<usize> = Vec::new();
        self.load_slot_to_reg(RAX, obj_slot);

        // ── receiver validity ───────────────────────────────────────────
        //
        // The READ table (`read_bounds_addr`), not the store table. The
        // question here is only "is this address one this heap handed out, so
        // the header reads below cannot fault" — the barrier question is the
        // gates' job now, and conflating the two is what left this arm dead
        // under every non-publishing collector.
        bail_recv.extend(if receiver_is_trusted_oop {
            self.emit_trusted_oop_receiver_check()
        } else {
            self.emit_guarded_getfield_receiver_check(self.helpers.read_bounds_addr)
        });

        // ── SATB pre-barrier gate ───────────────────────────────────────
        // Marking armed ⇒ the overwritten reference has to reach the snapshot,
        // which is the helper's job. Rare: armed only during a concurrent
        // mark phase.
        self.emit_mov_imm64_full(R11, pre as i64);
        self.emit_cmp_mem8_imm8(R11, 0, 0);
        bail_pre.push(self.emit_jcc_rel32_patch(0x85)); // JNE → helper

        // ── the receiver flags byte, read ONCE ──────────────────────────
        // `GC_FLAGS_BYTE_OFFSET` carries the flags in bits 0..3 and `gc_age` in
        // bits 4..7, so this single byte answers both the young-receiver
        // question the post gate asks and the per-object layout question the
        // store shape below asks. Read as a byte rather than as the dword the
        // older arms use: that dword starts 15 bytes into a 16-byte header and
        // takes three of its four bytes from the first instance field.
        self.emit_movzx_r32_mem8(RCX, RAX, cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32);

        // ── slot bounds ─────────────────────────────────────────────────
        // `field_index < num_slots`. A failure DROPS the store, matching
        // `jit_putfield_object`'s own out-of-bounds behaviour, so it targets
        // its own label rather than the helper.
        self.emit_mov_r32_mem_disp32(R11, RAX, cratonvm_types::NUM_SLOTS_OFFSET as i32);
        self.emit_mov_imm64(R10, field_index as i64);
        self.emit_cmp_r32_r32(R10, R11);
        let oob = self.emit_jcc_rel32_patch(0x83); // JAE → drop

        // ── the store, in whichever shape this OBJECT has ───────────────
        //
        // Both layouts store inline, and the reason is measured rather than
        // assumed. This arm used to send a non-compact receiver to the helper,
        // which made it an arm that essentially never fired:
        // `init_object_header` — the TLAB fast path serving nearly every
        // allocation for the interpreter and `jit_new_object` alike — writes a
        // LEGACY header unconditionally (`array_length = 0`, no
        // `GC_FLAG_COMPACT`) whatever layout the class has registered, because
        // it never consults `plan_object_alloc`. A run-time path census on
        // `RefStoreLoopProbe` put a number on it: `inline=0` out of
        // **16,380,000**, every one bailing at the compactness test, while the
        // compile-time census reported `gated=2 declined=0` and looked healthy.
        //
        // It is the same trap the inline `getfield` read fell into and climbed
        // out of on 2026-08-18, and the optimizing tier's twin of this arm on
        // 2026-09-02. The fix is theirs: emit both shapes and pick per OBJECT
        // on the header bit, exactly as `jit_putfield_object` does.
        self.load_slot_to_reg(RDX, val_slot);
        self.emit_test_r8_imm8(RCX, cratonvm_types::GC_FLAG_COMPACT);
        let legacy_shape = self.emit_jcc_rel32_patch(0x84); // JZ → the 16-byte cell
        // COMPACT: a reference field is the bare 8-byte pointer at the cell
        // base, which is what the guarded inline `getfield` reads back.
        self.emit_mov_mem_disp32_r64(RAX, RDX, cell_off);
        let shaped = self.emit_jmp_rel32_patch();
        // LEGACY: the uniform 16-byte `Value` cell — tag qword (the dword tag
        // plus its pad) then the pointer payload. `field_index < num_slots` was
        // checked above, and for a legacy object `num_slots` counts exactly
        // these cells, which is what makes this stride addressable.
        self.patch_rel32_to_here(legacy_shape);
        let legacy_off = (HEADER_SIZE + field_index * SLOT_SIZE) as i32; // Cast: disp32
        self.emit_mov_imm64(R10, i64::from(cratonvm_types::FIELD_CELL_TAG_OBJECT));
        self.emit_mov_mem_disp32_r64(
            RAX,
            R10,
            legacy_off + cratonvm_types::FIELD_CELL_TAG_OFFSET as i32,
        );
        self.emit_mov_mem_disp32_r64(
            RAX,
            RDX,
            legacy_off + cratonvm_types::FIELD_CELL_PAYLOAD64_OFFSET as i32,
        );
        self.patch_rel32_to_here(shaped);
        // Counted here rather than at the top: everything above can still
        // leave for the helper, and "reached the store" is the fact the
        // compile-time census cannot supply.
        self.emit_ref_store_path_trace(&crate::metrics::SP_REF_STORE_INLINE_TAKEN);

        // ── post-barrier gates ──────────────────────────────────────────
        // CL still holds the receiver's flags byte, which carries `gc_age` in
        // bits 4..7 and the GC flags in bits 0..3. Two shapes can rule the post
        // barrier out, and a publisher supplies exactly one of them
        // (`ref_store_gates` enforces that).
        let mut done: Vec<usize> = Vec::new();
        if let Some(mask) = self.ref_store_post_skip_mask() {
            // MASK — "the receiver carries none of the bits that could make a
            // post barrier necessary". The generational collector's shape:
            // `GC_FLAG_OLD_GEN` clear means the receiver is young, and a young
            // receiver needs no card. One `test r8, imm8` with no memory
            // operand, because the mask is a property of which collector is
            // running and that cannot change after start-up.
            self.emit_test_r8_imm8(RCX, mask);
            done.push(self.emit_jcc_rel32_patch(0x84)); // JZ → young receiver, no card
        } else {
            // FLOOR — `age << 4 | flags` compared unsigned against
            // `promotion_floor << 4` is an EXACT test of
            // `gc_age < promotion_floor`, because the flags nibble is at most
            // 15 and cannot carry `a << 4` up to `(a + 1) << 4`.
            self.emit_mov_imm64_full(R11, floor as i64);
            self.emit_cmp_r8_mem8(RCX, R11, 0);
            done.push(self.emit_jcc_rel32_patch(0x82)); // JB → young receiver, no card
        }

        self.emit_mov_imm64_full(R11, post as i64);
        self.emit_cmp_mem8_imm8(R11, 0, 0);
        done.push(self.emit_jcc_rel32_patch(0x84)); // JZ → no old objects, no card

        // Neither gate could rule the barrier out: run the collector's own.
        self.emit_ref_store_path_trace(&crate::metrics::SP_REF_STORE_BARRIER_TAKEN);
        self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
        self.load_slot_to_reg(ARG_REGS[1], obj_slot);
        self.load_slot_to_reg(ARG_REGS[2], val_slot);
        self.emit_call_absolute(self.helpers.write_barrier);
        done.push(self.emit_jmp_rel32_patch());

        // ── helper fallback: the full SATB + post barrier + store ───────
        //
        // With the trace on, each bail set gets a one-instruction stub naming
        // it before joining the helper; without it they all land here directly
        // and cost nothing.
        let bail_groups = [(bail_recv, 0usize), (bail_pre, 1)];
        let mut to_helper: Vec<usize> = Vec::new();
        if crate::x64::sp_ref_store_trace_enabled() {
            for (patches, reason) in bail_groups {
                if patches.is_empty() {
                    continue;
                }
                for b in patches {
                    self.patch_rel32_to_here(b);
                }
                self.emit_ref_store_path_trace(&crate::metrics::SP_REF_STORE_BAIL[reason]);
                to_helper.push(self.emit_jmp_rel32_patch());
            }
        } else {
            for (patches, _) in bail_groups {
                to_helper.extend(patches);
            }
        }
        for b in to_helper {
            self.patch_rel32_to_here(b);
        }
        self.emit_ref_store_path_trace(&crate::metrics::SP_REF_STORE_HELPER_TAKEN);
        self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
        self.load_slot_to_reg(ARG_REGS[1], obj_slot);
        self.emit_mov_imm32_sx(ARG_REGS[2], field_index as i32); // Cast: x86-64 imm32
        self.load_slot_to_reg(ARG_REGS[3], val_slot);
        self.emit_call_absolute(self.helpers.putfield_object);

        self.patch_rel32_to_here(oob);
        for d in done {
            self.patch_rel32_to_here(d);
        }
        note_gated_ref_store();
        true
    }
}
