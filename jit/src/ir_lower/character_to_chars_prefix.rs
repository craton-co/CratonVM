#[allow(unused_imports)]
use super::*;

impl<'a> Lowerer<'a> {
    /// JIT r20 wave 19, lane zCp (page
    /// `jr20-pstr3-code-point-latin1-static-init-mcs-flip-rows-CLOSED-20261012.md`,
    /// row `R20PIRCodePoint` cp-tochars;
    /// `CRATONVM_JIT_IR_CHARACTER_TO_CHARS_PREFIX`): the static
    /// `Character.toChars(int)` answered inline as a guarded PREFIX of its
    /// `Op::Call` -- a fresh `char[1]` holding a BMP code point, or a fresh
    /// `char[2]` holding a supplementary one's surrogate pair (high, then
    /// low), bumped from the TLAB by the `char[]` copies' plan
    /// ([`crate::x64::concat_emit::emit_ir_character_to_chars`]). That is the
    /// JDK body's answer for every valid code point; an invalid one (negative,
    /// or above `0x10FFFF`) declines to the call, which raises
    /// `IllegalArgumentException("Not a valid Unicode code point: 0x..")`
    /// itself, as do a full TLAB, no thread and a collector pre-check miss.
    /// Every decline precedes the first store. A primitive array: no reference
    /// store, no barrier; the hit edge makes no call, polls nothing and cannot
    /// throw, so it owes no map and no deopt point. Scratch: the `char[]`
    /// copies' set (RAX, RCX, RDX, R8-R11).
    pub(in crate::ir_lower) fn try_emit_ir_character_to_chars_prefix(
        &mut self,
        id: NodeId,
        info: &JitInvokeInfo,
        layout: crate::StringFieldLayout,
    ) {
        use crate::x64::concat_emit as ce;
        use crate::IrStringSearchKind as K;
        // The code point's register on entry to the emitter (no module constant).
        const R9: u8 = 9;
        if !K::CharacterToChars.admits_invoke_kind(info.invoke_kind) {
            self.note_string_prefix_declined(
                id,
                info,
                &format!("invoke_kind={}", info.invoke_kind),
            );
            return;
        }
        if crate::x64::narrow_oops_block_inline_fields() {
            self.note_string_prefix_declined(id, info, "narrow oops / armed ZGC barrier");
            return;
        }
        let graph = self.graph;
        let Some(node) = graph.nodes.get(id as usize) else {
            return;
        };
        // [ctrl, mem, codePoint], a `char[]` result.
        if node.inputs.len() != 3 + K::CharacterToChars.num_params() || node.ty != IrType::Ref {
            self.note_string_prefix_declined(
                id,
                info,
                &format!("node shape: {} inputs, ty {:?}", node.inputs.len(), node.ty),
            );
            return;
        }
        let code_point = node.inputs[2];
        if self.carry_names(code_point) {
            self.note_string_prefix_declined(id, info, "an operand is a live carry");
            return;
        }
        let Some(slot) = self.prefix_result_slot(id, node.ty) else {
            self.note_string_prefix_declined(id, info, "a carried, routed, or dropped result home");
            return;
        };
        let frame = ce::ConcatIrFrame {
            tls_disp: crate::x64::jit_thread_tls_disp(),
            thread_slot_off: self.shadow_thread_slot_off,
            cursor_off: self.tlab_cursor_off,
            end_off: self.tlab_end_off,
            class_id_off: self.concat_class_id_off,
            g1_table: self.g1_barrier_addr,
            region_bounds_addr: self.concat_region_bounds_addr,
        };
        let site = match ce::chars_ir_site(
            Some(layout),
            self.direct_helpers.compact_strings_flag_addr(),
            frame,
        ) {
            Ok(site) => site,
            Err(why) => {
                self.note_string_prefix_declined(id, info, why);
                return;
            }
        };
        if !ce::character_to_chars_site_fits(&site) {
            self.note_string_prefix_declined(id, info, "array header offsets past disp8");
            return;
        }
        // The engagement control (`CRATONVM_DBG_IR_COMPILES` / `CRATONVM_DBG_JITC`).
        if crate::ir_stage_reporting() {
            eprintln!(
                "[ir] character-to-chars-prefix n{id} {}.{}{}",
                info.class_name, info.method_name, info.descriptor,
            );
        }
        let mut decline: Vec<usize> = Vec::new();
        // RCX: the emitter writes it.
        self.note_rcx_written();
        self.gp_load_value(R9, code_point);
        ce::emit_ir_character_to_chars(&mut self.buf, &site, &mut decline);
        self.emit_integer_value_of_result_store(id, slot);
        // A call the single-pass backend has no inline form for, lowered as
        // straight-line code: acceptance evidence, as the `char[]` copies.
        crate::ir_evidence::note(crate::ir_evidence::Transform::ScalarIntrinsic);
        let hit = self.emit_jmp_rel32();
        for p in decline {
            self.patch_rel32_to_here(p);
        }
        self.call_prefix_join = Some(hit);
    }

    /// JIT r20 wave 18, lane xSb (book `jit-r20-strings-proposals.md`,
    /// STR20-2; `CRATONVM_JIT_IR_SB_TOSTRING_PREFIX`, inside
    /// `CRATONVM_JIT_IR_SB_PREFIX`): `StringBuilder.toString()` answered as a
    /// guarded PREFIX of the `Op::Call` -- the twin of the single-pass
    /// STR16-P1 LATIN1 body (`emit_sb_to_string_latin1_body`): an exact
    /// `java/lang/StringBuilder` receiver (which keeps `StringBuffer` and its
    /// `toStringCache` off the path) whose coder is LATIN1 gets a FRESH LATIN1
    /// String of its `count` bytes, allocated inline by
    /// `x64::concat_emit::emit_ir_sb_to_string` (the `char[]` copy prefixes'
    /// bump, collector arm and header stores).
    ///
    /// Same contract with the `Op::Call` arm as
    /// [`Self::try_emit_ir_string_chars_prefix`]: scratch RAX, RCX, RDX,
    /// R8-R11; the hit edge stores the String and jumps to
    /// [`Self::call_prefix_join`]; no call, no poll, nothing thrown. Every
    /// decline -- a replaced layout, a null or foreign receiver, a UTF16
    /// builder (the JDK's `maybeLatin1` compression is the call's), an empty
    /// one, and the allocation's own -- precedes the first store and lands on
    /// the unchanged call. No acceptance evidence: the single-pass body builds
    /// the same String inline, as for the other builder kinds.
    pub(in crate::ir_lower) fn try_emit_ir_sb_to_string_prefix(
        &mut self,
        id: NodeId,
        info: &JitInvokeInfo,
        layout: crate::StringFieldLayout,
    ) {
        use crate::x64::concat_emit as ce;
        use crate::IrStringSearchKind as K;
        let Some(b) = layout.builder else {
            self.note_string_prefix_declined(id, info, "layout has no StringBuilder half");
            return;
        };
        if !K::SbToString.admits_invoke_kind(info.invoke_kind)
            || b.class_id == 0
            || b.value_compact_is_narrow
            || !layout.has_coder
            || layout.string_class_id == 0
        {
            self.note_string_prefix_declined(
                id,
                info,
                &format!(
                    "invoke_kind={} builder_class_id={} value_compact_is_narrow={} has_coder={}",
                    info.invoke_kind, b.class_id, b.value_compact_is_narrow, layout.has_coder
                ),
            );
            return;
        }
        if crate::x64::narrow_oops_block_inline_fields() {
            self.note_string_prefix_declined(id, info, "narrow oops / armed ZGC barrier");
            return;
        }
        let graph = self.graph;
        let Some(node) = graph.nodes.get(id as usize) else {
            return;
        };
        // [ctrl, mem, receiver] -> a String.
        if node.inputs.len() != 3 || node.ty != IrType::Ref {
            self.note_string_prefix_declined(
                id,
                info,
                &format!("node shape: {} inputs, ty {:?}", node.inputs.len(), node.ty),
            );
            return;
        }
        let recv = node.inputs[2];
        if self.carry_names(recv) {
            self.note_string_prefix_declined(id, info, "an operand is a live carry");
            return;
        }
        let Some(slot) = self.prefix_result_slot(id, node.ty) else {
            self.note_string_prefix_declined(id, info, "a carried, routed, or dropped result home");
            return;
        };
        let frame = ce::ConcatIrFrame {
            tls_disp: crate::x64::jit_thread_tls_disp(),
            thread_slot_off: self.shadow_thread_slot_off,
            cursor_off: self.tlab_cursor_off,
            end_off: self.tlab_end_off,
            class_id_off: self.concat_class_id_off,
            g1_table: self.g1_barrier_addr,
            region_bounds_addr: self.concat_region_bounds_addr,
        };
        let site = match ce::chars_ir_site(
            Some(layout),
            self.direct_helpers.compact_strings_flag_addr(),
            frame,
        ) {
            Ok(site) => site,
            Err(why) => {
                self.note_string_prefix_declined(id, info, why);
                return;
            }
        };
        if crate::ir_stage_reporting() {
            eprintln!(
                "[ir] string-builder-prefix n{id} {}.{}{} kind={:?} builder_class_id={}",
                info.class_name,
                info.method_name,
                info.descriptor,
                K::SbToString,
                b.class_id,
            );
        }
        let mut decline: Vec<usize> = Vec::new();
        // RCX: the epoch guard's fallback form and the field loads write it.
        self.note_rcx_written();
        // A layout REPLACEMENT invalidates the baked builder offsets (the
        // String's are guarded again by the allocation's own guard).
        if let Some(p) = self.emit_layout_epoch_guard() {
            decline.push(p);
        }
        // RAX = receiver; null declines (the call raises the NPE).
        self.gp_load_value(RAX, recv);
        self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
        decline.push(self.emit_jcc_rel32(0x84)); // JZ
        self.emit_str_cmp_class_id(RAX, b.class_id);
        decline.push(self.emit_jcc_rel32(0x85)); // JNE
                                                 // R11D = coder, ECX = count (zero-extended), RDX = value, per layout.
        self.emit_str_test_mem8(
            RAX,
            cratonvm_types::GC_FLAGS_BYTE_OFFSET as u8, // Cast: x86-64 disp8
            cratonvm_types::GC_FLAG_COMPACT,
        );
        let legacy = self.emit_jcc_rel32(0x84); // JZ legacy
        if b.coder_compact_is_byte {
            self.emit_str_movzx8_base_disp32(R11, RAX, b.coder_compact_offset);
        } else {
            self.emit_load_base_disp32(R11, RAX, b.coder_compact_offset, false);
        }
        self.emit_load_base_disp32(RCX, RAX, b.count_compact_offset, false);
        self.emit_load_base_disp32(RDX, RAX, b.value_compact_offset, true);
        let have = self.emit_jmp_rel32();
        self.patch_rel32_to_here(legacy);
        self.emit_load_base_disp32(R11, RAX, b.coder_legacy_offset, false);
        self.emit_load_base_disp32(RCX, RAX, b.count_legacy_offset, false);
        self.emit_load_base_disp32(RDX, RAX, b.value_legacy_offset, true);
        self.patch_rel32_to_here(have);
        self.buf.emit(&[0x45, 0x85, 0xDB]); // TEST R11D, R11D
        decline.push(self.emit_jcc_rel32(0x85)); // JNZ: a UTF16 builder
        ce::emit_ir_sb_to_string(&mut self.buf, &site, &mut decline);
        self.emit_integer_value_of_result_store(id, slot);
        let hit = self.emit_jmp_rel32();
        for p in decline {
            self.patch_rel32_to_here(p);
        }
        self.call_prefix_join = Some(hit);
    }

    /// Round rx27, lane splitfast (`CRATONVM_JIT_IR_STRING_SUBSTRING_PREFIX` /
    /// `CRATONVM_JIT_IR_SEQ_SUBSEQUENCE_PREFIX`, both default ON, `=0` kills; page
    /// `r23-w5-compiled-jdk-string-split-and-regex-bodies-7-13x-20261021.md`):
    /// `String.substring(int, int)` / `String.subSequence(int, int)`
    /// ([`crate::IrStringSearchKind::Substring`]) and, for a String receiver,
    /// `CharSequence.subSequence(int, int)` (`SeqSubSequence`) answered as a
    /// guarded PREFIX of the `Op::Call`: a LATIN1 receiver with `0 <= begin <=
    /// end <= length` answers itself for the whole range, else a fresh LATIN1
    /// String of `value[begin..end]` with its `byte[]`, both in one TLAB bump
    /// (`x64::concat_emit::emit_ir_latin1_substring`). That is the per-field
    /// `substring` -> `StringLatin1.newString` -> `Arrays.copyOfRange([BII)`
    /// -> `new String` chain of the `String.split(CIZ)` fast path, which HotSpot
    /// runs as allocation plus an intrinsic copy.
    ///
    /// Same contract with the `Op::Call` arm as
    /// [`Self::try_emit_ir_string_chars_prefix`]: scratch RAX, RCX, RDX,
    /// R8-R11; the hit edge stores the String and jumps to
    /// [`Self::call_prefix_join`]; no call, no poll, nothing thrown. Every
    /// decline -- a replaced layout, a null receiver or one that is not exactly
    /// `java/lang/String` (a `CharSequence` receiver must also be a plain
    /// object), a UTF16 or unknown coder, every range the JDK body throws
    /// `StringIndexOutOfBoundsException` for, the empty range (the `""`
    /// literal) and the allocation's own -- precedes the first store and lands
    /// on the unchanged call, which answers or throws exactly as before.
    pub(in crate::ir_lower) fn try_emit_ir_string_substring_prefix(
        &mut self,
        id: NodeId,
        info: &JitInvokeInfo,
        kind: crate::IrStringSearchKind,
        layout: crate::StringFieldLayout,
    ) {
        use crate::x64::concat_emit as ce;
        use crate::IrStringSearchKind as K;
        // `emit_ir_latin1_substring`'s `endIndex` register (no module constant).
        const R9: u8 = 9;
        if !kind.admits_invoke_kind(info.invoke_kind)
            || !layout.has_coder
            || layout.string_class_id == 0
        {
            self.note_string_prefix_declined(
                id,
                info,
                &format!(
                    "invoke_kind={} has_coder={} string_class_id={}",
                    info.invoke_kind, layout.has_coder, layout.string_class_id
                ),
            );
            return;
        }
        if crate::x64::narrow_oops_block_inline_fields() {
            self.note_string_prefix_declined(id, info, "narrow oops / armed ZGC barrier");
            return;
        }
        let graph = self.graph;
        let Some(node) = graph.nodes.get(id as usize) else {
            return;
        };
        // [ctrl, mem, receiver, beginIndex, endIndex] -> a String.
        if node.inputs.len() != 3 + kind.num_params() || node.ty != IrType::Ref {
            self.note_string_prefix_declined(
                id,
                info,
                &format!("node shape: {} inputs, ty {:?}", node.inputs.len(), node.ty),
            );
            return;
        }
        if node.inputs[2..].iter().any(|&i| self.carry_names(i)) {
            self.note_string_prefix_declined(id, info, "an operand is a live carry");
            return;
        }
        let Some(slot) = self.prefix_result_slot(id, node.ty) else {
            self.note_string_prefix_declined(id, info, "a carried, routed, or dropped result home");
            return;
        };
        let frame = ce::ConcatIrFrame {
            tls_disp: crate::x64::jit_thread_tls_disp(),
            thread_slot_off: self.shadow_thread_slot_off,
            cursor_off: self.tlab_cursor_off,
            end_off: self.tlab_end_off,
            class_id_off: self.concat_class_id_off,
            g1_table: self.g1_barrier_addr,
            region_bounds_addr: self.concat_region_bounds_addr,
        };
        let site = match ce::chars_ir_site(
            Some(layout),
            self.direct_helpers.compact_strings_flag_addr(),
            frame,
        ) {
            Ok(site) => site,
            Err(why) => {
                self.note_string_prefix_declined(id, info, why);
                return;
            }
        };
        // The engagement control (`CRATONVM_DBG_IR_COMPILES` / `CRATONVM_DBG_JITC`).
        if crate::ir_stage_reporting() {
            eprintln!(
                "[ir] string-substring-prefix n{id} {}.{}{} kind={kind:?} string_class_id={}",
                info.class_name, info.method_name, info.descriptor, layout.string_class_id,
            );
        }
        let (recv, begin, end) = (node.inputs[2], node.inputs[3], node.inputs[4]);
        let mut decline: Vec<usize> = Vec::new();
        // RCX: the epoch guard's fallback form and the field loads write it.
        self.note_rcx_written();
        // A layout REPLACEMENT invalidates the baked field offsets (the
        // String's are guarded again by the allocation's own guard).
        if let Some(p) = self.emit_layout_epoch_guard() {
            decline.push(p);
        }
        // RAX = receiver; null declines (the call raises the NPE).
        self.gp_load_value(RAX, recv);
        self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
        decline.push(self.emit_jcc_rel32(0x84)); // JZ
        if kind == K::SeqSubSequence {
            // A `CharSequence` may be anything: a plain object only (an array
            // header carries its COMPONENT's class id), as the `Seq*` kinds.
            self.emit_str_test_mem8(
                RAX,
                cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8, // Cast: x86-64 disp8
                cratonvm_types::KIND_TAG_BYTE_MASK,
            );
            decline.push(self.emit_jcc_rel32(0x85)); // JNE
        }
        self.emit_str_cmp_class_id(RAX, layout.string_class_id);
        decline.push(self.emit_jcc_rel32(0x85)); // JNE

        // R10D = beginIndex, R9D = endIndex (writes only those; RAX stays the
        // receiver), then RCX = value, EDX = coder.
        self.gp_load_value(R10, begin);
        self.gp_load_value(R9, end);
        self.emit_str_value_and_coder(&layout, RAX, RCX, Some(RDX));
        self.buf.emit(&[0x85, 0xD2]); // TEST EDX, EDX
        decline.push(self.emit_jcc_rel32(0x85)); // JNZ: UTF16 (or no such coder)
        ce::emit_ir_latin1_substring(&mut self.buf, &site, &mut decline);
        self.emit_integer_value_of_result_store(id, slot);
        // A call chain the single-pass backend has no inline form for, lowered
        // as straight-line code: acceptance evidence, as the `char[]` copies.
        crate::ir_evidence::note(crate::ir_evidence::Transform::ScalarIntrinsic);
        let hit = self.emit_jmp_rel32();
        for p in decline {
            self.patch_rel32_to_here(p);
        }
        self.call_prefix_join = Some(hit);
    }

