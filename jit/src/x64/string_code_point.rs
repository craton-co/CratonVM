// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The single-pass code-point call-site intrinsics (JIT r23 wave 1, lane L5,
//! INTRINSICS22-1 in `docs/feature-designs/jit-ir-optimizations.md`):
//! `String.codePointAt(int)` ([`crate::JitIntrinsic::StringCodePointAt`],
//! `CRATONVM_JIT_SP_STRING_CODE_POINT_AT`) and
//! `StringBuilder.appendCodePoint(int)`
//! ([`crate::JitIntrinsic::StringBuilderAppendCodePoint`],
//! `CRATONVM_JIT_SP_SB_APPEND_CODE_POINT`).
//!
//! A method the single-pass backend keeps -- a STRINGBUILDER_ACCESS site other
//! than `length()` pins it while `CRATONVM_JIT_IR_SB_PREFIX` is opt-in -- paid
//! the native funnel for `codePointAt` (a registered `Intrinsic`, ~200 ns) and
//! the full JDK chain for `appendCodePoint` (`AbstractStringBuilder
//! .appendCodePoint` -> a virtual `append(char)` / `toChars` + `append(char[])`).
//! These bodies are the optimizing tier's `CodePointAt` / `SbAppendCodePoint`
//! prefixes (`ir_lower.rs`), instruction for instruction where the register
//! plan allows, so the two tiers answer the same inputs the same way.
//!
//! # The slow edge is a CALL
//!
//! As for the STRINGBUILDER_ACCESS family (`op_invoke.rs`): every input the
//! body does not serve -- a null receiver or `value`, an index outside
//! `[0, length)`, a full or LATIN1-for-a-wide-code-point builder, an invalid
//! code point -- declines into the site's ordinary `invoke_dispatch`, which
//! runs the registered native or the JDK body and so throws exactly what the
//! call throws (`StringIndexOutOfBoundsException: Index 5 out of bounds for
//! length 3`, `IllegalArgumentException: Not a valid Unicode code point:
//! 0x110000`, the JEP 358 NPE). No uncommon trap: an index at the end of a
//! string is what every code-point loop reaches once. Both entries are listed
//! in `string_intrinsic_declines_to_a_call`, so both compile doors mint the
//! site's `JitInvokeInfo`.
//!
//! # Why no barrier and no GC point on the fast path
//!
//! `codePointAt` only reads. `appendCodePoint` stores into a `byte[]` and an
//! `int` field -- no reference -- and allocates nothing (a growth declines), so
//! nothing on the inline path can collect or move the receiver.

use super::concat_emit::{jcc_patch, jmp_patch, patch_here};
use super::*;
use crate::ExecutableBuffer;

impl Compiler {
    /// The code-point intrinsic at `pc`, if `callee_entry` names one.
    ///
    /// `None`: not a code-point entry, nothing emitted. `Some(Ok(()))`: the
    /// site is fully emitted (operands popped, result pushed). `Some(Err(site))`:
    /// registration and emission disagree (no `JitInvokeInfo`, no layout); the
    /// caller must fail the compile with `site` rather than fall through, since
    /// the direct-call path would `CALL` the sentinel.
    pub(super) fn emit_sp_code_point_intrinsic(
        &mut self,
        pc: usize,
        callee_entry: usize,
    ) -> Option<Result<(), &'static str>> {
        use crate::JitIntrinsic as I;
        let site = if callee_entry == I::StringCodePointAt.as_entry() {
            CpSite::StrAt
        } else if callee_entry == I::StringCodePointBefore.as_entry() {
            CpSite::StrBefore
        } else if callee_entry == I::StringCodePointCount.as_entry() {
            CpSite::StrCount
        } else if callee_entry == I::StringBuilderAppendCodePoint.as_entry() {
            CpSite::SbAppend
        } else if callee_entry == I::StringBuilderCodePointAt.as_entry() {
            CpSite::SbAt
        } else {
            return None;
        };
        let info = match self
            .invoke_info_idx
            .get(&pc)
            .map(|&i| self.invoke_info[i].1)
        {
            Some(info) if !info.is_null() => info,
            _ => return Some(Err("singlepass-codegen/code-point-no-invoke-info")),
        };
        let Some(layout) = self.string_layout else {
            return Some(Err("singlepass-codegen/code-point-no-layout"));
        };
        let builder = if matches!(site, CpSite::SbAppend | CpSite::SbAt) {
            match layout.builder {
                // The narrow-`value` refusal lives in
                // `StringBuilderFieldLayout::new`; a layout that reached here
                // has a full-width `value`, and the body reads it as one.
                Some(b) if !b.value_compact_is_narrow => Some(b),
                _ => return Some(Err("singlepass-codegen/sb-code-point-no-layout")),
            }
        } else {
            if !layout.has_coder {
                return Some(Err("singlepass-codegen/code-point-no-coder"));
            }
            None
        };
        let returns_builder = site == CpSite::SbAppend;

        self.flush_scratch_registers();
        // Operands, deepest first: the receiver, then the int argument(s).
        let arg2_slot = if site == CpSite::StrCount {
            Some(self.pop_stack())
        } else {
            None
        };
        let arg_slot = self.pop_stack();
        let recv_slot = self.pop_stack();
        let mut decline: Vec<usize> = Vec::new();

        // The String / builder offsets are a compile-time claim about two class
        // layouts; a layout replacement bumps the epoch and the site takes the
        // call. First, before any operand load: the guard clobbers RCX and R11.
        if jit_sp_field_layout_guard_enabled() {
            decline.extend(self.emit_layout_epoch_guard());
        }

        // RAX = receiver; null takes the call (its NPE).
        self.load_slot_to_reg(RAX, recv_slot);
        self.emit_test_r64_r64(RAX);
        decline.push(self.emit_jcc_rel32_patch(0x84)); // JZ

        match (site, builder) {
            (CpSite::StrAt, _) => {
                self.emit_string_code_point_at_body(&layout, arg_slot, &mut decline)
            }
            (CpSite::StrBefore | CpSite::StrCount, _) => {
                self.emit_string_code_point_span_body(&layout, arg_slot, arg2_slot, &mut decline)
            }
            (CpSite::SbAt, Some(b)) => self.emit_sb_code_point_at_body(&b, arg_slot, &mut decline),
            (CpSite::SbAppend, Some(b)) => {
                self.emit_sb_append_code_point_site(&b, recv_slot, arg_slot, &mut decline)
            }
            (CpSite::SbAt | CpSite::SbAppend, None) => {
                return Some(Err("singlepass-codegen/sb-code-point-no-layout"));
            }
        }
        let done = self.emit_jmp_rel32_patch();

