#[allow(unused_imports)]
use super::*;

#[path = "hashmap_inline.rs"]
mod hashmap_inline;

impl<'a> Lowerer<'a> {
    /// nio20 wave 3, lane L2 (the residual of NIO19-8, page
    /// `r20-string-pin-and-planning-gate-residual-20261007`): the five
    /// `java/lang/String` members the single-pass backend intrinsifies --
    /// `hashCode()`, `equals(Object)`, `compareTo(String)`, `indexOf(int)`,
    /// `indexOf(String)` -- answered inline as a guarded PREFIX of the
    /// `Op::Call`'s unchanged lowering, so a method containing one no longer
    /// has to stay on the single-pass backend to keep an inline body
    /// (`lib.rs::ir_string_search_prefix_handles` lifts the planning gate and
    /// the pin for exactly the sites this emits).
    ///
    /// Same contract with the `Op::Call` arm as
    /// [`Self::try_emit_ir_arraylist_access_prefix`]: scratch RAX, RCX, RDX,
    /// R10 and R11 only (R11 and ECX through the epoch guard's fallback); the
    /// hit edge writes the result home (and register) and jumps to
    /// [`Self::call_prefix_join`]; no call, no GC point and no deopt on the hit
    /// path. Every uncertain case DECLINES to the call, which runs the real
    /// `String` method and answers -- or throws -- exactly as before.
    ///
    /// # What it answers
    ///
    /// * `hashCode()` -- a NON-ZERO cached `hash` (what the JDK body returns
    ///   without reading `value`). nio20 wave 4 (L5, W3-L2-2): a zero `hash`
    ///   on a COMPACT receiver is computed inline and a non-zero result
    ///   cached ([`Self::emit_str_hash_compute`]); a zero result or a legacy
    ///   receiver declines and the call computes and caches it.
    ///
    /// nio20 wave 4 (L5, W3-L2-1): `compareTo` and both `indexOf` forms
    /// answer UTF16 operands too (see each bullet below for the coder pairs
    /// still declined); W3-L2-3: `equals` / `compareTo` compare 16 bytes and
    /// `indexOf(int)` scans 16 bytes per SSE2 step
    /// (`CRATONVM_JIT_IR_STRING_SSE2`). The `StringBuilder` members are
    /// [`Self::try_emit_ir_string_builder_prefix`].
    /// * `equals(Object)` -- `this == arg` is `true`; a null argument is
    ///   `false`; an argument that is not a plain `java/lang/String` declines
    ///   (`false` would be right -- String is final -- but this tier stays as
    ///   conservative as the single-pass body, which deopts there); otherwise
    ///   equal coders, equal `value.length` and equal bytes (8 at a time, then
    ///   a byte tail). Correct for LATIN1 and UTF16 alike: with equal coders the
    ///   strings are equal exactly when the byte arrays are. Unequal coders
    ///   answer `false`, matching HotSpot even for noncanonical Strings.
    /// * `compareTo(String)` -- two LATIN1 strings: the unsigned difference of
    ///   the first differing bytes, else the length difference
    ///   (`StringLatin1.compareTo`); two UTF16 strings: the same over code
    ///   units (`StringUTF16.compareTo`). Mixed coders, or a null argument,
    ///   decline -- JIT round 21 (lane strintr,
    ///   `CRATONVM_JIT_IR_STRING_COMPARE_TO_MIXED`): mixed coders answer
    ///   inline over decoded chars ([`Self::emit_str_compare_to_mixed_arm`]).
    /// * `indexOf(int)` -- a LATIN1 receiver: `-1` for any needle outside
    ///   `0..=0xFF` (`StringLatin1.canEncode`; this covers negative, invalid
    ///   and supplementary code points, which can never occur in a LATIN1
    ///   string), else the first matching byte. A UTF16 receiver: a needle
    ///   in `0..=0xFFFF` is one code unit (`StringUTF16.indexOfChar`);
    ///   a negative or supplementary needle declines (the JDK's validity gate
    ///   and surrogate-pair search stay the call's) -- JIT round 21 (lane
    ///   strintr, `CRATONVM_JIT_IR_STRING_INDEXOF_SUPPLEMENTARY`): answered
    ///   inline, `-1` for a negative / invalid code point, the pair scan for a
    ///   supplementary one.
    /// * `indexOf(String)` -- all four coder pairs: same-coder pairs and
    ///   UTF16-haystack / LATIN1-needle use the first-occurrence search; the
    ///   reverse mixed pair returns `-1`, matching `String.indexOf`'s coder
    ///   dispatch. A malformed odd-length UTF16 backing array or null argument
    ///   declines.
    /// * `CharSequence.length()` / `isEmpty()` / `charAt(int)` at an
    ///   `invokeinterface` (proposal NIO20-L3-4): behind a plain-object test
    ///   and the exact String class-id compare the IR expander has no node
    ///   for; any other receiver (`StringBuilder`, `CharBuffer`, `AsciiString`,
    ///   ...) DECLINES to the interface call -- never a deopt, so unlike the
    ///   single-pass guarded body it needs no receiver profile. `charAt`
    ///   declines an index outside `[0, length)` (the call throws).
    ///
    /// The receiver is `[recv+0] == string_class_id`-checked although a
    /// `java/lang/String`-declared receiver can be nothing else: four bytes
    /// against reading a non-String's fields as `value`/`coder`. Field reads
    /// dispatch per object on `GC_FLAG_COMPACT` exactly as the single-pass
    /// `emit_load_string_value_ptr` / `emit_load_string_i32_field` do.
    ///
    /// # Declines at compile time
    ///
    /// No String layout on this compile (every door but the IR door, see
    /// [`CompileStringLayoutScope`]), no `coder`, no class id, a site
    /// `crate::ir_string_search_prefix_kind` does not name (that includes
    /// `CRATONVM_JIT_IR_STRING_SEARCH_PREFIX=0`), an invoke kind other than
    /// virtual or devirtualised special, narrow oops or an armed ZGC read
    /// barrier (`value` is a reference field; the planning gate asks the same
    /// screen), and the carry / dropped-home shapes the other prefixes refuse.
    ///
    /// # No safepoint in the loops
    ///
    /// The byte loops poll nothing, like the single-pass bodies they replace;
    /// a multi-megabyte `equals` delays a safepoint by its own length.
    pub(in crate::ir_lower) fn try_emit_ir_string_search_prefix(
        &mut self,
        id: NodeId,
        info_ptr: usize,
    ) {
        use crate::IrStringSearchKind as K;
        if info_ptr == 0 {
            return;
        }
        // SAFETY: as every other read of an `Op::Call`'s `info_ptr` in this
        // file -- a live `JitInvokeInfo` owned by `ir_call_infos`.
        let info: &JitInvokeInfo = unsafe { &*(info_ptr as *const JitInvokeInfo) };
        let Some(kind) =
            crate::ir_string_search_prefix_kind(info.class_name, info.method_name, info.descriptor)
        else {
            self.try_emit_ir_hashmap_get_prefix(id, info);
            return;
        };
        // nio21 wave 5 (lane L5): a `CharSequence.charAt` inside a
        // class-predicated loop DEOPTS on a miss instead of calling
        // (`ir_optimize::seq_charat_traps_on_miss`, the predicate the field
        // forward also read). Every compile-time decline below then refuses
        // the compile instead of falling back to the call, which the forward
        // has assumed away.
        let trap =
            kind == K::SeqCharAt && crate::ir_optimize::seq_charat_traps_on_miss(self.graph, id);
        // JIT r23 wave 2b, lane L5 (INTRINSICS22-5): `Arrays.equals` reads
        // no String layout; its own emitter.
        if kind == K::ArraysEquals {
            self.try_emit_ir_arrays_equals_prefix(id, info);
            return;
        }
        let Some(layout) = self.string_layout else {
            self.note_string_prefix_declined(id, info, "no String layout in this compile");
            self.refuse_seq_charat_trap(trap, "no String layout in this compile");
            return;
        };
        // JIT r20 wave 18, lane xSb (STR20-2): `StringBuilder.toString()`
        // allocates, with its own emitter (the builder half AND the String
        // allocation plan).
        if kind == K::SbToString {
            self.try_emit_ir_sb_to_string_prefix(id, info, layout);
            return;
        }
        // Round rx27, lane splitfast: `substring` / `subSequence` allocate,
        // with their own emitter (the String allocation plan).
        if matches!(kind, K::Substring | K::SeqSubSequence) {
            self.try_emit_ir_string_substring_prefix(id, info, kind, layout);
            return;
        }
        if matches!(
            kind,
            K::ReplaceChar | K::Latin1ReplaceChar | K::ReplaceCharSeq | K::StringConcat
        ) {
            self.try_emit_ir_string_replace_concat_prefix(id, info, kind, layout);
            return;
        }
        // Round rx27 wave 3 (lane splitfast): the static
        // `Arrays.copyOfRange(byte[], int, int)` allocates through the same plan.
        if kind == K::ArraysCopyOfRangeBytes {
            self.try_emit_ir_arrays_copy_of_range_prefix(id, info, layout);
            return;
        }
        // nio20 wave 4, lane L5 (W3-L2-4): the `StringBuilder` members read
        // the layout's builder half, not the String one.
        if kind.is_builder() {
            self.try_emit_ir_string_builder_prefix(id, info, kind, layout);
            return;
        }
        // nio20 wave 6, lane L2: `getChars` is a void four-argument store
        // loop with its own emitter.
        if kind == K::GetChars {
            self.try_emit_ir_string_getchars_prefix(id, info, layout);
            return;
        }
        // ... and the static `StringLatin1.inflate` it ends in (no layout
        // read; asked behind the layout test above anyway, so the
        // `call_site_may_take_inline_prefix` screen, which requires a
        // layout, stays a superset).
        if kind == K::Inflate {
            self.try_emit_ir_string_inflate_prefix(id, info);
            return;
        }
        // JIT round 21, lane strings (ZCP-4): the static
        // `StringLatin1.compareToCI`, likewise with no layout read.
        if kind == K::Latin1CompareToCI {
            self.try_emit_ir_latin1_compare_to_ci_prefix(id, info);
            return;
        }
        // JIT round 21, lane str21b (STRINGS2-21-4): the static
        // `StringLatin1.regionMatchesCI`, likewise with no layout read.
        if kind == K::Latin1RegionMatchesCI {
            self.try_emit_ir_latin1_region_matches_ci_prefix(id, info);
            return;
        }
        // JIT round 21, lane strings2 (STRINGS21-1): the static
        // `StringLatin1.toLowerCase` / `toUpperCase`, which allocate.
        // STRINGS21 (str21): `Latin1ToLowerCase` is also named by the
        // `CRATONVM_JIT_IR_STRING_INTERNAL_ADMIT` worker block, which is ON by
        // default, so `CRATONVM_JIT_IR_LATIN1_CASE_MAP_PREFIX=0` could not
        // withdraw this emitter: with that switch off the lower-case worker
        // takes the wave-3 `try_emit_ir_string_latin1_case_prefix` below, as
        // the admit's own contract says. `Latin1ToUpperCase` is named by the
        // case-map switch alone, so it needs no screen.
        if kind == K::Latin1ToUpperCase
            || (kind == K::Latin1ToLowerCase && crate::ir_latin1_case_map_prefix_enabled())
        {
            self.try_emit_ir_latin1_case_map_prefix(id, info, kind, layout);
            return;
        }
        // JIT r20 wave 18, lane xUtf8 (STR20-6): the two static
        // `StringCoding` leaves, likewise with no layout read.
        if kind.is_string_coding() {
            self.try_emit_ir_string_coding_prefix(id, info, kind);
            return;
        }
        // JIT r20 wave 12, lane qStr (W6-L2-2): the three `char[]` copies
        // allocate, with their own emitter.
        if kind.is_chars_copy() {
            self.try_emit_ir_string_chars_prefix(id, info, kind, layout);
            return;
        }
        // JIT r20 wave 19, lane zCp: the static `Character.toChars(int)`
        // allocates a `char[]` through the same plan, with its own emitter.
        if kind == K::CharacterToChars {
            self.try_emit_ir_character_to_chars_prefix(id, info, layout);
            return;
        }
        // JIT r21 wave 1/3, lane strings: the Latin-1 case kinds (the two
        // `toLowerCase` user sites, which may allocate, and the lower-case
        // `StringLatin1` worker, a static site with no String receiver).
        if kind.is_latin1_case() {
            self.try_emit_ir_string_latin1_case_prefix(id, info, kind, layout);
            return;
        }
        if !kind.admits_invoke_kind(info.invoke_kind)
            || !layout.has_coder
            || layout.string_class_id == 0
        {
            self.refuse_seq_charat_trap(trap, "the String layout has no coder or class id");
            return;
        }
        if crate::x64::narrow_oops_block_inline_fields() {
            self.refuse_seq_charat_trap(trap, "narrow oops");
            return;
        }
        let graph = self.graph;
        let Some(node) = graph.nodes.get(id as usize) else {
            self.refuse_seq_charat_trap(trap, "no node");
            return;
        };
        // [ctrl, mem, receiver] or [ctrl, mem, receiver, argument]; `int`,
        // `byte` and `boolean` results are all `IrType::Int` here. JIT r21
        // (lane strings): the two `toLowerCase` kinds answer the receiver.
        let want_ty = if matches!(kind, K::LowerLocaleUnchanged | K::LowerUnchanged) {
            IrType::Ref
        } else {
            IrType::Int
        };
        if node.inputs.len() != 3 + kind.num_params() || node.ty != want_ty {
            self.refuse_seq_charat_trap(trap, "call shape");
            return;
        }
        // JIT r21 wave 1, lane strings (STRINGS21-1): `coder()` /
        // `isLatin1()` read this VM's `COMPACT_STRINGS` flag byte; without its
        // address the site stays the plain call (decided before any byte).
        let compact_flag_addr = if matches!(kind, K::StrCoder | K::StrIsLatin1) {
            match self.direct_helpers.compact_strings_flag_addr() {
                Some(a) => a,
                None => {
                    self.note_string_prefix_declined(id, info, "no compact-strings flag address");
                    return;
                }
            }
        } else {
            0
        };
        let recv = node.inputs[2];
        // JIT r23 wave 1, lane L5: `>= 1`, not `== 1` -- `codePointCount`
        // takes two (every other kind that reaches here takes 0 or 1; the
        // two-parameter `CountPositives` / `Latin1ToLowerCase` return to their
        // own emitters above).
        let arg = if kind.num_params() >= 1 {
            Some(node.inputs[3])
        } else {
            None
        };
        let arg2 = if kind.num_params() == 2 {
            Some(node.inputs[4])
        } else {
            None
        };
        if self.carry_names(recv)
            || arg.is_some_and(|a| self.carry_names(a))
            || arg2.is_some_and(|a| self.carry_names(a))
        {
            self.refuse_seq_charat_trap(trap, "a carried operand");
            return;
        }
        // The argument of the one-parameter kinds (the input-count screen
        // above guarantees it exists for them); never read for `hashCode`.
        let arg_id: NodeId = arg.unwrap_or(recv);
        // The second argument (`codePointCount`'s `endIndex`); read by no
        // other kind.
        let arg2_id: NodeId = arg2.unwrap_or(recv);
        let Some(slot) = self.prefix_result_slot(id, node.ty) else {
            self.refuse_seq_charat_trap(trap, "a carried, routed, or dropped result home");
            return;
        };
        // nio21 wave 5 (lane L5): the trap resumes AT the invoke, so it needs
        // the frame recorded at the top of the call's pc -- the callee chain
        // inside a splice (the flat frame of the outermost invoke would re-run
        // the spliced body's committed stores), the method's own snapshot
        // otherwise. Checked before a byte is emitted.
        let trap_pc = if trap {
            match node.bytecode_pc {
                Some(pc) if self.seq_charat_trap_frame_ok(pc) => Some(pc),
                _ => {
                    self.refuse_seq_charat_trap(trap, "no frame at the call's pc");
                    return;
                }
            }
        } else {
            None
        };
        if crate::ir_stage_reporting() {
            eprintln!(
                "[ir] string-search-prefix n{id} {}.{}{} kind={kind:?} string_class_id={}{}",
                info.class_name,
                info.method_name,
                info.descriptor,
                layout.string_class_id,
                if trap_pc.is_some() { " miss=deopt" } else { "" },
            );
        }
        // `[value + 8]` is the array length and `[value + 16 + i]` byte `i`,
        // the two displacements the encodings below bake as disp8.
        const _: () = assert!(
            cratonvm_types::ARRAY_LENGTH_OFFSET == 8 && cratonvm_types::ARRAY_DATA_OFFSET == 16,
            "the String search prefix bakes ARRAY_LENGTH_OFFSET 8 / ARRAY_DATA_OFFSET 16"
        );
        let sid = layout.string_class_id;
        let mut decline: Vec<usize> = Vec::new();
        let mut to_store: Vec<usize> = Vec::new();
        // RCX: the epoch guard's fallback form and the field loads write it.
        self.note_rcx_written();
        let cur_block = self.schedule.node_to_block.get(id as usize).copied();
        let layout_proven =
            cur_block.is_some_and(|b| self.field_proof_holds(&self.layout_proven, recv, b, false));
        let compact_proven = layout_proven
            && cur_block
                .is_some_and(|b| self.field_proof_holds(&self.compact_proven, recv, b, true));
        // A layout REPLACEMENT invalidates the baked compact offsets.
        if !layout_proven && !self.block_cse_layout_epoch_checked() {
            if let Some(p) = self.emit_layout_epoch_guard() {
                decline.push(p);
            }
        }
        // JIT rx27 wave 1 (lane scanloop,
        // `CRATONVM_JIT_IR_SEQ_CHARAT_PROVEN_STRING`, opt-in): a trapping
        // `charAt` whose receiver a dominating loop-entry
        // `ExactClassIs(recv, String)` guard proves skips the three receiver
        // tests below (`ir_optimize::charat_trap`, the rx27 section).
        let proven = if trap_pc.is_some() {
            crate::ir_optimize::seq_charat_receiver_proven_string(self.graph, id)
        } else {
            None
        };
        if crate::x64::dbg_jitc_enabled()
            && trap_pc.is_some()
            && crate::ir_optimize::seq_charat_proven_string_enabled()
        {
            match proven {
                Some((guard, header_bci)) => eprintln!(
                    "[cratonvm-jitc] ir-seq-charat-proven-string {} n{id} pc {:?}: receiver \
                     null/kind/class tests elided (String guard n{guard} at header bci \
                     {header_bci})",
                    self.graph.scope_method_key, node.bytecode_pc
                ),
                None => eprintln!(
                    "[cratonvm-jitc] ir-seq-charat-proven-string {} n{id} pc {:?}: kept: no \
                     licensed loop-entry String guard on this receiver",
                    self.graph.scope_method_key, node.bytecode_pc
                ),
            }
        }
        // RAX = receiver; null declines (the call raises the NPE at this bci).
        self.gp_load_value(RAX, recv);
        let exact_class_proven = proven.is_some()
            || cur_block.is_some_and(|b| self.dominating_exact_class(recv, b) == Some(sid))
            || self.checkcast_exact_proven(recv, sid);
        if !exact_class_proven {
            self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
            decline.push(self.emit_jcc_rel32(0x84)); // JZ

            // A `CharSequence` receiver may be anything: a plain object only (an
            // array header carries its COMPONENT's class id), then the exact
            // String class id -- the guard the IR expander has no node for.
            if matches!(kind, K::SeqLength | K::SeqIsEmpty | K::SeqCharAt) {
                self.emit_str_test_mem8(
                    RAX,
                    cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8, // Cast: x86-64 disp8
                    cratonvm_types::KIND_TAG_BYTE_MASK,
                );
                decline.push(self.emit_jcc_rel32(0x85)); // JNE
            }
            self.emit_str_cmp_class_id(RAX, sid);
            decline.push(self.emit_jcc_rel32(0x85)); // JNE
        }
        match kind {
            // JIT round 21, lane strintr2 (STRINTR21-1): the String-declared
            // twins (`StrLength` / `StrIsEmpty`) share the arm; the kind-bit
            // test above is the CharSequence kinds' only.
            K::SeqLength | K::SeqIsEmpty | K::StrLength | K::StrIsEmpty => {
                // RCX = value, EDX = coder.
                self.emit_str_value_and_coder(&layout, RAX, RCX, Some(RDX));
                self.emit_str_test_r64(RCX);
                decline.push(self.emit_jcc_rel32(0x84)); // JZ
                if matches!(kind, K::SeqLength | K::StrLength) {
                    // length() == value.length >> coder.
                    self.emit_load_base_disp32(RAX, RCX, 8, false);
                    self.buf.emit(&[0x89, 0xD1]); // MOV ECX, EDX
                    self.buf.emit(&[0xD3, 0xE8]); // SHR EAX, CL
                } else {
                    // isEmpty() == (value.length == 0).
                    self.buf.emit(&[0x31, 0xC0]); // XOR EAX, EAX
                    self.buf.emit(&[0x83, 0x79, 0x08, 0x00]); // CMP DWORD [RCX+8], 0
                    self.buf.emit(&[0x0F, 0x94, 0xC0]); // SETE AL
                }
            }
            // JIT round 21, lane strintr2 (STRINTR21-1): `StrCharAt`, a
            // `String.charAt` the accessor expander left a call, never traps
            // (`trap` is `SeqCharAt`'s alone): an index outside
            // `[0, length)` declines and the call raises the SIOOBE.
            K::SeqCharAt | K::StrCharAt => {
                let index = arg_id;
                self.emit_str_value_and_coder(&layout, RAX, RCX, Some(RDX));
                self.emit_str_test_r64(RCX);
                decline.push(self.emit_jcc_rel32(0x84)); // JZ
                                                         // R10 = index, zero-extended: the element load scales the
                                                         // FULL register, and the unsigned compares below decline a
                                                         // negative index (the call raises the exception).
                self.gp_load_value(R10, index);
                self.buf.emit(&[0x45, 0x89, 0xD2]); // MOV R10D, R10D
                self.emit_load_base_disp32(RAX, RCX, 8, false); // EAX = value.length
                self.buf.emit(&[0x85, 0xD2]); // TEST EDX, EDX
                let utf16 = self.emit_jcc_rel32(0x85); // JNZ
                                                       // LATIN1: one byte per char.
                self.buf.emit(&[0x41, 0x39, 0xC2]); // CMP R10D, EAX
                decline.push(self.emit_jcc_rel32(0x83)); // JAE
                self.buf.emit(&[0x42, 0x0F, 0xB6, 0x44, 0x11, 0x10]); // MOVZX EAX, BYTE [RCX+R10+16]
                let done = self.emit_jmp_rel32();
                // UTF16: two little-endian bytes per char (the single-pass
                // decode's order).
                self.patch_rel32_to_here(utf16);
                self.buf.emit(&[0xD1, 0xE8]); // SHR EAX, 1
                self.buf.emit(&[0x41, 0x39, 0xC2]); // CMP R10D, EAX
                decline.push(self.emit_jcc_rel32(0x83)); // JAE
                self.buf.emit(&[0x42, 0x0F, 0xB7, 0x44, 0x51, 0x10]); // MOVZX EAX, WORD [RCX+R10*2+16]
                self.patch_rel32_to_here(done);
            }
            K::HashCode => {
                // ECX = hash (4-byte payload in both layouts).
                self.emit_str_test_mem8(
                    RAX,
                    cratonvm_types::GC_FLAGS_BYTE_OFFSET as u8, // Cast: x86-64 disp8
                    cratonvm_types::GC_FLAG_COMPACT,
                );
                let legacy = self.emit_jcc_rel32(0x84); // JZ legacy
                self.emit_load_base_disp32(RCX, RAX, layout.hash_compact_offset, false);
                let have = self.emit_jmp_rel32();
                self.patch_rel32_to_here(legacy);
                self.emit_load_base_disp32(RCX, RAX, layout.hash_legacy_offset, false);
                self.patch_rel32_to_here(have);
                self.buf.emit(&[0x85, 0xC9]); // TEST ECX, ECX
                if crate::ir_string_hash_compute_enabled() {
                    // nio20 wave 4, lane L5 (W3-L2-2): not cached yet -- compute
                    // it here and cache a non-zero result, as `String.hashCode`
                    // does, for a COMPACT receiver.
                    let cached = self.emit_jcc_rel32(0x85); // JNZ: cached
                                                            // JIT round 19 proposal wave 6, lane str6 (STRGAP19-6,
                                                            // `CRATONVM_JIT_STRING_HASH_IS_ZERO`, opt-in): the JDK
                                                            // body's `!hashIsZero` test. A COMPACT receiver (RAX)
                                                            // whose `hashIsZero` is set answers 0 -- ECX already holds
                                                            // the zero `hash` the `cached` arm sign-extends.
                    let hash_is_zero = layout
                        .hash_is_zero_compact_offset
                        .filter(|_| crate::string_hash_is_zero_enabled());
                    let mut hash_is_zero_hit: Option<usize> = None;
                    if let Some(off) = hash_is_zero {
                        self.emit_str_test_mem8(
                            RAX,
                            cratonvm_types::GC_FLAGS_BYTE_OFFSET as u8, // Cast: x86-64 disp8
                            cratonvm_types::GC_FLAG_COMPACT,
                        );
                        let legacy = self.emit_jcc_rel32(0x84); // JZ: the compute declines it
                                                                // CMP BYTE [RAX + disp32], 0 (80 /7 ib, ModRM 0xB8)
                        self.buf.emit(&[0x80, 0xB8]);
                        self.buf.emit(&off.to_le_bytes());
                        self.buf.emit(&[0x00]);
                        hash_is_zero_hit = Some(self.emit_jcc_rel32(0x85)); // JNE
                        self.patch_rel32_to_here(legacy);
                    }
                    self.emit_str_hash_compute(&layout, recv, &mut decline, hash_is_zero);
                    to_store.push(self.emit_jmp_rel32());
                    self.patch_rel32_to_here(cached);
                    if let Some(p) = hash_is_zero_hit {
                        self.patch_rel32_to_here(p);
                    }
                } else {
                    decline.push(self.emit_jcc_rel32(0x84)); // JZ: not cached yet
                }
                self.buf.emit(&[0x48, 0x63, 0xC1]); // MOVSXD RAX, ECX
            }
            K::Equals => {
                let mut trues: Vec<usize> = Vec::new();
                let mut falses: Vec<usize> = Vec::new();
                let arg = arg_id;
                // R10 = argument.
                self.gp_load_value(R10, arg);
                self.buf.emit(&[0x4C, 0x39, 0xD0]); // CMP RAX, R10
                trues.push(self.emit_jcc_rel32(0x84)); // JE: this == anObject
                self.buf.emit(&[0x4D, 0x85, 0xD2]); // TEST R10, R10
                falses.push(self.emit_jcc_rel32(0x84)); // JZ: equals(null)
                                                        // A plain object of exactly `java/lang/String`, else decline.
                self.emit_str_test_mem8(
                    R10,
                    cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8, // Cast: x86-64 disp8
                    cratonvm_types::KIND_TAG_BYTE_MASK,
                );
                decline.push(self.emit_jcc_rel32(0x85)); // JNE: an array
                self.emit_str_cmp_class_id(R10, sid);
                decline.push(self.emit_jcc_rel32(0x85)); // JNE
                                                         // RCX = this.value, EDX = this.coder; R11 = arg.value, EAX =
                                                         // arg.coder (the receiver is not needed past its fields).
                self.emit_str_value_and_coder(&layout, RAX, RCX, Some(RDX));
                self.emit_str_test_r64(RCX);
                decline.push(self.emit_jcc_rel32(0x84)); // JZ
                self.emit_str_value_and_coder(&layout, R10, R11, Some(RAX));
                self.emit_str_test_r64(R11);
                decline.push(self.emit_jcc_rel32(0x84)); // JZ
                                                         // HotSpot String.equals returns false for unequal coders,
                                                         // including malformed or noncanonical Strings. Answer it here
                                                         // directly so both VM modes keep HotSpot's behavior.
                self.buf.emit(&[0x39, 0xD0]); // CMP EAX, EDX (coders)
                falses.push(self.emit_jcc_rel32(0x85)); // JNE
                                                         // EAX = value.length; CMP EAX, [R11+8].
                self.emit_load_base_disp32(RAX, RCX, 8, false);
                self.buf.emit(&[0x41, 0x3B, 0x43, 0x08]);
                falses.push(self.emit_jcc_rel32(0x85)); // JNE
                let (blk_mismatch, tail_done, byte_mismatch) =
                    self.emit_str_byte_compare_loop(false);
                falses.extend(blk_mismatch);
                falses.push(byte_mismatch);
                trues.push(tail_done);
                for p in trues {
                    self.patch_rel32_to_here(p);
                }
                self.buf.emit(&[0xB8, 0x01, 0x00, 0x00, 0x00]); // MOV EAX, 1
                to_store.push(self.emit_jmp_rel32());
                for p in falses {
                    self.patch_rel32_to_here(p);
                }
                self.buf.emit(&[0x31, 0xC0]); // XOR EAX, EAX
            }
            K::CompareTo => {
                let arg = arg_id;
                // R10 = argument; null declines (the call's NPE).
                self.gp_load_value(R10, arg);
                self.buf.emit(&[0x4D, 0x85, 0xD2]); // TEST R10, R10
                decline.push(self.emit_jcc_rel32(0x84)); // JZ
                self.emit_str_cmp_class_id(R10, sid);
                decline.push(self.emit_jcc_rel32(0x85)); // JNE
                                                         // Equal coders, else decline (nio20 wave 4, L5: W3-L2-1 added
                                                         // the UTF16 pair; a mixed pair still calls).
                self.emit_str_value_and_coder(&layout, RAX, RCX, Some(RDX));
                self.emit_str_test_r64(RCX);
                decline.push(self.emit_jcc_rel32(0x84)); // JZ
                self.emit_str_value_and_coder(&layout, R10, R11, Some(RAX));
                self.emit_str_test_r64(R11);
                decline.push(self.emit_jcc_rel32(0x84)); // JZ
                self.buf.emit(&[0x39, 0xD0]); // CMP EAX, EDX (coders)
                                              // JIT round 21, lane strintr (STR16-P3,
                                              // `CRATONVM_JIT_IR_STRING_COMPARE_TO_MIXED`): a mixed pair is
                                              // answered by the arm after the UTF16 one.
                let mixed_on = crate::ir_string_compare_to_mixed_enabled();
                let mixed = self.emit_jcc_rel32(0x85); // JNE: mixed coders
                if !mixed_on {
                    decline.push(mixed);
                }
                self.buf.emit(&[0x85, 0xD2]); // TEST EDX, EDX
                let utf16 = self.emit_jcc_rel32(0x85); // JNZ: both UTF16
                                                       // EAX = min(len1, len2) (lengths are non-negative: unsigned).
                self.emit_load_base_disp32(RAX, RCX, 8, false);
                self.emit_load_base_disp32(RDX, R11, 8, false);
                self.buf.emit(&[0x39, 0xD0]); // CMP EAX, EDX
                self.buf.emit(&[0x0F, 0x47, 0xC2]); // CMOVA EAX, EDX
                                                    // An 8-byte block mismatch falls into the byte loop, which
                                                    // finds the first differing byte inside that block.
                let (_, tail_done, byte_mismatch) = self.emit_str_byte_compare_loop(true);
                // Equal prefix: len1 - len2.
                self.patch_rel32_to_here(tail_done);
                self.emit_load_base_disp32(RAX, RCX, 8, false);
                self.buf.emit(&[0x41, 0x2B, 0x43, 0x08]); // SUB EAX, [R11+8]
                self.buf.emit(&[0x48, 0x63, 0xC0]); // MOVSXD RAX, EAX
                to_store.push(self.emit_jmp_rel32());
                // First mismatch at EDX: R10D = this[k]; EAX = that[k].
                self.patch_rel32_to_here(byte_mismatch);
                self.buf.emit(&[0x41, 0x0F, 0xB6, 0x44, 0x13, 0x10]); // MOVZX EAX, BYTE [R11+RDX+16]
                self.buf.emit(&[0x41, 0x29, 0xC2]); // SUB R10D, EAX
                self.buf.emit(&[0x49, 0x63, 0xC2]); // MOVSXD RAX, R10D
                to_store.push(self.emit_jmp_rel32());
                // Both UTF16 (`StringUTF16.compareTo`): the same byte loop over
                // the even min byte count; a mismatch at byte k lies in char
                // k >> 1, every earlier char equal. An odd byte length (not a
                // String the JDK builds) declines.
                self.patch_rel32_to_here(utf16);
                self.emit_str_test_mem8(RCX, 8, 1); // TEST BYTE [RCX+8], 1
                decline.push(self.emit_jcc_rel32(0x85)); // JNZ: odd
                self.emit_str_test_mem8(R11, 8, 1); // TEST BYTE [R11+8], 1
                decline.push(self.emit_jcc_rel32(0x85)); // JNZ: odd
                self.emit_load_base_disp32(RAX, RCX, 8, false);
                self.emit_load_base_disp32(RDX, R11, 8, false);
                self.buf.emit(&[0x39, 0xD0]); // CMP EAX, EDX
                self.buf.emit(&[0x0F, 0x47, 0xC2]); // CMOVA EAX, EDX
                let (_, tail_done16, byte_mismatch16) = self.emit_str_byte_compare_loop(true);
                // Equal prefix: (len1 >> 1) - (len2 >> 1).
                self.patch_rel32_to_here(tail_done16);
                self.emit_load_base_disp32(RAX, RCX, 8, false);
                self.emit_load_base_disp32(RDX, R11, 8, false);
                self.buf.emit(&[0xD1, 0xE8]); // SHR EAX, 1
                self.buf.emit(&[0xD1, 0xEA]); // SHR EDX, 1
                self.buf.emit(&[0x29, 0xD0]); // SUB EAX, EDX
                self.buf.emit(&[0x48, 0x63, 0xC0]); // MOVSXD RAX, EAX
                to_store.push(self.emit_jmp_rel32());
                // First differing char: the one holding byte EDX.
                self.patch_rel32_to_here(byte_mismatch16);
                self.buf.emit(&[0x83, 0xE2, 0xFE]); // AND EDX, -2
                self.buf.emit(&[0x44, 0x0F, 0xB7, 0x54, 0x11, 0x10]); // MOVZX R10D, WORD [RCX+RDX+16]
                self.buf.emit(&[0x41, 0x0F, 0xB7, 0x44, 0x13, 0x10]); // MOVZX EAX, WORD [R11+RDX+16]
                self.buf.emit(&[0x41, 0x29, 0xC2]); // SUB R10D, EAX
                self.buf.emit(&[0x49, 0x63, 0xC2]); // MOVSXD RAX, R10D
                if mixed_on {
                    to_store.push(self.emit_jmp_rel32());
                    // Mixed coders (`StringLatin1.compareToUTF16` /
                    // `StringUTF16.compareToLatin1`): char k of the LATIN1
                    // side is byte k, of the UTF16 side code unit k; the first
                    // differing pair's difference, else the length
                    // difference in chars. RCX = this.value, EDX = this.coder;
                    // R11 = that.value. An odd UTF16 byte length declines.
                    self.patch_rel32_to_here(mixed);
                    self.buf.emit(&[0x85, 0xD2]); // TEST EDX, EDX
                    let this_utf16 = self.emit_jcc_rel32(0x85); // JNZ
                    self.emit_str_compare_to_mixed_arm(false, &mut decline, &mut to_store);
                    self.patch_rel32_to_here(this_utf16);
                    self.emit_str_compare_to_mixed_arm(true, &mut decline, &mut to_store);
                }
            }
            K::IndexOfChar => {
                let arg = arg_id;
                self.emit_str_value_and_coder(&layout, RAX, RCX, Some(RDX));
                self.emit_str_test_r64(RCX);
                decline.push(self.emit_jcc_rel32(0x84)); // JZ
                                                         // R10D = ch.
                self.gp_load_value(R10, arg);
                self.buf.emit(&[0x85, 0xD2]); // TEST EDX, EDX
                let utf16 = self.emit_jcc_rel32(0x85); // JNZ: UTF16
                let sse = crate::ir_string_sse2_enabled();
                let mut minus_one: Vec<usize> = Vec::new();
                let mut done: Vec<usize> = Vec::new();
                // TEST R10D, 0xFFFFFF00: `ch >>> 8 != 0` cannot be in a LATIN1
                // string (`StringLatin1.canEncode`).
                self.buf.emit(&[0x41, 0xF7, 0xC2, 0x00, 0xFF, 0xFF, 0xFF]);
                minus_one.push(self.emit_jcc_rel32(0x85)); // JNZ -> -1
                self.emit_load_base_disp32(R11, RCX, 8, false); // R11D = length
                self.buf.emit(&[0x31, 0xD2]); // XOR EDX, EDX
                self.emit_str_byte_scan(sse, &mut minus_one, &mut done);
                // UTF16 (`StringUTF16.indexOfChar`): a needle in 0..=0xFFFF is
                // one code unit, every one of which (a lone surrogate too) the
                // JDK scans for as such; a negative or supplementary needle
                // declines (the JDK's validity gate and pair search).
                self.patch_rel32_to_here(utf16);
                self.buf.emit(&[0x41, 0x81, 0xFA, 0xFF, 0xFF, 0x00, 0x00]); // CMP R10D, 0xFFFF
                if crate::ir_string_indexof_supplementary_enabled() {
                    // JIT round 21, lane strintr (STR16-P3,
                    // `CRATONVM_JIT_IR_STRING_INDEXOF_SUPPLEMENTARY`): every
                    // needle outside `0..=0xFFFF` answered inline as
                    // `StringUTF16.indexOf` does -- `indexOfChar` never matches
                    // a negative one (a `char` is never negative), and
                    // `indexOfSupplementary` answers -1 for an invalid code
                    // point and otherwise scans for the surrogate pair at
                    // `i < length - 1`.
                    let bmp = self.emit_jcc_rel32(0x86); // JBE: 0..=0xFFFF
                    self.buf.emit(&[0x41, 0x81, 0xFA, 0xFF, 0xFF, 0x10, 0x00]); // CMP R10D, 0x10FFFF
                    minus_one.push(self.emit_jcc_rel32(0x87)); // JA (unsigned: negatives too) -> -1
                    self.emit_str_test_mem8(RCX, 8, 1); // TEST BYTE [RCX+8], 1
                    decline.push(self.emit_jcc_rel32(0x85)); // JNZ: an odd byte length
                    self.emit_load_base_disp32(R11, RCX, 8, false); // R11D = byte length
                    self.buf.emit(&[0x31, 0xD2]); // XOR EDX, EDX
                    self.emit_str_pair_scan(&mut minus_one, &mut done);
                    self.patch_rel32_to_here(bmp);
                } else {
                    decline.push(self.emit_jcc_rel32(0x87)); // JA (unsigned: negatives too)
                }
                self.emit_str_test_mem8(RCX, 8, 1); // TEST BYTE [RCX+8], 1
                decline.push(self.emit_jcc_rel32(0x85)); // JNZ: an odd byte length
                self.emit_load_base_disp32(R11, RCX, 8, false); // R11D = byte length
                self.buf.emit(&[0x31, 0xD2]); // XOR EDX, EDX (a byte offset)
                self.emit_str_unit_scan(sse, &mut minus_one, &mut done);
                self.emit_str_index_result(minus_one, done);
            }
            K::IndexOfStr => {
                let arg = arg_id;
                // R10 = needle; null declines (the call's NPE).
                self.gp_load_value(R10, arg);
                self.buf.emit(&[0x4D, 0x85, 0xD2]); // TEST R10, R10
                decline.push(self.emit_jcc_rel32(0x84)); // JZ
                self.emit_str_cmp_class_id(R10, sid);
                decline.push(self.emit_jcc_rel32(0x85)); // JNE
                                                         // RCX = haystack bytes, EDX = its coder; R11 = needle bytes,
                                                         // EAX = its coder.
                self.emit_str_value_and_coder(&layout, RAX, RCX, Some(RDX));
                self.emit_str_test_r64(RCX);
                decline.push(self.emit_jcc_rel32(0x84)); // JZ
                self.emit_str_value_and_coder(&layout, R10, R11, Some(RAX));
                self.emit_str_test_r64(R11);
                decline.push(self.emit_jcc_rel32(0x84)); // JZ
                // nio20 wave 4, L5 (W3-L2-1): all four coder pairs. HotSpot's
                // Latin1 haystack / UTF16 needle arm returns -1 by coder
                // dispatch; preserve that result without relying on a
                // compact-string invariant.
                self.buf.emit(&[0x85, 0xC0]); // TEST EAX, EAX
                let needle_latin1 = self.emit_jcc_rel32(0x84); // JZ
                self.buf.emit(&[0x85, 0xD2]); // TEST EDX, EDX
                let latin1_hay_utf16_needle = self.emit_jcc_rel32(0x84); // JZ
                self.emit_str_index_of_str_body(
                    &layout,
                    recv,
                    true,
                    true,
                    &mut decline,
                    &mut to_store,
                );
                let utf16_done = self.emit_jmp_rel32();
                self.patch_rel32_to_here(latin1_hay_utf16_needle);
                self.buf
                    .emit(&[0x48, 0xC7, 0xC0, 0xFF, 0xFF, 0xFF, 0xFF]); // MOV RAX, -1
                to_store.push(self.emit_jmp_rel32());
                self.patch_rel32_to_here(utf16_done);
                self.patch_rel32_to_here(needle_latin1);
                self.buf.emit(&[0x85, 0xD2]); // TEST EDX, EDX
                let hay_utf16 = self.emit_jcc_rel32(0x85); // JNZ
                self.emit_str_index_of_str_body(
                    &layout,
                    recv,
                    false,
                    false,
                    &mut decline,
                    &mut to_store,
                );
                self.patch_rel32_to_here(hay_utf16);
                self.emit_str_index_of_str_body(
                    &layout,
                    recv,
                    true,
                    false,
                    &mut decline,
                    &mut to_store,
                );
            }
            K::CodePointAt => {
                // JIT r21 wave 1, lane strings (CP21-1): JDK 17 / 21 / 25
                // `String.codePointAt` -- LATIN1: `checkIndex(index,
                // value.length)`, `value[index] & 0xff`; UTF16: `length =
                // value.length >> 1`, `checkIndex(index, length)`,
                // `StringUTF16.codePointAt(value, index, length)` (the high
                // unit, unless it is a high surrogate whose successor inside
                // `length` is a low one: then `Character.toCodePoint`). Every
                // index outside the range declines; the call throws the SIOOBE.
                let index = arg_id;
                self.emit_str_value_and_coder(&layout, RAX, RCX, Some(RDX));
                self.emit_str_test_r64(RCX);
                decline.push(self.emit_jcc_rel32(0x84)); // JZ
                                                         // R10 = index, zero-extended: a negative index is a huge
                                                         // unsigned one and declines at the compares below.
                self.gp_load_value(R10, index);
                self.buf.emit(&[0x45, 0x89, 0xD2]); // MOV R10D, R10D
                self.emit_load_base_disp32(RAX, RCX, 8, false); // EAX = value.length
                self.buf.emit(&[0x85, 0xD2]); // TEST EDX, EDX
                let utf16 = self.emit_jcc_rel32(0x85); // JNZ
                                                       // LATIN1: the byte, zero-extended.
                self.buf.emit(&[0x41, 0x39, 0xC2]); // CMP R10D, EAX
                decline.push(self.emit_jcc_rel32(0x83)); // JAE
                self.buf.emit(&[0x42, 0x0F, 0xB6, 0x44, 0x11, 0x10]); // MOVZX EAX, BYTE [RCX+R10+16]
                to_store.push(self.emit_jmp_rel32());
                // UTF16: EAX = length in chars; EDX = c1.
                self.patch_rel32_to_here(utf16);
                self.buf.emit(&[0xD1, 0xE8]); // SHR EAX, 1
                self.buf.emit(&[0x41, 0x39, 0xC2]); // CMP R10D, EAX
                decline.push(self.emit_jcc_rel32(0x83)); // JAE
                self.buf.emit(&[0x42, 0x0F, 0xB7, 0x54, 0x51, 0x10]); // MOVZX EDX, WORD [RCX+R10*2+16]
                                                                      // `Character.isHighSurrogate(c1)`: (c1 & 0xFC00) == 0xD800.
                self.buf.emit(&[0x41, 0x89, 0xD3]); // MOV R11D, EDX
                self.buf.emit(&[0x41, 0x81, 0xE3, 0x00, 0xFC, 0x00, 0x00]); // AND R11D, 0xFC00
                self.buf.emit(&[0x41, 0x81, 0xFB, 0x00, 0xD8, 0x00, 0x00]); // CMP R11D, 0xD800
                let mut single: Vec<usize> = vec![self.emit_jcc_rel32(0x85)]; // JNE: not a high surrogate
                                                        // `++index < end` (index < length <= 2^30: no overflow).
                self.buf.emit(&[0x45, 0x8D, 0x5A, 0x01]); // LEA R11D, [R10+1]
                self.buf.emit(&[0x41, 0x39, 0xC3]); // CMP R11D, EAX
                single.push(self.emit_jcc_rel32(0x83)); // JAE: the last unit
                                                        // R11D = c2; `Character.isLowSurrogate(c2)`.
                self.buf.emit(&[0x46, 0x0F, 0xB7, 0x5C, 0x51, 0x12]); // MOVZX R11D, WORD [RCX+R10*2+18]
                self.buf.emit(&[0x44, 0x89, 0xD8]); // MOV EAX, R11D
                self.buf.emit(&[0x25, 0x00, 0xFC, 0x00, 0x00]); // AND EAX, 0xFC00
                self.buf.emit(&[0x3D, 0x00, 0xDC, 0x00, 0x00]); // CMP EAX, 0xDC00
                single.push(self.emit_jcc_rel32(0x85)); // JNE: an unpaired high surrogate
                                                        // `Character.toCodePoint(c1, c2)`:
                                                        // ((c1 << 10) + c2) + (0x10000 - (0xD800 << 10) - 0xDC00).
                self.buf.emit(&[0xC1, 0xE2, 0x0A]); // SHL EDX, 10
                self.buf.emit(&[0x44, 0x01, 0xDA]); // ADD EDX, R11D
                // `Character.MIN_SUPPLEMENTARY_CODE_POINT - (MIN_HIGH_SURROGATE
                // << 10) - MIN_LOW_SURROGATE`, as the JDK folds it.
                const PAIR_OFFSET: i32 = 0x1_0000 - (0xD800 << 10) - 0xDC00;
                self.buf.emit(&[0x81, 0xC2]); // ADD EDX, imm32
                self.buf.emit(&PAIR_OFFSET.to_le_bytes());
                for p in single {
                    self.patch_rel32_to_here(p);
                }
                // A non-negative int (<= 0x10FFFF): the 32-bit move's zero
                // extension is its sign extension.
                self.buf.emit(&[0x89, 0xD0]); // MOV EAX, EDX
            }
            K::CodePointBefore => {
                // JIT r23 wave 1, lane L5 (STRINGS21-4): JDK 17 / 21 / 25
                // `String.codePointBefore`; the body is shared with the
                // single-pass row (`x64::string_code_point`).
                self.emit_str_value_and_coder(&layout, RAX, RCX, Some(RDX));
                self.emit_str_test_r64(RCX);
                decline.push(self.emit_jcc_rel32(0x84)); // JZ
                self.gp_load_value(R10, arg_id);
                crate::x64::string_code_point::emit_code_point_before_tail(
                    &mut self.buf,
                    &mut decline,
                );
            }
            K::CodePointCount => {
                // JIT r23 wave 1, lane L5 (STRINGS21-4): JDK 17 / 21 / 25
                // `String.codePointCount`; the body is shared with the
                // single-pass row (`x64::string_code_point`).
                const R9: u8 = 9;
                self.emit_str_value_and_coder(&layout, RAX, RCX, Some(RDX));
                self.emit_str_test_r64(RCX);
                decline.push(self.emit_jcc_rel32(0x84)); // JZ
                // R10D = begin, R9D = end.
                self.gp_load_value(R10, arg_id);
                self.gp_load_value(R9, arg2_id);
                crate::x64::string_code_point::emit_code_point_count_tail(
                    &mut self.buf,
                    &mut decline,
                );
            }
            K::IndexOfCharFrom => {
                // JIT j27 wave 1, lane STRRX (`CRATONVM_JIT_IR_STRING_INDEXOF_FROM_PREFIX`).
                self.emit_str_index_of_char_from_arm(&layout, arg_id, arg2_id, &mut decline);
            }
            K::StrCoder | K::StrIsLatin1 => {
                // JIT r21 wave 1, lane strings (STRINGS21-1): JDK 17 / 21 / 25
                // `byte coder() { return COMPACT_STRINGS ? coder : UTF16; }`
                // and `boolean isLatin1() { return COMPACT_STRINGS && coder
                // == LATIN1; }`. EDX = the `coder` field (without touching `value`).
                self.emit_str_coder_load(&layout, compact_proven);
                // Cast: the flag byte's address as the imm64's bits.
                self.emit_mov_reg_imm64(R11, compact_flag_addr as u64);
                self.buf.emit(&[0x41, 0x80, 0x3B, 0x00]); // CMP BYTE [R11], 0
                let compact_off = self.emit_jcc_rel32(0x84); // JE: COMPACT_STRINGS false
                if kind == K::StrCoder {
                    self.buf.emit(&[0x89, 0xD0]); // MOV EAX, EDX
                } else {
                    self.buf.emit(&[0x31, 0xC0]); // XOR EAX, EAX
                    self.buf.emit(&[0x85, 0xD2]); // TEST EDX, EDX
                    self.buf.emit(&[0x0F, 0x94, 0xC0]); // SETE AL
                }
                to_store.push(self.emit_jmp_rel32());
                self.patch_rel32_to_here(compact_off);
                if kind == K::StrCoder {
                    self.buf.emit(&[0xB8, 0x01, 0x00, 0x00, 0x00]); // MOV EAX, 1 (UTF16)
                } else {
                    self.buf.emit(&[0x31, 0xC0]); // XOR EAX, EAX (false)
                }
            }
            K::CompareToIgnoreCase => {
                // JIT r21 wave 1, lane strings (STRINGS21-2 / ZCP-4): JDK 17 /
                // 21 / 25 `CaseInsensitiveComparator.compare` with two LATIN1
                // operands is `StringLatin1.compareToCI(v1, v2)`: for the
                // first `k < min(len1, len2)` with `v1[k] != v2[k]`, `c =
                // (char) CharacterDataLatin1.toUpperCase(b)` then
                // `Character.toLowerCase(c)` of each; unequal folds answer
                // `fold1 - fold2`. Over Latin-1 that two-step fold is the one
                // map `emit_latin1_ci_fold` encodes, so "equal after either
                // step" is `fold1 == fold2`. Otherwise `len1 - len2`.
                let arg = arg_id;
                // R10 = the argument; null declines (the call's NPE).
                self.gp_load_value(R10, arg);
                self.buf.emit(&[0x4D, 0x85, 0xD2]); // TEST R10, R10
                decline.push(self.emit_jcc_rel32(0x84)); // JZ
                self.emit_str_cmp_class_id(R10, sid);
                decline.push(self.emit_jcc_rel32(0x85)); // JNE
                                                         // RCX = v1, EDX = coder1; R11 = v2, EAX = coder2.
                self.emit_str_value_and_coder(&layout, RAX, RCX, Some(RDX));
                self.emit_str_test_r64(RCX);
                decline.push(self.emit_jcc_rel32(0x84)); // JZ
                self.emit_str_value_and_coder(&layout, R10, R11, Some(RAX));
                self.emit_str_test_r64(R11);
                decline.push(self.emit_jcc_rel32(0x84)); // JZ
                self.buf.emit(&[0x09, 0xD0]); // OR EAX, EDX
                decline.push(self.emit_jcc_rel32(0x85)); // JNZ: not two LATIN1 operands
                self.emit_latin1_ci_loop(); // RAX = the answer
            }
            // Never reached: the `StringBuilder` kinds return through
            // `try_emit_ir_string_builder_prefix` at the top, `getChars`
            // through `try_emit_ir_string_getchars_prefix` (nio20 wave 6).
            K::SbLength
            | K::SbCharAt
            | K::SbAppendChar
            | K::SbAppendString
            | K::SbAppendInt
            | K::SbToString
            | K::SbSetLength
            | K::GetChars
            | K::Inflate
            | K::ToCharArray
            | K::CharsValueOf
            | K::CharsInit
            | K::CountPositives
            | K::EncodeAsciiArray
            | K::SbAppendCodePoint
            | K::SbCodePointAt
            | K::ArraysEquals
            | K::CharacterToChars
            // Round rx27 (lane splitfast): `try_emit_ir_string_substring_prefix`.
            | K::Substring
            | K::SeqSubSequence
            | K::ArraysCopyOfRangeBytes
            // JIT r21: `try_emit_ir_string_latin1_case_prefix`; JIT round 21
            // (lanes strings / strings2): `try_emit_ir_latin1_compare_to_ci_prefix`,
            // `try_emit_ir_latin1_case_map_prefix`.
            | K::LowerLocaleUnchanged
            | K::LowerUnchanged
            | K::Latin1CompareToCI
            | K::Latin1ToLowerCase
            | K::Latin1ToUpperCase
            // JIT round 21, lane str21b: `try_emit_ir_latin1_region_matches_ci_prefix`.
            | K::Latin1RegionMatchesCI
            | K::ReplaceChar
            | K::Latin1ReplaceChar
            | K::ReplaceCharSeq
            | K::StringConcat => return,
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
        // nio21 wave 5 (lane L5): every decline edge deopts at the invoke; the
        // call lowering emitted after this is never reached.
        if let Some(pc) = trap_pc {
            self.emit_seq_charat_trap(pc);
        }
        self.call_prefix_join = Some(hit);
    }

    /// nio21 wave 5 (lane L5): a `SeqCharAt` call
    /// `ir_optimize::seq_charat_traps_on_miss` names could not take the String
    /// prefix: refuse the compile. The field forward has treated the call as a
    /// non-writer, so the plain interface call is not a fall-back.
    pub(in crate::ir_lower) fn refuse_seq_charat_trap(&self, trap: bool, why: &'static str) {
        if trap {
            self.latch_bailout(Bailout::with_context(
                BailoutReason::Internal(
                    "ir_lower: a trapping CharSequence.charAt cannot take the String prefix",
                ),
                why.to_string(),
            ));
        }
    }

    /// nio21 wave 5 (lane L5): can a deopt at the call pc `pc` publish the
    /// frame recorded at the top of that pc? Inside a spliced body only a
    /// resolvable caller chain will do (and none inside a synchronized
    /// splice's window, which publishes the flat frame); outside one, the
    /// method's own snapshot at `pc`.
    pub(in crate::ir_lower) fn seq_charat_trap_frame_ok(&self, pc: usize) -> bool {
        if self.pc_is_in_a_spliced_body(pc) || self.splice_state_first_by_pc.contains_key(&pc) {
            !self.sync_window_site_publishes_no_chain()
                && self
                    .splice_chain_snapshot_for_site(pc)
                    .is_some_and(|(si, _)| self.splice_chain_frame_state(si).is_ok())
        } else {
            self.safepoint_first_by_bci.contains_key(&pc)
        }
    }

    /// nio21 wave 5 (lane L5): the miss arm of a trapping `SeqCharAt` prefix:
    /// an unconditional deopt at the invoke's pc, which re-executes the
    /// interface call in the interpreter (any receiver, the JDK's own
    /// exception for an index out of range). The `emit_unconditional_deopt`
    /// shape, with the receiver-class reason so the deopt history charges it
    /// as a failed speculation.
    pub(in crate::ir_lower) fn emit_seq_charat_trap(&mut self, pc: usize) {
        self.buf.emit(&[0x31, 0xC0]); // XOR EAX, EAX  (ZF=1: always taken)
                                      // A guard that never passes has no passing arrival to force.
        let forced_chain_deopts = std::mem::replace(&mut self.forced_chain_deopts, false);
        self.emit_deopt_if_zero(pc, DeoptReason::ReceiverTypeChanged);
        self.forced_chain_deopts = forced_chain_deopts;
    }

    /// JIT j27 wave 1, lane STRRX (`CRATONVM_JIT_IR_STRING_INDEXOF_FROM_PREFIX`):
    /// the String prefix's `indexOf(int ch, int fromIndex)` arm, JDK 25's body
    /// exactly:
    ///
    /// ```text
    /// fromIndex = Math.max(fromIndex, 0);
    /// LATIN1: StringLatin1.indexOf(value, ch, Math.min(fromIndex, len), len)
    ///         -- canEncode(ch) ? indexOfChar(..) : -1
    /// UTF16:  StringUTF16.indexOf(value, ch, Math.min(fromIndex, len), len)
    ///         -- ch < 0x10000 ? indexOfChar(..) (a negative ch never matches)
    ///                         : indexOfSupplementary(..) (-1 unless valid; the
    ///                           pair at i < len - 1)
    /// ```
    ///
    /// No input throws (`indexOfSupplementary`'s `checkBoundsBeginEnd` holds
    /// for `from <= len`), so only a null `value`, an unknown coder and an odd
    /// UTF16 byte length decline (the receiver's null / class tests ran
    /// before). On entry RAX holds the receiver (tested); the answer is left in
    /// RAX, sign-extended, falling through to the caller's store. Scratch:
    /// RAX, RCX, RDX, R9, R10, R11, XMM0, XMM1 (the `CodePointCount` and
    /// `IndexOfChar` arms' set).
    pub(in crate::ir_lower) fn emit_str_index_of_char_from_arm(
        &mut self,
        layout: &crate::StringFieldLayout,
        ch: NodeId,
        from: NodeId,
        decline: &mut Vec<usize>,
    ) {
        const R9: u8 = 9;
        // RCX = value, EDX = coder.
        self.emit_str_value_and_coder(layout, RAX, RCX, Some(RDX));
        self.emit_str_test_r64(RCX);
        decline.push(self.emit_jcc_rel32(0x84)); // JZ
        self.gp_load_value(R10, ch); // R10D = ch
        self.gp_load_value(R9, from); // R9D = fromIndex
                                      // R9D = Math.max(fromIndex, 0).
        self.buf.emit(&[0x31, 0xC0]); // XOR EAX, EAX
        self.buf.emit(&[0x45, 0x85, 0xC9]); // TEST R9D, R9D
        self.buf.emit(&[0x44, 0x0F, 0x48, 0xC8]); // CMOVS R9D, EAX
        self.emit_load_base_disp32(R11, RCX, 8, false); // R11D = value.length (bytes)
        let sse = crate::ir_string_sse2_enabled();
        let mut minus_one: Vec<usize> = Vec::new();
        let mut done: Vec<usize> = Vec::new();
        self.buf.emit(&[0x85, 0xD2]); // TEST EDX, EDX
        let utf16 = self.emit_jcc_rel32(0x85); // JNZ

        // ---- LATIN1 ----
        // `!canEncode(ch)` (`ch >>> 8 != 0`, negatives included) -> -1.
        self.buf.emit(&[0x41, 0xF7, 0xC2, 0x00, 0xFF, 0xFF, 0xFF]); // TEST R10D, 0xFFFFFF00
        minus_one.push(self.emit_jcc_rel32(0x85)); // JNZ
                                                   // EDX = Math.min(from, len): both non-negative, so unsigned. The
                                                   // SSE stage needs `EDX <= R11D` (it subtracts).
        self.buf.emit(&[0x45, 0x39, 0xD9]); // CMP R9D, R11D
        self.buf.emit(&[0x45, 0x0F, 0x47, 0xCB]); // CMOVA R9D, R11D
        self.buf.emit(&[0x44, 0x89, 0xCA]); // MOV EDX, R9D
        self.emit_str_byte_scan(sse, &mut minus_one, &mut done);

        // ---- UTF16 ----
        self.patch_rel32_to_here(utf16);
        self.buf.emit(&[0x83, 0xFA, 0x01]); // CMP EDX, 1
        decline.push(self.emit_jcc_rel32(0x85)); // JNE: no such coder
        self.emit_str_test_mem8(RCX, 8, 1); // TEST BYTE [RCX+8], 1
        decline.push(self.emit_jcc_rel32(0x85)); // JNZ: an odd byte length
        self.buf.emit(&[0x41, 0xD1, 0xEB]); // SHR R11D, 1 (len in chars)
        self.buf.emit(&[0x45, 0x39, 0xD9]); // CMP R9D, R11D
        self.buf.emit(&[0x45, 0x0F, 0x47, 0xCB]); // CMOVA R9D, R11D (from = min(from, len))
                                                  // Byte offsets from here: EDX = 2 * from, R11D = the byte length.
        self.buf.emit(&[0x44, 0x89, 0xCA]); // MOV EDX, R9D
        self.buf.emit(&[0xD1, 0xE2]); // SHL EDX, 1
        self.buf.emit(&[0x41, 0xD1, 0xE3]); // SHL R11D, 1
        self.buf.emit(&[0x41, 0x81, 0xFA, 0xFF, 0xFF, 0x00, 0x00]); // CMP R10D, 0xFFFF
        let bmp = self.emit_jcc_rel32(0x86); // JBE: 0..=0xFFFF
                                             // A negative needle (`indexOfChar` never matches it) and an invalid
                                             // code point (`indexOfSupplementary`'s validity gate) -> -1.
        self.buf.emit(&[0x41, 0x81, 0xFA, 0xFF, 0xFF, 0x10, 0x00]); // CMP R10D, 0x10FFFF
        minus_one.push(self.emit_jcc_rel32(0x87)); // JA (unsigned: negatives too)
        self.emit_str_pair_scan(&mut minus_one, &mut done);
        self.patch_rel32_to_here(bmp);
        self.emit_str_unit_scan(sse, &mut minus_one, &mut done);
        self.emit_str_index_result(minus_one, done);
        // A call the single-pass backend has no inline form for (no resolver
        // row names `indexOf(II)I`): acceptance evidence, as the `getChars`
        // prefix notes it, so a body whose only gain is this site is not
        // refused and memoised.
        crate::ir_evidence::note(crate::ir_evidence::Transform::ScalarIntrinsic);
    }

    /// The String prefix's LATIN1 `indexOfChar` scan (shared by the
    /// `indexOf(int)` and `indexOf(int, int)` arms): RCX the `byte[]`, EDX the
    /// start (`<= R11D`, which the SSE stage needs), R11D the length, R10B the
    /// needle. A hit pushes onto `done` with the index in EAX; the end of the
    /// array onto `minus_one`. 16 bytes per SSE2 step under `sse`.
    pub(in crate::ir_lower) fn emit_str_byte_scan(
        &mut self,
        sse: bool,
        minus_one: &mut Vec<usize>,
        done: &mut Vec<usize>,
    ) {
        if sse {
            // nio20 wave 4, L5 (W3-L2-3): 16 bytes per step.
            self.buf.emit(&[0x66, 0x41, 0x0F, 0x6E, 0xCA]); // MOVD XMM1, R10D
            self.buf.emit(&[0x66, 0x0F, 0x60, 0xC9]); // PUNPCKLBW XMM1, XMM1
            self.buf.emit(&[0x66, 0x0F, 0x61, 0xC9]); // PUNPCKLWD XMM1, XMM1
            self.buf.emit(&[0x66, 0x0F, 0x70, 0xC9, 0x00]); // PSHUFD XMM1, XMM1, 0
            done.push(self.emit_str_sse_scan_stage(false));
        }
        let top = self.buf.pos();
        self.buf.emit(&[0x44, 0x39, 0xDA]); // CMP EDX, R11D
        minus_one.push(self.emit_jcc_rel32(0x83)); // JAE -> -1
        self.buf.emit(&[0x44, 0x38, 0x54, 0x11, 0x10]); // CMP BYTE [RCX+RDX+16], R10B
        let found = self.emit_jcc_rel32(0x84); // JE
        self.buf.emit(&[0xFF, 0xC2]); // INC EDX
        self.emit_jmp_back_to(top);
        self.patch_rel32_to_here(found);
        self.buf.emit(&[0x89, 0xD0]); // MOV EAX, EDX
        done.push(self.emit_jmp_rel32());
    }

    /// The UTF16 `indexOfChar` scan for a needle in `0..=0xFFFF` (R10W): EDX
    /// the start BYTE offset (even, `<= R11D`), R11D the byte length (even).
    /// A hit leaves the char index in EAX; the contract of
    /// [`Self::emit_str_byte_scan`] otherwise.
    pub(in crate::ir_lower) fn emit_str_unit_scan(
        &mut self,
        sse: bool,
        minus_one: &mut Vec<usize>,
        done: &mut Vec<usize>,
    ) {
        if sse {
            self.buf.emit(&[0x66, 0x41, 0x0F, 0x6E, 0xCA]); // MOVD XMM1, R10D
            self.buf.emit(&[0x66, 0x0F, 0x61, 0xC9]); // PUNPCKLWD XMM1, XMM1
            self.buf.emit(&[0x66, 0x0F, 0x70, 0xC9, 0x00]); // PSHUFD XMM1, XMM1, 0
            done.push(self.emit_str_sse_scan_stage(true));
        }
        let top16 = self.buf.pos();
        self.buf.emit(&[0x44, 0x39, 0xDA]); // CMP EDX, R11D
        minus_one.push(self.emit_jcc_rel32(0x83)); // JAE -> -1
        self.buf.emit(&[0x66, 0x44, 0x39, 0x54, 0x11, 0x10]); // CMP WORD [RCX+RDX+16], R10W
        let found16 = self.emit_jcc_rel32(0x84); // JE
        self.buf.emit(&[0x83, 0xC2, 0x02]); // ADD EDX, 2
        self.emit_jmp_back_to(top16);
        self.patch_rel32_to_here(found16);
        self.buf.emit(&[0x89, 0xD0]); // MOV EAX, EDX
        self.buf.emit(&[0xD1, 0xE8]); // SHR EAX, 1
        done.push(self.emit_jmp_rel32());
    }

    /// `StringUTF16.indexOfSupplementary`'s scan for a VALID supplementary
    /// code point in R10D (clobbered): the surrogate pair as one
    /// little-endian dword (`hi | lo << 16`) at char `i < length - 1`. EDX the
    /// start BYTE offset (even), R11D the byte length (even; clobbered). A hit
    /// leaves the char index in EAX; the contract of
    /// [`Self::emit_str_byte_scan`] otherwise. Scratch EAX.
    pub(in crate::ir_lower) fn emit_str_pair_scan(
        &mut self,
        minus_one: &mut Vec<usize>,
        done: &mut Vec<usize>,
    ) {
        // EAX = hi = (ch >>> 10) + (0xD800 - (0x10000 >>> 10)).
        self.buf.emit(&[0x44, 0x89, 0xD0]); // MOV EAX, R10D
        self.buf.emit(&[0xC1, 0xE8, 0x0A]); // SHR EAX, 10
        self.buf.emit(&[0x05, 0xC0, 0xD7, 0x00, 0x00]); // ADD EAX, 0xD7C0
                                                        // R10D = (lo << 16) | hi, lo = (ch & 0x3FF) + 0xDC00.
        self.buf.emit(&[0x41, 0x81, 0xE2, 0xFF, 0x03, 0x00, 0x00]); // AND R10D, 0x3FF
        self.buf.emit(&[0x41, 0x81, 0xC2, 0x00, 0xDC, 0x00, 0x00]); // ADD R10D, 0xDC00
        self.buf.emit(&[0x41, 0xC1, 0xE2, 0x10]); // SHL R10D, 16
        self.buf.emit(&[0x41, 0x09, 0xC2]); // OR R10D, EAX
                                            // R11D = byte length - 2: a pair at byte offset EDX fits while
                                            // EDX < R11D, signed (an empty value gives -2).
        self.buf.emit(&[0x41, 0x83, 0xEB, 0x02]); // SUB R11D, 2
        let top_pair = self.buf.pos();
        self.buf.emit(&[0x44, 0x39, 0xDA]); // CMP EDX, R11D
        minus_one.push(self.emit_jcc_rel32(0x8D)); // JGE -> -1
        self.buf.emit(&[0x44, 0x39, 0x54, 0x11, 0x10]); // CMP DWORD [RCX+RDX+16], R10D
        let found_pair = self.emit_jcc_rel32(0x84); // JE
        self.buf.emit(&[0x83, 0xC2, 0x02]); // ADD EDX, 2
        self.emit_jmp_back_to(top_pair);
        self.patch_rel32_to_here(found_pair);
        self.buf.emit(&[0x89, 0xD0]); // MOV EAX, EDX
        self.buf.emit(&[0xD1, 0xE8]); // SHR EAX, 1
        done.push(self.emit_jmp_rel32());
    }

    /// The `indexOf` arms' join: every `minus_one` edge answers -1, every
    /// `done` edge its EAX; RAX = the answer, sign-extended.
    pub(in crate::ir_lower) fn emit_str_index_result(
        &mut self,
        minus_one: Vec<usize>,
        done: Vec<usize>,
    ) {
        for p in minus_one {
            self.patch_rel32_to_here(p);
        }
        self.buf.emit(&[0xB8, 0xFF, 0xFF, 0xFF, 0xFF]); // MOV EAX, -1
        for p in done {
            self.patch_rel32_to_here(p);
        }
        self.buf.emit(&[0x48, 0x63, 0xC0]); // MOVSXD RAX, EAX
    }

    /// JIT round 21, lane strintr (STR16-P3,
    /// `CRATONVM_JIT_IR_STRING_COMPARE_TO_MIXED`): one direction of the
    /// `String.compareTo` prefix's MIXED coder arm. On entry RCX is
    /// `this.value` and R11 `that.value` (both non-null `byte[]`s); with
    /// `this_utf16` RCX holds UTF-16 code units and R11 LATIN1 bytes, otherwise
    /// the reverse. The answer is `StringLatin1.compareToUTF16` /
    /// `StringUTF16.compareToLatin1`'s: `this[k] - that[k]` (decoded chars,
    /// unsigned) at the first differing `k < min(len1, len2)`, else
    /// `len1 - len2` in chars, in RAX (sign-extended) behind a `to_store`
    /// jump. An odd UTF16 byte length (not a String the JDK builds) declines.
    /// Scratch: RAX, RDX, R10 (RCX and R11 are read only).
    pub(in crate::ir_lower) fn emit_str_compare_to_mixed_arm(
        &mut self,
        this_utf16: bool,
        decline: &mut Vec<usize>,
        to_store: &mut Vec<usize>,
    ) {
        // The UTF16 side's byte length must be even.
        self.emit_str_test_mem8(if this_utf16 { RCX } else { R11 }, 8, 1);
        decline.push(self.emit_jcc_rel32(0x85)); // JNZ: odd
                                                 // EAX = len1 (chars), EDX = len2 (chars); EAX = min (unsigned).
        self.emit_load_base_disp32(RAX, RCX, 8, false);
        self.emit_load_base_disp32(RDX, R11, 8, false);
        if this_utf16 {
            self.buf.emit(&[0xD1, 0xE8]); // SHR EAX, 1
        } else {
            self.buf.emit(&[0xD1, 0xEA]); // SHR EDX, 1
        }
        self.buf.emit(&[0x39, 0xD0]); // CMP EAX, EDX
        self.buf.emit(&[0x0F, 0x47, 0xC2]); // CMOVA EAX, EDX
        self.buf.emit(&[0x31, 0xD2]); // XOR EDX, EDX (k)
        let top = self.buf.pos();
        self.buf.emit(&[0x39, 0xC2]); // CMP EDX, EAX
        let equal_prefix = self.emit_jcc_rel32(0x83); // JAE
        if this_utf16 {
            // R10D = that[k] (a byte); compare with this[k] (a unit).
            self.buf.emit(&[0x45, 0x0F, 0xB6, 0x54, 0x13, 0x10]); // MOVZX R10D, BYTE [R11+RDX+16]
            self.buf.emit(&[0x66, 0x44, 0x3B, 0x54, 0x51, 0x10]); // CMP R10W, WORD [RCX+RDX*2+16]
        } else {
            // R10D = this[k] (a byte); compare with that[k] (a unit).
            self.buf.emit(&[0x44, 0x0F, 0xB6, 0x54, 0x11, 0x10]); // MOVZX R10D, BYTE [RCX+RDX+16]
            self.buf.emit(&[0x66, 0x45, 0x3B, 0x54, 0x53, 0x10]); // CMP R10W, WORD [R11+RDX*2+16]
        }
        let differ = self.emit_jcc_rel32(0x85); // JNE
        self.buf.emit(&[0xFF, 0xC2]); // INC EDX
        self.emit_jmp_back_to(top);
        // First differing char at k = EDX.
        self.patch_rel32_to_here(differ);
        if this_utf16 {
            // this[k] - that[k] = unit - R10D.
            self.buf.emit(&[0x0F, 0xB7, 0x44, 0x51, 0x10]); // MOVZX EAX, WORD [RCX+RDX*2+16]
            self.buf.emit(&[0x44, 0x29, 0xD0]); // SUB EAX, R10D
            self.buf.emit(&[0x48, 0x63, 0xC0]); // MOVSXD RAX, EAX
        } else {
            // this[k] - that[k] = R10D - unit.
            self.buf.emit(&[0x41, 0x0F, 0xB7, 0x44, 0x53, 0x10]); // MOVZX EAX, WORD [R11+RDX*2+16]
            self.buf.emit(&[0x41, 0x29, 0xC2]); // SUB R10D, EAX
            self.buf.emit(&[0x49, 0x63, 0xC2]); // MOVSXD RAX, R10D
        }
        to_store.push(self.emit_jmp_rel32());
        // Equal prefix: len1 - len2 in chars.
        self.patch_rel32_to_here(equal_prefix);
        self.emit_load_base_disp32(RAX, RCX, 8, false);
        self.emit_load_base_disp32(RDX, R11, 8, false);
        if this_utf16 {
            self.buf.emit(&[0xD1, 0xE8]); // SHR EAX, 1
        } else {
            self.buf.emit(&[0xD1, 0xEA]); // SHR EDX, 1
        }
        self.buf.emit(&[0x29, 0xD0]); // SUB EAX, EDX
        self.buf.emit(&[0x48, 0x63, 0xC0]); // MOVSXD RAX, EAX
        to_store.push(self.emit_jmp_rel32());
    }

    /// The shared byte-compare loop of [`Self::try_emit_ir_string_search_prefix`]'s
    /// `equals` / `compareTo` arms: RCX and R11 are the two byte arrays, EAX
    /// the count to compare; EDX is the index on every exit and R10 holds
    /// `RCX`'s byte at it on the byte mismatch. Eight bytes per step while
    /// eight remain, then one at a time.
    ///
    /// Returns `(block_mismatch, all_equal, byte_mismatch)`, the forward edges
    /// the caller routes. With `block_mismatch_to_bytes` (`compareTo`, which
    /// needs the first differing BYTE) an 8-byte mismatch falls into the byte
    /// loop, which finds it inside that block, and `block_mismatch` is empty;
    /// otherwise (`equals`) it is returned for the caller's `false`.
    ///
    /// nio20 wave 4, lane L5 (W3-L2-3, `CRATONVM_JIT_IR_STRING_SSE2`): a
    /// 16-byte SSE2 stage (`MOVDQU` x2, `PCMPEQB`, `PMOVMSKB`) runs first
    /// while 16 bytes remain, in XMM0/XMM1 (this lowerer's per-op FP scratch,
    /// dead at a call prefix). Its mismatch is a block mismatch for `equals`
    /// and, for `compareTo`, re-enters the 8-byte stage at the same index,
    /// which narrows it to the byte within at most two more steps.
    pub(in crate::ir_lower) fn emit_str_byte_compare_loop(
        &mut self,
        block_mismatch_to_bytes: bool,
    ) -> (Vec<usize>, usize, usize) {
        let mut mismatches: Vec<usize> = Vec::new();
        self.buf.emit(&[0x31, 0xD2]); // XOR EDX, EDX
        let mut to_blk: Vec<usize> = Vec::new();
        if crate::ir_string_sse2_enabled() {
            let v16 = self.buf.pos();
            self.buf.emit(&[0x41, 0x89, 0xC2]); // MOV R10D, EAX
            self.buf.emit(&[0x41, 0x29, 0xD2]); // SUB R10D, EDX
            self.buf.emit(&[0x41, 0x83, 0xFA, 0x10]); // CMP R10D, 16
            to_blk.push(self.emit_jcc_rel32(0x82)); // JB: fewer than 16 left
            self.buf.emit(&[0xF3, 0x0F, 0x6F, 0x44, 0x11, 0x10]); // MOVDQU XMM0, [RCX+RDX+16]
            self.buf.emit(&[0xF3, 0x41, 0x0F, 0x6F, 0x4C, 0x13, 0x10]); // MOVDQU XMM1, [R11+RDX+16]
            self.buf.emit(&[0x66, 0x0F, 0x74, 0xC1]); // PCMPEQB XMM0, XMM1
            self.buf.emit(&[0x66, 0x44, 0x0F, 0xD7, 0xD0]); // PMOVMSKB R10D, XMM0
            self.buf.emit(&[0x41, 0x81, 0xFA, 0xFF, 0xFF, 0x00, 0x00]); // CMP R10D, 0xFFFF
            let v_mismatch = self.emit_jcc_rel32(0x85); // JNE
            if block_mismatch_to_bytes {
                to_blk.push(v_mismatch);
            } else {
                mismatches.push(v_mismatch);
            }
            self.buf.emit(&[0x83, 0xC2, 0x10]); // ADD EDX, 16
            self.emit_jmp_back_to(v16);
        }
        for p in to_blk {
            self.patch_rel32_to_here(p);
        }
        let blk = self.buf.pos();
        self.buf.emit(&[0x41, 0x89, 0xC2]); // MOV R10D, EAX
        self.buf.emit(&[0x41, 0x29, 0xD2]); // SUB R10D, EDX
        self.buf.emit(&[0x41, 0x83, 0xFA, 0x08]); // CMP R10D, 8
        let tail_from_blk = self.emit_jcc_rel32(0x82); // JB: fewer than 8 left
        self.buf.emit(&[0x4C, 0x8B, 0x54, 0x11, 0x10]); // MOV R10, [RCX+RDX+16]
        self.buf.emit(&[0x4D, 0x3B, 0x54, 0x13, 0x10]); // CMP R10, [R11+RDX+16]
        let blk_mismatch = self.emit_jcc_rel32(0x85); // JNE
        self.buf.emit(&[0x83, 0xC2, 0x08]); // ADD EDX, 8
        self.emit_jmp_back_to(blk);
        let tail = self.buf.pos();
        self.patch_rel32_to_here(tail_from_blk);
        if block_mismatch_to_bytes {
            self.patch_rel32_to_here(blk_mismatch);
        } else {
            mismatches.push(blk_mismatch);
        }
        self.buf.emit(&[0x39, 0xC2]); // CMP EDX, EAX
        let tail_done = self.emit_jcc_rel32(0x83); // JAE: all equal
        self.buf.emit(&[0x44, 0x0F, 0xB6, 0x54, 0x11, 0x10]); // MOVZX R10D, BYTE [RCX+RDX+16]
        self.buf.emit(&[0x45, 0x3A, 0x54, 0x13, 0x10]); // CMP R10B, BYTE [R11+RDX+16]
        let byte_mismatch = self.emit_jcc_rel32(0x85); // JNE
        self.buf.emit(&[0xFF, 0xC2]); // INC EDX
        self.emit_jmp_back_to(tail);
        (mismatches, tail_done, byte_mismatch)
    }

    /// The 16-byte SSE2 stage of the String prefix's `indexOf(int)` scans
    /// (nio20 wave 4, lane L5, W3-L2-3): RCX the `byte[]`, EDX the byte
    /// offset to start at, R11D the byte length, XMM1 the needle broadcast
    /// to every byte (`words == false`) or every 16-bit lane. While 16 bytes
    /// remain, one `MOVDQU` + `PCMPEQB`/`PCMPEQW` + `PMOVMSKB`; a hit leaves
    /// the char index in EAX and takes the returned forward jump, otherwise
    /// the stage falls through with EDX at the first unscanned byte (and
    /// EAX clobbered) into the caller's scalar tail. Writes EAX, EDX, XMM0.
    pub(in crate::ir_lower) fn emit_str_sse_scan_stage(&mut self, words: bool) -> usize {
        let v = self.buf.pos();
        self.buf.emit(&[0x44, 0x89, 0xD8]); // MOV EAX, R11D
        self.buf.emit(&[0x29, 0xD0]); // SUB EAX, EDX
        self.buf.emit(&[0x83, 0xF8, 0x10]); // CMP EAX, 16
        let exit = self.emit_jcc_rel32(0x82); // JB: the scalar tail
        self.buf.emit(&[0xF3, 0x0F, 0x6F, 0x44, 0x11, 0x10]); // MOVDQU XMM0, [RCX+RDX+16]
        if words {
            self.buf.emit(&[0x66, 0x0F, 0x75, 0xC1]); // PCMPEQW XMM0, XMM1
        } else {
            self.buf.emit(&[0x66, 0x0F, 0x74, 0xC1]); // PCMPEQB XMM0, XMM1
        }
        self.buf.emit(&[0x66, 0x0F, 0xD7, 0xC0]); // PMOVMSKB EAX, XMM0
        self.buf.emit(&[0x85, 0xC0]); // TEST EAX, EAX
        let hit = self.emit_jcc_rel32(0x85); // JNZ
        self.buf.emit(&[0x83, 0xC2, 0x10]); // ADD EDX, 16
        self.emit_jmp_back_to(v);
        self.patch_rel32_to_here(hit);
        self.buf.emit(&[0x0F, 0xBC, 0xC0]); // BSF EAX, EAX (the first matching byte)
        self.buf.emit(&[0x01, 0xD0]); // ADD EAX, EDX
        if words {
            self.buf.emit(&[0xD1, 0xE8]); // SHR EAX, 1 (byte offset -> char index)
        }
        let found = self.emit_jmp_rel32();
        self.patch_rel32_to_here(exit);
        found
    }

    /// One coder pair of the String prefix's `indexOf(String)` (nio20 wave 4,
    /// lane L5, W3-L2-1): the naive first-occurrence search over a haystack
    /// of `hay_utf16` code units and a needle of `needle_utf16` ones (LATIN1 /
    /// LATIN1, UTF16 / UTF16, UTF16 haystack / LATIN1 needle -- the JDK's
    /// `StringLatin1.indexOf`, `StringUTF16.indexOf`, `StringUTF16.indexOfLatin1`
    /// answers). On entry RCX is the haystack's `byte[]`, R11 the needle's;
    /// every exit pushes a jump onto `to_store` with the answer in RAX, or
    /// onto `decline` (a UTF16 array of odd byte length). `recv` is re-read
    /// on the found edge (no GC point since: the same object).
    ///
    /// Register plan: EAX = candidate starts left minus one, RCX = the
    /// candidate's address, R10D = `j` as a NEEDLE byte offset, EDX = the
    /// needle unit.
    pub(in crate::ir_lower) fn emit_str_index_of_str_body(
        &mut self,
        layout: &crate::StringFieldLayout,
        recv: NodeId,
        hay_utf16: bool,
        needle_utf16: bool,
        decline: &mut Vec<usize>,
        to_store: &mut Vec<usize>,
    ) {
        if hay_utf16 {
            self.emit_str_test_mem8(RCX, 8, 1); // TEST BYTE [RCX+8], 1
            decline.push(self.emit_jcc_rel32(0x85)); // JNZ: odd
        }
        if needle_utf16 {
            self.emit_str_test_mem8(R11, 8, 1); // TEST BYTE [R11+8], 1
            decline.push(self.emit_jcc_rel32(0x85)); // JNZ: odd
        }
        // EDX = the needle's byte length; an empty needle is found at 0.
        self.emit_load_base_disp32(RDX, R11, 8, false);
        self.buf.emit(&[0x85, 0xD2]); // TEST EDX, EDX
        let empty = self.emit_jcc_rel32(0x84); // JZ -> 0
                                               // EAX = n - m in chars, the positions left to try minus one; < 0 -> -1.
        self.emit_load_base_disp32(RAX, RCX, 8, false);
        if hay_utf16 {
            self.buf.emit(&[0xD1, 0xE8]); // SHR EAX, 1
        }
        if needle_utf16 {
            self.buf.emit(&[0xD1, 0xEA]); // SHR EDX, 1
        }
        self.buf.emit(&[0x29, 0xD0]); // SUB EAX, EDX
        let too_short = self.emit_jcc_rel32(0x88); // JS -> -1
                                                   // JIT r20 wave 12, lane qStr (W4-L5-1, `CRATONVM_JIT_IR_STRING_INDEXOF_SCAN`,
                                                   // inside `CRATONVM_JIT_IR_STRING_SSE2`): LATIN1 / LATIN1 skips
                                                   // candidate starts 16 at a time -- `PCMPEQB` of the 16 bytes at the
                                                   // cursor against the needle's first byte, `PMOVMSKB`, `BSF` to the
                                                   // first equal one -- before the byte compare, as HotSpot's
                                                   // `StringIndexOf` stub does. Taken only while at least 16 starts
                                                   // remain (EAX >= 15), so the 16-byte read stays inside the haystack:
                                                   // from the cursor to its end are EAX + m >= 16 bytes. A miss skips
                                                   // all 16 starts (none begins with the first byte); a hit moves the
                                                   // cursor to the first that does and lets the compare below decide.
        let scan = !hay_utf16
            && !needle_utf16
            && crate::ir_string_sse2_enabled()
            && crate::ir_string_indexof_scan_enabled();
        let mut scan_exhausted: Option<usize> = None;
        if scan {
            // XMM1 = needle[0] in every byte (EDX is free until the compare).
            self.buf.emit(&[0x41, 0x0F, 0xB6, 0x53, 0x10]); // MOVZX EDX, BYTE [R11+16]
            self.buf.emit(&[0x66, 0x0F, 0x6E, 0xCA]); // MOVD XMM1, EDX
            self.buf.emit(&[0x66, 0x0F, 0x60, 0xC9]); // PUNPCKLBW XMM1, XMM1
            self.buf.emit(&[0x66, 0x0F, 0x61, 0xC9]); // PUNPCKLWD XMM1, XMM1
            self.buf.emit(&[0x66, 0x0F, 0x70, 0xC9, 0x00]); // PSHUFD XMM1, XMM1, 0
        }
        let outer = self.buf.pos();
        if scan {
            self.buf.emit(&[0x83, 0xF8, 0x0F]); // CMP EAX, 15
            let scalar = self.emit_jcc_rel32(0x8C); // JL: fewer than 16 starts left
            self.buf.emit(&[0xF3, 0x0F, 0x6F, 0x41, 0x10]); // MOVDQU XMM0, [RCX+16]
            self.buf.emit(&[0x66, 0x0F, 0x74, 0xC1]); // PCMPEQB XMM0, XMM1
            self.buf.emit(&[0x66, 0x44, 0x0F, 0xD7, 0xD0]); // PMOVMSKB R10D, XMM0
            self.buf.emit(&[0x45, 0x85, 0xD2]); // TEST R10D, R10D
            let hit = self.emit_jcc_rel32(0x85); // JNZ
            self.buf.emit(&[0x48, 0x83, 0xC1, 0x10]); // ADD RCX, 16
            self.buf.emit(&[0x83, 0xE8, 0x10]); // SUB EAX, 16
            scan_exhausted = Some(self.emit_jcc_rel32(0x88)); // JS: no start left -> -1
            self.emit_jmp_back_to(outer);
            self.patch_rel32_to_here(hit);
            self.buf.emit(&[0x45, 0x0F, 0xBC, 0xD2]); // BSF R10D, R10D
            self.buf.emit(&[0x4C, 0x01, 0xD1]); // ADD RCX, R10
            self.buf.emit(&[0x44, 0x29, 0xD0]); // SUB EAX, R10D
            self.patch_rel32_to_here(scalar);
        }
        self.buf.emit(&[0x45, 0x31, 0xD2]); // XOR R10D, R10D
        let inner = self.buf.pos();
        self.buf.emit(&[0x45, 0x3B, 0x53, 0x08]); // CMP R10D, [R11+8]
        let found = self.emit_jcc_rel32(0x83); // JAE: the whole needle matched
        match (hay_utf16, needle_utf16) {
            (false, _) => {
                self.buf.emit(&[0x43, 0x0F, 0xB6, 0x54, 0x13, 0x10]); // MOVZX EDX, BYTE [R11+R10+16]
                self.buf.emit(&[0x42, 0x3A, 0x54, 0x11, 0x10]); // CMP DL, BYTE [RCX+R10+16]
            }
            (true, true) => {
                self.buf.emit(&[0x43, 0x0F, 0xB7, 0x54, 0x13, 0x10]); // MOVZX EDX, WORD [R11+R10+16]
                self.buf.emit(&[0x66, 0x42, 0x3B, 0x54, 0x11, 0x10]); // CMP DX, WORD [RCX+R10+16]
            }
            (true, false) => {
                self.buf.emit(&[0x43, 0x0F, 0xB6, 0x54, 0x13, 0x10]); // MOVZX EDX, BYTE [R11+R10+16]
                self.buf.emit(&[0x66, 0x42, 0x3B, 0x54, 0x51, 0x10]); // CMP DX, WORD [RCX+R10*2+16]
            }
        }
        let next = self.emit_jcc_rel32(0x85); // JNE
        if needle_utf16 {
            self.buf.emit(&[0x41, 0x83, 0xC2, 0x02]); // ADD R10D, 2
        } else {
            self.buf.emit(&[0x41, 0xFF, 0xC2]); // INC R10D
        }
        self.emit_jmp_back_to(inner);
        self.patch_rel32_to_here(next);
        if hay_utf16 {
            self.buf.emit(&[0x48, 0x83, 0xC1, 0x02]); // ADD RCX, 2
        } else {
            self.buf.emit(&[0x48, 0xFF, 0xC1]); // INC RCX
        }
        self.buf.emit(&[0xFF, 0xC8]); // DEC EAX
        self.emit_str_jcc_back(0x89, outer); // JNS
                                             // Not found.
        self.patch_rel32_to_here(too_short);
        if let Some(p) = scan_exhausted {
            self.patch_rel32_to_here(p);
        }
        self.buf.emit(&[0x48, 0xC7, 0xC0, 0xFF, 0xFF, 0xFF, 0xFF]); // MOV RAX, -1
        to_store.push(self.emit_jmp_rel32());
        self.patch_rel32_to_here(empty);
        self.buf.emit(&[0x31, 0xC0]); // XOR EAX, EAX
        to_store.push(self.emit_jmp_rel32());
        // Found: the index is the cursor minus the haystack's start, in
        // chars, the start re-read from the receiver.
        self.patch_rel32_to_here(found);
        self.gp_load_value(RDX, recv);
        self.emit_str_value_and_coder(layout, RDX, RDX, None);
        self.buf.emit(&[0x48, 0x89, 0xC8]); // MOV RAX, RCX
        self.buf.emit(&[0x48, 0x29, 0xD0]); // SUB RAX, RDX
        if hay_utf16 {
            self.buf.emit(&[0x48, 0xD1, 0xE8]); // SHR RAX, 1
        }
        to_store.push(self.emit_jmp_rel32());
    }

    /// `String.hashCode()`'s uncached arm in the String prefix (nio20 wave 4,
    /// lane L5, W3-L2-2, `CRATONVM_JIT_IR_STRING_HASH_COMPUTE`). On entry RAX
    /// is the receiver (a non-null String whose `hash` read 0). A COMPACT
    /// receiver's hash is computed inline -- `h = 31 * h + unit` over the
    /// LATIN1 bytes or the UTF16 code units, `StringLatin1.hashCode` /
    /// `StringUTF16.hashCode` -- and a NON-ZERO result is stored into `hash`
    /// (a plain `int` store, no barrier) and left in RAX for the caller's
    /// store path. A zero result declines (the call sets `hashIsZero`, which
    /// this layout does not address), as do a legacy receiver (its cell is
    /// the call's to write, as in the single-pass body), a null `value` and
    /// a UTF16 array of odd byte length. Writes RAX, RCX, RDX, R10, R11.
    ///
    /// `hash_is_zero` (STRGAP19-6, `CRATONVM_JIT_STRING_HASH_IS_ZERO`): the
    /// COMPACT `hashIsZero` byte's offset. With it a zero result is answered
    /// here too, after `hashIsZero = true` (a plain byte store, as the JDK
    /// body's), instead of declining; the receiver is COMPACT on that path.
    pub(in crate::ir_lower) fn emit_str_hash_compute(
        &mut self,
        layout: &crate::StringFieldLayout,
        recv: NodeId,
        decline: &mut Vec<usize>,
        hash_is_zero: Option<i32>,
    ) {
        self.emit_str_test_mem8(
            RAX,
            cratonvm_types::GC_FLAGS_BYTE_OFFSET as u8, // Cast: x86-64 disp8
            cratonvm_types::GC_FLAG_COMPACT,
        );
        decline.push(self.emit_jcc_rel32(0x84)); // JZ: legacy
        self.emit_str_value_and_coder(layout, RAX, RCX, Some(RDX));
        self.emit_str_test_r64(RCX);
        decline.push(self.emit_jcc_rel32(0x84)); // JZ
        self.emit_load_base_disp32(R11, RCX, 8, false); // R11D = byte length
        self.buf.emit(&[0x31, 0xC0]); // XOR EAX, EAX (h)
        self.buf.emit(&[0x85, 0xD2]); // TEST EDX, EDX
        let utf16 = self.emit_jcc_rel32(0x85); // JNZ
        crate::x64::concat_emit::emit_latin1_hash_unrolled4(&mut self.buf, 16);
        let done8 = self.emit_jmp_rel32();
        self.patch_rel32_to_here(utf16);
        self.emit_str_test_mem8(RCX, 8, 1); // TEST BYTE [RCX+8], 1
        decline.push(self.emit_jcc_rel32(0x85)); // JNZ: odd
        self.buf.emit(&[0x31, 0xD2]); // XOR EDX, EDX (a byte offset)
        let top16 = self.buf.pos();
        self.buf.emit(&[0x44, 0x39, 0xDA]); // CMP EDX, R11D
        let done16 = self.emit_jcc_rel32(0x83); // JAE
        self.buf.emit(&[0x44, 0x0F, 0xB7, 0x54, 0x11, 0x10]); // MOVZX R10D, WORD [RCX+RDX+16]
        self.buf.emit(&[0x6B, 0xC0, 0x1F]); // IMUL EAX, EAX, 31
        self.buf.emit(&[0x44, 0x01, 0xD0]); // ADD EAX, R10D
        self.buf.emit(&[0x83, 0xC2, 0x02]); // ADD EDX, 2
        self.emit_jmp_back_to(top16);
        self.patch_rel32_to_here(done8);
        self.patch_rel32_to_here(done16);
        self.buf.emit(&[0x85, 0xC0]); // TEST EAX, EAX
        if let Some(off) = hash_is_zero {
            let nonzero = self.emit_jcc_rel32(0x85); // JNZ
                                                     // `hashIsZero = true` on the (COMPACT) receiver; EAX stays 0.
            self.gp_load_value(RDX, recv);
            // MOV BYTE [RDX + disp32], 1 (C6 /0 ib, ModRM 0x82)
            self.buf.emit(&[0xC6, 0x82]);
            self.buf.emit(&off.to_le_bytes());
            self.buf.emit(&[0x01]);
            let joined = self.emit_jmp_rel32();
            self.patch_rel32_to_here(nonzero);
            self.gp_load_value(RDX, recv);
            self.emit_str_store32_base_disp32(RAX, RDX, layout.hash_compact_offset);
            self.patch_rel32_to_here(joined);
            self.buf.emit(&[0x48, 0x63, 0xC0]); // MOVSXD RAX, EAX
            return;
        }
        decline.push(self.emit_jcc_rel32(0x84)); // JZ: the call sets `hashIsZero`
                                                 // The receiver again (no GC point since), then `hash = h` (compact).
        self.gp_load_value(RDX, recv);
        self.emit_str_store32_base_disp32(RAX, RDX, layout.hash_compact_offset);
        self.buf.emit(&[0x48, 0x63, 0xC0]); // MOVSXD RAX, EAX
    }

    /// `MOV DWORD [base + disp32], src` (`89 /r`; a SIB byte for an RSP/R12
    /// base).
    pub(in crate::ir_lower) fn emit_str_store32_base_disp32(
        &mut self,
        src: u8,
        base: u8,
        disp: i32,
    ) {
        let rex = 0x40u8 | (((src >= 8) as u8) << 2) | ((base >= 8) as u8);
        if rex != 0x40 {
            self.buf.emit_byte(rex);
        }
        self.buf.emit(&[0x89, 0x80 | ((src & 7) << 3) | (base & 7)]);
        if base & 7 == 4 {
            self.buf.emit_byte(0x24);
        }
        self.buf.emit(&disp.to_le_bytes());
    }

    /// nio20 wave 4, lane L5 (proposal W3-L2-4): `StringBuilder.length()`,
    /// `charAt(int)` and `append(char)` answered inline as a guarded PREFIX of
    /// the `Op::Call`, over the layout's builder half
    /// ([`crate::StringBuilderFieldLayout`]) -- the single-pass
    /// STRINGBUILDER_ACCESS bodies (`x64/objects.rs`'s
    /// `emit_sb_append_char_body` / `emit_sb_char_at_body`, the `length` arm
    /// of `op_invoke.rs`), with every slow edge a decline to the call, which
    /// they already were there (a call, never a deopt). Same contract with
    /// the `Op::Call` arm as [`Self::try_emit_ir_string_search_prefix`].
    ///
    /// * `length()` -- `count`.
    /// * `charAt(int)` -- an index below `count` AND below the array's
    ///   capacity in chars (unsigned, so negatives decline), either coder; the
    ///   call raises the `StringIndexOutOfBoundsException`.
    /// * `append(char)` -- a LATIN1 builder, a char in `0..=0xFF`, and
    ///   `count < value.length` (unsigned): `value[count] = (byte) c;
    ///   count++`, returning the receiver. A full payload, a UTF16 builder
    ///   and a wider char decline (growth and inflation are the JDK body's /
    ///   native's). Two plain stores, a byte into a `byte[]` and an `int`
    ///   field: no reference, so no barrier; no allocation, so no GC point.
    ///   `maybeLatin1` is not touched: the JDK's LATIN1 arm does not touch it
    ///   either.
    ///
    /// The receiver guard is the exact class id (`StringBuilder` is final),
    /// which keeps `StringBuffer` (synchronized, a `toStringCache`) off the
    /// path. Per-object COMPACT / LEGACY dispatch on `GC_FLAG_COMPACT`, as
    /// the single-pass bodies.
    /// Stage reporting (`CRATONVM_DBG_IR_COMPILES` / `CRATONVM_DBG_JITC`):
    /// why a String / StringBuilder prefix site was left a plain call.
    pub(in crate::ir_lower) fn note_string_prefix_declined(
        &self,
        id: NodeId,
        info: &JitInvokeInfo,
        reason: &str,
    ) {
        if crate::ir_stage_reporting() {
            eprintln!(
                "[ir] string-prefix-declined n{id} {}.{}{}: {reason}",
                info.class_name, info.method_name, info.descriptor,
            );
        }
    }

    fn emit_sb_decode_value_word_if_zgc(
        &mut self,
        zgc_base: Option<u64>,
        decline: &mut Vec<usize>,
    ) {
        if let Some(base) = zgc_base {
            const R8: u8 = 8;
            const R9: u8 = 9;
            let _ = (R8, R9);
            self.buf.emit(&[0x49, 0x89, 0xC1]); // MOV R9, RAX (save recv)
            self.buf.emit(&[0x45, 0x89, 0xD8]); // MOV R8D, R11D (save count)
            self.buf.emit(&[0x48, 0x89, 0xC8]); // MOV RAX, RCX (raw value)
            let good = self.emit_colored_ref_word_barrier(base, decline);
            for p in good {
                self.patch_rel32_to_here(p);
            }
            self.buf.emit(&[0x48, 0x89, 0xC1]); // MOV RCX, RAX (decoded value)
            self.buf.emit(&[0x4C, 0x89, 0xC8]); // MOV RAX, R9 (restore recv)
            self.buf.emit(&[0x45, 0x89, 0xC3]); // MOV R11D, R8D (restore count)
        }
    }

    /// Load `StringBuilder` `{coder -> EDX, count -> R11D, value -> RCX}` from
    /// receiver `RAX`, eliding the `GC_FLAG_COMPACT` test and legacy branch when
    /// `compact_proven` is true.
    fn emit_sb_read_fields(&mut self, b: &crate::StringBuilderFieldLayout, compact_proven: bool) {
        let emit_compact = |lower: &mut Self| {
            if b.coder_compact_is_byte {
                lower.emit_str_movzx8_base_disp32(RDX, RAX, b.coder_compact_offset);
            } else {
                lower.emit_load_base_disp32(RDX, RAX, b.coder_compact_offset, false);
            }
            lower.emit_load_base_disp32(R11, RAX, b.count_compact_offset, false);
            lower.emit_load_base_disp32(RCX, RAX, b.value_compact_offset, true);
        };
        if compact_proven {
            emit_compact(self);
            return;
        }
        self.emit_str_test_mem8(
            RAX,
            cratonvm_types::GC_FLAGS_BYTE_OFFSET as u8, // Cast: x86-64 disp8
            cratonvm_types::GC_FLAG_COMPACT,
        );
        let legacy = self.emit_jcc_rel32(0x84); // JZ legacy
        emit_compact(self);
        let have = self.emit_jmp_rel32();
        self.patch_rel32_to_here(legacy);
        self.emit_load_base_disp32(RDX, RAX, b.coder_legacy_offset, false);
        self.emit_load_base_disp32(R11, RAX, b.count_legacy_offset, false);
        self.emit_load_base_disp32(RCX, RAX, b.value_legacy_offset, true);
        self.patch_rel32_to_here(have);
    }

    /// Store updated `StringBuilder.count` from 32-bit `src` into receiver `RAX`,
    /// eliding the `GC_FLAG_COMPACT` test and legacy store when `compact_proven`.
    fn emit_sb_write_count(
        &mut self,
        src: u8,
        b: &crate::StringBuilderFieldLayout,
        compact_proven: bool,
    ) {
        if compact_proven {
            self.emit_str_store32_base_disp32(src, RAX, b.count_compact_offset);
            return;
        }
        self.emit_str_test_mem8(
            RAX,
            cratonvm_types::GC_FLAGS_BYTE_OFFSET as u8, // Cast: x86-64 disp8
            cratonvm_types::GC_FLAG_COMPACT,
        );
        let legacy_store = self.emit_jcc_rel32(0x84); // JZ legacy
        self.emit_str_store32_base_disp32(src, RAX, b.count_compact_offset);
        let stored = self.emit_jmp_rel32();
        self.patch_rel32_to_here(legacy_store);
        self.emit_str_store32_base_disp32(src, RAX, b.count_legacy_offset);
        self.patch_rel32_to_here(stored);
    }

    pub(in crate::ir_lower) fn try_emit_ir_string_builder_prefix(
        &mut self,
        id: NodeId,
        info: &JitInvokeInfo,
        kind: crate::IrStringSearchKind,
        layout: crate::StringFieldLayout,
    ) {
        use crate::IrStringSearchKind as K;
        // nio20 w5 fix-back (L5): every decline names itself under stage
        // reporting (`note_string_prefix_declined`), so a site that never
        // shows the `[ir] string-builder-prefix` line says why.
        let Some(b) = layout.builder else {
            self.note_string_prefix_declined(id, info, "layout has no StringBuilder half");
            return;
        };
        if !kind.is_builder()
            || !kind.admits_invoke_kind(info.invoke_kind)
            || b.class_id == 0
            || b.value_compact_is_narrow
        {
            self.note_string_prefix_declined(
                id,
                info,
                &format!(
                    "invoke_kind={} builder_class_id={} value_compact_is_narrow={}",
                    info.invoke_kind, b.class_id, b.value_compact_is_narrow
                ),
            );
            return;
        }
        // `append(String)` reads the argument's `value` / `coder` through the
        // OUTER (String) layout; without a coder there is nothing to test.
        if kind == K::SbAppendString && !layout.has_coder {
            self.note_string_prefix_declined(id, info, "String layout has no coder");
            return;
        }
        let zgc_base =
            if kind == K::SbAppendChar && crate::x64::zgc_read_barrier_blocks_inline_fields() {
                crate::x64::zgc_colored_inline_field_load_base()
            } else {
                None
            };
        let zgc_blocked = match kind {
            // `length` / shrinking `setLength` only read/write `int count` (no Ref field).
            K::SbLength | K::SbSetLength => cratonvm_types::narrow_oops_enabled(),
            K::SbAppendChar => {
                cratonvm_types::narrow_oops_enabled()
                    || (crate::x64::zgc_read_barrier_blocks_inline_fields() && zgc_base.is_none())
            }
            _ => crate::x64::narrow_oops_block_inline_fields(),
        };
        if zgc_blocked {
            self.note_string_prefix_declined(id, info, "narrow oops / armed ZGC barrier");
            return;
        }
        let graph = self.graph;
        let Some(node) = graph.nodes.get(id as usize) else {
            return;
        };
        let want_ty = if matches!(
            kind,
            K::SbAppendChar | K::SbAppendString | K::SbAppendInt | K::SbAppendCodePoint
        ) {
            IrType::Ref
        } else if kind == K::SbSetLength {
            IrType::Void
        } else {
            IrType::Int
        };
        if node.inputs.len() != 3 + kind.num_params() || node.ty != want_ty {
            self.note_string_prefix_declined(
                id,
                info,
                &format!("node shape: {} inputs, ty {:?}", node.inputs.len(), node.ty),
            );
            return;
        }
        let recv = node.inputs[2];
        let arg = if kind.num_params() == 1 {
            Some(node.inputs[3])
        } else {
            None
        };
        if self.carry_names(recv) || arg.is_some_and(|a| self.carry_names(a)) {
            self.note_string_prefix_declined(id, info, "an operand is a live carry");
            return;
        }
        // The argument of the one-parameter kinds (the input-count screen
        // above guarantees it exists for them); never read for `length()`.
        let arg_id: NodeId = arg.unwrap_or(recv);
        let Some(slot) = self.prefix_result_slot(id, want_ty) else {
            self.note_string_prefix_declined(id, info, "result carried, routed, or unhomed");
            return;
        };
        if crate::ir_stage_reporting() {
            eprintln!(
                "[ir] string-builder-prefix n{id} {}.{}{} kind={kind:?} builder_class_id={}",
                info.class_name, info.method_name, info.descriptor, b.class_id,
            );
        }
        let exact_fresh_sb = matches!(
            graph.nodes.get(recv as usize).map(|n| &n.op),
            Some(&Op::New { class_id, .. }) if class_id == b.class_id
        );
        let cur_block = self.schedule.node_to_block.get(id as usize).copied();
        let layout_proven = cur_block
            .is_some_and(|b_idx| self.field_proof_holds(&self.layout_proven, recv, b_idx, false))
            || self.block_cse_layout_epoch_checked();
        let fresh_compact_sb = matches!(
            graph.nodes.get(recv as usize).map(|n| &n.op),
            Some(&Op::New { class_id, num_fields })
                if class_id == b.class_id
                    && cratonvm_types::compact_tlab_total_size(class_id, num_fields).is_some()
        );
        let sb_compact_proven = fresh_compact_sb
            || (layout_proven
                && cur_block.is_some_and(|b_idx| {
                    self.field_proof_holds(&self.compact_proven, recv, b_idx, true)
                }));
        let mut decline: Vec<usize> = Vec::new();
        self.note_rcx_written();
        if !layout_proven {
            if let Some(p) = self.emit_layout_epoch_guard() {
                decline.push(p);
            }
        }
        if kind == K::SbAppendChar && self.zgc_fused_gate.is_some() && zgc_base.is_none() {
            self.emit_zgc_unarmed_check(&mut decline);
        }
        // RAX = receiver; null declines (the call raises the NPE).
        self.gp_load_value(RAX, recv);
        if !exact_fresh_sb {
            self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
            decline.push(self.emit_jcc_rel32(0x84)); // JZ
            self.emit_str_cmp_class_id(RAX, b.class_id);
            decline.push(self.emit_jcc_rel32(0x85)); // JNE
        }
        match kind {
            K::SbLength => {
                if sb_compact_proven {
                    self.emit_load_base_disp32(RAX, RAX, b.count_compact_offset, false);
                } else {
                    self.emit_str_test_mem8(
                        RAX,
                        cratonvm_types::GC_FLAGS_BYTE_OFFSET as u8, // Cast: x86-64 disp8
                        cratonvm_types::GC_FLAG_COMPACT,
                    );
                    let legacy = self.emit_jcc_rel32(0x84); // JZ legacy
                    self.emit_load_base_disp32(RAX, RAX, b.count_compact_offset, false);
                    let have = self.emit_jmp_rel32();
                    self.patch_rel32_to_here(legacy);
                    self.emit_load_base_disp32(RAX, RAX, b.count_legacy_offset, false);
                    self.patch_rel32_to_here(have);
                }
                self.buf.emit(&[0x48, 0x63, 0xC0]); // MOVSXD RAX, EAX
            }
            K::SbCharAt | K::SbAppendChar => {
                // R10 = the index (zero-extended for SIB addressing) or the char.
                self.gp_load_value(R10, arg_id);
                if kind == K::SbCharAt {
                    self.buf.emit(&[0x45, 0x89, 0xD2]); // MOV R10D, R10D
                }
                if kind == K::SbAppendChar && !int_expr_in_latin1_range(self.graph, arg_id, 0) {
                    // LATIN1-encodable chars only (`StringLatin1.canEncode`).
                    self.buf.emit(&[0x41, 0x81, 0xFA, 0xFF, 0x00, 0x00, 0x00]); // CMP R10D, 0xFF
                    decline.push(self.emit_jcc_rel32(0x87)); // JA
                }
                // EDX = coder, R11D = count, RCX = value, per layout.
                self.emit_sb_read_fields(&b, sb_compact_proven);
                self.emit_sb_decode_value_word_if_zgc(zgc_base, &mut decline);
                self.emit_str_test_r64(RCX);
                decline.push(self.emit_jcc_rel32(0x84)); // JZ: no payload yet
                if kind == K::SbCharAt {
                    self.buf.emit(&[0x45, 0x39, 0xDA]); // CMP R10D, R11D (index vs count)
                    decline.push(self.emit_jcc_rel32(0x83)); // JAE
                    self.emit_load_base_disp32(R11, RCX, 8, false); // R11D = value.length
                    self.buf.emit(&[0x85, 0xD2]); // TEST EDX, EDX
                    let utf16 = self.emit_jcc_rel32(0x85); // JNZ
                    self.buf.emit(&[0x45, 0x39, 0xDA]); // CMP R10D, R11D (capacity)
                    decline.push(self.emit_jcc_rel32(0x83)); // JAE
                    self.buf.emit(&[0x42, 0x0F, 0xB6, 0x44, 0x11, 0x10]); // MOVZX EAX, BYTE [RCX+R10+16]
                    let done = self.emit_jmp_rel32();
                    self.patch_rel32_to_here(utf16);
                    self.buf.emit(&[0x41, 0xD1, 0xEB]); // SHR R11D, 1
                    self.buf.emit(&[0x45, 0x39, 0xDA]); // CMP R10D, R11D (capacity)
                    decline.push(self.emit_jcc_rel32(0x83)); // JAE
                    self.buf.emit(&[0x42, 0x0F, 0xB7, 0x44, 0x51, 0x10]); // MOVZX EAX, WORD [RCX+R10*2+16]
                    self.patch_rel32_to_here(done);
                } else {
                    // append(char): LATIN1 builder with room.
                    self.buf.emit(&[0x85, 0xD2]); // TEST EDX, EDX
                    decline.push(self.emit_jcc_rel32(0x85)); // JNZ: UTF16
                    self.buf.emit(&[0x44, 0x3B, 0x59, 0x08]); // CMP R11D, [RCX+8]
                    decline.push(self.emit_jcc_rel32(0x83)); // JAE: full (or a corrupt count)
                    self.buf.emit(&[0x46, 0x88, 0x54, 0x19, 0x10]); // MOV BYTE [RCX+R11+16], R10B
                    self.buf.emit(&[0x41, 0xFF, 0xC3]); // INC R11D
                                                        // `count = count + 1`, at the receiver's own layout.
                    self.emit_sb_write_count(R11, &b, sb_compact_proven);
                    // `append` returns its receiver, still in RAX.
                }
            }
            K::SbAppendString => {
                // nio20 wave 5, lane L5 (W4-L5-2): `AbstractStringBuilder
                // .append(String)` for a non-null LATIN1 String into a LATIN1
                // builder whose array already holds `count + len` bytes
                // (`ensureCapacityInternal` a no-op, `putStringAt` the
                // same-coder byte copy, `count += len`); every other shape
                // (null -> "null", either side UTF16, a growth) declines to the
                // call BEFORE anything is stored.
                //
                // R10 = the String, read before RCX / RDX / R11 are written (the
                // allocator never homes a value in them: they are scratch).
                self.gp_load_value(R10, arg_id);
                self.emit_str_test_r64(R10);
                decline.push(self.emit_jcc_rel32(0x84)); // JZ: append(null)
                                                         // EDX = builder coder, R11D = count, RCX = builder value.
                self.emit_sb_read_fields(&b, sb_compact_proven);
                self.emit_str_test_r64(RCX);
                decline.push(self.emit_jcc_rel32(0x84)); // JZ: no payload yet
                self.buf.emit(&[0x85, 0xD2]); // TEST EDX, EDX
                decline.push(self.emit_jcc_rel32(0x85)); // JNZ: UTF16 builder
                                                         // RDX = the String's value, EAX = its coder (the receiver is
                                                         // re-read below).
                self.emit_str_value_and_coder(&layout, R10, RDX, Some(RAX));
                self.buf.emit(&[0x85, 0xC0]); // TEST EAX, EAX
                decline.push(self.emit_jcc_rel32(0x85)); // JNZ: UTF16 String
                self.emit_str_test_r64(RDX);
                decline.push(self.emit_jcc_rel32(0x84)); // JZ: no value
                                                         // R10D = len (LATIN1: value.length); EAX = count + len, which
                                                         // must fit the builder's array (unsigned: both are < 2^31).
                self.emit_load_base_disp32(R10, RDX, 8, false);
                self.buf.emit(&[0x44, 0x89, 0xD8]); // MOV EAX, R11D
                self.buf.emit(&[0x44, 0x01, 0xD0]); // ADD EAX, R10D
                self.buf.emit(&[0x3B, 0x41, 0x08]); // CMP EAX, [RCX+8]
                decline.push(self.emit_jcc_rel32(0x87)); // JA: needs growth
                                                         // No decline past this point. RCX = &value[count] - 16 (an
                                                         // interior pointer, held only inside this no-safepoint
                                                         // sequence), R11D = the new count.
                self.buf.emit(&[0x4A, 0x8D, 0x0C, 0x19]); // LEA RCX, [RCX+R11]
                self.buf.emit(&[0x41, 0x89, 0xC3]); // MOV R11D, EAX
                if crate::ir_sb_append_string_sse2_enabled() {
                    // JIT r22 wave 2, lane intrinsics (NIO20W5-L5-3,
                    // `CRATONVM_JIT_IR_SB_APPEND_STRING_SSE2`, opt-in): 16
                    // bytes per step from the end while 16 remain, the byte
                    // loop below finishing the head. A String's `value` is
                    // never the builder's array, so the order of the copy is
                    // not observable (nothing reads the new bytes before
                    // `count` is stored). XMM0 is the lowerer's per-op scratch.
                    let blk = self.buf.pos();
                    self.buf.emit(&[0x41, 0x83, 0xFA, 0x10]); // CMP R10D, 16
                    let to_bytes = self.emit_jcc_rel32(0x82); // JB
                    self.buf.emit(&[0x41, 0x83, 0xEA, 0x10]); // SUB R10D, 16
                    self.buf.emit(&[0xF3, 0x42, 0x0F, 0x6F, 0x44, 0x12, 0x10]); // MOVDQU XMM0, [RDX+R10+16]
                    self.buf.emit(&[0xF3, 0x42, 0x0F, 0x7F, 0x44, 0x11, 0x10]); // MOVDQU [RCX+R10+16], XMM0
                    self.emit_jmp_back_to(blk);
                    self.patch_rel32_to_here(to_bytes);
                }
                self.buf.emit(&[0x45, 0x85, 0xD2]); // TEST R10D, R10D
                let empty = self.emit_jcc_rel32(0x84); // JZ: append("")
                let top = self.buf.pos();
                self.buf.emit(&[0x41, 0xFF, 0xCA]); // DEC R10D
                self.buf.emit(&[0x42, 0x0F, 0xB6, 0x44, 0x12, 0x10]); // MOVZX EAX, BYTE [RDX+R10+16]
                self.buf.emit(&[0x42, 0x88, 0x44, 0x11, 0x10]); // MOV BYTE [RCX+R10+16], AL
                self.buf.emit(&[0x45, 0x85, 0xD2]); // TEST R10D, R10D
                self.emit_str_jcc_back(0x85, top); // JNZ top
                self.patch_rel32_to_here(empty);
                // `count = count + len`, at the receiver's own layout; the
                // receiver is the result.
                self.gp_load_value(RAX, recv);
                self.emit_sb_write_count(R11, &b, sb_compact_proven);
            }
            K::SbAppendInt => {
                // JIT r20 wave 18, lane xSb (STR20-2,
                // `CRATONVM_JIT_IR_SB_APPEND_INT_PREFIX`):
                // `AbstractStringBuilder.append(int)` for a LATIN1 builder whose
                // array already holds `count + stringSize(i)` bytes -- the
                // digits in place, `count = spaceNeeded`, the receiver as the
                // result. A UTF16 builder (the JDK's `uncheckedGetCharsUTF16`
                // arm), a growth (`ensureCapacitySameCoder`'s `newCapacity`
                // policy) and a missing payload decline to the call BEFORE
                // anything is stored.
                //
                // R10 = i, read before RCX / RDX / R11 are written (scratch: the
                // allocator never homes a value in them).
                const R9: u8 = 9;
                self.gp_load_value(R10, arg_id);
                // EDX = builder coder, R11D = count, RCX = builder value.
                self.emit_sb_read_fields(&b, sb_compact_proven);
                self.emit_str_test_r64(RCX);
                decline.push(self.emit_jcc_rel32(0x84)); // JZ: no payload yet
                self.buf.emit(&[0x85, 0xD2]); // TEST EDX, EDX
                decline.push(self.emit_jcc_rel32(0x85)); // JNZ: UTF16 builder

                // The room test (its decline precedes every store), then the
                // digits; R9D = the new count.
                crate::x64::concat_emit::emit_sb_append_int_latin1(&mut self.buf, &mut decline);
                // `count = spaceNeeded`, at the receiver's own layout; the
                // receiver (re-read: no GC point since) is the result.
                self.gp_load_value(RAX, recv);
                self.emit_sb_write_count(R9, &b, sb_compact_proven);
            }
            K::SbCodePointAt => {
                // JIT r23 wave 2, lane L5 (R23W1L5-3): JDK 25
                // `AbstractStringBuilder.codePointAt` -- `checkIndex(index,
                // count)`; LATIN1 `value[index] & 0xff`; UTF16
                // `StringUTF16.codePointAtSB(value, index, count)`: c1, and when
                // it is a high surrogate with `index + 1 < count`, c2
                // (`checkIndex` against the payload first), the pair when c2 is
                // a low surrogate. Every index outside the count or the
                // payload, a coder other than 0 / 1 and no payload decline.
                self.gp_load_value(R10, arg_id);
                self.buf.emit(&[0x45, 0x89, 0xD2]); // MOV R10D, R10D
                                                    // EDX = coder, R11D = count, RCX = value, per layout.
                self.emit_sb_read_fields(&b, sb_compact_proven);
                self.emit_str_test_r64(RCX);
                decline.push(self.emit_jcc_rel32(0x84)); // JZ: no payload yet
                crate::x64::string_code_point::emit_sb_code_point_at_tail(
                    &mut self.buf,
                    &mut decline,
                );
            }
            K::SbAppendCodePoint => {
                // JIT r20 wave 19, lane zCp
                // (`CRATONVM_JIT_IR_SB_APPEND_CODE_POINT_PREFIX`): JDK 25's
                // `AbstractStringBuilder.appendCodePoint` where nothing grows
                // and nothing inflates --
                //
                // * LATIN1 builder, `cp` in `0..=0xFF`, `count < value.length`:
                //   `append((char) cp)`'s LATIN1 arm, `value[count++] = (byte) cp`;
                // * UTF16 builder, BMP `cp` (`cp >>> 16 == 0`, surrogate code
                //   units included, as `append(char)` stores them), `count <
                //   value.length >> 1`: `StringUTF16.putChar(value, count++, cp)`;
                // * UTF16 builder, supplementary `cp` (`cp - 0x10000` unsigned
                //   below `0x100000`), `count + 2 <= value.length >> 1`:
                //   `append(Character.toChars(cp))` -- `ensureCapacitySameCoder`
                //   a no-op, `putCharsSB` of the high then the low surrogate;
                //
                // then `count` advanced and the receiver returned. Everything
                // else -- a LATIN1 builder given a wider code point (the
                // inflation), a full payload (the growth), an invalid code
                // point (`toChars` throws `IllegalArgumentException`), a coder
                // other than 0 / 1, no payload -- declines to the call BEFORE
                // any store. Neither arm touches `maybeLatin1` (the JDK's
                // append paths do not). Plain stores into a `byte[]` and an
                // `int` field: no reference, no barrier, no allocation, no GC
                // point.
                const R9: u8 = 9;
                // R10D = the code point, read before RCX / RDX / R11 are
                // written (scratch: the allocator never homes a value in them).
                self.gp_load_value(R10, arg_id);
                // EDX = builder coder, R11D = count, RCX = builder value.
                self.emit_sb_read_fields(&b, sb_compact_proven);
                self.emit_str_test_r64(RCX);
                decline.push(self.emit_jcc_rel32(0x84)); // JZ: no payload yet
                self.buf.emit(&[0x85, 0xD2]); // TEST EDX, EDX
                let utf16 = self.emit_jcc_rel32(0x85); // JNZ
                                                       // ---- LATIN1 ----
                self.buf.emit(&[0x41, 0x81, 0xFA, 0xFF, 0x00, 0x00, 0x00]); // CMP R10D, 0xFF
                decline.push(self.emit_jcc_rel32(0x87)); // JA: inflation (or invalid)
                self.buf.emit(&[0x44, 0x3B, 0x59, 0x08]); // CMP R11D, [RCX+8]
                decline.push(self.emit_jcc_rel32(0x83)); // JAE: full (or a corrupt count)
                self.buf.emit(&[0x46, 0x88, 0x54, 0x19, 0x10]); // MOV BYTE [RCX+R11+16], R10B
                self.buf.emit(&[0x41, 0xFF, 0xC3]); // INC R11D
                let latin1_done = self.emit_jmp_rel32();
                // ---- UTF16 ----
                self.patch_rel32_to_here(utf16);
                self.buf.emit(&[0x83, 0xFA, 0x01]); // CMP EDX, 1
                decline.push(self.emit_jcc_rel32(0x85)); // JNE: no such coder
                                                         // EDX = the capacity in chars, `value.length >> 1`.
                self.emit_load_base_disp32(RDX, RCX, 8, false);
                self.buf.emit(&[0xD1, 0xEA]); // SHR EDX, 1
                self.buf.emit(&[0x45, 0x89, 0xD1]); // MOV R9D, R10D
                self.buf.emit(&[0x41, 0xC1, 0xE9, 0x10]); // SHR R9D, 16
                let supplementary = self.emit_jcc_rel32(0x85); // JNZ
                                                               // BMP: one char at `count`.
                self.buf.emit(&[0x41, 0x39, 0xD3]); // CMP R11D, EDX
                decline.push(self.emit_jcc_rel32(0x83)); // JAE: full
                self.buf.emit(&[0x66, 0x46, 0x89, 0x54, 0x59, 0x10]); // MOV WORD [RCX+R11*2+16], R10W
                self.buf.emit(&[0x41, 0xFF, 0xC3]); // INC R11D
                let bmp_done = self.emit_jmp_rel32();
                // Supplementary: valid only below 0x110000.
                self.patch_rel32_to_here(supplementary);
                self.buf.emit(&[0x45, 0x8D, 0x8A]); // LEA R9D, [R10 + disp32]
                self.buf.emit(&(-0x1_0000i32).to_le_bytes());
                self.buf.emit(&[0x41, 0x81, 0xF9]); // CMP R9D, imm32
                self.buf.emit(&0x10_0000i32.to_le_bytes());
                decline.push(self.emit_jcc_rel32(0x83)); // JAE: invalid, the call throws
                                                         // JIT r23 wave 1, lane L5 (`CRATONVM_JIT_IR_SB_APPEND_CODE_POINT_COUNT_GUARD`,
                                                         // correctness, default ON): `count < capacity` (unsigned) first,
                                                         // as the BMP arm checks it. Without it a negative `count`
                                                         // (reflection, `Unsafe`) made `count + 2` small and passed the
                                                         // room test below, and the two stores then addressed
                                                         // `value + 2 * (u32) count` -- outside the array.
                if crate::ir_sb_append_code_point_count_guard_enabled() {
                    self.buf.emit(&[0x41, 0x39, 0xD3]); // CMP R11D, EDX
                    decline.push(self.emit_jcc_rel32(0x83)); // JAE
                }
                self.buf.emit(&[0x45, 0x8D, 0x4B, 0x02]); // LEA R9D, [R11 + 2]
                self.buf.emit(&[0x41, 0x39, 0xD1]); // CMP R9D, EDX
                decline.push(self.emit_jcc_rel32(0x87)); // JA: needs growth
                                                         // No decline past this point. High surrogate at `count`:
                                                         // `(cp >>> 10) + (0xD800 - (0x10000 >>> 10))`.
                self.buf.emit(&[0x45, 0x89, 0xD1]); // MOV R9D, R10D
                self.buf.emit(&[0x41, 0xC1, 0xE9, 0x0A]); // SHR R9D, 10
                self.buf.emit(&[0x41, 0x81, 0xC1]); // ADD R9D, imm32
                self.buf
                    .emit(&crate::x64::concat_emit::HIGH_SURROGATE_BIAS.to_le_bytes());
                self.buf.emit(&[0x66, 0x46, 0x89, 0x4C, 0x59, 0x10]); // MOV WORD [RCX+R11*2+16], R9W
                                                                      // Low surrogate at `count + 1`: `(cp & 0x3FF) + 0xDC00`.
                self.buf.emit(&[0x45, 0x89, 0xD1]); // MOV R9D, R10D
                self.buf.emit(&[0x41, 0x81, 0xE1, 0xFF, 0x03, 0x00, 0x00]); // AND R9D, 0x3FF
                self.buf.emit(&[0x41, 0x81, 0xC1]); // ADD R9D, imm32
                self.buf
                    .emit(&crate::x64::concat_emit::LOW_SURROGATE_BASE.to_le_bytes());
                self.buf.emit(&[0x66, 0x46, 0x89, 0x4C, 0x59, 0x12]); // MOV WORD [RCX+R11*2+18], R9W
                self.buf.emit(&[0x41, 0x83, 0xC3, 0x02]); // ADD R11D, 2
                self.patch_rel32_to_here(latin1_done);
                self.patch_rel32_to_here(bmp_done);
                // `count = R11D`, at the receiver's own layout (RAX is still
                // the receiver, and the result).
                self.emit_sb_write_count(R11, &b, sb_compact_proven);
            }
            K::SbSetLength => {
                // `0 <= newLength <= count`: shrinks `count = newLength` with
                // zero allocations, zero zero-fill, and no Ref loads.
                self.gp_load_value(R10, arg_id);
                self.buf.emit(&[0x45, 0x85, 0xD2]); // TEST R10D, R10D
                decline.push(self.emit_jcc_rel32(0x88)); // JS (negative -> SIOOBE)
                if sb_compact_proven {
                    self.emit_load_base_disp32(R11, RAX, b.count_compact_offset, false);
                    self.buf.emit(&[0x45, 0x39, 0xDA]); // CMP R10D, R11D
                    decline.push(self.emit_jcc_rel32(0x87)); // JA (newLength > count)
                    self.emit_str_store32_base_disp32(R10, RAX, b.count_compact_offset);
                } else {
                    self.emit_str_test_mem8(
                        RAX,
                        cratonvm_types::GC_FLAGS_BYTE_OFFSET as u8, // Cast: x86-64 disp8
                        cratonvm_types::GC_FLAG_COMPACT,
                    );
                    let legacy = self.emit_jcc_rel32(0x84); // JZ legacy
                    self.emit_load_base_disp32(R11, RAX, b.count_compact_offset, false);
                    self.buf.emit(&[0x45, 0x39, 0xDA]); // CMP R10D, R11D
                    decline.push(self.emit_jcc_rel32(0x87)); // JA (newLength > count)
                    self.emit_str_store32_base_disp32(R10, RAX, b.count_compact_offset);
                    let stored = self.emit_jmp_rel32();
                    self.patch_rel32_to_here(legacy);
                    self.emit_load_base_disp32(R11, RAX, b.count_legacy_offset, false);
                    self.buf.emit(&[0x45, 0x39, 0xDA]); // CMP R10D, R11D
                    decline.push(self.emit_jcc_rel32(0x87)); // JA (newLength > count)
                    self.emit_str_store32_base_disp32(R10, RAX, b.count_legacy_offset);
                    self.patch_rel32_to_here(stored);
                }
            }
            // Screened above (`is_builder`); a String kind never reaches here,
            // and `SbToString` is routed to its own emitter before this one.
            _ => {}
        }
        if want_ty != IrType::Void {
            self.emit_prefix_result_store(id, slot);
        }
        let hit = self.emit_jmp_rel32();
        for p in decline {
            self.patch_rel32_to_here(p);
        }
        self.call_prefix_join = Some(hit);
    }

    /// Resolve the destination slot for an inline call prefix (`None` when the
    /// result is `Void`, written solely to an `assigned_gpr`, or is an unused
    /// reference whose frame home was dropped).
    pub(in crate::ir_lower) fn prefix_result_slot(
        &self,
        id: NodeId,
        want_ty: IrType,
    ) -> Option<Option<i32>> {
        if want_ty == IrType::Void {
            return Some(None);
        }
        let node = self.graph.nodes.get(id as usize)?;
        if self.carry_of.get(id as usize).copied().flatten().is_some() {
            return None;
        }
        let home_dropped = self.home_dropped.get(id as usize).copied().unwrap_or(false);
        if home_dropped {
            let site_routed_drop =
                crate::ir_lower::r25_w7a_reg_alloc::routed_result(&node.op, node.ty)
                    && self.site_named.get(id as usize).copied().unwrap_or(false);
            if site_routed_drop {
                return None;
            }
            let dead_ref_drop = want_ty == IrType::Ref
                && self.assigned_gpr(id).is_none()
                && self.use_count.get(id as usize).copied() == Some(0)
                && !self
                    .deopt_named_reachable
                    .get(id as usize)
                    .copied()
                    .unwrap_or(true);
            if self.assigned_gpr(id).is_none() && !dead_ref_drop {
                return None;
            }
            return Some(None);
        }
        self.planned_slot_off(id).ok().map(Some)
    }

    /// Publish `RAX` into `id`'s assigned GPR (if any) and optional frame home.
    pub(in crate::ir_lower) fn emit_prefix_result_store(&mut self, id: NodeId, slot: Option<i32>) {
        if let Some(reg) = self.assigned_gpr(id) {
            self.emit_mov_reg_reg64(reg, RAX);
        }
        if let Some(slot) = slot {
            let mut bytes = FrameAccess::new();
            enc_frame_store(RAX, slot, &mut bytes);
            self.buf.emit(bytes.as_slice());
        }
    }

    /// nio20 wave 6, lane L2 (`heapcb`, NIO19-8's last residual):
    /// `String.getChars(int srcBegin, int srcEnd, char[] dst, int dstBegin)`
    /// answered inline as a guarded PREFIX of the `Op::Call`, for a LATIN1
    /// receiver -- the whole JDK body (`checkBoundsBeginEnd`,
    /// `checkBoundsOffCount`, `StringLatin1.getChars` -> `StringLatin1.inflate`,
    /// a registered native reached through the lean native door) as one
    /// bounds screen and a byte -> char copy. No tier had an intrinsic for it,
    /// so `HeapCharBuffer.put(String,int,int)` paid two door crossings per op
    /// on either tier.
    ///
    /// Same contract with the `Op::Call` arm as
    /// [`Self::try_emit_ir_string_search_prefix`]: scratch RAX, RCX, RDX, R10,
    /// R11 (and XMM0/XMM1, the lowerer's per-op FP scratch, for the 8-char
    /// SSE2 step under `CRATONVM_JIT_IR_STRING_SSE2`); the method is `void`,
    /// so the hit edge stores nothing and jumps to [`Self::call_prefix_join`];
    /// no call, no GC point, no deopt on the hit path.
    ///
    /// # Declines (to the call, which answers or throws exactly as before)
    ///
    /// A replaced layout, a null receiver or one whose header is not
    /// `java/lang/String`, a null `value`, a UTF16 coder, `srcEnd` outside
    /// `[0, length]` or `srcBegin` outside `[0, srcEnd]` (unsigned compares,
    /// so negatives decline), a null `dst` or one whose header is not a
    /// `char[]`, `dstBegin` outside `[0, dst.length]`, and `srcEnd - srcBegin
    /// > dst.length - dstBegin`. Those are exactly the JDK body's throwing
    /// conditions (SIOOBE for the source bounds, NPE for `dst`, SIOOBE for the
    /// destination bounds), plus the UTF16 arm, so every throwing input
    /// reaches the call with nothing stored. Past the last decline the copy
    /// cannot fail.
    ///
    /// The stores are `char`s into a primitive array: no reference, no
    /// barrier. `value` and `dst` are distinct arrays (a `byte[]` and a
    /// `char[]`), so the copy cannot overlap. Like the other String prefixes
    /// the loop polls no safepoint; a multi-megabyte `getChars` delays one by
    /// its own length, as the native it replaces did.
    pub(in crate::ir_lower) fn try_emit_ir_string_getchars_prefix(
        &mut self,
        id: NodeId,
        info: &JitInvokeInfo,
        layout: crate::StringFieldLayout,
    ) {
        use crate::IrStringSearchKind as K;
        if !K::GetChars.admits_invoke_kind(info.invoke_kind)
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
        let Some(char_array_tag) = cratonvm_types::primitive_array_kind_tags_byte("[C") else {
            return;
        };
        let graph = self.graph;
        let Some(node) = graph.nodes.get(id as usize) else {
            return;
        };
        // [ctrl, mem, receiver, srcBegin, srcEnd, dst, dstBegin], void.
        if node.inputs.len() != 3 + K::GetChars.num_params() || node.ty != IrType::Void {
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
        let recv = node.inputs[2];
        let src_begin = node.inputs[3];
        let src_end = node.inputs[4];
        let dst = node.inputs[5];
        let dst_begin = node.inputs[6];
        if crate::ir_stage_reporting() {
            eprintln!(
                "[ir] string-getchars-prefix n{id} {}.{}{} string_class_id={}",
                info.class_name, info.method_name, info.descriptor, layout.string_class_id,
            );
        }
        // `[arr + 8]` is the array length and `[arr + 16 + i]` element byte
        // `i`: the two displacements below. The search prefix above asserts
        // the length offset at compile time; the payload offset is asserted
        // here.
        const _: () = assert!(
            cratonvm_types::ARRAY_DATA_OFFSET == 16,
            "the getChars prefix bakes ARRAY_DATA_OFFSET 16"
        );
        const KIND_TAGS: u8 = cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8; // Cast: x86-64 disp8
        const _: () = assert!(cratonvm_types::KIND_TAGS_BYTE_OFFSET <= 127);
        let mut decline: Vec<usize> = Vec::new();
        // RCX: the epoch guard's fallback form and the `value` load write it.
        self.note_rcx_written();
        if let Some(p) = self.emit_layout_epoch_guard() {
            decline.push(p);
        }
        // RAX = receiver; null declines (the call raises the NPE).
        self.gp_load_value(RAX, recv);
        self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
        decline.push(self.emit_jcc_rel32(0x84)); // JZ
        self.emit_str_cmp_class_id(RAX, layout.string_class_id);
        decline.push(self.emit_jcc_rel32(0x85)); // JNE
                                                 // RCX = value, EDX = coder; a null value or UTF16 declines.
        self.emit_str_value_and_coder(&layout, RAX, RCX, Some(RDX));
        self.emit_str_test_r64(RCX);
        decline.push(self.emit_jcc_rel32(0x84)); // JZ
        self.buf.emit(&[0x85, 0xD2]); // TEST EDX, EDX
        decline.push(self.emit_jcc_rel32(0x85)); // JNZ: UTF16
                                                 // EAX = length() (LATIN1: value.length).
        self.emit_load_base_disp32(RAX, RCX, 8, false);
        // R10D = srcEnd, zero-extended: unsigned compares decline negatives.
        self.gp_load_value(R10, src_end);
        self.buf.emit(&[0x45, 0x89, 0xD2]); // MOV R10D, R10D
        self.buf.emit(&[0x41, 0x39, 0xC2]); // CMP R10D, EAX
        decline.push(self.emit_jcc_rel32(0x87)); // JA: srcEnd > length (or < 0)
                                                 // R11D = srcBegin.
        self.gp_load_value(R11, src_begin);
        self.buf.emit(&[0x45, 0x89, 0xDB]); // MOV R11D, R11D
        self.buf.emit(&[0x45, 0x39, 0xD3]); // CMP R11D, R10D
        decline.push(self.emit_jcc_rel32(0x87)); // JA: srcBegin > srcEnd (or < 0)
                                                 // R10D = count = srcEnd - srcBegin; RCX = &value[srcBegin] - 16 (an
                                                 // interior pointer, held only inside this no-safepoint sequence).
        self.buf.emit(&[0x45, 0x29, 0xDA]); // SUB R10D, R11D
        self.buf.emit(&[0x4A, 0x8D, 0x0C, 0x19]); // LEA RCX, [RCX+R11]
                                                  // RDX = dst: non-null and a `char[]` (the descriptor promises one;
                                                  // the header test is four bytes against trusting it).
        self.gp_load_value(RDX, dst);
        self.emit_str_test_r64(RDX);
        decline.push(self.emit_jcc_rel32(0x84)); // JZ: the call's NPE
        self.buf.emit(&[0x80, 0x7A, KIND_TAGS, char_array_tag]); // CMP BYTE [RDX+KIND_TAGS], [C tag
        decline.push(self.emit_jcc_rel32(0x85)); // JNE
                                                 // EAX = dstBegin, in [0, dst.length].
        self.gp_load_value(RAX, dst_begin);
        self.buf.emit(&[0x89, 0xC0]); // MOV EAX, EAX
        self.buf.emit(&[0x3B, 0x42, 0x08]); // CMP EAX, [RDX+8]
        decline.push(self.emit_jcc_rel32(0x87)); // JA
                                                 // count <= dst.length - dstBegin (both sides in [0, 2^31)).
        self.emit_load_base_disp32(R11, RDX, 8, false); // R11D = dst.length
        self.buf.emit(&[0x41, 0x29, 0xC3]); // SUB R11D, EAX
        self.buf.emit(&[0x45, 0x39, 0xDA]); // CMP R10D, R11D
        decline.push(self.emit_jcc_rel32(0x87)); // JA
                                                 // No decline past this point. RDX = &dst[dstBegin] - 16.
        self.buf.emit(&[0x48, 0x8D, 0x14, 0x42]); // LEA RDX, [RDX+RAX*2]
        self.emit_str_inflate_copy();
        // A call chain the single-pass backend has no inline form for, lowered
        // as straight-line code: acceptance evidence, like a scalar intrinsic
        // (a call-site prefix is otherwise invisible to the gate, and a body
        // whose only gain is this one would be refused and memoised).
        crate::ir_evidence::note(crate::ir_evidence::Transform::ScalarIntrinsic);
        let hit = self.emit_jmp_rel32();
        for p in decline {
            self.patch_rel32_to_here(p);
        }
        self.call_prefix_join = Some(hit);
    }

    /// nio20 wave 6, lane L2: `StringLatin1.inflate(byte[] src, int srcOff,
    /// char[] dst, int dstOff, int len)` -- the `@IntrinsicCandidate` the JDK's
    /// LATIN1 `getChars` / `toChars` / `StringUTF16` paths end in, a native
    /// door crossing in this VM -- answered inline at an `invokestatic` as a
    /// guarded PREFIX of the `Op::Call` (kind
    /// [`crate::IrStringSearchKind::Inflate`]), with the same copy as
    /// [`Self::try_emit_ir_string_getchars_prefix`]. Reached when a caller of
    /// `inflate` (`StringLatin1.getChars`, typically spliced into a compiled
    /// `String.getChars`) is on the optimizing tier.
    ///
    /// The JDK body is an unchecked loop: a non-positive `len` copies nothing
    /// (even with null arrays) and an out-of-range index throws
    /// `ArrayIndexOutOfBoundsException` AFTER the elements before it were
    /// copied. So this answers only the fully in-range case -- non-null
    /// `byte[]` and `char[]` headers, `srcOff + len <= src.length` and
    /// `dstOff + len <= dst.length`, every value non-negative (unsigned
    /// compares) -- and DECLINES every other input to the call, before
    /// anything is stored, which reproduces the partial copy and the throw.
    /// No field offsets are baked, so no layout-epoch guard.
    pub(in crate::ir_lower) fn try_emit_ir_string_inflate_prefix(
        &mut self,
        id: NodeId,
        info: &JitInvokeInfo,
    ) {
        use crate::IrStringSearchKind as K;
        if !K::Inflate.admits_invoke_kind(info.invoke_kind) {
            self.note_string_prefix_declined(
                id,
                info,
                &format!("invoke_kind={}", info.invoke_kind),
            );
            return;
        }
        let (Some(byte_array_tag), Some(char_array_tag)) = (
            cratonvm_types::primitive_array_kind_tags_byte("[B"),
            cratonvm_types::primitive_array_kind_tags_byte("[C"),
        ) else {
            return;
        };
        let graph = self.graph;
        let Some(node) = graph.nodes.get(id as usize) else {
            return;
        };
        // [ctrl, mem, src, srcOff, dst, dstOff, len], void (static: `src`
        // stands where an instance method's receiver would).
        if node.inputs.len() != 3 + K::Inflate.num_params() || node.ty != IrType::Void {
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
        let src = node.inputs[2];
        let src_off = node.inputs[3];
        let dst = node.inputs[4];
        let dst_off = node.inputs[5];
        let len = node.inputs[6];
        if crate::ir_stage_reporting() {
            eprintln!(
                "[ir] string-inflate-prefix n{id} {}.{}{}",
                info.class_name, info.method_name, info.descriptor,
            );
        }
        const _: () = assert!(
            cratonvm_types::ARRAY_DATA_OFFSET == 16,
            "the inflate prefix bakes ARRAY_DATA_OFFSET 16"
        );
        const KIND_TAGS: u8 = cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8; // Cast: x86-64 disp8
        const _: () = assert!(cratonvm_types::KIND_TAGS_BYTE_OFFSET <= 127);
        let mut decline: Vec<usize> = Vec::new();
        // R10D = len, zero-extended: a negative `len` fails the bounds below
        // and declines (the call copies nothing).
        self.gp_load_value(R10, len);
        self.buf.emit(&[0x45, 0x89, 0xD2]); // MOV R10D, R10D
                                            // RCX = src: non-null `byte[]`.
        self.gp_load_value(RCX, src);
        self.emit_str_test_r64(RCX);
        decline.push(self.emit_jcc_rel32(0x84)); // JZ
        self.buf.emit(&[0x80, 0x79, KIND_TAGS, byte_array_tag]); // CMP BYTE [RCX+KIND_TAGS], [B tag
        decline.push(self.emit_jcc_rel32(0x85)); // JNE
                                                 // R11D = srcOff in [0, src.length]; len <= src.length - srcOff.
        self.gp_load_value(R11, src_off);
        self.buf.emit(&[0x45, 0x89, 0xDB]); // MOV R11D, R11D
        self.buf.emit(&[0x44, 0x3B, 0x59, 0x08]); // CMP R11D, [RCX+8]
        decline.push(self.emit_jcc_rel32(0x87)); // JA
        self.emit_load_base_disp32(RAX, RCX, 8, false); // EAX = src.length
        self.buf.emit(&[0x44, 0x29, 0xD8]); // SUB EAX, R11D
        self.buf.emit(&[0x41, 0x39, 0xC2]); // CMP R10D, EAX
        decline.push(self.emit_jcc_rel32(0x87)); // JA
        self.buf.emit(&[0x4A, 0x8D, 0x0C, 0x19]); // LEA RCX, [RCX+R11]
                                                  // RDX = dst: non-null `char[]`.
        self.gp_load_value(RDX, dst);
        self.emit_str_test_r64(RDX);
        decline.push(self.emit_jcc_rel32(0x84)); // JZ
        self.buf.emit(&[0x80, 0x7A, KIND_TAGS, char_array_tag]); // CMP BYTE [RDX+KIND_TAGS], [C tag
        decline.push(self.emit_jcc_rel32(0x85)); // JNE
                                                 // EAX = dstOff in [0, dst.length]; len <= dst.length - dstOff.
        self.gp_load_value(RAX, dst_off);
        self.buf.emit(&[0x89, 0xC0]); // MOV EAX, EAX
        self.buf.emit(&[0x3B, 0x42, 0x08]); // CMP EAX, [RDX+8]
        decline.push(self.emit_jcc_rel32(0x87)); // JA
        self.emit_load_base_disp32(R11, RDX, 8, false); // R11D = dst.length
        self.buf.emit(&[0x41, 0x29, 0xC3]); // SUB R11D, EAX
        self.buf.emit(&[0x45, 0x39, 0xDA]); // CMP R10D, R11D
        decline.push(self.emit_jcc_rel32(0x87)); // JA
                                                 // No decline past this point.
        self.buf.emit(&[0x48, 0x8D, 0x14, 0x42]); // LEA RDX, [RDX+RAX*2]
        self.emit_str_inflate_copy();
        crate::ir_evidence::note(crate::ir_evidence::Transform::ScalarIntrinsic);
        let hit = self.emit_jmp_rel32();
        for p in decline {
            self.patch_rel32_to_here(p);
        }
        self.call_prefix_join = Some(hit);
    }

    /// JIT r23 wave 2b, lane L5 (INTRINSICS22-5's inline-prefix half,
    /// `CRATONVM_JIT_IR_ARRAYS_EQUALS_PREFIX`): the static integral
    /// `Arrays.equals([X[X)Z` answered inline as a guarded PREFIX of the
    /// `Op::Call`, the JDK 25 body exactly: `a == b` true (two nulls
    /// included); either null false; unequal lengths false; else
    /// `ArraysSupport.mismatch` over `length << shift` bytes (16 per SSE2
    /// step, [`Self::emit_str_byte_compare_loop`]), equal iff none differs --
    /// a byte compare is exact for every integral element type (`boolean[]`
    /// holds 0 / 1). An operand that is not an array of the declared element
    /// type (kind tag) or a byte count past `i32::MAX` declines to the call. No
    /// field offsets are baked; the loop reads only and polls nothing (a leaf
    /// of at most `length` steps, as `vectorizedMismatch`). Scratch: RAX, RCX,
    /// RDX, R10, R11, XMM0/XMM1.
    pub(in crate::ir_lower) fn try_emit_ir_arrays_equals_prefix(
        &mut self,
        id: NodeId,
        info: &JitInvokeInfo,
    ) {
        let (elem, shift): (&str, u8) = match info.descriptor {
            "([B[B)Z" => ("[B", 0),
            "([Z[Z)Z" => ("[Z", 0),
            "([C[C)Z" => ("[C", 1),
            "([S[S)Z" => ("[S", 1),
            "([I[I)Z" => ("[I", 2),
            "([J[J)Z" => ("[J", 3),
            _ => return,
        };
        if info.invoke_kind != 3 {
            self.note_string_prefix_declined(id, info, "not an invokestatic");
            return;
        }
        let Some(tag) = cratonvm_types::primitive_array_kind_tags_byte(elem) else {
            return;
        };
        let graph = self.graph;
        let Some(node) = graph.nodes.get(id as usize) else {
            return;
        };
        // [ctrl, mem, a, a2], `boolean` as `Int`.
        if node.inputs.len() != 4 || node.ty != IrType::Int {
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
        if crate::ir_stage_reporting() {
            eprintln!(
                "[ir] arrays-equals-prefix n{id} {}.{}{}",
                info.class_name, info.method_name, info.descriptor,
            );
        }
        const _: () = assert!(
            cratonvm_types::ARRAY_LENGTH_OFFSET == 8 && cratonvm_types::ARRAY_DATA_OFFSET == 16,
            "the Arrays.equals prefix bakes ARRAY_LENGTH_OFFSET 8 / ARRAY_DATA_OFFSET 16"
        );
        const KIND_TAGS: u8 = cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8; // Cast: x86-64 disp8
        const _: () = assert!(cratonvm_types::KIND_TAGS_BYTE_OFFSET <= 127);
        let (a, a2) = (node.inputs[2], node.inputs[3]);
        let mut decline: Vec<usize> = Vec::new();
        let mut answer_false: Vec<usize> = Vec::new();
        let mut answer_true: Vec<usize> = Vec::new();
        self.note_rcx_written();
        // RCX = a, R11 = a2.
        self.gp_load_value(RCX, a);
        self.gp_load_value(R11, a2);
        self.buf.emit(&[0x4C, 0x39, 0xD9]); // CMP RCX, R11
        answer_true.push(self.emit_jcc_rel32(0x84)); // JE: the same array (or two nulls)
        self.emit_str_test_r64(RCX);
        answer_false.push(self.emit_jcc_rel32(0x84)); // JZ
        self.buf.emit(&[0x4D, 0x85, 0xDB]); // TEST R11, R11
        answer_false.push(self.emit_jcc_rel32(0x84)); // JZ
        self.buf.emit(&[0x80, 0x79, KIND_TAGS, tag]); // CMP BYTE [RCX+KIND_TAGS], tag
        decline.push(self.emit_jcc_rel32(0x85)); // JNE
        self.buf.emit(&[0x41, 0x80, 0x7B, KIND_TAGS, tag]); // CMP BYTE [R11+KIND_TAGS], tag
        decline.push(self.emit_jcc_rel32(0x85)); // JNE
        self.emit_load_base_disp32(RAX, RCX, 8, false); // EAX = a.length
        self.buf.emit(&[0x41, 0x3B, 0x43, 0x08]); // CMP EAX, [R11+8]
        answer_false.push(self.emit_jcc_rel32(0x85)); // JNE: unequal lengths
        if shift > 0 {
            // The byte count must fit the loop's 32-bit index.
            self.buf.emit(&[0x3D]); // CMP EAX, imm32
            self.buf.emit(&(0x7FFF_FFFFi32 >> shift).to_le_bytes());
            decline.push(self.emit_jcc_rel32(0x87)); // JA
            self.buf.emit(&[0xC1, 0xE0, shift]); // SHL EAX, shift
        }
        // No decline past this point.
        let (mismatches, all_equal, byte_mismatch) = self.emit_str_byte_compare_loop(false);
        answer_false.extend(mismatches);
        answer_false.push(byte_mismatch);
        answer_true.push(all_equal);
        for p in answer_false {
            self.patch_rel32_to_here(p);
        }
        self.buf.emit(&[0x31, 0xC0]); // XOR EAX, EAX
        let to_store = self.emit_jmp_rel32();
        for p in answer_true {
            self.patch_rel32_to_here(p);
        }
        self.buf.emit(&[0xB8, 0x01, 0x00, 0x00, 0x00]); // MOV EAX, 1
        self.patch_rel32_to_here(to_store);
        self.emit_prefix_result_store(id, slot);
        crate::ir_evidence::note(crate::ir_evidence::Transform::ScalarIntrinsic);
        let hit = self.emit_jmp_rel32();
        for p in decline {
            self.patch_rel32_to_here(p);
        }
        self.call_prefix_join = Some(hit);
    }

    /// JIT r23 wave 6, lane L5 (`CRATONVM_JIT_IR_INLINE_REF_ARRAYCOPY`, perf,
    /// default ON, `=0` restores the plain call): an inline PREFIX of the
    /// `System.arraycopy(Object, int, Object, int, int)` call that copies a
    /// REFERENCE range itself when the copy needs neither a per-element store
    /// check nor any collector barrier work, and otherwise falls through,
    /// unchanged, to the call (the bound `jit_system_arraycopy_direct`, which
    /// owns every exception, partial copy and barriered store).
    ///
    /// Copies only when, at run time, all of these hold (any miss DECLINES
    /// before a single store):
    /// * the collector has no barrier work for ANY store into `dst`: the SATB
    ///   `pre` byte is zero (no marking window), and the destination array is
    ///   young by the published plan (flags byte under the mask / floor) or the
    ///   `post` byte is zero (no old objects) -- exactly the gates under which
    ///   the gated inline `aastore` ([`Self::emit_gated_ir_aastore`]) stores a
    ///   non-null value with no barrier, applied once for the whole range;
    /// * `src` and `dst` are non-null REFERENCE arrays (the whole kind-tags
    ///   byte equals the reference-array tag; bits 6..7 are reserved zero on
    ///   arrays);
    /// * every element of `src` is storable into `dst` with no check: the
    ///   component class ids are equal, or `dst`'s is 0 (`java/lang/Object`,
    ///   which the inline `aastore`'s exact-component arm also stores into
    ///   without a check);
    /// * `srcPos`, `dstPos`, `len` are non-negative and both ranges fit.
    ///
    /// The copy moves 8-byte words (each one aligned and whole, as
    /// `copy_slots_atomic` moves them) in memmove order: backward when the
    /// destination address is above the source (a same-array overlap; for two
    /// arrays either order is right), forward otherwise. No safepoint, call or
    /// allocation runs between the gates and the last store, so the gates hold
    /// for the whole copy, as they do for the one-store `aastore`.
    ///
    /// Compile-time declines: the switch, a site other than the one triple at
    /// an `invokestatic`, no published store plan (G1 -- the helper keeps its
    /// own G1 arm), narrow oops (a slot is not 8 bytes; the plan gate already
    /// refuses them), an armed ZGC read barrier (the helper declines its raw
    /// copy there too), and the carry shapes every prefix refuses. Scratch: RAX,
    /// RCX, RDX, R10, R11 and the flags.
    pub(in crate::ir_lower) fn try_emit_ir_ref_arraycopy_prefix(
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
        if info.class_name != "java/lang/System"
            || info.method_name != "arraycopy"
            || info.descriptor != "(Ljava/lang/Object;ILjava/lang/Object;II)V"
            || info.invoke_kind != 3
        {
            return;
        }
        // JIT r23 wave 12, lane L6: the node shape is asked FIRST (it was asked
        // after the collector gates), so the primitive arm below can be emitted
        // on a collector with no reference-store plan too. Every check is
        // compile time only; only which decline note prints can change.
        let graph = self.graph;
        let Some(node) = graph.nodes.get(id as usize) else {
            return;
        };
        // [ctrl, mem, src, srcPos, dst, dstPos, len].
        if node.inputs.len() != 7 {
            self.note_string_prefix_declined(
                id,
                info,
                &format!("node shape: {} inputs", node.inputs.len()),
            );
            return;
        }
        if node.inputs[2..].iter().any(|&i| self.carry_names(i)) {
            self.note_string_prefix_declined(id, info, "an operand is a live carry");
            return;
        }
        let (src, src_pos, dst, dst_pos, len) = (
            node.inputs[2],
            node.inputs[3],
            node.inputs[4],
            node.inputs[5],
            node.inputs[6],
        );
        // All-primitive `System.arraycopy` inline fast prefix (`[Z`, `[B`, `[C`,
        // `[S`, `[I`, `[F`, `[J`, `[D`), including self-array overlapping
        // copies. Its one hit jump joins the reference copy's (below) or, when
        // no reference arm is emitted (G1 / ZGC), is this prefix's join.
        let prim_hit = if ir_inline_byte_arraycopy_enabled() {
            self.note_rcx_written();
            self.emit_ir_byte_arraycopy_arm((src, src_pos, dst, dst_pos, len))
        } else {
            None
        };
        let Some((pre, post, floor)) = self.gated_ir_aastore_gates() else {
            self.note_string_prefix_declined(id, info, "no published reference-store plan");
            self.call_prefix_join = prim_hit;
            return;
        };
        if crate::x64::zgc_read_barrier_blocks_inline_fields()
            || cratonvm_types::element_byte_size(cratonvm_types::ArrayElementType::Reference) != 8
        {
            self.note_string_prefix_declined(id, info, "armed ZGC or narrow reference slots");
            self.call_prefix_join = prim_hit;
            return;
        }
        const _: () = assert!(
            cratonvm_types::ARRAY_LENGTH_OFFSET == 8 && cratonvm_types::ARRAY_DATA_OFFSET == 16,
            "the reference arraycopy prefix bakes ARRAY_LENGTH_OFFSET 8 / ARRAY_DATA_OFFSET 16"
        );
        const KIND_TAGS: u8 = cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8; // Cast: x86-64 disp8
        const _: () = assert!(cratonvm_types::KIND_TAGS_BYTE_OFFSET <= 127);
        // Kind `Array` with element type `Reference` (0) in bits 2..5.
        const REF_ARRAY_TAG: u8 = cratonvm_types::ObjectKind::Array as u8; // Cast: a 2-bit kind
        if crate::ir_stage_reporting() {
            eprintln!("[ir] ref-arraycopy-prefix n{id}");
        }
        let mut decline: Vec<usize> = Vec::new();
        self.note_rcx_written();
        // --- The collector: no barrier work for any store into `dst`. ---
        self.gp_load_value(RAX, dst);
        self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
        decline.push(self.emit_jcc_rel32(0x84)); // JZ (the call throws NPE)
        decline.push(self.emit_ir_satb_pre_gate(pre, false));
        let mut post_ok: Vec<usize> = Vec::new();
        self.buf.emit(&[0x0F, 0xB6, 0x88]); // MOVZX ECX, byte [RAX + disp32]
        self.buf
            .emit(&(cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32).to_le_bytes());
        if let Some(mask) = self.ref_store_post_skip_mask {
            self.buf.emit(&[0xF6, 0xC1, mask]); // TEST CL, mask
            post_ok.push(self.emit_jcc_rel32(0x84)); // JZ -> young array
        } else {
            self.emit_mov_reg_imm64(R11, floor as u64);
            self.buf.emit(&[0x41, 0x3A, 0x0B]); // CMP CL, byte [R11]
            post_ok.push(self.emit_jcc_rel32(0x82)); // JB -> young array
        }
        self.emit_mov_reg_imm64(R11, post as u64);
        self.buf.emit(&[0x41, 0x80, 0x3B, 0x00]); // CMP byte [R11], 0
        decline.push(self.emit_jcc_rel32(0x85)); // JNE: old objects exist
        for p in post_ok {
            self.patch_rel32_to_here(p);
        }
        // --- The arrays: non-null reference arrays, no store check. ---
        self.gp_load_value(RAX, src);
        self.gp_load_value(RDX, dst);
        self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
        decline.push(self.emit_jcc_rel32(0x84)); // JZ
        self.buf.emit(&[0x80, 0x78, KIND_TAGS, REF_ARRAY_TAG]); // CMP BYTE [RAX+KIND_TAGS], tag
        decline.push(self.emit_jcc_rel32(0x85)); // JNE
        self.buf.emit(&[0x80, 0x7A, KIND_TAGS, REF_ARRAY_TAG]); // CMP BYTE [RDX+KIND_TAGS], tag
        decline.push(self.emit_jcc_rel32(0x85)); // JNE
        self.buf.emit(&[0x8B, 0x0A]); // MOV ECX, [RDX] (dst component id)
        self.buf.emit(&[0x85, 0xC9]); // TEST ECX, ECX
        let object_dst = self.emit_jcc_rel32(0x84); // JZ: `Object[]` takes anything
        self.buf.emit(&[0x3B, 0x08]); // CMP ECX, [RAX] (src component id)
                                      // JIT r23 wave 11, lane L6 (`CRATONVM_JIT_IR_INLINE_REF_ARRAYCOPY_CHECKED`):
                                      // under the switch a mismatch enters the checked arm
                                      // ([`Self::emit_ir_ref_arraycopy_checked_arm`]) instead of the call.
        let checked_entry = if ir_inline_ref_arraycopy_checked_enabled() {
            Some(self.emit_jcc_rel32(0x85)) // JNE: the checked arm
        } else {
            decline.push(self.emit_jcc_rel32(0x85)); // JNE: a store check would be owed
            None
        };
        self.patch_rel32_to_here(object_dst);
        // --- The ranges (sign-extended `int`s; `length` zero-extended). ---
        self.gp_load_value(R11, len);
        self.buf.emit(&[0x4D, 0x63, 0xDB]); // MOVSXD R11, R11D
        self.buf.emit(&[0x4D, 0x85, 0xDB]); // TEST R11, R11
        decline.push(self.emit_jcc_rel32(0x88)); // JS
        self.gp_load_value(R10, src_pos);
        self.buf.emit(&[0x4D, 0x63, 0xD2]); // MOVSXD R10, R10D
        self.buf.emit(&[0x4D, 0x85, 0xD2]); // TEST R10, R10
        decline.push(self.emit_jcc_rel32(0x88)); // JS
        self.buf.emit(&[0x8B, 0x48, 0x08]); // MOV ECX, [RAX+8] (src.length)
        self.buf.emit(&[0x4C, 0x29, 0xD1]); // SUB RCX, R10
        self.buf.emit(&[0x4C, 0x39, 0xD9]); // CMP RCX, R11
        decline.push(self.emit_jcc_rel32(0x8C)); // JL: srcPos + len > src.length
        self.buf.emit(&[0x4E, 0x8D, 0x54, 0xD0, 0x10]); // LEA R10, [RAX + R10*8 + 16]
        self.gp_load_value(RCX, dst_pos);
        self.buf.emit(&[0x48, 0x63, 0xC9]); // MOVSXD RCX, ECX
        self.buf.emit(&[0x48, 0x85, 0xC9]); // TEST RCX, RCX
        decline.push(self.emit_jcc_rel32(0x88)); // JS
        self.buf.emit(&[0x8B, 0x42, 0x08]); // MOV EAX, [RDX+8] (dst.length)
        self.buf.emit(&[0x48, 0x29, 0xC8]); // SUB RAX, RCX
        self.buf.emit(&[0x4C, 0x39, 0xD8]); // CMP RAX, R11
        decline.push(self.emit_jcc_rel32(0x8C)); // JL: dstPos + len > dst.length
        self.buf.emit(&[0x48, 0x8D, 0x4C, 0xCA, 0x10]); // LEA RCX, [RDX + RCX*8 + 16]
                                                        // No decline past this point. R10 = &src[srcPos], RCX = &dst[dstPos],
                                                        // R11 = len.
        let mut hit: Vec<usize> = Vec::new();
        // JIT r23 wave 12, lane L6: the byte-array arm's hit (a forward jump
        // emitted before this arm) joins the same end.
        hit.extend(prim_hit);
        self.buf.emit(&[0x4D, 0x85, 0xDB]); // TEST R11, R11
        hit.push(self.emit_jcc_rel32(0x84)); // JZ: nothing to copy
        self.emit_ir_ref_copy_words_unrolled(&mut hit);
        if let Some(entry) = checked_entry {
            self.emit_ir_ref_arraycopy_checked_arm(
                entry,
                (src_pos, dst_pos, len),
                &mut hit,
                &mut decline,
            );
        }
        for p in hit {
            self.patch_rel32_to_here(p);
        }
        crate::ir_evidence::note(crate::ir_evidence::Transform::ScalarIntrinsic);
        let done = self.emit_jmp_rel32();
        for p in decline {
            self.patch_rel32_to_here(p);
        }
        self.call_prefix_join = Some(done);
    }

    /// Inline fast path for `ScopedMemoryAccess.copyMemoryInternal` and
    /// `Unsafe.copyMemory0` when both bases are null (off-heap -> off-heap),
    /// sessions are null, addresses are untagged real pointers, `0 <= bytes <= 4096`,
    /// and either `dst <= src` or `dst >= src + bytes` (forward-copy safe).
    pub(in crate::ir_lower) fn try_emit_ir_off_heap_copy_prefix(&mut self, id: NodeId, info_ptr: usize) {
        if info_ptr == 0 {
            return;
        }
        // SAFETY: the planner keeps each nonzero call row alive through lowering.
        let info: &JitInvokeInfo = unsafe { &*(info_ptr as *const JitInvokeInfo) };
        let is_scoped = info.class_name == "jdk/internal/misc/ScopedMemoryAccess"
            && info.method_name == "copyMemoryInternal"
            && info.descriptor == "(Ljdk/internal/foreign/MemorySessionImpl;Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JLjava/lang/Object;JJ)V"
            && self.direct_helpers.scoped_memory_copy_memory != 0;
        let is_unsafe = info.class_name == "jdk/internal/misc/Unsafe"
            && info.method_name == "copyMemory0"
            && info.descriptor == "(Ljava/lang/Object;JLjava/lang/Object;JJ)V"
            && self.direct_helpers.unsafe_copy_memory != 0;
        if !is_scoped && !is_unsafe {
            return;
        }
        let graph = self.graph;
        let Some(node) = graph.nodes.get(id as usize) else {
            return;
        };
        let expected_inputs = if is_scoped { 10 } else { 8 };
        if node.inputs.len() != expected_inputs {
            return;
        }
        if node.inputs[2..].iter().any(|&i| self.carry_names(i)) {
            return;
        }
        self.note_rcx_written();
        let recv = node.inputs[2];
        let (src_sess, dst_sess, src_base, src_off, dst_base, dst_off, bytes) = if is_scoped {
            (
                Some(node.inputs[3]),
                Some(node.inputs[4]),
                node.inputs[5],
                node.inputs[6],
                node.inputs[7],
                node.inputs[8],
                node.inputs[9],
            )
        } else {
            (
                None,
                None,
                node.inputs[3],
                node.inputs[4],
                node.inputs[5],
                node.inputs[6],
                node.inputs[7],
            )
        };
        let mut decline: Vec<usize> = Vec::new();
        self.gp_load_value(RAX, recv);
        self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
        decline.push(self.emit_jcc_rel32(0x84)); // JZ
        if let (Some(ss), Some(ds)) = (src_sess, dst_sess) {
            self.gp_load_value(RAX, ss);
            self.gp_load_value(RDX, ds);
            self.buf.emit(&[0x48, 0x09, 0xD0]); // OR RAX, RDX
            decline.push(self.emit_jcc_rel32(0x85)); // JNZ
        }
        self.gp_load_value(RAX, src_base);
        self.gp_load_value(RDX, dst_base);
        self.buf.emit(&[0x48, 0x09, 0xD0]); // OR RAX, RDX
        decline.push(self.emit_jcc_rel32(0x85)); // JNZ

        self.gp_load_value(R11, bytes);
        // CMP R11, 4096; JA decline (handles negative and >4096)
        self.buf.emit(&[0x49, 0x81, 0xFB, 0x00, 0x10, 0x00, 0x00]);
        decline.push(self.emit_jcc_rel32(0x87)); // JA

        self.gp_load_value(R10, src_off);
        self.buf.emit(&[0x4D, 0x85, 0xD2]); // TEST R10, R10
        decline.push(self.emit_jcc_rel32(0x84)); // JZ

        self.gp_load_value(RCX, dst_off);
        self.buf.emit(&[0x48, 0x85, 0xC9]); // TEST RCX, RCX
        decline.push(self.emit_jcc_rel32(0x84)); // JZ

        // Reject tagged arena handles (bit 62 = 1 << 62).
        self.buf.emit(&[0x4C, 0x89, 0xD0]); // MOV RAX, R10
        self.buf.emit(&[0x48, 0x09, 0xC8]); // OR RAX, RCX
        self.buf.emit(&[0x48, 0xBA, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x40]); // MOV RDX, 1<<62
        self.buf.emit(&[0x48, 0x85, 0xD0]); // TEST RAX, RDX
        decline.push(self.emit_jcc_rel32(0x85)); // JNZ

        // Forward copy is safe if dst <= src OR dst >= src + bytes.
        self.buf.emit(&[0x4C, 0x39, 0xD1]); // CMP RCX, R10
        let safe = self.emit_jcc_rel32(0x86); // JBE safe
        self.buf.emit(&[0x4B, 0x8D, 0x04, 0x1A]); // LEA RAX, [R10 + R11]
        self.buf.emit(&[0x48, 0x39, 0xC1]); // CMP RCX, RAX
        decline.push(self.emit_jcc_rel32(0x82)); // JB decline
        self.patch_rel32_to_here(safe);

        self.buf.emit(&BYTE_ARRAYCOPY_LOOPS);
        let hit = self.emit_jmp_rel32();
        for d in decline {
            self.patch_rel32_to_here(d);
        }
        self.call_prefix_join = Some(hit);
    }

    /// All-primitive `System.arraycopy` arm of [`Self::try_emit_ir_ref_arraycopy_prefix`],
    /// delegating to [`super::prim_arraycopy::emit_ir_prim_arraycopy_arm`].
    /// Scratch: RAX, RCX, RDX, R8..R11, XMM0, XMM1, and flags.
    pub(in crate::ir_lower) fn emit_ir_byte_arraycopy_arm(
        &mut self,
        args: (NodeId, NodeId, NodeId, NodeId, NodeId),
    ) -> Option<usize> {
        super::prim_arraycopy::emit_ir_prim_arraycopy_arm(self, args)
    }

    /// JIT r23 wave 11, lane L6 (`CRATONVM_JIT_IR_INLINE_REF_ARRAYCOPY_CHECKED`,
    /// perf, opt-in): the CHECKED copy of [`Self::try_emit_ir_ref_arraycopy_prefix`].
    /// It covers a reference copy whose component ids differ and whose
    /// destination is not `Object[]`, such as `ArrayList$SubList.toArray(T[])`'s
    /// `Object[]` -> `String[]`. The prefix used to send that copy whole to the
    /// bound helper (`jit_system_arraycopy_direct`'s checked arm: one call,
    /// both arrays validated, one header read per element; 44 % of the
    /// `W8L6SplitParts list` row in the w10 RIP sample).
    ///
    /// Entered from the component compare (`entry`), with RAX = `src` and RDX =
    /// `dst`, after the collector gates have already passed. It runs the
    /// prefix's range checks (the same bytes), then copies FORWARD one element
    /// at a time. Two arrays with different components are never one array, so
    /// they never overlap. For each element it loads the word, then:
    ///   * a null is stored;
    ///   * a non-null is stored only when it is a plain object (the kind tag is
    ///     `Object`, so never an array, whose header carries its COMPONENT's
    ///     id) AND its class id EQUALS `dst`'s component id (the exact-class
    ///     proof);
    ///   * any other element, including a subclass, is not proven here. It
    ///     declines to the call with the original arguments.
    ///
    /// Why the declined partial copy is exact. The helper re-copies the prefix
    /// that this arm already stored into the same slots of the same OTHER array
    /// (idempotent), then proves, copies or throws exactly as before. That is
    /// the argument the helper's own checked fallback makes.
    ///
    /// Each element is proven from the very word it stores (one load), as
    /// HotSpot's checkcast arraycopy does, so a racing writer of `src` cannot
    /// slip an unproven reference into `dst`. No call, safepoint or allocation
    /// runs between the gates and the last store, so the gates (no SATB window,
    /// `dst` young or no old objects) hold for every store, as they do for the
    /// unchecked copy.
    ///
    /// The unchecked copy just before this arm in the buffer jumps over it
    /// (pushed on `hit`). Every success jump is pushed on `hit`, every miss on
    /// `decline`.
    pub(in crate::ir_lower) fn emit_ir_ref_arraycopy_checked_arm(
        &mut self,
        entry: usize,
        (src_pos, dst_pos, len): (NodeId, NodeId, NodeId),
        hit: &mut Vec<usize>,
        decline: &mut Vec<usize>,
    ) {
        const KIND_TAGS: u8 = cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8; // Cast: x86-64 disp8
        hit.push(self.emit_jmp_rel32()); // the unchecked copy is done
        self.patch_rel32_to_here(entry);
        // --- The prefix's range checks, the same bytes (RAX = src, RDX = dst). ---
        self.gp_load_value(R11, len);
        self.buf.emit(&[0x4D, 0x63, 0xDB]); // MOVSXD R11, R11D
        self.buf.emit(&[0x4D, 0x85, 0xDB]); // TEST R11, R11
        decline.push(self.emit_jcc_rel32(0x88)); // JS
        self.gp_load_value(R10, src_pos);
        self.buf.emit(&[0x4D, 0x63, 0xD2]); // MOVSXD R10, R10D
        self.buf.emit(&[0x4D, 0x85, 0xD2]); // TEST R10, R10
        decline.push(self.emit_jcc_rel32(0x88)); // JS
        self.buf.emit(&[0x8B, 0x48, 0x08]); // MOV ECX, [RAX+8] (src.length)
        self.buf.emit(&[0x4C, 0x29, 0xD1]); // SUB RCX, R10
        self.buf.emit(&[0x4C, 0x39, 0xD9]); // CMP RCX, R11
        decline.push(self.emit_jcc_rel32(0x8C)); // JL: srcPos + len > src.length
        self.buf.emit(&[0x4E, 0x8D, 0x54, 0xD0, 0x10]); // LEA R10, [RAX + R10*8 + 16]
        self.gp_load_value(RCX, dst_pos);
        self.buf.emit(&[0x48, 0x63, 0xC9]); // MOVSXD RCX, ECX
        self.buf.emit(&[0x48, 0x85, 0xC9]); // TEST RCX, RCX
        decline.push(self.emit_jcc_rel32(0x88)); // JS
        self.buf.emit(&[0x8B, 0x42, 0x08]); // MOV EAX, [RDX+8] (dst.length)
        self.buf.emit(&[0x48, 0x29, 0xC8]); // SUB RAX, RCX
        self.buf.emit(&[0x4C, 0x39, 0xD8]); // CMP RAX, R11
        decline.push(self.emit_jcc_rel32(0x8C)); // JL: dstPos + len > dst.length
        self.buf.emit(&[0x48, 0x8D, 0x4C, 0xCA, 0x10]); // LEA RCX, [RDX + RCX*8 + 16]
        self.buf.emit(&[0x4D, 0x85, 0xDB]); // TEST R11, R11
        hit.push(self.emit_jcc_rel32(0x84)); // JZ: nothing to copy
                                             // R10 = &src[srcPos], RCX = &dst[dstPos], R11 = len > 0.
        self.buf.emit(&[0x8B, 0x12]); // MOV EDX, [RDX] (dst component id)
        let top = self.buf.pos();
        self.buf.emit(&[0x49, 0x8B, 0x02]); // MOV RAX, [R10]
        self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
                                            // JZ store: over TEST BYTE (4) + JNZ rel32 (6) + CMP (2) + JNE rel32 (6).
        self.buf.emit(&[0x74, CHECKED_COPY_NULL_SKIP]);
        self.buf
            .emit(&[0xF6, 0x40, KIND_TAGS, cratonvm_types::KIND_TAG_BYTE_MASK]);
        decline.push(self.emit_jcc_rel32(0x85)); // JNZ: not a plain object
        self.buf.emit(&[0x3B, 0x10]); // CMP EDX, [RAX] (the element's class id)
        decline.push(self.emit_jcc_rel32(0x85)); // JNE: not proven storable
                                                 // store:
        self.buf.emit(&[0x48, 0x89, 0x01]); // MOV [RCX], RAX
        self.buf.emit(&[0x49, 0x83, 0xC2, 0x08]); // ADD R10, 8
        self.buf.emit(&[0x48, 0x83, 0xC1, 0x08]); // ADD RCX, 8
        self.buf.emit(&[0x49, 0xFF, 0xCB]); // DEC R11
                                            // JNZ top (rel32).
        self.buf.emit(&[0x0F, 0x85]);
        // Cast: a backward displacement of a few dozen bytes.
        let rel = top as i64 - (self.buf.pos() as i64 + 4);
        self.buf.emit(&(rel as i32).to_le_bytes());
        hit.push(self.emit_jmp_rel32()); // copied
    }

    /// The copy of [`Self::try_emit_ir_ref_arraycopy_prefix`] under
    /// `CRATONVM_JIT_IR_INLINE_REF_ARRAYCOPY_UNROLL` (JIT r23 wave 7, lane L5;
    /// perf, default ON, `=0` restores the one-word loops). Entry: R10 =
    /// `&src[srcPos]`, RCX = `&dst[dstPos]`, R11 = `len` > 0. Every rel32 it
    /// pushes on `hit` must be patched to the copy's end.
    ///
    /// n23-w6, `new Object[n]` + copy spins: the one-word loop ran BACKWARD
    /// for every fresh destination (allocated above the source, so `dst > src`)
    /// at ~2 cycles per word on its store, and a 1000-word copy into a
    /// preallocated array took 370 ns against the helper's 282 and HotSpot's
    /// 225. Two changes, each keeping every element a whole aligned 8-byte
    /// load and store (no element is ever torn, as `copy_slots_atomic`):
    ///
    /// * backward ONLY when the ranges overlap with `dst` above `src`
    ///   (`src < dst < src + 8 len`, unsigned); every other copy -- two arrays
    ///   included -- runs forward, the direction the prefetchers follow;
    /// * four words per iteration (two loads, two stores, twice: RAX and RDX),
    ///   then a one-word tail. Memmove-correct in both directions: each pair
    ///   is loaded before it is stored and the pairs advance in the copy's
    ///   direction, so a store never lands on a source word not yet loaded.
    ///
    /// Scratch: RAX, RDX (the destination object is not needed past the
    /// address computation), R10, R11, RCX, the flags.
    pub(in crate::ir_lower) fn emit_ir_ref_copy_words_unrolled(&mut self, hit: &mut Vec<usize>) {
        // Backward only for `src < dst < src + 8*len`.
        self.buf.emit(&[0x4C, 0x39, 0xD1]); // CMP RCX, R10
        let forward_low = self.emit_jcc_rel32(0x86); // JBE: dst at or below src
        self.buf.emit(&[0x4B, 0x8D, 0x04, 0xDA]); // LEA RAX, [R10 + R11*8]
        self.buf.emit(&[0x48, 0x39, 0xC1]); // CMP RCX, RAX
        let forward_high = self.emit_jcc_rel32(0x83); // JAE: no overlap
                                                      // Backward, from the last word down.
        self.buf.emit(&[0x4F, 0x8D, 0x54, 0xDA, 0xF8]); // LEA R10, [R10 + R11*8 - 8]
        self.buf.emit(&[0x4A, 0x8D, 0x4C, 0xD9, 0xF8]); // LEA RCX, [RCX + R11*8 - 8]
        self.buf.emit(&[0x49, 0x83, 0xEB, 0x04]); // SUB R11, 4
        let back_tail = self.emit_jcc_rel32(0x82); // JB: fewer than four
        let top = self.buf.pos();
        self.buf.emit(&[0x49, 0x8B, 0x02]); // MOV RAX, [R10]
        self.buf.emit(&[0x49, 0x8B, 0x52, 0xF8]); // MOV RDX, [R10 - 8]
        self.buf.emit(&[0x48, 0x89, 0x01]); // MOV [RCX], RAX
        self.buf.emit(&[0x48, 0x89, 0x51, 0xF8]); // MOV [RCX - 8], RDX
        self.buf.emit(&[0x49, 0x8B, 0x42, 0xF0]); // MOV RAX, [R10 - 16]
        self.buf.emit(&[0x49, 0x8B, 0x52, 0xE8]); // MOV RDX, [R10 - 24]
        self.buf.emit(&[0x48, 0x89, 0x41, 0xF0]); // MOV [RCX - 16], RAX
        self.buf.emit(&[0x48, 0x89, 0x51, 0xE8]); // MOV [RCX - 24], RDX
        self.buf.emit(&[0x49, 0x83, 0xEA, 0x20]); // SUB R10, 32
        self.buf.emit(&[0x48, 0x83, 0xE9, 0x20]); // SUB RCX, 32
        self.buf.emit(&[0x49, 0x83, 0xEB, 0x04]); // SUB R11, 4
                                                  // Cast: a 42-byte body, so the JAE's rel8 is -44.
        let rel = top as i64 - (self.buf.pos() as i64 + 2);
        self.buf.emit(&[0x73, rel as i8 as u8]); // JAE top: four or more were left
        self.patch_rel32_to_here(back_tail);
        self.buf.emit(&[0x49, 0x83, 0xC3, 0x04]); // ADD R11, 4 (the 0..=3 left)
        hit.push(self.emit_jcc_rel32(0x84)); // JZ: done
        let top = self.buf.pos();
        self.buf.emit(&[0x49, 0x8B, 0x02]); // MOV RAX, [R10]
        self.buf.emit(&[0x48, 0x89, 0x01]); // MOV [RCX], RAX
        self.buf.emit(&[0x49, 0x83, 0xEA, 0x08]); // SUB R10, 8
        self.buf.emit(&[0x48, 0x83, 0xE9, 0x08]); // SUB RCX, 8
        self.buf.emit(&[0x49, 0xFF, 0xCB]); // DEC R11
                                            // Cast: a 17-byte body, so the JNZ's rel8 is -19.
        let rel = top as i64 - (self.buf.pos() as i64 + 2);
        self.buf.emit(&[0x75, rel as i8 as u8]); // JNZ top
        hit.push(self.emit_jmp_rel32());
        // Forward.
        self.patch_rel32_to_here(forward_low);
        self.patch_rel32_to_here(forward_high);
        self.buf.emit(&[0x49, 0x83, 0xEB, 0x04]); // SUB R11, 4
        let fwd_tail = self.emit_jcc_rel32(0x82); // JB: fewer than four
        let top = self.buf.pos();
        self.buf.emit(&[0x49, 0x8B, 0x02]); // MOV RAX, [R10]
        self.buf.emit(&[0x49, 0x8B, 0x52, 0x08]); // MOV RDX, [R10 + 8]
        self.buf.emit(&[0x48, 0x89, 0x01]); // MOV [RCX], RAX
        self.buf.emit(&[0x48, 0x89, 0x51, 0x08]); // MOV [RCX + 8], RDX
        self.buf.emit(&[0x49, 0x8B, 0x42, 0x10]); // MOV RAX, [R10 + 16]
        self.buf.emit(&[0x49, 0x8B, 0x52, 0x18]); // MOV RDX, [R10 + 24]
        self.buf.emit(&[0x48, 0x89, 0x41, 0x10]); // MOV [RCX + 16], RAX
        self.buf.emit(&[0x48, 0x89, 0x51, 0x18]); // MOV [RCX + 24], RDX
        self.buf.emit(&[0x49, 0x83, 0xC2, 0x20]); // ADD R10, 32
        self.buf.emit(&[0x48, 0x83, 0xC1, 0x20]); // ADD RCX, 32
        self.buf.emit(&[0x49, 0x83, 0xEB, 0x04]); // SUB R11, 4
                                                  // Cast: as above.
        let rel = top as i64 - (self.buf.pos() as i64 + 2);
        self.buf.emit(&[0x73, rel as i8 as u8]); // JAE top
        self.patch_rel32_to_here(fwd_tail);
        self.buf.emit(&[0x49, 0x83, 0xC3, 0x04]); // ADD R11, 4
        hit.push(self.emit_jcc_rel32(0x84)); // JZ: done
        let top = self.buf.pos();
        self.buf.emit(&[0x49, 0x8B, 0x02]); // MOV RAX, [R10]
        self.buf.emit(&[0x48, 0x89, 0x01]); // MOV [RCX], RAX
        self.buf.emit(&[0x49, 0x83, 0xC2, 0x08]); // ADD R10, 8
        self.buf.emit(&[0x48, 0x83, 0xC1, 0x08]); // ADD RCX, 8
        self.buf.emit(&[0x49, 0xFF, 0xCB]); // DEC R11
                                            // Cast: as above.
        let rel = top as i64 - (self.buf.pos() as i64 + 2);
        self.buf.emit(&[0x75, rel as i8 as u8]); // JNZ top
                                                 // Falls through to the copy's end.
    }

    /// JIT r23 wave 8, lane L8 (`CRATONVM_JIT_IR_INLINE_REFLECT_NEW_ARRAY`;
    /// page `r23-w3-reference-arraycopy-costs-microseconds-FIXED-20261021.md`):
    /// `Array.newArray(component, n)` -- the call inside `Array.newInstance`,
    /// `Arrays.copyOf(T[], int)` and `Collection.toArray(T[])` -- allocated
    /// INLINE as a guarded PREFIX of its `Op::Call`, as HotSpot's C2 does
    /// (`LibraryCallKit::inline_unsafe_newArray`). n23-w7: `copyOf-10` 75-79
    /// ns against HotSpot's 10, most of it the bound helper's per-call cost
    /// (`jit_reflect_new_array_direct`: contain scope, boundary note, the
    /// mirror memo's thread-locals, `jit_anewarray_object_body`'s entry hooks).
    ///
    /// Only a site this compile binds to that thin helper gets the prefix,
    /// so it only ever replaces the helper's SERVED arm, never a JDK body.
    /// The hit path:
    ///
    /// 1. the component mirror's class id `c` from the mirror-hint band,
    ///    proven by `c`'s class word ([`reflect_new_array_resolve_bytes`]: a
    ///    non-array, non-primitive reference class, the helper's admitted
    ///    set), parked in the phi-copy scratch word (a class id, never a
    ///    reference; no GC point until it is read);
    /// 2. the reference-array TLAB bump of a compiled `anewarray` with `c` as
    ///    its class word
    ///    ([`crate::runtime_lowering::emit_inline_tlab_ref_array_runtime_class_fast_path_ir`]):
    ///    the length screened to `0..=cap` by one unsigned compare, the
    ///    header before the commit, the body the refill's zeroed chunk, ZGC's
    ///    start bit set inline before the commit;
    /// 3. the result home (and register), then the join.
    ///
    /// It is the array `jit_anewarray_object_body(c, n)` hands the helper,
    /// header word for header word: the class word is the component's id
    /// there too, and neither creates the array class. Every other input --
    /// a null component (the NPE), a negative or over-cap length (the NASE,
    /// the native's large-array ladder), a primitive / `void` / array /
    /// lambda-proxy / unhinted mirror, a thread with no cached `JvmThread`, a
    /// full TLAB, a refused ZGC start-bit store -- declines to the unchanged
    /// call before anything is committed. The hit makes no call, polls
    /// nothing and cannot collect or trap, so it owes no map, republish or
    /// reload (the `Integer.valueOf` box prefix's argument).
    ///
    /// JVMTI `VMObjectAlloc` / `SampledObjectAlloc` (j27 wave 4, lane
    /// CLONEIR; parity fix, no switch): the helper declines to the native
    /// (which posts the event) while a C JVMTI env listens, and so does the
    /// prefix now: its first bytes test the VM's event word
    /// (`DirectHelperTable::jvmti_alloc_events_word`, published only by an
    /// `experimental-debug` build, the only one with a C JVMTI table) and
    /// decline before anything else. It used to post nothing.
    ///
    /// Compile-time declines: the switch; the site screen; no helper bind; no
    /// `getClass()` table or `CRATONVM_JIT_CLASS_IS_ASSIGNABLE_INLINE=0` (no
    /// hints are ever filled); an armed ZGC read barrier; the inline bump's
    /// own switches (`CRATONVM_JIT_IR_INLINE_TLAB`,
    /// `CRATONVM_JIT_INLINE_TLAB_ANEWARRAY`, on Generational / G1
    /// `CRATONVM_JIT_IR_INLINE_TLAB_NEWARRAY` and
    /// `CRATONVM_JIT_IR_INLINE_TLAB_ANEWARRAY_ANY_GC`, on ZGC the inline array
    /// announce); carried operands or result, a length or result without a
    /// frame home. Scratch: RAX, RCX, RDX, R8-R11, the flags.
    pub(in crate::ir_lower) fn try_emit_ir_reflect_new_array_prefix(
        &mut self,
        id: NodeId,
        info_ptr: usize,
    ) {
        if info_ptr == 0 || !ir_inline_reflect_new_array_enabled() {
            return;
        }
        // SAFETY: as every other read of an `Op::Call`'s `info_ptr` in this
        // file -- a live `JitInvokeInfo` owned by `ir_call_infos`.
        let info: &JitInvokeInfo = unsafe { &*(info_ptr as *const JitInvokeInfo) };
        if !reflect_new_array_prefix_site(
            info.class_name,
            info.method_name,
            info.descriptor,
            info.invoke_kind,
        ) {
            return;
        }
        let graph = self.graph;
        let Some(node) = graph.node_opt(id) else {
            return;
        };
        // [ctrl, mem, component, length].
        if node.inputs.len() != 4 || node.ty != IrType::Ref {
            self.note_string_prefix_declined(id, info, "node shape");
            return;
        }
        let helper = self.direct_helpers.reflect_new_array;
        let bound_to_helper = helper != 0
            && node
                .bytecode_pc
                .and_then(|pc| self.direct_calls.get(&pc))
                .is_some_and(|&(entry, _)| entry == helper);
        if !bound_to_helper {
            self.note_string_prefix_declined(id, info, "not bound to the thin helper");
            return;
        }
        let table = self.direct_helpers.getclass_mirror_words;
        if table == 0
            || !crate::class_is_assignable_inline_enabled()
            || crate::x64::zgc_read_barrier_blocks_inline_fields()
            || !crate::ir_thin_prefix_enabled()
        {
            self.note_string_prefix_declined(id, info, "no mirror hints / armed ZGC");
            return;
        }
        let registration = cratonvm_types::jit_tlab_registration_required();
        let bump_admitted = ir_inline_tlab_enabled()
            && crate::x64::inline_tlab_anewarray_enabled()
            && if registration {
                crate::x64::zgc_inline_array_announce_enabled() && self.zgc_note_tlab_object != 0
            } else {
                crate::runtime_lowering::ir_inline_anewarray_any_gc_enabled()
            };
        let scratch = self.phi_copy_scratch_slot_off;
        if !bump_admitted || self.anewarray_object == 0 || scratch <= 0 {
            self.note_string_prefix_declined(id, info, "inline array bump not admitted");
            return;
        }
        let (component, len) = (node.inputs[2], node.inputs[3]);
        if self.carry_names(component)
            || self.carry_names(len)
            || graph.node_opt(len).map(|n| n.ty) != Some(IrType::Int)
            || self
                .home_dropped
                .get(len as usize)
                .copied()
                .unwrap_or(false)
        {
            self.note_string_prefix_declined(id, info, "a carried or home-less operand");
            return;
        }
        let Ok(length_off) = self.slot_of_checked(len) else {
            return;
        };
        let Some(slot) = self.prefix_result_slot(id, node.ty) else {
            self.note_string_prefix_declined(id, info, "a carried, routed, or dropped result home");
            return;
        };
        let plan = crate::runtime_lowering::InlineTlabArrayPlan {
            thread_slot_off: self.shadow_thread_slot_off,
            thread_tls_disp: self.ir_tlab_thread_tls_disp(),
            cursor_off: self.tlab_cursor_off,
            end_off: self.tlab_end_off,
            context_off: self.context_slot_off,
            atype: 0,
            length_off,
            announce: if registration {
                self.zgc_note_tlab_object
            } else {
                0
            },
            // The class word is the run-time one; the plan's `Some` only
            // admits the reference arm.
            component_class_id: Some(0),
            const_length: match graph.node_opt(len).map(|n| &n.op) {
                Some(Op::Const(v)) => i32::try_from(*v).ok().and_then(|v| u32::try_from(v).ok()),
                _ => None,
            },
        };
        if crate::ir_stage_reporting() || crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] reflect-new-array-inline (ir) n{id} pc={:?} table={table:#x} \
                 announce={}",
                node.bytecode_pc,
                plan.announce != 0
            );
        }
        self.note_rcx_written();
        self.gp_load_value(RDX, component);
        let (bytes, edges) =
            reflect_new_array_resolve_bytes(table, self.direct_helpers.jvmti_alloc_events_word);
        let at = self.buf.pos();
        self.buf.emit(&bytes);
        let mut decline: Vec<usize> = edges.into_iter().map(|e| at + e).collect();
        // MOV [RBP - scratch], RCX: the component class id, zero-extended.
        let mut park = FrameAccess::new();
        enc_frame_store(RCX, scratch, &mut park);
        self.buf.emit(park.as_slice());
        let bumped = crate::runtime_lowering::emit_inline_tlab_ref_array_runtime_class_fast_path_ir(
            &mut self.buf,
            &plan,
            self.anewarray_object,
            scratch,
            &mut decline,
        );
        if bumped {
            self.emit_prefix_result_store(id, slot);
            crate::ir_evidence::note(crate::ir_evidence::Transform::ScalarIntrinsic);
            let hit = self.emit_jmp_rel32();
            for p in decline {
                self.patch_rel32_to_here(p);
            }
            self.call_prefix_join = Some(hit);
            return;
        }
        // The bump refused, having emitted nothing: every edge, falling off
        // the end included, lands on the unchanged call.
        self.note_string_prefix_declined(id, info, "the bump emitter refused");
        for p in decline {
            self.patch_rel32_to_here(p);
        }
    }

    /// JIT r20 wave 18, lane xUtf8 (book `jit-r20-strings-proposals.md`,
    /// STR20-6; `CRATONVM_JIT_IR_STRING_CODING_PREFIX`): the two
    /// `java/lang/StringCoding` `@IntrinsicCandidate` leaves the coders' ASCII
    /// runs go through, answered inline at their `invokestatic` as a guarded
    /// PREFIX of the `Op::Call` (HotSpot has a stub for each):
    ///
    /// * `countPositives(byte[] ba, int off, int len)` -- `String.decodeASCII`
    ///   (`UTF_8$Decoder.decodeArrayLoop`'s `JLA.uncheckedDecodeASCII`),
    ///   `hasNegatives`, the `new String(bytes, cs)` / `getBytes` ASCII fast
    ///   paths: 16 bytes per SSE2 step (`PMOVMSKB`, then `BSF` on a hit);
    /// * `implEncodeAsciiArray(char[] sa, int sp, byte[] da, int dp, int len)`
    ///   -- `UTF_8$Encoder.encodeArrayLoop`'s `JLA.uncheckedEncodeASCII`: 8
    ///   chars per SSE2 step (a saturating add of `0x7F80` sets bit 15 of a
    ///   lane exactly when its char is `>= 0x80`; `PACKUSWB` then narrows the
    ///   eight tested chars, all `< 0x80`, from the SAME register, so a racing
    ///   writer can never get an untested char stored).
    ///
    /// Both JDK bodies are unchecked loops: a non-positive `len` touches
    /// nothing (even a null array), and an out-of-range index throws
    /// `ArrayIndexOutOfBoundsException` after the elements before it were
    /// scanned / stored. So this answers only the fully in-range case --
    /// non-null arrays of the declared element types (kind tag), every offset
    /// in `[0, length]` and `len <= length - off` (unsigned compares, so a
    /// negative value declines) -- and DECLINES every other input to the call
    /// before anything is stored, which reproduces the bytecode's answer,
    /// partial store and throw. The answer is the bytecode's exactly: the
    /// index of the first negative byte, else `len` (the JDK's implSpec would
    /// allow less; the interpreter answers exactly this), and the number of
    /// chars copied before the first one `>= 0x80`, at or past which nothing
    /// is stored. No field offsets are baked, so no layout-epoch guard; the
    /// loops poll nothing (leaves of at most `len` steps, like the bodies they
    /// replace). Scratch: RAX, RCX, RDX, R10, R11, XMM0/XMM1, the `Inflate`
    /// prefix's set.
    pub(in crate::ir_lower) fn try_emit_ir_string_coding_prefix(
        &mut self,
        id: NodeId,
        info: &JitInvokeInfo,
        kind: crate::IrStringSearchKind,
    ) {
        use crate::IrStringSearchKind as K;
        if !kind.is_string_coding() || !kind.admits_invoke_kind(info.invoke_kind) {
            self.note_string_prefix_declined(
                id,
                info,
                &format!("invoke_kind={}", info.invoke_kind),
            );
            return;
        }
        let (Some(byte_array_tag), Some(char_array_tag)) = (
            cratonvm_types::primitive_array_kind_tags_byte("[B"),
            cratonvm_types::primitive_array_kind_tags_byte("[C"),
        ) else {
            return;
        };
        let graph = self.graph;
        let Some(node) = graph.nodes.get(id as usize) else {
            return;
        };
        // [ctrl, mem, ba, off, len] / [ctrl, mem, sa, sp, da, dp, len], `int`
        // (static: the first array stands where a receiver would).
        if node.inputs.len() != 3 + kind.num_params() || node.ty != IrType::Int {
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
        if crate::ir_stage_reporting() {
            eprintln!(
                "[ir] string-coding-prefix n{id} {}.{}{} kind={kind:?}",
                info.class_name, info.method_name, info.descriptor,
            );
        }
        // The array length at `+8` is baked as the `Inflate` prefix bakes it
        // (the String search prefix's `const` assert covers that layout).
        const _: () = assert!(
            cratonvm_types::ARRAY_DATA_OFFSET == 16,
            "the StringCoding prefix bakes ARRAY_DATA_OFFSET 16"
        );
        const KIND_TAGS: u8 = cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8; // Cast: x86-64 disp8
        const _: () = assert!(cratonvm_types::KIND_TAGS_BYTE_OFFSET <= 127);
        let mut decline: Vec<usize> = Vec::new();
        // The raw `LEA RCX` below writes RCX too.
        self.note_rcx_written();
        // R10D = len, zero-extended: a negative `len` fails the bounds below
        // and declines (the call answers it).
        let len = node.inputs[if kind == K::CountPositives { 4 } else { 6 }];
        self.gp_load_value(R10, len);
        self.buf.emit(&[0x45, 0x89, 0xD2]); // MOV R10D, R10D
                                            // RCX = the source array: non-null, of the declared element type.
        let (src, src_off) = (node.inputs[2], node.inputs[3]);
        let src_tag = if kind == K::CountPositives {
            byte_array_tag
        } else {
            char_array_tag
        };
        self.gp_load_value(RCX, src);
        self.emit_str_test_r64(RCX);
        decline.push(self.emit_jcc_rel32(0x84)); // JZ
        self.buf.emit(&[0x80, 0x79, KIND_TAGS, src_tag]); // CMP BYTE [RCX+KIND_TAGS], tag
        decline.push(self.emit_jcc_rel32(0x85)); // JNE
                                                 // R11D = off in [0, src.length]; len <= src.length - off.
        self.gp_load_value(R11, src_off);
        self.buf.emit(&[0x45, 0x89, 0xDB]); // MOV R11D, R11D
        self.buf.emit(&[0x44, 0x3B, 0x59, 0x08]); // CMP R11D, [RCX+8]
        decline.push(self.emit_jcc_rel32(0x87)); // JA
        self.emit_load_base_disp32(RAX, RCX, 8, false); // EAX = src.length
        self.buf.emit(&[0x44, 0x29, 0xD8]); // SUB EAX, R11D
        self.buf.emit(&[0x41, 0x39, 0xC2]); // CMP R10D, EAX
        decline.push(self.emit_jcc_rel32(0x87)); // JA
        if kind == K::CountPositives {
            // No decline past this point.
            self.buf.emit(&[0x4A, 0x8D, 0x0C, 0x19]); // LEA RCX, [RCX+R11]
            self.emit_str_count_positives_scan();
        } else {
            self.buf.emit(&[0x4A, 0x8D, 0x0C, 0x59]); // LEA RCX, [RCX+R11*2]
            let (dst, dst_off) = (node.inputs[4], node.inputs[5]);
            // RDX = da: non-null `byte[]`.
            self.gp_load_value(RDX, dst);
            self.emit_str_test_r64(RDX);
            decline.push(self.emit_jcc_rel32(0x84)); // JZ
            self.buf.emit(&[0x80, 0x7A, KIND_TAGS, byte_array_tag]); // CMP BYTE [RDX+KIND_TAGS], [B tag
            decline.push(self.emit_jcc_rel32(0x85)); // JNE
                                                     // EAX = dp in [0, da.length]; len <= da.length - dp.
            self.gp_load_value(RAX, dst_off);
            self.buf.emit(&[0x89, 0xC0]); // MOV EAX, EAX
            self.buf.emit(&[0x3B, 0x42, 0x08]); // CMP EAX, [RDX+8]
            decline.push(self.emit_jcc_rel32(0x87)); // JA
            self.emit_load_base_disp32(R11, RDX, 8, false); // R11D = da.length
            self.buf.emit(&[0x41, 0x29, 0xC3]); // SUB R11D, EAX
            self.buf.emit(&[0x45, 0x39, 0xDA]); // CMP R10D, R11D
            decline.push(self.emit_jcc_rel32(0x87)); // JA
                                                     // No decline past this point.
            self.buf.emit(&[0x48, 0x8D, 0x14, 0x02]); // LEA RDX, [RDX+RAX]
            self.emit_str_encode_ascii_copy();
        }
        // EAX = the answer, in [0, len]. The call route's `store_rax(slot)`
        // at run time, written as the ArrayList prefix writes it: the
        // resident register when the value has one, then the home word.
        self.emit_prefix_result_store(id, slot);
        // A call the single-pass backend has no inline form for, lowered as
        // straight-line code: acceptance evidence, as `Inflate`.
        crate::ir_evidence::note(crate::ir_evidence::Transform::ScalarIntrinsic);
        let hit = self.emit_jmp_rel32();
        for p in decline {
            self.patch_rel32_to_here(p);
        }
        self.call_prefix_join = Some(hit);
    }

    /// JIT r21 wave 1/3, lane strings: the Latin-1 case kinds
    /// ([`crate::IrStringSearchKind::is_latin1_case`]) as a guarded PREFIX of
    /// the `Op::Call`, with the `char[]` copies' contract
    /// ([`Self::try_emit_ir_string_chars_prefix`]: scratch RAX, RCX, RDX,
    /// R8-R11; the hit edge stores the result and joins; no call, no poll, no
    /// throw; every decline precedes the first store and lands on the
    /// unchanged call).
    ///
    /// * `String.toLowerCase(Locale)` / `toLowerCase()` (ZCP-3,
    ///   `CRATONVM_JIT_IR_STRING_LOWER_UNCHANGED_PREFIX`) and the static
    ///   `StringLatin1.toLowerCase(String, byte[], Locale)` (the worker,
    ///   `CRATONVM_JIT_IR_STRING_INTERNAL_ADMIT`): JDK 17 / 21 / 25
    ///   `StringLatin1.toLowerCase` -- a null locale throws (decline); the
    ///   scan for the first `CharacterDataLatin1.isUpperCase` byte (`A-Z`,
    ///   `0xC0..=0xD6`, `0xD8..=0xDE`: the JDK's `B[]` bit `0x0002`); none
    ///   answers `str` itself. From the first upper-case byte on, a byte
    ///   ConditionalSpecialCasing's lower-case table names for `tr` / `az` /
    ///   `lt` (`I`, `J`, `0xCC`, `0xCD` -- every other row of it is outside
    ///   Latin-1) declines, because the language is not read here; otherwise
    ///   the result is the non-locale arm's, `new String(mapped, LATIN1)` of
    ///   `CharacterDataLatin1.toLowerCase` of every byte, and that is what
    ///   [`crate::x64::concat_emit::emit_ir_latin1_to_lower_string`] builds
    ///   (a FRESH String, as the JDK's). Without an allocation plan for this
    ///   compile, a changed string declines. A UTF16 receiver declines.
    pub(in crate::ir_lower) fn try_emit_ir_string_latin1_case_prefix(
        &mut self,
        id: NodeId,
        info: &JitInvokeInfo,
        kind: crate::IrStringSearchKind,
        layout: crate::StringFieldLayout,
    ) {
        use crate::x64::concat_emit as ce;
        use crate::IrStringSearchKind as K;
        if !kind.is_latin1_case()
            || !kind.admits_invoke_kind(info.invoke_kind)
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
        let Some(byte_array_tag) = cratonvm_types::primitive_array_kind_tags_byte("[B") else {
            return;
        };
        const KIND_TAGS: u8 = cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8; // Cast: x86-64 disp8
        const _: () = assert!(cratonvm_types::KIND_TAGS_BYTE_OFFSET <= 127);
        const _: () = assert!(
            cratonvm_types::ARRAY_LENGTH_OFFSET == 8 && cratonvm_types::ARRAY_DATA_OFFSET == 16,
            "the Latin-1 case prefix bakes ARRAY_LENGTH_OFFSET 8 / ARRAY_DATA_OFFSET 16"
        );
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
        let Some(slot) = self.prefix_result_slot(id, node.ty) else {
            self.note_string_prefix_declined(id, info, "a carried, routed, or dropped result home");
            return;
        };
        // The allocation plan of the changed-string arm (the `char[]` copies'
        // screens); `None` keeps the unchanged arm only.
        let site = {
            let frame = ce::ConcatIrFrame {
                tls_disp: crate::x64::jit_thread_tls_disp(),
                thread_slot_off: self.shadow_thread_slot_off,
                cursor_off: self.tlab_cursor_off,
                end_off: self.tlab_end_off,
                class_id_off: self.concat_class_id_off,
                g1_table: self.g1_barrier_addr,
                region_bounds_addr: self.concat_region_bounds_addr,
            };
            match ce::chars_ir_site(
                Some(layout),
                self.direct_helpers.compact_strings_flag_addr(),
                frame,
            ) {
                Ok(site) => Some(site),
                Err(why) => {
                    self.note_string_prefix_declined(id, info, why);
                    None
                }
            }
        };
        if crate::ir_stage_reporting() {
            eprintln!(
                "[ir] string-latin1-case-prefix n{id} {}.{}{} kind={kind:?} allocates={}",
                info.class_name,
                info.method_name,
                info.descriptor,
                site.is_some(),
            );
        }
        let mut decline: Vec<usize> = Vec::new();
        self.note_rcx_written();
        // The `str` the unchanged arm answers (the receiver, or the worker's
        // first argument).
        let str_node = node.inputs[2];
        // The locale: the worker's third operand, the user site's
        // argument; `toLowerCase()` passes `Locale.getDefault()`.
        let locale = match kind {
            K::Latin1ToLowerCase => Some(node.inputs[4]),
            K::LowerLocaleUnchanged => Some(node.inputs[3]),
            _ => None,
        };
        if let Some(l) = locale {
            self.gp_load_value(R10, l);
            self.buf.emit(&[0x4D, 0x85, 0xD2]); // TEST R10, R10
            decline.push(self.emit_jcc_rel32(0x84)); // JZ: the call's NPE
        }
        if kind == K::Latin1ToLowerCase {
            // RCX = the `byte[]` the caller proved LATIN1.
            self.gp_load_value(RCX, node.inputs[3]);
            self.emit_str_test_r64(RCX);
            decline.push(self.emit_jcc_rel32(0x84)); // JZ
            self.buf.emit(&[0x80, 0x79, KIND_TAGS, byte_array_tag]); // CMP BYTE [RCX+KIND_TAGS], [B
            decline.push(self.emit_jcc_rel32(0x85)); // JNE
        } else {
            // A layout REPLACEMENT invalidates the baked field offsets.
            if let Some(p) = self.emit_layout_epoch_guard() {
                decline.push(p);
            }
            // RAX = the receiver: non-null, a String; RCX = value,
            // EDX = coder (LATIN1 only).
            self.gp_load_value(RAX, str_node);
            self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
            decline.push(self.emit_jcc_rel32(0x84)); // JZ
            self.emit_str_cmp_class_id(RAX, layout.string_class_id);
            decline.push(self.emit_jcc_rel32(0x85)); // JNE
            self.emit_str_value_and_coder(&layout, RAX, RCX, Some(RDX));
            self.emit_str_test_r64(RCX);
            decline.push(self.emit_jcc_rel32(0x84)); // JZ
            self.buf.emit(&[0x85, 0xD2]); // TEST EDX, EDX
            decline.push(self.emit_jcc_rel32(0x85)); // JNZ: UTF16
        }
        // R11D = len; EDX = the index of the first upper-case byte.
        self.buf.emit(&[0x44, 0x8B, 0x59, 0x08]); // MOV R11D, [RCX+8]
        self.buf.emit(&[0x31, 0xD2]); // XOR EDX, EDX
        let top = self.buf.pos();
        self.buf.emit(&[0x44, 0x39, 0xDA]); // CMP EDX, R11D
        let unchanged = self.emit_jcc_rel32(0x83); // JAE: no upper-case byte
        self.buf.emit(&[0x44, 0x0F, 0xB6, 0x54, 0x11, 0x10]); // MOVZX R10D, BYTE [RCX+RDX+16]
        self.buf.emit(&[0x41, 0x8D, 0x42, 0xBF]); // LEA EAX, [R10-0x41]
        self.buf.emit(&[0x83, 0xF8, 0x19]); // CMP EAX, 25
        let mut changed = vec![self.emit_jcc_rel32(0x86)]; // JBE: `A-Z`
        self.buf.emit(&[0x41, 0x8D, 0x82, 0x40, 0xFF, 0xFF, 0xFF]); // LEA EAX, [R10-0xC0]
        self.buf.emit(&[0x83, 0xF8, 0x1E]); // CMP EAX, 0x1E
        let not_upper = self.emit_jcc_rel32(0x87); // JA: below 0xC0 or above 0xDE
        self.buf.emit(&[0x41, 0x81, 0xFA, 0xD7, 0x00, 0x00, 0x00]); // CMP R10D, 0xD7
        changed.push(self.emit_jcc_rel32(0x85)); // JNE: `0xC0..=0xDE` minus the sign
        self.patch_rel32_to_here(not_upper);
        self.buf.emit(&[0xFF, 0xC2]); // INC EDX
        self.emit_jmp_back_to(top);
        // The first upper-case byte is at EDX.
        for p in changed {
            self.patch_rel32_to_here(p);
        }
        if let Some(site) = site.as_ref() {
            // Every byte from EDX on: none of `I`, `J`, `0xCC`, `0xCD`
            // (the bytes before it are not upper case, so not these).
            let sens_top = self.buf.pos();
            self.buf.emit(&[0x44, 0x39, 0xDA]); // CMP EDX, R11D
            let clean = self.emit_jcc_rel32(0x83); // JAE: no sensitive byte
            self.buf.emit(&[0x44, 0x0F, 0xB6, 0x54, 0x11, 0x10]); // MOVZX R10D, BYTE [RCX+RDX+16]
            self.buf.emit(&[0x41, 0x8D, 0x42, 0xB7]); // LEA EAX, [R10-0x49]
            self.buf.emit(&[0x83, 0xF8, 0x01]); // CMP EAX, 1
            decline.push(self.emit_jcc_rel32(0x86)); // JBE: `I` / `J`
            self.buf.emit(&[0x41, 0x8D, 0x82, 0x34, 0xFF, 0xFF, 0xFF]); // LEA EAX, [R10-0xCC]
            self.buf.emit(&[0x83, 0xF8, 0x01]); // CMP EAX, 1
            decline.push(self.emit_jcc_rel32(0x86)); // JBE: `0xCC` / `0xCD`
            self.buf.emit(&[0xFF, 0xC2]); // INC EDX
            self.emit_jmp_back_to(sens_top);
            self.patch_rel32_to_here(clean);
            // RDX = the array, ECX = its length: the emitter's entry.
            self.buf.emit(&[0x48, 0x89, 0xCA]); // MOV RDX, RCX
            self.buf.emit(&[0x44, 0x89, 0xD9]); // MOV ECX, R11D
            ce::emit_ir_latin1_to_lower_string(&mut self.buf, site, &mut decline);
            // RAX = the fresh String: on to the shared store.
            let to_store = self.emit_jmp_rel32();
            // `return str`.
            self.patch_rel32_to_here(unchanged);
            self.gp_load_value(RAX, str_node);
            self.patch_rel32_to_here(to_store);
        } else {
            // No allocation plan: a changed string is the call.
            decline.push(self.emit_jmp_rel32());
            self.patch_rel32_to_here(unchanged);
            self.gp_load_value(RAX, str_node);
        }
        self.emit_integer_value_of_result_store(id, slot);
        crate::ir_evidence::note(crate::ir_evidence::Transform::ScalarIntrinsic);
        let hit = self.emit_jmp_rel32();
        for p in decline {
            self.patch_rel32_to_here(p);
        }
        self.call_prefix_join = Some(hit);
    }

    /// JIT round 21, lane strings (ZCP-4,
    /// `CRATONVM_JIT_IR_LATIN1_COMPARE_TO_CI_PREFIX`): the static
    /// `StringLatin1.compareToCI(byte[] value, byte[] other)` answered inline
    /// at its `invokestatic` as a guarded PREFIX of the `Op::Call`. The JDK
    /// body (17 / 21 / 25 alike) walks `k < min(len1, len2)`; at a differing
    /// byte it folds both through `(char) CharacterDataLatin1.toUpperCase` and
    /// then `Character.toLowerCase` and answers the first non-zero difference,
    /// else `len1 - len2`. Over Latin-1 that fold is: `A-Z` and
    /// `0xC0..=0xDE` except `0xD7` plus `0x20`; `0xB5` (MICRO SIGN, upper
    /// case `0x39C`) to `0x3BC`; every other byte (`0xFF` -- upper case
    /// `0x178`, lower case back to `0xFF` --, `0xDF`, `0xD7`, `0xF7`, `0xAA`,
    /// `0xBA`, ...) to itself. The fold is injective on the image of the
    /// upper-case step, so "upper cases differ and lower cases differ" is
    /// "folds differ", and the answer is the folds' difference. A null array
    /// (the body's `arraylength` NPE) or a non-`byte[]` declines before any
    /// read. Scratch: RAX, RCX, RDX, R10, R11, XMM0. No safepoint in the loop
    /// (a leaf of at most `min(len1, len2)` steps, like the body it replaces).
    pub(in crate::ir_lower) fn try_emit_ir_latin1_compare_to_ci_prefix(
        &mut self,
        id: NodeId,
        info: &JitInvokeInfo,
    ) {
        use crate::IrStringSearchKind as K;
        if !K::Latin1CompareToCI.admits_invoke_kind(info.invoke_kind) {
            self.note_string_prefix_declined(
                id,
                info,
                &format!("invoke_kind={}", info.invoke_kind),
            );
            return;
        }
        let Some(byte_array_tag) = cratonvm_types::primitive_array_kind_tags_byte("[B") else {
            return;
        };
        let graph = self.graph;
        let Some(node) = graph.nodes.get(id as usize) else {
            return;
        };
        // [ctrl, mem, value, other], `int`.
        if node.inputs.len() != 3 + K::Latin1CompareToCI.num_params() || node.ty != IrType::Int {
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
        if crate::ir_stage_reporting() {
            eprintln!(
                "[ir] latin1-compare-to-ci-prefix n{id} {}.{}{}",
                info.class_name, info.method_name, info.descriptor,
            );
        }
        const _: () = assert!(
            cratonvm_types::ARRAY_LENGTH_OFFSET == 8 && cratonvm_types::ARRAY_DATA_OFFSET == 16,
            "the compareToCI prefix bakes ARRAY_LENGTH_OFFSET 8 / ARRAY_DATA_OFFSET 16"
        );
        const KIND_TAGS: u8 = cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8; // Cast: x86-64 disp8
        const _: () = assert!(cratonvm_types::KIND_TAGS_BYTE_OFFSET <= 127);
        let mut decline: Vec<usize> = Vec::new();
        self.note_rcx_written();
        // RCX = value, R11 = other: non-null `byte[]`s.
        self.gp_load_value(RCX, node.inputs[2]);
        self.emit_str_test_r64(RCX);
        decline.push(self.emit_jcc_rel32(0x84)); // JZ
        self.buf.emit(&[0x80, 0x79, KIND_TAGS, byte_array_tag]); // CMP BYTE [RCX+KIND_TAGS], [B tag
        decline.push(self.emit_jcc_rel32(0x85)); // JNE
        self.gp_load_value(R11, node.inputs[3]);
        self.emit_str_test_r64(R11);
        decline.push(self.emit_jcc_rel32(0x84)); // JZ
        self.buf
            .emit(&[0x41, 0x80, 0x7B, KIND_TAGS, byte_array_tag]); // CMP BYTE [R11+KIND_TAGS], [B tag
        decline.push(self.emit_jcc_rel32(0x85)); // JNE
                                                 // No decline past this point. EAX = lim = min(len1, len2).
        self.emit_load_base_disp32(RAX, RCX, 8, false);
        self.emit_load_base_disp32(RDX, R11, 8, false);
        self.buf.emit(&[0x39, 0xD0]); // CMP EAX, EDX
        self.buf.emit(&[0x0F, 0x47, 0xC2]); // CMOVA EAX, EDX
        self.buf.emit(&[0x31, 0xD2]); // XOR EDX, EDX (k)
        let top = self.buf.pos();
        self.buf.emit(&[0x39, 0xC2]); // CMP EDX, EAX
        let equal = self.emit_jcc_rel32(0x83); // JAE: no folded difference
                                               // Eight equal bytes at a time while eight remain.
        self.buf.emit(&[0x41, 0x89, 0xC2]); // MOV R10D, EAX
        self.buf.emit(&[0x41, 0x29, 0xD2]); // SUB R10D, EDX
        self.buf.emit(&[0x41, 0x83, 0xFA, 0x08]); // CMP R10D, 8
        let to_byte = self.emit_jcc_rel32(0x82); // JB
        self.buf.emit(&[0x4C, 0x8B, 0x54, 0x11, 0x10]); // MOV R10, [RCX+RDX+16]
        self.buf.emit(&[0x4D, 0x3B, 0x54, 0x13, 0x10]); // CMP R10, [R11+RDX+16]
        let block_differs = self.emit_jcc_rel32(0x85); // JNE
        self.buf.emit(&[0x83, 0xC2, 0x08]); // ADD EDX, 8
        self.emit_jmp_back_to(top);
        // One byte: equal bytes step on.
        self.patch_rel32_to_here(to_byte);
        self.patch_rel32_to_here(block_differs);
        self.buf.emit(&[0x44, 0x0F, 0xB6, 0x54, 0x11, 0x10]); // MOVZX R10D, BYTE [RCX+RDX+16]
        self.buf.emit(&[0x45, 0x3A, 0x54, 0x13, 0x10]); // CMP R10B, BYTE [R11+RDX+16]
        let same_byte = self.emit_jcc_rel32(0x84); // JE
                                                   // Differing bytes: fold both (lim parked in XMM0).
        self.buf.emit(&[0x66, 0x0F, 0x6E, 0xC0]); // MOVD XMM0, EAX
        self.buf.emit(&[0x41, 0x0F, 0xB6, 0x44, 0x13, 0x10]); // MOVZX EAX, BYTE [R11+RDX+16]
        self.emit_latin1_case_fold(true);
        self.emit_latin1_case_fold(false);
        self.buf.emit(&[0x41, 0x39, 0xC2]); // CMP R10D, EAX
        let folds_differ = self.emit_jcc_rel32(0x85); // JNE
        self.buf.emit(&[0x66, 0x0F, 0x7E, 0xC0]); // MOVD EAX, XMM0
        self.patch_rel32_to_here(same_byte);
        self.buf.emit(&[0xFF, 0xC2]); // INC EDX
        self.emit_jmp_back_to(top);
        // c1 - c2.
        self.patch_rel32_to_here(folds_differ);
        self.buf.emit(&[0x41, 0x29, 0xC2]); // SUB R10D, EAX
        self.buf.emit(&[0x49, 0x63, 0xC2]); // MOVSXD RAX, R10D
        let answered = self.emit_jmp_rel32();
        // len1 - len2.
        self.patch_rel32_to_here(equal);
        self.emit_load_base_disp32(RAX, RCX, 8, false);
        self.buf.emit(&[0x41, 0x2B, 0x43, 0x08]); // SUB EAX, [R11+8]
        self.buf.emit(&[0x48, 0x63, 0xC0]); // MOVSXD RAX, EAX
        self.patch_rel32_to_here(answered);
        self.emit_prefix_result_store(id, slot);
        // A call chain the single-pass backend has no inline form for:
        // acceptance evidence, as `Inflate`.
        crate::ir_evidence::note(crate::ir_evidence::Transform::ScalarIntrinsic);
        let hit = self.emit_jmp_rel32();
        for p in decline {
            self.patch_rel32_to_here(p);
        }
        self.call_prefix_join = Some(hit);
    }

    /// JIT round 21, lane str21b (STRINGS2-21-4,
    /// `CRATONVM_JIT_IR_LATIN1_REGION_MATCHES_CI_PREFIX`, opt-in): the static
    /// `StringLatin1.regionMatchesCI(byte[] value, int toffset, byte[] other,
    /// int ooffset, int len)` -- the tail of every `String.equalsIgnoreCase` /
    /// `regionMatches(true, ..)` over two LATIN1 strings -- answered inline as
    /// a guarded PREFIX of the `Op::Call`. The JDK body walks `len` bytes: an
    /// unequal pair continues only when `CharacterDataLatin1.equalsIgnoreCase`
    /// holds (`A-Z` / `a-z` and `0xC0..=0xDE` / `0xE0..=0xFE` but `0xD7` /
    /// `0xF7` against their other case; `0xB5`, `0xDF`, `0xFF` against
    /// nothing). The pass is [`crate::x64::latin1_region_ci_emit`]: equal
    /// bytes eight at a time, then the byte test. A null or non-`byte[]`
    /// operand, a negative offset or length, and a region past either array's
    /// end DECLINE before anything is read past a header, so the call still
    /// raises (or answers `true` for an empty region) exactly as before. Scratch
    /// RAX, RCX, RDX, R8-R11. No safepoint in the loop (a leaf of at most `len`
    /// steps, like the body it replaces).
    pub(in crate::ir_lower) fn try_emit_ir_latin1_region_matches_ci_prefix(
        &mut self,
        id: NodeId,
        info: &JitInvokeInfo,
    ) {
        use crate::IrStringSearchKind as K;
        if !K::Latin1RegionMatchesCI.admits_invoke_kind(info.invoke_kind) {
            self.note_string_prefix_declined(
                id,
                info,
                &format!("invoke_kind={}", info.invoke_kind),
            );
            return;
        }
        let Some(byte_array_tag) = cratonvm_types::primitive_array_kind_tags_byte("[B") else {
            return;
        };
        let graph = self.graph;
        let Some(node) = graph.nodes.get(id as usize) else {
            return;
        };
        // [ctrl, mem, value, toffset, other, ooffset, len], `boolean` as `Int`.
        if node.inputs.len() != 3 + K::Latin1RegionMatchesCI.num_params() || node.ty != IrType::Int
        {
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
        if crate::ir_stage_reporting() {
            eprintln!(
                "[ir] latin1-region-matches-ci-prefix n{id} {}.{}{}",
                info.class_name, info.method_name, info.descriptor,
            );
        }
        const KIND_TAGS: u8 = cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8; // Cast: x86-64 disp8
        const _: () = assert!(cratonvm_types::KIND_TAGS_BYTE_OFFSET <= 127);
        let (value, toffset, other, ooffset, len) = (
            node.inputs[2],
            node.inputs[3],
            node.inputs[4],
            node.inputs[5],
            node.inputs[6],
        );
        let mut decline: Vec<usize> = Vec::new();
        self.note_rcx_written();
        // RCX = value: a non-null `byte[]`.
        self.gp_load_value(RCX, value);
        self.emit_str_test_r64(RCX);
        decline.push(self.emit_jcc_rel32(0x84)); // JZ
        self.buf.emit(&[0x80, 0x79, KIND_TAGS, byte_array_tag]); // CMP BYTE [RCX+KIND_TAGS], [B tag
        decline.push(self.emit_jcc_rel32(0x85)); // JNE
                                                 // RDX = other: a non-null `byte[]`.
        self.gp_load_value(RDX, other);
        self.emit_str_test_r64(RDX);
        decline.push(self.emit_jcc_rel32(0x84)); // JZ
        self.buf.emit(&[0x80, 0x7A, KIND_TAGS, byte_array_tag]); // CMP BYTE [RDX+KIND_TAGS], [B tag
        decline.push(self.emit_jcc_rel32(0x85)); // JNE
                                                 // R11 = toffset, RAX = ooffset, R10 = len (low halves; the pass
                                                 // zero-extends them, so a negative int is above every length).
        self.gp_load_value(R11, toffset);
        self.gp_load_value(RAX, ooffset);
        self.gp_load_value(R10, len);
        crate::x64::latin1_region_ci_emit::emit_latin1_region_matches_ci(
            &mut self.buf,
            &mut decline,
        );
        // EAX = 1 / 0.
        self.emit_prefix_result_store(id, slot);
        // A loop the single-pass backend has no inline form for: acceptance
        // evidence, as `Latin1CompareToCI`.
        crate::ir_evidence::note(crate::ir_evidence::Transform::ScalarIntrinsic);
        let hit = self.emit_jmp_rel32();
        for p in decline {
            self.patch_rel32_to_here(p);
        }
        self.call_prefix_join = Some(hit);
    }

    /// The Latin-1 case fold of [`Self::try_emit_ir_latin1_compare_to_ci_prefix`],
    /// in place on R10D (`r10`) or EAX holding a byte `0..=255`:
    /// `Character.toLowerCase((char) CharacterDataLatin1.toUpperCase(b))`.
    pub(in crate::ir_lower) fn emit_latin1_case_fold(&mut self, r10: bool) {
        // `CMP reg, imm32`.
        fn cmp(buf: &mut Vec<u8>, r10: bool, imm: u32) {
            if r10 {
                buf.extend_from_slice(&[0x41, 0x81, 0xFA]);
            } else {
                buf.push(0x3D);
            }
            buf.extend_from_slice(&imm.to_le_bytes());
        }
        let mut done: Vec<usize> = Vec::new();
        let mut code: Vec<u8> = Vec::new();
        cmp(&mut code, r10, 0xB5);
        self.buf.emit(&code);
        let not_micro = self.emit_jcc_rel32(0x85); // JNE
        if r10 {
            self.buf.emit(&[0x41, 0xBA]); // MOV R10D, imm32
        } else {
            self.buf.emit(&[0xB8]); // MOV EAX, imm32
        }
        self.buf.emit(&0x3BC_u32.to_le_bytes());
        done.push(self.emit_jmp_rel32());
        self.patch_rel32_to_here(not_micro);
        // (bound, Jcc to `done`), in order; the `Z` row's JBE goes to the add.
        let mut upper_ascii: Option<usize> = None;
        for (bound, cc) in [
            (u32::from(b'A'), 0x82u8), // JB: below `A`
            (u32::from(b'Z'), 0x86),   // JBE: `A-Z` -> the add
            (0xC0, 0x82),              // JB: `[\]^_` .. `0xBF`
            (0xDE, 0x87),              // JA: `0xDF..=0xFF`
            (0xD7, 0x84),              // JE: MULTIPLICATION SIGN
        ] {
            code.clear();
            cmp(&mut code, r10, bound);
            self.buf.emit(&code);
            let p = self.emit_jcc_rel32(cc);
            if cc == 0x86 {
                upper_ascii = Some(p);
            } else {
                done.push(p);
            }
        }
        if let Some(p) = upper_ascii {
            self.patch_rel32_to_here(p);
        }
        if r10 {
            self.buf.emit(&[0x41, 0x83, 0xC2, 0x20]); // ADD R10D, 0x20
        } else {
            self.buf.emit(&[0x83, 0xC0, 0x20]); // ADD EAX, 0x20
        }
        for p in done {
            self.patch_rel32_to_here(p);
        }
    }

    /// JIT round 21, lane strings2 (STRINGS21-1,
    /// `CRATONVM_JIT_IR_LATIN1_CASE_MAP_PREFIX`): the statics
    /// `StringLatin1.toLowerCase` / `toUpperCase(String str, byte[] value,
    /// Locale locale)` answered inline as a guarded PREFIX of the `Op::Call`,
    /// pass by pass as `x64::latin1_case_emit` documents them against the JDK
    /// 25 bodies:
    ///
    /// 1. a null `locale` (the body's NPE) or a null / non-`byte[]` `value`
    ///    declines;
    /// 2. the scan: no byte the fold changes answers `str` itself (the JDK's
    ///    `first == len` identity); the upper case declines `0xB5` / `0xDF` /
    ///    `0xFF` (answered outside Latin-1) unless its switch answers it:
    ///    `0xDF` alone a longer LATIN1 String (`..._UPPER_SHARP_S`), any
    ///    `0xB5` / `0xFF` a fresh UTF16 String (`..._UPPER_UTF16`, round 21
    ///    lane latin1up);
    /// 3. a changed byte the `tr` / `az` / `lt` rules treat differently asks
    ///    the language ([`crate::LocaleLanguageLayout`]); one of the three, or
    ///    a language the screen cannot read (no layout half, a legacy
    ///    instance), declines;
    /// 4. a fresh `byte[len]` + LATIN1 String in one TLAB bump, the payload
    ///    copied (`x64::concat_emit::emit_ir_sb_to_string`, the inline
    ///    `StringBuilder.toString()`'s allocation, with its collector arm and
    ///    every decline before the first store), then folded in place -- the
    ///    array is private until the String is returned, and the fold stores
    ///    bytes only, so no barrier and no safepoint lies between.
    ///
    /// When the allocation cannot be planned for this compile (`chars_ir_site`
    /// refuses: no TLAB offsets, a collector premise, the inline-TLAB levers)
    /// passes 1-2 still answer the unchanged case and step 4 declines. Scratch
    /// RAX, RCX, RDX, R8-R11, XMM0, all clobbered by the call it fronts. The
    /// hit edge stores the String and jumps to [`Self::call_prefix_join`].
    pub(in crate::ir_lower) fn try_emit_ir_latin1_case_map_prefix(
        &mut self,
        id: NodeId,
        info: &JitInvokeInfo,
        kind: crate::IrStringSearchKind,
        layout: crate::StringFieldLayout,
    ) {
        use crate::x64::concat_emit as ce;
        use crate::x64::latin1_case_emit as lc;
        const R8: u8 = 8;
        const R9: u8 = 9;
        let lower = kind == crate::IrStringSearchKind::Latin1ToLowerCase;
        if !kind.admits_invoke_kind(info.invoke_kind)
            || !layout.has_coder
            || layout.string_class_id == 0
            || layout.value_compact_is_narrow
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
        let Some(byte_array_tag) = cratonvm_types::primitive_array_kind_tags_byte("[B") else {
            return;
        };
        let graph = self.graph;
        let Some(node) = graph.nodes.get(id as usize) else {
            return;
        };
        // [ctrl, mem, str, value, locale] -> a String.
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
        let (str_n, value_n, locale_n) = (node.inputs[2], node.inputs[3], node.inputs[4]);
        let frame = ce::ConcatIrFrame {
            tls_disp: crate::x64::jit_thread_tls_disp(),
            thread_slot_off: self.shadow_thread_slot_off,
            cursor_off: self.tlab_cursor_off,
            end_off: self.tlab_end_off,
            class_id_off: self.concat_class_id_off,
            g1_table: self.g1_barrier_addr,
            region_bounds_addr: self.concat_region_bounds_addr,
        };
        let site = ce::chars_ir_site(
            Some(layout),
            self.direct_helpers.compact_strings_flag_addr(),
            frame,
        );
        if crate::ir_stage_reporting() {
            eprintln!(
                "[ir] latin1-case-map-prefix n{id} {}.{}{} lower={lower} alloc={} locale_layout={}",
                info.class_name,
                info.method_name,
                info.descriptor,
                site.as_ref().map_or_else(|why| *why, |_| "inline"),
                layout.locale_language.is_some(),
            );
        }
        // (`x64::latin1_case_emit` pins the array header offsets it bakes.)
        const KIND_TAGS: u8 = cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8; // Cast: x86-64 disp8
        let mut decline: Vec<usize> = Vec::new();
        self.note_rcx_written();
        // The Locale / BaseLocale offsets are baked: a layout REPLACEMENT
        // declines (the String's are guarded again by the allocation).
        if let Some(p) = self.emit_layout_epoch_guard() {
            decline.push(p);
        }
        // XMM0 = locale, non-null (the body throws the NPE first).
        self.gp_load_value(RAX, locale_n);
        self.emit_str_test_r64(RAX);
        decline.push(self.emit_jcc_rel32(0x84)); // JZ
        self.buf.emit(&[0x66, 0x48, 0x0F, 0x6E, 0xC0]); // MOVQ XMM0, RAX
        self.gp_load_value(RCX, value_n); // RCX = value: a non-null `byte[]`
        self.emit_str_test_r64(RCX);
        decline.push(self.emit_jcc_rel32(0x84)); // JZ
        self.buf.emit(&[0x80, 0x79, KIND_TAGS, byte_array_tag]); // CMP BYTE [RCX+KIND_TAGS], [B tag
        decline.push(self.emit_jcc_rel32(0x85)); // JNE
        self.gp_load_value(R9, str_n); // R9 = str
        self.emit_load_base_disp32(R10, RCX, 8, false); // R10D = len

        // JIT round 21, lane ci: the 16-byte SSE2 scan / fold
        // (`CRATONVM_JIT_IR_LATIN1_CASE_MAP_SIMD`; it clobbers XMM0, so the
        // locale is re-read from its home for the screen) and the upper case's
        // inline `0xDF` -> "SS" (`CRATONVM_JIT_IR_LATIN1_UPPER_SHARP_S`).
        let simd = crate::ir_latin1_case_map_simd_enabled();
        let sharp_s = !lower && site.is_ok() && crate::ir_latin1_upper_sharp_s_enabled();
        // JIT round 21, lane latin1up (`CRATONVM_JIT_IR_LATIN1_UPPER_UTF16`):
        // a `0xB5` / `0xFF` answers a fresh UTF16 String inline (scan bit 3).
        let utf16 = !lower && site.is_ok() && crate::ir_latin1_upper_utf16_enabled();
        if utf16 {
            lc::emit_latin1_case_scan_wide(&mut self.buf, lower, simd, sharp_s, true, &mut decline);
        } else {
            lc::emit_latin1_case_scan_with(&mut self.buf, lower, simd, sharp_s, &mut decline);
        }
        self.buf.emit(&[0x41, 0xF6, 0xC0, 0x01]); // TEST R8B, 1
        let unchanged = self.emit_jcc_rel32(0x84); // JZ: answer `str`
        match site {
            Ok(site) => {
                self.buf.emit(&[0x45, 0x89, 0xC3]); // MOV R11D, R8D: the scan's bits
                self.buf.emit(&[0x41, 0xF6, 0xC0, 0x02]); // TEST R8B, 2
                let plain = self.emit_jcc_rel32(0x84); // JZ: no locale-sensitive byte
                match layout.locale_language {
                    Some(loc) => {
                        if simd {
                            self.gp_load_value(R8, locale_n);
                        } else {
                            self.buf.emit(&[0x66, 0x49, 0x0F, 0x7E, 0xC0]); // MOVQ R8, XMM0
                        }
                        let buf = &mut self.buf;
                        lc::emit_locale_language_screen(buf, &loc, &layout, &mut decline);
                    }
                    None => decline.push(self.emit_jmp_rel32()),
                }
                self.patch_rel32_to_here(plain);
                let mut folded: Option<usize> = None;
                let mut widened: Option<usize> = None;
                if utf16 {
                    // `toUpperCaseEx(.., false)` + `StringUTF16.newString`: a
                    // char above 0xFF never compresses, so `coder = UTF16`
                    // over `2 * (len + count(0xDF))` bytes, always a new String.
                    self.buf.emit(&[0x41, 0xF6, 0xC3, 0x08]); // TEST R11B, 8: 0xB5 / 0xFF
                    let narrow = self.emit_jcc_rel32(0x84); // JZ
                    self.buf.emit(&[0x66, 0x48, 0x0F, 0x6E, 0xC1]); // MOVQ XMM0, RCX: value
                    lc::emit_count_sharp_s(&mut self.buf);
                    // 64-bit: the inline cap must see `2 * units` past 2^32.
                    self.buf.emit(&[0x49, 0x8D, 0x0C, 0x02]); // LEA RCX, [R10+RAX]
                    self.buf.emit(&[0x48, 0x01, 0xC9]); // ADD RCX, RCX: bytes
                    ce::emit_ir_utf16_string_alloc(&mut self.buf, &site, &mut decline);
                    self.emit_load_base_disp32(R11, RAX, layout.value_compact_offset, true);
                    self.buf.emit(&[0x66, 0x48, 0x0F, 0x7E, 0xC2]); // MOVQ RDX, XMM0
                    lc::emit_copy_upper_utf16(&mut self.buf);
                    widened = Some(self.emit_jmp_rel32());
                    self.patch_rel32_to_here(narrow);
                    if !sharp_s {
                        // The `wide` scan reports an 0xDF (bit 2) instead of
                        // declining it; without the "SS" arm the call answers.
                        self.buf.emit(&[0x41, 0xF6, 0xC3, 0x04]); // TEST R11B, 4
                        decline.push(self.emit_jcc_rel32(0x85)); // JNZ
                    }
                }
                if sharp_s {
                    self.buf.emit(&[0x41, 0xF6, 0xC3, 0x04]); // TEST R11B, 4: an 0xDF
                    let no_sharp_s = self.emit_jcc_rel32(0x84); // JZ

                    // len + count(0xDF) bytes, written by the expanding copy.
                    self.buf.emit(&[0x66, 0x48, 0x0F, 0x6E, 0xC1]); // MOVQ XMM0, RCX: value
                    lc::emit_count_sharp_s(&mut self.buf);
                    self.buf.emit(&[0x41, 0x8D, 0x0C, 0x02]); // LEA ECX, [R10+RAX]
                    ce::emit_ir_latin1_string_alloc(&mut self.buf, &site, &mut decline, false);
                    self.emit_load_base_disp32(R11, RAX, layout.value_compact_offset, true);
                    self.buf.emit(&[0x66, 0x48, 0x0F, 0x7E, 0xC2]); // MOVQ RDX, XMM0
                    lc::emit_copy_expanding_sharp_s(&mut self.buf);
                    lc::emit_latin1_case_map_in_place_with(&mut self.buf, lower, simd);
                    folded = Some(self.emit_jmp_rel32());
                    self.patch_rel32_to_here(no_sharp_s);
                }
                // RDX = value, ECX = len: the builder copy's inputs.
                self.buf.emit(&[0x48, 0x89, 0xCA]); // MOV RDX, RCX
                self.buf.emit(&[0x44, 0x89, 0xD1]); // MOV ECX, R10D
                ce::emit_ir_sb_to_string(&mut self.buf, &site, &mut decline);
                // RAX = the fresh String (COMPACT, `value` at the layout's
                // compact offset, which the allocation plan pinned).
                self.emit_load_base_disp32(R11, RAX, layout.value_compact_offset, true);
                lc::emit_latin1_case_map_in_place_with(&mut self.buf, lower, simd);
                if let Some(p) = folded {
                    self.patch_rel32_to_here(p);
                }
                if let Some(p) = widened {
                    self.patch_rel32_to_here(p);
                }
            }
            Err(_) => decline.push(self.emit_jmp_rel32()),
        }
        let store = self.emit_jmp_rel32();
        self.patch_rel32_to_here(unchanged);
        self.buf.emit(&[0x4C, 0x89, 0xC8]); // MOV RAX, R9: `str` itself
        self.patch_rel32_to_here(store);
        self.emit_integer_value_of_result_store(id, slot);
        // A call chain the single-pass backend has no inline form for:
        // acceptance evidence, as `Latin1CompareToCI`.
        crate::ir_evidence::note(crate::ir_evidence::Transform::ScalarIntrinsic);
        let hit = self.emit_jmp_rel32();
        for p in decline {
            self.patch_rel32_to_here(p);
        }
        self.call_prefix_join = Some(hit);
    }

    /// JIT r20 wave 12, lane qStr (book `jit-r20-nio-proposals-w6-L2.md`,
    /// W6-L2-2; `CRATONVM_JIT_IR_STRING_CHARS_PREFIX`): the three `char[]`
    /// copies the optimizing tier answers inline as a guarded PREFIX of the
    /// `Op::Call`, each with an inline TLAB allocation
    /// (`x64::concat_emit::emit_ir_to_char_array` /
    /// `emit_ir_string_from_chars`, the inline concat's bump, collector arm and
    /// header stores):
    ///
    /// * `String.toCharArray()` -- a fresh `char[length()]`, the LATIN1
    ///   payload inflated or the UTF16 one copied;
    /// * the static `String.valueOf(char[])` / `copyValueOf(char[])` -- a
    ///   fresh String (and its `byte[]`) whose coder is the JDK's
    ///   `COMPACT_STRINGS` + `StringUTF16.compress` answer;
    /// * `new String(char[])` at its `invokespecial` -- the receiver's
    ///   `value` and `coder`, by the same rule.
    ///
    /// Same contract with the `Op::Call` arm as
    /// [`Self::try_emit_ir_string_search_prefix`], with the inline concat's
    /// scratch set (RAX, RCX, RDX, R8-R11, XMM0/XMM1: caller-saved, none an
    /// `IR_LOWER_LS_GPRS` home, all clobbered by the call it fronts). The hit
    /// edge stores the result (none for `<init>`) and jumps to
    /// [`Self::call_prefix_join`]; it makes no call, polls nothing and cannot
    /// throw, so it owes no map and no deopt point. Every decline -- each
    /// listed on the two emitters -- precedes the first store and lands on the
    /// unchanged call, which answers or throws exactly as before.
    pub(in crate::ir_lower) fn try_emit_ir_string_chars_prefix(
        &mut self,
        id: NodeId,
        info: &JitInvokeInfo,
        kind: crate::IrStringSearchKind,
        layout: crate::StringFieldLayout,
    ) {
        use crate::x64::concat_emit as ce;
        use crate::IrStringSearchKind as K;
        // `emit_ir_string_from_chars`' receiver register (no module constant).
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
        // [ctrl, mem, receiver-or-array(, array)]: a `char[]` / String result,
        // or void for the constructor.
        let want_ty = if kind == K::CharsInit {
            IrType::Void
        } else {
            IrType::Ref
        };
        if node.inputs.len() != 3 + kind.num_params() || node.ty != want_ty {
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
        let want_ty = if kind == K::CharsInit {
            IrType::Void
        } else {
            node.ty
        };
        let Some(slot) = self.prefix_result_slot(id, want_ty) else {
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
                "[ir] string-chars-prefix n{id} {}.{}{} kind={kind:?} string_class_id={}",
                info.class_name, info.method_name, info.descriptor, layout.string_class_id,
            );
        }
        let mut decline: Vec<usize> = Vec::new();
        // RCX: the epoch guard's fallback form and the field loads write it.
        self.note_rcx_written();
        match kind {
            K::ToCharArray => {
                // A layout REPLACEMENT invalidates the baked field offsets.
                if let Some(p) = self.emit_layout_epoch_guard() {
                    decline.push(p);
                }
                // RAX = receiver; null declines (the call raises the NPE).
                self.gp_load_value(RAX, node.inputs[2]);
                self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
                decline.push(self.emit_jcc_rel32(0x84)); // JZ
                self.emit_str_cmp_class_id(RAX, layout.string_class_id);
                decline.push(self.emit_jcc_rel32(0x85)); // JNE
                                                         // RDX = value, ECX = coder.
                self.emit_str_value_and_coder(&layout, RAX, RDX, Some(RCX));
                ce::emit_ir_to_char_array(&mut self.buf, &site, &mut decline);
            }
            K::CharsValueOf => {
                self.gp_load_value(RDX, node.inputs[2]);
                ce::emit_ir_string_from_chars(&mut self.buf, &site, &mut decline, false);
            }
            K::CharsInit => {
                self.gp_load_value(R9, node.inputs[2]);
                self.gp_load_value(RDX, node.inputs[3]);
                ce::emit_ir_string_from_chars(&mut self.buf, &site, &mut decline, true);
            }
            // `is_chars_copy` admitted only the three above; nothing emitted.
            _ => return,
        }
        if kind != K::CharsInit {
            self.emit_prefix_result_store(id, slot);
        }
        // A call chain the single-pass backend has no inline form for, lowered
        // as straight-line code: acceptance evidence, as the `getChars` prefix.
        crate::ir_evidence::note(crate::ir_evidence::Transform::ScalarIntrinsic);
        let hit = self.emit_jmp_rel32();
        for p in decline {
            self.patch_rel32_to_here(p);
        }
        self.call_prefix_join = Some(hit);
    }
}

fn int_expr_bounds(graph: &crate::ir::Graph, id: NodeId, depth: u8) -> Option<(i64, i64)> {
    use crate::ir::{IrType, Op};
    if depth > 5 {
        return None;
    }
    let node = graph.nodes.get(id as usize)?;
    if node.ty != IrType::Int {
        return None;
    }
    match node.op {
        Op::Const(c) => {
            let v = i64::from(c as i32);
            Some((v, v))
        }
        Op::And if node.inputs.len() == 2 => {
            let mask_of = |nid: NodeId| match graph.nodes.get(nid as usize).map(|n| (&n.op, n.ty)) {
                Some((Op::Const(m), IrType::Int)) => {
                    let m32 = *m as i32;
                    (m32 >= 0).then_some(i64::from(m32))
                }
                _ => None,
            };
            mask_of(node.inputs[1])
                .or_else(|| mask_of(node.inputs[0]))
                .map(|m| (0, m))
        }
        Op::Add if node.inputs.len() == 2 => {
            let (al, ah) = int_expr_bounds(graph, node.inputs[0], depth + 1)?;
            let (bl, bh) = int_expr_bounds(graph, node.inputs[1], depth + 1)?;
            let lo = al.checked_add(bl)?;
            let hi = ah.checked_add(bh)?;
            if lo >= i64::from(i32::MIN) && hi <= i64::from(i32::MAX) {
                Some((lo, hi))
            } else {
                None
            }
        }
        Op::UShr if node.inputs.len() == 2 => {
            let sh = match graph
                .nodes
                .get(node.inputs[1] as usize)
                .map(|n| (&n.op, n.ty))
            {
                Some((Op::Const(s), IrType::Int)) => (*s as u32) & 31,
                _ => return None,
            };
            (sh >= 24).then(|| (0, i64::from(u32::MAX >> sh)))
        }
        _ => None,
    }
}

fn int_expr_in_latin1_range(graph: &crate::ir::Graph, id: NodeId, depth: u8) -> bool {
    int_expr_bounds(graph, id, depth).is_some_and(|(lo, hi)| lo >= 0 && hi <= 0xFF)
}