    /// The `CharsIrSite` (allocation plan, collector arm, thread source) of
    /// this compile, or why there is none. Shared by the rx27 wave-3 prefixes.
    pub(in crate::ir_lower) fn rx27_chars_site(
        &self,
        layout: crate::StringFieldLayout,
    ) -> Result<crate::x64::concat_emit::CharsIrSite, &'static str> {
        use crate::x64::concat_emit as ce;
        let frame = ce::ConcatIrFrame {
            tls_disp: crate::x64::jit_thread_tls_disp(),
            thread_slot_off: self.shadow_thread_slot_off,
            cursor_off: self.tlab_cursor_off,
            end_off: self.tlab_end_off,
            class_id_off: self.concat_class_id_off,
            g1_table: self.g1_barrier_addr,
            region_bounds_addr: self.concat_region_bounds_addr,
        };
        ce::chars_ir_site(
            Some(layout),
            self.direct_helpers.compact_strings_flag_addr(),
            frame,
        )
    }

    /// Round rx27 wave 3, lane splitfast (`CRATONVM_JIT_IR_ARRAYS_COPY_OF_RANGE_PREFIX`,
    /// opt-in; page `rx27-splitfast-copyofrange-byte-prefix-proposal-FIXED-20261029.md`):
    /// the static `Arrays.copyOfRange(byte[], int, int)`
    /// ([`crate::IrStringSearchKind::ArraysCopyOfRangeBytes`]) answered as a
    /// guarded PREFIX of its `Op::Call`: a fresh `byte[to - from]` bumped and
    /// copied inline (`x64::concat_emit::emit_ir_byte_copy_of_range`). Same
    /// contract with the `Op::Call` arm as
    /// [`Self::try_emit_ir_string_chars_prefix`]: scratch RAX, RCX, RDX,
    /// R8-R11; no call, no poll, nothing thrown on the hit edge; every decline
    /// (each throwing input, the zero-padded `to > length`, the allocation's
    /// own) precedes the first store and lands on the unchanged call.
    pub(in crate::ir_lower) fn try_emit_ir_arrays_copy_of_range_prefix(
        &mut self,
        id: NodeId,
        info: &JitInvokeInfo,
        layout: crate::StringFieldLayout,
    ) {
        use crate::x64::concat_emit as ce;
        use crate::IrStringSearchKind as K;
        const R9: u8 = 9;
        let kind = K::ArraysCopyOfRangeBytes;
        if !kind.admits_invoke_kind(info.invoke_kind) {
            self.note_string_prefix_declined(id, info, "not a static call");
            return;
        }
        let graph = self.graph;
        let Some(node) = graph.nodes.get(id as usize) else {
            return;
        };
        // [ctrl, mem, original, from, to] -> a byte[].
        if node.inputs.len() != 3 + kind.num_params() || node.ty != IrType::Ref {
            self.note_string_prefix_declined(
                id,
                info,
                &format!("node shape: {} inputs, ty {:?}", node.inputs.len(), node.ty),
            );
            return;
        }
        if node.inputs[2..].iter().any(|&i| self.carry_names(i)) {
            self.note_string_prefix_declined(id, info, "an operand is a live carry");
            return;
        }
        let Some(slot) = self.prefix_result_slot(id, node.ty) else {
            self.note_string_prefix_declined(id, info, "a carried, routed, or dropped result home");
            return;
        };
        let site = match self.rx27_chars_site(layout) {
            Ok(site) => site,
            Err(why) => {
                self.note_string_prefix_declined(id, info, why);
                return;
            }
        };
        if crate::ir_stage_reporting() {
            eprintln!(
                "[ir] arrays-copy-of-range-prefix n{id} {}.{}{}",
                info.class_name, info.method_name, info.descriptor,
            );
        }
        let (original, from, to) = (node.inputs[2], node.inputs[3], node.inputs[4]);
        let mut decline: Vec<usize> = Vec::new();
        self.note_rcx_written();
        // Each load writes only its destination.
        self.gp_load_value(R10, from);
        self.gp_load_value(R9, to);
        self.gp_load_value(RDX, original);
        ce::emit_ir_byte_copy_of_range(&mut self.buf, &site, &mut decline);
        self.emit_integer_value_of_result_store(id, slot);
        crate::ir_evidence::note(crate::ir_evidence::Transform::ScalarIntrinsic);
        let hit = self.emit_jmp_rel32();
        for p in decline {
            self.patch_rel32_to_here(p);
        }
        self.call_prefix_join = Some(hit);
    }

    /// Round rx27 wave 3, lane splitfast (`CRATONVM_JIT_IR_ARRAYLIST_ADD_PREFIX`,
    /// opt-in; page `docs/feature-designs/collections-interception.md`):
    /// `ArrayList.add(Object)` answered as a guarded PREFIX of its `Op::Call`
    /// where the JDK's own `ArrayList` bytecode runs (the layout carries the
    /// add half only then): JDK 25's
    ///
    /// ```text
    /// modCount++; add(e, elementData, size); return true;
    /// // add(E, Object[], int): if (s == elementData.length) elementData = grow();
    /// //                        elementData[s] = e; size = s + 1;
    /// ```
    ///
    /// for the no-growth case. Declines (to the call, nothing stored): a
    /// replaced layout; a null receiver, not a plain object, not exactly
    /// `java/util/ArrayList`, or a LEGACY instance; a null element (left to
    /// the call); a null or non-reference-array `elementData`; `size < 0` or
    /// `size >= length` (growth is the JDK body's); a non-null last slot with
    /// spare capacity (the stale-view marker the get prefix screens); a
    /// non-null slot at `size` (SATB: the overwritten value must be null);
    /// and every collector screen of `x64::concat_emit::emit_ir_ref_store_proof`
    /// (the store must owe nothing). Past them: `modCount + 1`, the store,
    /// `size + 1`, `true`. No call, no poll, nothing thrown; scratch RAX, RCX,
    /// RDX, R8-R11.
    pub(in crate::ir_lower) fn try_emit_ir_arraylist_add_prefix(
        &mut self,
        id: NodeId,
        info_ptr: usize,
    ) {
        use crate::x64::concat_emit as ce;
        const R9: u8 = 9;
        let Some(al) = self.array_list_layout else {
            return;
        };
        let Some(mod_count_off) = al.mod_count_compact_offset else {
            return;
        };
        if info_ptr == 0 || !crate::ir_arraylist_add_prefix_enabled() {
            return;
        }
        // SAFETY: as every other read of an `Op::Call`'s `info_ptr` in this
        // file -- a live `JitInvokeInfo` owned by `ir_call_infos`.
        let info: &JitInvokeInfo = unsafe { &*(info_ptr as *const JitInvokeInfo) };
        if !crate::arraylist_add_site(
            info.class_name,
            info.method_name,
            info.descriptor,
            info.invoke_kind,
        ) {
            return;
        }
        let Some(layout) = self.string_layout else {
            self.note_string_prefix_declined(id, info, "no String layout (no allocation plan)");
            return;
        };
        if crate::x64::narrow_oops_block_inline_fields() {
            self.note_string_prefix_declined(id, info, "narrow oops / armed ZGC barrier");
            return;
        }
        let graph = self.graph;
        let Some(node) = graph.nodes.get(id as usize) else {
            return;
        };
        // [ctrl, mem, receiver, element] -> boolean.
        if node.inputs.len() != 4 || node.ty != IrType::Int {
            self.note_string_prefix_declined(
                id,
                info,
                &format!("node shape: {} inputs, ty {:?}", node.inputs.len(), node.ty),
            );
            return;
        }
        let (recv, elem) = (node.inputs[2], node.inputs[3]);
        if self.carry_names(recv) || self.carry_names(elem) {
            self.note_string_prefix_declined(id, info, "an operand is a live carry");
            return;
        }
        let Some(slot) = self.prefix_result_slot(id, node.ty) else {
            self.note_string_prefix_declined(id, info, "a carried, routed, or dropped result home");
            return;
        };
        // The collector arm and thread source (no allocation is made).
        let site = match self.rx27_chars_site(layout) {
            Ok(site) => site,
            Err(why) => {
                self.note_string_prefix_declined(id, info, why);
                return;
            }
        };
        if crate::ir_stage_reporting() {
            eprintln!(
                "[ir] arraylist-add-prefix n{id} {}.{}{} class_id={}",
                info.class_name, info.method_name, info.descriptor, al.class_id,
            );
        }
        const DATA: u8 = cratonvm_types::ARRAY_DATA_OFFSET as u8; // Cast: x86-64 disp8
        let mut decline: Vec<usize> = Vec::new();
        self.note_rcx_written();
        if let Some(p) = self.emit_layout_epoch_guard() {
            decline.push(p);
        }
        // RAX = receiver: non-null, a plain object, exactly ArrayList, COMPACT.
        self.gp_load_value(RAX, recv);
        self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
        decline.push(self.emit_jcc_rel32(0x84)); // JZ
        self.emit_str_test_mem8(
            RAX,
            cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8, // Cast: x86-64 disp8
            cratonvm_types::KIND_TAG_BYTE_MASK,
        );
        decline.push(self.emit_jcc_rel32(0x85)); // JNE
        self.emit_str_cmp_class_id(RAX, al.class_id);
        decline.push(self.emit_jcc_rel32(0x85)); // JNE
        self.emit_str_test_mem8(
            RAX,
            cratonvm_types::GC_FLAGS_BYTE_OFFSET as u8, // Cast: x86-64 disp8
            cratonvm_types::GC_FLAG_COMPACT,
        );
        decline.push(self.emit_jcc_rel32(0x84)); // JZ: legacy instance

        // R9 = the element (non-null).
        self.gp_load_value(R9, elem);
        self.buf.emit(&[0x4D, 0x85, 0xC9]); // TEST R9, R9
        decline.push(self.emit_jcc_rel32(0x84)); // JZ: a null element is the call's

        // RDX = elementData: non-null and exactly `Object[]` (rx27 wave 4
        // review item 2: a swapped `String[]` is the call's `aastore`).
        self.emit_load_base_disp32(RDX, RAX, al.data_compact_offset, true);
        ce::emit_ir_object_array_screen(&mut self.buf, al.object_class_id, &mut decline);

        // The store owes the collector nothing (clobbers RAX, R8, R10, R11).
        ce::emit_ir_ref_store_proof(&mut self.buf, &site, &mut decline);
        // RAX = receiver again; R10D = size, R11D = elementData.length.
        self.gp_load_value(RAX, recv);
        self.emit_load_base_disp32(R10, RAX, al.size_compact_offset, false);
        self.buf.emit(&[0x45, 0x85, 0xD2]); // TEST R10D, R10D
        decline.push(self.emit_jcc_rel32(0x88)); // JS
        self.emit_load_base_disp32(
            R11,
            RDX,
            cratonvm_types::ARRAY_LENGTH_OFFSET as i32, // Cast: small header constant
            false,
        );
        self.buf.emit(&[0x45, 0x39, 0xDA]); // CMP R10D, R11D (size vs length)
        decline.push(self.emit_jcc_rel32(0x83)); // JAE: growth is the call's

        // Spare capacity: a non-null LAST slot is the stale-view marker.
        // CMP QWORD [RDX + R11*8 + (DATA - 8)], 0.
        self.buf.emit(&[
            0x4A,
            0x83,
            0x7C,
            0xDA,
            (cratonvm_types::ARRAY_DATA_OFFSET - cratonvm_types::REF_ELEMENT_SIZE) as u8, // Cast: x86-64 disp8
            0x00,
        ]);
        decline.push(self.emit_jcc_rel32(0x85)); // JNE

        // SATB: the slot at `size` must hold null.
        // CMP QWORD [RDX + R10*8 + DATA], 0 (R10 zero-extended by its load).
        self.buf.emit(&[0x4A, 0x83, 0x7C, 0xD2, DATA, 0x00]);
        decline.push(self.emit_jcc_rel32(0x85)); // JNE

        // ---- no decline past this point ----
        // modCount++ (INC DWORD [RAX + disp32]).
        self.buf.emit(&[0xFF, 0x80]);
        self.buf.emit(&mod_count_off.to_le_bytes());
        // elementData[size] = e (MOV [RDX + R10*8 + DATA], R9).
        self.buf.emit(&[0x4E, 0x89, 0x4C, 0xD2, DATA]);
        // size = size + 1 (MOV DWORD [RAX + disp32], R10D).
        self.buf.emit(&[0x41, 0xFF, 0xC2]); // INC R10D
        self.buf.emit(&[0x44, 0x89, 0x90]);
        self.buf.emit(&al.size_compact_offset.to_le_bytes());
        self.buf.emit(&[0xB8, 0x01, 0x00, 0x00, 0x00]); // MOV EAX, 1 (true)
        self.emit_integer_value_of_result_store(id, slot);
        crate::ir_evidence::note(crate::ir_evidence::Transform::ScalarIntrinsic);
        let hit = self.emit_jmp_rel32();
        for p in decline {
            self.patch_rel32_to_here(p);
        }
        self.call_prefix_join = Some(hit);
    }

    /// The LATIN1 -> UTF-16 copy shared by the `getChars` and `inflate`
    /// prefixes (nio20 wave 6, L2): RCX = `&src[from] - 16` (a `byte[]`
    /// payload cursor), RDX = `&dst[to] - 16` (a `char[]` one), R10D = the
    /// count, already proven in range for both arrays. Copies `count` bytes
    /// into `count` chars (`c = b & 0xFF`). Writes RAX, R11 and, under
    /// `CRATONVM_JIT_IR_STRING_SSE2`, XMM0/XMM1: eight chars per step
    /// (`MOVQ` the bytes, `PUNPCKLBW` against zero -- byte -> little-endian
    /// u16 --, `MOVDQU` the sixteen bytes), then one at a time. No safepoint
    /// poll, no reference store.
    pub(in crate::ir_lower) fn emit_str_inflate_copy(&mut self) {
        // R11D = i, counting up to R10D.
        self.buf.emit(&[0x45, 0x31, 0xDB]); // XOR R11D, R11D
        if crate::ir_string_sse2_enabled() {
            // Eight chars per step: MOVQ the bytes, interleave with zero
            // (PUNPCKLBW: byte -> little-endian u16), MOVDQU the 16 bytes.
            self.buf.emit(&[0x66, 0x0F, 0xEF, 0xC9]); // PXOR XMM1, XMM1
            let v8 = self.buf.pos();
            self.buf.emit(&[0x44, 0x89, 0xD0]); // MOV EAX, R10D
            self.buf.emit(&[0x44, 0x29, 0xD8]); // SUB EAX, R11D
            self.buf.emit(&[0x83, 0xF8, 0x08]); // CMP EAX, 8
            let to_tail = self.emit_jcc_rel32(0x82); // JB: fewer than 8 left
            self.buf.emit(&[0xF3, 0x42, 0x0F, 0x7E, 0x44, 0x19, 0x10]); // MOVQ XMM0, [RCX+R11+16]
            self.buf.emit(&[0x66, 0x0F, 0x60, 0xC1]); // PUNPCKLBW XMM0, XMM1
            self.buf.emit(&[0xF3, 0x42, 0x0F, 0x7F, 0x44, 0x5A, 0x10]); // MOVDQU [RDX+R11*2+16], XMM0
            self.buf.emit(&[0x41, 0x83, 0xC3, 0x08]); // ADD R11D, 8
            self.emit_jmp_back_to(v8);
            self.patch_rel32_to_here(to_tail);
        }
        let tail = self.buf.pos();
        self.buf.emit(&[0x45, 0x39, 0xD3]); // CMP R11D, R10D
        let done = self.emit_jcc_rel32(0x83); // JAE
        self.buf.emit(&[0x42, 0x0F, 0xB6, 0x44, 0x19, 0x10]); // MOVZX EAX, BYTE [RCX+R11+16]
        self.buf.emit(&[0x66, 0x42, 0x89, 0x44, 0x5A, 0x10]); // MOV [RDX+R11*2+16], AX
        self.buf.emit(&[0x41, 0xFF, 0xC3]); // INC R11D
        self.emit_jmp_back_to(tail);
        self.patch_rel32_to_here(done);
    }

    /// JIT r20 wave 18, lane xUtf8 (STR20-6): `StringCoding.countPositives`'
    /// scan, with RCX = `ba + off` (an array base, payload at `+16`) and R10D =
    /// `len`, both proved in range by the caller. Leaves the bytecode's answer
    /// in EAX (zero-extended): the index of the first negative byte, else
    /// `len`. Scratch R11, XMM0. Each byte the answer depends on is read once
    /// (the SSE2 hit is answered by `BSF` on the loaded mask, not re-read).
    pub(in crate::ir_lower) fn emit_str_count_positives_scan(&mut self) {
        // R11D = i, counting up to R10D.
        self.buf.emit(&[0x45, 0x31, 0xDB]); // XOR R11D, R11D
        let mut found_in_eax: Vec<usize> = Vec::new();
        if crate::ir_string_sse2_enabled() {
            // Sixteen bytes per step: a negative byte is a set mask bit.
            let v16 = self.buf.pos();
            self.buf.emit(&[0x44, 0x89, 0xD0]); // MOV EAX, R10D
            self.buf.emit(&[0x44, 0x29, 0xD8]); // SUB EAX, R11D
            self.buf.emit(&[0x83, 0xF8, 0x10]); // CMP EAX, 16
            let to_tail = self.emit_jcc_rel32(0x82); // JB: fewer than 16 left
            self.buf.emit(&[0xF3, 0x42, 0x0F, 0x6F, 0x44, 0x19, 0x10]); // MOVDQU XMM0, [RCX+R11+16]
            self.buf.emit(&[0x66, 0x0F, 0xD7, 0xC0]); // PMOVMSKB EAX, XMM0
            self.buf.emit(&[0x85, 0xC0]); // TEST EAX, EAX
            let hit = self.emit_jcc_rel32(0x85); // JNZ
            self.buf.emit(&[0x41, 0x83, 0xC3, 0x10]); // ADD R11D, 16
            self.emit_jmp_back_to(v16);
            self.patch_rel32_to_here(hit);
            self.buf.emit(&[0x0F, 0xBC, 0xC0]); // BSF EAX, EAX
            self.buf.emit(&[0x44, 0x01, 0xD8]); // ADD EAX, R11D
            found_in_eax.push(self.emit_jmp_rel32());
            self.patch_rel32_to_here(to_tail);
        }
        let tail = self.buf.pos();
        self.buf.emit(&[0x45, 0x39, 0xD3]); // CMP R11D, R10D
        let all = self.emit_jcc_rel32(0x83); // JAE: no negative byte
        self.buf.emit(&[0x42, 0x80, 0x7C, 0x19, 0x10, 0x00]); // CMP BYTE [RCX+R11+16], 0
        let negative = self.emit_jcc_rel32(0x8C); // JL
        self.buf.emit(&[0x41, 0xFF, 0xC3]); // INC R11D
        self.emit_jmp_back_to(tail);
        self.patch_rel32_to_here(negative);
        self.buf.emit(&[0x44, 0x89, 0xD8]); // MOV EAX, R11D
        found_in_eax.push(self.emit_jmp_rel32());
        self.patch_rel32_to_here(all);
        self.buf.emit(&[0x44, 0x89, 0xD0]); // MOV EAX, R10D
        for p in found_in_eax {
            self.patch_rel32_to_here(p);
        }
    }

    /// JIT r20 wave 18, lane xUtf8 (STR20-6): `StringCoding.implEncodeAsciiArray`'s
    /// copy, with RCX = `sa + sp * 2` (a `char[]` base, payload at `+16`), RDX
    /// = `da + dp` (a `byte[]` base) and R10D = `len`, every bound proved by
    /// the caller. Copies chars while they are `< 0x80` and leaves the count
    /// copied in EAX (zero-extended); nothing is stored at or past the first
    /// char `>= 0x80`. Scratch R11, XMM0, XMM1.
    pub(in crate::ir_lower) fn emit_str_encode_ascii_copy(&mut self) {
        // R11D = i, counting up to R10D.
        self.buf.emit(&[0x45, 0x31, 0xDB]); // XOR R11D, R11D
        if crate::ir_string_sse2_enabled() {
            // Eight chars per step. XMM1 = 0x7F80 in every lane, rebuilt each
            // step because the test consumes it: `PADDUSW` saturates, so lane
            // bit 15 is set exactly when the char is `>= 0x80`, and it is
            // the high byte's bit 7 in `PMOVMSKB` (the odd mask bits). The
            // eight chars that passed are narrowed from XMM0 itself.
            let v8 = self.buf.pos();
            self.buf.emit(&[0x44, 0x89, 0xD0]); // MOV EAX, R10D
            self.buf.emit(&[0x44, 0x29, 0xD8]); // SUB EAX, R11D
            self.buf.emit(&[0x83, 0xF8, 0x08]); // CMP EAX, 8
            let to_tail = self.emit_jcc_rel32(0x82); // JB: fewer than 8 left
            self.buf.emit(&[0xF3, 0x42, 0x0F, 0x6F, 0x44, 0x59, 0x10]); // MOVDQU XMM0, [RCX+R11*2+16]
            self.buf.emit(&[0xB8, 0x80, 0x7F, 0x80, 0x7F]); // MOV EAX, 0x7F807F80
            self.buf.emit(&[0x66, 0x0F, 0x6E, 0xC8]); // MOVD XMM1, EAX
            self.buf.emit(&[0x66, 0x0F, 0x70, 0xC9, 0x00]); // PSHUFD XMM1, XMM1, 0
            self.buf.emit(&[0x66, 0x0F, 0xDD, 0xC8]); // PADDUSW XMM1, XMM0
            self.buf.emit(&[0x66, 0x0F, 0xD7, 0xC1]); // PMOVMSKB EAX, XMM1
            self.buf.emit(&[0xA9, 0xAA, 0xAA, 0x00, 0x00]); // TEST EAX, 0xAAAA
            let wide = self.emit_jcc_rel32(0x85); // JNZ: a char >= 0x80 in this step
            self.buf.emit(&[0x66, 0x0F, 0x67, 0xC0]); // PACKUSWB XMM0, XMM0
            self.buf.emit(&[0x66, 0x42, 0x0F, 0xD6, 0x44, 0x1A, 0x10]); // MOVQ [RDX+R11+16], XMM0
            self.buf.emit(&[0x41, 0x83, 0xC3, 0x08]); // ADD R11D, 8
            self.emit_jmp_back_to(v8);
            self.patch_rel32_to_here(to_tail);
            self.patch_rel32_to_here(wide);
        }
        // One char at a time (the remainder, and the step that holds the
        // first char >= 0x80, which stops it).
        let tail = self.buf.pos();
        self.buf.emit(&[0x45, 0x39, 0xD3]); // CMP R11D, R10D
        let done = self.emit_jcc_rel32(0x83); // JAE
        self.buf.emit(&[0x42, 0x0F, 0xB7, 0x44, 0x59, 0x10]); // MOVZX EAX, WORD [RCX+R11*2+16]
        self.buf.emit(&[0x3D, 0x80, 0x00, 0x00, 0x00]); // CMP EAX, 0x80
        let stop = self.emit_jcc_rel32(0x83); // JAE
        self.buf.emit(&[0x42, 0x88, 0x44, 0x1A, 0x10]); // MOV [RDX+R11+16], AL
        self.buf.emit(&[0x41, 0xFF, 0xC3]); // INC R11D
        self.emit_jmp_back_to(tail);
        self.patch_rel32_to_here(done);
        self.patch_rel32_to_here(stop);
        self.buf.emit(&[0x44, 0x89, 0xD8]); // MOV EAX, R11D
    }

    /// A String's `value` (64-bit; narrow oops are refused by the caller) into
    /// `value_dst` and, when asked, its `coder` (zero-extended) into
    /// `coder_dst`, dispatching on `GC_FLAG_COMPACT` as the single-pass
    /// `emit_load_string_value_ptr` / `emit_load_string_i32_field` do.
    /// `value_dst` may be `obj` only when no coder is asked (the one load
    /// reads `obj` before writing it).
    pub(in crate::ir_lower) fn emit_str_value_and_coder(
        &mut self,
        layout: &crate::StringFieldLayout,
        obj: u8,
        value_dst: u8,
        coder_dst: Option<u8>,
    ) {
        debug_assert!(coder_dst.is_none() || (value_dst != obj && coder_dst != Some(obj)));
        self.emit_str_test_mem8(
            obj,
            cratonvm_types::GC_FLAGS_BYTE_OFFSET as u8, // Cast: x86-64 disp8
            cratonvm_types::GC_FLAG_COMPACT,
        );
        let legacy = self.emit_jcc_rel32(0x84); // JZ legacy
        if let Some(c) = coder_dst {
            if layout.coder_compact_is_byte {
                self.emit_str_movzx8_base_disp32(c, obj, layout.coder_compact_offset);
            } else {
                self.emit_load_base_disp32(c, obj, layout.coder_compact_offset, false);
            }
        }
        self.emit_load_base_disp32(value_dst, obj, layout.value_compact_offset, true);
        let have = self.emit_jmp_rel32();
        self.patch_rel32_to_here(legacy);
        if let Some(c) = coder_dst {
            self.emit_load_base_disp32(c, obj, layout.coder_legacy_offset, false);
        }
        self.emit_load_base_disp32(value_dst, obj, layout.value_legacy_offset, true);
        self.patch_rel32_to_here(have);
    }

    /// `TEST BYTE [base + disp8], imm8` (`F6 /0 ib`). `base` is never RSP/R12.
    pub(in crate::ir_lower) fn emit_str_test_mem8(&mut self, base: u8, disp: u8, imm: u8) {
        debug_assert!(base & 7 != 4);
        if base >= 8 {
            self.buf.emit_byte(0x41);
        }
        self.buf.emit(&[0xF6, 0x40 | (base & 7), disp, imm]);
    }

    /// `CMP DWORD [base + 0], imm32` (`81 /7 id`): the header class id.
    pub(in crate::ir_lower) fn emit_str_cmp_class_id(&mut self, base: u8, class_id: u32) {
        debug_assert!(base & 7 != 4);
        if base >= 8 {
            self.buf.emit_byte(0x41);
        }
        self.buf.emit(&[0x81, 0x78 | (base & 7), 0x00]);
        self.buf.emit(&class_id.to_le_bytes());
    }

    /// JIT r21 wave 1/3, lane strings: the `StringLatin1.compareToCI` loop over
    /// two non-null LATIN1 `byte[]`s in RCX / R11 (both kinds that answer it):
    /// the first `k < min(len1, len2)` whose bytes differ AND whose
    /// [`Self::emit_latin1_ci_fold`] folds differ answers `fold1 - fold2`,
    /// else `len1 - len2`; RAX = the answer, sign-extended. No decline.
    /// Clobbers RAX, RDX, R10 and the flags.
    pub(in crate::ir_lower) fn emit_latin1_ci_loop(&mut self) {
        self.emit_latin1_ci_lim(); // R10D = min(len1, len2)
        self.buf.emit(&[0x31, 0xD2]); // XOR EDX, EDX (k)
        let top = self.buf.pos();
        self.buf.emit(&[0x44, 0x39, 0xD2]); // CMP EDX, R10D
        let tail = self.emit_jcc_rel32(0x83); // JAE: the common prefix is equal
        self.buf.emit(&[0x0F, 0xB6, 0x44, 0x11, 0x10]); // MOVZX EAX, BYTE [RCX+RDX+16]
        self.buf.emit(&[0x41, 0x3A, 0x44, 0x13, 0x10]); // CMP AL, BYTE [R11+RDX+16]
        let mismatch = self.emit_jcc_rel32(0x85); // JNE
        let next = self.buf.pos();
        self.buf.emit(&[0xFF, 0xC2]); // INC EDX
        self.emit_jmp_back_to(top);
        // Differing bytes: EAX = fold(v1[k]), R10D = fold(v2[k]).
        self.patch_rel32_to_here(mismatch);
        self.emit_latin1_ci_fold(RAX);
        self.buf.emit(&[0x45, 0x0F, 0xB6, 0x54, 0x13, 0x10]); // MOVZX R10D, BYTE [R11+RDX+16]
        self.emit_latin1_ci_fold(R10);
        self.buf.emit(&[0x44, 0x29, 0xD0]); // SUB EAX, R10D
        let differ = self.emit_jcc_rel32(0x85); // JNZ: the answer
                                                // Equal folds: R10D was the bound; reload it and go on.
        self.emit_latin1_ci_lim();
        self.emit_jmp_back_to(next);
        self.patch_rel32_to_here(tail);
        self.emit_load_base_disp32(RAX, RCX, 8, false); // EAX = len1
        self.buf.emit(&[0x41, 0x2B, 0x43, 0x08]); // SUB EAX, [R11+8]
        self.patch_rel32_to_here(differ);
        self.buf.emit(&[0x48, 0x63, 0xC0]); // MOVSXD RAX, EAX
    }

    /// JIT r21 wave 1, lane strings (STRINGS21-2): `R10D = min(len1, len2)`
    /// over the two LATIN1 `byte[]`s in RCX / R11; EAX is scratch.
    pub(in crate::ir_lower) fn emit_latin1_ci_lim(&mut self) {
        self.buf.emit(&[0x44, 0x8B, 0x51, 0x08]); // MOV R10D, [RCX+8]
        self.buf.emit(&[0x41, 0x8B, 0x43, 0x08]); // MOV EAX, [R11+8]
        self.buf.emit(&[0x41, 0x39, 0xC2]); // CMP R10D, EAX
        self.buf.emit(&[0x44, 0x0F, 0x47, 0xD0]); // CMOVA R10D, EAX
    }

    /// JIT r21 wave 1, lane strings (STRINGS21-2): `reg32 = fold(reg32)` for a
    /// Latin-1 byte value `0..=0xFF`, in place and with no scratch, where
    /// `fold(b) = Character.toLowerCase((char)
    /// CharacterDataLatin1.toUpperCase(b))`: `A-Z` and `0xC0..=0xDE` except
    /// `0xD7` gain `0x20`; `0xB5` (upper `0x39C`) is `0x3BC`; every other byte
    /// (`a-z`, `0xDF`, `0xE0..=0xFF` with `0xFF` via `0x178`, the rest) is
    /// itself. `reg` is RAX or R10 (the immediate forms below; 32-bit
    /// compares, so `0xB5` / `0xC0` never sign-extend).
    pub(in crate::ir_lower) fn emit_latin1_ci_fold(&mut self, reg: u8) {
        debug_assert!(reg == RAX || reg == R10);
        let rex = (reg >= 8).then_some(0x41u8);
        let r = reg & 7;
        let cmp = |s: &mut Self, imm: u32| {
            if let Some(p) = rex {
                s.buf.emit_byte(p);
            }
            s.buf.emit(&[0x81, 0xF8 | r]); // CMP r32, imm32
            s.buf.emit(&imm.to_le_bytes());
        };
        let mut done: Vec<usize> = Vec::new();
        cmp(self, 0x41);
        done.push(self.emit_jcc_rel32(0x82)); // JB: below `A`
        cmp(self, 0x5A);
        let add_a = self.emit_jcc_rel32(0x86); // JBE: `A-Z`
        cmp(self, 0xB5);
        let mu = self.emit_jcc_rel32(0x84); // JE: the micro sign
        cmp(self, 0xC0);
        done.push(self.emit_jcc_rel32(0x82)); // JB
        cmp(self, 0xDE);
        done.push(self.emit_jcc_rel32(0x87)); // JA: `0xDF..=0xFF`
        cmp(self, 0xD7);
        done.push(self.emit_jcc_rel32(0x84)); // JE: the multiplication sign
        self.patch_rel32_to_here(add_a);
        if let Some(p) = rex {
            self.buf.emit_byte(p);
        }
        self.buf.emit(&[0x83, 0xC0 | r, 0x20]); // ADD r32, 0x20
        done.push(self.emit_jmp_rel32());
        self.patch_rel32_to_here(mu);
        if let Some(p) = rex {
            self.buf.emit_byte(p);
        }
        self.buf.emit(&[0xB8 | r]); // MOV r32, imm32
        self.buf.emit(&0x3BCu32.to_le_bytes());
        for p in done {
            self.patch_rel32_to_here(p);
        }
    }

    /// `TEST r64, r64`.
    pub(in crate::ir_lower) fn emit_str_test_r64(&mut self, reg: u8) {
        let ext = (reg >= 8) as u8;
        self.buf.emit(&[
            0x48 | (ext << 2) | ext,
            0x85,
            0xC0 | ((reg & 7) << 3) | (reg & 7),
        ]);
    }

    /// `MOVZX dst32, BYTE [base + disp32]` (a SIB byte for an RSP/R12 base).
    pub(in crate::ir_lower) fn emit_str_movzx8_base_disp32(
        &mut self,
        dst: u8,
        base: u8,
        disp: i32,
    ) {
        if dst == RCX {
            self.note_rcx_written();
        }
        let rex = 0x40u8 | (((dst >= 8) as u8) << 2) | ((base >= 8) as u8);
        if rex != 0x40 {
            self.buf.emit_byte(rex);
        }
        self.buf
            .emit(&[0x0F, 0xB6, 0x80 | ((dst & 7) << 3) | (base & 7)]);
        if base & 7 == 4 {
            self.buf.emit_byte(0x24);
        }
        self.buf.emit(&disp.to_le_bytes());
    }

    /// `Jcc rel32` back to the earlier offset `target`.
    pub(in crate::ir_lower) fn emit_str_jcc_back(&mut self, cc: u8, target: usize) {
        let patch = self.emit_jcc_rel32(cc);
        let rel = target as i32 - (patch as i32 + 4);
        let _ = Self::patch_or_bail(&mut self.buf, patch, rel);
    }

    /// BOX_HASH_EQUALS in the optimizing tier (round 11 wave 12, lane
    /// hashmap's page `r11w12-hashmap-ir-box-hash-equals-prefix-patch`, applied
    /// by lane calls): answer `Integer.hashCode()` and
    /// `Integer.equals(Object)` inline at an `Object`/`Number`/`Integer`-typed
    /// `invokevirtual`, as a guarded PREFIX of the call's unchanged lowering.
    /// The twin of `x64/op_invoke.rs::try_emit_box_hash_equals_prefix`.
    ///
    /// Same contract with the `Op::Call` arm as
    /// [`Self::try_emit_ir_arraylist_access_prefix`]: scratch RAX, RCX, RDX
    /// and R10 only (R11 through the epoch guard); the hit edge writes the
    /// result home (and register) and jumps to [`Self::call_prefix_join`].
    ///
    /// Every uncertain case DECLINES to the call, never deopts: a null
    /// receiver (the call raises the NPE at this bci), a receiver or argument
    /// that is not a plain object of exactly `java/lang/Integer` (the class is
    /// final, so for the argument "exactly Integer" is `instanceof Integer`,
    /// JLS `Integer.equals`), and a replaced layout. `equals(null)` answers
    /// `false` inline.
    pub(in crate::ir_lower) fn try_emit_ir_box_hash_equals_prefix(
        &mut self,
        id: NodeId,
        info_ptr: usize,
    ) {
        if info_ptr == 0 {
            return;
        }
        // SAFETY: as every other read of an `Op::Call`'s `info_ptr` in this
        // file -- a live `JitInvokeInfo` owned by `ir_call_infos`.
        let info: &JitInvokeInfo = unsafe { &*(info_ptr as *const JitInvokeInfo) };
        let is_hash = info.method_name == "hashCode" && info.descriptor == "()I";
        let is_equals = info.method_name == "equals" && info.descriptor == "(Ljava/lang/Object;)Z";
        if info.invoke_kind != 0 || !(is_hash || is_equals) {
            return;
        }
        let integer_site = matches!(
            info.class_name,
            "java/lang/Object" | "java/lang/Number" | "java/lang/Integer"
        );
        // Round 11 wave 14 (lane hashmap, the String half of
        // `r11w11-hashmap-cratonbench-hashmap-where-the-time-goes`): the
        // single-pass prefix's `String.hashCode()` arm, at the same sites
        // (an `Object`-typed `hashCode`, where the String-typed intrinsics do
        // not reach) -- `HashMap.hash` for a `HashMap<String, V>`.
        let string_site = is_hash && info.class_name == "java/lang/Object";
        if !integer_site && !string_site {
            return;
        }
        // The `Integer` half: the guard id, proven initialised (so its layout
        // is registered). `Integer.value` is the class's only instance
        // field, slot 0.
        let integer = if integer_site && !crate::box_unbox_intrinsic_disabled() {
            self.direct_helpers
                .integer_class_id()
                .filter(|&cid| self.direct_helpers.class_is_initialized(cid))
                .and_then(|cid| {
                    crate::AtomicIntFieldLayout::new(0, cid)
                        .filter(|l| l.class_id == cid)
                        .map(|l| (cid, l))
                })
        } else {
            None
        };
        // The `String` half: the guard id and the `hash` cell, built from
        // the VM's slot index by the same int-cell arithmetic
        // (`AtomicIntFieldLayout::new`) `StringFieldLayout::new` uses for
        // `hash`. The VM answers only for the `byte[]`/`coder` layout, the
        // single-pass arm's `has_coder` screen.
        let string = if string_site && crate::object_hashcode_string_guard_enabled() {
            self.direct_helpers
                .string_hash_field()
                .filter(|&(cid, _)| self.direct_helpers.class_is_initialized(cid))
                .and_then(|(cid, hash_idx)| {
                    crate::AtomicIntFieldLayout::new(hash_idx, cid)
                        .filter(|l| l.class_id == cid)
                        .map(|l| (cid, l))
                })
        } else {
            None
        };
        // nio20 wave 5, lane L2: the `Class` half -- an `Object`-typed
        // `hashCode()` whose receiver is exactly `java/lang/Class` (final, does
        // not override `hashCode`, so `Object.hashCode`'s identity hash) is
        // answered from the header when a hash is installed there
        // (`emit_ir_identity_hash_read`); `HashMap<Class<?>, V>` keys reach
        // `HashMap.hash` exactly here. Same screens as
        // `try_emit_ir_identity_hash_prefix`.
        let class_arm = if string_site && ir_identity_hash_inline_enabled() {
            ir_class_class_id(&self.direct_helpers)
        } else {
            None
        };
        // JIT round 21, lane hash (NATIVE21-1): the identity arm -- an
        // `Object`-typed `hashCode()` whose receiver is a plain object of a
        // class whose `hashCode` IS `Object.hashCode` answers the identity
        // hash from the header (`emit_ir_identity_hash_read`), as the `Class`
        // arm does. Exactly `java/lang/Object` (class id 0) always; the class
        // the site's inline cache was seeded with when the VM proves it
        // inherits `Object.hashCode` (`identity_hash_profiled_class`). Same
        // screens as the `Class` arm.
        let identity_arm = string_site
            && object_hash_identity_arm_enabled()
            && ir_identity_hash_inline_enabled();
        // JIT round 21, lane hashlock (HASH2-21-1): the equals identity arm --
        // an `Object`-typed `equals(Object)` whose receiver is a plain object
        // of a class whose `equals(Object)` IS `Object.equals` answers
        // `this == arg` (`identity_equals_class_table`); every other receiver
        // keeps the call.
        let equals_arm = is_equals
            && info.class_name == "java/lang/Object"
            && object_equals_identity_arm_enabled();
        // An equals site has nothing to answer without the `Integer` half
        // (or, since HASH2-21-1, the equals identity arm).
        if integer.is_none()
            && ((string.is_none() && class_arm.is_none() && !identity_arm) || is_equals)
            && !equals_arm
        {
            return;
        }
        let graph = self.graph;
        let Some(node) = graph.node_opt(id) else {
            return;
        };
        // [ctrl, mem, receiver] or [ctrl, mem, receiver, argument].
        let want_inputs = if is_equals { 4 } else { 3 };
        if node.inputs.len() != want_inputs || node.ty != IrType::Int {
            return;
        }
        let recv = node.inputs[2];
        let arg = if is_equals {
            Some(node.inputs[3])
        } else {
            None
        };
        if self.carry_names(recv) || arg.is_some_and(|a| self.carry_names(a)) {
            return;
        }
        let Some(slot) = self.prefix_result_slot(id, node.ty) else {
            return;
        };
        if crate::ir_stage_reporting() {
            eprintln!(
                "[ir] box-hash-equals-prefix n{id} {}.{}{} integer_class_id={:?} string_class_id={:?}",
                info.class_name,
                info.method_name,
                info.descriptor,
                integer.map(|(cid, _)| cid),
                string.map(|(cid, _)| cid),
            );
        }
        let mut decline: Vec<usize> = Vec::new();
        let mut to_store: Vec<usize> = Vec::new();
        // RCX: the epoch guard's fallback form and the value load write it.
        self.note_rcx_written();
        if let Some(p) = self.emit_layout_epoch_guard() {
            decline.push(p);
        }
        // RAX = receiver; null declines (the call raises the NPE at this bci).
        self.gp_load_value(RAX, recv);
        self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
        decline.push(self.emit_jcc_rel32(0x84)); // JZ
                                                 // TEST BYTE [RAX+KIND_TAGS], KIND_TAG_BYTE_MASK (F6 /0 ib) ; JNE array_recv/decline.
        self.buf.emit(&[
            0xF6,
            0x40,
            cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8, // Cast: x86-64 disp8
            cratonvm_types::KIND_TAG_BYTE_MASK,
        ]);
        let array_recv = if (!is_equals && identity_arm) || (is_equals && equals_arm) {
            Some(self.emit_jcc_rel32(0x85))
        } else {
            decline.push(self.emit_jcc_rel32(0x85));
            None
        };
        // The `Integer` class compare's miss: the `String` arm below when
        // there is one, else a decline.
        let mut not_integer: Option<usize> = None;
        // The `String` class compare's miss (nio20 wave 5 L2): the `Class` arm
        // when there is one, else a decline.
        let mut not_string: Option<usize> = None;
        if let Some((cid, layout)) = integer {
            let (compact, legacy) = (layout.value_compact_offset, layout.value_legacy_offset);
            // CMP DWORD [RAX+0], Integer ; JNE not_integer.
            self.buf.emit(&[0x81, 0x78, 0x00]);
            self.buf.emit(&cid.to_le_bytes());
            not_integer = Some(self.emit_jcc_rel32(0x85));
            // TEST BYTE [RAX+GC_FLAGS], COMPACT (F6 /0 ib) ; JZ legacy.
            self.buf.emit(&[
                0xF6,
                0x40,
                cratonvm_types::GC_FLAGS_BYTE_OFFSET as u8, // Cast: x86-64 disp8
                cratonvm_types::GC_FLAG_COMPACT,
            ]);
            let recv_legacy = self.emit_jcc_rel32(0x84);
            self.emit_load_base_disp32(RCX, RAX, compact, false); // ECX = this.value
            let recv_have = self.emit_jmp_rel32();
            self.patch_rel32_to_here(recv_legacy);
            self.emit_load_base_disp32(RCX, RAX, legacy, false);
            self.patch_rel32_to_here(recv_have);
            self.emit_ir_box_hash_equals_integer_tail(
                cid,
                compact,
                legacy,
                arg,
                &mut decline,
                &mut to_store,
            );
        }
        if let Some((str_cid, layout)) = string {
            if let Some(p) = not_integer.take() {
                self.patch_rel32_to_here(p);
            }
            // `String.hashCode()` from a NON-ZERO cached `hash` only, which is
            // what the JDK method returns without reading `value` or `coder`;
            // zero (not computed yet, or `hashIsZero`) declines and the call
            // computes it. A racy read is the JDK's own benign race: a
            // non-zero `hash` is only ever the final one. String is final, so
            // the exact class id selects exactly its `hashCode`.
            // CMP DWORD [RAX+0], String ; JNE not_string (the `Class` arm
            // below when there is one, else a decline).
            self.buf.emit(&[0x81, 0x78, 0x00]);
            self.buf.emit(&str_cid.to_le_bytes());
            not_string = Some(self.emit_jcc_rel32(0x85));
            // TEST BYTE [RAX+GC_FLAGS], COMPACT (F6 /0 ib) ; JZ legacy.
            self.buf.emit(&[
                0xF6,
                0x40,
                cratonvm_types::GC_FLAGS_BYTE_OFFSET as u8, // Cast: x86-64 disp8
                cratonvm_types::GC_FLAG_COMPACT,
            ]);
            let hash_legacy = self.emit_jcc_rel32(0x84);
            self.emit_load_base_disp32(RCX, RAX, layout.value_compact_offset, false); // ECX = hash
            let hash_have = self.emit_jmp_rel32();
            self.patch_rel32_to_here(hash_legacy);
            self.emit_load_base_disp32(RCX, RAX, layout.value_legacy_offset, false);
            self.patch_rel32_to_here(hash_have);
            self.buf.emit(&[0x85, 0xC9]); // TEST ECX, ECX
            decline.push(self.emit_jcc_rel32(0x84)); // JZ: not cached
            self.buf.emit(&[0x48, 0x63, 0xC1]); // MOVSXD RAX, ECX
            to_store.push(self.emit_jmp_rel32());
        }
        // JIT round 21, lane hash (NATIVE21-1): the profiled class of the
        // identity arm, from this site's own inline cache seed.
        let profiled_cid = if identity_arm {
            node.bytecode_pc
                .and_then(|pc| self.ic_slots.get(&pc))
                .and_then(|&(mic, _)| identity_hash_profiled_class(&self.direct_helpers, mic))
                .filter(|&cid| Some(cid) != class_arm)
        } else {
            None
        };
        // JIT round 21, lane hash2: the per-VM class verdict table, when on.
        let class_table = if identity_arm {
            identity_hash_class_table(&self.direct_helpers)
        } else {
            None
        };
        // nio20 wave 5, lane L2: the `Class` arm (see `class_arm` above).
        // Reached from the `Integer` / `String` compares' misses; RAX is still
        // the non-null plain receiver on both.
        // JIT round 21, lane hash: the identity arm shares its header read.
        if class_arm.is_some() || identity_arm {
            if let Some(p) = not_integer.take() {
                self.patch_rel32_to_here(p);
            }
            if let Some(p) = not_string.take() {
                self.patch_rel32_to_here(p);
            }
            if crate::x64::dbg_jitc_enabled() {
                eprintln!(
                    "[cratonvm-jitc] identity-hash-inline (ir, Object.hashCode Class arm) n{id} pc={:?} class_id={class_arm:?} identity_arm={identity_arm} profiled={profiled_cid:?} class_table={}",
                    node.bytecode_pc,
                    class_table.is_some(),
                );
            }
            // The compares that select the read; the last one's miss declines.
            let mut to_read: Vec<usize> = Vec::new();
            if let Some(class_cid) = class_arm {
                // CMP DWORD [RAX+0], Class ; JE read (JNE decline when last).
                self.buf.emit(&[0x81, 0x78, 0x00]);
                self.buf.emit(&class_cid.to_le_bytes());
                if identity_arm {
                    to_read.push(self.emit_jcc_rel32(0x84));
                } else {
                    decline.push(self.emit_jcc_rel32(0x85));
                }
            }
            if identity_arm {
                if let Some(cid) = profiled_cid {
                    // CMP DWORD [RAX+0], profiled ; JE read.
                    self.buf.emit(&[0x81, 0x78, 0x00]);
                    self.buf.emit(&cid.to_le_bytes());
                    to_read.push(self.emit_jcc_rel32(0x84));
                }
                // CMP DWORD [RAX+0], 0 (`java/lang/Object`) ; JNE decline.
                self.buf.emit(&[0x81, 0x78, 0x00]);
                self.buf.emit(&0u32.to_le_bytes());
                if let Some(table) = class_table {
                    // JIT round 21, lane hash2: JE read; any other class
                    // reads its verdict byte (misses decline).
                    to_read.push(self.emit_jcc_rel32(0x84));
                    self.note_rcx_written();
                    let (bytes, edges) = identity_hash_class_table_test_bytes(table, 0);
                    let at = self.buf.pos();
                    self.buf.emit(&bytes);
                    decline.extend(edges.into_iter().map(|e| at + e));
                } else {
                    decline.push(self.emit_jcc_rel32(0x85));
                }
            }
            for p in to_read {
                self.patch_rel32_to_here(p);
            }
            self.emit_ir_identity_hash_read(&mut decline);
            to_store.push(self.emit_jmp_rel32());
        }
        // JIT round 21, lane hashlock (HASH2-21-1): the equals identity arm,
        // reached from the `Integer` compare's miss (or straight from the kind
        // test); RAX is still the non-null plain receiver.
        if equals_arm {
            if let Some(arg) = arg {
                if let Some(p) = not_integer.take() {
                    self.patch_rel32_to_here(p);
                }
                let eq_profiled = node
                    .bytecode_pc
                    .and_then(|pc| self.ic_slots.get(&pc))
                    .and_then(|&(mic, _)| {
                        identity_equals_profiled_class(&self.direct_helpers, mic)
                    });
                let eq_table = identity_equals_class_table(&self.direct_helpers);
                if crate::x64::dbg_jitc_enabled() {
                    eprintln!(
                        "[cratonvm-jitc] identity-equals-inline (ir, Object.equals) n{id} pc={:?} profiled={eq_profiled:?} class_table={}",
                        node.bytecode_pc,
                        eq_table.is_some(),
                    );
                }
                let mut to_answer: Vec<usize> = Vec::new();
                if let Some(cid) = eq_profiled {
                    // CMP DWORD [RAX+0], profiled ; JE answer.
                    self.buf.emit(&[0x81, 0x78, 0x00]);
                    self.buf.emit(&cid.to_le_bytes());
                    to_answer.push(self.emit_jcc_rel32(0x84));
                }
                // CMP DWORD [RAX+0], 0 (`java/lang/Object`).
                self.buf.emit(&[0x81, 0x78, 0x00]);
                self.buf.emit(&0u32.to_le_bytes());
                if let Some(table) = eq_table {
                    // JE answer; any other class reads its verdict byte
                    // (the identity-hash table's test bytes: same layout).
                    to_answer.push(self.emit_jcc_rel32(0x84));
                    self.note_rcx_written();
                    let (bytes, edges) = identity_hash_class_table_test_bytes(table, 0);
                    let at = self.buf.pos();
                    self.buf.emit(&bytes);
                    decline.extend(edges.into_iter().map(|e| at + e));
                } else {
                    // JNE decline.
                    decline.push(self.emit_jcc_rel32(0x85));
                }
                for p in to_answer {
                    self.patch_rel32_to_here(p);
                }
                // `Object.equals` is `this == obj`: R10 = argument (a null
                // one is never equal to the non-null receiver).
                self.gp_load_value(R10, arg);
                self.buf.emit(&[0x4C, 0x39, 0xD0]); // CMP RAX, R10
                self.buf.emit(&[0x0F, 0x94, 0xC0]); // SETE AL
                self.buf.emit(&[0x0F, 0xB6, 0xC0]); // MOVZX EAX, AL
                to_store.push(self.emit_jmp_rel32());
            }
        }
        if let Some(p) = array_recv {
            self.patch_rel32_to_here(p);
            if let Some(arg) = arg {
                self.gp_load_value(R10, arg);
                self.buf.emit(&[0x4C, 0x39, 0xD0]); // CMP RAX, R10
                self.buf.emit(&[0x0F, 0x94, 0xC0]); // SETE AL
                self.buf.emit(&[0x0F, 0xB6, 0xC0]); // MOVZX EAX, AL
                to_store.push(self.emit_jmp_rel32());
            } else {
                self.emit_ir_identity_hash_read(&mut decline);
                to_store.push(self.emit_jmp_rel32());
            }
        }
        if let Some(p) = not_string {
            decline.push(p);
        }
        if let Some(p) = not_integer {
            decline.push(p);
        }
        // The call route's `store_rax(slot)` at run time, written as the
        // ArrayList prefix writes it: the resident register when the value
        // has one, then the home word.
        for p in to_store {
            self.patch_rel32_to_here(p);
        }
        self.emit_prefix_result_store(id, slot);
        let hit = self.emit_jmp_rel32();
        for p in decline {
            self.patch_rel32_to_here(p);
        }
        self.call_prefix_join = Some(hit);
    }

    /// The `Integer` half's answer, emitted after the receiver's class compare
    /// with `this.value` in ECX (and RAX the receiver): `hashCode()` is the
    /// value; `equals(arg)` compares it with a plain `Integer` argument's,
    /// answers `false` for a null one, and declines for anything else.
    /// Split out of [`Self::try_emit_ir_box_hash_equals_prefix`] unchanged
    /// (round 11 wave 14) so the `String` arm can follow it.
    pub(in crate::ir_lower) fn emit_ir_box_hash_equals_integer_tail(
        &mut self,
        cid: u32,
        compact: i32,
        legacy: i32,
        arg: Option<NodeId>,
        decline: &mut Vec<usize>,
        to_store: &mut Vec<usize>,
    ) {
        match arg {
            None => {
                // hashCode(): the value, sign-extended (the IR's int
                // convention; the ArrayList prefix's `size()` ends the same way).
                self.buf.emit(&[0x48, 0x63, 0xC1]); // MOVSXD RAX, ECX
                to_store.push(self.emit_jmp_rel32());
            }
            Some(arg) => {
                // R10 = argument; `equals(null)` is false.
                self.gp_load_value(R10, arg);
                self.buf.emit(&[0x4D, 0x85, 0xD2]); // TEST R10, R10
                let arg_null = self.emit_jcc_rel32(0x84);
                // TEST BYTE [R10+KIND_TAGS], KIND_TAG_BYTE_MASK (41 F6 42 disp8 ib).
                self.buf.emit(&[
                    0x41,
                    0xF6,
                    0x42,
                    cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8, // Cast: x86-64 disp8
                    cratonvm_types::KIND_TAG_BYTE_MASK,
                ]);
                decline.push(self.emit_jcc_rel32(0x85));
                // CMP DWORD [R10+0], Integer (41 81 7A 00 id).
                self.buf.emit(&[0x41, 0x81, 0x7A, 0x00]);
                self.buf.emit(&cid.to_le_bytes());
                decline.push(self.emit_jcc_rel32(0x85));
                // TEST BYTE [R10+GC_FLAGS], COMPACT (41 F6 42 disp8 ib).
                self.buf.emit(&[
                    0x41,
                    0xF6,
                    0x42,
                    cratonvm_types::GC_FLAGS_BYTE_OFFSET as u8, // Cast: x86-64 disp8
                    cratonvm_types::GC_FLAG_COMPACT,
                ]);
                let arg_legacy = self.emit_jcc_rel32(0x84);
                self.emit_load_base_disp32(RDX, R10, compact, false); // EDX = arg.value
                let arg_have = self.emit_jmp_rel32();
                self.patch_rel32_to_here(arg_legacy);
                self.emit_load_base_disp32(RDX, R10, legacy, false);
                self.patch_rel32_to_here(arg_have);
                self.buf.emit(&[0x39, 0xD1]); // CMP ECX, EDX
                self.buf.emit(&[0x0F, 0x94, 0xC0]); // SETE AL
                self.buf.emit(&[0x0F, 0xB6, 0xC0]); // MOVZX EAX, AL
                to_store.push(self.emit_jmp_rel32());
                self.patch_rel32_to_here(arg_null);
                self.buf.emit(&[0x31, 0xC0]); // XOR EAX, EAX
                to_store.push(self.emit_jmp_rel32());
            }
        }
    }

    /// INTEGER_VALUE_OF_INLINE_BOX in the optimizing tier (round 11 wave 17,
    /// lane hashmap; proposal W15-1 in `docs/feature-designs/jit-ir-optimizations.md`):
    /// allocate `Integer.valueOf(int)`'s box INLINE when the value is outside
    /// the box cache, as a guarded PREFIX of the call's unchanged lowering.
    ///
    /// # Why
    ///
    /// Every boxing loop pays one `jit_integer_value_of_direct` Rust call per
    /// box: `CratonBench hashmap` makes three per put+get pair (30 M), and
    /// every value from 128 up allocates. That helper is memoized as far as a
    /// helper can be (`r11-rt-helper-hot-paths-repeat-work-per-call` item 1),
    /// so what is left is the call itself -- the panic guard, the boundary and
    /// SATB bookkeeping, six thread-local memo reads, the Rust TLAB allocator
    /// and the pending-return handoff. HotSpot allocates the box inline.
    ///
    /// # What the fast path answers, and why it is exactly the helper's answer
    ///
    /// Only a site that this compile already binds to the thin helper
    /// (`direct_calls[pc] == direct_helpers.integer_value_of`) gets the
    /// prefix, so the prefix only ever replaces that helper, never a real-JDK
    /// `valueOf` body. For a value `v < -128 || v > high` the helper allocates
    /// a FRESH `java/lang/Integer` with `value = v` (JLS 5.1.7 requires
    /// identity only inside the cache), and so does the hit path: the same
    /// inline TLAB bump `Op::New` takes (`emit_inline_tlab_fast_path_ir`,
    /// header written before the cursor commits), then the 4-byte `value`
    /// store. `-128..=high` declines to the call, which answers from the
    /// NATIVE box table, so `valueOf(5) == valueOf(5)` is unchanged.
    ///
    /// `high` is the `IntegerCache.high` this VM's native resolved
    /// ([`WELL_KNOWN_INTEGER_CACHE_HIGH`]). It is fixed once resolved
    /// (`lang_math::integer_cache_bound` files it once per VM), so baking it
    /// is sound; an unresolved bound answers nothing and the site keeps the
    /// call.
    ///
    /// # Why the hit path owes no map, no frame republish and no reload
    ///
    /// It makes no call and cannot collect: the bump either fits the TLAB or
    /// declines to the call before anything is written. Like `Op::New`'s
    /// sunk-map fast path, it skips the call route's safepoint map, and a
    /// frame-block publication it skipped is made by the next safepoint that
    /// needs it. It cannot trap either (no null check, no exception), so a
    /// splice around it has nothing to re-execute.
    ///
    /// # Screens (each declines to the unchanged call)
    ///
    /// Compile time: the kill switches (`CRATONVM_JIT_IR_VALUEOF_INLINE_BOX=0`,
    /// `CRATONVM_JIT_NO_BOX_UNBOX_INTRINSIC=1`, and the inline bump's own
    /// `CRATONVM_JIT_IR_INLINE_TLAB=0` / `CRATONVM_JIT_IR_TLAB_SKIP_POST_INIT=0`);
    /// under ZGC, an unarmed inline announce ([`box_tlab_admitted`]: since al16
    /// the box is announced inline after the bump, or the bump is undone and
    /// the call allocates); `Integer` unresolvable, ambiguous or not initialised;
    /// no compact layout of exactly one 4-byte `Int` field (`Integer` has no
    /// finalizer and no `long`/`float`/`double` field, which is the
    /// `skip_post_init` condition `lib.rs` checks for an `Op::New` class); an
    /// operand or result that a carry holds or whose home is dropped.
    /// Run time: the bump's own layout-replacement guard, a thread with no
    /// cached `JvmThread`, a full TLAB, and a fresh object that is not compact
    /// (it is left behind as unreachable garbage with a complete header, and
    /// the call allocates the answer).
    ///
    /// Scratch: RAX, RCX, R10 and R11, the prefixes' set (the bump writes RAX,
    /// R10 and R11 only, so ECX still holds the value when it is stored).
    ///
    /// # The cached range (JIT round 16, lane callbind)
    ///
    /// Under `CRATONVM_JIT_IR_VALUEOF_CACHE_READ=1` (default off) the prefix
    /// first answers a value INSIDE the cache from the VM's `Integer` box table
    /// ([`integer_cache_read_bytes`]): the slot `jit_integer_value_of_direct`'s
    /// `integer_cache_peek` loads, so the answer is the helper's canonical
    /// box. An empty slot declines to the call (which fills it); a value
    /// outside the table goes on to the allocation arm above, or to the call
    /// when that arm is off or refused. The table is leaked and never resized
    /// (`lang_math::integer_cache_slots_raw`), so its address and length are
    /// baked. The hit path is one load: no call, no allocation, no safepoint,
    /// nothing to trap. HotSpot reads `IntegerCache.cache` inline the same way;
    /// here the canonical boxes live in the native's table, not that array.
    pub(in crate::ir_lower) fn try_emit_ir_integer_value_of_prefix(
        &mut self,
        id: NodeId,
        info_ptr: usize,
    ) {
        if info_ptr == 0 || crate::box_unbox_intrinsic_disabled() {
            return;
        }
        let alloc_enabled = true;
        let cache_read_enabled = ir_integer_value_of_cache_read_enabled();
        if !alloc_enabled && !cache_read_enabled {
            return;
        }
        // SAFETY: as every other read of an `Op::Call`'s `info_ptr` in this
        // file -- a live `JitInvokeInfo` owned by `ir_call_infos`.
        let info: &JitInvokeInfo = unsafe { &*(info_ptr as *const JitInvokeInfo) };
        if info.invoke_kind != 3
            || info.class_name != "java/lang/Integer"
            || info.method_name != "valueOf"
            || info.descriptor != "(I)Ljava/lang/Integer;"
        {
            return;
        }
        let graph = self.graph;
        let Some(node) = graph.node_opt(id) else {
            return;
        };
        // [ctrl, mem, value].
        if node.inputs.len() != 3 || node.ty != IrType::Ref {
            return;
        }
        // Only a site bound to the thin helper: the prefix replaces THAT
        // helper's out-of-cache arm and nothing else.
        let helper = self.direct_helpers.integer_value_of;
        let bound_to_helper = helper != 0
            && node
                .bytecode_pc
                .and_then(|pc| self.direct_calls.get(&pc))
                .is_none_or(|&(entry, _)| entry == helper);
        if !bound_to_helper {
            return;
        }
        let arg = node.inputs[2];
        if graph.node_opt(arg).map(|n| n.ty) != Some(IrType::Int) || self.carry_names(arg) {
            return;
        }
        let Some(slot) = self.prefix_result_slot(id, node.ty) else {
            return;
        };
        let cache = if cache_read_enabled {
            ir_integer_cache_slots(&self.direct_helpers)
        } else {
            None
        };
        let alloc = if alloc_enabled {
            self.integer_value_of_alloc_plan()
        } else {
            None
        };
        if alloc.is_none() && cache.is_none() {
            return;
        }
        if crate::ir_stage_reporting() {
            if let Some((cid, high, _, total, value_off)) = alloc {
                eprintln!(
                    "[ir] integer-valueof-inline-box n{id} integer_class_id={cid} high={high} \
                     value_off={value_off} total={total}"
                );
            }
            if let Some((_, len)) = cache {
                eprintln!("[ir] integer-valueof-cache-read n{id} slots={len}");
            }
        }
        let mut decline: Vec<usize> = Vec::new();
        // ECX = the value. It survives the bump, which writes RAX, R10, R11,
        // and the cache read, which writes RAX and R10.
        self.note_rcx_written();
        self.gp_load_value(RCX, arg);
        // The cached range: on a filled slot RAX is the canonical box.
        let mut cache_hit: Option<usize> = None;
        if let Some((base, len)) = cache {
            let (bytes, outside, empty) = integer_cache_read_bytes(base, len);
            let at = self.buf.pos();
            self.buf.emit(&bytes);
            decline.push(at + empty);
            if alloc.is_none() {
                // Nothing else to try: the hit stores here, and a value
                // outside the table takes the call with the empty slots.
                decline.push(at + outside);
                self.emit_integer_value_of_result_store(id, slot);
                let hit = self.emit_jmp_rel32();
                for p in decline {
                    self.patch_rel32_to_here(p);
                }
                self.call_prefix_join = Some(hit);
                return;
            }
            cache_hit = Some(self.emit_jmp_rel32());
            self.patch_rel32_to_here(at + outside);
        }
        let Some((cid, high, replace_count_before, _, value_off)) = alloc else {
            return;
        };
        // CMP ECX, -128 (83 /7 ib) ; JL allocate.
        self.buf.emit(&[0x83, 0xF9, 0x80]);
        let below_cache = self.emit_jcc_rel32(0x8C);
        // CMP ECX, high (81 /7 id) ; JLE decline -- inside the cache.
        self.buf.emit(&[0x81, 0xF9]);
        self.buf.emit(&high.to_le_bytes());
        decline.push(self.emit_jcc_rel32(0x8E));
        self.patch_rel32_to_here(below_cache);
        let plan = crate::runtime_lowering::InlineTlabPlan {
            thread_slot_off: self.shadow_thread_slot_off,
            thread_tls_disp: self.ir_tlab_thread_tls_disp(),
            cursor_off: self.tlab_cursor_off,
            end_off: self.tlab_end_off,
            post_init: self.tlab_post_init,
            context_off: self.context_slot_off,
            class_id: cid,
            num_fields: 1,
            announce: 0,
            skip_post_init: true,
            register_first: 0,
            post_init_helper_first: false,
        };
        let mut slow: Vec<usize> = Vec::new();
        // al16: under ZGC the box is announced inline, or the call allocates
        // (see `box_tlab_admitted`). nio21 wave 10 L1: through
        // `emit_inline_tlab_fast_path_announced_ir`, which sets the start bit
        // BEFORE the cursor commits (`CRATONVM_ZGC_JIT_START_BIT_BEFORE_COMMIT`;
        // `=0` is the al16 commit-then-announce-or-rollback pair).
        let bumped = if cratonvm_types::jit_tlab_registration_required() {
            crate::runtime_lowering::emit_inline_tlab_fast_path_announced_ir(
                &mut self.buf,
                &plan,
                self.new_object,
                &mut slow,
            )
        } else {
            crate::runtime_lowering::emit_inline_tlab_fast_path_ir(
                &mut self.buf,
                &plan,
                self.new_object,
                &mut slow,
            )
        };
        decline.extend(slow);
        let (_, replace_count_after) = cratonvm_types::layout_replace_guard(cid);
        if bumped && replace_count_after == replace_count_before {
            // TEST BYTE [RAX+GC_FLAGS], COMPACT (F6 /0 ib) ; JZ decline: the
            // bump decided the shape itself, and only a compact body has the
            // 4-byte cell at `value_off`.
            self.buf.emit(&[
                0xF6,
                0x40,
                cratonvm_types::GC_FLAGS_BYTE_OFFSET as u8, // Cast: x86-64 disp8
                cratonvm_types::GC_FLAG_COMPACT,
            ]);
            decline.push(self.emit_jcc_rel32(0x84));
            // MOV DWORD [RAX+disp32], ECX (89 /r, mod=10 reg=ECX rm=RAX). A
            // primitive store into a fresh object: no barrier.
            self.buf.emit(&[0x89, 0x88]);
            self.buf.emit(&value_off.to_le_bytes());
            // A cache hit joins the allocation's result store.
            if let Some(p) = cache_hit {
                self.patch_rel32_to_here(p);
            }
            self.emit_integer_value_of_result_store(id, slot);
            let hit = self.emit_jmp_rel32();
            for p in decline {
                self.patch_rel32_to_here(p);
            }
            self.call_prefix_join = Some(hit);
            return;
        }
        // The bump refused (nothing was emitted), or the layout was replaced
        // while this site was being emitted: every edge, including falling
        // off the end, lands on the unchanged call -- except a cache hit,
        // which still has its box in RAX and gets its own result store.
        if let Some(p) = cache_hit {
            let to_call = self.emit_jmp_rel32();
            self.patch_rel32_to_here(p);
            self.emit_integer_value_of_result_store(id, slot);
            let hit = self.emit_jmp_rel32();
            self.patch_rel32_to_here(to_call);
            for p in decline {
                self.patch_rel32_to_here(p);
            }
            self.call_prefix_join = Some(hit);
            return;
        }
        for p in decline {
            self.patch_rel32_to_here(p);
        }
    }

    /// The allocation arm's compile-time admission for
    /// [`Self::try_emit_ir_integer_value_of_prefix`], asked up front so nothing
    /// is emitted for a site it would refuse (`emit_inline_tlab_new_ir_impl`'s
    /// declines, plus the `skip_post_init` conditions of `Op::New`):
    /// `(Integer's class id, IntegerCache.high, the layout replacement count
    /// read BEFORE the layout, the compact total size, value's offset)`.
    /// Extracted unchanged from that function by JIT round 16 (lane callbind),
    /// so the cached-range read can run where this arm declines.
    pub(in crate::ir_lower) fn integer_value_of_alloc_plan(
        &self,
    ) -> Option<(u32, i32, u32, usize, i32)> {
        if !ir_inline_tlab_enabled()
            || !box_tlab_admitted()
            || self.shadow_thread_slot_off <= 0
            || self.tlab_post_init == 0
            || self.new_object == 0
            || self.tlab_cursor_off == self.tlab_end_off
        {
            return None;
        }
        let cid = self
            .direct_helpers
            .integer_class_id()
            .filter(|&cid| self.direct_helpers.class_is_initialized(cid))?;
        let high = ir_integer_cache_high(&self.direct_helpers)?;
        // The replacement count BEFORE the layout is read: if it moves while
        // this site is emitted, the bump's guard may have baked a count newer
        // than the offset below, and the prefix is abandoned (see the caller).
        let (_, replace_count_before) = cratonvm_types::layout_replace_guard(cid);
        let total = cratonvm_types::compact_tlab_total_size(cid, 1)?;
        let value_off = match cratonvm_types::compact_field_disp_storage(cid, 0) {
            Some((off, cratonvm_types::FieldStorageKind::Int)) => off,
            _ => return None,
        };
        if total % 8 != 0 || value_off < 8 || value_off.saturating_add(4) > total {
            return None;
        }
        let value_off = i32::try_from(value_off).ok()?;
        Some((cid, high, replace_count_before, total, value_off))
    }

    /// The call route's `store_rax(slot)` at run time, written as the other
    /// prefixes write it: RAX into the node's register, if it has one, and
    /// into its optional frame home.
    pub(in crate::ir_lower) fn emit_integer_value_of_result_store(
        &mut self,
        id: NodeId,
        slot: Option<i32>,
    ) {
        self.emit_prefix_result_store(id, slot);
    }

    /// LONG_VALUE_OF_INLINE_BOX (round 12 wave 2, lane `calls`; proposal C12-2
    /// in `docs/feature-designs/profile-guided-inlining.md`): the `Long.valueOf(long)` twin of
    /// [`Self::try_emit_ir_integer_value_of_prefix`], on the same terms and
    /// with the same screens, replacing only the out-of-cache arm of the thin
    /// `jit_long_value_of_direct` helper a site is bound to.
    ///
    /// `Long`'s box cache is the fixed `-128..=127` (`Long.valueOf` has no
    /// `high` property, and the helper tests exactly that range), so a value
    /// outside it gets a FRESH `java/lang/Long` inline and every value inside
    /// it declines to the call, which answers from the box table: identity is
    /// unchanged. The comparisons are 64-bit, and the store is the 8-byte
    /// compact `value` cell (aligned; a layout that does not align it declines).
    ///
    /// Extra kill switch: `CRATONVM_JIT_LONG_VALUEOF_INLINE_BOX=0` (both tiers);
    /// `CRATONVM_JIT_IR_VALUEOF_INLINE_BOX=0` turns this tier's pair off.
    ///
    /// # The cached range (JIT round 16 wave 7, lane callbind, CB16-3)
    ///
    /// Under `CRATONVM_JIT_IR_LONG_VALUEOF_CACHE_READ` (default on since hga) the
    /// prefix first answers a value inside `-128..=127` from the VM's `Long` box
    /// table ([`long_cache_read_bytes`]), the slot `long_cache_peek` loads, as
    /// the `Integer` twin does: an empty slot takes the call (which fills it),
    /// a value outside goes on to the allocation arm, or to the call when that
    /// arm is off or refused. With the switch off the emitted bytes are the
    /// allocation-only prefix's, unchanged.
    pub(in crate::ir_lower) fn try_emit_ir_long_value_of_prefix(
        &mut self,
        id: NodeId,
        info_ptr: usize,
    ) {
        if info_ptr == 0 || crate::box_unbox_intrinsic_disabled() {
            return;
        }
        let alloc_enabled = long_value_of_inline_box_enabled();
        let cache_read_enabled = true;
        if !alloc_enabled && !cache_read_enabled {
            return;
        }
        // SAFETY: as every other read of an `Op::Call`'s `info_ptr` in this
        // file -- a live `JitInvokeInfo` owned by `ir_call_infos`.
        let info: &JitInvokeInfo = unsafe { &*(info_ptr as *const JitInvokeInfo) };
        if info.invoke_kind != 3
            || info.class_name != "java/lang/Long"
            || info.method_name != "valueOf"
            || info.descriptor != "(J)Ljava/lang/Long;"
        {
            return;
        }
        let graph = self.graph;
        let Some(node) = graph.node_opt(id) else {
            return;
        };
        // [ctrl, mem, value].
        if node.inputs.len() != 3 || node.ty != IrType::Ref {
            return;
        }
        let helper = self.direct_helpers.long_value_of;
        let bound_to_helper = helper != 0
            && node
                .bytecode_pc
                .and_then(|pc| self.direct_calls.get(&pc))
                .is_none_or(|&(entry, _)| entry == helper);
        if !bound_to_helper {
            return;
        }
        let Some(&arg) = node.inputs.get(2) else {
            return;
        };
        if graph.node_opt(arg).map(|n| n.ty) != Some(IrType::Long) || self.carry_names(arg) {
            return;
        }
        let Some(slot) = self.prefix_result_slot(id, node.ty) else {
            return;
        };
        let cache = if cache_read_enabled {
            ir_long_cache_slots(&self.direct_helpers)
        } else {
            None
        };
        let alloc = if alloc_enabled {
            self.long_value_of_alloc_plan()
        } else {
            None
        };
        if alloc.is_none() && cache.is_none() {
            return;
        }
        if crate::ir_stage_reporting() {
            if let Some((cid, _, total, value_off)) = alloc {
                eprintln!(
                    "[ir] long-valueof-inline-box n{id} long_class_id={cid} value_off={value_off} \
                     total={total}"
                );
            }
            if cache.is_some() {
                eprintln!("[ir] long-valueof-cache-read n{id} slots=256");
            }
        }
        let mut decline: Vec<usize> = Vec::new();
        // RCX = the value, all 64 bits. It survives the bump (RAX, R10, R11)
        // and the cache read (RAX, R10).
        self.note_rcx_written();
        self.gp_load_value(RCX, arg);
        // The cached range: on a filled slot RAX is the canonical box.
        let mut cache_hit: Option<usize> = None;
        if let Some(base) = cache {
            let (bytes, outside, empty) = long_cache_read_bytes(base);
            let at = self.buf.pos();
            self.buf.emit(&bytes);
            decline.push(at + empty);
            if alloc.is_none() {
                // Nothing else to try: the hit stores here, and a value
                // outside the table takes the call with the empty slots.
                decline.push(at + outside);
                self.emit_integer_value_of_result_store(id, slot);
                let hit = self.emit_jmp_rel32();
                for p in decline {
                    self.patch_rel32_to_here(p);
                }
                self.call_prefix_join = Some(hit);
                return;
            }
            cache_hit = Some(self.emit_jmp_rel32());
            self.patch_rel32_to_here(at + outside);
        }
        let Some((cid, replace_count_before, _, value_off)) = alloc else {
            return;
        };
        // CMP RCX, -128 (REX.W 83 /7 ib) ; JL allocate.
        self.buf.emit(&[0x48, 0x83, 0xF9, 0x80]);
        let below_cache = self.emit_jcc_rel32(0x8C);
        // CMP RCX, 127 (REX.W 83 /7 ib) ; JLE decline -- inside the cache.
        self.buf.emit(&[0x48, 0x83, 0xF9, 0x7F]);
        decline.push(self.emit_jcc_rel32(0x8E));
        self.patch_rel32_to_here(below_cache);
        let plan = crate::runtime_lowering::InlineTlabPlan {
            thread_slot_off: self.shadow_thread_slot_off,
            thread_tls_disp: self.ir_tlab_thread_tls_disp(),
            cursor_off: self.tlab_cursor_off,
            end_off: self.tlab_end_off,
            post_init: self.tlab_post_init,
            context_off: self.context_slot_off,
            class_id: cid,
            num_fields: 1,
            announce: 0,
            skip_post_init: true,
            register_first: 0,
            post_init_helper_first: false,
        };
        let mut slow: Vec<usize> = Vec::new();
        // al16 / nio21 wave 10 L1: as the `Integer` prefix above -- under ZGC
        // the start bit is set before the cursor commits, or the call
        // allocates.
        let bumped = if cratonvm_types::jit_tlab_registration_required() {
            crate::runtime_lowering::emit_inline_tlab_fast_path_announced_ir(
                &mut self.buf,
                &plan,
                self.new_object,
                &mut slow,
            )
        } else {
            crate::runtime_lowering::emit_inline_tlab_fast_path_ir(
                &mut self.buf,
                &plan,
                self.new_object,
                &mut slow,
            )
        };
        decline.extend(slow);
        let (_, replace_count_after) = cratonvm_types::layout_replace_guard(cid);
        if bumped && replace_count_after == replace_count_before {
            // TEST BYTE [RAX+GC_FLAGS], COMPACT ; JZ decline.
            self.buf.emit(&[
                0xF6,
                0x40,
                cratonvm_types::GC_FLAGS_BYTE_OFFSET as u8, // Cast: x86-64 disp8
                cratonvm_types::GC_FLAG_COMPACT,
            ]);
            decline.push(self.emit_jcc_rel32(0x84));
            // MOV QWORD [RAX+disp32], RCX (REX.W 89 /r, mod=10 reg=RCX rm=RAX).
            // A primitive store into a fresh object: no barrier.
            self.buf.emit(&[0x48, 0x89, 0x88]);
            self.buf.emit(&value_off.to_le_bytes());
            // A cache hit joins the allocation's result store (the same
            // bytes the allocation-only prefix wrote here).
            if let Some(p) = cache_hit {
                self.patch_rel32_to_here(p);
            }
            self.emit_integer_value_of_result_store(id, slot);
            let hit = self.emit_jmp_rel32();
            for p in decline {
                self.patch_rel32_to_here(p);
            }
            self.call_prefix_join = Some(hit);
            return;
        }
        // The bump refused (nothing was emitted), or the layout was replaced
        // while this site was being emitted: every edge lands on the unchanged
        // call, except a cache hit, which has its box in RAX.
        if let Some(p) = cache_hit {
            let to_call = self.emit_jmp_rel32();
            self.patch_rel32_to_here(p);
            self.emit_integer_value_of_result_store(id, slot);
            let hit = self.emit_jmp_rel32();
            self.patch_rel32_to_here(to_call);
            for p in decline {
                self.patch_rel32_to_here(p);
            }
            self.call_prefix_join = Some(hit);
            return;
        }
        for p in decline {
            self.patch_rel32_to_here(p);
        }
    }

    /// The allocation arm's compile-time admission for
    /// [`Self::try_emit_ir_long_value_of_prefix`]: `(Long's class id, the
    /// layout replacement count read BEFORE the layout, the compact total
    /// size, value's offset)`. Extracted unchanged from that function by JIT
    /// round 16 wave 7 (lane callbind), so the cached-range read can run where
    /// this arm declines (ZGC's TLAB registration, for one).
    pub(in crate::ir_lower) fn long_value_of_alloc_plan(&self) -> Option<(u32, u32, usize, i32)> {
        if !ir_inline_tlab_enabled()
            || !box_tlab_admitted()
            || self.shadow_thread_slot_off <= 0
            || self.tlab_post_init == 0
            || self.new_object == 0
            || self.tlab_cursor_off == self.tlab_end_off
        {
            return None;
        }
        let cid = ir_long_class_id(&self.direct_helpers)
            .filter(|&cid| self.direct_helpers.class_is_initialized(cid))?;
        let (_, replace_count_before) = cratonvm_types::layout_replace_guard(cid);
        let total = cratonvm_types::compact_tlab_total_size(cid, 1)?;
        let value_off = match cratonvm_types::compact_field_disp_storage(cid, 0) {
            Some((off, cratonvm_types::FieldStorageKind::Long)) => off,
            _ => return None,
        };
        if total % 8 != 0
            || value_off < 8
            || value_off % 8 != 0
            || value_off.saturating_add(8) > total
        {
            return None;
        }
        let value_off = i32::try_from(value_off).ok()?;
        Some((cid, replace_count_before, total, value_off))
    }

    /// `CMP dword [rip+disp32], imm32` against the replacement epoch — the
    /// whole guard in one 10-byte instruction, reporting whether the counter
    /// was within ±2GB RIP reach of it.
    ///
    /// Replaces the far form `MOV R11, imm64` + `CMP dword [R11], imm32`: two
    /// instructions and 17 bytes become one and ten, without clobbering R11 or
    /// RCX. The guard is emitted once per INLINE FIELD ACCESS SITE and executed
    /// on every one of them, so a loop containing a `getfield` paid both every
    /// iteration — 4 of the ~27 instructions in
    /// `probes/FieldLoop.java`'s loop body, for a counter only a layout
    /// REPLACEMENT bumps.
    ///
    /// `81 /7 id` with ModRM `mod=00, rm=101` is the RIP-relative form
    /// (`0x3D`), and the displacement is measured from the end of the WHOLE
    /// instruction — past the trailing `imm32`, which is why the reach test
    /// adds 10 and not 6. Both compare forms perform one aligned 32-bit read.
    ///
    /// Sound here **for free**: this lowerer never duplicates emitted bytes to
    /// a second address, so a displacement that is right when emitted stays
    /// right — the reason `emit_test_safepoint_flag_rip` gives two hundred
    /// lines below. `x64`'s twin sits inside a body its native unroller
    /// byte-copies and needs the fixup pass to earn the same shape; it has one
    /// (`rip_abs_disp32_patches`, declaring a 4-byte trail for the `imm32`)
    /// and emits the same instruction since 2026-09-10.
    pub(in crate::ir_lower) fn emit_cmp_layout_epoch_rip(
        &mut self,
        addr: usize,
        expected: u32,
    ) -> bool {
        if !ir_epoch_guard_rip_enabled() {
            return false;
        }
        // 81 3D <disp32> <imm32>
        const LEN: usize = 10;
        // Cast: buffer base plus a non-negative offset.
        let here = self.buf.as_ptr() as usize + self.buf.pos();
        let Some(disp) = rip_disp32(here, LEN, addr) else {
            return false;
        };
        self.buf.emit(&[0x81, 0x3D]);
        self.buf.emit(&disp.to_le_bytes());
        self.buf.emit(&(expected as i32).to_le_bytes()); // Cast: the baked epoch
        true
    }

    /// zs s3/bt: the fused reference-store gate -- `CMP word [word], 0; JE` --
    /// in front of a gated store's full gate sequence. Returns the `JE` patch,
    /// which the caller routes to its barrier-free store; the fall-through is
    /// the unchanged sequence. `word` comes from
    /// [`crate::x64::zgc_fused_ref_store_gate_of`] (its doc is the soundness
    /// argument). RIP-relative when the word is within ±2GB of this
    /// instruction, else through R11. Clobbers R11 (fallback form) and the
    /// flags only.
    pub(in crate::ir_lower) fn emit_zgc_fused_ref_store_gate(&mut self, word: usize) -> usize {
        // Cast: buffer base plus a non-negative offset.
        let here = self.buf.as_ptr() as usize + self.buf.pos();
        let bytes = zgc_fused_gate_cmp_bytes(here, word);
        self.buf.emit(&bytes);
        self.emit_jcc_rel32(0x84) // JE -> the barrier-free store
    }

    /// Receiver alignment + containment in one of the three published
    /// `JIT_READ_BOUNDS` regions, with RAX holding the receiver. Failures push
    /// their patch offsets onto `slow`.
    ///
    /// Shared by the inline `getfield` read and the gated reference `putfield`
    /// below, deliberately: both answer the same question — "is this address
    /// one this heap handed out, so the header reads cannot fault" — and a
    /// second transcription of six compares against a six-word table is a place
    /// for the two to disagree about a region index.
    ///
    /// Clobbers RCX and RDX. A caller that needs the flags byte in CL must read
    /// it AFTER this.
    pub(in crate::ir_lower) fn emit_receiver_align_and_containment(
        &mut self,
        slow: &mut Vec<usize>,
    ) {
        self.emit_receiver_align_and_containment_at(slow, RAX);
    }

    pub(in crate::ir_lower) fn emit_receiver_align_and_containment_at(
        &mut self,
        slow: &mut Vec<usize>,
        receiver: u8,
    ) {
        // alignment: the low three bits must be clear.
        self.emit_mov_reg_reg64_raw(RCX, receiver);
        self.buf.emit(&[0x48, 0x83, 0xE1, 0x07]); // AND RCX, 7
        slow.push(self.emit_jcc_rel32(0x85)); // JNZ
                                              // containment in one of the three published regions.
                                              // RDX = &JIT_READ_BOUNDS = [b0, e0, b1, e1, b2, e2].
        self.emit_mov_reg_imm64(RDX, self.read_bounds_addr as u64);
        self.emit_cmp_receiver_mem_rdx(receiver, 0);
        let below_b0 = self.emit_jcc_rel32(0x82); // JB -> try region 1
        self.emit_cmp_receiver_mem_rdx(receiver, 8);
        let ok0 = self.emit_jcc_rel32(0x82); // JB -> inside region 0
        self.patch_rel32_to_here(below_b0);
        self.emit_cmp_receiver_mem_rdx(receiver, 16);
        let below_b1 = self.emit_jcc_rel32(0x82); // JB -> try region 2
        self.emit_cmp_receiver_mem_rdx(receiver, 24);
        let ok1 = self.emit_jcc_rel32(0x82); // JB -> inside region 1
        self.patch_rel32_to_here(below_b1);
        self.emit_cmp_receiver_mem_rdx(receiver, 32);
        slow.push(self.emit_jcc_rel32(0x82)); // JB -> slow
        self.emit_cmp_receiver_mem_rdx(receiver, 40);
        slow.push(self.emit_jcc_rel32(0x83)); // JAE -> slow
        self.patch_rel32_to_here(ok0);
        self.patch_rel32_to_here(ok1);
    }

    /// Round 16 (lane fieldguard, FG16-6): the reference-load half of
    /// [`Self::emit_receiver_align_and_containment`], for a receiver the IR
    /// types `Ref`. `CMP qword [JIT_READ_BOUNDS + 8], 0; JE slow`: region 0's
    /// end word is zero exactly when the table is cleared.
    ///
    /// # Why this check survives when the containment does not
    ///
    /// For the RECEIVER, the containment is redundant under the `Ref` type
    /// proof, exactly as the primitive trusted-oop arm argues (a value the IR
    /// types as an object reference is an oop, or an earlier JIT/GC failure
    /// already happened). But for a reference LOAD the same compares also gate
    /// the VALUE: the collectors clear `JIT_READ_BOUNDS` whenever reference
    /// slots may stop holding plain pointers -- ZGC's `set_barrier_color` on
    /// arming the read barrier, ZGC's one-way withdrawal once its arena has
    /// decommitted holes, and every heap teardown -- and a cleared table makes
    /// every containment fail, so ALREADY-COMPILED inline reference reads fall
    /// to the barriered helper (`x64::licm::zgc_codegen_honours_read_barrier`,
    /// mechanism 1). Dropping the compares without this check would silently
    /// remove that mechanism. Every publisher writes slot 0 when the table is
    /// live (`gen_heap` slots 0..2 from its regions, G1 and ZGC slot 0), and
    /// every clear zeroes slot 0, so slot 0's end word decides. The same
    /// arm-at-a-safepoint obligation `set_barrier_color` states for the
    /// containment applies unchanged.
    ///
    /// Clobbers R11 only; RAX (the receiver) is untouched.
    /// Compare the live end word with zero. Absolute disp32 is sign-extended
    /// on x86-64, so use it only for a positive address representable as i32.
    /// Keep the full-address form for all other placements.
    pub(in crate::ir_lower) fn emit_cmp_read_bounds_live(&mut self) {
        if let Some(addr) = self
            .read_bounds_addr
            .checked_add(8)
            .and_then(|addr| i32::try_from(addr).ok())
        {
            self.buf.emit(&[0x48, 0x83, 0x3c, 0x25]); // CMP qword [disp32], 0
            self.buf.emit(&addr.to_le_bytes());
            self.buf.emit_byte(0);
        } else {
            self.emit_mov_reg_imm64(R11, self.read_bounds_addr as u64);
            self.buf.emit(&[0x49, 0x83, 0x7B, 0x08, 0x00]); // CMP qword [R11 + 8], 0
        }
    }

    pub(in crate::ir_lower) fn emit_read_bounds_live_check(&mut self, slow: &mut Vec<usize>) {
        if self.block_cse_read_bounds_live_checked() {
            return;
        }
        self.emit_cmp_read_bounds_live();
        slow.push(self.emit_jcc_rel32(0x84)); // JE -> slow: the table is cleared
    }

    /// JIT r23 wave 4 stage 1 (lane L8): [`Self::emit_read_bounds_live_check`]
    /// for [`crate::ir::UnboxOp::FieldRef`], which has no helper to fall back
    /// to: a cleared `JIT_READ_BOUNDS` (ZGC arming its read barrier, ZGC's
    /// withdrawal, a heap teardown) DEOPTS, and the interpreter re-executes
    /// the read through the barriered path. Under G1 and Gen the table stays
    /// live and this is one compare that never fires; the read itself is the
    /// plain `MOV` those collectors owe. Clobbers R11; RAX is untouched.
    pub(in crate::ir_lower) fn emit_ffm_element_read_bounds_live_deopt(&mut self, bci: usize) {
        if self.block_cse_read_bounds_live_checked() {
            return;
        }
        self.emit_cmp_read_bounds_live();
        self.emit_deopt_unless(0x85, bci, DeoptReason::ReceiverTypeChanged); // JNE: live
        self.block_cse_mark_read_bounds_live_checked();
    }

    /// JIT r23 wave 4 stage 1 (lane L8): one arm of
    /// [`crate::ir::UnboxOp::FieldBool`] / [`crate::ir::UnboxOp::FieldRef`],
    /// with the receiver in RAX and the answer left in RAX.
    ///
    /// * `FieldBool`, compact: `MOVZX EAX, BYTE [RAX + disp]` -- the 1-byte
    ///   storage, zero-extended, so the bytes after it (the next field) never
    ///   reach the `Int` result. Legacy: `MOV EAX, [RAX + disp]`, the `Int`
    ///   cell's 32-bit payload.
    /// * `FieldRef`, compact: `MOV RAX, [RAX + disp]`, the bare pointer.
    ///   Legacy: the cell's tag must say `Object` (`Object(None)` keeps the tag
    ///   with a zero payload) or the read deopts, as `lower_scoped_raw`'s owner
    ///   cell does -- a cell holding another kind would hand back its payload
    ///   as a wild pointer -- then `MOV RAX, [RAX + disp]`.
    pub(in crate::ir_lower) fn emit_ffm_element_read_access(
        &mut self,
        uop: crate::ir::UnboxOp,
        disp: i32,
        legacy: bool,
        bci: usize,
    ) {
        match (uop, legacy) {
            (crate::ir::UnboxOp::FieldBool(_), false) => {
                self.buf.emit(&[0x0F, 0xB6, 0x80]); // MOVZX EAX, BYTE [RAX + disp32]
            }
            (crate::ir::UnboxOp::FieldBool(_), true) => {
                self.buf.emit(&[0x8B, 0x80]); // MOV EAX, [RAX + disp32]
            }
            (crate::ir::UnboxOp::FieldRef(_), false) => {
                self.buf.emit(&[0x48, 0x8B, 0x80]); // MOV RAX, [RAX + disp32]
            }
            (crate::ir::UnboxOp::FieldRef(_), true) => {
                let tag = crate::ir::ffm_element_ref_legacy_tag_offset(disp);
                self.buf.emit(&[0x81, 0xB8]); // CMP DWORD [RAX + disp32], imm32
                self.buf.emit(&tag.to_le_bytes());
                self.buf
                    .emit(&cratonvm_types::FIELD_CELL_TAG_OBJECT.to_le_bytes());
                self.emit_deopt_unless(0x84, bci, DeoptReason::ReceiverTypeChanged); // JE
                self.buf.emit(&[0x48, 0x8B, 0x80]); // MOV RAX, [RAX + disp32]
            }
            _ => {
                // Callers only route `is_ffm_element_read` ops here; refuse the
                // compile instead of panicking if that ever changes.
                self.latch_bailout(Bailout::new(BailoutReason::Internal(
                    "ir_lower: not an FFM element read",
                )));
                return;
            }
        }
        self.buf.emit(&disp.to_le_bytes());
    }

    /// `LOCK INC qword [counter]` when the reference-store path trace is on,
    /// and nothing at all otherwise.
    ///
    /// Uses R11, which is free at both call sites, and clobbers flags — which
    /// is why it is only ever emitted where the next instruction does not read
    /// them (immediately before the value load on the inline path, and before
    /// the argument marshal on the helper path).
    pub(in crate::ir_lower) fn emit_ref_store_path_trace(
        &mut self,
        counter: &'static std::sync::atomic::AtomicU64,
    ) {
        if !crate::x64::ir_ref_store_trace_enabled() {
            return;
        }
        self.emit_mov_reg_imm64(R11, counter as *const _ as u64);
        self.buf.emit(&[0xF0, 0x49, 0xFF, 0x03]); // LOCK INC qword [R11]
    }

    /// COV-03 — the optimizing tier's **gated** compact reference `putfield`.
    ///
    /// The caller has already loaded the receiver into RAX and emitted the
    /// inline null check that DEOPTS, so this sequence starts from a non-null
    /// receiver and never has to reproduce the NullPointerException semantics.
    /// Returns `false` without emitting anything when the site is not admitted,
    /// leaving the caller's unconditional `jit_putfield_object` call in place.
    ///
    /// # Why this exists
    ///
    /// The single-pass backend got a gated reference store on 2026-09-02 and
    /// this tier did not, so the barrier plan the generational collector
    /// publishes was inert exactly where hot loops are compiled. A probe of
    /// nothing but reference stores in a counted loop reported `gated=0
    /// declined=0` on that tier's counter while the method's own admission line
    /// said `admitted to the optimizing pipeline`: not refused — never asked.
    ///
    /// # What makes this sound
    ///
    /// The gates are `x64::objects::emit_gated_compact_ref_putfield`'s, read
    /// through the same predicate, and each names a PREFIX of the barrier
    /// helper's own control flow — a skipped call is a call that would have
    /// returned having done nothing. The one structural difference is where the
    /// store sits relative to the post-barrier decision:
    ///
    /// * the single-pass arm stores INLINE, then decides, and on "barrier
    ///   needed" calls the collector's own `write_barrier`;
    /// * this arm decides FIRST, and on "barrier needed" takes the full
    ///   `jit_putfield_object`, which performs the store itself.
    ///
    /// That is not a shortcut, it is the absence of one. (gcg g4/jm review: two
    /// sentences here were stale and are corrected. This tier DOES have the
    /// heap pointer `write_barrier` takes as its first argument -- it is the
    /// context slot every reference store already reserves for
    /// `jit_putfield_object`. And `CRATONVM_JIT_INLINE_CARD_MARK`, default ON
    /// since the gcg triage tg1, IS read here: [`Self::emit_ir_putfield_card_arm`]
    /// stores inline over an already-dirty card. A clean card still takes the
    /// helper unless the card-store form (`CRATONVM_JIT_CARD_STORE_BARRIER`,
    /// default ON since gcg g6/jm) marks it inline, which the putfield card
    /// arm does too (gch h1/fr: this sentence used to say "it has no putfield
    /// yet").) Deciding first costs one forward branch on the barriered path and
    /// keeps ONE piece of code — the helper — responsible for every store whose
    /// barrier is not settled inline.
    ///
    /// Declining is always safe, and is the only thing a missing plan, an
    /// unresolved compact slot or a disagreeing descriptor can produce.
    pub(in crate::ir_lower) fn emit_gated_ir_ref_putfield(
        &mut self,
        node_pc: Option<usize>,
        base: NodeId,
        value: NodeId,
        field_index: i64,
        fresh_receiver: bool,
    ) -> bool {
        // Both switches, because both mean "do not store a reference field
        // inline". `CRATONVM_NO_JIT_INLINE_PUTFIELD` is the older, broader one
        // and the single-pass gated arm honours it; an emitter that ignored it
        // would leave someone who set it to chase a lost store still getting
        // inline stores, from the tier they were least likely to look at.
        if !crate::x64::ir_gated_ref_store_enabled() || !crate::x64::inline_putfield_enabled() {
            crate::metrics::note_ir_ref_store_decline(0);
            return false;
        }
        let Some(pc) = node_pc else {
            crate::metrics::note_ir_ref_store_decline(1);
            return false;
        };
        // The resolved compact offset for THIS site. Without it there is no
        // inline address to store to — `HEADER_SIZE + field_index * SLOT_SIZE`
        // is the legacy cell displacement and a compact object does not obey
        // it. This is the plumbing COV-03 named as the blocker.
        // Keyed by `(pc, is_reference)` since 2026-09-02 — the IR String-access
        // expansion emits two accesses at one pc, one Int and one Ref, so a
        // pc-only key could name at most one of them. A reference STORE is
        // always the `true` half.
        // nio21 wave 18 (lane L3): see `emit_inline_compact_getfield`.
        // JIT r23 wave 11b (lane L5): a synthetic pc's store takes the row its
        // builder named for THIS node (`synthetic_store_row`), as the load
        // arm does (`CRATONVM_JIT_IR_SYNTHETIC_STORE_FIELD_ROW`).
        let row = if self.graph.synthetic_field_access_pcs.contains(&pc) {
            self.synthetic_store_row
                .filter(|_| ir_synthetic_store_field_row_enabled())
        } else {
            self.compact_fields.get(&(pc, true)).copied()
        };
        let Some((c_off, c_is_ref, type_tag)) = row else {
            crate::metrics::note_ir_ref_store_decline(2);
            return false;
        };
        // Narrow oops would make the stored word an encoded 32-bit reference
        // rather than the bare pointer this emits; legacy layout would make
        // `c_off` the wrong displacement even when one is resolved.
        // zt t32 (section 7, `CRATONVM_ZGC_INLINE_COLORED_STORES`): an armed
        // colored-store heap stores inline with the colored word, never with
        // the inline card arm (a generational plan).
        let colored_store = if crate::x64::narrow_oops_block_inline_fields() {
            match crate::x64::zgc_colored_inline_store_base() {
                Some(b) if self.ir_inline_card_view().is_none() => Some(b),
                None if self.zgc_plain_inline_ref_store_ok() => None,
                _ => {
                    crate::metrics::note_ir_ref_store_decline(3);
                    return false;
                }
            }
        } else {
            None
        };
        if !cratonvm_types::compact_ref_fields_enabled() {
            crate::metrics::note_ir_ref_store_decline(3);
            return false;
        }
        // A colored store writes `Remapped`, which is the right word only with
        // the pre gate idle (no mark: the good color is `Remapped`). A fresh
        // receiver's gate-free store may run inside a mark (the mark opened in
        // the allocation's own slow path, or the receiver is born black), where
        // it leaves a `Remapped` word the mark never heals: after the closing
        // pause flips back, that word reads GOOD and names an object the plan
        // relocated, so a later load returns the stale address (Spring
        // `CommonAnnotationBeanRegistrationAotContributionTests` failed 2 runs
        // in 6 with an NPE in `ZipPath.readAttributes`; the walk check read
        // thousands of such words). The gate stays on a colored store.
        let fresh_receiver = ir_fresh_store_skips_gate(fresh_receiver, colored_store.is_some());
        // No published plan ⇒ no way to rule either barrier out. G1 is the
        // collector that publishes none (it takes its own arm just below).
        // ZGC DOES publish one, per heap (`ZgcRealHeap::jit_ref_store_plan`:
        // pre = `mark_active`, post = `has_old_objects`, floor = age 0), so a
        // ZGC site reaches the gated sequence after this test; only an
        // unpublished (test) table declines here. (zr z1/br: this comment
        // used to list ZGC as a non-publisher.)
        let Some((pre, post, floor)) = self.ref_store_gates else {
            // G1 publishes no gate plan, but the single-pass tier stores
            // inline under G1 all the same, behind its own filter (F-08). This
            // tier's G1 arm is that filter in "decide first" form.
            if self.g1_ref_store_available() {
                return self.emit_g1_ir_ref_putfield(
                    pc,
                    base,
                    value,
                    field_index,
                    c_off,
                    c_is_ref,
                    type_tag,
                    fresh_receiver,
                );
            }
            crate::metrics::note_ir_ref_store_decline(4);
            return false;
        };
        // Descriptor agreement — the same defence the inline `getfield` arm
        // applies and for the same reason: a resolver that fabricated a compact
        // slot (the WildFly Host Controller SIGSEGV) must not steer a store,
        // where it would write a full pointer over a primitive cell.
        if !c_is_ref || !matches!(type_tag, b'L' | b'[') {
            crate::metrics::note_ir_ref_store_decline(5);
            return false;
        }
        // Receiver validity. A base node typed `IrType::Ref` is the IR's own
        // proof that this is an oop — at least as strong as the single-pass
        // `stack_oop_marks` argument its trusted-oop arm rests on — and the
        // caller's null check has already run. Anything else needs the
        // published READ bounds; without them there is no proof to be had, and
        // the site declines rather than dereference an unvalidated address.
        let trusted_oop_receiver = crate::x64::trusted_oop_receiver_getfield_enabled()
            && self.graph.nodes[base as usize].ty == IrType::Ref;
        if !trusted_oop_receiver && self.read_bounds_addr == 0 {
            crate::metrics::note_ir_ref_store_decline(6);
            return false;
        }

        // RAX holds the non-null receiver on entry. The bail sets are kept
        // APART rather than in one vector so the run-time trace can say which
        // gate refused — see `IR_REF_STORE_BAIL`. Without the trace they are
        // concatenated and every one of them lands on the same helper label,
        // which is what they did before this split and what they cost.
        let mut bail_recv: Vec<usize> = Vec::new();
        let mut bail_pre: Vec<usize> = Vec::new();
        let mut bail_barrier: Vec<usize> = Vec::new();
        // `cell_off` below is a compile-time claim about this class's compact
        // layout; the receiver checks that follow are not the only way this arm
        // must be able to decline.
        // JIT round 25 (item 4 wave 2): under the proven-receiver switch the
        // epoch compare is the shared field guard (elided under a dominating
        // deopting guard, deopting where the store may deopt), and a receiver
        // whose compactness is proven skips the per-object shape test below.
        //
        // Item 4 residual 3: and, as the G1 arm, a site with no proof yet may
        // put one down -- the deopting compactness guard right after the
        // epoch guard, recorded only when that guard does not rejoin. Every
        // path that reaches the inline store past it carries a compact
        // receiver either way (the epoch guard's rejoin goes to the helper),
        // so this site skips the shape test too.
        let store_may_deopt = !store_receiver_cannot_be_null(self.graph, base);
        let proven_mode = if self.proven_fields_on && trusted_oop_receiver {
            self.proven_receiver_mode(base, pc, store_may_deopt)
        } else {
            None
        };
        let compact_known = proven_mode.is_some();
        if self.proven_fields_on {
            let epoch = self.emit_field_layout_guard(base, pc, store_may_deopt);
            let epoch_rejoins = epoch.is_some();
            bail_recv.extend(epoch);
            match proven_mode {
                Some(ProvenReceiverMode::Proven) => {
                    self.proven_field_census.ref_store_proven += 1;
                }
                Some(ProvenReceiverMode::Establish) => {
                    // RAX: the non-null receiver; the trap touches the flags
                    // only, and nothing below reads them before setting them.
                    self.emit_compact_receiver_trap(base, pc, !epoch_rejoins);
                    self.proven_field_census.store_establish += 1;
                    self.proven_field_census.gated_ref_store_establish += 1;
                }
                None => {}
            }
        } else {
            bail_recv.extend(self.emit_layout_epoch_guard());
        }
        if !trusted_oop_receiver {
            self.emit_receiver_align_and_containment(&mut bail_recv);
        }

        let mut inline_store: Vec<usize> = Vec::new();
        let mut inline_store_rdx: Vec<usize> = Vec::new();
        let value_is_null = matches!(
            self.graph.nodes.get(value as usize).map(|n| &n.op),
            Some(Op::Const(0))
        );
        let cell_off = c_off as i32; // absolute compact displacement
        let legacy_off = (HEADER_SIZE + field_index as usize * SLOT_SIZE) as i32;
        let legacy = IrLegacyRefCell {
            num_slots_disp: cratonvm_types::NUM_SLOTS_OFFSET as i32,
            tag_disp: legacy_off + cratonvm_types::FIELD_CELL_TAG_OFFSET as i32,
            payload_disp: legacy_off + FIELD_CELL_PAYLOAD64_OFFSET as i32,
        };
        let mut card_done: Vec<usize> = Vec::new();
        let fused = self.zgc_fused_gate;
        if fused.is_some() && fresh_receiver {
            // zs s5/bt2: fresh ZGC receiver owes no barrier; omit the dead gate bytes
            // so hot constructor blocks stay compact in I-cache.
            if crate::x64::dbg_jitc_enabled() {
                eprintln!("[cratonvm-jitc] zgc fresh-receiver ref store: no gate @pc={pc}");
            }
            if !compact_known {
                self.buf.emit(&[0x0F, 0xB6, 0x88]); // MOVZX ECX, byte [RAX + disp32]
                self.buf
                    .emit(&(cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32).to_le_bytes());
            }
        } else if value_is_null {
            // Null written value never creates a cross-generation/card edge; only
            // the SATB pre-barrier (unless fresh on generational) remains.
            let gen_fresh_no_pre =
                fresh_receiver && fused.is_none() && self.ref_store_post_skip_mask.is_some();
            if !gen_fresh_no_pre {
                let gate_once = self.ir_satb_gate_once_here();
                if fused.is_some() && colored_store.is_none() {
                    self.emit_ir_zgc_satb_null_old_skip(
                        pre,
                        gate_once || fused.is_some(),
                        compact_known,
                        false,
                        cell_off,
                        &mut bail_pre,
                    );
                } else {
                    bail_pre.push(self.emit_ir_satb_pre_gate(pre, gate_once || fused.is_some()));
                }
            }
            if !compact_known {
                self.buf.emit(&[0x0F, 0xB6, 0x88]); // MOVZX ECX, byte [RAX + disp32]
                self.buf
                    .emit(&(cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32).to_le_bytes());
            }
        } else {
            if let Some(word) = fused {
                let byte_after_gate = compact_known && colored_store.is_none();
                if !byte_after_gate {
                    self.buf.emit(&[0x0F, 0xB6, 0x88]); // MOVZX ECX, byte [RAX + disp32]
                    self.buf
                        .emit(&(cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32).to_le_bytes());
                }
                inline_store.push(self.emit_zgc_fused_ref_store_gate(word));
                if byte_after_gate {
                    self.buf.emit(&[0x0F, 0xB6, 0x88]); // MOVZX ECX, byte [RAX + disp32]
                    self.buf
                        .emit(&(cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32).to_le_bytes());
                }
            }

            let gen_fresh_no_pre =
                fresh_receiver && fused.is_none() && self.ref_store_post_skip_mask.is_some();
            if gen_fresh_no_pre && crate::x64::dbg_jitc_enabled() {
                eprintln!("[cratonvm-jitc] gen fresh-receiver ref store: no SATB gate @pc={pc}");
            }
            // -- SATB pre-barrier gate --------------------------------------
            let gate_once = self.ir_satb_gate_once_here();
            if !gen_fresh_no_pre {
                if fused.is_some() && colored_store.is_none() {
                    self.emit_ir_zgc_satb_null_old_skip(
                        pre,
                        gate_once,
                        compact_known,
                        true,
                        cell_off,
                        &mut bail_pre,
                    );
                } else {
                    bail_pre.push(self.emit_ir_satb_pre_gate(pre, gate_once));
                }
            }
            if gen_fresh_no_pre {
                if !compact_known {
                    self.buf.emit(&[0x0F, 0xB6, 0x88]); // MOVZX ECX, byte [RAX + disp32]
                    self.buf
                        .emit(&(cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32).to_le_bytes());
                }
                inline_store.push(self.emit_jmp_rel32());
            } else if fused.is_none()
                && compact_known
                && self.ir_inline_card_view().is_some_and(|(v, ..)| {
                    crate::x64::card_view_whole_heap_form(v)
                        && crate::x64::jit_post_only_card_mark_enabled()
                })
            {
                let (view_addr, _, _, card_shift) = self.ir_inline_card_view().unwrap();
                crate::x64::note_post_only_card_site_in(view_addr);
                self.gp_load_value(RDX, value);
                self.buf.emit(&[0x48, 0x89, 0x90]); // MOV [RAX + disp32], RDX
                self.buf.emit(&cell_off.to_le_bytes());
                self.emit_ir_whole_heap_card_mark(
                    view_addr,
                    card_shift,
                    None,
                    &mut bail_barrier,
                    &mut card_done,
                );
            } else {
                if fused.is_none() {
                    self.buf.emit(&[0x0F, 0xB6, 0x88]); // MOVZX ECX, byte [RAX + disp32]
                    self.buf
                        .emit(&(cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32).to_le_bytes());
                }
                if fused.is_none()
                    && self.ir_inline_card_view().is_some_and(|(v, ..)| {
                        crate::x64::card_view_whole_heap_form(v)
                            && crate::x64::jit_post_only_card_mark_enabled()
                    })
                {
                    let view = self.ir_inline_card_view().unwrap();
                    self.emit_ir_putfield_card_arm(
                        view,
                        None,
                        value,
                        cell_off,
                        legacy,
                        field_index,
                        &mut inline_store,
                        &mut inline_store_rdx,
                        &mut bail_barrier,
                        &mut card_done,
                    );
                } else {
                    if let Some(mask) = self.ref_store_post_skip_mask {
                        self.buf.emit(&[0xF6, 0xC1, mask]); // TEST CL, mask
                        inline_store.push(self.emit_jcc_rel32(0x84)); // JZ -> young receiver
                    } else {
                        self.emit_ir_cmp_cl_mem_byte(floor);
                        inline_store.push(self.emit_jcc_rel32(0x82)); // JB -> young receiver
                    }
                    self.emit_ir_gate_byte_zero_cmp(post);
                    inline_store.push(self.emit_jcc_rel32(0x84)); // JZ -> no old objects
                    match self.ir_inline_card_view() {
                        Some(view) => self.emit_ir_putfield_card_arm(
                            view,
                            None,
                            value,
                            cell_off,
                            legacy,
                            field_index,
                            &mut inline_store,
                            &mut inline_store_rdx,
                            &mut bail_barrier,
                            &mut card_done,
                        ),
                        None => bail_barrier.push(self.emit_jmp_rel32()),
                    }
                }
            }
        }

        for patch in inline_store {
            self.patch_rel32_to_here(patch);
        }
        if !value_is_null || !compact_known {
            self.gp_load_value(RDX, value);
        }
        for patch in inline_store_rdx {
            self.patch_rel32_to_here(patch);
        }
        self.emit_ref_store_path_trace(&crate::metrics::IR_REF_STORE_INLINE_TAKEN);
        let legacy_shape = if compact_known {
            None
        } else {
            self.buf
                .emit(&[0xF6, 0xC1, cratonvm_types::GC_FLAG_COMPACT]); // TEST CL, imm8
            Some(self.emit_jcc_rel32(0x84)) // JZ -> the 16-byte cell
        };
        self.emit_ref_store_path_trace(&crate::metrics::IR_REF_STORE_SHAPE_COMPACT);
        if value_is_null {
            self.buf.emit(&[0x48, 0xC7, 0x80]); // MOV qword [RAX + disp32], 0
            self.buf.emit(&cell_off.to_le_bytes());
            self.buf.emit(&0i32.to_le_bytes());
        } else {
            if let Some(zbase) = colored_store {
                crate::x64::emit_zgc_colored_store_encode_rdx(&mut self.buf, zbase);
            }
            self.buf.emit(&[0x48, 0x89, 0x90]); // MOV [RAX + disp32], RDX
            self.buf.emit(&cell_off.to_le_bytes());
        }

        let defer_cold = self.mir_mode == MirMode::Off && !crate::x64::ir_ref_store_trace_enabled();
        let stored = if !compact_known || !defer_cold {
            Some(self.emit_jmp_rel32())
        } else {
            None
        };

        let mut legacy_oob: Option<usize> = None;
        let mut stored_legacy: Option<usize> = None;
        if let Some(p) = legacy_shape {
            self.patch_rel32_to_here(p);
            self.emit_ref_store_path_trace(&crate::metrics::IR_REF_STORE_SHAPE_LEGACY);
            self.buf.emit(&[0x44, 0x8B, 0x98]); // MOV R11D, dword [RAX + disp32]
            self.buf.emit(&legacy.num_slots_disp.to_le_bytes());
            self.emit_mov_reg_imm64(R10, field_index as u64);
            self.buf.emit(&[0x45, 0x3B, 0xD3]); // CMP R10D, R11D
            legacy_oob = Some(self.emit_jcc_rel32(0x83)); // JAE -> drop
            self.emit_mov_reg_imm64(R10, u64::from(cratonvm_types::FIELD_CELL_TAG_OBJECT));
            self.buf.emit(&[0x4C, 0x89, 0x90]); // MOV [RAX + disp32], R10
            self.buf.emit(&legacy.tag_disp.to_le_bytes());
            self.buf.emit(&[0x48, 0x89, 0x90]); // MOV [RAX + disp32], RDX
            self.buf.emit(&legacy.payload_disp.to_le_bytes());
            if !defer_cold {
                stored_legacy = Some(self.emit_jmp_rel32());
            }
        }

        let bail_groups = [(bail_recv, 0usize), (bail_pre, 1), (bail_barrier, 3)];
        let mut to_helper: Vec<usize> = Vec::new();
        if crate::x64::ir_ref_store_trace_enabled() {
            for (patches, reason) in bail_groups {
                if patches.is_empty() {
                    continue;
                }
                for patch in patches {
                    self.patch_rel32_to_here(patch);
                }
                self.emit_ref_store_path_trace(&crate::metrics::IR_REF_STORE_BAIL[reason]);
                to_helper.push(self.emit_jmp_rel32());
            }
        } else {
            for (patches, _) in bail_groups {
                to_helper.extend(patches);
            }
        }

        if let Some(p) = stored {
            self.patch_rel32_to_here(p);
        }
        if let Some(p) = legacy_oob {
            self.patch_rel32_to_here(p);
        }
        if let Some(p) = stored_legacy {
            self.patch_rel32_to_here(p);
        }
        if defer_cold {
            for patch in card_done {
                self.patch_rel32_to_here(patch);
            }
            if !to_helper.is_empty() {
                self.deferred_putfield_tails
                    .push(DeferredPutfieldTail::Helper {
                        jumps: to_helper,
                        base_slot: self.slot_of(base),
                        field_index,
                        value_slot: self.slot_of(value),
                        done: self.buf.pos(),
                    });
            }
        } else {
            for patch in to_helper {
                self.patch_rel32_to_here(patch);
            }
            self.emit_ref_store_path_trace(&crate::metrics::IR_REF_STORE_HELPER_TAKEN);
            self.load_context(CALL_ARG_REGS[0]);
            self.load_reg_from_frame(CALL_ARG_REGS[1], self.slot_of(base));
            self.emit_mov_reg_imm64(CALL_ARG_REGS[2], field_index as i64 as u64);
            self.load_reg_from_frame(CALL_ARG_REGS[3], self.slot_of(value));
            self.emit_mov_reg_imm64(RAX, self.putfield_object as u64);
            self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
            for patch in card_done {
                self.patch_rel32_to_here(patch);
            }
        }
        crate::metrics::note_ir_ref_store_gated();
        true
    }

    /// The inline generational card CHECK for a gated reference `putfield`
    /// ([`Self::emit_gated_ir_ref_putfield`]; gce e1/f, 2026-09-29): the
    /// `putfield` twin of [`Self::emit_ir_aastore_card_arm`] and the
    /// optimizing tier's port of the single-pass `emit_gen_card_barrier` with
    /// `mark == holder` (`x64/op_field.rs`, `x64/objects.rs`). It was the
    /// blocker on
    /// `docs/feature-designs/jit-ir-optimizations.md`:
    /// under `CRATONVM_JIT_INLINE_CARD_MARK` the single-pass tier skipped the
    /// barrier call on a dirty card while this tier called
    /// `jit_putfield_object` for every old receiver, so the flag's A/B could
    /// not measure a loop this tier compiles.
    ///
    /// Entry: RAX = the non-null receiver, flagged old by the gates; CL = its
    /// flags byte; the SATB `pre` byte was zero; old objects exist. `pre` is
    /// the gate byte the arm tests AGAIN right before the store, or `None`
    /// under `CRATONVM_JIT_IR_SATB_GATE_ONCE` (gcj j3/rt3;
    /// [`ir_satb_gate_once_enabled`] is the argument).
    ///
    /// ```text
    ///   rdx = value ; test rdx, rdx ; jz  store       null: no edge
    ///   mov  r11, imm64 &view
    ///   mov  r10, [r11 + old_base]
    ///   cmp  rax, r10            ; jb  helper   flagged old outside the view
    ///   cmp  rax, [r11 + old_end]; jae helper
    ///   cmp  rdx, r10            ; jb  young
    ///   cmp  rdx, [r11 + old_end]; jb  store    old -> old: no card
    /// young:
    ///   r10 = &cards[(rax - old_base) >> CARD_SHIFT]   the HEADER card
    ///   cmp  byte [r10], DIRTY   ; jne helper   PRE: clean card -> the helper
    ///   cmp  byte [pre], 0       ; jne helper   SATB, again, right before the store
    ///   test cl, COMPACT ; jz legacy
    ///   mov  [rax + cell_off], rdx ; jmp post
    /// legacy:
    ///   cmp  dword [rax + NUM_SLOTS], field_index ; jbe helper (it drops the store)
    ///   mov  qword [rax + tag], TAG_OBJECT ; mov [rax + payload], rdx
    /// post:
    ///   cmp  byte [r10], DIRTY   ; je  done     POST
    ///   call write_barrier ; jmp done           card only (gci i12/wb)
    /// ```
    ///
    /// The mark is the receiver itself, the card the helper dirties for a
    /// field store (`jit_putfield_object` -> the collector's `write_barrier`
    /// on the holder), for both shapes: a compact and a legacy object start
    /// at the same address. The soundness argument is
    /// [`Self::emit_ir_aastore_card_arm`]'s, point for point: a clean card is
    /// marked inline under `CRATONVM_JIT_CARD_STORE_BARRIER` (default ON since
    /// gcg g6/jm; the `store` argument of [`Self::emit_ir_card_pre_check`]
    /// below) or is the helper; a dirty card already makes the next young
    /// collection scan this object; a takeover freeze between the store and
    /// POST sees the card PRE saw dirty, and a collection that cleaned it is
    /// caught by POST, which calls the card-only `write_barrier` (gci i12/wb:
    /// it used to take the helper, whose second store of the value could undo
    /// another thread's store made after the pause; see
    /// [`Self::emit_ir_card_post_clean`]); every unsure edge is the helper.
    ///
    /// The legacy arm's bounds test sends an out-of-range index to the helper
    /// (which drops the store) rather than dropping it here, so it needs no
    /// label of its own. Clobbers RDX, R10, R11 and the flags; RAX and RCX
    /// survive on every edge (the `store` edge re-reads CL).
    #[allow(clippy::too_many_arguments)]
    pub(in crate::ir_lower) fn emit_ir_putfield_card_arm(
        &mut self,
        view: (usize, bool, u8, u8),
        pre: Option<usize>,
        value: NodeId,
        cell_off: i32,
        legacy: IrLegacyRefCell,
        field_index: i64,
        inline_store: &mut Vec<usize>,
        inline_store_rdx: &mut Vec<usize>,
        to_helper: &mut Vec<usize>,
        done: &mut Vec<usize>,
    ) {
        let (view_addr, _precise, _header_disp8, card_shift) = view;
        let field_index_imm = i32::try_from(field_index).ok();
        let tag_imm = i32::try_from(cratonvm_types::FIELD_CELL_TAG_OBJECT).ok();
        let (Some(field_index_imm), Some(tag_imm)) = (field_index_imm, tag_imm) else {
            // A displacement the encodings below cannot carry: the helper,
            // exactly as before this arm existed.
            to_helper.push(self.emit_jmp_rel32());
            return;
        };
        // t1 lane D (`x64::jit_whole_heap_card_mark_enabled`): on a whole-heap
        // card map, behind the post-only shape, no null / range / old -> old
        // test before the store and an unconditional hull mark after it.
        let whole_heap = crate::x64::card_view_whole_heap_form(view_addr)
            && crate::x64::jit_post_only_card_mark_enabled();
        if whole_heap {
            crate::x64::note_post_only_card_site_in(view_addr);
            self.gp_load_value(RDX, value);
            if let Some(pre) = pre {
                self.emit_ir_gate_byte_zero_cmp(pre);
                to_helper.push(self.emit_jcc_rel32(0x85)); // JNE -> helper
            }
            self.buf
                .emit(&[0xF6, 0xC1, cratonvm_types::GC_FLAG_COMPACT]); // TEST CL, imm8
            let legacy_shape = self.emit_jcc_rel32(0x84); // JZ -> the 16-byte cell
            self.buf.emit(&[0x48, 0x89, 0x90]); // MOV [RAX + disp32], RDX
            self.buf.emit(&cell_off.to_le_bytes());
            let post = self.emit_jmp_rel32();
            self.patch_rel32_to_here(legacy_shape);
            self.buf.emit(&[0x44, 0x8B, 0x98]); // MOV R11D, dword [RAX + disp32]
            self.buf.emit(&legacy.num_slots_disp.to_le_bytes());
            self.buf.emit(&[0x41, 0x81, 0xFB]); // CMP R11D, imm32
            self.buf.emit(&field_index_imm.to_le_bytes());
            to_helper.push(self.emit_jcc_rel32(0x86)); // JBE -> helper (index out of range)
            self.buf.emit(&[0x48, 0xC7, 0x80]); // MOV qword [RAX + disp32], imm32
            self.buf.emit(&legacy.tag_disp.to_le_bytes());
            self.buf.emit(&tag_imm.to_le_bytes());
            self.buf.emit(&[0x48, 0x89, 0x90]); // MOV [RAX + disp32], RDX
            self.buf.emit(&legacy.payload_disp.to_le_bytes());
            self.patch_rel32_to_here(post);
            self.emit_ir_whole_heap_card_mark(view_addr, card_shift, None, to_helper, done);
            return;
        }
        let _ = inline_store;
        let old_base = crate::x64::CARD_VIEW_OLD_BASE.to_le_bytes();
        let old_end = crate::x64::CARD_VIEW_OLD_END.to_le_bytes();
        self.gp_load_value(RDX, value);
        self.buf.emit(&[0x48, 0x85, 0xD2]); // TEST RDX, RDX
        inline_store_rdx.push(self.emit_jcc_rel32(0x84)); // JZ -> store (null: no edge)
        self.emit_mov_reg_imm64(R11, view_addr as u64); // Cast: a baked data address
        self.buf.emit(&[0x4D, 0x8B, 0x93]); // MOV R10, [R11 + disp32]
        self.buf.emit(&old_base);
        self.buf.emit(&[0x4C, 0x39, 0xD0]); // CMP RAX, R10
        to_helper.push(self.emit_jcc_rel32(0x82)); // JB -> helper
        self.buf.emit(&[0x49, 0x3B, 0x83]); // CMP RAX, [R11 + disp32]
        self.buf.emit(&old_end);
        to_helper.push(self.emit_jcc_rel32(0x83)); // JAE -> helper
        self.buf.emit(&[0x4C, 0x39, 0xD2]); // CMP RDX, R10
        let young_value = self.emit_jcc_rel32(0x82); // JB -> young value
        self.buf.emit(&[0x49, 0x3B, 0x93]); // CMP RDX, [R11 + disp32]
        self.buf.emit(&old_end);
        inline_store_rdx.push(self.emit_jcc_rel32(0x82)); // JB -> old -> old: no card
        self.patch_rel32_to_here(young_value);
        // &cards[(rax - old_base) >> CARD_SHIFT]: the header card.
        self.buf.emit(&[0x49, 0x89, 0xC2]); // MOV R10, RAX
        self.emit_ir_card_address_from_r10(view_addr, card_shift);
        // PRE: clean card -> helper, or (gcf f2/p, card-store form) marked here.
        let store =
            ir_card_store_barrier_enabled().then_some((view_addr, IrCardMark::Rax, card_shift));
        // gcj j2/jt2 (`CRATONVM_JIT_POST_ONLY_CARD_MARK`): no PRE check; the
        // POST edge below marks a clean card. One reading for both halves.
        let post_only = crate::x64::jit_post_only_card_mark_enabled();
        if post_only {
            crate::x64::note_post_only_card_site_in(view_addr);
        } else {
            self.emit_ir_card_pre_check(to_helper, store);
        }
        if let Some(pre) = pre {
            self.emit_ir_gate_byte_zero_cmp(pre);
            to_helper.push(self.emit_jcc_rel32(0x85)); // JNE -> helper
        }
        // The store, in this OBJECT's shape (CL is the flags byte).
        self.buf
            .emit(&[0xF6, 0xC1, cratonvm_types::GC_FLAG_COMPACT]); // TEST CL, imm8
        let legacy_shape = self.emit_jcc_rel32(0x84); // JZ -> the 16-byte cell
        self.buf.emit(&[0x48, 0x89, 0x90]); // MOV [RAX + disp32], RDX
        self.buf.emit(&cell_off.to_le_bytes());
        let post = self.emit_jmp_rel32();
        self.patch_rel32_to_here(legacy_shape);
        self.buf.emit(&[0x44, 0x8B, 0x98]); // MOV R11D, dword [RAX + disp32]
        self.buf.emit(&legacy.num_slots_disp.to_le_bytes());
        self.buf.emit(&[0x41, 0x81, 0xFB]); // CMP R11D, imm32
        self.buf.emit(&field_index_imm.to_le_bytes());
        to_helper.push(self.emit_jcc_rel32(0x86)); // JBE -> helper (index out of range)
        self.buf.emit(&[0x48, 0xC7, 0x80]); // MOV qword [RAX + disp32], imm32
        self.buf.emit(&legacy.tag_disp.to_le_bytes());
        self.buf.emit(&tag_imm.to_le_bytes());
        self.buf.emit(&[0x48, 0x89, 0x90]); // MOV [RAX + disp32], RDX
        self.buf.emit(&legacy.payload_disp.to_le_bytes());
        self.patch_rel32_to_here(post);
        self.emit_ir_card_post_compare(view_addr, post_only, done);
        if post_only {
            // gcj j2/jt2: the store is done and no PRE marked the card.
            self.emit_ir_card_post_only_clean(None, store, to_helper, done);
        } else {
            // gci i12/wb item 3: the store is done; only the card is owed.
            self.emit_ir_card_post_clean(None, to_helper, done);
        }
    }

    /// gci i12/wb (`gci-i12wb-jit-barrier-review-findings-FIXED-20261015` item 3):
    /// the POST edge of the inline card arms, reached only when the card PRE
    /// saw dirty (or marked) is clean again after the store -- a collection
    /// cleared it while the BUG-03 takeover held this thread between the two.
    ///
    /// The store has already happened, so only the card is owed: call the
    /// collector's card-only `write_barrier(vm_ptr, mark, value)`, as the
    /// single-pass `emit_gen_card_barrier` does on its slow edge. These arms
    /// used to take the full helper (`jit_putfield_object` / `jit_aastore`),
    /// which stores the value a SECOND time and so could undo a store another
    /// thread made after the pause. Without a wired barrier (`0`), and under
    /// `CRATONVM_JIT_IR_CARD_POST_BARRIER_ONLY=0`, the full helper as before.
    ///
    /// Entry: RAX = the receiver / array, RDX = the stored (non-null) value,
    /// RCX = the zero-extended index for an `element` mark (`Some(HEADER_SIZE
    /// as disp8)`). R10 and R11 carry the operands across the argument loads:
    /// neither is an argument register on either ABI.
    pub(in crate::ir_lower) fn emit_ir_card_post_clean(
        &mut self,
        element: Option<u8>,
        to_helper: &mut Vec<usize>,
        done: &mut Vec<usize>,
    ) {
        if self.card_write_barrier == 0 {
            to_helper.push(self.emit_jmp_rel32());
            return;
        }
        match element {
            // LEA R10, [RAX + RCX*8 + disp8]: the element's slot (8-aligned).
            Some(d) => self.buf.emit(&[0x4C, 0x8D, 0x54, 0xC8, d]),
            None => self.buf.emit(&[0x49, 0x89, 0xC2]), // MOV R10, RAX
        }
        self.emit_mov_reg_reg64(R11, RDX);
        self.load_context(CALL_ARG_REGS[0]);
        self.emit_mov_reg_reg64(CALL_ARG_REGS[1], R10);
        self.emit_mov_reg_reg64(CALL_ARG_REGS[2], R11);
        // (vm_ptr, card_addr, val_ptr) -> ().
        cratonvm_jit_api::assert_helper_call_shape!(
            "write_barrier",
            int_args = 3,
            returns_value = false
        );
        self.emit_mov_reg_imm64(RAX, self.card_write_barrier as u64);
        self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
        done.push(self.emit_jmp_rel32());
    }

    /// The published barrier gates when this compile may emit the gated inline
    /// `aastore` store ([`Self::emit_gated_ir_aastore`]), `None` otherwise.
    ///
    /// Declines under the IR gated-store switch (`CRATONVM_JIT_IR_REF_STORE=0`,
    /// the same lever as the reference `putfield` twin) or the plan switch
    /// the single-pass `aastore` arm honours (`CRATONVM_JIT_GATED_REF_STORE=0`);
    /// with no published plan (G1; ZGC publishes one per heap since zr z1/br,
    /// `ZgcRealHeap::jit_ref_store_plan`); and under narrow oops, where an
    /// element is not a bare 8-byte pointer. (zs s3/bt: this used to say it
    /// also declined "under the ZGC load barrier". It tests no such thing:
    /// ZGC slots hold plain pointers until the colored-pointer arming plan's
    /// A2b, which must add that decline here and to the `putfield` twin.)
    pub(in crate::ir_lower) fn gated_ir_aastore_gates(&self) -> Option<(usize, usize, usize)> {
        if !crate::x64::ir_gated_ref_store_enabled()
            || !crate::x64::gated_ref_store_enabled()
            || (crate::x64::narrow_oops_block_inline_fields()
                && self.ir_colored_aastore().is_none()
                && !self.zgc_plain_inline_ref_store_ok())
        {
            return None;
        }
        self.ref_store_gates
    }

    /// zt t32 (successor page section 7, `CRATONVM_ZGC_INLINE_COLORED_STORES`):
    /// the arena base the gated `aastore` ([`Self::emit_gated_ir_aastore`])
    /// encodes its element against on an armed colored-store heap, or `None`.
    /// Its barrier-free store runs only with the SATB pre gate idle (the good
    /// color `Remapped`, `zgc_colored_inline_store_base`); a busy gate already
    /// reaches `jit_aastore`. Never with the inline card arm.
    pub(in crate::ir_lower) fn ir_colored_aastore(&self) -> Option<u64> {
        if self.ir_inline_card_view().is_some() {
            return None;
        }
        crate::x64::zgc_colored_inline_store_base()
    }

    /// Round 11 wave 5 (lane lower, `r11-irlower-aastore-pays-two-helper-calls`,
    /// proposal W4-2): the barrier-free `aastore` element store, gated like
    /// [`Self::emit_gated_ir_ref_putfield`] but on the ARRAY's flags byte, with
    /// the caller's unchanged `jit_aastore` call as the barriered path.
    ///
    /// Entry contract: this site's null and bounds guards have run, and the
    /// covariance obligation is discharged -- statically, or by a
    /// `jit_aastore_type_check` that answered `0` on the path that reaches
    /// here (a null value skips that call and needs no check). `gates` comes
    /// from [`Self::gated_ir_aastore_gates`].
    ///
    /// Every run-time decline falls through to the position this returns at,
    /// where the caller emits the `jit_aastore` call. The returned rel32 is the
    /// inline store's exit; the caller patches it to the point after that
    /// call.
    ///
    /// What the skipped helper would have done, and why it is a no-op here:
    /// * null, bounds, covariance: established (entry contract);
    /// * SATB pre-barrier: the published `pre` byte is zero, and it is armed
    ///   only while mutators are parked -- nothing between the test and the
    ///   store is a safepoint;
    /// * post barrier: a null value records no edge; otherwise the array's
    ///   flags byte passes the young mask/floor or the `post` byte is zero.
    ///   These are the tests the single-pass `aastore` arm makes in front of
    ///   its `write_barrier` call (`x64/op_array.rs`).
    ///
    /// The element is `[array + index*8 + HEADER_SIZE]`, the address this
    /// tier's `aaload` reads. The index is re-read from its home, so it is
    /// zero-extended exactly as the bounds guard saw it (`MOV ECX, ECX`).
    /// Clobbers RAX, RCX, RDX, R10, R11 and the flags.
    ///
    /// Round 13 wave 2 (lane irhash, proposal W7-2 of
    /// `docs/feature-designs/jit-ir-optimizations.md`): where the gates say the post barrier MAY
    /// have work (a non-null value into an old array while old objects exist)
    /// this used to take `jit_aastore` every time -- every `tab[i] = newNode(..)`
    /// of `HashMap.putVal` into its old table. With
    /// [`Self::ir_inline_card_view`] it first runs the single-pass tier's
    /// inline card CHECK ([`Self::emit_ir_aastore_card_arm`]) and stores inline
    /// when the card is already dirty. Returns every rel32 that must be patched
    /// to the point after the caller's helper call.
    pub(in crate::ir_lower) fn emit_gated_ir_aastore(
        &mut self,
        gates: (usize, usize, usize),
        array: NodeId,
        index: NodeId,
        value: NodeId,
        fresh_receiver: bool,
    ) -> Vec<usize> {
        let (pre, post, floor) = gates;
        let mut to_helper: Vec<usize> = Vec::new();
        let mut inline_store: Vec<usize> = Vec::new();
        let mut done: Vec<usize> = Vec::new();
        self.gp_load_value(RAX, array);
        self.gp_load_value(RDX, value);
        let colored_store = self.ir_colored_aastore();
        let fresh_skip_gate = ir_fresh_store_skips_gate(fresh_receiver, colored_store.is_some());
        // zs s3/bt: the fused gate (ZGC's age-floor plan): both gate bytes
        // zero is sufficient for every arm below that reaches the store (a
        // null value, a young array, no old objects), so it stores at once;
        // otherwise the full sequence runs unchanged. See
        // `x64::zgc_fused_ref_store_gate_of`. For a fresh `NewArray` (Q3)
        // with no intervening type-check call, jump straight to the store.
        if let Some(word) = self.zgc_fused_gate {
            if fresh_skip_gate {
                inline_store.push(self.emit_jmp_rel32());
            } else {
                inline_store.push(self.emit_zgc_fused_ref_store_gate(word));
            }
        }
        // SATB pre-barrier gate. gcj j3/rt3 (`CRATONVM_JIT_IR_SATB_GATE_ONCE`):
        // the store's only one on the Generational mask plan.
        let gate_once = self.ir_satb_gate_once_here();
        let gen_fresh_no_pre = fresh_receiver
            && self.zgc_fused_gate.is_none()
            && self.ref_store_post_skip_mask.is_some();
        if !gen_fresh_no_pre {
            to_helper.push(self.emit_ir_satb_pre_gate(pre, gate_once));
        }
        if gen_fresh_no_pre {
            inline_store.push(self.emit_jmp_rel32());
        } else if self.zgc_fused_gate.is_none()
            && self.ir_inline_card_view().is_some_and(|(v, ..)| {
                crate::x64::card_view_whole_heap_form(v)
                    && crate::x64::jit_post_only_card_mark_enabled()
            })
        {
            let view = self.ir_inline_card_view().unwrap();
            self.emit_ir_aastore_card_arm(
                view,
                None,
                index,
                &mut inline_store,
                &mut to_helper,
                &mut done,
            );
        } else {
            // Post-barrier gates. A null value records no edge.
            self.buf.emit(&[0x48, 0x85, 0xD2]); // TEST RDX, RDX
            inline_store.push(self.emit_jcc_rel32(0x84)); // JZ -> store
            self.buf.emit(&[0x0F, 0xB6, 0x88]); // MOVZX ECX, byte [RAX + disp32]
            self.buf
                .emit(&(cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32).to_le_bytes());
            if let Some(mask) = self.ref_store_post_skip_mask {
                self.buf.emit(&[0xF6, 0xC1, mask]); // TEST CL, mask
                inline_store.push(self.emit_jcc_rel32(0x84)); // JZ -> young array
            } else {
                self.emit_ir_cmp_cl_mem_byte(floor);
                inline_store.push(self.emit_jcc_rel32(0x82)); // JB -> young array
            }
            self.emit_ir_gate_byte_zero_cmp(post);
            inline_store.push(self.emit_jcc_rel32(0x84)); // JZ -> no old objects
            match self.ir_inline_card_view() {
                Some(view) => self.emit_ir_aastore_card_arm(
                    view,
                    None,
                    index,
                    &mut inline_store,
                    &mut to_helper,
                    &mut done,
                ),
                None => to_helper.push(self.emit_jmp_rel32()),
            }
        }
        // The barrier-free store.
        for patch in inline_store {
            self.patch_rel32_to_here(patch);
        }
        self.emit_ref_aastore_index_rcx(index);
        if let Some(zbase) = self.ir_colored_aastore() {
            // zt t32: the colored word (R11 only; nothing below reads RDX).
            crate::x64::emit_zgc_colored_store_encode_rdx(&mut self.buf, zbase);
        }
        // `MOV QWORD [RAX + RCX*8 + HEADER_SIZE], RDX`: without narrow oops a
        // reference element is the same 8-byte word a `long` element is, at
        // the displacement `aaload` reads, so the `lastore` encoder emits it.
        self.emit_gpr_array_elem_store(MemKind::Long, ArrayStoreSource::Reg(RDX));
        done.push(self.emit_jmp_rel32());
        for patch in to_helper {
            self.patch_rel32_to_here(patch);
        }
        done
    }

    /// r21 barriers (BARRIERS21-2): the G1 plan this compile's reference
    /// `aastore` may store inline under, or `None` (the site keeps
    /// `jit_aastore`).
    ///
    /// The plan needs, all at once:
    ///
    /// * the switch [`ir_g1_gated_aastore_enabled`] on;
    /// * the reference-store levers this tier's other inline stores honour:
    ///   `CRATONVM_JIT_IR_REF_STORE` and `CRATONVM_JIT_GATED_REF_STORE`;
    /// * no narrow oops (the element store below writes a bare 8-byte
    ///   pointer, as the generational twin does);
    /// * the shared G1 recognition `x64::g1_aastore_plan_from`, over the words
    ///   this lowerer copied from the helper table.
    pub(in crate::ir_lower) fn ir_g1_aastore_plan(&self) -> Option<crate::x64::G1AastorePlan> {
        if !ir_g1_gated_aastore_enabled()
            || !crate::x64::ir_gated_ref_store_enabled()
            || !crate::x64::gated_ref_store_enabled()
            || crate::x64::narrow_oops_block_inline_fields()
        {
            return None;
        }
        crate::x64::g1_aastore_plan_from(
            self.satb_pre_gate,
            self.ref_store_gates.is_some() || self.ref_store_post_skip_mask.is_some(),
            self.ir_card_view.is_some(),
            self.g1_barrier_addr,
            self.g1_post_write_barrier,
        )
    }

    /// r21 barriers (BARRIERS21-2): the G1 reference `aastore` without the
    /// whole `jit_aastore` call. This is the G1 twin of
    /// [`Self::emit_gated_ir_aastore`], with the same entry contract and the
    /// same return. This site's null and bounds guards have run, and the
    /// covariance obligation is discharged. Every decline falls through to the
    /// position this returns at, where the caller emits `jit_aastore`. The
    /// returned rel32s are patched to the point after that call.
    ///
    /// ```text
    ///   rax = array ; rdx = value
    ///   <g1_aastore_screen_bytes>       ; armed SATB gate -> jit_aastore
    ///   rcx = index ; store ; jmp done   ; null / same region: no barrier
    /// cross:
    ///   rcx = index ; store
    ///   call jit_g1_post_write_barrier(vm, array, value) ; jmp done
    /// ```
    ///
    /// This is exactly what `jit_aastore` would do on these paths.
    ///
    /// * Its SATB log of the old element is a no-op while the gate byte is
    ///   clear. A take-over freeze between the gate test and the store is
    ///   covered by G1's concurrent-start scan of what frozen peers hold: the
    ///   array is in RAX (`scan_frozen_held_objects`, as for g1put).
    /// * Its post barrier, `G1Collector::write_barrier`, is
    ///   `post_write_barrier_rset(array, value)`, which returns on a null
    ///   value and on a same-region pair. The cross-region call makes that
    ///   same call.
    /// * Its covariance re-check is moot (discharged), and so is its bounds
    ///   re-check (guarded).
    ///
    /// The lean barrier cannot collect (no safepoint, no allocation), so like
    /// the generational arm's card-only call it needs no map.
    ///
    /// Clobbers RAX, RCX, RDX, R10, R11, the flags and, on the cross-region
    /// path, every caller-saved register (the site is a call site for the
    /// allocator already: its helper path is `jit_aastore`).
    pub(in crate::ir_lower) fn emit_g1_ir_aastore(
        &mut self,
        plan: crate::x64::G1AastorePlan,
        array: NodeId,
        index: NodeId,
        value: NodeId,
    ) -> Vec<usize> {
        if crate::x64::dbg_jitc_enabled() {
            eprintln!("[cratonvm-jitc] ir g1 gated aastore (BARRIERS21-2)");
        }
        let mut done: Vec<usize> = Vec::new();
        self.note_rcx_written();
        self.gp_load_value(RAX, array);
        self.gp_load_value(RDX, value);
        let g1_geom = crate::ir_g1_edge_filter::g1_edge_filter_geom(&self.direct_helpers);
        let at = self.buf.pos();
        let (bytes, decline, cross, holder_in_r10) = if let Some(ref g) = g1_geom {
            let here = self.buf.as_ptr() as usize + at; // Cast: buffer address
            let s = crate::ir_g1_edge_filter::g1_aastore_screen_with_geom(plan, g, here);
            (s.bytes, s.decline, s.cross, s.holder_in_r10)
        } else {
            let (bytes, decline, cross) = g1_aastore_screen_bytes(plan);
            (bytes, decline, cross, false)
        };
        self.buf.emit(&bytes);
        let to_helper: Vec<usize> = decline.into_iter().map(|e| at + e).collect();
        let cross: Vec<usize> = cross.into_iter().map(|e| at + e).collect();
        // A null or same-region value (or young-region holder): the barrier-free store.
        self.emit_ref_aastore_index_rcx(index);
        self.emit_gpr_array_elem_store(MemKind::Long, ArrayStoreSource::Reg(RDX));
        done.push(self.emit_jmp_rel32());
        // Cross-region: the store, then G1's own remembered-set barrier.
        for patch in cross {
            self.patch_rel32_to_here(patch);
        }
        self.emit_ref_aastore_index_rcx(index);
        self.emit_gpr_array_elem_store(MemKind::Long, ArrayStoreSource::Reg(RDX));
        // RAX (= array), RDX (= value), and R10 (= array - arena_base when the
        // screen says so) stay live across the index load and the element store;
        // the edge filter writes RAX, RCX, RDX, R10, R11 (all reloaded before the
        // post-barrier call).
        if let Some(ref g) = g1_geom {
            let at = self.buf.pos();
            let here = self.buf.as_ptr() as usize + at; // Cast: buffer address
            let (bytes, misses) = if holder_in_r10 {
                crate::ir_g1_edge_filter::g1_edge_filter_hit_bytes_from_offsets(g, here)
            } else {
                crate::ir_g1_edge_filter::g1_edge_filter_hit_bytes_rip(g, here)
            };
            self.buf.emit(&bytes);
            done.push(self.emit_jmp_rel32());
            for m in misses {
                self.patch_rel32_to_here(at + m);
            }
        }
        self.load_context(CALL_ARG_REGS[0]);
        self.gp_load_value(CALL_ARG_REGS[1], array);
        self.gp_load_value(CALL_ARG_REGS[2], value);
        cratonvm_jit_api::assert_helper_call_shape!(
            "g1_post_write_barrier",
            int_args = 3,
            returns_value = false
        );
        self.emit_mov_reg_imm64(RAX, plan.post as u64); // Cast: the helper's address
        self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
        done.push(self.emit_jmp_rel32());
        for patch in to_helper {
            self.patch_rel32_to_here(patch);
        }
        done
    }

    /// The generational card view this compile's gated `aastore` may check
    /// inline, as `(view address, element-precise marks, HEADER_SIZE as a
    /// disp8, CARD_SHIFT)`, or `None` (round 13 wave 2, lane irhash, W7-2).
    ///
    /// `None` unless [`ir_inline_card_check_enabled`] (the single-pass tier's
    /// opt-in `CRATONVM_JIT_INLINE_CARD_MARK`, so one A/B covers both tiers,
    /// and this tier's own kill switch) and the helper table names a valid
    /// view (`x64::gen_card_view_of`, the single-pass tier's own predicate,
    /// resolved once into `ir_card_view`). The two constants are range-checked
    /// here rather than narrowed with a cast, so an encoding they would not fit
    /// declines the arm instead of emitting a wrong displacement.
    /// gcj j3/rt3: does this gated reference store test its SATB pre gate
    /// ONCE ([`ir_satb_gate_once_enabled`])? Only on the generational mask
    /// plan with an inline card view -- the shape whose card arm re-tests
    /// the gate -- so G1 (no plan), ZGC (the fused / floor plan) and a site
    /// without a card view keep every byte.
    pub(in crate::ir_lower) fn ir_satb_gate_once_here(&self) -> bool {
        self.zgc_fused_gate.is_none()
            && self.ref_store_post_skip_mask.is_some()
            && self.ir_inline_card_view().is_some()
            && ir_satb_gate_once_enabled()
    }

    /// Shared gate-byte zero compare: `CMP byte [rip+disp32], 0` (7 bytes, no
    /// register clobbered) when `addr` is in ±2 GiB reach, falling back to
    /// `MOV R11, imm64; CMP byte [R11], 0` (14 bytes).
    pub(in crate::ir_lower) fn emit_ir_gate_byte_zero_cmp(&mut self, addr: usize) {
        let here = self.buf.as_ptr() as usize + self.buf.pos(); // Cast: buffer address
        if let Some(bytes) = zgc_pre_byte_cmp_rip_bytes(here, addr) {
            self.buf.emit(&bytes);
        } else {
            self.emit_mov_reg_imm64(R11, addr as u64);
            self.buf.emit(&[0x41, 0x80, 0x3B, 0x00]); // CMP byte [R11], 0
        }
    }

    /// `CMP CL, byte [rip+disp32]` (6 bytes, no register clobbered) when `addr`
    /// is in ±2 GiB reach, falling back to `MOV R11, imm64; CMP CL, byte [R11]`.
    pub(in crate::ir_lower) fn emit_ir_cmp_cl_mem_byte(&mut self, addr: usize) {
        let here = self.buf.as_ptr() as usize + self.buf.pos(); // Cast: buffer address
        if let Some(disp) = rip_disp32(here, 6, addr) {
            self.buf.emit(&[0x3A, 0x0D]); // CMP CL, byte [RIP + disp32]
            self.buf.emit(&disp.to_le_bytes());
        } else {
            self.emit_mov_reg_imm64(R11, addr as u64);
            self.buf.emit(&[0x41, 0x3A, 0x0B]); // CMP CL, byte [R11]
        }
    }

    /// The gated reference store's SATB pre gate: `CMP byte [pre], 0; JNE`
    /// (the returned patch). Emits `CMP byte [rip+disp32], 0` when `pre` is
    /// within ±2 GiB reach, otherwise `MOV R11, imm64; CMP byte [R11], 0`.
    pub(in crate::ir_lower) fn emit_ir_satb_pre_cmp(&mut self, pre: usize, once: bool) {
        if once && crate::x64::dbg_jitc_enabled() {
            let here = self.buf.as_ptr() as usize + self.buf.pos(); // Cast: buffer address
            let rip = zgc_pre_byte_cmp_rip_bytes(here, pre).is_some();
            eprintln!(
                "[cratonvm-jitc] gen ref store: one SATB gate rip={rip} here={here:#x} gate={pre:#x}"
            );
        }
        self.emit_ir_gate_byte_zero_cmp(pre);
    }

    pub(in crate::ir_lower) fn emit_ir_satb_pre_gate(&mut self, pre: usize, once: bool) -> usize {
        self.emit_ir_satb_pre_cmp(pre, once);
        self.emit_jcc_rel32(0x85) // JNE -> helper
    }

    /// ZGC SATB pre-barrier gate with an inline null-old-value check: when
    /// `pre != 0`, stay inline if the compact slot still holds `0` (`null`).
    fn emit_ir_zgc_satb_null_old_skip(
        &mut self,
        pre: usize,
        once: bool,
        compact_known: bool,
        cl_loaded: bool,
        cell_off: i32,
        bail_pre: &mut Vec<usize>,
    ) {
        self.emit_ir_satb_pre_cmp(pre, once);
        let no_satb = self.emit_jcc_rel32(0x84); // JE -> no_satb
        if !compact_known {
            if cl_loaded {
                self.buf
                    .emit(&[0xF6, 0xC1, cratonvm_types::GC_FLAG_COMPACT]); // TEST CL, imm8
            } else {
                self.buf.emit(&[0xF6, 0x80]); // TEST byte [RAX + disp32], imm8
                self.buf
                    .emit(&(cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32).to_le_bytes());
                self.buf.emit_byte(cratonvm_types::GC_FLAG_COMPACT);
            }
            bail_pre.push(self.emit_jcc_rel32(0x84)); // JZ -> helper
        }
        // CMP qword [RAX + cell_off], 0; JNE -> helper
        self.buf.emit(&[0x48, 0x83, 0xB8]);
        self.buf.emit(&cell_off.to_le_bytes());
        self.buf.emit_byte(0x00);
        bail_pre.push(self.emit_jcc_rel32(0x85));
        self.patch_rel32_to_here(no_satb);
    }

    pub(in crate::ir_lower) fn ir_inline_card_view(&self) -> Option<(usize, bool, u8, u8)> {
        let (view, precise) = self.ir_card_view?;
        if !ir_inline_card_check_enabled() {
            return None;
        }
        let header_disp8 = u8::try_from(HEADER_SIZE).ok().filter(|&d| d < 0x80)?;
        let card_shift = u8::try_from(cratonvm_types::CARD_SHIFT)
            .ok()
            .filter(|&s| s < 64)?;
        Some((view, precise, header_disp8, card_shift))
    }

    /// The inline generational card CHECK for a gated `aastore`
    /// ([`Self::emit_gated_ir_aastore`]), the optimizing tier's twin of
    /// `x64::objects`'s `emit_gen_card_check` / `emit_gen_card_barrier`
    /// (round 13 wave 2, lane irhash, proposal W7-2).
    ///
    /// Entry: RAX = the array, flagged old by the gates; RDX = the value,
    /// non-null; the SATB `pre` byte was zero; old objects exist.
    ///
    /// ```text
    ///   mov  r11, imm64 &view
    ///   mov  r10, [r11 + old_base]
    ///   cmp  rax, r10            ; jb  helper   flagged old outside the view:
    ///   cmp  rax, [r11 + old_end]; jae helper   the collector decides
    ///   cmp  rdx, r10            ; jb  young
    ///   cmp  rdx, [r11 + old_end]; jb  store    old -> old: no card
    /// young:
    ///   r10 = precise ? lea [rax + rcx*8 + HEADER] (jae helper past old_end) : rax
    ///   r10 = &cards[(r10 - old_base) >> CARD_SHIFT]
    ///   cmp  byte [r10], DIRTY   ; jne helper   PRE: clean card -> the helper
    ///   cmp  byte [pre], 0       ; jne helper   SATB, again, right before the store
    ///   store
    ///   cmp  byte [r10], DIRTY   ; je  done     POST
    ///   call write_barrier ; jmp done           card only (gci i12/wb)
    /// ```
    ///
    /// Why it is sound (the single-pass argument, `emit_gen_card_barrier`):
    ///
    /// * A clean card is not this arm's to skip. Under
    ///   `CRATONVM_JIT_CARD_STORE_BARRIER` (default ON since gcg g6/jm) the
    ///   clean edge MARKS the card inline and stores
    ///   ([`Self::emit_ir_card_store_inline`], through
    ///   [`Self::emit_ir_card_pre_check`]); an index at or above the view's
    ///   scan-bound mirror, and every clean card with the switch at `0`, takes
    ///   the helper, which keeps the summary map and the scan bound the
    ///   collector's. (gch h1/fr, `gcg-g4jm-...` item 8: this bullet used to
    ///   say compiled code never writes a card byte, which the default no
    ///   longer is.)
    /// * A dirty card already makes the next young collection scan this array
    ///   (its element card when marks are element-precise, the header card
    ///   otherwise -- the same mark the helper would dirty), so the new
    ///   old -> young edge is found. Cards are cleared only at stop-the-world.
    /// * A BUG-03 takeover can freeze this thread at any instruction. Frozen
    ///   before the store, the slot still holds its old value. Frozen between
    ///   the store and the POST test, the card is the one PRE saw dirty, so
    ///   the collection scans the array and sees the edge (the young value is
    ///   in RDX, rooted conservatively, not moved). A collection that cleaned
    ///   the card in between is caught by POST, which calls the card-only
    ///   `write_barrier` on the same mark (gci i12/wb,
    ///   [`Self::emit_ir_card_post_clean`]; it used to take `jit_aastore`, whose
    ///   second store could undo another thread's store made after the pause).
    /// * Every "not sure" edge goes to the helper, never to a skip.
    ///
    /// Clobbers RCX, R10, R11 and the flags; RAX and RDX survive on every edge.
    pub(in crate::ir_lower) fn emit_ir_aastore_card_arm(
        &mut self,
        view: (usize, bool, u8, u8),
        pre: Option<usize>,
        index: NodeId,
        inline_store: &mut Vec<usize>,
        to_helper: &mut Vec<usize>,
        done: &mut Vec<usize>,
    ) {
        let (view_addr, precise, header_disp8, card_shift) = view;
        // t1 lane D (`x64::jit_whole_heap_card_mark_enabled`): on a whole-heap
        // card map, behind the post-only shape, no range / old -> old test
        // before the store and an unconditional hull mark after it (the
        // element's card when marks are element-precise).
        if crate::x64::card_view_whole_heap_form(view_addr)
            && crate::x64::jit_post_only_card_mark_enabled()
        {
            crate::x64::note_post_only_card_site_in(view_addr);
            if let Some(pre) = pre {
                self.emit_ir_gate_byte_zero_cmp(pre);
                to_helper.push(self.emit_jcc_rel32(0x85)); // JNE -> helper
            }
            self.emit_ref_aastore_index_rcx(index);
            self.emit_gpr_array_elem_store(MemKind::Long, ArrayStoreSource::Reg(RDX));
            self.emit_ir_whole_heap_card_mark(
                view_addr,
                card_shift,
                precise.then_some(header_disp8),
                to_helper,
                done,
            );
            return;
        }
        let old_base = crate::x64::CARD_VIEW_OLD_BASE.to_le_bytes();
        let old_end = crate::x64::CARD_VIEW_OLD_END.to_le_bytes();
        self.emit_mov_reg_imm64(R11, view_addr as u64); // Cast: a baked data address
        self.buf.emit(&[0x4D, 0x8B, 0x93]); // MOV R10, [R11 + disp32]
        self.buf.emit(&old_base);
        self.buf.emit(&[0x4C, 0x39, 0xD0]); // CMP RAX, R10
        to_helper.push(self.emit_jcc_rel32(0x82)); // JB -> helper
        self.buf.emit(&[0x49, 0x3B, 0x83]); // CMP RAX, [R11 + disp32]
        self.buf.emit(&old_end);
        to_helper.push(self.emit_jcc_rel32(0x83)); // JAE -> helper
        self.buf.emit(&[0x4C, 0x39, 0xD2]); // CMP RDX, R10
        let young_value = self.emit_jcc_rel32(0x82); // JB -> young value
        self.buf.emit(&[0x49, 0x3B, 0x93]); // CMP RDX, [R11 + disp32]
        self.buf.emit(&old_end);
        inline_store.push(self.emit_jcc_rel32(0x82)); // JB -> old -> old: no card
        self.patch_rel32_to_here(young_value);
        if precise {
            // The ELEMENT's slot, the mark the helper hands the collector on
            // an element-precise table (`emit_aastore_write_barrier_args`).
            self.emit_ref_aastore_index_rcx(index);
            self.buf.emit(&[0x4C, 0x8D, 0x54, 0xC8, header_disp8]); // LEA R10, [RAX + RCX*8 + disp8]
            self.buf.emit(&[0x4D, 0x3B, 0x93]); // CMP R10, [R11 + disp32]
            self.buf.emit(&old_end);
            to_helper.push(self.emit_jcc_rel32(0x83)); // JAE -> helper
        } else {
            self.buf.emit(&[0x49, 0x89, 0xC2]); // MOV R10, RAX
        }
        // &cards[(mark - old_base) >> CARD_SHIFT]: `cards_neg` is the map's
        // address negated, so subtracting it adds the base.
        self.emit_ir_card_address_from_r10(view_addr, card_shift);
        // PRE: clean card -> helper, or (gcf f2/p, card-store form) marked here.
        // The element mark re-forms `[RAX + RCX*8 + HEADER]`: RCX still holds
        // the zero-extended index the LEA above used.
        let mark = if precise {
            IrCardMark::Element(header_disp8)
        } else {
            IrCardMark::Rax
        };
        let store = ir_card_store_barrier_enabled().then_some((view_addr, mark, card_shift));
        // gcj j2/jt2 (`CRATONVM_JIT_POST_ONLY_CARD_MARK`): no PRE check; the
        // POST edge below marks a clean card. One reading for both halves.
        let post_only = crate::x64::jit_post_only_card_mark_enabled();
        if post_only {
            crate::x64::note_post_only_card_site_in(view_addr);
        } else {
            self.emit_ir_card_pre_check(to_helper, store);
        }
        if let Some(pre) = pre {
            self.emit_ir_gate_byte_zero_cmp(pre);
            to_helper.push(self.emit_jcc_rel32(0x85)); // JNE -> helper
        }
        if !precise {
            self.emit_ref_aastore_index_rcx(index);
        }
        self.emit_gpr_array_elem_store(MemKind::Long, ArrayStoreSource::Reg(RDX));
        self.emit_ir_card_post_compare(view_addr, post_only, done);
        // gci i12/wb item 3: the store is done; only the card is owed (RCX
        // still holds the zero-extended index the store used).
        if post_only {
            self.emit_ir_card_post_only_clean(
                precise.then_some(header_disp8),
                store,
                to_helper,
                done,
            );
        } else {
            self.emit_ir_card_post_clean(precise.then_some(header_disp8), to_helper, done);
        }
    }

    /// The POST card-byte test both inline card arms make after the store:
    /// `CMP byte [R10], DIRTY; JE done`, R10 = `&cards[idx]`.
    ///
    /// gcj j2/jt2 residual 5: under `CRATONVM_JIT_POST_ONLY_CARD_MARK` this is
    /// the arm's ONLY card compare, so under `CRATONVM_DBG_IR_REF_STORE_TRACE=1`
    /// it counts its outcome in the view's trace words the PRE test counts in
    /// otherwise ([`Self::emit_ir_card_pre_check`]); `gen_jit_card_ir_dirty` /
    /// `_clean` then read the post-only arm too instead of zero. R11 is
    /// reloaded with the view first (the store paths use it as scratch); both
    /// edges leave it so, which the clean edge's callers allow (they reload
    /// or overwrite it). Untraced, or with PRE present, the two classic
    /// instructions.
    pub(in crate::ir_lower) fn emit_ir_card_post_compare(
        &mut self,
        view_addr: usize,
        post_only: bool,
        done: &mut Vec<usize>,
    ) {
        let dirty = crate::x64::CARD_DIRTY_BYTE;
        self.buf.emit(&[0x41, 0x80, 0x3A, dirty]); // CMP byte [R10], DIRTY
        if post_only && crate::x64::ir_ref_store_trace_enabled() {
            let clean_edge = self.emit_jcc_rel32(0x85); // JNE -> clean
            self.emit_mov_reg_imm64(R11, view_addr as u64); // Cast: a baked data address
                                                            // LOCK INC qword [R11 + disp8]: F0, REX.W+B, FF /0, ModRM 01 000 011.
            self.buf
                .emit(&[0xF0, 0x49, 0xFF, 0x43, IR_CARD_TRACE_DIRTY]);
            done.push(self.emit_jmp_rel32()); // dirty -> done
            self.patch_rel32_to_here(clean_edge);
            self.emit_mov_reg_imm64(R11, view_addr as u64); // Cast: a baked data address
            self.buf
                .emit(&[0xF0, 0x49, 0xFF, 0x43, IR_CARD_TRACE_CLEAN]);
        } else {
            done.push(self.emit_jcc_rel32(0x84)); // JE -> done (POST)
        }
    }

    /// The PRE card-byte test both inline card arms make
    /// ([`Self::emit_ir_aastore_card_arm`], [`Self::emit_ir_putfield_card_arm`]):
    /// `CMP byte [R10], DIRTY; JNE helper`, R10 = `&cards[idx]`, R11 = the view.
    ///
    /// gcf f1/p: under `CRATONVM_DBG_IR_REF_STORE_TRACE=1` it also counts the
    /// outcome in the card view's own trace words (`JitCardView::trace`,
    /// `IR_CARD_TRACE_DIRTY` / `_CLEAN`), because the gce ve2 A/B of
    /// `CRATONVM_JIT_INLINE_CARD_MARK` had no run-time line that could show
    /// this arm engaging: its inline stores bump neither
    /// `IR_REF_STORE_INLINE_TAKEN` nor a bail counter. A view word is per VM
    /// and adds no process `static`. Untraced, the two instructions are the
    /// ones each arm emitted inline before.
    ///
    /// gcf f2/p: `store` = `Some((view, mark, card_shift))` under
    /// `CRATONVM_JIT_CARD_STORE_BARRIER=1`: the CLEAN edge marks the card
    /// inline ([`Self::emit_ir_card_store_inline`]) and continues to the
    /// store, instead of taking the helper; only an index at or above the
    /// view's scan-bound mirror still takes it. `None` emits exactly the
    /// bytes above.
    pub(in crate::ir_lower) fn emit_ir_card_pre_check(
        &mut self,
        to_helper: &mut Vec<usize>,
        store: Option<(usize, IrCardMark, u8)>,
    ) {
        let dirty = crate::x64::CARD_DIRTY_BYTE;
        if let Some((view, mark, card_shift)) = store {
            self.buf.emit(&[0x41, 0x80, 0x3A, dirty]); // CMP byte [R10], DIRTY
            let dirty_edge = self.emit_jcc_rel32(0x84); // JE -> dirty
            let traced = crate::x64::ir_ref_store_trace_enabled();
            if traced {
                // LOCK INC qword [R11 + disp8]: F0, REX.W+B, FF /0, ModRM 01 000 011.
                self.buf
                    .emit(&[0xF0, 0x49, 0xFF, 0x43, IR_CARD_TRACE_CLEAN]);
            }
            self.emit_ir_card_store_inline(
                ir_card_view_has_summary(view),
                mark,
                card_shift,
                to_helper,
            );
            let marked = self.emit_jmp_rel32();
            self.patch_rel32_to_here(dirty_edge);
            if traced {
                self.buf
                    .emit(&[0xF0, 0x49, 0xFF, 0x43, IR_CARD_TRACE_DIRTY]);
            }
            self.patch_rel32_to_here(marked);
            return;
        }
        self.buf.emit(&[0x41, 0x80, 0x3A, dirty]); // CMP byte [R10], DIRTY
        if crate::x64::ir_ref_store_trace_enabled() {
            let dirty_edge = self.emit_jcc_rel32(0x84); // JE -> dirty
                                                        // LOCK INC qword [R11 + disp8]: F0, REX.W+B, FF /0, ModRM 01 000 011.
            self.buf
                .emit(&[0xF0, 0x49, 0xFF, 0x43, IR_CARD_TRACE_CLEAN]);
            to_helper.push(self.emit_jmp_rel32()); // clean -> helper
            self.patch_rel32_to_here(dirty_edge);
            self.buf
                .emit(&[0xF0, 0x49, 0xFF, 0x43, IR_CARD_TRACE_DIRTY]);
        } else {
            to_helper.push(self.emit_jcc_rel32(0x85)); // JNE -> helper (PRE)
        }
    }

    /// The card address both inline card arms form, from the mark in R10
    /// (R11 = the view): `R10 = &cards[(R10 - old_base) >> CARD_SHIFT]`.
    ///
    /// The classic form is `SUB R10, [R11 + old_base]; SHR R10, shift;
    /// SUB R10, [R11 + cards_neg]` (`cards_neg` is the map's address negated,
    /// so subtracting it adds the base) -- the bytes both arms always emitted.
    /// gcj j2/jt2 (`CRATONVM_JIT_CARD_BYTE_MAP_BASE`, opt-in), on a view whose
    /// covered range is card-aligned (`JitCardView::FLAG_BYTE_MAP_BASE`):
    /// HotSpot's `SHR R10, shift; SUB R10, [R11 + card_base_neg]`, one load
    /// and one instruction fewer, the same card byte.
    pub(in crate::ir_lower) fn emit_ir_card_address_from_r10(
        &mut self,
        view_addr: usize,
        card_shift: u8,
    ) {
        if crate::x64::jit_card_byte_map_base_enabled()
            && crate::x64::card_view_has_byte_map_base(view_addr)
        {
            self.buf.emit(&[0x49, 0xC1, 0xEA, card_shift]); // SHR R10, imm8
            self.buf.emit(&[0x4D, 0x2B, 0x93]); // SUB R10, [R11 + disp32]
            self.buf
                .emit(&crate::x64::CARD_VIEW_CARD_BASE_NEG.to_le_bytes());
            return;
        }
        self.buf.emit(&[0x4D, 0x2B, 0x93]); // SUB R10, [R11 + disp32]
        self.buf.emit(&crate::x64::CARD_VIEW_OLD_BASE.to_le_bytes());
        self.buf.emit(&[0x49, 0xC1, 0xEA, card_shift]); // SHR R10, imm8
        self.buf.emit(&[0x4D, 0x2B, 0x93]); // SUB R10, [R11 + disp32]
        self.buf
            .emit(&crate::x64::CARD_VIEW_CARDS_NEG.to_le_bytes());
    }

    /// gcj j2/jt2 (`CRATONVM_JIT_POST_ONLY_CARD_MARK`) — the CLEAN edge of a
    /// post-only card arm: the store has happened and no PRE check marked the
    /// card, so this marks it. Entry: R10 = `&cards[idx]` (read clean), RAX =
    /// the receiver / array, RDX = the value, RCX = the zero-extended index for
    /// an `element` mark; R11 is NOT the view (the arm loaded the SATB gate
    /// address into it, and the legacy `putfield` shape its field count).
    ///
    /// Under the card-store form (`store`, `CRATONVM_JIT_CARD_STORE_BARRIER`)
    /// the card is marked inline after R11 is reloaded with the view
    /// ([`Self::emit_ir_card_store_inline`]); an index at or above the
    /// scan-bound mirror, and every clean card without that form, is the
    /// card-only `write_barrier` call ([`Self::emit_ir_card_post_clean`]),
    /// never the full helper's second store.
    ///
    /// A take-over freeze between the store and the card byte is the
    /// collector's to finish: see `x64::jit_post_only_card_mark_enabled`
    /// (RAX and RDX stay live from the store to the card byte on every edge).
    /// t1 lane D (`x64::jit_whole_heap_card_mark_enabled`, the soundness
    /// argument) — the card mark AFTER the store on a whole-heap card map:
    ///
    /// ```text
    ///   mov  r11, imm64 &view
    ///   r10 = element ? lea [rax + rcx*8 + disp8] : rax
    ///   sub  r10, [r11 + hull_lo]
    ///   cmp  r10, [r11 + hull_len] ; jae miss
    ///   shr  r10, CARD_SHIFT
    ///   sub  r10, [r11 + hull_buf_neg]
    ///   mov  byte [r10], DIRTY ; jmp done
    /// miss:                                     outside the hull
    ///   call write_barrier(vm, mark, rdx) ; jmp done   (emit_ir_card_post_clean)
    /// ```
    ///
    /// Entry: the store is done; RAX = the receiver / array, RDX = the value,
    /// RCX = the zero-extended index for an `element` mark. RAX and RDX stay
    /// live from the store to the card byte (the post-only completion's
    /// contract).
    pub(in crate::ir_lower) fn emit_ir_whole_heap_card_mark(
        &mut self,
        view_addr: usize,
        card_shift: u8,
        element: Option<u8>,
        to_helper: &mut Vec<usize>,
        done: &mut Vec<usize>,
    ) {
        if self
            .emit_ir_whole_heap_card_mark_sib_fast(view_addr, card_shift, element, to_helper, done)
        {
            return;
        }
        let baked = crate::x64::card_view_hull_baked(view_addr);
        let miss;
        if let Some((lo, cards, map)) = baked {
            // The hull never moves: no data load but the card byte itself.
            match element {
                Some(d) => self.buf.emit(&[0x4C, 0x8D, 0x54, 0xC8, d]), // LEA R10, [RAX + RCX*8 + disp8]
                None => self.buf.emit(&[0x49, 0x89, 0xC2]),             // MOV R10, RAX
            }
            self.buf.emit(&[0x49, 0xC1, 0xEA, card_shift]); // SHR R10, imm8
                                                            // 32-bit index (`x64::hull_index_imm32` says why a wrap is harmless).
            self.buf.emit(&[0x41, 0x81, 0xEA]); // SUB R10D, imm32
            self.buf
                .emit(&crate::x64::hull_index_imm32(lo).to_le_bytes());
            self.buf.emit(&[0x41, 0x81, 0xFA]); // CMP R10D, imm32 (cards)
            self.buf.emit(&cards.to_le_bytes());
            miss = self.emit_jcc_rel32(0x83); // JAE -> outside the hull
            self.emit_mov_reg_imm64(R11, map as u64); // Cast: baked hull map
            self.buf.emit(&[0x4D, 0x01, 0xDA]); // ADD R10, R11
        } else {
            self.emit_mov_reg_imm64(R11, view_addr as u64); // Cast: a baked data address
            match element {
                Some(d) => self.buf.emit(&[0x4C, 0x8D, 0x54, 0xC8, d]), // LEA R10, [RAX + RCX*8 + disp8]
                None => self.buf.emit(&[0x49, 0x89, 0xC2]),             // MOV R10, RAX
            }
            self.buf.emit(&[0x4D, 0x2B, 0x93]); // SUB R10, [R11 + disp32]
            self.buf.emit(&crate::x64::CARD_VIEW_HULL_LO.to_le_bytes());
            self.buf.emit(&[0x4D, 0x3B, 0x93]); // CMP R10, [R11 + disp32]
            self.buf.emit(&crate::x64::CARD_VIEW_HULL_LEN.to_le_bytes());
            miss = self.emit_jcc_rel32(0x83); // JAE -> outside the hull
            self.buf.emit(&[0x49, 0xC1, 0xEA, card_shift]); // SHR R10, imm8
            self.buf.emit(&[0x4D, 0x2B, 0x93]); // SUB R10, [R11 + disp32]
            self.buf
                .emit(&crate::x64::CARD_VIEW_HULL_BUF_NEG.to_le_bytes());
        }
        // Read first (always on a table with a summary): an already-dirty
        // card is not stored again, and its summary byte is already set.
        let summary = ir_card_view_has_summary(view_addr);
        if summary || crate::x64::jit_whole_heap_cond_card_mark_enabled() {
            self.buf
                .emit(&[0x41, 0x80, 0x3A, crate::x64::CARD_DIRTY_BYTE]); // CMP byte [R10], DIRTY
            done.push(self.emit_jcc_rel32(0x84)); // JE -> done
        }
        if let (true, Some((lo, _, map))) = (summary, baked) {
            // The baked clean edge: every address from the SAME 32-bit index
            // the range test admitted (`x64::hull_index_imm32`), summary
            // byte, card byte, summary byte.
            let summary_map = crate::x64::card_view_hull_summary_map(view_addr);
            for (extra, base) in [
                (IR_CARD_SUMMARY_SHIFT, summary_map),
                (0, map),
                (IR_CARD_SUMMARY_SHIFT, summary_map),
            ] {
                match element {
                    Some(d) => self.buf.emit(&[0x4C, 0x8D, 0x54, 0xC8, d]), // LEA R10, [RAX + RCX*8 + disp8]
                    None => self.buf.emit(&[0x49, 0x89, 0xC2]),             // MOV R10, RAX
                }
                self.buf.emit(&[0x49, 0xC1, 0xEA, card_shift]); // SHR R10, imm8
                self.buf.emit(&[0x41, 0x81, 0xEA]); // SUB R10D, imm32
                self.buf
                    .emit(&crate::x64::hull_index_imm32(lo).to_le_bytes());
                if extra != 0 {
                    self.buf.emit(&[0x49, 0xC1, 0xEA, extra]); // SHR R10, imm8
                }
                self.emit_mov_reg_imm64(R11, base as u64); // Cast: baked map address
                self.buf.emit(&[0x4D, 0x01, 0xDA]); // ADD R10, R11
                self.buf
                    .emit(&[0x41, 0xC6, 0x02, crate::x64::CARD_DIRTY_BYTE]); // MOV byte [R10], DIRTY
            }
        } else if summary {
            // Summary byte, card byte, summary byte (`mark_dirty_lockfree`'s order).
            let s = card_shift + IR_CARD_SUMMARY_SHIFT;
            for (shift, word) in [
                (s, crate::x64::CARD_VIEW_HULL_SUMMARY_NEG),
                (card_shift, crate::x64::CARD_VIEW_HULL_BUF_NEG),
                (s, crate::x64::CARD_VIEW_HULL_SUMMARY_NEG),
            ] {
                match element {
                    Some(d) => self.buf.emit(&[0x4C, 0x8D, 0x54, 0xC8, d]), // LEA R10, [RAX + RCX*8 + disp8]
                    None => self.buf.emit(&[0x49, 0x89, 0xC2]),             // MOV R10, RAX
                }
                self.buf.emit(&[0x4D, 0x2B, 0x93]); // SUB R10, [R11 + disp32]
                self.buf.emit(&crate::x64::CARD_VIEW_HULL_LO.to_le_bytes());
                self.buf.emit(&[0x49, 0xC1, 0xEA, shift]); // SHR R10, imm8
                self.buf.emit(&[0x4D, 0x2B, 0x93]); // SUB R10, [R11 + disp32]
                self.buf.emit(&word.to_le_bytes());
                self.buf
                    .emit(&[0x41, 0xC6, 0x02, crate::x64::CARD_DIRTY_BYTE]); // MOV byte [R10], DIRTY
            }
        } else {
            self.buf
                .emit(&[0x41, 0xC6, 0x02, crate::x64::CARD_DIRTY_BYTE]); // MOV byte [R10], DIRTY
        }
        done.push(self.emit_jmp_rel32());
        self.patch_rel32_to_here(miss);
        // t1 lane R (`x64::jit_whole_heap_miss_young_skip_enabled`): a young
        // receiver outside the hull (a moved semispace) needs no card and no
        // call. RAX = the receiver / array (entry contract).
        if crate::x64::jit_whole_heap_miss_young_skip_enabled() {
            self.buf.emit(&[
                0xF6,
                0x40,
                cratonvm_types::GC_FLAGS_BYTE_OFFSET as u8, // Cast: x86-64 disp8
                cratonvm_types::GC_FLAG_OLD_GEN,
            ]); // TEST byte [RAX + flags], OLD_GEN
            done.push(self.emit_jcc_rel32(0x84)); // JZ -> done (young receiver)
        }
        self.emit_ir_card_post_clean(element, to_helper, done);
    }

    pub(in crate::ir_lower) fn emit_ir_card_post_only_clean(
        &mut self,
        element: Option<u8>,
        store: Option<(usize, IrCardMark, u8)>,
        to_helper: &mut Vec<usize>,
        done: &mut Vec<usize>,
    ) {
        if let Some((view, mark, card_shift)) = store {
            self.emit_mov_reg_imm64(R11, view as u64); // Cast: a baked data address
            let mut refused: Vec<usize> = Vec::new();
            self.emit_ir_card_store_inline(
                ir_card_view_has_summary(view),
                mark,
                card_shift,
                &mut refused,
            );
            done.push(self.emit_jmp_rel32());
            for patch in refused {
                self.patch_rel32_to_here(patch);
            }
        }
        self.emit_ir_card_post_clean(element, to_helper, done);
    }

    /// gcf f2/p — this tier's compiled CARD STORE, the twin of the single-pass
    /// `emit_gen_card_store_inline` (`x64/objects.rs`; the soundness argument
    /// is written there): the collector's `mark_dirty_lockfree` clean->dirty
    /// arm inline.
    ///
    /// Entry: R11 = the view, R10 = `&cards[idx]` (read CLEAN), RAX (and RCX
    /// for an element mark) as [`IrCardMark`] says. Exit: R10 = `&cards[idx]`
    /// again, for the arm's POST test; R11 = the view. Pushes the "index at or
    /// above the scan-bound mirror" edge onto `to_helper`.
    ///
    /// ```text
    ///   r10 = (mark - old_base) >> CARD_SHIFT ; cmp r10, [r11 + scan_bound] ; jae helper
    ///  (r10 >>= 6 ; r10 -= [r11 + summary_neg] ; mov byte [r10], DIRTY)
    ///   r10 = ((mark - old_base) >> CARD_SHIFT) - cards_neg ; mov byte [r10], DIRTY
    ///  (r10 = ((mark - old_base) >> (CARD_SHIFT + 6)) - summary_neg ; mov byte [r10], DIRTY
    ///   r10 = ((mark - old_base) >> CARD_SHIFT) - cards_neg)
    /// ```
    pub(in crate::ir_lower) fn emit_ir_card_store_inline(
        &mut self,
        summary: bool,
        mark: IrCardMark,
        card_shift: u8,
        to_helper: &mut Vec<usize>,
    ) {
        let dirty = crate::x64::CARD_DIRTY_BYTE;
        let old_base = crate::x64::CARD_VIEW_OLD_BASE.to_le_bytes();
        let cards_neg = crate::x64::CARD_VIEW_CARDS_NEG.to_le_bytes();
        let scan_bound = IR_CARD_VIEW_SCAN_BOUND.to_le_bytes();
        let summary_neg = IR_CARD_VIEW_SUMMARY_NEG.to_le_bytes();
        // R10 = (mark - old_base) >> shift.
        let load_shifted = |s: &mut Self, shift: u8| {
            match mark {
                IrCardMark::Rax => s.buf.emit(&[0x49, 0x89, 0xC2]), // MOV R10, RAX
                IrCardMark::Element(d) => s.buf.emit(&[0x4C, 0x8D, 0x54, 0xC8, d]), // LEA R10, [RAX + RCX*8 + disp8]
            }
            s.buf.emit(&[0x4D, 0x2B, 0x93]); // SUB R10, [R11 + disp32]
            s.buf.emit(&old_base);
            s.buf.emit(&[0x49, 0xC1, 0xEA, shift]); // SHR R10, imm8
        };
        load_shifted(self, card_shift);
        self.buf.emit(&[0x4D, 0x3B, 0x93]); // CMP R10, [R11 + disp32]
        self.buf.emit(&scan_bound);
        to_helper.push(self.emit_jcc_rel32(0x83)); // JAE -> helper (above the mirror)
        if summary {
            self.buf.emit(&[0x49, 0xC1, 0xEA, IR_CARD_SUMMARY_SHIFT]); // SHR R10, imm8
            self.buf.emit(&[0x4D, 0x2B, 0x93]); // SUB R10, [R11 + disp32]
            self.buf.emit(&summary_neg);
            self.buf.emit(&[0x41, 0xC6, 0x02, dirty]); // MOV byte [R10], DIRTY (summary)
            load_shifted(self, card_shift);
        }
        self.buf.emit(&[0x4D, 0x2B, 0x93]); // SUB R10, [R11 + disp32]
        self.buf.emit(&cards_neg);
        self.buf.emit(&[0x41, 0xC6, 0x02, dirty]); // MOV byte [R10], DIRTY (card)
        if summary {
            load_shifted(self, card_shift + IR_CARD_SUMMARY_SHIFT);
            self.buf.emit(&[0x4D, 0x2B, 0x93]); // SUB R10, [R11 + disp32]
            self.buf.emit(&summary_neg);
            self.buf.emit(&[0x41, 0xC6, 0x02, dirty]); // MOV byte [R10], DIRTY (summary again)
                                                       // R10 = &cards[idx] again, for the POST test.
            load_shifted(self, card_shift);
            self.buf.emit(&[0x4D, 0x2B, 0x93]); // SUB R10, [R11 + disp32]
            self.buf.emit(&cards_neg);
        }
    }

    /// Inline PRIMITIVE `putfield` (`I J F D Z B C S`), the store twin of
    /// [`Self::emit_inline_compact_getfield`] and the optimizing tier's port of
    /// the single-pass `try_emit_inline_primitive_putfield` (round 11 wave 2,
    /// lane lower; `r11-irlower-no-inline-primitive-putfield`). Under compact
    /// layout every primitive field store in this tier was a `jit_putfield_*`
    /// CALL, where the baseline tier emits one or two `MOV`s.
    ///
    /// Entry contract: RAX holds the receiver, already null-checked by
    /// `emit_store_receiver_null_check` (or provably non-null). Returns `None`,
    /// having emitted NOTHING, when the site is not admitted; the caller then
    /// emits its helper call exactly as before. Returns `Some(done)` after
    /// emitting the guarded inline store: every declined run-time path falls
    /// through to the position this returns at, where the caller emits the
    /// UNCHANGED helper call, and the caller patches each `done` jump to the
    /// point after that call.
    ///
    /// What the helper does, reproduced (`jit_putfield_{int,long,float,
    /// double}`, the same list the single-pass port documents):
    ///
    /// 1. an unvalidated receiver is never dereferenced — the containment
    ///    check against the READ bounds, or the IR's `Ref` type proof, exactly
    ///    as the inline `getfield`'s primitive arm trusts it;
    /// 2. on a legacy object, `field_index < num_slots`, else the helper
    ///    (which drops the store);
    /// 3. a compact object stores the field's own width at its absolute
    ///    compact displacement `c_off`; a legacy object gets its whole 16-byte
    ///    `Value` cell, word 0 (tag + 32-bit payload) then word 1 (64-bit
    ///    payload or zero);
    /// 4. a sub-int value is narrowed to the field's width first.
    ///
    /// A replaced layout (the epoch guard) takes the helper. No barrier: a
    /// primitive store has none. A volatile store still gets its StoreLoad
    /// fence from `lower_block`, after whichever path ran.
    ///
    /// Clobbers RCX, RDX and (on the epoch guard's fallback form) R11; RAX is
    /// preserved, and no allocatable register is touched.
    pub(in crate::ir_lower) fn emit_inline_prim_putfield(
        &mut self,
        node_pc: Option<usize>,
        kind: MemKind,
        base: NodeId,
        value: NodeId,
        field_index: i64,
    ) -> Option<Vec<usize>> {
        // zt t32 (successor page section 7, `CRATONVM_ZGC_INLINE_PRIMITIVE_FIELDS`):
        // an armed ZGC barrier closes this arm only for an untrusted receiver
        // (a primitive store owes no barrier; the trusted one needs only the
        // null test, no read-bounds table).
        let armed_trusted = crate::x64::zgc_armed_inline_primitive_fields_enabled()
            && crate::x64::trusted_oop_receiver_getfield_enabled()
            && self
                .graph
                .nodes
                .get(base as usize)
                .is_some_and(|n| n.ty == IrType::Ref);
        if !self.inline_prim_putfield_on
            || !crate::x64::inline_putfield_enabled()
            || (crate::x64::narrow_oops_block_inline_fields() && !armed_trusted)
            || !cratonvm_types::compact_ref_fields_enabled()
        {
            return None;
        }
        let pc = node_pc?;
        // The resolved compact cell for THIS site, keyed `(pc, false)`: a
        // primitive store is always the non-reference half of the key.
        // nio21 wave 18 (lane L3): see `emit_inline_compact_getfield`.
        // JIT r23 wave 11b (lane L5): or this node's own row
        // (`synthetic_store_row`, `CRATONVM_JIT_IR_SYNTHETIC_STORE_FIELD_ROW`).
        let row = if self.graph.synthetic_field_access_pcs.contains(&pc) {
            self.synthetic_store_row
                .filter(|&(_, is_ref, _)| !is_ref && ir_synthetic_store_field_row_enabled())
        } else {
            self.compact_fields.get(&(pc, false)).copied()
        };
        // Round 16 (lane fieldguard, FG16-7): no compact row -> the legacy arm
        // alone, as for the inline `getfield` (`legacy_only_field_tag`).
        let legacy_only = row.is_none();
        let (c_off, c_is_ref, type_tag) = match row {
            Some(r) => r,
            None if legacy_only => (0, false, legacy_only_store_tag(kind)),
            None => return None,
        };
        // Descriptor agreement, as the inline `getfield` demands it: a resolver
        // that fabricated a compact slot must not steer a store width.
        let width_ok = match type_tag {
            b'I' | b'Z' | b'B' | b'C' | b'S' => kind == MemKind::Int,
            b'J' => kind == MemKind::Long,
            b'F' => kind == MemKind::Float,
            b'D' => kind == MemKind::Double,
            _ => false,
        };
        if c_is_ref || !width_ok {
            return None;
        }
        let trusted_oop_receiver = crate::x64::trusted_oop_receiver_getfield_enabled()
            && self
                .graph
                .nodes
                .get(base as usize)
                .is_some_and(|n| n.ty == IrType::Ref);
        if !trusted_oop_receiver && self.read_bounds_addr == 0 {
            return None;
        }
        // Since the 8-byte compact header (dev, 2026-09-24) a compact layout
        // carries ABSOLUTE displacements, header included -- the same
        // `c_off as i32` `emit_inline_compact_getfield` bakes.
        let cell_off = i32::try_from(c_off).ok()?;
        let legacy_off = usize::try_from(field_index)
            .ok()
            .and_then(|f| f.checked_mul(SLOT_SIZE))
            .and_then(|b| b.checked_add(HEADER_SIZE))
            .and_then(|o| i32::try_from(o).ok())?;
        let legacy_hi_off = legacy_off.checked_add(FIELD_CELL_PAYLOAD64_OFFSET as i32)?;
        let field_index_imm = i32::try_from(field_index).ok()?;

        let mut slow: Vec<usize> = Vec::new();
        // `cell_off` is a compile-time claim about this class's compact layout.
        // The guard touches RAX in neither form. Round 16 (lane fieldguard,
        // FG16-1): the guard may be a deopting one, or absent when a
        // dominating deopting guard already proved this receiver's layout
        // (`emit_field_layout_guard`). A store may deopt here only where
        // `store_cannot_deopt` already predicts a deopt at its bci.
        let store_may_deopt = !store_receiver_cannot_be_null(self.graph, base);
        // JIT round 25 (item 4, `CRATONVM_JIT_IR_PROVEN_RECEIVER_FIELDS`):
        // decided before anything is emitted -- `Establish` needs the layout
        // guard below not to rejoin. The caller's null check never rejoins
        // (it deopts or takes the implicit-exception pad), so the receiver is
        // non-null on every path through here. A store deopts only where the
        // prediction counts it (`store_may_deopt`), as the layout guard does.
        let proven_mode = if self.proven_fields_on && !legacy_only && trusted_oop_receiver {
            self.proven_receiver_mode(base, pc, store_may_deopt)
        } else {
            None
        };
        // A legacy-only site bakes no compact displacement, so it owes none.
        let mut epoch_rejoins = false;
        if !legacy_only {
            let epoch = self.emit_field_layout_guard(base, pc, store_may_deopt);
            epoch_rejoins = epoch.is_some();
            slow.extend(epoch);
        }
        if !trusted_oop_receiver {
            // Clobbers RCX and RDX, never RAX.
            self.emit_receiver_align_and_containment(&mut slow);
        }
        // The value, narrowed to a sub-int field's width, BEFORE the layout
        // branch: both arms store RDX (the single-pass port stored the field
        // index from its legacy arm until this ordering was fixed there).
        self.gp_load_value(RDX, value);
        match type_tag {
            b'B' => self.buf.emit(&[0x48, 0x0F, 0xBE, 0xD2]), // MOVSX RDX, DL
            b'S' => self.buf.emit(&[0x48, 0x0F, 0xBF, 0xD2]), // MOVSX RDX, DX
            b'C' => self.buf.emit(&[0x0F, 0xB7, 0xD2]),       // MOVZX EDX, DX
            b'Z' => self.buf.emit(&[0x83, 0xE2, 0x01]),       // AND EDX, 1
            _ => {}
        }
        if self.proven_fields_on {
            match proven_mode {
                // Item 4: no compact test and no legacy arm -- the proof (or
                // the deopting guard emitted here) rules a legacy object out.
                // Every declined path left in `slow` is the epoch guard's.
                Some(mode) => {
                    if mode == ProvenReceiverMode::Establish {
                        self.emit_compact_receiver_trap(base, pc, !epoch_rejoins);
                        self.proven_field_census.store_establish += 1;
                    } else {
                        self.proven_field_census.store_proven += 1;
                    }
                    self.emit_compact_prim_field_store(type_tag, cell_off);
                    if slow.is_empty() {
                        return Some(Vec::new());
                    }
                    let done = vec![self.emit_jmp_rel32()];
                    for p in slow {
                        self.patch_rel32_to_here(p);
                    }
                    return Some(done);
                }
                None => {
                    self.proven_field_census.store_full += 1;
                    self.proven_field_site_census("store", base, pc, store_may_deopt, true);
                }
            }
        }
        // Per-OBJECT layout, as the inline `getfield` decides it.
        self.buf.emit(&[0xF6, 0x80]); // TEST byte [RAX + disp32], imm8
        self.buf
            .emit(&(cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32).to_le_bytes());
        self.buf.emit_byte(cratonvm_types::GC_FLAG_COMPACT);
        let mut done: Vec<usize> = Vec::new();
        if legacy_only {
            // FG16-7: a compact object (its class gained a layout after this
            // compile resolved its rows) takes the helper.
            slow.push(self.emit_jcc_rel32(0x85)); // JNZ -> helper
        } else {
            let legacy = self.emit_jcc_rel32(0x84); // JZ -> the 16-byte cell
                                                    // --- compact object: the field's own width at the packed offset ---
            self.emit_compact_prim_field_store(type_tag, cell_off);
            done.push(self.emit_jmp_rel32());
            // --- legacy object: a whole `Value` cell, word 0 then word 1, the
            //     words `write_value_atomic` leaves (tags: Int 0, Long 1, Float 2,
            //     Double 3; sub-int fields are `Value::Int`) ---
            self.patch_rel32_to_here(legacy);
        }
        // Bounds, LEGACY shape only: `num_slots <= field_index` (unsigned) ->
        // helper, which drops the store exactly as it always did. Only a
        // legacy (long-header) object has a field count at `NUM_SLOTS_OFFSET`;
        // since the 8-byte compact header (dev, 2026-09-24) offset 8 of a
        // compact instance is its first field, and its layout is its class's,
        // which the epoch guard above pinned.
        self.buf.emit(&[0x8B, 0x88]); // MOV ECX, dword [RAX + disp32]
        self.buf.emit(&legacy_num_slots_disp().to_le_bytes());
        self.buf.emit(&[0x81, 0xF9]); // CMP ECX, imm32
        self.buf.emit(&field_index_imm.to_le_bytes());
        slow.push(self.emit_jcc_rel32(0x86)); // JBE -> helper
        let tag: u32 = match type_tag {
            b'J' => 1,
            b'F' => 2,
            b'D' => 3,
            _ => 0,
        };
        if matches!(type_tag, b'J' | b'D') {
            // word 0 = tag (bytes 4..8 zero); word 1 = the 64-bit payload.
            self.buf.emit(&[0x48, 0xC7, 0x80]); // MOV qword [RAX + disp32], imm32
            self.buf.emit(&legacy_off.to_le_bytes());
            self.buf.emit(&tag.to_le_bytes());
            self.buf.emit(&[0x48, 0x89, 0x90]); // MOV qword [RAX + disp32], RDX
            self.buf.emit(&legacy_hi_off.to_le_bytes());
        } else {
            // word 0 = tag | payload32 << 32; word 1 = 0.
            self.buf.emit(&[0x89, 0xD1]); // MOV ECX, EDX (zero-extends)
            self.buf.emit(&[0x48, 0xC1, 0xE1, 0x20]); // SHL RCX, 32
            if tag != 0 {
                // Cast: `tag` is 0..=3, a positive imm8.
                self.buf.emit(&[0x48, 0x83, 0xC9, tag as u8]); // OR RCX, imm8
            }
            self.buf.emit(&[0x48, 0x89, 0x88]); // MOV qword [RAX + disp32], RCX
            self.buf.emit(&legacy_off.to_le_bytes());
            self.buf.emit(&[0x31, 0xC9]); // XOR ECX, ECX
            self.buf.emit(&[0x48, 0x89, 0x88]); // MOV qword [RAX + disp32], RCX
            self.buf.emit(&legacy_hi_off.to_le_bytes());
        }
        done.push(self.emit_jmp_rel32());
        // --- every declined path: the caller's unchanged helper call ---
        for p in slow {
            self.patch_rel32_to_here(p);
        }
        Some(done)
    }

    /// May this compile store a reference field inline under G1? See
    /// [`Self::emit_g1_ir_ref_putfield`].
    pub(in crate::ir_lower) fn g1_ref_store_available(&self) -> bool {
        ir_g1_ref_store_enabled()
            && crate::x64::g1_inline_barrier_enabled()
            && crate::x64::g1_barrier_table_live(self.g1_barrier_addr)
            && self.putfield_object != 0
    }

    /// The G1 inline reference store, in this tier's "decide first" form
    /// (`CRATONVM_JIT_IR_G1_REF_STORE`, default on).
    ///
    /// Under G1 this tier took `jit_putfield_object` for EVERY reference store:
    /// G1 publishes no gate plan (`ref_store_gates` is `None`), and that
    /// decline predates the single-pass tier's own G1 arm (F-08), which stores
    /// inline and filters the post barrier. So an allocating method -- every
    /// `new Node(l, r)` -- paid a Rust call per field in this tier and none in
    /// the other.
    ///
    /// The store stays inline only when NO barrier can have work to do, and
    /// every other case takes the helper, which does the store and both
    /// barriers itself (this tier has no heap pointer in its frame to call the
    /// collector's barrier with, which is why it decides first):
    ///
    /// * **compact receiver** -- a legacy one takes the helper;
    /// * **the old value is null** -- so the SATB pre-barrier has nothing to
    ///   enqueue whether marking is active or not. That is the shape of every
    ///   constructor store, and what the single-pass arm tests too;
    /// * **the new value is null, or in the receiver's region** -- G1's
    ///   `post_write_barrier_rset` returns for both without recording a
    ///   remembered-set edge. The region test is F-08's, against the same
    ///   `JIT_G1_BARRIER` table (arena base, region mask).
    ///
    /// # g1put (2026-10-05, `CRATONVM_JIT_IR_G1_GATED_REF_STORE`, default on)
    ///
    /// Two of those conditions sent the commonest overwrite to the full helper:
    /// `HashMap.put` of an existing key (`e.value = value`) has a non-null old
    /// value, and its value (a cached box) lives in another region than the
    /// node. Each such put paid `jit_putfield_object` -> `G1Collector::set_field`
    /// (header, layout, autobox screen, SATB test, store, two region lookups,
    /// card, edge memo): `R16OsrdoorParts put-osr` 76 ns under G1 against 14
    /// under ZGC. With the switch on:
    ///
    /// * **old value non-null** is helper-bound only while the SATB pre-gate
    ///   byte is armed ([`Self::satb_pre_gate`],
    ///   `gc::gen_heap::jit_satb_pre_gate_addr`). That byte is armed BEFORE
    ///   G1's queue goes active and disarmed AFTER it goes inactive, and the
    ///   activation is a stop-the-world step, so with no safepoint between the
    ///   test and the store a clear byte proves the pre-barrier owes nothing --
    ///   the same proof the generational gated arm relies on. (gci i12/wb: for
    ///   a thread that ARRIVES at the pause. One the BUG-03 takeover freezes
    ///   between the test and the store is covered because RAX holds the
    ///   receiver across the window and the G1 concurrent-start pause scans
    ///   what frozen peers hold, `G1Collector::scan_frozen_held_objects`, as
    ///   the generational one does, `initial_mark_with_frozen`.)
    /// * **cross-region value** stores inline and then calls the lean
    ///   `jit_g1_post_write_barrier` (`vm_ptr`, receiver, value), the
    ///   remembered-set half the single-pass F-08 arm calls, instead of the
    ///   helper that would also redo the store and its screens.
    ///
    /// `=0` restores the bytes above exactly.
    #[allow(clippy::too_many_arguments)]
    pub(in crate::ir_lower) fn emit_g1_ir_ref_putfield(
        &mut self,
        pc: usize,
        base: NodeId,
        value: NodeId,
        field_index: i64,
        c_off: u32,
        c_is_ref: bool,
        type_tag: u8,
        fresh_receiver: bool,
    ) -> bool {
        if !c_is_ref || !matches!(type_tag, b'L' | b'[') {
            crate::metrics::note_ir_ref_store_decline(5);
            return false;
        }
        let trusted_oop_receiver = crate::x64::trusted_oop_receiver_getfield_enabled()
            && self.graph.nodes[base as usize].ty == IrType::Ref;
        if !trusted_oop_receiver && self.read_bounds_addr == 0 {
            crate::metrics::note_ir_ref_store_decline(6);
            return false;
        }
        let cell_off = c_off as i32; // absolute compact displacement
        let mut to_helper: Vec<usize> = Vec::new();
        // JIT round 25 (item 4 wave 2): under the proven-receiver switch, the
        // shared field guard for the epoch (as `emit_inline_prim_putfield`),
        // and the compact test dropped when a proof covers the receiver, or
        // put down as the deopting guard where the site may establish one.
        let proven_mode = if self.proven_fields_on && trusted_oop_receiver {
            self.proven_receiver_mode(base, pc, !store_receiver_cannot_be_null(self.graph, base))
        } else {
            None
        };
        let mut epoch_rejoins = false;
        if self.proven_fields_on {
            let epoch = self.emit_field_layout_guard(
                base,
                pc,
                !store_receiver_cannot_be_null(self.graph, base),
            );
            epoch_rejoins = epoch.is_some();
            to_helper.extend(epoch);
        } else {
            to_helper.extend(self.emit_layout_epoch_guard());
        }
        // RAX: the receiver, already null-checked by the caller.
        if !trusted_oop_receiver {
            self.emit_receiver_align_and_containment(&mut to_helper);
        }
        match proven_mode {
            Some(ProvenReceiverMode::Proven) => {
                self.proven_field_census.ref_store_proven += 1;
            }
            Some(ProvenReceiverMode::Establish) => {
                // No rejoining edge ahead (predicted): the caller's null check
                // deopts, the receiver is trusted, the epoch guard deopts.
                self.emit_compact_receiver_trap(base, pc, !epoch_rejoins);
                self.proven_field_census.store_establish += 1;
            }
            None => {
                // Compact receiver only.
                self.buf.emit(&[0xF6, 0x80]); // TEST byte [RAX + disp32], imm8
                self.buf
                    .emit(&(cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32).to_le_bytes());
                self.buf.emit_byte(cratonvm_types::GC_FLAG_COMPACT);
                to_helper.push(self.emit_jcc_rel32(0x84)); // JZ -> helper
            }
        }
        // g1put: the gated form (see the doc above), only with both of its
        // addresses wired.
        let gated = ir_g1_gated_ref_store_enabled()
            && self.satb_pre_gate != 0
            && self.g1_post_write_barrier != 0;
        if !fresh_receiver {
            // g1put: no mark running -> the old value does not matter.
            let mut no_satb: Option<usize> = None;
            if gated {
                self.emit_ir_satb_pre_cmp(self.satb_pre_gate, true);
                no_satb = Some(self.emit_jcc_rel32(0x84)); // JE -> past the old-value test
            }
            // The old value must be null: nothing for the SATB pre-barrier.
            self.buf.emit(&[0x48, 0x8B, 0x88]); // MOV RCX, [RAX + disp32]
            self.buf.emit(&cell_off.to_le_bytes());
            self.buf.emit(&[0x48, 0x85, 0xC9]); // TEST RCX, RCX
            to_helper.push(self.emit_jcc_rel32(0x85)); // JNZ -> helper
            if let Some(p) = no_satb {
                self.patch_rel32_to_here(p);
            }
        }
        let value_is_null = matches!(
            self.graph.nodes.get(value as usize).map(|n| &n.op),
            Some(Op::Const(0))
        );
        let g1_geom = crate::ir_g1_edge_filter::g1_edge_filter_geom(&self.direct_helpers);
        let mut cross_region_opt: Option<usize> = None;
        if value_is_null {
            self.emit_ref_store_path_trace(&crate::metrics::IR_REF_STORE_INLINE_TAKEN);
            self.buf.emit(&[0x48, 0xC7, 0x80]); // MOV qword [RAX + disp32], 0
            self.buf.emit(&cell_off.to_le_bytes());
            self.buf.emit(&0i32.to_le_bytes());
        } else {
            self.gp_load_value(RDX, value);
            // A null value needs no post barrier.
            self.buf.emit(&[0x48, 0x85, 0xD2]); // TEST RDX, RDX
            let null_value = self.emit_jcc_rel32(0x84); // JZ -> store
                                                        // Same region: ((recv - base) ^ (val - base)) & region_mask == 0.
            if let Some(ref g) = g1_geom {
                self.emit_mov_reg_imm64(R9, g.arena_base as u64);
                self.buf.emit(&[0x48, 0x89, 0xC1]); // MOV RCX, RAX
                self.buf.emit(&[0x4C, 0x29, 0xC9]); // SUB RCX, R9
                self.buf.emit(&[0x49, 0x89, 0xD2]); // MOV R10, RDX
                self.buf.emit(&[0x4D, 0x29, 0xCA]); // SUB R10, R9
                self.buf.emit(&[0x4C, 0x31, 0xD1]); // XOR RCX, R10
                self.buf.emit(&[0x49, 0x89, 0xCB]); // MOV R11, RCX
                self.buf.emit(&[0x49, 0xC1, 0xEB, g.region_shift]); // SHR R11, region_shift
            } else {
                self.emit_mov_reg_imm64(R11, self.g1_barrier_addr as u64);
                // Load G1B_ARENA_BASE once into R9 (`MOV R9, [R11 + disp32]`).
                self.buf.emit(&[0x4D, 0x8B, 0x8B]);
                self.buf.emit(&G1B_ARENA_BASE.to_le_bytes());
                self.buf.emit(&[0x48, 0x89, 0xC1]); // MOV RCX, RAX
                self.buf.emit(&[0x4C, 0x29, 0xC9]); // SUB RCX, R9
                self.buf.emit(&[0x49, 0x89, 0xD2]); // MOV R10, RDX
                self.buf.emit(&[0x4D, 0x29, 0xCA]); // SUB R10, R9
                self.buf.emit(&[0x4C, 0x31, 0xD1]); // XOR RCX, R10
                self.buf.emit(&[0x49, 0x23, 0x8B]); // AND RCX, [R11 + disp32]
                self.buf.emit(&G1B_REGION_MASK.to_le_bytes());
            }
            let defer_g1 =
                self.mir_mode == MirMode::Off && !crate::x64::ir_ref_store_trace_enabled();
            let young = if gated && !defer_g1 {
                crate::x64::g1_young_holder_skip_bytes(RCX, Some(R10), R11)
            } else {
                None
            };
            let mut store_patches: Vec<usize> = Vec::new();
            let cross_region = if let Some((bytes, skips)) = young {
                store_patches.push(self.emit_jcc_rel32(0x84)); // JZ -> same region: store
                let at = self.buf.pos();
                self.buf.emit(&bytes);
                store_patches.extend(skips.into_iter().map(|s| at + s));
                self.emit_jmp_rel32() // not young -> cross-region
            } else {
                self.emit_jcc_rel32(0x85) // JNZ -> cross-region
            };
            if !gated {
                to_helper.push(cross_region);
            } else {
                cross_region_opt = Some(cross_region);
            }
            // The barrier-free store.
            self.patch_rel32_to_here(null_value);
            for p in store_patches {
                self.patch_rel32_to_here(p);
            }
            self.emit_ref_store_path_trace(&crate::metrics::IR_REF_STORE_INLINE_TAKEN);
            self.buf.emit(&[0x48, 0x89, 0x90]); // MOV [RAX + disp32], RDX
            self.buf.emit(&cell_off.to_le_bytes());
        }
        if self.mir_mode == MirMode::Off && !crate::x64::ir_ref_store_trace_enabled() {
            if cross_region_opt.is_some() || !to_helper.is_empty() {
                self.deferred_putfield_tails.push(DeferredPutfieldTail::G1 {
                    cross_region: cross_region_opt,
                    to_helper,
                    cell_off,
                    base_slot: self.slot_of(base),
                    field_index,
                    value_slot: self.slot_of(value),
                    done: self.buf.pos(),
                });
            }
        } else if cross_region_opt.is_some() || !to_helper.is_empty() {
            let stored = self.emit_jmp_rel32();
            let mut barriered: Option<usize> = None;
            let mut filtered: Option<usize> = None;
            if let Some(cross_region) = cross_region_opt {
                self.patch_rel32_to_here(cross_region);
                self.emit_ref_store_path_trace(&crate::metrics::IR_REF_STORE_INLINE_TAKEN);
                self.buf.emit(&[0x48, 0x89, 0x90]); // MOV [RAX + disp32], RDX
                self.buf.emit(&cell_off.to_le_bytes());
                if let Some(ref g) = g1_geom {
                    let (bytes, misses) = crate::ir_g1_edge_filter::g1_edge_filter_hit_bytes(g);
                    let at = self.buf.pos();
                    self.buf.emit(&bytes);
                    filtered = Some(self.emit_jmp_rel32());
                    for m in misses {
                        self.patch_rel32_to_here(at + m);
                    }
                }
                self.load_context(CALL_ARG_REGS[0]);
                self.load_reg_from_frame(CALL_ARG_REGS[1], self.slot_of(base));
                self.load_reg_from_frame(CALL_ARG_REGS[2], self.slot_of(value));
                cratonvm_jit_api::assert_helper_call_shape!(
                    "g1_post_write_barrier",
                    int_args = 3,
                    returns_value = false
                );
                self.emit_mov_reg_imm64(RAX, self.g1_post_write_barrier as u64);
                self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
                barriered = Some(self.emit_jmp_rel32());
            }
            for patch in to_helper {
                self.patch_rel32_to_here(patch);
            }
            self.emit_ref_store_path_trace(&crate::metrics::IR_REF_STORE_HELPER_TAKEN);
            self.load_context(CALL_ARG_REGS[0]);
            self.load_reg_from_frame(CALL_ARG_REGS[1], self.slot_of(base));
            self.emit_mov_reg_imm64(CALL_ARG_REGS[2], field_index as u64);
            self.load_reg_from_frame(CALL_ARG_REGS[3], self.slot_of(value));
            self.emit_mov_reg_imm64(RAX, self.putfield_object as u64);
            self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
            self.patch_rel32_to_here(stored);
            if let Some(p) = barriered {
                self.patch_rel32_to_here(p);
            }
            if let Some(p) = filtered {
                self.patch_rel32_to_here(p);
            }
        }
        crate::metrics::note_ir_ref_store_gated();
        true
    }

    /// JIT round 23 wave 9 (lane W9-L7, `CRATONVM_JIT_IR_GETFIELD_GUARD_NON_NULL`):
    /// does a `Guard(base != null)` (`Cmp(Ne)` against the `null` constant,
    /// either order) come before the node being lowered in its own block?
    /// A guard leaves the compiled code when its condition is false, and a
    /// block runs straight through in schedule order, so past the guard `base`
    /// is non-null. That is the null-base load predicate's shape
    /// (`ir_optimize::plant_null_base_load_predicates`): its guard is planted
    /// in the pre-header ahead of the copies of the reads it licenses, which
    /// therefore owe no null test. Asked of the SCHEDULE at lowering time, so
    /// no pass that moves a read away from its guard can make it wrong.
    pub(in crate::ir_lower) fn block_guard_proves_non_null(&self, base: NodeId) -> bool {
        if !ir_getfield_guard_non_null_enabled() {
            return false;
        }
        let Some(def) = self.cur_def else {
            return false;
        };
        let Some(b) = self.schedule.node_to_block.get(def as usize).copied() else {
            return false;
        };
        let Some(block) = self.schedule.blocks.get(b) else {
            return false;
        };
        let is_null = |x: NodeId| {
            self.graph
                .nodes
                .get(x as usize)
                .is_some_and(|k| k.op == Op::Const(0) && k.ty == IrType::Ref)
        };
        for &id in &block.nodes {
            if id == def {
                return false;
            }
            let Some(n) = self.graph.nodes.get(id as usize) else {
                continue;
            };
            if !matches!(n.op, Op::Guard { .. }) || n.inputs.len() != 2 {
                continue;
            }
            let Some(c) = self.graph.nodes.get(n.inputs[1] as usize) else {
                continue;
            };
            if c.op != Op::Cmp(CmpOp::Ne) || c.inputs.len() != 2 {
                continue;
            }
            if (c.inputs[0] == base && is_null(c.inputs[1]))
                || (c.inputs[1] == base && is_null(c.inputs[0]))
            {
                return true;
            }
        }
        false
    }
    /// Guarded inline prefix for `String.replace(CC)`, `StringLatin1.replace([BCC)`,
    /// 1-byte `String.replace(CharSequence, CharSequence)`, and `String.concat(String)`.
    pub(in crate::ir_lower) fn try_emit_ir_string_replace_concat_prefix(
        &mut self,
        id: NodeId,
        info: &JitInvokeInfo,
        kind: crate::IrStringSearchKind,
        layout: crate::StringFieldLayout,
    ) {
        use crate::x64::concat_emit as ce;
        use crate::IrStringSearchKind as K;
        if !kind.admits_invoke_kind(info.invoke_kind)
            || !layout.has_coder
            || layout.string_class_id == 0
        {
            self.note_string_prefix_declined(
                id,
                info,
                &format!(
                    "invoke_kind={} has_coder={} string_class_id={}",
                    info.invoke_kind, layout.has_coder, layout.string_class_id
                ),
            );
            return;
        }
        if crate::x64::narrow_oops_block_inline_fields() {
            self.note_string_prefix_declined(id, info, "narrow oops / armed ZGC barrier");
            return;
        }
        let graph = self.graph;
        let Some(node) = graph.nodes.get(id as usize) else {
            return;
        };
        if node.inputs.len() != 3 + kind.num_params() || node.ty != IrType::Ref {
            self.note_string_prefix_declined(
                id,
                info,
                &format!("node shape: {} inputs, ty {:?}", node.inputs.len(), node.ty),
            );
            return;
        }
        if node.inputs[2..].iter().any(|&i| self.carry_names(i)) {
            self.note_string_prefix_declined(id, info, "an operand is a live carry");
            return;
        }
        if self.carry_of.get(id as usize).copied().flatten().is_some()
            || self.home_dropped.get(id as usize).copied().unwrap_or(false)
        {
            self.note_string_prefix_declined(id, info, "a carried or dropped result home");
            return;
        }
        let Ok(slot) = self.planned_slot_off(id) else {
            self.note_string_prefix_declined(id, info, "no result home");
            return;
        };
        let site = match self.rx27_chars_site(layout) {
            Ok(site) => site,
            Err(why) => {
                self.note_string_prefix_declined(id, info, why);
                return;
            }
        };
        self.note_rcx_written();
        let mut decline: Vec<usize> = Vec::new();
        if let Some(p) = self.emit_layout_epoch_guard() {
            decline.push(p);
        }

        match kind {
            K::ReplaceChar => {
                let (recv, old_c, new_c) = (node.inputs[2], node.inputs[3], node.inputs[4]);
                self.gp_load_value(RAX, recv);
                self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
                decline.push(self.emit_jcc_rel32(0x84));
                self.emit_str_cmp_class_id(RAX, layout.string_class_id);
                decline.push(self.emit_jcc_rel32(0x85));
                self.gp_load_value(R10, old_c);
                self.gp_load_value(R9, new_c);
                self.emit_str_value_and_coder(&layout, RAX, RCX, Some(RDX));
                self.buf.emit(&[0x85, 0xD2]); // TEST EDX, EDX
                decline.push(self.emit_jcc_rel32(0x85));
                ce::emit_ir_latin1_replace_char(&mut self.buf, &site, &mut decline, false);
            }
            K::Latin1ReplaceChar => {
                let (val, old_c, new_c) = (node.inputs[2], node.inputs[3], node.inputs[4]);
                self.gp_load_value(RCX, val);
                self.gp_load_value(R10, old_c);
                self.gp_load_value(R9, new_c);
                ce::emit_ir_latin1_replace_char(&mut self.buf, &site, &mut decline, true);
            }
            K::ReplaceCharSeq => {
                let (recv, target, repl) = (node.inputs[2], node.inputs[3], node.inputs[4]);
                self.gp_load_value(RAX, recv);
                self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
                decline.push(self.emit_jcc_rel32(0x84));
                self.emit_str_cmp_class_id(RAX, layout.string_class_id);
                decline.push(self.emit_jcc_rel32(0x85));

                // Screen target as non-null plain String, LATIN1, length == 1 -> R10D.
                self.gp_load_value(R11, target);
                self.buf.emit(&[0x4D, 0x85, 0xDB]); // TEST R11, R11
                decline.push(self.emit_jcc_rel32(0x84));
                self.emit_str_test_mem8(
                    R11,
                    cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8,
                    cratonvm_types::KIND_TAG_BYTE_MASK,
                );
                decline.push(self.emit_jcc_rel32(0x85));
                self.emit_str_cmp_class_id(R11, layout.string_class_id);
                decline.push(self.emit_jcc_rel32(0x85));
                self.emit_str_value_and_coder(&layout, R11, R10, Some(RDX));
                self.buf.emit(&[0x85, 0xD2]); // TEST EDX, EDX
                decline.push(self.emit_jcc_rel32(0x85));
                ce::emit_ir_latin1_len1_char(&mut self.buf, &site, R10, &mut decline);

                // Screen replacement as non-null plain String, LATIN1, length == 1 -> R9D.
                self.gp_load_value(R11, repl);
                self.buf.emit(&[0x4D, 0x85, 0xDB]); // TEST R11, R11
                decline.push(self.emit_jcc_rel32(0x84));
                self.emit_str_test_mem8(
                    R11,
                    cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8,
                    cratonvm_types::KIND_TAG_BYTE_MASK,
                );
                decline.push(self.emit_jcc_rel32(0x85));
                self.emit_str_cmp_class_id(R11, layout.string_class_id);
                decline.push(self.emit_jcc_rel32(0x85));
                self.emit_str_value_and_coder(&layout, R11, R9, Some(RDX));
                self.buf.emit(&[0x85, 0xD2]); // TEST EDX, EDX
                decline.push(self.emit_jcc_rel32(0x85));
                ce::emit_ir_latin1_len1_char(&mut self.buf, &site, R9, &mut decline);

                self.emit_str_value_and_coder(&layout, RAX, RCX, Some(RDX));
                self.buf.emit(&[0x85, 0xD2]); // TEST EDX, EDX
                decline.push(self.emit_jcc_rel32(0x85));
                ce::emit_ir_latin1_replace_char(&mut self.buf, &site, &mut decline, false);
            }
            K::StringConcat => {
                let (recv, arg) = (node.inputs[2], node.inputs[3]);
                self.gp_load_value(RAX, recv);
                self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
                decline.push(self.emit_jcc_rel32(0x84));
                self.emit_str_cmp_class_id(RAX, layout.string_class_id);
                decline.push(self.emit_jcc_rel32(0x85));

                self.gp_load_value(R11, arg);
                self.buf.emit(&[0x4D, 0x85, 0xDB]); // TEST R11, R11
                decline.push(self.emit_jcc_rel32(0x84));
                self.emit_str_cmp_class_id(R11, layout.string_class_id);
                decline.push(self.emit_jcc_rel32(0x85));
                self.emit_str_value_and_coder(&layout, R11, R10, Some(RDX));
                self.buf.emit(&[0x85, 0xD2]); // TEST EDX, EDX
                decline.push(self.emit_jcc_rel32(0x85));

                self.emit_str_value_and_coder(&layout, RAX, RCX, Some(RDX));
                self.buf.emit(&[0x85, 0xD2]); // TEST EDX, EDX
                decline.push(self.emit_jcc_rel32(0x85));
                ce::emit_ir_latin1_concat(&mut self.buf, &site, &mut decline);
            }
            _ => return,
        }

        self.emit_integer_value_of_result_store(id, Some(slot));
        crate::ir_evidence::note(crate::ir_evidence::Transform::ScalarIntrinsic);
        let hit = self.emit_jmp_rel32();
        for p in decline {
            self.patch_rel32_to_here(p);
        }
        self.call_prefix_join = Some(hit);
    }
}