        // ---- decline edge: the unchanged dispatch -----------------------
        for p in decline {
            self.patch_rel32_to_here(p);
        }
        let nargs: usize = if arg2_slot.is_some() { 3 } else { 2 };
        let Some(args_base) = self.reserve_spill_slots(nargs, SpillReason::HelperArgs) else {
            return Some(Err("singlepass-codegen/code-point-args-spill-exhausted"));
        };
        // `jit_invoke_dispatch`'s buffer runs arg[0] at the HIGHEST offset
        // down. Every operand is loaded before any is stored: the buffer can
        // overlap the operand homes.
        self.load_slot_to_reg(RAX, recv_slot);
        self.load_slot_to_reg(RCX, arg_slot);
        if let Some(a2) = arg2_slot {
            self.load_slot_to_reg(RDX, a2);
        }
        let top = args_base + (nargs as i32 - 1) * 8; // Cast: 2 or 3
        self.emit_store_local(top, RAX);
        self.emit_store_local(top - 8, RCX);
        if arg2_slot.is_some() {
            self.emit_store_local(top - 16, RDX);
        }
        self.emit_load_vm_context(ARG_REGS[0]);
        self.emit_mov_imm64(ARG_REGS[1], info as *const _ as i64);
        self.emit_lea_frame_slot(ARG_REGS[2], top);
        self.emit_mov_imm32_sx(ARG_REGS[3], nargs as i32); // Cast: 2 or 3
        self.emit_pre_safepoint_spill_before_helper_call();
        // (vm_ptr, info_ptr, args_ptr, num_args) -> result | sentinel.
        cratonvm_jit_api::assert_helper_call_shape!(
            "invoke_dispatch",
            int_args = 4,
            returns_value = true
        );
        self.emit_call_absolute(self.helpers.invoke_dispatch);
        self.emit_oop_map_for_safepoint();
        // The code-point reads return an int (never the `i64::MIN`
        // pending-exception sentinel), `appendCodePoint` the builder.
        self.emit_post_invoke_exception_check(if returns_builder { b'L' } else { b'I' });

        // ---- join ----------------------------------------------------------
        self.patch_rel32_to_here(done);
        self.next_spill_offset = args_base;
        self.push_from_rax();
        if returns_builder {
            self.mark_top_as_oop();
        }
        Some(Ok(()))
    }

    /// `StringBuilder.appendCodePoint(int)`: the class guard, then one arm per
    /// instance layout, each with its growth stub (on entry RAX = the
    /// receiver, not null; RAX = the receiver on the fall-through).
    fn emit_sb_append_code_point_site(
        &mut self,
        b: &crate::StringBuilderFieldLayout,
        recv_slot: StackSlot,
        arg_slot: StackSlot,
        decline: &mut Vec<usize>,
    ) {
        // The exact receiver guard: `StringBuilder` is final, so a header
        // class-id match IS that class, and it keeps a `StringBuffer`
        // (synchronized, `toStringCache`) off this path.
        //   CMP DWORD [RAX + 0], class_id
        self.buf.emit(&[0x81, 0x78, 0x00]);
        self.buf.emit(&b.class_id.to_le_bytes());
        decline.push(self.emit_jcc_rel32_patch(0x85)); // JNE
                                                       // JIT r23 wave 2, lane L5 (R23W1L5-1,
                                                       // `CRATONVM_JIT_SP_SB_APPEND_CODE_POINT_GROW`): a full LATIN1
                                                       // payload grows through `DirectHelperTable::sb_grow` and the
                                                       // arm runs again, as `append(char)`'s does (`str17`). The
                                                       // retry re-reads the code point from its frame home (the stub
                                                       // reuses R10), so it needs one.
        let cp_home = match arg_slot {
            StackSlot::Frame(off) => Some(off),
            _ => None,
        };
        let grow_helper = if sp_sb_append_code_point_grow_enabled()
            && cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_SB_GROW")
            && cp_home.is_some()
        {
            self.direct_helpers.sb_grow
        } else {
            0
        };
        let post_pop = self.next_spill_offset;
        let grow_save = if grow_helper != 0 {
            self.reserve_sb_grow_save(&[recv_slot, arg_slot])
        } else {
            None
        };
        let mut grow_c: Vec<usize> = Vec::new();
        let mut grow_l: Vec<usize> = Vec::new();
        self.emit_test_mem8_imm8(
            RAX,
            cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32,
            cratonvm_types::GC_FLAG_COMPACT,
        );
        let legacy = self.emit_jcc_rel32_patch(0x84); // JZ

        // Each arm (and its retry) starts by reading R10D = the code
        // point.
        let body_c = self.buf.pos();
        self.emit_load_code_point(R10, arg_slot, cp_home);
        self.emit_sb_append_code_point_body(
            b.count_compact_offset,
            b.value_compact_offset,
            b.coder_compact_offset,
            b.coder_compact_is_byte,
            decline,
            grow_save.is_some().then_some(&mut grow_c),
        );
        let joined = self.emit_jmp_rel32_patch();
        self.patch_rel32_to_here(legacy);
        let body_l = self.buf.pos();
        self.emit_load_code_point(R10, arg_slot, cp_home);
        self.emit_sb_append_code_point_body(
            b.count_legacy_offset,
            b.value_legacy_offset,
            b.coder_legacy_offset,
            false,
            decline,
            grow_save.is_some().then_some(&mut grow_l),
        );
        self.patch_rel32_to_here(joined);
        // `appendCodePoint` returns its receiver, still in RAX.
        let over_stubs = self.emit_jmp_rel32_patch();
        if let Some(save) = grow_save {
            for (list, retry, count_offset, value_offset) in [
                (
                    grow_c,
                    body_c,
                    b.count_compact_offset,
                    b.value_compact_offset,
                ),
                (grow_l, body_l, b.count_legacy_offset, b.value_legacy_offset),
            ] {
                self.emit_sb_grow_stub(
                    list,
                    retry,
                    count_offset,
                    value_offset,
                    false,
                    save,
                    grow_helper,
                    decline,
                );
            }
        }
        self.patch_rel32_to_here(over_stubs);
        self.next_spill_offset = post_pop;
    }

    /// `String.codePointBefore(int)` / `codePointCount(int, int)`: the class
    /// check, `value` / `coder`, the arguments, then the shared tails
    /// ([`emit_code_point_before_tail`] / [`emit_code_point_count_tail`]). On
    /// entry RAX = the receiver (not null); RAX = the answer on the
    /// fall-through.
    fn emit_string_code_point_span_body(
        &mut self,
        layout: &crate::StringFieldLayout,
        arg_slot: StackSlot,
        arg2_slot: Option<StackSlot>,
        decline: &mut Vec<usize>,
    ) {
        if layout.string_class_id != 0 {
            self.buf.emit(&[0x81, 0x78, 0x00]); // CMP DWORD [RAX+0], imm32
            self.buf.emit(&layout.string_class_id.to_le_bytes());
            decline.push(self.emit_jcc_rel32_patch(0x85)); // JNE
        }
        // RCX = value (the byte[]); null takes the call.
        self.emit_load_string_value_ptr(
            RCX,
            RAX,
            layout.value_compact_offset,
            layout.value_legacy_offset,
        );
        self.emit_test_r64_r64(RCX);
        decline.push(self.emit_jcc_rel32_patch(0x84)); // JZ

        // RDX = coder (0 LATIN1 / 1 UTF16).
        self.emit_load_string_i32_field(
            RDX,
            RAX,
            layout.coder_compact_offset,
            layout.coder_compact_is_byte,
            layout.coder_legacy_offset,
        );
        self.load_slot_to_reg(R10, arg_slot);
        match arg2_slot {
            Some(end) => {
                self.load_slot_to_reg(R9, end);
                emit_code_point_count_tail(&mut self.buf, decline);
            }
            None => emit_code_point_before_tail(&mut self.buf, decline),
        }
    }

    /// `StringBuilder.codePointAt(int)`: the class guard, `coder` / `count` /
    /// `value` per instance layout, then [`emit_sb_code_point_at_tail`]. On
    /// entry RAX = the receiver (not null); RAX = the code point on the
    /// fall-through.
    fn emit_sb_code_point_at_body(
        &mut self,
        b: &crate::StringBuilderFieldLayout,
        arg_slot: StackSlot,
        decline: &mut Vec<usize>,
    ) {
        //   CMP DWORD [RAX + 0], class_id
        self.buf.emit(&[0x81, 0x78, 0x00]);
        self.buf.emit(&b.class_id.to_le_bytes());
        decline.push(self.emit_jcc_rel32_patch(0x85)); // JNE

        // R10 = the index, zero-extended (it scales the full register).
        self.load_slot_to_reg(R10, arg_slot);
        self.buf.emit(&[0x45, 0x89, 0xD2]); // MOV R10D, R10D
        self.emit_test_mem8_imm8(
            RAX,
            cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32,
            cratonvm_types::GC_FLAG_COMPACT,
        );
        let legacy = self.emit_jcc_rel32_patch(0x84); // JZ
        self.emit_sb_code_point_loads(
            b.coder_compact_offset,
            b.coder_compact_is_byte,
            b.count_compact_offset,
            b.value_compact_offset,
        );
        let joined = self.emit_jmp_rel32_patch();
        self.patch_rel32_to_here(legacy);
        self.emit_sb_code_point_loads(
            b.coder_legacy_offset,
            false,
            b.count_legacy_offset,
            b.value_legacy_offset,
        );
        self.patch_rel32_to_here(joined);
        self.emit_test_r64_r64(RCX);
        decline.push(self.emit_jcc_rel32_patch(0x84)); // JZ: no payload yet
        emit_sb_code_point_at_tail(&mut self.buf, decline);
    }

    /// EDX = `coder`, R11D = `count`, RCX = `value` of the builder in RAX, at
    /// one instance layout's offsets.
    fn emit_sb_code_point_loads(
        &mut self,
        coder_offset: i32,
        coder_is_byte: bool,
        count_offset: i32,
        value_offset: i32,
    ) {
        if coder_is_byte {
            self.buf.emit(&[0x0F, 0xB6, 0x90]); // MOVZX EDX, BYTE [RAX+disp32]
        } else {
            self.buf.emit(&[0x8B, 0x90]); // MOV EDX, [RAX+disp32]
        }
        self.buf.emit(&coder_offset.to_le_bytes());
        self.buf.emit(&[0x44, 0x8B, 0x98]); // MOV R11D, [RAX+disp32]
        self.buf.emit(&count_offset.to_le_bytes());
        self.buf.emit(&[0x48, 0x8B, 0x88]); // MOV RCX, [RAX+disp32]
        self.buf.emit(&value_offset.to_le_bytes());
    }

    /// `String.codePointAt(int)`'s body: JDK 17 / 21 / 25 -- LATIN1:
    /// `checkIndex(index, value.length)`, `value[index] & 0xff`; UTF16: `length
    /// = value.length >> 1`, `checkIndex(index, length)`,
    /// `StringUTF16.codePointAt(value, index, length)` (the unit at `index`,
    /// unless it is a high surrogate whose successor inside `length` is a low
    /// one: then `Character.toCodePoint`). On entry RAX = the receiver (not
    /// null); on the fall-through RAX = the code point, zero-extended (a
    /// non-negative int, `<= 0x10FFFF`). Every refusal pushes into `decline`.
    fn emit_string_code_point_at_body(
        &mut self,
        layout: &crate::StringFieldLayout,
        index_slot: StackSlot,
        decline: &mut Vec<usize>,
    ) {
        let (Ok(len_off), Ok(data_off)) = (
            u8::try_from(cratonvm_types::ARRAY_LENGTH_OFFSET),
            u8::try_from(cratonvm_types::ARRAY_DATA_OFFSET),
        ) else {
            self.buf
                .mark_codegen_unencodable("code-point-at-array-offsets");
            return;
        };
        if len_off > 127 || data_off > 125 {
            self.buf
                .mark_codegen_unencodable("code-point-at-array-offsets");
            return;
        }
        // The exact String class id, as the optimizing tier's prefix checks it
        // (`String` is final): a verified `String`-declared site cannot see
        // anything else, and one compare keeps a forged receiver off the
        // field reads below.
        if layout.string_class_id != 0 {
            self.buf.emit(&[0x81, 0x78, 0x00]); // CMP DWORD [RAX+0], imm32
            self.buf.emit(&layout.string_class_id.to_le_bytes());
            decline.push(self.emit_jcc_rel32_patch(0x85)); // JNE
        }
        // RCX = value (the byte[]); null takes the call.
        self.emit_load_string_value_ptr(
            RCX,
            RAX,
            layout.value_compact_offset,
            layout.value_legacy_offset,
        );
        self.emit_test_r64_r64(RCX);
        decline.push(self.emit_jcc_rel32_patch(0x84)); // JZ

        // R10D = coder (0 LATIN1 / 1 UTF16).
        self.emit_load_string_i32_field(
            R10,
            RAX,
            layout.coder_compact_offset,
            layout.coder_compact_is_byte,
            layout.coder_legacy_offset,
        );
        // R9 = index, zero-extended: a negative index is a huge unsigned one
        // and declines at the compares below; a valid one addresses with all
        // 64 bits of R9.
        self.load_slot_to_reg(R9, index_slot);
        self.buf.emit(&[0x45, 0x89, 0xC9]); // MOV R9D, R9D
        self.buf.emit(&[0x8B, 0x41, len_off]); // MOV EAX, [RCX+len]
        self.buf.emit(&[0x45, 0x85, 0xD2]); // TEST R10D, R10D
        let utf16 = self.emit_jcc_rel32_patch(0x85); // JNZ

        // ---- LATIN1: the byte, zero-extended ----
        self.buf.emit(&[0x41, 0x39, 0xC1]); // CMP R9D, EAX
        decline.push(self.emit_jcc_rel32_patch(0x83)); // JAE
        self.buf.emit(&[0x42, 0x0F, 0xB6, 0x44, 0x09, data_off]); // MOVZX EAX, BYTE [RCX+R9+data]
        let latin1_done = self.emit_jmp_rel32_patch();
        // ---- UTF16 ----
        self.patch_rel32_to_here(utf16);
        // A coder other than 0 / 1 is no String the JDK builds; the call judges.
        self.buf.emit(&[0x41, 0x83, 0xFA, 0x01]); // CMP R10D, 1
        decline.push(self.emit_jcc_rel32_patch(0x85)); // JNE
        self.buf.emit(&[0xD1, 0xE8]); // SHR EAX, 1  (length in chars)
        self.buf.emit(&[0x41, 0x39, 0xC1]); // CMP R9D, EAX
        decline.push(self.emit_jcc_rel32_patch(0x83)); // JAE

        // EDX = c1.
        self.buf.emit(&[0x42, 0x0F, 0xB7, 0x54, 0x49, data_off]); // MOVZX EDX, WORD [RCX+R9*2+data]

        // `Character.isHighSurrogate(c1)`: (c1 & 0xFC00) == 0xD800.
        self.buf.emit(&[0x41, 0x89, 0xD3]); // MOV R11D, EDX
        self.buf.emit(&[0x41, 0x81, 0xE3, 0x00, 0xFC, 0x00, 0x00]); // AND R11D, 0xFC00
        self.buf.emit(&[0x41, 0x81, 0xFB, 0x00, 0xD8, 0x00, 0x00]); // CMP R11D, 0xD800
        let mut single: Vec<usize> = vec![self.emit_jcc_rel32_patch(0x85)]; // JNE

        // `++index < end` (index < length <= 2^30: no overflow).
        self.buf.emit(&[0x45, 0x8D, 0x59, 0x01]); // LEA R11D, [R9+1]
        self.buf.emit(&[0x41, 0x39, 0xC3]); // CMP R11D, EAX
        single.push(self.emit_jcc_rel32_patch(0x83)); // JAE: the last unit

        // R11D = c2; `Character.isLowSurrogate(c2)`.
        self.buf.emit(&[0x46, 0x0F, 0xB7, 0x5C, 0x49, data_off + 2]); // MOVZX R11D, WORD [RCX+R9*2+data+2]
        self.buf.emit(&[0x44, 0x89, 0xD8]); // MOV EAX, R11D
        self.buf.emit(&[0x25, 0x00, 0xFC, 0x00, 0x00]); // AND EAX, 0xFC00
        self.buf.emit(&[0x3D, 0x00, 0xDC, 0x00, 0x00]); // CMP EAX, 0xDC00
        single.push(self.emit_jcc_rel32_patch(0x85)); // JNE: an unpaired high surrogate

        // `Character.toCodePoint(c1, c2)`:
        // ((c1 << 10) + c2) + (0x10000 - (0xD800 << 10) - 0xDC00).
        self.buf.emit(&[0xC1, 0xE2, 0x0A]); // SHL EDX, 10
        self.buf.emit(&[0x44, 0x01, 0xDA]); // ADD EDX, R11D
        self.buf.emit(&[0x81, 0xC2]); // ADD EDX, imm32
        self.buf.emit(&CODE_POINT_PAIR_OFFSET.to_le_bytes());
        for p in single {
            self.patch_rel32_to_here(p);
        }
        self.buf.emit(&[0x89, 0xD0]); // MOV EAX, EDX
        self.patch_rel32_to_here(latin1_done);
    }

    /// `dst` = the code point operand: a plain frame load when it has a home
    /// (so a growth stub's retry, which jumps back here, re-reads it), else
    /// the ordinary slot load (no stub is planned without a home).
    fn emit_load_code_point(&mut self, dst: u8, slot: StackSlot, home: Option<i32>) {
        match home {
            Some(off) => self.emit_load_local(dst, off),
            None => self.load_slot_to_reg(dst, slot),
        }
    }

    /// One layout's arm of `StringBuilder.appendCodePoint(int)`: JDK 25's
    /// `AbstractStringBuilder.appendCodePoint` where nothing grows and nothing
    /// inflates --
    ///
    /// * LATIN1 builder, `cp` in `0..=0xFF`, `count < value.length`:
    ///   `append((char) cp)`'s LATIN1 arm, `value[count++] = (byte) cp`;
    /// * UTF16 builder, BMP `cp` (`cp >>> 16 == 0`; surrogate code units
    ///   included, as `isBmpCodePoint` admits them), `count < value.length >>
    ///   1`: `StringUTF16.putChar(value, count++, cp)`;
    /// * UTF16 builder, supplementary `cp` (`cp - 0x10000` unsigned below
    ///   `0x100000`), `count + 2 <= value.length >> 1`: `append(toChars(cp))`,
    ///   the high then the low surrogate.
    ///
    /// Everything else -- the inflation, the growth, an invalid code point
    /// (`toChars`' `IllegalArgumentException`), a coder other than 0 / 1, no
    /// payload, a `count` outside the payload -- declines BEFORE any store.
    /// Neither arm touches `maybeLatin1` (the JDK's append paths do not).
    ///
    /// On entry RAX = the receiver (null-checked and class-id-guarded), R10D =
    /// the code point. RCX, RDX, R9, R11 are scratch. RAX is unchanged on exit.
    fn emit_sb_append_code_point_body(
        &mut self,
        count_offset: i32,
        value_offset: i32,
        coder_offset: i32,
        coder_is_byte: bool,
        decline: &mut Vec<usize>,
        grow: Option<&mut Vec<usize>>,
    ) {
        let (Ok(len_off), Ok(data_off)) = (
            u8::try_from(cratonvm_types::ARRAY_LENGTH_OFFSET),
            u8::try_from(cratonvm_types::ARRAY_DATA_OFFSET),
        ) else {
            self.buf
                .mark_codegen_unencodable("sb-append-code-point-array-offsets");
            return;
        };
        if len_off > 127 || data_off > 125 {
            self.buf
                .mark_codegen_unencodable("sb-append-code-point-array-offsets");
            return;
        }
        // EDX = coder.
        if coder_is_byte {
            self.buf.emit(&[0x0F, 0xB6, 0x90]); // MOVZX EDX, BYTE [RAX+disp32]
        } else {
            self.buf.emit(&[0x8B, 0x90]); // MOV EDX, [RAX+disp32]
        }
        self.buf.emit(&coder_offset.to_le_bytes());
        // R11D = count (zero-extended into R11, which addresses with it).
        self.buf.emit(&[0x44, 0x8B, 0x98]); // MOV R11D, [RAX+disp32]
        self.buf.emit(&count_offset.to_le_bytes());
        // RCX = value (the byte[]); null takes the call.
        self.buf.emit(&[0x48, 0x8B, 0x88]); // MOV RCX, [RAX+disp32]
        self.buf.emit(&value_offset.to_le_bytes());
        self.emit_test_r64_r64(RCX);
        decline.push(self.emit_jcc_rel32_patch(0x84)); // JZ
        self.buf.emit(&[0x85, 0xD2]); // TEST EDX, EDX
        let utf16 = self.emit_jcc_rel32_patch(0x85); // JNZ

        // ---- LATIN1 ----
        self.buf.emit(&[0x41, 0x81, 0xFA, 0xFF, 0x00, 0x00, 0x00]); // CMP R10D, 0xFF
        decline.push(self.emit_jcc_rel32_patch(0x87)); // JA: inflation (or invalid)
        self.buf.emit(&[0x44, 0x3B, 0x59, len_off]); // CMP R11D, [RCX+len]
        let full = self.emit_jcc_rel32_patch(0x83); // JAE: full (or a corrupt count)
        match grow {
            // A growth only for a real `count` (`>= 0`): a negative one
            // (reflection, `Unsafe`) declines, so the stub's `count + 1` is
            // always a capacity the retry then finds room in.
            Some(g) => {
                let skip = self.emit_jmp_rel32_patch();
                self.patch_rel32_to_here(full);
                self.buf.emit(&[0x45, 0x85, 0xDB]); // TEST R11D, R11D
                decline.push(self.emit_jcc_rel32_patch(0x88)); // JS
                g.push(self.emit_jmp_rel32_patch());
                self.patch_rel32_to_here(skip);
            }
            None => decline.push(full),
        }
        self.buf.emit(&[0x46, 0x88, 0x54, 0x19, data_off]); // MOV BYTE [RCX+R11+data], R10B
        self.buf.emit(&[0x41, 0xFF, 0xC3]); // INC R11D
        let latin1_done = self.emit_jmp_rel32_patch();
        // ---- UTF16 ----
        self.patch_rel32_to_here(utf16);
        self.buf.emit(&[0x83, 0xFA, 0x01]); // CMP EDX, 1
        decline.push(self.emit_jcc_rel32_patch(0x85)); // JNE: no such coder

        // EDX = the capacity in chars, `value.length >> 1`.
        self.buf.emit(&[0x8B, 0x51, len_off]); // MOV EDX, [RCX+len]
        self.buf.emit(&[0xD1, 0xEA]); // SHR EDX, 1

        // `count < capacity` (unsigned) before either arm: a negative or
        // oversized `count` (reflection, `Unsafe`) never addresses the array.
        self.buf.emit(&[0x41, 0x39, 0xD3]); // CMP R11D, EDX
        decline.push(self.emit_jcc_rel32_patch(0x83)); // JAE: full
        self.buf.emit(&[0x45, 0x89, 0xD1]); // MOV R9D, R10D
        self.buf.emit(&[0x41, 0xC1, 0xE9, 0x10]); // SHR R9D, 16
        let supplementary = self.emit_jcc_rel32_patch(0x85); // JNZ

        // BMP: one char at `count`.
        self.buf.emit(&[0x66, 0x46, 0x89, 0x54, 0x59, data_off]); // MOV WORD [RCX+R11*2+data], R10W
        self.buf.emit(&[0x41, 0xFF, 0xC3]); // INC R11D
        let bmp_done = self.emit_jmp_rel32_patch();
        // Supplementary: valid only below 0x110000.
        self.patch_rel32_to_here(supplementary);
        self.buf.emit(&[0x45, 0x8D, 0x8A]); // LEA R9D, [R10 + disp32]
        self.buf.emit(&(-0x1_0000i32).to_le_bytes());
        self.buf.emit(&[0x41, 0x81, 0xF9]); // CMP R9D, imm32
        self.buf.emit(&0x10_0000i32.to_le_bytes());
        decline.push(self.emit_jcc_rel32_patch(0x83)); // JAE: invalid, the call throws

        // `count + 2 <= capacity` (count < capacity <= 2^30: no overflow).
        self.buf.emit(&[0x45, 0x8D, 0x4B, 0x02]); // LEA R9D, [R11+2]
        self.buf.emit(&[0x41, 0x39, 0xD1]); // CMP R9D, EDX
        decline.push(self.emit_jcc_rel32_patch(0x87)); // JA: needs growth

        // No decline past this point. High surrogate at `count`:
        // `(cp >>> 10) + (0xD800 - (0x10000 >>> 10))`.
        self.buf.emit(&[0x45, 0x89, 0xD1]); // MOV R9D, R10D
        self.buf.emit(&[0x41, 0xC1, 0xE9, 0x0A]); // SHR R9D, 10
        self.buf.emit(&[0x41, 0x81, 0xC1]); // ADD R9D, imm32
        self.buf
            .emit(&super::concat_emit::HIGH_SURROGATE_BIAS.to_le_bytes());
        self.buf.emit(&[0x66, 0x46, 0x89, 0x4C, 0x59, data_off]); // MOV WORD [RCX+R11*2+data], R9W

        // Low surrogate at `count + 1`: `(cp & 0x3FF) + 0xDC00`.
        self.buf.emit(&[0x45, 0x89, 0xD1]); // MOV R9D, R10D
        self.buf.emit(&[0x41, 0x81, 0xE1, 0xFF, 0x03, 0x00, 0x00]); // AND R9D, 0x3FF
        self.buf.emit(&[0x41, 0x81, 0xC1]); // ADD R9D, imm32
        self.buf
            .emit(&super::concat_emit::LOW_SURROGATE_BASE.to_le_bytes());
        self.buf.emit(&[0x66, 0x46, 0x89, 0x4C, 0x59, data_off + 2]); // MOV WORD [RCX+R11*2+data+2], R9W
        self.buf.emit(&[0x41, 0x83, 0xC3, 0x02]); // ADD R11D, 2
        self.patch_rel32_to_here(latin1_done);
        self.patch_rel32_to_here(bmp_done);
        // `count = R11D`.
        self.buf.emit(&[0x44, 0x89, 0x98]); // MOV [RAX+disp32], R11D
        self.buf.emit(&count_offset.to_le_bytes());
    }
}

/// Which code-point site [`Compiler::emit_sp_code_point_intrinsic`] emits.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CpSite {
    StrAt,
    StrBefore,
    StrCount,
    SbAppend,
    SbAt,
}

// ---- Shared tails: BOTH tiers emit these bytes (the optimizing tier's
// `CodePointBefore` / `CodePointCount` / `SbCodePointAt` prefix arms in
// `ir_lower.rs`, and the single-pass rows above), over a bare
// `ExecutableBuffer`, so the two answers cannot drift. Each pushes every
// refusal into `decline` (a rel32 placeholder the caller binds to its call)
// and leaves the answer in EAX (zero-extended: a non-negative int).
// Clobbers RAX, RDX, R9, R10, R11 (RCX is read only).

const _: () = assert!(
    cratonvm_types::ARRAY_LENGTH_OFFSET == 8 && cratonvm_types::ARRAY_DATA_OFFSET == 16,
    "the code-point tails bake ARRAY_LENGTH_OFFSET 8 / ARRAY_DATA_OFFSET 16"
);

/// A backward `JMP` to `top`.
fn jmp_back(buf: &mut ExecutableBuffer, top: usize) {
    let p = jmp_patch(buf);
    let rel = top as i32 - (p as i32 + 4); // Cast: rel32 displacement
    let _ = buf.try_patch_i32(p, rel);
}

/// JDK 17 / 21 / 25 `String.codePointBefore`: `i = index - 1`,
/// `checkIndex(i, length())`; LATIN1 `value[i] & 0xff`; UTF16
/// `StringUTF16.codePointBefore(value, index)` -- `c2 = unit[i]`, and when it
/// is a low surrogate and `i > 0` with `c1 = unit[i - 1]` a high one,
/// `Character.toCodePoint(c1, c2)`. Entry: RCX = `value` (not null), EDX =
/// `coder`, R10D = `index`. Every index outside `[1, length]` and a coder
/// other than 0 / 1 decline.
pub(crate) fn emit_code_point_before_tail(buf: &mut ExecutableBuffer, decline: &mut Vec<usize>) {
    // R10D = index - 1, zero-extended: `index == 0` wraps to a huge unsigned
    // value and `Integer.MIN_VALUE` to `0x7FFFFFFF`; both decline below.
    buf.emit(&[0x45, 0x8D, 0x52, 0xFF]); // LEA R10D, [R10 - 1]
    buf.emit(&[0x8B, 0x41, 0x08]); // MOV EAX, [RCX+8] (value.length)
    buf.emit(&[0x85, 0xD2]); // TEST EDX, EDX
    let utf16 = jcc_patch(buf, 0x85); // JNZ

    // LATIN1: the byte, zero-extended.
    buf.emit(&[0x41, 0x39, 0xC2]); // CMP R10D, EAX
    decline.push(jcc_patch(buf, 0x83)); // JAE
    buf.emit(&[0x42, 0x0F, 0xB6, 0x44, 0x11, 0x10]); // MOVZX EAX, BYTE [RCX+R10+16]
    let latin1_done = jmp_patch(buf);

    // UTF16: EAX = length in chars; EDX = c2.
    patch_here(buf, utf16);
    buf.emit(&[0x83, 0xFA, 0x01]); // CMP EDX, 1
    decline.push(jcc_patch(buf, 0x85)); // JNE: no such coder
    buf.emit(&[0xD1, 0xE8]); // SHR EAX, 1
    buf.emit(&[0x41, 0x39, 0xC2]); // CMP R10D, EAX
    decline.push(jcc_patch(buf, 0x83)); // JAE
    buf.emit(&[0x42, 0x0F, 0xB7, 0x54, 0x51, 0x10]); // MOVZX EDX, WORD [RCX+R10*2+16]

    // `Character.isLowSurrogate(c2)`: (c2 & 0xFC00) == 0xDC00.
    buf.emit(&[0x41, 0x89, 0xD3]); // MOV R11D, EDX
    buf.emit(&[0x41, 0x81, 0xE3, 0x00, 0xFC, 0x00, 0x00]); // AND R11D, 0xFC00
    buf.emit(&[0x41, 0x81, 0xFB, 0x00, 0xDC, 0x00, 0x00]); // CMP R11D, 0xDC00
    let mut single = vec![jcc_patch(buf, 0x85)]; // JNE
    buf.emit(&[0x45, 0x85, 0xD2]); // TEST R10D, R10D
    single.push(jcc_patch(buf, 0x84)); // JZ: the first unit

    // R11D = c1 = unit[i - 1]; `Character.isHighSurrogate(c1)`.
    buf.emit(&[0x46, 0x0F, 0xB7, 0x5C, 0x51, 0x0E]); // MOVZX R11D, WORD [RCX+R10*2+14]
    buf.emit(&[0x44, 0x89, 0xD8]); // MOV EAX, R11D
    buf.emit(&[0x25, 0x00, 0xFC, 0x00, 0x00]); // AND EAX, 0xFC00
    buf.emit(&[0x3D, 0x00, 0xD8, 0x00, 0x00]); // CMP EAX, 0xD800
    single.push(jcc_patch(buf, 0x85)); // JNE: an unpaired low surrogate

    // `Character.toCodePoint(c1, c2)`.
    buf.emit(&[0x41, 0xC1, 0xE3, 0x0A]); // SHL R11D, 10
    buf.emit(&[0x44, 0x01, 0xDA]); // ADD EDX, R11D
    buf.emit(&[0x81, 0xC2]); // ADD EDX, imm32
    buf.emit(&CODE_POINT_PAIR_OFFSET.to_le_bytes());
    for p in single {
        patch_here(buf, p);
    }
    buf.emit(&[0x89, 0xD0]); // MOV EAX, EDX
    patch_here(buf, latin1_done);
}

/// JDK 17 / 21 / 25 `String.codePointCount`:
/// `Preconditions.checkFromToIndex(begin, end, length(), IOOBE_FORMATTER)`;
/// LATIN1 `end - begin`; UTF16 `StringUTF16.codePointCount(value, begin,
/// end)` -- `end - begin`, less one for every high surrogate at `i` (walking
/// `begin..end`) whose successor `i + 1 < end` is a low one, which is then
/// skipped. Entry: RCX = `value` (not null), EDX = `coder`, R10D = `begin`,
/// R9D = `end`. Any other range and a coder other than 0 / 1 decline. The loop
/// reads only, inside `[begin, end)`.
pub(crate) fn emit_code_point_count_tail(buf: &mut ExecutableBuffer, decline: &mut Vec<usize>) {
    buf.emit(&[0x8B, 0x41, 0x08]); // MOV EAX, [RCX+8] (value.length)
    buf.emit(&[0x85, 0xD2]); // TEST EDX, EDX
    let latin1_len = jcc_patch(buf, 0x84); // JZ
    buf.emit(&[0x83, 0xFA, 0x01]); // CMP EDX, 1
    decline.push(jcc_patch(buf, 0x85)); // JNE: no such coder
    buf.emit(&[0xD1, 0xE8]); // SHR EAX, 1
    patch_here(buf, latin1_len);

    // `begin < 0 || begin > end || end > length` (signed) declines.
    buf.emit(&[0x45, 0x85, 0xD2]); // TEST R10D, R10D
    decline.push(jcc_patch(buf, 0x88)); // JS
    buf.emit(&[0x45, 0x39, 0xCA]); // CMP R10D, R9D
    decline.push(jcc_patch(buf, 0x8F)); // JG
    buf.emit(&[0x41, 0x39, 0xC1]); // CMP R9D, EAX
    decline.push(jcc_patch(buf, 0x8F)); // JG

    // Both non-negative now: clear the upper halves the scaled loads use.
    buf.emit(&[0x45, 0x89, 0xD2]); // MOV R10D, R10D
    buf.emit(&[0x45, 0x89, 0xC9]); // MOV R9D, R9D

    // EAX = end - begin, the LATIN1 answer and the UTF16 start.
    buf.emit(&[0x44, 0x89, 0xC8]); // MOV EAX, R9D
    buf.emit(&[0x44, 0x29, 0xD0]); // SUB EAX, R10D
    buf.emit(&[0x85, 0xD2]); // TEST EDX, EDX
    let mut counted = vec![jcc_patch(buf, 0x84)]; // JZ: LATIN1

    // UTF16 walk, R10D = i.
    let top = buf.pos();
    buf.emit(&[0x45, 0x39, 0xCA]); // CMP R10D, R9D
    counted.push(jcc_patch(buf, 0x8D)); // JGE: done
    buf.emit(&[0x46, 0x0F, 0xB7, 0x5C, 0x51, 0x10]); // MOVZX R11D, WORD [RCX+R10*2+16]
    buf.emit(&[0x41, 0xFF, 0xC2]); // INC R10D
    buf.emit(&[0x41, 0x81, 0xE3, 0x00, 0xFC, 0x00, 0x00]); // AND R11D, 0xFC00
    buf.emit(&[0x41, 0x81, 0xFB, 0x00, 0xD8, 0x00, 0x00]); // CMP R11D, 0xD800
    let not_high = jcc_patch(buf, 0x85); // JNE
    buf.emit(&[0x45, 0x39, 0xCA]); // CMP R10D, R9D
    counted.push(jcc_patch(buf, 0x8D)); // JGE: the high one was last
    buf.emit(&[0x46, 0x0F, 0xB7, 0x5C, 0x51, 0x10]); // MOVZX R11D, WORD [RCX+R10*2+16]
    buf.emit(&[0x41, 0x81, 0xE3, 0x00, 0xFC, 0x00, 0x00]); // AND R11D, 0xFC00
    buf.emit(&[0x41, 0x81, 0xFB, 0x00, 0xDC, 0x00, 0x00]); // CMP R11D, 0xDC00
    let not_low = jcc_patch(buf, 0x85); // JNE
    buf.emit(&[0xFF, 0xC8]); // DEC EAX
    buf.emit(&[0x41, 0xFF, 0xC2]); // INC R10D
    patch_here(buf, not_high);
    patch_here(buf, not_low);
    jmp_back(buf, top);
    for p in counted {
        patch_here(buf, p);
    }
}

/// JDK 25 `AbstractStringBuilder.codePointAt`: `checkIndex(index, count)`;
/// LATIN1 `value[index] & 0xff`; UTF16 `StringUTF16.codePointAtSB(value,
/// index, count)` -- c1, and when it is a high surrogate with `index + 1 <
/// count`, c2 (`checkIndex` against the payload first: a `count` past the
/// payload is the call's to throw), the pair when c2 is a low surrogate.
/// Entry: R10 = `index` (zero-extended), EDX = `coder`, R11D = `count`, RCX =
/// `value` (not null). Every index outside the count or the payload and a
/// coder other than 0 / 1 decline.
pub(crate) fn emit_sb_code_point_at_tail(buf: &mut ExecutableBuffer, decline: &mut Vec<usize>) {
    buf.emit(&[0x45, 0x39, 0xDA]); // CMP R10D, R11D (index vs count)
    decline.push(jcc_patch(buf, 0x83)); // JAE
    buf.emit(&[0x45, 0x89, 0xD9]); // MOV R9D, R11D (count)
    buf.emit(&[0x44, 0x8B, 0x59, 0x08]); // MOV R11D, [RCX+8] (value.length)
    buf.emit(&[0x85, 0xD2]); // TEST EDX, EDX
    let utf16 = jcc_patch(buf, 0x85); // JNZ

    // LATIN1: inside the payload, the byte.
    buf.emit(&[0x45, 0x39, 0xDA]); // CMP R10D, R11D
    decline.push(jcc_patch(buf, 0x83)); // JAE
    buf.emit(&[0x42, 0x0F, 0xB6, 0x44, 0x11, 0x10]); // MOVZX EAX, BYTE [RCX+R10+16]
    let latin1_done = jmp_patch(buf);

    patch_here(buf, utf16);
    buf.emit(&[0x83, 0xFA, 0x01]); // CMP EDX, 1
    decline.push(jcc_patch(buf, 0x85)); // JNE: no such coder
    buf.emit(&[0x41, 0xD1, 0xEB]); // SHR R11D, 1 (capacity in chars)
    buf.emit(&[0x45, 0x39, 0xDA]); // CMP R10D, R11D
    decline.push(jcc_patch(buf, 0x83)); // JAE
    buf.emit(&[0x42, 0x0F, 0xB7, 0x54, 0x51, 0x10]); // MOVZX EDX, WORD [RCX+R10*2+16] (c1)
    buf.emit(&[0x89, 0xD0]); // MOV EAX, EDX
    buf.emit(&[0x25, 0x00, 0xFC, 0x00, 0x00]); // AND EAX, 0xFC00
    buf.emit(&[0x3D, 0x00, 0xD8, 0x00, 0x00]); // CMP EAX, 0xD800
    let mut single = vec![jcc_patch(buf, 0x85)]; // JNE: not a high surrogate
    buf.emit(&[0x41, 0x8D, 0x42, 0x01]); // LEA EAX, [R10+1]
    buf.emit(&[0x44, 0x39, 0xC8]); // CMP EAX, R9D (`++index < count`)
    single.push(jcc_patch(buf, 0x83)); // JAE: the last unit
    buf.emit(&[0x44, 0x39, 0xD8]); // CMP EAX, R11D (`checkIndex(index, value)`)
    decline.push(jcc_patch(buf, 0x83)); // JAE: the call throws
    buf.emit(&[0x42, 0x0F, 0xB7, 0x44, 0x51, 0x12]); // MOVZX EAX, WORD [RCX+R10*2+18] (c2)
    buf.emit(&[0x41, 0x89, 0xC3]); // MOV R11D, EAX
    buf.emit(&[0x41, 0x81, 0xE3, 0x00, 0xFC, 0x00, 0x00]); // AND R11D, 0xFC00
    buf.emit(&[0x41, 0x81, 0xFB, 0x00, 0xDC, 0x00, 0x00]); // CMP R11D, 0xDC00
    single.push(jcc_patch(buf, 0x85)); // JNE: an unpaired high surrogate
    buf.emit(&[0xC1, 0xE2, 0x0A]); // SHL EDX, 10
    buf.emit(&[0x01, 0xC2]); // ADD EDX, EAX
    buf.emit(&[0x81, 0xC2]); // ADD EDX, imm32
    buf.emit(&CODE_POINT_PAIR_OFFSET.to_le_bytes());
    for p in single {
        patch_here(buf, p);
    }
    buf.emit(&[0x89, 0xD0]); // MOV EAX, EDX
    patch_here(buf, latin1_done);
}

/// `CRATONVM_JIT_SP_SB_APPEND_CODE_POINT_GROW` (JIT r23 wave 2, lane L5,
/// R23W1L5-1; perf, default ON, `=0` declines a full payload to the call as
/// before; also needs `CRATONVM_JIT_SB_GROW`): the single-pass
/// `appendCodePoint` row grows a full LATIN1 payload through the `sb_grow`
/// stub and runs again. Compile time only.
fn sp_sb_append_code_point_grow_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_SP_SB_APPEND_CODE_POINT_GROW")
}

/// `Character.MIN_SUPPLEMENTARY_CODE_POINT - (MIN_HIGH_SURROGATE << 10) -
/// MIN_LOW_SURROGATE`, as the JDK folds `Character.toCodePoint`.
const CODE_POINT_PAIR_OFFSET: i32 = 0x1_0000 - (0xD800 << 10) - 0xDC00;

#[cfg(test)]
mod tests {
    use super::CODE_POINT_PAIR_OFFSET;

    /// The folded constant, the surrogate split and the screens of both bodies,
    /// computed in Rust over the boundary values and compared with `char`'s own
    /// UTF-16 encoding -- the arithmetic the emitted instructions perform.
    #[test]
    fn r23_l5_code_point_arithmetic_matches_utf16() {
        let to_code_point = |hi: u32, lo: u32| {
            ((hi << 10).wrapping_add(lo) as i32).wrapping_add(CODE_POINT_PAIR_OFFSET)
        };
        for cp in [0x1_0000u32, 0x1_F600, 0x2_0000, 0xF_FFFF, 0x10_FFFF] {
            let c = char::from_u32(cp).expect("a valid supplementary code point");
            let mut units = [0u16; 2];
            let enc = c.encode_utf16(&mut units);
            assert_eq!(enc.len(), 2);
            let hi = (cp >> 10).wrapping_add(super::super::concat_emit::HIGH_SURROGATE_BIAS as u32);
            let lo = (cp & 0x3FF) + super::super::concat_emit::LOW_SURROGATE_BASE as u32;
            assert_eq!(
                (hi as u16, lo as u16),
                (units[0], units[1]),
                "split {cp:#x}"
            );
            assert_eq!(to_code_point(hi, lo), cp as i32, "join {cp:#x}");
            // The supplementary screen admits it ...
            assert!(cp.wrapping_sub(0x1_0000) < 0x10_0000);
        }
        // ... and refuses everything else a `>>> 16 != 0` code point can be.
        for bad in [0x11_0000u32, 0x7FFF_FFFF, 0x8000_0000, 0xFFFF_FFFF] {
            assert!(
                bad >> 16 != 0 && bad.wrapping_sub(0x1_0000) >= 0x10_0000,
                "{bad:#x}"
            );
        }
        // The BMP arm takes every `cp >>> 16 == 0`, lone surrogates included
        // (`Character.isBmpCodePoint`), and the LATIN1 screen is `<= 0xFF`
        // unsigned, so a negative code point declines there too.
        for bmp in [0u32, 0xFF, 0x100, 0xD800, 0xDBFF, 0xDC00, 0xFFFF] {
            assert_eq!(bmp >> 16, 0);
        }
        assert!((-1i32) as u32 > 0xFF);
        // The high-surrogate and low-surrogate masks.
        for u in 0u32..=0xFFFF {
            assert_eq!(u & 0xFC00 == 0xD800, (0xD800..=0xDBFF).contains(&u));
            assert_eq!(u & 0xFC00 == 0xDC00, (0xDC00..=0xDFFF).contains(&u));
        }
    }

    /// The disp8 encodings the bodies use for the array header must fit.
    #[test]
    fn r23_l5_array_header_offsets_fit_a_disp8() {
        assert!(cratonvm_types::ARRAY_LENGTH_OFFSET <= 127);
        assert!(cratonvm_types::ARRAY_DATA_OFFSET <= 125);
    }
}
