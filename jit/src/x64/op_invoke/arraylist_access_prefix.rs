#[allow(unused_imports)]
use super::*;

impl Compiler {
    /// ARRAYLIST_ACCESS (round 9 wave 8, `arraylist8`): emit the inline
    /// `ArrayList.get(int)` / `size()` fast path as a guarded PREFIX of the
    /// site's ordinary virtual/interface dispatch, and return the hit edge's
    /// `JMP rel32` patch. `None` emits nothing.
    ///
    /// Page: `perf-collection-natives-cost-a-generic-native-dispatch-per-call`.
    ///
    /// # Contract with the caller
    ///
    /// Called at the top of the dispatch arm, after `info_ref`/`n` are known and
    /// before a single byte of the dispatch is emitted. It only PEEKS at the
    /// operand stack -- the receiver and the index stay where the dispatch will
    /// pop them -- and it only writes RAX, RCX, RDX, R8, R9 (and R11 inside the
    /// layout epoch guard's fallback form), all of which the dispatch below
    /// overwrites before reading: after the walk's `flush_scratch_registers` no
    /// operand lives in any of them. Every decline edge is patched to the byte
    /// after the prefix, i.e. the first byte of the UNCHANGED dispatch, which
    /// then runs the registered native exactly as today (MIC/PIC and the
    /// site-cached native leaf included). The hit edge must be patched at the
    /// dispatch arm's result push, with the answer in RAX.
    ///
    /// # What the fast path answers
    ///
    /// Exactly what `native_al_get` / `native_al_size`'s wave-3 fast path
    /// (`al_fast_state` plus an in-range index) answers, and nothing else:
    ///
    /// ```text
    ///   [layout epoch guard]                           ; JNE decline
    ///   RAX = recv ; TEST RAX,RAX ; JZ decline         ; the NPE is the native's
    ///   CMP BYTE  [RAX+KIND_TAGS], Object ; JNE decline ; an array carries its
    ///                                                   ; COMPONENT's id at +0
    ///   CMP DWORD [RAX+class_id], ArrayList ; JNE decline ; exact: no subclass
    ///   TEST BYTE [RAX+GC_FLAGS], COMPACT ; JZ legacy
    ///     R8D = [RAX+size_c] ; RDX = [RAX+data_c] ; JMP have
    ///   legacy:
    ///     CMP DWORD [RAX+NUM_SLOTS], min ; JB decline   ; both cells present
    ///     CMP DWORD [RAX+size_tag], Int  ; JNE decline
    ///     CMP DWORD [RAX+data_tag], Object ; JNE decline
    ///     R8D = [RAX+size_l] ; RDX = [RAX+data_l]
    ///   have:
    ///   TEST R8D,R8D ; JS decline                       ; size >= 0
    ///   TEST RDX,RDX ; JZ decline                       ; elementData != null
    ///   CMP BYTE [RDX+KIND_TAGS], Array|Reference ; JNE decline
    ///   R9D = elementData.length
    ///   get:  ECX = index ; CMP ECX,R8D ; JAE decline ; CMP ECX,R9D ; JAE decline
    ///   CMP R8D,R9D ; JAE nomark                        ; `view_source_in`:
    ///   CMP QWORD [RDX+R9*8+DATA-8],0 ; JNE decline     ; a trailing non-null slot
    ///   nomark:
    ///   get:  RAX = [RDX+RCX*8+DATA]    size: MOVSXD RAX,R8D
    ///   JMP hit
    ///   decline:  <the dispatch>
    /// ```
    ///
    /// The index compares are unsigned, so a negative index declines too, and
    /// the IndexOutOfBoundsException is the native's -- this path never builds
    /// an exception and never deopts: an out-of-range index is ordinary control
    /// flow in real code, and a deopt would re-run the method in the
    /// interpreter every time.
    ///
    /// The reference loads are raw 8-byte loads, the representation the inline
    /// `aaload` reads; `ArrayListFieldLayout::new` refuses narrow oops and an
    /// armed ZGC read barrier, and this re-checks the barrier at emission, the
    /// refusal the inline `getfield` takes.
    ///
    /// # Why a prefix and not a `JitDirectCall` sentinel
    ///
    /// A sentinel must be registered at BOTH compile doors, with a
    /// `JitInvokeInfo` minted for its decline edge, or the method drops to the
    /// interpreter (`string_intrinsic_declines_to_a_call`'s history), and it
    /// would take the site off its MIC/PIC. As a prefix the site keeps its
    /// ordinary registration and its inline caches, the optimizing tier (which
    /// never runs this emitter) is untouched, and there is nothing for
    /// registration and emission to disagree about: every refusal is a `None`
    /// here, which emits the plain dispatch.
    pub(in crate::x64::op_invoke) fn try_emit_arraylist_access_prefix(
        &mut self,
        pc: usize,
        info: &crate::JitInvokeInfo,
    ) -> Option<usize> {
        let layout = self.string_layout.and_then(|l| l.array_list)?;
        let (entry, num_params, _ret, guard) = crate::try_resolve_arraylist_intrinsic(
            info.class_name,
            info.method_name,
            info.descriptor,
            Some(layout),
        )?;
        // Virtual (0) and interface (2) only: `invokespecial` of `get` names a
        // statically bound body, which is not the question this answers.
        if !matches!(info.invoke_kind, 0 | 2) || guard == 0 {
            return None;
        }
        if crate::x64::zgc_read_barrier_blocks_inline_fields() {
            return None;
        }
        let is_get = entry == crate::JitIntrinsic::ArrayListGet.as_entry();
        let depth = num_params + 1;
        if info.num_jit_args != depth || self.stack.len() < depth {
            return None;
        }
        let recv_slot = *self.stack.get(self.stack.len() - depth)?;
        let index_slot = if is_get {
            Some(*self.stack.last()?)
        } else {
            None
        };
        if matches!(recv_slot, StackSlot::Xmm(..)) || matches!(index_slot, Some(StackSlot::Xmm(..)))
        {
            return None;
        }
        // The prefix writes R8/R9, the deferred operand cache's registers. The
        // walk flushed them before this arm, so this never fires; it is here so
        // a future caller that did not flush gets the plain dispatch rather
        // than a clobbered operand.
        if self
            .stack
            .iter()
            .any(|slot| matches!(slot, StackSlot::Scratch(..)))
        {
            return None;
        }
        if false {
            eprintln!(
                "[cratonvm-jitc] arraylist-intrinsic {} {}.{}{} @pc={pc} guard_class_id={guard}",
                self.method_label, info.class_name, info.method_name, info.descriptor,
            );
        }

        let mut decline: Vec<usize> = Vec::new();

        // The baked compact offsets are a compile-time claim about ArrayList's
        // layout; a layout REPLACEMENT bumps the process epoch and this site
        // then declines for good. Emitted first: its fallback form clobbers
        // RCX and R11.
        if jit_sp_field_layout_guard_enabled() {
            if let Some(p) = self.emit_layout_epoch_guard() {
                decline.push(p);
            }
        }

        // RAX = receiver; null declines (the native reports the NPE).
        self.load_slot_to_reg(RAX, recv_slot);
        self.emit_test_r64_r64(RAX);
        decline.push(self.emit_jcc_rel32_patch(0x84)); // JZ
                                                       // A plain object only: a reference array's header carries its
                                                       // COMPONENT's class id at +0, so an `ArrayList[]` would pass the id
                                                       // compare below.
                                                       // TEST the kind bits only: a hashed compact instance carries identity-
                                                       // hash bits in the rest of this byte.
        self.emit_test_mem8_imm8(
            RAX,
            cratonvm_types::KIND_TAGS_BYTE_OFFSET as i32, // Cast: small header constant
            cratonvm_types::KIND_TAG_BYTE_MASK,
        );
        decline.push(self.emit_jcc_rel32_patch(0x85)); // JNE
                                                       // CMP DWORD [RAX + class_id_off], ArrayList -- `81 /7 id` with
                                                       // ModRM mod=10 rm=RAX, the form the inline `instanceof` uses.
        let cid_off = self.helpers.class_id_offset_in_obj as i32; // Cast: x86-64 disp32
        self.buf.emit(&[0x81, 0xB8]);
        self.buf.emit(&cid_off.to_le_bytes());
        self.buf.emit(&guard.to_le_bytes());
        decline.push(self.emit_jcc_rel32_patch(0x85)); // JNE

        // One compact/legacy branch for both fields.
        self.emit_test_mem8_imm8(
            RAX,
            cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32, // Cast: small header constant
            cratonvm_types::GC_FLAG_COMPACT,
        );
        let legacy = self.emit_jcc_rel32_patch(0x84); // JZ -> legacy
                                                      // COMPACT: `size` is a bare 4-byte int, `elementData` a bare pointer.
        self.emit_mov_r32_mem_disp32(R8, RAX, layout.size_compact_offset);
        self.emit_mov_r64_mem_disp32(RDX, RAX, layout.data_compact_offset);
        let have = self.emit_jmp_rel32_patch();

        // LEGACY: 16-byte `Value` cells. Both cells must exist, and each must
        // carry the tag its payload is read as.
        self.patch_rel32_to_here(legacy);
        // CMP DWORD [RAX + NUM_SLOTS_OFFSET], min_num_slots ; JB decline
        self.buf.emit(&[0x81, 0xB8]);
        self.buf
            .emit(&(cratonvm_types::NUM_SLOTS_OFFSET as i32).to_le_bytes()); // Cast: small header constant
        self.buf.emit(&layout.min_num_slots.to_le_bytes());
        decline.push(self.emit_jcc_rel32_patch(0x82)); // JB
                                                       // CMP DWORD [RAX + size_tag], 0 (`Value::Int`) ; JNE decline
        self.buf.emit(&[0x81, 0xB8]);
        self.buf.emit(&layout.size_legacy_tag_offset.to_le_bytes());
        self.buf.emit(&0u32.to_le_bytes());
        decline.push(self.emit_jcc_rel32_patch(0x85)); // JNE
                                                       // CMP DWORD [RAX + data_tag], FIELD_CELL_TAG_OBJECT ; JNE decline
        self.buf.emit(&[0x81, 0xB8]);
        self.buf.emit(&layout.data_legacy_tag_offset.to_le_bytes());
        self.buf
            .emit(&cratonvm_types::FIELD_CELL_TAG_OBJECT.to_le_bytes());
        decline.push(self.emit_jcc_rel32_patch(0x85)); // JNE
        self.emit_mov_r32_mem_disp32(R8, RAX, layout.size_legacy_offset);
        self.emit_mov_r64_mem_disp32(RDX, RAX, layout.data_legacy_offset);

        self.patch_rel32_to_here(have);
        // TEST R8D,R8D ; JS decline -- `al_fast_state` requires size >= 0.
        self.buf.emit(&[0x45, 0x85, 0xC0]);
        decline.push(self.emit_jcc_rel32_patch(0x88)); // JS
                                                       // elementData must be a live REFERENCE array.
        self.emit_test_r64_r64(RDX);
        decline.push(self.emit_jcc_rel32_patch(0x84)); // JZ
        let ref_array_tags = (cratonvm_types::ObjectKind::Array as u8)
            | ((cratonvm_types::ArrayElementType::Reference as u8) << 2);
        self.emit_cmp_mem8_imm8(
            RDX,
            cratonvm_types::KIND_TAGS_BYTE_OFFSET as i32, // Cast: small header constant
            ref_array_tags,
        );
        decline.push(self.emit_jcc_rel32_patch(0x85)); // JNE
                                                       // R9D = elementData.length (zero-extends into R9).
        self.emit_mov_r32_mem_disp32(
            R9,
            RDX,
            cratonvm_types::ARRAY_LENGTH_OFFSET as i32, // Cast: small header constant
        );
        if let Some(idx) = index_slot {
            self.load_slot_to_reg(RCX, idx);
            // MOV ECX,ECX: the element load below scales the FULL register,
            // so clear whatever the upper half of the int slot holds.
            self.buf.emit(&[0x89, 0xC9]);
            // Unsigned: a negative index is a huge one and declines.
            self.emit_cmp_r32_r32(RCX, R8); // index vs size
            decline.push(self.emit_jcc_rel32_patch(0x83)); // JAE
            self.emit_cmp_r32_r32(RCX, R9); // index vs length: never an unchecked read
            decline.push(self.emit_jcc_rel32_patch(0x83)); // JAE
        }
        // `view_source_in`, verbatim: with spare capacity, a non-null LAST slot
        // names a source map this list is a stale view of, which the native
        // must resync first.
        self.emit_cmp_r32_r32(R8, R9); // size vs length
        let no_marker = self.emit_jcc_rel32_patch(0x83); // JAE -> no spare capacity
                                                         // CMP QWORD [RDX + R9*8 + (ARRAY_DATA_OFFSET - 8)], 0
                                                         //   REX.W|REX.X, 83 /7 ib, ModRM mod=01 rm=SIB, SIB *8 idx=R9 base=RDX.
        self.buf.emit(&[
            0x4A,
            0x83,
            0x7C,
            0xCA,
            (cratonvm_types::ARRAY_DATA_OFFSET - cratonvm_types::REF_ELEMENT_SIZE) as u8, // Cast: x86-64 disp8
            0x00,
        ]);
        decline.push(self.emit_jcc_rel32_patch(0x85)); // JNE
        self.patch_rel32_to_here(no_marker);

        if is_get {
            // MOV RAX, [RDX + RCX*8 + ARRAY_DATA_OFFSET]
            self.buf.emit(&[
                0x48,
                0x8B,
                0x44,
                0xCA,
                cratonvm_types::ARRAY_DATA_OFFSET as u8, // Cast: x86-64 disp8
            ]);
        } else {
            // MOVSXD RAX, R8D -- `size()` into the 64-bit operand slot.
            self.buf.emit(&[0x49, 0x63, 0xC0]);
        }
        let hit = self.emit_jmp_rel32_patch();

        // ---- decline: the first byte of the unchanged dispatch -------------
        for p in decline {
            self.patch_rel32_to_here(p);
        }
        // The dispatch below starts at a join of many edges; no register
        // mirrors a frame slot here.
        self.slot_mirror = None;
        Some(hit)
    }

    /// BOX_HASH_EQUALS (round 11 wave 12, lane `hashmap`): answer
    /// `Integer.hashCode()`, `Integer.equals(Object)` and an already-cached
    /// `String.hashCode()` inline, as a guarded PREFIX of an `Object`-typed
    /// virtual site's ordinary dispatch, and return the hit edges' `JMP rel32`
    /// patch sites (the answer is in RAX on each). Empty = nothing emitted.
    ///
    /// # Why
    ///
    /// `CratonBench hashmap` makes 30 M such calls in one phase
    /// (`HashMap.hash` twice and `HashMap.getNode`'s `key.equals(k)` once per
    /// put+get pair). The callees are natives, which an inline cache can never
    /// hold, so every call missed into `jit_invoke_virtual_mic` and was served
    /// from its native-site cache at ~320 cycles
    /// (`r11w11-hashmap-cratonbench-hashmap-where-the-time-goes-CLOSED-20260928.md`).
    ///
    /// # The shape, and what it answers
    ///
    /// ```text
    ///   [layout epoch guard]                       ; JNE decline
    ///   RAX = receiver ; TEST ; JZ decline         ; null: the call raises the NPE
    ///   CMP BYTE [RAX+KIND_TAGS], Object ; JNE decline
    ///   CMP DWORD [RAX+cid], Integer ; JNE string_or_decline
    ///     hashCode: RAX = value                    ; JMP hit
    ///     equals:   RDX = arg ; TEST ; JZ false    ; `equals(null)` is false
    ///               CMP BYTE [RDX+KIND_TAGS], Object ; JNE decline
    ///               CMP DWORD [RDX+cid], Integer ; JNE decline
    ///               EAX = (this.value == arg.value) ; JMP hit
    ///     false:    EAX = 0 ; JMP hit
    ///   string_or_decline:                         ; hashCode at an Object site only
    ///   CMP DWORD [RAX+cid], String ; JNE decline
    ///   ECX = hash ; TEST ; JZ decline             ; not cached yet: the call computes it
    ///   RAX = RCX ; JMP hit
    ///   decline: <the unchanged dispatch>
    /// ```
    ///
    /// `Integer` and `String` are final, so a receiver whose class id IS one of
    /// them selects exactly that class's own `hashCode`/`equals` at any site
    /// whose static type it can inhabit, and those are fixed by the Java SE
    /// specification: `Integer.hashCode()` is `value`, `Integer.equals(o)` is
    /// `o instanceof Integer && value == ((Integer) o).value`. The argument's
    /// class-id compare is the `instanceof` for a final class; anything it
    /// cannot prove (an array, another class, a second loader's copy of the
    /// NAME) declines to the call instead of answering `false`, so the
    /// registered native keeps its own rule for those.
    ///
    /// `String.hashCode()` is answered only from a NON-ZERO cached `hash`,
    /// which is what the JDK method returns without looking at `value` or
    /// `coder`. A zero `hash` (not computed yet, or `hashIsZero`) declines, and
    /// the call computes, caches and returns it, so neither `coder` nor
    /// `hashIsZero` is re-implemented here. Reading `hash` racily is the JDK's
    /// own benign race: a non-zero value is only ever the final hash.
    ///
    /// Every edge that is not a hit is a DECLINE to the site's ordinary
    /// dispatch, never a deopt: a `HashMap` key site legitimately sees every
    /// key class, and a deopt per foreign key would re-run the method in the
    /// interpreter each time. A null receiver declines too, so the NPE is the
    /// dispatch path's, at this invoke's bci, exactly as before.
    ///
    /// # Declines at compile time
    ///
    /// Not `invokevirtual` of `hashCode()I`/`equals(Ljava/lang/Object;)Z`
    /// declared on `Object`/`Number`/`Integer` (String: `Object` only); no
    /// `Integer` class id from the VM (`DirectHelperTable::integer_class_id`),
    /// `Integer` not initialised, or its `value` not a 4-byte cell
    /// (`AtomicIntFieldLayout::new`); no String layout with a `coder`;
    /// `CRATONVM_JIT_NO_BOX_UNBOX_INTRINSIC=1` (the `Integer` half, the same
    /// switch as the unbox intrinsic that reads the same field) and
    /// `CRATONVM_JIT_OBJECT_HASHCODE_STRING=0` (the `String` half); an operand
    /// in an XMM register or a scratch register the prefix would clobber.
    pub(in crate::x64::op_invoke) fn try_emit_box_hash_equals_prefix(
        &mut self,
        pc: usize,
        info: &crate::JitInvokeInfo,
    ) -> Vec<usize> {
        let is_hash = info.method_name == "hashCode" && info.descriptor == "()I";
        let is_equals = info.method_name == "equals" && info.descriptor == "(Ljava/lang/Object;)Z";
        if info.invoke_kind != 0 || !(is_hash || is_equals) {
            return Vec::new();
        }
        let integer_site = matches!(
            info.class_name,
            "java/lang/Object" | "java/lang/Number" | "java/lang/Integer"
        );
        // `String.hashCode()` only where the String-typed intrinsics do not
        // already reach: an `Object`-typed site.
        let string_site = is_hash && info.class_name == "java/lang/Object";
        if !integer_site && !string_site {
            return Vec::new();
        }
        // The `Integer` half: the guard id, proven initialised (so its layout
        // is registered), and its `value` cell's two addresses.
        let integer = if integer_site && !crate::box_unbox_intrinsic_disabled() {
            self.direct_helpers
                .integer_class_id()
                .filter(|&cid| self.direct_helpers.class_is_initialized(cid))
                .and_then(|cid| {
                    crate::AtomicIntFieldLayout::new(0, cid)
                        .filter(|l| l.class_id == cid)
                        .map(|l| (cid, l.value_compact_offset, l.value_legacy_offset))
                })
        } else {
            None
        };
        // The `String` half: the `hash` cell and the guard id.
        let string = if string_site && crate::object_hashcode_string_guard_enabled() {
            self.string_layout
                .filter(|l| l.has_coder && l.string_class_id != 0)
                .map(|l| {
                    (
                        l.string_class_id,
                        l.hash_compact_offset,
                        l.hash_legacy_offset,
                    )
                })
        } else {
            None
        };
        // nio21 wave 8 L5: the `Class` half, the single-pass twin of the
        // optimizing tier's (`ir_lower::try_emit_ir_box_hash_equals_prefix`,
        // nio20 wave 5 L2). An `Object`-typed `hashCode()` whose receiver is
        // exactly `java/lang/Class` (final, does not override `hashCode`) is
        // `Object.hashCode`'s identity hash, answered from the header when one
        // is installed there (`ir_lower::identity_hash_read_bytes`; its
        // declines land on the unchanged dispatch). Same screens as
        // `try_emit_sp_identity_hash_dispatch_prefix`, plus its own switch.
        let class_arm = if string_site
            && sp_box_hash_class_arm_enabled()
            && sp_identity_hash_inline_enabled()
            && crate::ir_lower::ir_identity_hash_inline_enabled()
        {
            crate::ir_lower::ir_class_class_id(&self.direct_helpers)
        } else {
            None
        };
        // JIT round 21, lane hash (NATIVE21-1): the identity arm, the twin of
        // the optimizing tier's (`ir_lower::try_emit_ir_box_hash_equals_prefix`):
        // a plain receiver of exactly `java/lang/Object` (class id 0), or of
        // the class this site's inline cache was seeded with when the VM
        // proves it inherits `Object.hashCode`
        // (`ir_lower::identity_hash_profiled_class`), answers the installed
        // identity hash from the header. Same screens as the `Class` arm.
        let identity_arm = string_site
            && crate::ir_lower::object_hash_identity_arm_enabled()
            && sp_identity_hash_inline_enabled()
            && crate::ir_lower::ir_identity_hash_inline_enabled();
        let profiled_cid = if identity_arm {
            self.mic_slots_idx
                .get(&pc)
                .and_then(|&i| self.mic_slots.get(i))
                .and_then(|&(_, mic)| {
                    crate::ir_lower::identity_hash_profiled_class(
                        &self.direct_helpers,
                        // Cast: the slot's address, as the IR tier's map holds it.
                        mic as usize,
                    )
                })
                .filter(|&cid| Some(cid) != class_arm)
        } else {
            None
        };
        // JIT round 21, lane hashlock (HASH2-21-1): the equals identity arm,
        // the twin of the optimizing tier's: an `Object`-typed
        // `equals(Object)` whose receiver is a plain object of a class whose
        // `equals(Object)` IS `Object.equals` answers `this == arg`.
        let equals_arm = is_equals
            && info.class_name == "java/lang/Object"
            && crate::ir_lower::object_equals_identity_arm_enabled();
        if integer.is_none()
            && string.is_none()
            && class_arm.is_none()
            && !identity_arm
            && !equals_arm
        {
            return Vec::new();
        }
        // An equals site has nothing to answer without the `Integer` half
        // (or, since HASH2-21-1, the equals identity arm).
        if is_equals && integer.is_none() && !equals_arm {
            return Vec::new();
        }
        let depth = if is_equals { 2 } else { 1 };
        if info.num_jit_args != depth || self.stack.len() < depth {
            return Vec::new();
        }
        let Some(&recv_slot) = self.stack.get(self.stack.len() - depth) else {
            return Vec::new();
        };
        let arg_slot = if is_equals {
            match self.stack.last() {
                Some(&s) => Some(s),
                None => return Vec::new(),
            }
        } else {
            None
        };
        if matches!(recv_slot, StackSlot::Xmm(..)) || matches!(arg_slot, Some(StackSlot::Xmm(..))) {
            return Vec::new();
        }
        // The prefix writes RAX, RCX, RDX, R8 and (the epoch guard's fallback)
        // R11. The walk flushed the scratch operand cache before this arm, so
        // this never fires; a caller that did not gets the plain dispatch.
        if self
            .stack
            .iter()
            .any(|slot| matches!(slot, StackSlot::Scratch(..)))
        {
            return Vec::new();
        }
        if false {
            eprintln!(
                "[cratonvm-jitc] box-hash-equals-prefix {} {}.{}{} @pc={pc} integer={:?} string={:?} class={:?}",
                self.method_label,
                info.class_name,
                info.method_name,
                info.descriptor,
                integer.map(|(cid, ..)| cid),
                string.map(|(cid, ..)| cid),
                class_arm,
            );
        }

        let cid_off = self.helpers.class_id_offset_in_obj as i32; // Cast: x86-64 disp32
        let mut decline: Vec<usize> = Vec::new();
        let mut hits: Vec<usize> = Vec::new();

        // The baked compact offsets are a compile-time claim about a layout; a
        // layout REPLACEMENT bumps the process epoch and this site then
        // declines for good. First: its fallback form clobbers RCX and R11.
        if jit_sp_field_layout_guard_enabled() {
            if let Some(p) = self.emit_layout_epoch_guard() {
                decline.push(p);
            }
        }

        // RAX = receiver. Null declines: the dispatch raises the NPE.
        self.load_slot_to_reg(RAX, recv_slot);
        self.emit_test_r64_r64(RAX);
        decline.push(self.emit_jcc_rel32_patch(0x84)); // JZ
        self.emit_test_mem8_imm8(
            RAX,
            cratonvm_types::KIND_TAGS_BYTE_OFFSET as i32, // Cast: small header constant
            cratonvm_types::KIND_TAG_BYTE_MASK,
        );
        let array_recv = if (!is_equals && identity_arm) || (is_equals && equals_arm) {
            Some(self.emit_jcc_rel32_patch(0x85)) // JNE -> array receiver arm
        } else {
            decline.push(self.emit_jcc_rel32_patch(0x85)); // JNE
            None
        };

        let mut not_integer: Option<usize> = None;
        if let Some((int_cid, value_compact, value_legacy)) = integer {
            // CMP DWORD [RAX + cid_off], Integer -- `81 /7 id`, ModRM mod=10
            // rm=RAX, the form the ArrayList prefix uses.
            self.buf.emit(&[0x81, 0xB8]);
            self.buf.emit(&cid_off.to_le_bytes());
            self.buf.emit(&int_cid.to_le_bytes());
            not_integer = Some(self.emit_jcc_rel32_patch(0x85)); // JNE
            if let Some(arg) = arg_slot {
                // equals(Object). RDX = the argument.
                self.load_slot_to_reg(RDX, arg);
                self.emit_test_r64_r64(RDX);
                let arg_null = self.emit_jcc_rel32_patch(0x84); // JZ -> false
                self.emit_test_mem8_imm8(
                    RDX,
                    cratonvm_types::KIND_TAGS_BYTE_OFFSET as i32, // Cast: small header constant
                    cratonvm_types::KIND_TAG_BYTE_MASK,
                );
                decline.push(self.emit_jcc_rel32_patch(0x85)); // JNE
                                                               // CMP DWORD [RDX + cid_off], Integer -- ModRM mod=10 rm=RDX.
                self.buf.emit(&[0x81, 0xBA]);
                self.buf.emit(&cid_off.to_le_bytes());
                self.buf.emit(&int_cid.to_le_bytes());
                decline.push(self.emit_jcc_rel32_patch(0x85)); // JNE
                                                               // Both values, each through its OWN object's compact/legacy
                                                               // bit: the two instances need not share a layout.
                self.emit_load_string_i32_field(RCX, RAX, value_compact, false, value_legacy);
                self.emit_load_string_i32_field(R8, RDX, value_compact, false, value_legacy);
                self.emit_cmp_r32_r32(RCX, R8);
                self.buf.emit(&[0x0F, 0x94, 0xC0]); // SETE AL
                self.buf.emit(&[0x0F, 0xB6, 0xC0]); // MOVZX EAX, AL
                hits.push(self.emit_jmp_rel32_patch());
                // `equals(null)` is false.
                self.patch_rel32_to_here(arg_null);
                self.emit_xor_reg_self(RAX);
                hits.push(self.emit_jmp_rel32_patch());
            } else {
                // hashCode(): the value, sign-extended into the int slot.
                self.emit_load_string_i32_field(RAX, RAX, value_compact, false, value_legacy);
                hits.push(self.emit_jmp_rel32_patch());
            }
        }
        // The `String` compare's miss: the `Class` arm when there is one,
        // else a decline.
        let mut not_string: Option<usize> = None;
        if let Some((str_cid, hash_compact, hash_legacy)) = string {
            if let Some(p) = not_integer.take() {
                self.patch_rel32_to_here(p);
            }
            // CMP DWORD [RAX + cid_off], String.
            self.buf.emit(&[0x81, 0xB8]);
            self.buf.emit(&cid_off.to_le_bytes());
            self.buf.emit(&str_cid.to_le_bytes());
            not_string = Some(self.emit_jcc_rel32_patch(0x85)); // JNE
            self.emit_load_string_i32_field(RCX, RAX, hash_compact, false, hash_legacy);
            self.buf.emit(&[0x85, 0xC9]); // TEST ECX, ECX
            decline.push(self.emit_jcc_rel32_patch(0x84)); // JZ: not cached
            self.emit_mov_reg_reg(RAX, RCX);
            hits.push(self.emit_jmp_rel32_patch());
        }
        // nio21 wave 8 L5: the `Class` arm. Reached from the `Integer` /
        // `String` compares' misses (or straight from the kind test when
        // neither half exists); RAX is still the non-null plain receiver on
        // every one of them -- the halves above load into RCX/RDX/R8 only
        // after their own class compare hits.
        // JIT round 21, lane hash: the identity arm shares the `Class` arm's
        // header read; the last compare's miss declines.
        if class_arm.is_some() || identity_arm {
            if let Some(p) = not_integer.take() {
                self.patch_rel32_to_here(p);
            }
            if let Some(p) = not_string.take() {
                self.patch_rel32_to_here(p);
            }
            let mut to_read: Vec<usize> = Vec::new();
            if let Some(class_cid) = class_arm {
                // CMP DWORD [RAX + cid_off], Class ; JE read (JNE decline
                // when it is the last compare).
                self.buf.emit(&[0x81, 0xB8]);
                self.buf.emit(&cid_off.to_le_bytes());
                self.buf.emit(&class_cid.to_le_bytes());
                if identity_arm {
                    to_read.push(self.emit_jcc_rel32_patch(0x84)); // JE
                } else {
                    decline.push(self.emit_jcc_rel32_patch(0x85)); // JNE
                }
            }
            if identity_arm {
                if let Some(cid) = profiled_cid {
                    // CMP DWORD [RAX + cid_off], profiled ; JE read.
                    self.buf.emit(&[0x81, 0xB8]);
                    self.buf.emit(&cid_off.to_le_bytes());
                    self.buf.emit(&cid.to_le_bytes());
                    to_read.push(self.emit_jcc_rel32_patch(0x84)); // JE
                }
                // CMP DWORD [RAX + cid_off], 0 (`java/lang/Object`) ; JNE decline.
                self.buf.emit(&[0x81, 0xB8]);
                self.buf.emit(&cid_off.to_le_bytes());
                self.buf.emit(&0u32.to_le_bytes());
                // JIT round 21, lane hash2 (`CRATONVM_JIT_OBJECT_HASH_CLASS_TABLE`):
                // JE read; any other class reads its per-VM verdict byte
                // (`ir_lower::identity_hash_class_table_test_bytes`, the
                // optimizing tier's bytes; misses decline). Writes RCX, RDX.
                if let Some(table) =
                    crate::ir_lower::identity_hash_class_table(&self.direct_helpers)
                {
                    to_read.push(self.emit_jcc_rel32_patch(0x84)); // JE
                    let (bytes, edges) =
                        crate::ir_lower::identity_hash_class_table_test_bytes(table, cid_off);
                    let at = self.buf.pos();
                    self.buf.emit(&bytes);
                    decline.extend(edges.into_iter().map(|e| at + e));
                } else {
                    decline.push(self.emit_jcc_rel32_patch(0x85)); // JNE
                }
            }
            for p in to_read {
                self.patch_rel32_to_here(p);
            }
            // RAX = the header's identity hash, sign-extended; an unhashed,
            // locked or forwarded header declines. Writes RCX and RDX.
            let (read, edges) = crate::ir_lower::identity_hash_read_bytes();
            let at = self.buf.pos();
            self.buf.emit(&read);
            decline.extend(edges.into_iter().map(|e| at + e));
            hits.push(self.emit_jmp_rel32_patch());
        }
        // JIT round 21, lane hashlock (HASH2-21-1): the equals identity arm,
        // reached from the `Integer` compare's miss (or straight from the kind
        // test); RAX is still the non-null plain receiver.
        if equals_arm {
            if let Some(arg) = arg_slot {
                if let Some(p) = not_integer.take() {
                    self.patch_rel32_to_here(p);
                }
                let eq_profiled = self
                    .mic_slots_idx
                    .get(&pc)
                    .and_then(|&i| self.mic_slots.get(i))
                    .and_then(|&(_, mic)| {
                        crate::ir_lower::identity_equals_profiled_class(
                            &self.direct_helpers,
                            // Cast: the slot's address, as the IR tier's map holds it.
                            mic as usize,
                        )
                    });
                let eq_table = crate::ir_lower::identity_equals_class_table(&self.direct_helpers);
                if false {
                    eprintln!(
                        "[cratonvm-jitc] identity-equals-inline (sp, Object.equals) {} @pc={pc} profiled={eq_profiled:?} class_table={}",
                        self.method_label,
                        eq_table.is_some(),
                    );
                }
                let mut to_answer: Vec<usize> = Vec::new();
                if let Some(cid) = eq_profiled {
                    // CMP DWORD [RAX + cid_off], profiled ; JE answer.
                    self.buf.emit(&[0x81, 0xB8]);
                    self.buf.emit(&cid_off.to_le_bytes());
                    self.buf.emit(&cid.to_le_bytes());
                    to_answer.push(self.emit_jcc_rel32_patch(0x84)); // JE
                }
                // CMP DWORD [RAX + cid_off], 0 (`java/lang/Object`).
                self.buf.emit(&[0x81, 0xB8]);
                self.buf.emit(&cid_off.to_le_bytes());
                self.buf.emit(&0u32.to_le_bytes());
                if let Some(table) = eq_table {
                    // JE answer; any other class reads its verdict byte (the
                    // identity-hash table's test bytes: same layout). Writes
                    // RCX, RDX.
                    to_answer.push(self.emit_jcc_rel32_patch(0x84)); // JE
                    let (bytes, edges) =
                        crate::ir_lower::identity_hash_class_table_test_bytes(table, cid_off);
                    let at = self.buf.pos();
                    self.buf.emit(&bytes);
                    decline.extend(edges.into_iter().map(|e| at + e));
                } else {
                    decline.push(self.emit_jcc_rel32_patch(0x85)); // JNE
                }
                for p in to_answer {
                    self.patch_rel32_to_here(p);
                }
                // `Object.equals` is `this == obj`: RDX = argument (a null one
                // is never equal to the non-null receiver).
                self.load_slot_to_reg(RDX, arg);
                self.buf.emit(&[0x48, 0x39, 0xD0]); // CMP RAX, RDX
                self.buf.emit(&[0x0F, 0x94, 0xC0]); // SETE AL
                self.buf.emit(&[0x0F, 0xB6, 0xC0]); // MOVZX EAX, AL
                hits.push(self.emit_jmp_rel32_patch());
            }
        }
        if let Some(p) = array_recv {
            self.patch_rel32_to_here(p);
            if let Some(arg) = arg_slot {
                self.load_slot_to_reg(RDX, arg);
                self.buf.emit(&[0x48, 0x39, 0xD0]); // CMP RAX, RDX
                self.buf.emit(&[0x0F, 0x94, 0xC0]); // SETE AL
                self.buf.emit(&[0x0F, 0xB6, 0xC0]); // MOVZX EAX, AL
                hits.push(self.emit_jmp_rel32_patch());
            } else {
                let (read, edges) = crate::ir_lower::identity_hash_read_bytes();
                let at = self.buf.pos();
                self.buf.emit(&read);
                decline.extend(edges.into_iter().map(|e| at + e));
                hits.push(self.emit_jmp_rel32_patch());
            }
        }
        if let Some(p) = not_integer {
            decline.push(p);
        }
        if let Some(p) = not_string {
            decline.push(p);
        }

        // ---- decline: the first byte of the unchanged dispatch -------------
        for p in decline {
            self.patch_rel32_to_here(p);
        }
        self.slot_mirror = None;
        hits
    }

    /// VALUE_OF_INLINE_BOX (round 12 wave 1, lane `calls`; proposal W17-1 in
    /// `docs/feature-designs/jit-ir-optimizations.md`): allocate `Integer.valueOf(int)`'s box
    /// INLINE when the value is outside the box cache, as a guarded prefix of
    /// the site's unchanged `jit_integer_value_of_direct` call. Returns the
    /// hit edge's `JMP rel32` patch site (the box is in RAX); `None` means
    /// nothing reaches the join (either nothing was emitted, or every emitted
    /// edge lands on the call).
    ///
    /// The single-pass twin of `ir_lower::try_emit_ir_integer_value_of_prefix`
    /// and the same answer: a value `v < -128 || v > high` gets a FRESH
    /// `java/lang/Integer` with `value = v`, which is what the helper returns
    /// for it (JLS 5.1.7 requires identity only inside the cache), and
    /// `-128..=high` declines to the call, which answers from the NATIVE box
    /// table, so `valueOf(5) == valueOf(5)` is unchanged. `high` is the bound
    /// this VM's native resolved (`ir_lower::ir_integer_cache_high`); it is
    /// fixed once resolved, and an unresolved bound emits nothing.
    ///
    /// ```text
    ///   ECX = v
    ///   CMP ECX, -128 ; JL alloc
    ///   CMP ECX, high ; JLE decline            ; inside the cache
    /// alloc:
    ///   <the `new` site's bump, decline form>   ; guard/thread/full -> decline
    ///   TEST BYTE [RAX+GC_FLAGS], COMPACT ; JZ decline
    ///   MOV DWORD [RAX+value_off], ECX ; JMP hit
    /// decline: <the unchanged call>
    /// ```
    ///
    /// The hit path makes no call, so it cannot collect, trap or deopt: it owes
    /// no safepoint map, no spill and no sentinel check, and it is the same
    /// allocation the `new` site's sunk-spill fast path makes (header before
    /// the cursor commit, `emit_inline_tlab_new_with`'s own contract). The bump
    /// writes RAX, R10, R11 (RDX under the zero-elision opt-out) and never
    /// RCX, so ECX still holds the value at the store; none of them homes a
    /// local, and the walk flushed the operand cache before this arm. A fresh
    /// object that is not compact (a layout or domain change at run time) is
    /// left behind as unreachable garbage with a complete header, and the call
    /// allocates the answer.
    ///
    /// Declines at compile time: `CRATONVM_JIT_SP_VALUEOF_INLINE_BOX=0`,
    /// `CRATONVM_JIT_NO_BOX_UNBOX_INTRINSIC=1`, `CRATONVM_JIT_DISABLE_INLINE_NEW`,
    /// the inline-TLAB opt-out; under ZGC, an unarmed inline announce
    /// (`ir_lower::box_tlab_admitted`: since al16 the box is announced inline
    /// after the bump, or the bump is undone and the call allocates); no
    /// cursor/end offsets; no thread source that is not a
    /// call (the cached slot or the `JIT_THREAD` mirror); `Integer`
    /// unresolved, ambiguous or not initialised; no compact layout of exactly
    /// one 4-byte `Int` cell; an operand in an XMM or scratch register.
    pub(in crate::x64) fn try_emit_value_of_box_prefix(&mut self, pc: usize, value: StackSlot) -> Option<usize> {
        if crate::box_unbox_intrinsic_disabled()
            || false
            || !inline_tlab_new_enabled()
            || !crate::ir_lower::box_tlab_admitted()
            || !self.needs_heap
            || self.failed
        {
            return None;
        }
        if self.helpers.tlab_cursor_offset_in_thread == self.helpers.tlab_end_offset_in_thread
            || (self.jit_thread_slot_off == 0 && jit_thread_tls_disp() == 0)
            || self.deferred_alloc_blind_spill
            || self.deferred_alloc_shadow_push
        {
            return None;
        }
        if matches!(value, StackSlot::Xmm(..))
            || self
                .stack
                .iter()
                .any(|slot| matches!(slot, StackSlot::Scratch(..)))
        {
            return None;
        }
        let cid = self
            .direct_helpers
            .integer_class_id()
            .filter(|&cid| self.direct_helpers.class_is_initialized(cid))?;
        let high = crate::ir_lower::ir_integer_cache_high(&self.direct_helpers)?;
        // The replacement count BEFORE the layout is read: if it moves while
        // this site is emitted, the bump may have baked a layout newer than
        // the offset below, and the hit edge is withdrawn (see the end).
        let (_, replace_count_before) = cratonvm_types::layout_replace_guard(cid);
        let total = cratonvm_types::compact_tlab_total_size(cid, 1)?;
        let value_off = match cratonvm_types::compact_field_disp_storage(cid, 0) {
            Some((off, cratonvm_types::FieldStorageKind::Int)) => off,
            _ => return None,
        };
        if total % 8 != 0
            || total > 256
            || value_off < cratonvm_types::COMPACT_HEADER_SIZE
            || value_off.saturating_add(4) > total
        {
            return None;
        }
        let value_off = i32::try_from(value_off).ok()?;
        if false {
            eprintln!(
                "[cratonvm-jitc] valueof-inline-box {} @pc={pc} integer_class_id={cid} \
                 high={high} value_off={value_off} total={total}",
                self.method_label,
            );
        }

        let mut decline: Vec<usize> = Vec::new();
        // ECX = the value. The int slot is sign-extended; only ECX is read.
        self.load_slot_to_reg(RCX, value);
        // JIT r24 (CompletableFuture page): the cached range answered INLINE
        // from the VM's box table, as the optimizing tier's prefix does
        // (`ir_lower::integer_cache_read_bytes`, under the same
        // `CRATONVM_JIT_IR_VALUEOF_CACHE_READ` and its own
        // `CRATONVM_JIT_SP_VALUEOF_CACHE_READ`, both default ON): a filled
        // slot is the helper's canonical box in RAX; an empty slot declines
        // to the call (which fills it); a value outside the table goes on to
        // the allocation arm. A hot `x -> x + 1` lambda body kept single-pass
        // spent a quarter of the `done` row in `integer_cache_peek`.
        let mut cache_hit: Option<usize> = None;
        if crate::ir_lower::ir_integer_value_of_cache_read_enabled()
        {
            if let Some((base, len)) = crate::ir_lower::ir_integer_cache_slots(&self.direct_helpers)
            {
                let (bytes, outside, empty) = crate::ir_lower::integer_cache_read_bytes(base, len);
                let at = self.buf.pos();
                self.buf.emit(&bytes);
                decline.push(at + empty);
                cache_hit = Some(self.emit_jmp_rel32_patch());
                self.patch_rel32_to_here(at + outside);
            }
        }
        // CMP ECX, -128 (83 /7 ib) ; JL alloc.
        self.buf.emit(&[0x83, 0xF9, 0x80]);
        let below_cache = self.emit_jcc_rel32_patch(0x8C);
        // CMP ECX, high (81 /7 id) ; JLE decline -- inside the cache.
        self.buf.emit(&[0x81, 0xF9]);
        self.buf.emit(&high.to_le_bytes());
        decline.push(self.emit_jcc_rel32_patch(0x8E));
        self.patch_rel32_to_here(below_cache);
        // al16 / nio21 wave 10 L1: under ZGC the decline form announces the
        // box itself (`inline_announce` = registration required): the start
        // bit before the cursor commit, a refusal declining with nothing
        // committed; with `CRATONVM_ZGC_JIT_START_BIT_BEFORE_COMMIT=0` it
        // emits the al16 commit-then-announce-or-rollback form.
        self.emit_inline_tlab_new_with(
            cid,
            1,
            super::objects::TlabPostInit::Skip,
            cratonvm_types::jit_tlab_registration_required(),
            Some(&mut decline),
        );
        let (_, replace_count_after) = cratonvm_types::layout_replace_guard(cid);
        let mut hit = None;
        if replace_count_after == replace_count_before {
            // TEST BYTE [RAX+GC_FLAGS], COMPACT ; JZ decline: only a compact
            // body has the 4-byte cell at `value_off`.
            self.emit_test_mem8_imm8(
                RAX,
                cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32, // Cast: small header constant
                cratonvm_types::GC_FLAG_COMPACT,
            );
            decline.push(self.emit_jcc_rel32_patch(0x84));
            // MOV DWORD [RAX+disp32], ECX (89 /r, mod=10 reg=ECX rm=RAX). A
            // primitive store into a fresh object: no barrier.
            self.buf.emit(&[0x89, 0x88]);
            self.buf.emit(&value_off.to_le_bytes());
            // A cache hit joins here with its box in RAX.
            if let Some(p) = cache_hit.take() {
                self.patch_rel32_to_here(p);
            }
            hit = Some(self.emit_jmp_rel32_patch());
        }
        // Without a hit edge, a cache hit still has its answer: it gets its
        // own hit jump, and the bump's fall-through skips over it.
        if let Some(p) = cache_hit {
            let skip = self.emit_jmp_rel32_patch();
            self.patch_rel32_to_here(p);
            hit = Some(self.emit_jmp_rel32_patch());
            decline.push(skip);
        }
        // ---- decline: the first byte of the unchanged call ----------------
        // Without a hit edge (the layout moved under the emission) the bump's
        // fall-through lands here too, and its object is garbage.
        for p in decline {
            self.patch_rel32_to_here(p);
        }
        self.slot_mirror = None;
        hit
    }

    /// LONG_VALUE_OF_INLINE_BOX (round 12 wave 2, lane `calls`; proposal C12-2
    /// in `docs/feature-designs/profile-guided-inlining.md`): the `Long.valueOf(long)` twin of
    /// [`Self::try_emit_value_of_box_prefix`], same contract, same screens,
    /// same decline-form bump, as a guarded prefix of the site's unchanged
    /// `jit_long_value_of_direct` call.
    ///
    /// `Long`'s box cache is the fixed `-128..=127` (no `high` property; the
    /// helper tests exactly that range), so the compares are 64-bit against
    /// constants and nothing is resolved but the class id. A value outside the
    /// cache gets a FRESH `java/lang/Long` with its 8-byte compact `value`
    /// cell written; a value inside it, and every decline, runs the call,
    /// which answers from the box table, so identity is unchanged.
    ///
    /// ```text
    ///   RCX = v
    ///   CMP RCX, -128 ; JL alloc
    ///   CMP RCX, 127  ; JLE decline
    /// alloc:
    ///   <the `new` site's bump, decline form>
    ///   TEST BYTE [RAX+GC_FLAGS], COMPACT ; JZ decline
    ///   MOV QWORD [RAX+value_off], RCX ; JMP hit
    /// decline: <the unchanged call>
    /// ```
    ///
    /// Extra compile-time declines: `CRATONVM_JIT_LONG_VALUEOF_INLINE_BOX=0`,
    /// `Long` unresolved, ambiguous or not initialised, and a compact layout
    /// that is not exactly one 8-aligned `Long` cell.
    pub(in crate::x64) fn try_emit_long_value_of_box_prefix(&mut self, pc: usize, value: StackSlot) -> Option<usize> {
        if !crate::ir_lower::long_value_of_inline_box_enabled()
            || crate::box_unbox_intrinsic_disabled()
            || false
            || !inline_tlab_new_enabled()
            || !crate::ir_lower::box_tlab_admitted()
            || !self.needs_heap
            || self.failed
        {
            return None;
        }
        if self.helpers.tlab_cursor_offset_in_thread == self.helpers.tlab_end_offset_in_thread
            || (self.jit_thread_slot_off == 0 && jit_thread_tls_disp() == 0)
            || self.deferred_alloc_blind_spill
            || self.deferred_alloc_shadow_push
        {
            return None;
        }
        if matches!(value, StackSlot::Xmm(..))
            || self
                .stack
                .iter()
                .any(|slot| matches!(slot, StackSlot::Scratch(..)))
        {
            return None;
        }
        let cid = crate::ir_lower::ir_long_class_id(&self.direct_helpers)
            .filter(|&cid| self.direct_helpers.class_is_initialized(cid))?;
        let (_, replace_count_before) = cratonvm_types::layout_replace_guard(cid);
        let total = cratonvm_types::compact_tlab_total_size(cid, 1)?;
        let value_off = match cratonvm_types::compact_field_disp_storage(cid, 0) {
            Some((off, cratonvm_types::FieldStorageKind::Long)) => off,
            _ => return None,
        };
        if total % 8 != 0
            || total > 256
            || value_off < cratonvm_types::COMPACT_HEADER_SIZE
            || value_off % 8 != 0
            || value_off.saturating_add(8) > total
        {
            return None;
        }
        let value_off = i32::try_from(value_off).ok()?;
        if false {
            eprintln!(
                "[cratonvm-jitc] long-valueof-inline-box {} @pc={pc} long_class_id={cid} \
                 value_off={value_off} total={total}",
                self.method_label,
            );
        }

        let mut decline: Vec<usize> = Vec::new();
        // RCX = the value, all 64 bits.
        self.load_slot_to_reg(RCX, value);
        // CMP RCX, -128 (REX.W 83 /7 ib) ; JL alloc.
        self.buf.emit(&[0x48, 0x83, 0xF9, 0x80]);
        let below_cache = self.emit_jcc_rel32_patch(0x8C);
        // CMP RCX, 127 (REX.W 83 /7 ib) ; JLE decline -- inside the cache.
        self.buf.emit(&[0x48, 0x83, 0xF9, 0x7F]);
        decline.push(self.emit_jcc_rel32_patch(0x8E));
        self.patch_rel32_to_here(below_cache);
        // al16 / nio21 wave 10 L1: under ZGC the decline form announces the
        // box itself (`inline_announce` = registration required): the start
        // bit before the cursor commit, a refusal declining with nothing
        // committed; with `CRATONVM_ZGC_JIT_START_BIT_BEFORE_COMMIT=0` it
        // emits the al16 commit-then-announce-or-rollback form.
        self.emit_inline_tlab_new_with(
            cid,
            1,
            super::objects::TlabPostInit::Skip,
            cratonvm_types::jit_tlab_registration_required(),
            Some(&mut decline),
        );
        let (_, replace_count_after) = cratonvm_types::layout_replace_guard(cid);
        let mut hit = None;
        if replace_count_after == replace_count_before {
            self.emit_test_mem8_imm8(
                RAX,
                cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32, // Cast: small header constant
                cratonvm_types::GC_FLAG_COMPACT,
            );
            decline.push(self.emit_jcc_rel32_patch(0x84));
            // MOV QWORD [RAX+disp32], RCX (REX.W 89 /r, mod=10 reg=RCX rm=RAX).
            // A primitive store into a fresh object: no barrier.
            self.buf.emit(&[0x48, 0x89, 0x88]);
            self.buf.emit(&value_off.to_le_bytes());
            hit = Some(self.emit_jmp_rel32_patch());
        }
        for p in decline {
            self.patch_rel32_to_here(p);
        }
        self.slot_mirror = None;
        hit
    }

    /// nio20 wave 2, lane L2: may a guarded prefix stand in front of this
    /// instance site's plain thin-helper CALL ([`Self::try_emit_sp_getclass_prefix`],
    /// [`Self::try_emit_sp_unsafe_access_prefix`])?
    ///
    /// The prefix sits right after `pop_invoke_args` and the receiver's null
    /// check, and its hit edge lands at the result push, past everything the
    /// call emits in between: the service-range copy, `emit_stack_arg_setup`,
    /// the pre-safepoint spill (or the metadata-only publication), the CALL,
    /// its oop map and post-safepoint reload, the stack-arg cleanup, the
    /// callee-deopt check and the post-invoke exception check. That is sound
    /// exactly when every compile-time state change those make is either
    /// balanced on the decline path alone or untouched, which these screens
    /// establish (the static arm's VALUE_OF_INLINE_BOX prefix has the same
    /// shape and the same argument):
    ///
    /// * **No `JitInvokeInfo`** (a thin helper, not a compiled callee): no
    ///   service range is reserved, so no frame word is pushed to
    ///   `pending_staged_arg_oops` for a map that might be emitted later than
    ///   the join (the plain arm emits its map only under precise maps or the
    ///   shadow stack) -- a hit would leave such a word unwritten while a
    ///   later map names it. With no info there is no callee-deopt service to
    ///   skip either. `pending_staged_args_unmapped` and the argument-register
    ///   bits `pop_invoke_args` raised only make the NEXT map more
    ///   conservative if this one is not emitted.
    /// * **No operand in a scratch or XMM register** below the arguments: the
    ///   spill's `flush_scratch_registers` (moving-young) stores those and
    ///   rewrites `self.stack` to name the frame word, which a hit edge would
    ///   leave unwritten. With none, the spill changes no slot's home, and its
    ///   register-homed-local stores and the reload after the map only copy
    ///   values the hit path never disturbed (it writes RAX, RCX, RDX, R10 and
    ///   R11, none of which homes a local or an operand here).
    /// * **Nothing pending**: no open shadow publication and no deferred
    ///   allocation spill (only a `new` site raises those), so the spill's
    ///   push and the map's reload pair with each other on the decline path.
    /// * **No guard row, no judged `VarHandle` record, no argument hand-off**:
    ///   the site is the plain unguarded bind the helper tables describe.
    ///
    /// The hit path itself makes no call: it cannot collect, trap or deopt,
    /// so it owes no map, no spill and no sentinel check.
    pub(in crate::x64::op_invoke) fn sp_thin_helper_prefix_admitted(
        &self,
        has_info: bool,
        guard_class_id: u32,
        vh_judged_site: usize,
        args_owned_by_callee: bool,
        callee_needs_ctx: bool,
    ) -> bool {
        !has_info
            && guard_class_id == 0
            && vh_judged_site == 0
            && !args_owned_by_callee
            && callee_needs_ctx
            && !self.failed
            && self.pending_shadow.is_empty()
            && !self.deferred_alloc_blind_spill
            && !self.deferred_alloc_shadow_push
            && !self
                .stack
                .iter()
                .any(|slot| matches!(slot, StackSlot::Scratch(..) | StackSlot::Xmm(..)))
    }

    /// nio20 wave 3, lane L3 (W2L2-1): emit the guarded prefix `prefix` names
    /// (one [`SP_THIN_PREFIX_ROWS`] row, matched by [`sp_thin_prefix_row`]) in
    /// front of a plain direct arm's thin-helper CALL, after the arm asked
    /// [`Self::sp_thin_helper_prefix_admitted`]. Returns the hit edge's
    /// `JMP rel32` patch site, the answer in RAX; `None` emitted nothing and
    /// the call stands alone. `arg_slots` are the popped arguments, the
    /// receiver first at the instance arm; `callee_params` excludes it.
    pub(in crate::x64::op_invoke) fn try_emit_sp_thin_prefix(
        &mut self,
        prefix: SpThinPrefix,
        pc: usize,
        callee_entry: usize,
        callee_params: usize,
        ret_type: u8,
        arg_slots: &[StackSlot],
    ) -> Option<usize> {
        if !prefix.enabled() {
            return None;
        }
        match prefix {
            SpThinPrefix::IntegerValueOfBox => match arg_slots {
                [value] => self.try_emit_value_of_box_prefix(pc, *value),
                _ => None,
            },
            SpThinPrefix::LongValueOfBox => match arg_slots {
                [value] => self.try_emit_long_value_of_box_prefix(pc, *value),
                _ => None,
            },
            SpThinPrefix::ObjectGetClass => self.try_emit_sp_getclass_prefix(pc, arg_slots),
            SpThinPrefix::UnsafeAccess => self.try_emit_sp_unsafe_access_prefix(
                pc,
                callee_entry,
                callee_params,
                ret_type,
                arg_slots,
            ),
            SpThinPrefix::IntegerIntValue => self.try_emit_sp_unbox_prefix(pc, false, arg_slots),
            SpThinPrefix::LongLongValue => self.try_emit_sp_unbox_prefix(pc, true, arg_slots),
            SpThinPrefix::ThreadCurrentThread => {
                let is_carrier = callee_entry != 0
                    && callee_entry == self.direct_helpers.thread_current_carrier_thread;
                self.try_emit_sp_current_thread_or_carrier_prefix(pc, is_carrier, arg_slots)
            }
            SpThinPrefix::ScopedAccess => {
                self.try_emit_sp_scoped_access_prefix(pc, callee_entry, arg_slots)
            }
            SpThinPrefix::IdentityHashCode => {
                self.try_emit_sp_identity_hash_prefix(pc, arg_slots)
            }
        }
    }

    /// nio20 wave 4, lane L3 (page
    /// `r20-sp-thread-current-thread-prefix-needs-a-per-vm-thread-word-FIXED-20261012.md`):
    /// `Thread.currentThread()` at a plain static site bound to the thin
    /// `jit_thread_current_thread_direct` helper, as a guarded prefix of that
    /// unchanged CALL ([`Self::sp_thin_helper_prefix_admitted`] for the
    /// contract): the helper's fast arm, inline.
    ///
    /// ```text
    ///   MOV RAX, gs/fs:[JIT_THREAD]          ; JvmThread* (TLS mirror)
    ///   TEST RAX,RAX ; JZ call               ; no JIT thread context
    ///   MOV RAX, [RAX+thread_java_obj_offset]; Option<ObjectRef>, 0 = None
    ///   TEST RAX,RAX ; JZ call               ; no mirror yet
    ///   JMP hit
    /// ```
    ///
    /// The VM publishes the offset only where the helper's per-call gate can
    /// never refuse for the VM's life (`thread_java_obj_offset_for`: not
    /// `--real-jdk`, an unclassified gate), so a hit answers exactly what the
    /// helper's fast arm would; every other case is the call. No Rust crossing
    /// is made (no `note_jit_boundary` owed), nothing allocates, collects or
    /// traps, and only RAX is written. Declines at compile time with no
    /// published offset, no TLS mirror displacement (the fetch would be a
    /// helper CALL), or `CRATONVM_JIT_SP_THIN_PREFIX=0`.
    pub(in crate::x64::op_invoke) fn try_emit_sp_identity_hash_prefix(
        &mut self,
        pc: usize,
        arg_slots: &[StackSlot],
    ) -> Option<usize> {
        if self.failed
            || crate::x64::zgc_read_barrier_blocks_inline_fields()
        {
            return None;
        }
        let [obj] = arg_slots else {
            return None;
        };
        let obj = *obj;
        if matches!(obj, StackSlot::Xmm(..))
            || self
                .stack
                .iter()
                .any(|s| matches!(s, StackSlot::Scratch(..)))
        {
            return None;
        }
        if crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] identity-hash-inline (sp-thin) {} @pc={pc}",
                self.method_label,
            );
        }
        let mut decline: Vec<usize> = Vec::new();
        self.load_slot_to_reg(RAX, obj);
        self.emit_test_r64_r64(RAX);
        let null_hit = Some(self.emit_jcc_rel32_patch(0x84)); // JZ
        let (read, edges) = crate::ir_lower::identity_hash_read_bytes();
        let at = self.buf.pos();
        self.buf.emit(&read);
        decline.extend(edges.into_iter().map(|e| at + e));
        if let Some(jz) = null_hit {
            self.patch_rel32_to_here(jz);
        }
        let hit = self.emit_jmp_rel32_patch();
        for p in decline {
            self.patch_rel32_to_here(p);
        }
        self.slot_mirror = None;
        Some(hit)
    }

    pub(in crate::x64::op_invoke) fn try_emit_sp_current_thread_prefix(
        &mut self,
        pc: usize,
        arg_slots: &[StackSlot],
    ) -> Option<usize> {
        self.try_emit_sp_current_thread_or_carrier_prefix(pc, false, arg_slots)
    }

    pub(in crate::x64) fn try_emit_sp_current_thread_or_carrier_prefix(
        &mut self,
        pc: usize,
        is_carrier: bool,
        arg_slots: &[StackSlot],
    ) -> Option<usize> {
        if self.failed || !arg_slots.is_empty() {
            return None;
        }
        let off = i32::try_from(self.direct_helpers.thread_java_obj_offset).ok()?;
        let tls_disp = u32::try_from(jit_thread_tls_disp()).ok()?;
        if off == 0 || tls_disp == 0 {
            return None;
        }
        if crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] current-thread-inline (sp) {} @pc={pc} off={off}",
                self.method_label,
            );
        }
        let mut decline: Vec<usize> = Vec::with_capacity(3);
        self.emit_mov_rax_tls_disp32(tls_disp);
        self.emit_test_r64_r64(RAX);
        decline.push(self.emit_jcc_rel32_patch(0x84)); // JZ
        if is_carrier && self.direct_helpers.vthread_state_offset >= 4 {
            let carrier_off = (self.direct_helpers.vthread_state_offset as i32) - 4;
            self.buf.emit(&[0x83, 0xB8]); // CMP DWORD PTR [RAX + disp32], 0
            self.buf.emit(&carrier_off.to_le_bytes());
            self.buf.emit(&[0x00]);
            decline.push(self.emit_jcc_rel32_patch(0x85)); // JNE decline
        }
        self.buf.emit(&[0x48, 0x8B, 0x80]); // MOV RAX, [RAX + disp32]
        self.buf.emit(&off.to_le_bytes());
        self.emit_test_r64_r64(RAX);
        decline.push(self.emit_jcc_rel32_patch(0x84)); // JZ
        let hit = self.emit_jmp_rel32_patch();
        for p in decline {
            self.patch_rel32_to_here(p);
        }
        self.slot_mirror = None;
        Some(hit)
    }

    /// JIT round 19 proposal wave 7, lane tail7 (OSRFRAME18-2,
    /// [`sp_tail_leaf_helpers_enabled`]): at a sibling-tail site whose
    /// context-taking helper keeps its frame, call the helper's LEAF TWIN
    /// first and return its answer straight away:
    ///
    /// ```text
    ///   <ctx + args in ABI registers>   ; emit_stack_arg_setup (Win64 shadow)
    ///   CALL twin                        ; frame UP, mirror names this frame
    ///   CMP RAX, decline ; JE cold       ; (TEST RAX,RAX for a 0 decline)
    ///   [narrow an ireturn]              ; as the ireturn arm does
    ///   <epilogue> ; RET                 ; what the tail form's callee did
    /// cold:                              ; the ordinary call, unchanged
    /// ```
    ///
    /// # Why the hit owes none of the VM-entry protocol
    ///
    /// The twin runs only its helper's fast arm (`vm/src/jit/helpers.rs`
    /// `jit_*_leaf`): no allocation, no raise (its panic guard RECORDS and
    /// declines), no Java, no dispatch, no safepoint. Nothing can walk this
    /// thread's stack while it runs, so this call needs no safepoint id, no
    /// register spill, no oop map at its return address, no frame republish
    /// (the twin is Rust: it never writes the mirror) and no sentinel check.
    /// And unlike the tail form it never runs with the frame torn down, so
    /// even a twin that broke its contract would be walked through a LIVE
    /// frame record (the residual risk is a stale safepoint id, not a dead
    /// frame). The ordinary call on the decline edge is byte for byte the
    /// call this site made before, with its whole protocol.
    ///
    /// # What the hit path may clobber
    ///
    /// The twin is a Rust function: it clobbers every caller-saved register.
    /// The hit path returns, so only the DECLINE edge reads the frame's state
    /// again, and it must find it intact: no operand or argument may live in a
    /// scratch or XMM register (caller-saved), no float local in a register
    /// the platform ABI does not preserve (SysV: every XMM), and nothing may be
    /// pending that the ordinary call's spill pairs with. The register-homed
    /// GPR locals are callee-saved (`LOCAL_REGS`). A self-locking body is
    /// refused: its return arms release the monitor, which the hit path would
    /// skip. Returns whether the prefix was emitted (`false` emits nothing).
    pub(in crate::x64::op_invoke) fn try_emit_sp_tail_leaf_twin(
        &mut self,
        return_op: u8,
        twin: usize,
        decline: i64,
        arg_slots: &[StackSlot],
    ) -> bool {
        if self.failed
            || twin == 0
            || self.self_lock_release_armed
            || self.self_lock_obj_off > 0
            || !self.pending_shadow.is_empty()
            || self.deferred_alloc_blind_spill
            || self.deferred_alloc_shadow_push
            || arg_slots
                .iter()
                .chain(self.stack.iter())
                .any(|slot| matches!(slot, StackSlot::Scratch(..) | StackSlot::Xmm(..)))
        {
            return false;
        }
        if cfg!(not(target_os = "windows")) && !self.alloc_used_xmms.is_empty() {
            return false;
        }
        if crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] tail-leaf-twin (sp) {} twin={twin:#x} decline={decline}",
                self.method_label,
            );
        }
        let total_sub = self.emit_stack_arg_setup(arg_slots, true);
        self.emit_call_absolute(twin);
        self.emit_stack_arg_cleanup(total_sub);
        self.slot_mirror = None;
        self.emit_cmp_rax_leaf_twin_decline(decline);
        let cold = self.emit_jcc_rel32_patch(0x84); // JE cold
        if return_op == 0xac {
            let tag = crate::narrowed_int_return_tag(&self.method_key);
            self.emit_narrow_int_return(tag);
        }
        self.emit_epilogue();
        self.patch_rel32_to_here(cold);
        true
    }

    /// Set ZF exactly when RAX holds a leaf twin's `decline`: `TEST RAX, RAX`
    /// for `0`, `CMP RAX, imm8` / `imm32` when it fits (sign-extended), and
    /// otherwise (TAIL7-19-3, lane tail8: the hash twin's decline lies outside
    /// the `int` range) `MOV R11, imm64 ; CMP RAX, R11`. R11 is caller-saved:
    /// the twin CALL just clobbered it, and nothing the site keeps lives there
    /// (the twin sites refuse every scratch-register operand).
    pub(in crate::x64::op_invoke) fn emit_cmp_rax_leaf_twin_decline(&mut self, decline: i64) {
        if decline == 0 {
            self.emit_test_r64_r64(RAX);
        } else if let Ok(imm) = i8::try_from(decline) {
            // CMP RAX, imm8 (REX.W 83 /7 ib, ModRM 0xF8 = mod11 /7 rm=RAX).
            self.buf.emit(&[0x48, 0x83, 0xF8, imm as u8]); // Cast: imm8 encoding
        } else if let Ok(imm) = i32::try_from(decline) {
            // CMP RAX, imm32 (REX.W 81 /7 id, sign-extended).
            self.buf.emit(&[0x48, 0x81, 0xF8]);
            self.buf.emit(&imm.to_le_bytes());
        } else {
            self.emit_mov_imm64(R11, decline);
            // CMP RAX, R11 (REX.W+R 39 /r: ModRM mod11 reg=R11&7 rm=RAX).
            self.buf.emit(&[0x4C, 0x39, 0xD8]);
        }
    }

    /// TAIL7-19-3 (lane tail8, [`sp_tail_leaf_helpers_wide_enabled`]): the
    /// INSTANCE direct arm's leaf-twin site -- `return tl.get();`,
    /// `return U.compareAndSetInt(o, off, e, n);` -- with
    /// [`Self::try_emit_sp_tail_leaf_twin`]'s contract. Asked after the
    /// receiver's null check and the thin prefixes, before anything of the
    /// ordinary call (service copy, spill, safepoint id) is emitted, so the
    /// decline edge is that call, byte for byte.
    ///
    /// Refused (emits nothing): the twin rows are not this switch's; the site
    /// is not a tail (an `xreturn` of the call's type next; a merge point
    /// there is fine, as the hit path returns by itself); it is protected (a
    /// handler would see a frame the hit path returns from); a thin prefix
    /// was emitted (its decline already IS this
    /// call, and the prefix serves the fast arm without one); a judged
    /// `VarHandle` row (its context register is the site record) or
    /// callee-owned arguments (their oop registers are pending for the call).
    #[allow(clippy::too_many_arguments)]
    pub(in crate::x64::op_invoke) fn try_emit_sp_instance_tail_leaf_twin(
        &mut self,
        code: &[u8],
        code_len: usize,
        pc: usize,
        invoke_len: usize,
        branch_targets: &[bool],
        callee_entry: usize,
        callee_needs_ctx: bool,
        has_info: bool,
        ret_type: u8,
        prefix_emitted: bool,
        vh_judged_site: usize,
        args_owned_by_callee: bool,
        arg_slots: &[StackSlot],
    ) -> bool {
        if !sp_tail_leaf_helpers_wide_enabled()
            || !callee_needs_ctx
            || has_info
            || prefix_emitted
            || vh_judged_site != 0
            || args_owned_by_callee
            || !sp_tailcall_enabled()
        {
            return false;
        }
        // The `xreturn` may be a merge point: the hit path returns by itself
        // and the decline lowers the call and the `xreturn` as usual (see the
        // static arm's `twin_site_shape`). `branch_targets` is kept for the
        // shape's bounds only.
        let next = pc + invoke_len;
        if next >= code_len
            || next >= branch_targets.len()
            || !tail_return_op_matches(ret_type, code[next])
            || self.pc_is_protected(pc)
        {
            return false;
        }
        let crate::direct_helpers::ThinHelperVmReach::LeafTwin { twin, decline } = self
            .direct_helpers
            .thin_helper_vm_reach_with(callee_entry, false, true)
        else {
            return false;
        };
        self.try_emit_sp_tail_leaf_twin(code[next], twin, decline, arg_slots)
    }

    /// MEGA619-1 (lane tail7, [`sp_lazy_return_republish_enabled`]): may the
    /// call at `pc`, `invoke_len` bytes long, drop its post-call republish?
    /// [`lazy_return_republish_rule`] over this compile's state. `false`
    /// without precise maps or a frame-record helper (then no republish is
    /// emitted anyway).
    pub(in crate::x64::op_invoke) fn sp_lazy_return_republish_admitted(
        &self,
        code: &[u8],
        pc: usize,
        invoke_len: usize,
        branch_targets: &[bool],
        sync_site: bool,
        ret_type: u8,
    ) -> bool {
        let window = sp_lazy_return_republish_window_enabled();
        if !self.precise_maps
            || self.helpers.frame_record == 0
            || !(sp_lazy_return_republish_enabled() || window)
        {
            return false;
        }
        // TAIL7-19-1 (lane tail8): under the window switch the `xreturn` may
        // follow a VM-free straight-line run; the rule then judges that
        // `xreturn` (the run's own merge points were checked by the walk).
        let next = if window {
            lazy_return_window_end(code, pc + invoke_len, branch_targets).unwrap_or(pc + invoke_len)
        } else {
            pc + invoke_len
        };
        let admitted = lazy_return_republish_rule(
            code.get(next).copied(),
            branch_targets.get(next).copied().unwrap_or(true),
            self.pc_is_protected(pc),
            self.self_lock_release_armed || self.self_lock_obj_off > 0,
            sync_site,
            ret_type,
            shadow_watch() && self.shadow_savebase_slot_off != 0,
        );
        if admitted && crate::x64::dbg_jitc_enabled() {
            let window_ops = next - (pc + invoke_len);
            eprintln!(
                "[cratonvm-jitc] lazy-return-republish (sp) {} @pc={pc} window={window_ops}",
                self.method_label,
            );
        }
        admitted
    }

    /// nio20 wave 3, lane L3 (W2L2-2): `Integer.intValue()` (`wide == false`)
    /// / `Long.longValue()` (`wide == true`) at a plain instance site bound to
    /// the thin `jit_integer_int_value_direct` / `jit_long_long_value_direct`
    /// helper, as a guarded prefix of that unchanged CALL
    /// ([`Self::sp_thin_helper_prefix_admitted`] for the contract). The
    /// `JitIntrinsic::{IntegerIntValue,LongLongValue}` arm's load, with its
    /// deopts turned into declines to the call: a null receiver (the helper
    /// raises the NPE with the receiver action), an array or another class
    /// (the helper validates and dispatches), and a layout replacement since
    /// the compile (the epoch guard). The payload is read per object through
    /// the compact / legacy branch, from the offsets `AtomicIntFieldLayout` /
    /// `AtomicLongFieldLayout` give for slot 0 of the class the VM names
    /// (`integer_class_id`, `ir_lower::ir_long_class_id`); an `int` is
    /// sign-extended into RAX as the helper returns it.
    ///
    /// ```text
    ///   [epoch guard ; JNE call]
    ///   RAX = recv ; TEST RAX,RAX ; JZ call
    ///   TEST BYTE [RAX+KIND_TAGS], KIND_TAG_BYTE_MASK ; JNZ call
    ///   CMP DWORD [RAX+class_id], cid ; JNE call
    ///   TEST BYTE [RAX+GC_FLAGS], COMPACT ; JZ legacy
    ///   MOV ECX/RCX, [RAX+compact] ; JMP done
    /// legacy: MOV ECX/RCX, [RAX+legacy]
    /// done: MOVSXD RAX, ECX / MOV RAX, RCX ; JMP hit
    /// ```
    ///
    /// Declines at compile time under `CRATONVM_JIT_SP_THIN_PREFIX=0`,
    /// `CRATONVM_JIT_NO_BOX_UNBOX_INTRINSIC`, with no class id or no
    /// layout for slot 0, and for an XMM receiver. Writes RAX, RCX and R11
    /// only (the epoch guard's fallback form), as the contract allows.
    pub(in crate::x64::op_invoke) fn try_emit_sp_unbox_prefix(
        &mut self,
        pc: usize,
        wide: bool,
        arg_slots: &[StackSlot],
    ) -> Option<usize> {
        if crate::box_unbox_intrinsic_disabled() || self.failed {
            return None;
        }
        let [recv] = arg_slots else {
            return None;
        };
        let recv = *recv;
        if matches!(recv, StackSlot::Xmm(..)) {
            return None;
        }
        let cid = if wide {
            crate::ir_lower::ir_long_class_id(&self.direct_helpers)?
        } else {
            self.direct_helpers.integer_class_id()?
        };
        let (compact_off, legacy_off) = if wide {
            crate::AtomicLongFieldLayout::new(0, cid)
                .map(|l| (l.value_compact_offset, l.value_legacy_offset))?
        } else {
            crate::AtomicIntFieldLayout::new(0, cid)
                .map(|l| (l.value_compact_offset, l.value_legacy_offset))?
        };
        // Resolved before any byte is emitted: a refusal after the first
        // decline branch would leave it unpatched.
        let cid_off = i32::try_from(self.helpers.class_id_offset_in_obj).ok()?;
        if crate::x64::dbg_jitc_enabled() {
            let name = if wide {
                "Long.longValue"
            } else {
                "Integer.intValue"
            };
            eprintln!(
                "[cratonvm-jitc] unbox-inline (sp) {} @pc={pc} {name} class_id={cid} \
                 compact_off={compact_off} legacy_off={legacy_off}",
                self.method_label,
            );
        }
        let mut decline: Vec<usize> = Vec::new();
        // First, before any operand load: the guard's fallback form clobbers
        // RCX and R11.
        if jit_sp_field_layout_guard_enabled() {
            decline.extend(self.emit_layout_epoch_guard());
        }
        // JZ: a null receiver takes the call (its NPE).
        self.load_slot_to_reg(RAX, recv);
        self.emit_test_r64_r64(RAX);
        decline.push(self.emit_jcc_rel32_patch(0x84));

        // JNE: a plain object only (an array's header carries its COMPONENT's id).
        self.emit_test_mem8_imm8(
            RAX,
            cratonvm_types::KIND_TAGS_BYTE_OFFSET as i32, // Cast: small header constant
            cratonvm_types::KIND_TAG_BYTE_MASK,
        );
        decline.push(self.emit_jcc_rel32_patch(0x85));

        // CMP DWORD [RAX + class_id], cid (81 /7 id, mod=10 rm=RAX) ; JNE.
        self.buf.emit(&[0x81, 0xB8]);
        self.buf.emit(&cid_off.to_le_bytes());
        self.buf.emit(&cid.to_le_bytes());
        decline.push(self.emit_jcc_rel32_patch(0x85));

        // The per-object layout branch of the `JitIntrinsic` arm.
        self.emit_test_mem8_imm8(
            RAX,
            cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32, // Cast: small header constant
            cratonvm_types::GC_FLAG_COMPACT,
        );
        let legacy = self.emit_jcc_rel32_patch(0x84); // JZ
        if wide {
            self.buf.emit(&[0x48, 0x8B, 0x88]); // MOV RCX, [RAX + disp32]
        } else {
            self.buf.emit(&[0x8B, 0x88]); // MOV ECX, [RAX + disp32]
        }
        self.buf.emit(&compact_off.to_le_bytes());
        let done = self.emit_jmp_rel32_patch();
        self.patch_rel32_to_here(legacy);
        if wide {
            self.buf.emit(&[0x48, 0x8B, 0x88]); // MOV RCX, [RAX + disp32]
        } else {
            self.buf.emit(&[0x8B, 0x88]); // MOV ECX, [RAX + disp32]
        }
        self.buf.emit(&legacy_off.to_le_bytes());
        self.patch_rel32_to_here(done);
        if wide {
            self.buf.emit(&[0x48, 0x89, 0xC8]); // MOV RAX, RCX
        } else {
            self.buf.emit(&[0x48, 0x63, 0xC1]); // MOVSXD RAX, ECX
        }
        let hit = self.emit_jmp_rel32_patch();
        for p in decline {
            self.patch_rel32_to_here(p);
        }
        self.slot_mirror = None;
        Some(hit)
    }

    /// `Object.getClass()` in a SINGLE-PASS body (method-entry and OSR doors)
    /// as two loads (nio20 wave 2, lane L2; the single-pass half of NIO19-5,
    /// `r20-getclass-two-loads-single-pass-and-osr-tiers-FIXED-20261008.md`): the
    /// twin of `ir_lower::try_emit_ir_getclass_prefix`, reading the same per-VM
    /// word table (`DirectHelperTable::getclass_mirror_words`, written by the
    /// VM's `ClassRealm::getclass_mirror_words` under the `class_mirrors`
    /// lock; a non-zero word always equals `class_mirrors[class_id]`).
    ///
    /// A guarded prefix of the site's unchanged `jit_object_get_class_direct`
    /// CALL ([`Self::sp_thin_helper_prefix_admitted`] for the contract).
    /// Returns the hit edge's `JMP rel32` patch site, the mirror in RAX.
    /// Declines, to the call, exactly where the optimizing tier's prefix does:
    /// a null receiver (the arm's own null check raised it already; this test
    /// is belt and braces), an array with no published band word (its header
    /// carries the COMPONENT's id; since nio21 the primitive- and
    /// reference-array bands answer, `ir_lower::getclass_array_arm_bytes`),
    /// an id past the table and an unpublished word.
    ///
    /// ```text
    ///   RAX = recv ; TEST RAX,RAX ; JZ call
    ///   TEST BYTE [RAX+KIND_TAGS], KIND_TAG_BYTE_MASK ; JNZ call
    ///   MOV  EAX, [RAX+class_id]          ; zero-extended
    ///   MOV  R10, table
    ///   CMP  RAX, [R10] ; JAE call        ; word 0 = capacity
    ///   MOV  RAX, [R10+RAX*8+8] ; TEST RAX,RAX ; JZ call
    ///   JMP  hit
    /// ```
    ///
    /// Declines at compile time under `CRATONVM_JIT_GETCLASS_INLINE=0`, with no
    /// table on this compile, and under an armed ZGC read barrier (the words
    /// are rewritten only while mutators are parked, never colored; the gate
    /// every inline reference load shares).
    pub(in crate::x64) fn try_emit_sp_getclass_prefix(&mut self, pc: usize, arg_slots: &[StackSlot]) -> Option<usize> {
        let table = self.direct_helpers.getclass_mirror_words;
        if table == 0
            || self.direct_helpers.object_get_class == 0
            || !crate::getclass_inline_enabled()
        {
            if std::env::var_os("DBG_MI").is_some() && self.method_label.contains("TomcatHack") {
                eprintln!(
                    "[DBG_MI_GC] decline table={:#x} obj_gc={:#x} en={} zgc={}",
                    table,
                    self.direct_helpers.object_get_class,
                    crate::getclass_inline_enabled(),
                    crate::x64::zgc_read_barrier_blocks_inline_fields(),
                );
            }
            return None;
        }
        let [recv] = arg_slots else {
            return None;
        };
        let recv = *recv;
        if matches!(recv, StackSlot::Xmm(..)) {
            return None;
        }
        // Resolved before any byte is emitted: a refusal after the first
        // decline branch would leave it unpatched.
        let cid_off = i32::try_from(self.helpers.class_id_offset_in_obj).ok()?;
        let table_imm = i64::try_from(table).ok()?;
        if crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] object-getclass-inline (sp) {} @pc={pc} table={table:#x}",
                self.method_label,
            );
        }
        // nio21 wave 1 L5: the optimizing tier's primitive-array arm, same
        // bytes, same switch.
        let prim_arrays = crate::ir_lower::getclass_prim_array_words_enabled();
        // nio21 wave 2 L5: and its reference-array arm.
        let ref_arrays = crate::ir_lower::getclass_ref_array_words_enabled();
        let mut decline: Vec<usize> = Vec::new();
        self.load_slot_to_reg(RAX, recv);
        self.emit_test_r64_r64(RAX);
        decline.push(self.emit_jcc_rel32_patch(0x84)); // JZ
                                                       // R10 = the table, before the kind test (both arms read it).
        self.emit_mov_imm64(R10, table_imm);
        // A plain object takes the class-id lookup (TEST the kind bits: a
        // hashed compact instance carries identity-hash bits in the rest of
        // this byte).
        self.emit_test_mem8_imm8(
            RAX,
            cratonvm_types::KIND_TAGS_BYTE_OFFSET as i32, // Cast: small header constant
            cratonvm_types::KIND_TAG_BYTE_MASK,
        );
        let not_plain = self.emit_jcc_rel32_patch(0x85); // JNE
                                                         // MOV EAX, [RAX + class_id]: the 32-bit load zero-extends, so the
                                                         // index below is unsigned.
        self.emit_mov_r32_mem_disp32(RAX, RAX, cid_off);
        // The class's word, or (nio20 wave 5 L2) a lambda proxy's in the
        // table's second band; the optimizing tier's exact bytes
        // (`ir_lower::getclass_table_lookup_bytes`). Its declines: an id with no
        // word, an unpublished word.
        let (lookup, edges) = crate::ir_lower::getclass_table_lookup_bytes();
        let at = self.buf.pos();
        self.buf.emit(&lookup);
        decline.extend(edges.into_iter().map(|e| at + e));
        if prim_arrays || ref_arrays {
            // JMP loaded ; array: <array arm> ; loaded:
            let to_loaded = self.emit_jmp_rel32_patch();
            self.patch_rel32_to_here(not_plain);
            let (arm, edges) = crate::ir_lower::getclass_array_arm_bytes(ref_arrays, prim_arrays);
            let at = self.buf.pos();
            self.buf.emit(&arm);
            decline.extend(edges.into_iter().map(|e| at + e));
            self.patch_rel32_to_here(to_loaded);
        } else {
            decline.push(not_plain);
        }
        let hit = self.emit_jmp_rel32_patch();
        for p in decline {
            self.patch_rel32_to_here(p);
        }
        self.slot_mirror = None;
        Some(hit)
    }

    /// The `Unsafe` primitive-array accessors in a SINGLE-PASS body (nio20
    /// wave 2, lane L2; `r20-unsafe-accessor-inline-is-optimizing-tier-only-FIXED-20261012.md`):
    /// the twin of `ir_lower::Lowerer::try_emit_ir_unsafe_access_prefix`, as a
    /// guarded prefix of the site's unchanged thin-helper CALL
    /// (`jit_unsafe_get_int_direct` & co; [`Self::sp_thin_helper_prefix_admitted`]
    /// for the contract). Returns the hit edge's `JMP rel32` patch site; a
    /// get's value is in RAX, widened as the helper widens it.
    ///
    /// The site is recognised by the helper the door bound, mapped back to its
    /// `UnsafeInlineAccess::GATE_BITS` row and through the SAME recogniser and
    /// admission word the optimizing tier asks
    /// (`direct_helpers::unsafe_inline_access_site`,
    /// `DirectHelperTable::unsafe_inline_admitted`), so the two tiers cannot
    /// disagree about what is served. Only the eight accessors this backend
    /// binds to a thin helper reach here (`get/put{Byte,Short,Int,Long}`);
    /// `getChar`/`putChar` and the `*Unaligned` bodies are not thin-bound in a
    /// single-pass body and keep their call.
    ///
    /// Same bytes and the same answer as the optimizing tier: a non-null base
    /// whose header says ARRAY of a PRIMITIVE element type,
    /// `offset >= ARRAY_DATA_OFFSET` (signed) and
    /// `offset - ARRAY_DATA_OFFSET + width <= length << log2(element size)`;
    /// every other shape (null receiver or base, a plain object, a reference
    /// array, an out-of-range offset) declines to the call. No GC barrier: the
    /// access moves primitive bytes only.
    pub(in crate::x64::op_invoke) fn try_emit_sp_unsafe_access_prefix(
        &mut self,
        pc: usize,
        callee_entry: usize,
        callee_params: usize,
        ret_type: u8,
        arg_slots: &[StackSlot],
    ) -> Option<usize> {
        let mask = self.direct_helpers.unsafe_inline_admitted;
        if mask == 0
            || callee_entry == 0
            || !crate::direct_helpers::unsafe_accessor_inline_enabled()
        {
            return None;
        }
        let dh = &self.direct_helpers;
        // JIT round 20 wave 12, lane qUnsafe (AT-4): the
        // `compareAndSetReference` thin bind takes the reference-field arm.
        if callee_entry == dh.unsafe_cas_reference {
            if ret_type != b'Z'
                || callee_params != 4
                || arg_slots.len() != 5
                || arg_slots
                    .iter()
                    .any(|slot| matches!(slot, StackSlot::Xmm(..)))
                || !crate::direct_helpers::unsafe_inline_ref_cas_site(
                    "jdk/internal/misc/Unsafe",
                    "compareAndSetReference",
                    "(Ljava/lang/Object;JLjava/lang/Object;Ljava/lang/Object;)Z",
                )
            {
                return None;
            }
            return self.emit_sp_unsafe_ref_cas_core(pc, arg_slots);
        }
        // JIT round 21 wave 1, lane updaters (UPDATERS21-1): the
        // `getReferenceVolatile` / `putReferenceVolatile` thin binds take the
        // reference-field arm (`emit_sp_unsafe_ref_volatile_core`).
        if callee_entry == dh.unsafe_get_reference_volatile
            || callee_entry == dh.unsafe_put_reference_volatile
        {
            let is_put = callee_entry == dh.unsafe_put_reference_volatile;
            let (method, desc) = if is_put {
                ("putReferenceVolatile", "(Ljava/lang/Object;JLjava/lang/Object;)V")
            } else {
                ("getReferenceVolatile", "(Ljava/lang/Object;J)Ljava/lang/Object;")
            };
            let access = crate::direct_helpers::unsafe_inline_ref_volatile_site(
                "jdk/internal/misc/Unsafe",
                method,
                desc,
            )?;
            if ret_type != (if is_put { b'V' } else { b'L' })
                || callee_params != access.num_params()
                || arg_slots.len() != 1 + access.num_params()
                || arg_slots
                    .iter()
                    .any(|slot| matches!(slot, StackSlot::Xmm(..)))
            {
                return None;
            }
            return self.emit_sp_unsafe_ref_volatile_core(pc, access, arg_slots);
        }
        // nio21 wave 5 L3: the `compareAndSet{Int,Long}` thin binds (rows 14
        // and 15) take the RMW prefix, `LOCK CMPXCHG` on an aligned
        // primitive-array element.
        let cas_row = if callee_entry == dh.unsafe_cas_int {
            Some(14)
        } else if callee_entry == dh.unsafe_cas_long {
            Some(15)
        } else {
            None
        };
        if let Some(row) = cas_row {
            let (name, desc) = crate::direct_helpers::UnsafeInlineAccess::GATE_BITS[row];
            let rmw = crate::direct_helpers::unsafe_inline_rmw_site(
                "jdk/internal/misc/Unsafe",
                name,
                desc,
            )?;
            if !rmw.admitted(mask)
                || ret_type != rmw.ret_type()
                || callee_params != rmw.num_params()
                || arg_slots.len() != 1 + callee_params
                || arg_slots
                    .iter()
                    .any(|slot| matches!(slot, StackSlot::Xmm(..)))
            {
                return None;
            }
            if crate::x64::dbg_jitc_enabled() {
                eprintln!(
                    "[cratonvm-jitc] unsafe-rmw-inline (sp) {} @pc={pc} {name}{desc}",
                    self.method_label,
                );
            }
            return Some(self.emit_sp_unsafe_rmw_core(rmw, arg_slots));
        }
        // The `GATE_BITS` row of each thin-bound accessor.
        let row = if callee_entry == dh.unsafe_get_byte {
            0
        } else if callee_entry == dh.unsafe_get_short {
            1
        } else if callee_entry == dh.unsafe_get_int {
            3
        } else if callee_entry == dh.unsafe_get_long {
            4
        } else if callee_entry == dh.unsafe_put_byte {
            5
        } else if callee_entry == dh.unsafe_put_short {
            6
        } else if callee_entry == dh.unsafe_put_int {
            8
        } else if callee_entry == dh.unsafe_put_long {
            9
        // nio21 wave 4 L4: the volatile int/long thin binds (wave 3's
        // helpers), rows 10..14; aligned primitive-array elements only.
        } else if callee_entry == dh.unsafe_get_int_volatile {
            10
        } else if callee_entry == dh.unsafe_get_long_volatile {
            11
        } else if callee_entry == dh.unsafe_put_int_volatile {
            12
        } else if callee_entry == dh.unsafe_put_long_volatile {
            13
        } else {
            return None;
        };
        let (name, desc) = crate::direct_helpers::UnsafeInlineAccess::GATE_BITS[row];
        let access = crate::direct_helpers::unsafe_inline_access_site(
            "jdk/internal/misc/Unsafe",
            name,
            desc,
        )?;
        if access.unaligned || access.big_endian_operand || !access.admitted(mask) {
            return None;
        }
        let want_ret = if access.is_put {
            b'V'
        } else {
            match (access.width, access.signed) {
                (1, _) => b'B',
                (2, true) => b'S',
                (2, false) => b'C',
                (4, _) => b'I',
                _ => b'J',
            }
        };
        if ret_type != want_ret
            || callee_params != access.num_params()
            || arg_slots.len() != 1 + callee_params
            || arg_slots
                .iter()
                .any(|slot| matches!(slot, StackSlot::Xmm(..)))
        {
            return None;
        }
        let recv = arg_slots[0];
        let base = arg_slots[1];
        let offset = arg_slots[2];
        let value = if access.is_put {
            Some(arg_slots[3])
        } else {
            None
        };
        if crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] unsafe-inline (sp) {} @pc={pc} {name}{desc} width={} put={}",
                self.method_label, access.width, access.is_put,
            );
        }
        Some(self.emit_sp_unsafe_access_core(access, recv, base, offset, value, None))
    }

    /// nio20 wave 4, lane L3 (the residual of
    /// `r20-unsafe-accessor-inline-is-optimizing-tier-only-FIXED-20261012.md`): the
    /// `Unsafe` primitive-array prefix at the DISPATCH arm, for the accessors
    /// that reach a single-pass body with a `JitInvokeInfo` and no thin bind
    /// -- `getChar` / `putChar` (natives with no thin helper) and the
    /// little-endian `{get,put}{Short,Char,Int,Long}Unaligned` forms when no
    /// compiled body was bound. Same recogniser, admission word and bytes as
    /// [`Self::try_emit_sp_unsafe_access_prefix`] and the optimizing tier's
    /// prefix (`direct_helpers::unsafe_inline_access_site`,
    /// `UnsafeInlineAccess::admitted` against
    /// `DirectHelperTable::unsafe_inline_admitted`), so every tier serves the
    /// same set.
    ///
    /// The shape of [`Self::try_emit_arraylist_access_prefix`]: emitted before
    /// the dispatch pops anything, operands read off the virtual stack, every
    /// decline lands on the first byte of the UNCHANGED dispatch, and the hit
    /// edge is patched at the arm's result push with the answer in RAX (a hit
    /// makes no call, so it owes neither the map nor the exception check the
    /// dispatch emits before that push). The `bigEndian` four-operand forms
    /// decline (a byte swap this prefix does not emit). Off under
    /// `CRATONVM_JIT_SP_THIN_PREFIX=0` and the family's
    /// `CRATONVM_JIT_UNSAFE_ACCESSOR_INLINE=0`.
    pub(in crate::x64::op_invoke) fn try_emit_sp_unsafe_dispatch_prefix(
        &mut self,
        pc: usize,
        info: &crate::JitInvokeInfo,
    ) -> Option<usize> {
        let mask = self.direct_helpers.unsafe_inline_admitted;
        if mask == 0
            || self.failed
            || !sp_thin_prefix_enabled()
            || !crate::direct_helpers::unsafe_accessor_inline_enabled()
        {
            return None;
        }
        let Some(access) = crate::direct_helpers::unsafe_inline_access_site(
            info.class_name,
            info.method_name,
            info.descriptor,
        ) else {
            // nio21 wave 5 L3: the CAS / getAndAdd / getAndSet rows (the
            // `weakCompareAndSet*` / `getAndAdd*` / `getAndSet*` Java bodies
            // reach a single-pass body here when no compiled body was bound).
            return self.try_emit_sp_unsafe_rmw_dispatch_prefix(pc, info, mask);
        };
        // `Unsafe` is final: a virtual (0) or statically bound (1) site names
        // this very method; never an interface or static row. JIT round 20
        // (lane memory): the four-operand `*Unaligned(.., boolean bigEndian)`
        // forms are served too, their trailing operand swapping the bytes at
        // run time (`sp_unsafe_big_endian_prefix_enabled`).
        if (access.big_endian_operand && !sp_unsafe_big_endian_prefix_enabled())
            || !access.admitted(mask)
            || !matches!(info.invoke_kind, 0 | 1)
        {
            return None;
        }
        let depth = 1 + access.num_params();
        if info.num_jit_args != depth || self.stack.len() < depth {
            return None;
        }
        let ops = &self.stack[self.stack.len() - depth..];
        // The core writes RAX, RCX, RDX, R10 and R11: no operand may live in
        // a scratch register (the walk flushed them before this arm, as the
        // ArrayList prefix also asks) or an XMM one.
        if ops.iter().any(|s| matches!(s, StackSlot::Xmm(..)))
            || self
                .stack
                .iter()
                .any(|s| matches!(s, StackSlot::Scratch(..)))
        {
            return None;
        }
        let recv = ops[0];
        let base = ops[1];
        let offset = ops[2];
        let value = access.is_put.then(|| ops[3]);
        // `[recv, base, offset, (value,) bigEndian]`: the last operand.
        let order = access.big_endian_operand.then(|| ops[depth - 1]);
        if crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] unsafe-inline (sp-dispatch) {} @pc={pc} {}{} width={} put={} unaligned={} big_endian_operand={}",
                self.method_label,
                info.method_name,
                info.descriptor,
                access.width,
                access.is_put,
                access.unaligned,
                access.big_endian_operand,
            );
        }
        Some(self.emit_sp_unsafe_access_core(access, recv, base, offset, value, order))
    }

    /// nio21 wave 5 L3: [`Self::try_emit_sp_unsafe_dispatch_prefix`] for the
    /// `Unsafe` CAS / getAndAdd / getAndSet rows
    /// (`direct_helpers::unsafe_inline_rmw_site`): the same placement, operand
    /// screen and hit contract, with [`Self::emit_sp_unsafe_rmw_core`]'s bytes.
    pub(in crate::x64::op_invoke) fn try_emit_sp_unsafe_rmw_dispatch_prefix(
        &mut self,
        pc: usize,
        info: &crate::JitInvokeInfo,
        mask: usize,
    ) -> Option<usize> {
        let Some(rmw) = crate::direct_helpers::unsafe_inline_rmw_site(
            info.class_name,
            info.method_name,
            info.descriptor,
        ) else {
            // JIT round 21, lane atomics (UNSAFE21-2): the sub-word CAS rows
            // (natives with no thin helper: only this dispatch arm).
            if let Some(sub) = crate::direct_helpers::unsafe_inline_subword_site(
                info.class_name,
                info.method_name,
                info.descriptor,
            ) {
                if !matches!(info.invoke_kind, 0 | 1)
                    || info.num_jit_args != 5
                    || self.stack.len() < 5
                {
                    return None;
                }
                let ops: Vec<StackSlot> = self.stack[self.stack.len() - 5..].to_vec();
                if ops.iter().any(|s| matches!(s, StackSlot::Xmm(..)))
                    || self
                        .stack
                        .iter()
                        .any(|s| matches!(s, StackSlot::Scratch(..)))
                {
                    return None;
                }
                return self.emit_sp_unsafe_subword_core(pc, sub, &ops);
            }
            // JIT round 21 wave 1, lane updaters (UPDATERS21-1): the
            // reference volatile get / put rows with no thin bind (and the
            // acquire / opaque / release bodies).
            if let Some(access) = crate::direct_helpers::unsafe_inline_ref_volatile_site(
                info.class_name,
                info.method_name,
                info.descriptor,
            ) {
                let depth = 1 + access.num_params();
                if !matches!(info.invoke_kind, 0 | 1)
                    || info.num_jit_args != depth
                    || self.stack.len() < depth
                {
                    return None;
                }
                let ops: Vec<StackSlot> = self.stack[self.stack.len() - depth..].to_vec();
                if ops.iter().any(|s| matches!(s, StackSlot::Xmm(..)))
                    || self
                        .stack
                        .iter()
                        .any(|s| matches!(s, StackSlot::Scratch(..)))
                {
                    return None;
                }
                return self.emit_sp_unsafe_ref_volatile_core(pc, access, &ops);
            }
            // JIT round 20 wave 12, lane qUnsafe (AT-4): the reference CAS
            // row (`compareAndSetReference` with no thin bind, and the
            // `weakCompareAndSetReference*` bodies).
            if !crate::direct_helpers::unsafe_inline_ref_cas_site(
                info.class_name,
                info.method_name,
                info.descriptor,
            ) || !matches!(info.invoke_kind, 0 | 1)
                || info.num_jit_args != 5
                || self.stack.len() < 5
            {
                return None;
            }
            let ops: Vec<StackSlot> = self.stack[self.stack.len() - 5..].to_vec();
            if ops.iter().any(|s| matches!(s, StackSlot::Xmm(..)))
                || self
                    .stack
                    .iter()
                    .any(|s| matches!(s, StackSlot::Scratch(..)))
            {
                return None;
            }
            return self.emit_sp_unsafe_ref_cas_core(pc, &ops);
        };
        if !rmw.admitted(mask) || !matches!(info.invoke_kind, 0 | 1) {
            return None;
        }
        let depth = 1 + rmw.num_params();
        if info.num_jit_args != depth || self.stack.len() < depth {
            return None;
        }
        let ops: Vec<StackSlot> = self.stack[self.stack.len() - depth..].to_vec();
        if ops.iter().any(|s| matches!(s, StackSlot::Xmm(..)))
            || self
                .stack
                .iter()
                .any(|s| matches!(s, StackSlot::Scratch(..)))
        {
            return None;
        }
        if crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] unsafe-rmw-inline (sp-dispatch) {} @pc={pc} {}{} width={} op={:?}",
                self.method_label, info.method_name, info.descriptor, rmw.width, rmw.op,
            );
        }
        Some(self.emit_sp_unsafe_rmw_core(rmw, &ops))
    }

    /// The bytes both single-pass RMW prefixes emit (nio21 wave 5 L3), the
    /// twin of `ir_lower::Lowerer::try_emit_ir_unsafe_rmw_prefix`: `ops` is
    /// `[receiver, base, offset, a, (b)]`. The receiver and base null tests,
    /// [`Self::emit_sp_prim_array_screen`], the alignment screen
    /// (`direct_helpers::unsafe_inline_rmw_screen_bytes`), then the one
    /// locked instruction (`direct_helpers::unsafe_inline_rmw_bytes`); since
    /// nio21 wave 7 L3 a non-array base takes the instance-field arm
    /// (`direct_helpers::unsafe_inline_field_cell_bytes`) when the VM baked its
    /// cell table, joining the same instruction at its compact displacement.
    /// Every other shape declines to whatever code follows. Returns the hit edge's
    /// `JMP rel32` patch site with the site's result in RAX (`0`/`1`, or the
    /// previous value with an `int` sign-extended). Writes RAX, RCX, RDX,
    /// R10, R11.
    pub(in crate::x64::op_invoke) fn emit_sp_unsafe_rmw_core(
        &mut self,
        rmw: crate::direct_helpers::UnsafeInlineRmw,
        ops: &[StackSlot],
    ) -> usize {
        let w = rmw.width;
        let (recv, base, offset) = (ops[0], ops[1], ops[2]);
        let (expected, operand) = if rmw.has_expected() {
            (Some(ops[3]), ops[4])
        } else {
            (None, ops[3])
        };
        // nio21 wave 7 L3 (W6-L3-1): the instance-field arm, the optimizing
        // tier's (`direct_helpers::unsafe_field_arm_words`).
        let field_arm = crate::direct_helpers::unsafe_field_arm_words(&self.direct_helpers);
        // JIT round 21, lane atomics (STATICCAS21-2): the static arm, the
        // optimizing tier's (`direct_helpers::unsafe_static_arm_table`).
        let static_arm = crate::direct_helpers::unsafe_static_arm_table(&self.direct_helpers);
        let mut decline: Vec<usize> = Vec::new();
        // R10 = the `Unsafe` receiver; null declines (the call's NPE).
        self.load_slot_to_reg(R10, recv);
        self.emit_test_r64_r64(R10);
        decline.push(self.emit_jcc_rel32_patch(0x84)); // JZ
                                                       // RAX = base; null declines (no off-heap arm for an atomic).
        self.load_slot_to_reg(RAX, base);
        self.emit_test_r64_r64(RAX);
        decline.push(self.emit_jcc_rel32_patch(0x84)); // JZ
                                                       // A non-array base takes the field arm, emitted after the hit path.
        let to_field = (field_arm.is_some() || static_arm.is_some()).then(|| {
            let (bytes, edge) = crate::direct_helpers::unsafe_inline_not_array_bytes();
            let at = self.buf.pos();
            self.buf.emit(&bytes);
            at + edge
        });
        self.emit_sp_prim_array_screen(w, offset, &mut decline);
        let (screen, edges) = crate::direct_helpers::unsafe_inline_rmw_screen_bytes(w);
        let at = self.buf.pos();
        self.buf.emit(&screen);
        decline.extend(edges.into_iter().map(|e| at + e));
        // RAX = the base, R11 = the array offset or (from the field arm) the
        // compact displacement. A jump target: no reload elision across it.
        let join = self.buf.pos();
        self.slot_mirror = None;
        // R10 = expected (a CAS), RDX = the operand: the screen is done with
        // both, and no operand lives in a scratch register.
        if let Some(expected) = expected {
            self.load_slot_to_reg(R10, expected);
        }
        self.load_slot_to_reg(RDX, operand);
        self.buf
            .emit(&crate::direct_helpers::unsafe_inline_rmw_bytes(rmw));
        let hit = self.emit_jmp_rel32_patch();
        if let Some(to_field) = to_field {
            self.patch_rel32_to_here(to_field);
            self.load_slot_to_reg(R11, offset);
            // STATICCAS21-2 (lane atomics): a static-band offset on its
            // class's mirror joins with RAX = the statics block and R11 = the
            // static's payload displacement; any other offset falls through
            // to the instance-field screen with R11 still the offset.
            if let Some(table) = static_arm {
                let (bytes, edges, not_static) =
                    crate::direct_helpers::unsafe_inline_static_cell_bytes(table, w);
                let at = self.buf.pos();
                self.buf.emit(&bytes);
                decline.extend(edges.into_iter().map(|e| at + e));
                self.emit_jmp_back_to(join);
                if field_arm.is_some() {
                    self.patch_rel32_to_here(at + not_static);
                } else {
                    decline.push(at + not_static);
                }
            }
            if let Some((cells, epoch_addr)) = field_arm {
                let (bytes, edges) =
                    crate::direct_helpers::unsafe_inline_field_cell_bytes(cells, epoch_addr, w);
                let at = self.buf.pos();
                self.buf.emit(&bytes);
                decline.extend(edges.into_iter().map(|e| at + e));
                self.emit_jmp_back_to(join);
            }
        }
        for p in decline {
            self.patch_rel32_to_here(p);
        }
        self.slot_mirror = None;
        hit
    }

    /// JIT round 21, lane atomics (UNSAFE21-2): the single-pass twin of
    /// `ir_lower::Lowerer::try_emit_ir_unsafe_subword_prefix`; `ops` is
    /// `[receiver, base, offset, expected, new]`. The same pieces in the same
    /// order -- null tests, the not-array kind test (an array declines), the
    /// sub-word cell screen (`direct_helpers::unsafe_inline_subword_field_cell_bytes`)
    /// and the sub-word CAS bytes (`direct_helpers::unsafe_inline_subword_rmw_bytes`).
    /// Returns the hit edge's `JMP rel32` patch site with the result in RAX
    /// (the witness sign-extended, or `0` / `1`), or `None` (nothing emitted)
    /// when this compile has no sub-word arm
    /// (`direct_helpers::unsafe_subword_field_arm_words`). Writes RAX, RCX,
    /// RDX, R10, R11.
    pub(in crate::x64::op_invoke) fn emit_sp_unsafe_subword_core(
        &mut self,
        pc: usize,
        rmw: crate::direct_helpers::UnsafeSubwordRmw,
        ops: &[StackSlot],
    ) -> Option<usize> {
        if ops.len() != 5 || self.failed || !sp_thin_prefix_enabled() {
            return None;
        }
        let (cells, epoch_addr) =
            crate::direct_helpers::unsafe_subword_field_arm_words(&self.direct_helpers, rmw)?;
        let (recv, base, offset, expected, new) = (ops[0], ops[1], ops[2], ops[3], ops[4]);
        if crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] unsafe-subword-inline (sp) {} @pc={pc} width={} cae={}",
                self.method_label, rmw.width, rmw.cae,
            );
        }
        let mut decline: Vec<usize> = Vec::new();
        // R10 = the `Unsafe` receiver; null declines (the call's NPE).
        self.load_slot_to_reg(R10, recv);
        self.emit_test_r64_r64(R10);
        decline.push(self.emit_jcc_rel32_patch(0x84)); // JZ
                                                       // RAX = base; null declines.
        self.load_slot_to_reg(RAX, base);
        self.emit_test_r64_r64(RAX);
        decline.push(self.emit_jcc_rel32_patch(0x84)); // JZ
                                                       // An array base declines; a plain object jumps over that decline.
        let (bytes, edge) = crate::direct_helpers::unsafe_inline_not_array_bytes();
        let at = self.buf.pos();
        self.buf.emit(&bytes);
        let to_field = at + edge;
        decline.push(self.emit_jmp_rel32_patch());
        self.patch_rel32_to_here(to_field);
        self.slot_mirror = None;
        // R11 = the offset, then the published compact displacement.
        self.load_slot_to_reg(R11, offset);
        let (bytes, edges) = crate::direct_helpers::unsafe_inline_subword_field_cell_bytes(
            cells, epoch_addr, rmw.width,
        );
        let at = self.buf.pos();
        self.buf.emit(&bytes);
        decline.extend(edges.into_iter().map(|e| at + e));
        // R10 = expected, RDX = new; the CMPXCHG leaves the result in RAX.
        self.load_slot_to_reg(R10, expected);
        self.load_slot_to_reg(RDX, new);
        self.buf
            .emit(&crate::direct_helpers::unsafe_inline_subword_rmw_bytes(rmw));
        let hit = self.emit_jmp_rel32_patch();
        for p in decline {
            self.patch_rel32_to_here(p);
        }
        self.slot_mirror = None;
        Some(hit)
    }

    /// JIT round 20 wave 12, lane qUnsafe (AT-4 / W7-L3-2): the single-pass
    /// twin of `ir_lower::Lowerer::try_emit_ir_unsafe_ref_cas_prefix` for
    /// both single-pass `Unsafe` prefixes (the thin bind's and the dispatch
    /// arm's): `ops` is `[receiver, base, offset, expected, new]`. The same
    /// pieces in the same order -- null tests, the not-array kind test (an
    /// array declines), the reference-store gate screen
    /// (`ir_lower::vh_ref_store_gate_screen_bytes`), the reference cell
    /// screen (`direct_helpers::unsafe_inline_ref_field_cell_bytes`) and the
    /// RMW prefixes' CAS bytes. Returns the hit edge's `JMP rel32` patch site
    /// with `0`/`1` in RAX, or `None` (nothing emitted) when this compile has
    /// no reference arm: no admission bit / cell table / gate plan, an armed
    /// ZGC read barrier, narrow oops, or the single-pass prefixes off.
    pub(in crate::x64::op_invoke) fn emit_sp_unsafe_ref_cas_core(&mut self, pc: usize, ops: &[StackSlot]) -> Option<usize> {
        if ops.len() != 5
            || self.failed
            || !sp_thin_prefix_enabled()
            || crate::x64::zgc_read_barrier_blocks_inline_fields()
        {
            return None;
        }
        let field = crate::direct_helpers::unsafe_ref_cas_field_arm_words(&self.direct_helpers);
        // j27 wave 6, lane T1DISP20 (DISP20-8, opt-in): the reference-array
        // element arm (`crate::unsafe_ref_array_arm`).
        let array = crate::unsafe_ref_array_arm::unsafe_ref_array_arm_admitted(
            &self.direct_helpers,
            crate::unsafe_ref_array_arm::UNSAFE_REF_ARRAY_CAS_GATE_BIT,
        );
        if field.is_none() && !array {
            return None;
        }
        let gates = crate::ir_lower::vh_ref_store_gates(
            self.ref_store_gates(),
            self.ref_store_post_skip_mask(),
        )?;
        let (recv, base, offset, expected, new) = (ops[0], ops[1], ops[2], ops[3], ops[4]);
        if crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] unsafe-ref-cas-inline (sp) {} @pc={pc}",
                self.method_label,
            );
        }
        let mut decline: Vec<usize> = Vec::new();
        // R10 = the `Unsafe` receiver; null declines (the call's NPE).
        self.load_slot_to_reg(R10, recv);
        self.emit_test_r64_r64(R10);
        decline.push(self.emit_jcc_rel32_patch(0x84)); // JZ
                                                       // RAX = base; null declines.
        self.load_slot_to_reg(RAX, base);
        self.emit_test_r64_r64(RAX);
        decline.push(self.emit_jcc_rel32_patch(0x84)); // JZ
                                                       // An array base declines (or, with the array arm, takes it); a plain
                                                       // object jumps to the field arm.
        let (bytes, edge) = crate::direct_helpers::unsafe_inline_not_array_bytes();
        let at = self.buf.pos();
        self.buf.emit(&bytes);
        let to_field = at + edge;
        let to_join = if array {
            crate::unsafe_ref_array_arm::note_ref_array_arm("sp cas", &self.method_label);
            // The barrier gates, before the store (RAX = holder, RDX = new).
            self.load_slot_to_reg(RDX, new);
            let (bytes, edges) = crate::ir_lower::vh_ref_store_gate_screen_bytes(gates);
            let at = self.buf.pos();
            self.buf.emit(&bytes);
            decline.extend(edges.into_iter().map(|e| at + e));
            // R11 = the offset, kept: `[RAX + R11]` is the element slot.
            self.load_slot_to_reg(R11, offset);
            let (bytes, edges) = crate::unsafe_ref_array_arm::unsafe_ref_array_screen_bytes();
            let at = self.buf.pos();
            self.buf.emit(&bytes);
            decline.extend(edges.into_iter().map(|e| at + e));
            Some(self.emit_jmp_rel32_patch())
        } else {
            decline.push(self.emit_jmp_rel32_patch());
            None
        };
        self.patch_rel32_to_here(to_field);
        self.slot_mirror = None;
        if let Some((cells, epoch_addr)) = field {
            // The barrier gates, before the store (RAX = holder, RDX = new).
            self.load_slot_to_reg(RDX, new);
            let (bytes, edges) = crate::ir_lower::vh_ref_store_gate_screen_bytes(gates);
            let at = self.buf.pos();
            self.buf.emit(&bytes);
            decline.extend(edges.into_iter().map(|e| at + e));
            // R11 = the offset, then the published compact displacement.
            self.load_slot_to_reg(R11, offset);
            let (bytes, edges) =
                crate::direct_helpers::unsafe_inline_ref_field_cell_bytes(cells, epoch_addr);
            let at = self.buf.pos();
            self.buf.emit(&bytes);
            decline.extend(edges.into_iter().map(|e| at + e));
        } else {
            decline.push(self.emit_jmp_rel32_patch());
        }
        // The array arm joins here (RAX = the base, R11 = the slot's offset
        // or displacement): a jump target, so no reload elision across it.
        if let Some(to_join) = to_join {
            self.patch_rel32_to_here(to_join);
            self.slot_mirror = None;
        }
        // R10 = expected, RDX = new; the CAS leaves `0`/`1` in RAX.
        self.load_slot_to_reg(R10, expected);
        self.load_slot_to_reg(RDX, new);
        self.buf
            .emit(&crate::direct_helpers::unsafe_inline_rmw_bytes(
                crate::direct_helpers::UnsafeInlineRmw {
                    width: 8,
                    op: crate::direct_helpers::UnsafeRmwOp::Cas,
                },
            ));
        let hit = self.emit_jmp_rel32_patch();
        for p in decline {
            self.patch_rel32_to_here(p);
        }
        self.slot_mirror = None;
        Some(hit)
    }

    /// JIT round 21 wave 1, lane updaters (UPDATERS21-1,
    /// `CRATONVM_JIT_UNSAFE_REF_VOLATILE_FIELD`, opt-in): the single-pass twin
    /// of `ir_lower::Lowerer::try_emit_ir_unsafe_ref_volatile_prefix` for both
    /// single-pass `Unsafe` prefixes (the thin bind's and the dispatch arm's):
    /// `ops` is `[receiver, base, offset, (value)]`. The pieces of
    /// [`Self::emit_sp_unsafe_ref_cas_core`] in the same order -- null tests,
    /// the not-array kind test, for a put the reference-store gate screen, the
    /// reference cell screen -- then
    /// `direct_helpers::unsafe_ref_volatile_access_bytes`. Returns the hit
    /// edge's `JMP rel32` patch site (a get's reference in RAX), or `None`
    /// (nothing emitted) when this compile has no such arm.
    pub(in crate::x64::op_invoke) fn emit_sp_unsafe_ref_volatile_core(
        &mut self,
        pc: usize,
        access: crate::direct_helpers::UnsafeRefVolatileAccess,
        ops: &[StackSlot],
    ) -> Option<usize> {
        if ops.len() != 1 + access.num_params()
            || self.failed
            || !sp_thin_prefix_enabled()
            || crate::x64::zgc_read_barrier_blocks_inline_fields()
        {
            return None;
        }
        let cells =
            crate::direct_helpers::unsafe_ref_volatile_field_arm_words(&self.direct_helpers, access);
        // j27 wave 6, lane T1DISP20 (DISP20-8, opt-in): a get also serves a
        // reference-array element (`crate::unsafe_ref_array_arm`).
        let array = !access.is_put
            && crate::unsafe_ref_array_arm::unsafe_ref_array_arm_admitted(
                &self.direct_helpers,
                crate::unsafe_ref_array_arm::UNSAFE_REF_ARRAY_GET_GATE_BIT,
            );
        if cells.is_none() && !array {
            return None;
        }
        let gates = if access.is_put {
            Some(crate::ir_lower::vh_ref_store_gates(
                self.ref_store_gates(),
                self.ref_store_post_skip_mask(),
            )?)
        } else {
            None
        };
        let (recv, base, offset) = (ops[0], ops[1], ops[2]);
        let value = if access.is_put { Some(ops[3]) } else { None };
        if crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] unsafe-ref-volatile-inline (sp) {} @pc={pc} put={} xchg={}",
                self.method_label, access.is_put, access.exchange,
            );
        }
        let mut decline: Vec<usize> = Vec::new();
        // R10 = the `Unsafe` receiver; null declines (the call's NPE).
        self.load_slot_to_reg(R10, recv);
        self.emit_test_r64_r64(R10);
        decline.push(self.emit_jcc_rel32_patch(0x84)); // JZ
        // RAX = base; null declines.
        self.load_slot_to_reg(RAX, base);
        self.emit_test_r64_r64(RAX);
        decline.push(self.emit_jcc_rel32_patch(0x84)); // JZ
        // An array base declines (or, for a get with the array arm, takes it);
        // a plain object jumps to the field arm.
        let (bytes, edge) = crate::direct_helpers::unsafe_inline_not_array_bytes();
        let at = self.buf.pos();
        self.buf.emit(&bytes);
        let to_field = at + edge;
        let to_join = if array {
            crate::unsafe_ref_array_arm::note_ref_array_arm("sp get", &self.method_label);
            // R11 = the offset, kept: `[RAX + R11]` is the element slot.
            self.load_slot_to_reg(R11, offset);
            let (bytes, edges) = crate::unsafe_ref_array_arm::unsafe_ref_array_screen_bytes();
            let at = self.buf.pos();
            self.buf.emit(&bytes);
            decline.extend(edges.into_iter().map(|e| at + e));
            Some(self.emit_jmp_rel32_patch())
        } else {
            decline.push(self.emit_jmp_rel32_patch());
            None
        };
        self.patch_rel32_to_here(to_field);
        self.slot_mirror = None;
        if let Some((cells, epoch_addr)) = cells {
            if let (Some(g), Some(value)) = (gates, value) {
                // The barrier gates, before the store (RAX = holder, RDX = value).
                self.load_slot_to_reg(RDX, value);
                let (bytes, edges) = crate::ir_lower::vh_ref_store_gate_screen_bytes(g);
                let at = self.buf.pos();
                self.buf.emit(&bytes);
                decline.extend(edges.into_iter().map(|e| at + e));
            }
            // R11 = the offset, then the published compact displacement.
            self.load_slot_to_reg(R11, offset);
            let (bytes, edges) =
                crate::direct_helpers::unsafe_inline_ref_field_cell_bytes(cells, epoch_addr);
            let at = self.buf.pos();
            self.buf.emit(&bytes);
            decline.extend(edges.into_iter().map(|e| at + e));
        } else {
            decline.push(self.emit_jmp_rel32_patch());
        }
        // The array arm joins here: a jump target, no reload elision across it.
        if let Some(to_join) = to_join {
            self.patch_rel32_to_here(to_join);
            self.slot_mirror = None;
        }
        // The screen wrote RDX: the value again.
        if let Some(value) = value {
            self.load_slot_to_reg(RDX, value);
        }
        self.buf
            .emit(&crate::direct_helpers::unsafe_ref_volatile_access_bytes(access));
        if access.exchange {
            // JIT round 23 wave 1, lane L9: a getAndSet's result is the
            // reference the `XCHG` left in RDX.
            self.buf
                .emit(&crate::direct_helpers::UNSAFE_REF_EXCHANGE_RESULT_BYTES);
        }
        let hit = self.emit_jmp_rel32_patch();
        for p in decline {
            self.patch_rel32_to_here(p);
        }
        self.slot_mirror = None;
        Some(hit)
    }

    /// nio21 wave 11 L5 (W9L2-2, page
    /// `r21-varhandle-constant-handle-field-access-inline-FIXED-20261012.md`): a
    /// JUDGED thin `VarHandle` bind (`get*` / `set*` / `compareAndSet` over an
    /// instance field) answered inline when its handle is a constant
    /// `static final`, as a guarded prefix of the bind's unchanged CALL
    /// ([`Self::sp_thin_helper_prefix_admitted`] for the contract, asked by the
    /// arm with the judged record set aside: the record is the CALL's context
    /// word and nothing else, and a hit makes no CALL). The site's name and
    /// descriptor come from the record's `JitInvokeInfo`; `arg_slots` are the
    /// popped `[handle, receiver, values...]`. See
    /// [`Self::emit_sp_vh_const_prefix`].
    pub(in crate::x64::op_invoke) fn try_emit_sp_vh_const_thin_prefix(
        &mut self,
        pc: usize,
        code: &[u8],
        record: usize,
        ret_type: u8,
        arg_slots: &[StackSlot],
    ) -> Option<usize> {
        if self.failed || !sp_thin_prefix_enabled() {
            return None;
        }
        // SAFETY: `record` is the judged row's entry, decoded by this arm
        // through `varhandle_judged_site_helper`: a `VarHandleJudgedSite` the
        // door owns for the whole compile, like every `JitInvokeInfo` pointer
        // in these tables.
        let info: &crate::JitInvokeInfo =
            unsafe { &(*(record as *const crate::VarHandleJudgedSite)).info };
        self.emit_sp_vh_const_prefix(pc, code, info, ret_type, arg_slots)
    }

    /// nio21 wave 11 L5 (W9L2-2): [`Self::emit_sp_vh_const_prefix`] at the
    /// DISPATCH arm, for the access modes no thin helper binds
    /// (`getAndAdd*`, `getAndSet*`, the weak CAS family, `compareAndExchange*`,
    /// `getAndBitwise*`, and every mode while the binds are off). The shape of
    /// [`Self::try_emit_sp_unsafe_dispatch_prefix`]: emitted before the
    /// dispatch pops anything, the operands read off the virtual stack, every
    /// decline landing on the first byte of the UNCHANGED dispatch, the hit
    /// edge patched at the arm's result push with the answer in RAX (a hit
    /// makes no call, so it owes neither the map nor the exception check).
    pub(in crate::x64::op_invoke) fn try_emit_sp_vh_const_dispatch_prefix(
        &mut self,
        pc: usize,
        code: &[u8],
        info: &crate::JitInvokeInfo,
    ) -> Option<usize> {
        if self.failed
            || !sp_thin_prefix_enabled()
            || info.class_name != "java/lang/invoke/VarHandle"
            || !matches!(info.invoke_kind, 0 | 1)
        {
            return None;
        }
        let depth = info.num_jit_args;
        if depth < 2 || self.stack.len() < depth {
            return None;
        }
        // The core writes RAX, RCX, RDX, R10 and R11: no operand may live in a
        // scratch register (the walk flushed them before this arm, as the
        // `Unsafe` dispatch prefix also asks).
        if self
            .stack
            .iter()
            .any(|s| matches!(s, StackSlot::Scratch(..)))
        {
            return None;
        }
        let ops: Vec<StackSlot> = self.stack[self.stack.len() - depth..].to_vec();
        self.emit_sp_vh_const_prefix(pc, code, info, info.return_type, &ops)
    }

    /// The constant-handle `VarHandle` field access both single-pass arms
    /// emit (nio21 wave 11 L5; the twin of
    /// `ir_lower::Lowerer::try_emit_ir_vh_const_prefix`, the shared section
    /// above `ir_lower::vh_const_core_bytes` says what is served). `ops` is
    /// `[handle, receiver, values...]`.
    ///
    /// Unlike the optimizing tier, this backend has no graph to say which
    /// `getstatic` produced the handle operand, so it picks the nearest
    /// preceding `getstatic` of a field the VM filed a plan for (of the site's
    /// value kind) and PROVES the choice at run time: the operand must equal
    /// that static's current value, read the way the inline `getstatic` reads
    /// it. A `static final` is assigned once, by the `<clinit>` that ran before
    /// the VM filed the plan, so equality means "this is the filed handle", and
    /// a wrong guess merely declines. Then the shared screens and access.
    ///
    /// ```text
    ///   [epoch guard ; JNE call]
    ///   R11 = handle ; RCX = the static's value ; CMP RCX, R11 ; JNE call
    ///   RAX = receiver ; R10 = expected ; RDX = value
    ///   <vh_const_core_bytes: receiver / value screens, gates, the access>
    ///   JMP hit                          ; the result in RAX
    /// ```
    ///
    /// Returns the hit edge's `JMP rel32` patch site; `None` emitted nothing.
    /// Declines at compile time under `CRATONVM_JIT_VH_CONST_INLINE=0` (and
    /// `..._REF_INLINE=0` for a reference field), `CRATONVM_JIT_SP_THIN_PREFIX=0`,
    /// an armed ZGC read barrier, a site shape `vh_const_site_shape` refuses,
    /// no filed plan before the site, no static base for it, no compact layout
    /// for the slot, and an operand in an XMM register.
    pub(in crate::x64::op_invoke) fn emit_sp_vh_const_prefix(
        &mut self,
        pc: usize,
        code: &[u8],
        info: &crate::JitInvokeInfo,
        ret_type: u8,
        ops: &[StackSlot],
    ) -> Option<usize> {
        // j27 wave 2 (lane VHCOMP, `CRATONVM_JIT_VH_CONST_STATIC`): a
        // receiver-less static-field shape (`x64::vh_static_sp`), asked before
        // the ZGC gate: its handle proof can only decline.
        if info.class_name == "java/lang/invoke/VarHandle" {
            if let Some(hit) = self.emit_sp_vh_const_static_prefix(pc, code, info, ret_type, ops) {
                return Some(hit);
            }
        }
        if info.class_name != "java/lang/invoke/VarHandle"
            || !crate::ir_lower::vh_const_inline_enabled()
            || crate::x64::zgc_read_barrier_blocks_inline_fields()
        {
            return None;
        }
        let Some(shape) = crate::ir_lower::vh_const_site_shape(info.method_name, info.descriptor)
        else {
            // JIT r20 wave 18 (lane xVhArr, AT-2): an array-element shape.
            return self.emit_sp_vh_const_array_prefix(pc, code, info, ret_type, ops);
        };
        if ops.len() != 2 + shape.values.len()
            || ret_type != shape.ret
            || ops.iter().any(|s| matches!(s, StackSlot::Xmm(..)))
        {
            return None;
        }
        let (plan, static_class, static_field) =
            self.sp_vh_const_candidate(code, pc, shape.kind)?;
        let ref_facts = if plan.kind == b'L' {
            Some(crate::ir_lower::vh_const_ref_facts(
                &self.direct_helpers,
                static_class,
                static_field,
            )?)
        } else {
            None
        };
        // Everything resolved before the first byte: a refusal after a
        // decline branch would leave it unpatched.
        let cid_off = i32::try_from(self.helpers.class_id_offset_in_obj).ok()?;
        // JIT round 21 wave 1, lane updaters: and a reference get whose site
        // returns a narrower class, with the cast as a result screen
        // (`CRATONVM_JIT_VH_CONST_CAST_RETURN`, opt-in).
        // JIT round 21, lane vhcae (`CRATONVM_JIT_VH_CONST_RET_SAME_LOADER`):
        // a typed reference return is proven in any class of the coordinate
        // class's loader, as in the optimizing tier's prefix.
        let ret_caller = if matches!(shape.ret, b'L' | b'[') {
            crate::ir_lower::vh_const_ret_proof_caller(
                &self.direct_helpers,
                info.declaring_class_id,
                plan.receiver_class_id,
            )
        } else {
            info.declaring_class_id
        };
        let core = crate::ir_lower::vh_const_core_cast_return(
            &self.direct_helpers,
            &shape,
            plan,
            ref_facts.as_ref(),
            info.declaring_class_id,
            ret_caller,
            self.sp_vh_const_store_barriers(),
            cid_off,
        )?;
        let base_cell = self
            .direct_helpers
            .resolve_static_base(static_class, static_field)?;
        let (compare, compare_edges) = sp_vh_const_handle_compare_bytes(base_cell, static_field)?;
        // JIT round 21, lane spg1vh (G1VHSOFT21-1): a G1 core's cross-region
        // post edge (`post`), served below by `emit_sp_vh_g1_post_edge`; every
        // other core has none and keeps `vh_const_core_bytes`' bytes exactly.
        // j27 wave 4 (lane VHCARD, `CRATONVM_JIT_SP_VH_CONST_GEN_CARD_EDGE`):
        // a generational write that may owe a card takes `post` to the card
        // edge (`x64::vh_gen_card_sp`) instead of declining.
        let gen_edge = self.sp_vh_gen_card_edge_admitted(&core, ops);
        let (access, access_edges, post) = if core.g1.is_some() {
            crate::ir_lower::vh_const_core_bytes_post(&core)
        } else if gen_edge {
            crate::ir_lower::vh_const_core_bytes_gen_edge(&core)
        } else {
            let (access, edges) = crate::ir_lower::vh_const_core_bytes(&core);
            (access, edges, None)
        };
        let value_slot = if shape.has_expected() {
            Some(ops[3])
        } else if shape.values.len() == 1 {
            Some(ops[2])
        } else {
            None
        };
        if crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] vh-const-inline (sp) {} @pc={pc} {}{} kind={} class={} slot={} \
                 disp={} static=({static_class},{static_field}) checks={:?}/{:?} gated={}",
                self.method_label,
                info.method_name,
                info.descriptor,
                char::from(core.kind),
                core.class_id,
                plan.slot,
                core.disp,
                core.expected_check,
                core.value_check,
                core.gates.is_some(),
            );
            if let Some(g1) = core.g1 {
                eprintln!(
                    "[cratonvm-jitc] vh-const-inline (sp) {} @pc={pc} g1 plan post_call={}",
                    self.method_label, g1.post_call
                );
            }
        }
        let mut decline: Vec<usize> = Vec::new();
        // First, before any operand load: the guard's fallback form clobbers
        // RCX and R11.
        if jit_sp_field_layout_guard_enabled() {
            decline.extend(self.emit_layout_epoch_guard());
        }
        // The handle proof.
        self.load_slot_to_reg(R11, ops[0]);
        let at = self.buf.pos();
        self.buf.emit(&compare);
        decline.extend(compare_edges.into_iter().map(|e| at + e));
        // The operands, then the shared screens and the access.
        self.load_slot_to_reg(RAX, ops[1]);
        if shape.has_expected() {
            self.load_slot_to_reg(R10, ops[2]);
            self.load_slot_to_reg(RDX, ops[3]);
        } else if shape.values.len() == 1 {
            self.load_slot_to_reg(RDX, ops[2]);
        }
        let at = self.buf.pos();
        self.buf.emit(&access);
        decline.extend(access_edges.into_iter().map(|e| at + e));
        let hit_at = self.buf.pos();
        let hit = self.emit_jmp_rel32_patch();
        // JIT round 21, lane spg1vh (G1VHSOFT21-1): G1's cross-region post
        // edge, entered BEFORE the access with RAX / R10 / RDX still the
        // operands (the screens wrote only RCX and R11): the access, then
        // G1's remembered-set barrier, then the hit edge.
        if let Some(p) = post.map(|e| at + e) {
            match value_slot {
                Some(value) => {
                    self.patch_rel32_to_here(p);
                    let keep = shape.ret != b'V';
                    let edge_declines = if gen_edge {
                        let expected = shape.has_expected().then(|| ops[2]);
                        self.emit_sp_vh_gen_card_edge(&core, keep, ops[1], expected, value)
                    } else {
                        self.emit_sp_vh_g1_post_edge(&core, keep, ops[1], value)
                    };
                    self.emit_jmp_back_to(hit_at);
                    decline.extend(edge_declines);
                }
                // Not reachable (a write mode has a value): the edge then
                // takes the call, before any access.
                None => decline.push(p),
            }
        }
        for p in decline {
            self.patch_rel32_to_here(p);
        }
        self.slot_mirror = None;
        Some(hit)
    }

    /// JIT round 21, lane spg1vh (G1VHSOFT21-1 of
    /// `docs/feature-designs/jit-barrier-lowering.md`): the barrier
    /// plan [`Self::emit_sp_vh_const_prefix`] builds a reference write
    /// against -- the published generational / ZGC gates, or, under G1 (which
    /// publishes none), G1's own plan through the optimizing tier's rule
    /// (`ir_lower::vh_g1_gates`: the same admission, so the two tiers cannot
    /// disagree on which sites are served). Off under
    /// `CRATONVM_JIT_SP_VH_CONST_G1=0` (every G1 reference write keeps the
    /// call, as before). The post edge (`VhG1Gates::post_call`) also needs a
    /// reserved VM-context slot, which its barrier call loads; without one a
    /// cross-region value declines to the call.
    pub(in crate::x64::op_invoke) fn sp_vh_const_store_barriers(&self) -> crate::ir_lower::VhStoreBarriers {
        let gates = self.ref_store_gates();
        let g1 = if sp_vh_const_g1_enabled() {
            crate::ir_lower::vh_g1_gates(
                gates,
                self.helpers.g1_barrier_addr,
                self.helpers.ref_store_pre_gate,
                self.helpers.g1_post_write_barrier,
            )
        } else {
            None
        };
        match g1 {
            Some(mut g1) => {
                g1.post_call &= self.heap_local_offset != 0;
                crate::ir_lower::VhStoreBarriers::G1(g1)
            }
            None => crate::ir_lower::VhStoreBarriers::Gates(gates, self.ref_store_post_skip_mask()),
        }
    }

    /// JIT round 21, lane spg1vh (G1VHSOFT21-1): the single-pass twin of the
    /// optimizing tier's G1 post edge (`Lowerer::try_emit_ir_vh_const_prefix`).
    /// Entered from `vh_g1_store_screen`'s cross-region branch with RAX = the
    /// receiver, R10 = `expected`, RDX = the value (all screened; no mark is
    /// running, or the replaced value is null), it performs the access, then
    /// the single-pass `putfield`'s own G1 post barrier
    /// ([`Self::emit_g1_post_write_barrier_regs`], so the two cannot disagree)
    /// whose F-08 arm carries VHCAE21-1's edge filter
    /// (`CRATONVM_JIT_G1_EDGE_FILTER`), and leaves the access's result in RAX.
    /// Store first, barrier after, no safepoint between: exactly the gated G1
    /// `putfield` (`emit_inline_body_compact_ref_putfield`). As there, the
    /// barrier runs after a CAS whatever its outcome (HotSpot's G1 does too).
    ///
    /// ```text
    ///   <vh_const_access_bytes>              ; RAX = the result
    ///   [SUB RSP, 48 ; MOV [RSP+32], RAX]    ; keep_result: above the callee's home area
    ///   RAX = recv ; RDX = value ; <emit_g1_post_write_barrier_regs>   ; its filters, then the call
    ///   [MOV RAX, [RSP+32] ; ADD RSP, 48]
    /// ```
    ///
    /// The result is kept on the machine stack, not in a frame word: the
    /// single-pass operand cache has no callee-saved register to spare, and a
    /// new frame word would move the spill cursor. 48 bytes keep RSP 16-byte
    /// aligned at the CALL, and the word at RSP+32 lies above the 32-byte home
    /// area a Windows x64 callee may write. A reference result held there
    /// across the barrier is safe for the reason the optimizing tier's frame
    /// copy is: `jit_g1_post_write_barrier` never parks for a collection.
    /// Every register written (RAX, RCX, RDX, R10, R11, the barrier call's
    /// caller-saved set) is one the CALL this prefixes clobbers anyway; R8 /
    /// R9 operands are read before the call writes them. Frame operands are
    /// RBP-relative, so the moved RSP does not disturb their reloads.
    ///
    /// Returns the rel32 decline fields of the access's own cast screens
    /// (`vh_const_access_bytes`), which run before any write and before the
    /// result is kept; the caller patches them to the call.
    pub(in crate::x64::op_invoke) fn emit_sp_vh_g1_post_edge(
        &mut self,
        core: &crate::ir_lower::VhConstCore,
        keep_result: bool,
        recv: StackSlot,
        value: StackSlot,
    ) -> Vec<usize> {
        let (access, access_edges) = crate::ir_lower::vh_const_access_bytes(core);
        let access_at = self.buf.pos();
        self.buf.emit(&access);
        let declines: Vec<usize> = access_edges.into_iter().map(|e| access_at + e).collect();
        if keep_result {
            self.buf.emit(&SP_VH_G1_KEEP_RESULT);
        }
        // vhc21 review: no edge filter of its own here. `emit_g1_post_write_barrier_regs`
        // runs it (null / same region / young holder, then VHCAE21-1's edge
        // filter from the operands' slots, then the call); an explicit copy in
        // front of it re-tested the same facts on every miss.
        self.load_slot_to_reg(RAX, recv);
        self.load_slot_to_reg(RDX, value);
        self.emit_g1_post_write_barrier_regs(RAX, RDX, RCX, recv, value);
        if keep_result {
            self.buf.emit(&SP_VH_G1_RESTORE_RESULT);
        }
        declines
    }

    /// The nearest `getstatic` before `pc` of a field the VM filed a
    /// constant-`VarHandle` plan of `kind` for, as `(plan, class id, static
    /// field index)`. A heuristic only: [`Self::emit_sp_vh_const_prefix`]
    /// proves the choice at run time.
    pub(in crate::x64::op_invoke) fn sp_vh_const_candidate(
        &self,
        code: &[u8],
        pc: usize,
        kind: u8,
    ) -> Option<(crate::ir_lower::VhConstPlan, u32, usize)> {
        let mut best: Option<(usize, crate::ir_lower::VhConstPlan, u32, usize)> = None;
        for &(spc, class_id, field_index, type_tag, _) in &self.static_field_info {
            if spc >= pc
                || type_tag != b'L'
                || code.get(spc) != Some(&0xb2)
                || best.is_some_and(|b| b.0 > spc)
            {
                continue;
            }
            let Some(plan) =
                crate::ir_lower::vh_const_plan(&self.direct_helpers, class_id, field_index)
            else {
                continue;
            };
            if plan.kind == kind {
                best = Some((spc, plan, class_id, field_index));
            }
        }
        best.map(|(_, plan, class_id, field_index)| (plan, class_id, field_index))
    }

    /// JIT r20 wave 18 (lane xVhArr, AT-2 of
    /// `docs/feature-designs/jit-ir-optimizations.md`): the
    /// ARRAY-ELEMENT arm of [`Self::emit_sp_vh_const_prefix`] (the twin of
    /// `ir_lower::Lowerer::try_emit_ir_vh_const_array_prefix`; the section
    /// above `ir_lower::vh_const_array_element_enabled` says what is served).
    /// `ops` is `[handle, array, index, values...]`. The handle proof is the
    /// field arm's (the nearest preceding `getstatic` of a field filed as an
    /// array handle of the site's element kind, compared at run time with the
    /// operand), then the shared array screen and access:
    ///
    /// ```text
    ///   R11 = handle ; RCX = the static's value ; CMP RCX, R11 ; JNE call
    ///   RAX = array ; R11 = index ; R10 = expected ; RDX = value
    ///   <vh_const_array_core_bytes: null / int[] / long[] / index screens, the access>
    ///   JMP hit                          ; the result in RAX
    /// ```
    ///
    /// No layout epoch guard: an array's header and payload offsets are not
    /// part of the compact field layout the guard protects. Declines at
    /// compile time under `CRATONVM_JIT_VH_CONST_ARRAY_ELEMENT=0`, a shape
    /// `vh_const_array_site_shape` refuses, no filed array plan before the
    /// site, no static base for it, and an operand in an XMM register.
    pub(in crate::x64::op_invoke) fn emit_sp_vh_const_array_prefix(
        &mut self,
        pc: usize,
        code: &[u8],
        info: &crate::JitInvokeInfo,
        ret_type: u8,
        ops: &[StackSlot],
    ) -> Option<usize> {
        let shape = crate::ir_lower::vh_const_array_site_shape(info.method_name, info.descriptor)?;
        if ops.len() != 3 + shape.values.len()
            || ret_type != shape.ret
            || ops.iter().any(|s| matches!(s, StackSlot::Xmm(..)))
        {
            return None;
        }
        let (plan_kind, static_class, static_field) =
            self.sp_vh_const_array_candidate(code, pc, shape.kind)?;
        // JIT round 21 wave 1, lane updaters (UPDATERS21-5,
        // `CRATONVM_JIT_VH_CONST_REF_ARRAY_GET`, opt-in): a reference-array GET.
        // JIT round 21 wave 22, lane vhconst (UPDATERS21-5,
        // `CRATONVM_JIT_VH_CONST_REF_ARRAY_STORE`, opt-in): and its stores.
        if shape.kind == b'L' && shape.op != crate::ir_lower::VhConstOp::Get {
            return self.emit_sp_vh_const_ref_array_store_prefix(
                pc,
                info,
                &shape,
                ops,
                plan_kind,
                static_class,
                static_field,
            );
        }
        if shape.kind == b'L' {
            let cid_off = i32::try_from(self.helpers.class_id_offset_in_obj).ok()?;
            let core = crate::ir_lower::vh_const_ref_array_core(
                &self.direct_helpers,
                &shape,
                plan_kind,
                info.declaring_class_id,
                cid_off,
            )?;
            let base_cell = self
                .direct_helpers
                .resolve_static_base(static_class, static_field)?;
            let (compare, compare_edges) =
                sp_vh_const_handle_compare_bytes(base_cell, static_field)?;
            let (access, access_edges) = crate::ir_lower::vh_const_ref_array_get_bytes(&core);
            if crate::x64::dbg_jitc_enabled() {
                eprintln!(
                    "[cratonvm-jitc] vh-const-ref-array-inline (sp) {} @pc={pc} {}{} \
                     static=({static_class},{static_field}) cast={:?}",
                    self.method_label, info.method_name, info.descriptor, core.result_check,
                );
            }
            let mut decline: Vec<usize> = Vec::new();
            self.load_slot_to_reg(R11, ops[0]);
            let at = self.buf.pos();
            self.buf.emit(&compare);
            decline.extend(compare_edges.into_iter().map(|e| at + e));
            self.load_slot_to_reg(RAX, ops[1]);
            self.load_slot_to_reg(R11, ops[2]);
            let at = self.buf.pos();
            self.buf.emit(&access);
            decline.extend(access_edges.into_iter().map(|e| at + e));
            let hit = self.emit_jmp_rel32_patch();
            for p in decline {
                self.patch_rel32_to_here(p);
            }
            self.slot_mirror = None;
            return Some(hit);
        }
        let core = crate::ir_lower::vh_const_array_core(&shape, plan_kind)?;
        // Everything resolved before the first byte: a refusal after a
        // decline branch would leave it unpatched.
        let base_cell = self
            .direct_helpers
            .resolve_static_base(static_class, static_field)?;
        let (compare, compare_edges) = sp_vh_const_handle_compare_bytes(base_cell, static_field)?;
        let (access, access_edges) = crate::ir_lower::vh_const_array_core_bytes(&core);
        if crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] vh-const-array-inline (sp) {} @pc={pc} {}{} kind={} \
                 static=({static_class},{static_field})",
                self.method_label,
                info.method_name,
                info.descriptor,
                char::from(core.kind),
            );
        }
        let mut decline: Vec<usize> = Vec::new();
        // The handle proof.
        self.load_slot_to_reg(R11, ops[0]);
        let at = self.buf.pos();
        self.buf.emit(&compare);
        decline.extend(compare_edges.into_iter().map(|e| at + e));
        // The operands, then the shared screens and the access.
        self.load_slot_to_reg(RAX, ops[1]);
        self.load_slot_to_reg(R11, ops[2]);
        if shape.has_expected() {
            self.load_slot_to_reg(R10, ops[3]);
            self.load_slot_to_reg(RDX, ops[4]);
        } else if shape.values.len() == 1 {
            self.load_slot_to_reg(RDX, ops[3]);
        }
        let at = self.buf.pos();
        self.buf.emit(&access);
        decline.extend(access_edges.into_iter().map(|e| at + e));
        let hit = self.emit_jmp_rel32_patch();
        for p in decline {
            self.patch_rel32_to_here(p);
        }
        self.slot_mirror = None;
        Some(hit)
    }

    /// JIT round 21 wave 22, lane vhconst (UPDATERS21-5,
    /// `CRATONVM_JIT_VH_CONST_REF_ARRAY_STORE`, opt-in): the reference-array
    /// STORE arm of [`Self::emit_sp_vh_const_array_prefix`] (the twin of
    /// `ir_lower::Lowerer::try_emit_ir_vh_const_ref_array_store_prefix`; the
    /// section above `ir_lower::vh_const_cast_cae_enabled` says what is
    /// served). `ops` is `[handle, array, index, (expected,) value]`.
    ///
    /// ```text
    ///   R11 = handle ; RCX = the static's value ; CMP RCX, R11 ; JNE call
    ///   RAX = array ; R10 = expected ; RDX = value
    ///   <vh_const_ref_array_store_screen_bytes: array, gates, store check>
    ///   R11 = index ; R10 = expected ; RDX = value     ; the screens spent them
    ///   <vh_const_ref_array_store_access_bytes: index screen, the store>
    ///   JMP hit                          ; the result in RAX
    /// ```
    #[allow(clippy::too_many_arguments)]
    pub(in crate::x64::op_invoke) fn emit_sp_vh_const_ref_array_store_prefix(
        &mut self,
        pc: usize,
        info: &crate::JitInvokeInfo,
        shape: &crate::ir_lower::VhConstShape,
        ops: &[StackSlot],
        plan_kind: u8,
        static_class: u32,
        static_field: usize,
    ) -> Option<usize> {
        if shape.values.is_empty() || ops.len() != 3 + shape.values.len() {
            return None;
        }
        // Everything resolved before the first byte: a refusal after a
        // decline branch would leave it unpatched.
        let cid_off = i32::try_from(self.helpers.class_id_offset_in_obj).ok()?;
        let core = crate::ir_lower::vh_const_ref_array_store_core(
            &self.direct_helpers,
            shape,
            plan_kind,
            self.ref_store_gates(),
            self.ref_store_post_skip_mask(),
            cid_off,
        )?;
        let base_cell = self
            .direct_helpers
            .resolve_static_base(static_class, static_field)?;
        let (compare, compare_edges) = sp_vh_const_handle_compare_bytes(base_cell, static_field)?;
        let (screen, screen_edges) = crate::ir_lower::vh_const_ref_array_store_screen_bytes(&core);
        let (access, access_edges) = crate::ir_lower::vh_const_ref_array_store_access_bytes(&core);
        if crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] vh-const-ref-array-store-inline (sp) {} @pc={pc} {}{} \
                 static=({static_class},{static_field}) display={}",
                self.method_label,
                info.method_name,
                info.descriptor,
                core.display_cell.is_some(),
            );
        }
        let (expected, value) = if shape.has_expected() {
            (Some(ops[3]), ops[4])
        } else {
            (None, ops[3])
        };
        let mut decline: Vec<usize> = Vec::new();
        // The handle proof.
        self.load_slot_to_reg(R11, ops[0]);
        let at = self.buf.pos();
        self.buf.emit(&compare);
        decline.extend(compare_edges.into_iter().map(|e| at + e));
        // The first half: the array, the gates, the store check.
        self.load_slot_to_reg(RAX, ops[1]);
        if let Some(expected) = expected {
            self.load_slot_to_reg(R10, expected);
        }
        self.load_slot_to_reg(RDX, value);
        let at = self.buf.pos();
        self.buf.emit(&screen);
        decline.extend(screen_edges.into_iter().map(|e| at + e));
        // The second half: its operands again, then the index and the store.
        // r21 review #10 review point: reload registers must not touch RAX.
        debug_assert_ne!(R11, RAX);
        debug_assert_ne!(R10, RAX);
        debug_assert_ne!(RDX, RAX);
        self.load_slot_to_reg(R11, ops[2]);
        if let Some(expected) = expected {
            self.load_slot_to_reg(R10, expected);
        }
        self.load_slot_to_reg(RDX, value);
        let at = self.buf.pos();
        self.buf.emit(&access);
        decline.extend(access_edges.into_iter().map(|e| at + e));
        let hit = self.emit_jmp_rel32_patch();
        for p in decline {
            self.patch_rel32_to_here(p);
        }
        self.slot_mirror = None;
        Some(hit)
    }

    /// JIT r20 wave 18 (lane xVhArr): the nearest `getstatic` before `pc` of a
    /// field the VM filed an array-element plan of element `kind` for, as
    /// `(kind, class id, static field index)`. A heuristic only, as
    /// [`Self::sp_vh_const_candidate`]: the arm proves it at run time.
    pub(in crate::x64::op_invoke) fn sp_vh_const_array_candidate(
        &self,
        code: &[u8],
        pc: usize,
        kind: u8,
    ) -> Option<(u8, u32, usize)> {
        let mut best: Option<(usize, u32, usize)> = None;
        for &(spc, class_id, field_index, type_tag, _) in &self.static_field_info {
            if spc >= pc
                || type_tag != b'L'
                || code.get(spc) != Some(&0xb2)
                || best.is_some_and(|b| b.0 > spc)
            {
                continue;
            }
            if crate::ir_lower::vh_const_array_plan(&self.direct_helpers, class_id, field_index)
                == Some(kind)
            {
                best = Some((spc, class_id, field_index));
            }
        }
        best.map(|(_, class_id, field_index)| (kind, class_id, field_index))
    }

    /// The identity hash in a SINGLE-PASS body (nio21 wave 1, lane L5;
    /// proposal W5-L2-1, page
    /// `r20-identity-hash-is-a-native-call-in-compiled-code-FIXED-20261012.md`): the
    /// twin of `ir_lower::Lowerer::try_emit_ir_identity_hash_prefix`, at the
    /// dispatch arm (no thin helper serves these sites, so the MIC route /
    /// `jit_invoke_dispatch` is what they reach). Same recogniser
    /// (`ir_lower::ir_identity_hash_site`), same `Class` id
    /// (`ir_lower::ir_class_class_id`), same header decode
    /// (`ir_lower::identity_hash_read_bytes`), so the two tiers answer the same
    /// set: `System.identityHashCode(o)` for any object whose header already
    /// holds a hash, and `Class.hashCode()` for an exact `java/lang/Class`
    /// receiver. Everything else -- null, an unhashed object, a thin-locked or
    /// inflated compact instance, a forwarded header, any other receiver --
    /// declines to the first byte of the UNCHANGED dispatch, which runs the
    /// registered native exactly as before.
    ///
    /// The shape of [`Self::try_emit_sp_unsafe_dispatch_prefix`]: emitted
    /// before the dispatch pops anything, the operand read off the virtual
    /// stack; returns the hit edge's `JMP rel32` patch site, the hash
    /// sign-extended in RAX (the int convention the dispatch's result push
    /// expects). A hit makes no call, so it owes no map and no exception
    /// check. Writes RAX, RCX and RDX only.
    ///
    /// Declines at compile time under `CRATONVM_JIT_SP_IDENTITY_HASH_INLINE=0`,
    /// `CRATONVM_JIT_IR_IDENTITY_HASH_INLINE=0` (both tiers off together),
    /// `CRATONVM_JIT_SP_THIN_PREFIX=0`, an armed ZGC read barrier, no `Class`
    /// id for a `Class.hashCode()` site, and an operand in an XMM or scratch
    /// register.
    pub(in crate::x64::op_invoke) fn try_emit_sp_identity_hash_dispatch_prefix(
        &mut self,
        pc: usize,
        info: &crate::JitInvokeInfo,
    ) -> Option<usize> {
        if self.failed
            || !sp_thin_prefix_enabled()
            || !sp_identity_hash_inline_enabled()
            || !crate::ir_lower::ir_identity_hash_inline_enabled()
            || crate::x64::zgc_read_barrier_blocks_inline_fields()
        {
            return None;
        }
        let any_object = crate::ir_lower::ir_identity_hash_site(
            info.class_name,
            info.method_name,
            info.descriptor,
            info.invoke_kind,
        )?;
        let class_guard = if any_object {
            None
        } else {
            Some(crate::ir_lower::ir_class_class_id(&self.direct_helpers)?)
        };
        if info.num_jit_args != 1 || self.stack.is_empty() {
            return None;
        }
        let obj = *self.stack.last()?;
        // RAX, RCX, RDX are written: no operand may live in a scratch
        // register (the walk flushed them before this arm) or an XMM one.
        if matches!(obj, StackSlot::Xmm(..))
            || self
                .stack
                .iter()
                .any(|s| matches!(s, StackSlot::Scratch(..)))
        {
            return None;
        }
        let cid_off = i32::try_from(self.helpers.class_id_offset_in_obj).ok()?;
        if crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] identity-hash-inline (sp-dispatch) {} @pc={pc} {}.{}{} class_guard={class_guard:?}",
                self.method_label, info.class_name, info.method_name, info.descriptor,
            );
        }
        let mut decline: Vec<usize> = Vec::new();
        // RAX = the object; null for System.identityHashCode answers 0 directly.
        self.load_slot_to_reg(RAX, obj);
        self.emit_test_r64_r64(RAX);
        let null_hit = if any_object {
            Some(self.emit_jcc_rel32_patch(0x84))
        } else {
            decline.push(self.emit_jcc_rel32_patch(0x84)); // JZ
            None
        };
        if let Some(cid) = class_guard {
            // A plain object of exactly `java/lang/Class`.
            self.emit_test_mem8_imm8(
                RAX,
                cratonvm_types::KIND_TAGS_BYTE_OFFSET as i32, // Cast: small header constant
                cratonvm_types::KIND_TAG_BYTE_MASK,
            );
            decline.push(self.emit_jcc_rel32_patch(0x85)); // JNE
                                                           // CMP DWORD [RAX + cid_off], Class -- `81 /7 id`, mod=10 rm=RAX.
            self.buf.emit(&[0x81, 0xB8]);
            self.buf.emit(&cid_off.to_le_bytes());
            self.buf.emit(&cid.to_le_bytes());
            decline.push(self.emit_jcc_rel32_patch(0x85)); // JNE
        }
        let (read, edges) = crate::ir_lower::identity_hash_read_bytes();
        let at = self.buf.pos();
        self.buf.emit(&read);
        decline.extend(edges.into_iter().map(|e| at + e));
        if let Some(jz) = null_hit {
            self.patch_rel32_to_here(jz);
        }
        let hit = self.emit_jmp_rel32_patch();
        for p in decline {
            self.patch_rel32_to_here(p);
        }
        self.slot_mirror = None;
        Some(hit)
    }

    /// The bytes both single-pass `Unsafe` prefixes emit (the thin-bound arm's
    /// [`Self::try_emit_sp_unsafe_access_prefix`] and the dispatch arm's
    /// [`Self::try_emit_sp_unsafe_dispatch_prefix`]): the tag / range test and
    /// the one load or store, declining to whatever code follows. Returns the
    /// hit edge's `JMP rel32` patch site; a get's value is in RAX.
    ///
    /// `order` (JIT round 20, lane memory): the `bigEndian` operand of a
    /// four-operand `*Unaligned` form; the access then goes through
    /// [`Self::emit_sp_raw_access_at_rdx`] in both arms, which swaps the
    /// bytes when it is non-zero (a get before its widening, a put before its
    /// store), exactly as the `ScopedMemoryAccess` prefix does. `None` emits
    /// the bytes this function always emitted.
    pub(in crate::x64::op_invoke) fn emit_sp_unsafe_access_core(
        &mut self,
        access: crate::direct_helpers::UnsafeInlineAccess,
        recv: StackSlot,
        base: StackSlot,
        offset: StackSlot,
        value: Option<StackSlot>,
        order: Option<StackSlot>,
    ) -> usize {
        let w = access.width;

        let mut decline: Vec<usize> = Vec::new();
        // R10 = the `Unsafe` receiver; null declines (the call's NPE). The
        // operands live in the frame, a callee-saved register or R8/R9, none
        // of which this sequence writes, so the loads may come in any order.
        self.load_slot_to_reg(R10, recv);
        self.emit_test_r64_r64(R10);
        decline.push(self.emit_jcc_rel32_patch(0x84)); // JZ
                                                       // RAX = base; null (an off-heap address or arena handle) declines --
                                                       // or, since nio21 wave 3 (`sp_unsafe_offheap_prefix_enabled`), takes
                                                       // the off-heap arm emitted after this one.
        self.load_slot_to_reg(RAX, base);
        self.emit_test_r64_r64(RAX);
        let to_offheap = self.emit_jcc_rel32_patch(0x84); // JZ
                                                          // nio21 wave 4 L4: a volatile access never takes the off-heap arm (its
                                                          // raw store has no StoreLoad, and a null-base volatile is the arena
                                                          // natives' business); it declines to the call.
                                                          // JIT round 20 wave 12, lane qUnsafe (L2-2): nor a boolean / float / double
                                                          // row (`UnsafeInlineAccess::heap_only`).
        let offheap = sp_unsafe_offheap_prefix_enabled() && !access.volatile && !access.heap_only();
        if !offheap {
            decline.push(to_offheap);
        }
        // JIT round 20 wave 11, lane pAtom (W7-L3-1 / AT-3): a volatile
        // int/long access to a non-array base takes the instance-field arm
        // (emitted after the hit path), the optimizing tier's
        // (`direct_helpers::unsafe_volatile_field_arm_words`). Never with
        // `order` (a volatile row has no `bigEndian` operand).
        let field_arm = if order.is_none() {
            crate::direct_helpers::unsafe_volatile_field_arm_words(&self.direct_helpers, access)
        } else {
            None
        };
        // t1 wave 9, lane ATOM2 (ATOMICS21-1, opt-in): the static arm, the
        // optimizing tier's (`ir_lower::unsafe_volatile_static_arm_table`).
        let static_arm = if order.is_none() {
            crate::ir_lower::unsafe_volatile_static_arm_table(&self.direct_helpers, access)
        } else {
            None
        };
        let to_field = (field_arm.is_some() || static_arm.is_some()).then(|| {
            let (bytes, edge) = crate::direct_helpers::unsafe_inline_not_array_bytes();
            let at = self.buf.pos();
            self.buf.emit(&bytes);
            at + edge
        });
        self.emit_sp_prim_array_screen(w, offset, &mut decline);
        if access.volatile {
            // R11 = offset; a misaligned one declines (the natives' raw,
            // non-atomic arm). The payload starts 8-aligned at +16, so an
            // aligned offset is an aligned address. TEST R11B, w-1 ; JNZ.
            self.buf.emit(&[0x41, 0xF6, 0xC3, w - 1]);
            decline.push(self.emit_jcc_rel32_patch(0x85));
        }
        // RAX = the base, R11 = the array offset or (from the field arm) the
        // published compact displacement. A jump target when the field arm
        // is emitted: no reload elision across it.
        let field_join = self.buf.pos();
        if field_arm.is_some() || static_arm.is_some() {
            self.slot_mirror = None;
        }

        if let Some(order) = order {
            // JIT round 20 (lane memory): a big-endian `*Unaligned` form. The
            // screen left RAX = the base and R11 = the offset; the element
            // address goes to RDX and the shared raw access swaps (RAX, RCX).
            self.buf.emit(&[0x4A, 0x8D, 0x14, 0x18]); // LEA RDX, [RAX + R11]
            self.emit_sp_raw_access_at_rdx(w, access.signed, value, Some(order));
        } else if let Some(value) = value {
            // RDX = the value; its low `w` bytes are stored.
            self.load_slot_to_reg(RDX, value);
            if access.value_kind == b'Z' {
                // JIT r20 lane rev12: `putBoolean` stores `x & 1`, as HotSpot
                // (and `native_unsafe_put_boolean_mb`) do. AND EDX, 1.
                self.buf
                    .emit(&crate::direct_helpers::UNSAFE_BOOLEAN_NORMALIZE_EDX);
            }
            // MOV [RAX + R11], DL / DX / EDX / RDX (SIB: base RAX, index R11);
            // a volatile put is XCHG [RAX + R11], EDX / RDX instead (an
            // implicitly locked store, the StoreLoad a volatile store owes).
            // JIT round 20 wave 12, lane qUnsafe (L2-2): a release / opaque
            // row (`ordered_store`) is the plain `MOV`.
            match (w, access.volatile && !access.ordered_store) {
                (4, true) => self.buf.emit(&[0x42, 0x87, 0x14, 0x18]),
                (8, true) => self.buf.emit(&[0x4A, 0x87, 0x14, 0x18]),
                (1, _) => self.buf.emit(&[0x42, 0x88, 0x14, 0x18]),
                (2, _) => self.buf.emit(&[0x66, 0x42, 0x89, 0x14, 0x18]),
                (4, _) => self.buf.emit(&[0x42, 0x89, 0x14, 0x18]),
                _ => self.buf.emit(&[0x4A, 0x89, 0x14, 0x18]),
            }
        } else {
            // The raw bytes into RAX (SIB: base RAX, index R11), widened the
            // way the helper's `i64::from(..)` widens: `byte`/`short`/`int`
            // sign-extend, `char` zero-extends.
            match w {
                1 => self.buf.emit(&[0x4A, 0x0F, 0xBE, 0x04, 0x18]), // MOVSX RAX, BYTE
                2 if access.signed => self.buf.emit(&[0x4A, 0x0F, 0xBF, 0x04, 0x18]), // MOVSX RAX, WORD
                2 => self.buf.emit(&[0x42, 0x0F, 0xB7, 0x04, 0x18]), // MOVZX EAX, WORD
                // JIT round 20 wave 12, lane qUnsafe (L2-2): a `float`'s bits
                // zero-extended, as a call's `F` return leaves them.
                4 if access.is_fp() => self.buf.emit(&[0x42, 0x8B, 0x04, 0x18]), // MOV EAX, DWORD
                4 => self.buf.emit(&[0x4A, 0x63, 0x04, 0x18]), // MOVSXD RAX, DWORD
                _ => self.buf.emit(&[0x4A, 0x8B, 0x04, 0x18]), // MOV RAX, QWORD
            }
            // L2-2: `getBoolean` answers `0`/`1` (`native_unsafe_get_boolean_mb`).
            if access.value_kind == b'Z' {
                self.buf
                    .emit(&crate::direct_helpers::UNSAFE_BOOLEAN_NORMALIZE_RAX);
            }
        }
        let hit = self.emit_jmp_rel32_patch();
        if let Some(to_field) = to_field {
            self.patch_rel32_to_here(to_field);
            self.slot_mirror = None;
            self.load_slot_to_reg(R11, offset);
            // t1 wave 9, lane ATOM2 (ATOMICS21-1): the optimizing tier's
            // static arm, same bytes in the same order: a static-band offset
            // on its class's mirror leaves RAX = the statics block and R11 =
            // the payload displacement and joins the MOV / XCHG; any other
            // offset falls through to the field screen (or declines).
            if let Some(table) = static_arm {
                let (bytes, edges, not_static) =
                    crate::direct_helpers::unsafe_inline_static_cell_bytes(table, w);
                let at = self.buf.pos();
                self.buf.emit(&bytes);
                decline.extend(edges.into_iter().map(|e| at + e));
                self.emit_jmp_back_to(field_join);
                if field_arm.is_some() {
                    self.patch_rel32_to_here(at + not_static);
                } else {
                    decline.push(at + not_static);
                }
                if crate::x64::dbg_jitc_enabled() {
                    eprintln!(
                        "[cratonvm-jitc] unsafe-inline (sp) {} volatile static arm width={} put={}",
                        self.method_label, w, access.is_put,
                    );
                }
            }
            if let Some((cells, epoch_addr)) = field_arm {
                // W7-L3-1: R11 = the offset, then the cell screen leaves R11 =
                // the compact displacement the VM's hardware field atomic
                // published for this `(class id, slot, width)`, or declines.
                // RAX (the base) is untouched by the screen.
                let (bytes, edges) =
                    crate::direct_helpers::unsafe_inline_field_cell_bytes(cells, epoch_addr, w);
                let at = self.buf.pos();
                self.buf.emit(&bytes);
                decline.extend(edges.into_iter().map(|e| at + e));
                self.emit_jmp_back_to(field_join);
                if crate::x64::dbg_jitc_enabled() {
                    eprintln!(
                        "[cratonvm-jitc] unsafe-inline (sp) {} volatile field arm width={} put={}",
                        self.method_label, w, access.is_put,
                    );
                }
            }
        }
        if offheap {
            // nio21 wave 3 (lane L3, W2L3-3): a null base. The helper's
            // `raw_accessor_{read,write}_bytes` with `obj == 0`: a positive
            // untagged address is a real pointer; a tagged one is an arena
            // handle, served inline only when it resolves to a live slotted
            // block that holds the whole access. Everything else declines.
            self.patch_rel32_to_here(to_offheap);
            self.load_slot_to_reg(R11, offset);
            self.emit_sp_offheap_address(w, &mut decline);
            self.emit_sp_raw_access_at_rdx(w, access.signed, value, order);
            // Join the heap arm's hit `JMP` (one hit edge for the caller).
            self.emit_jmp_back_to(hit - 1);
        }
        for p in decline {
            self.patch_rel32_to_here(p);
        }
        self.slot_mirror = None;
        hit
    }

    /// The heap-array screen of the single-pass `Unsafe` prefix (nio20 wave 2,
    /// lane L2), moved out of [`Self::emit_sp_unsafe_access_core`] unchanged by
    /// nio21 wave 3 L3 so the `ScopedMemoryAccess` prefix shares it byte for
    /// byte: RAX = a non-null base. Loads R11 = `offset` (a `long`) and
    /// leaves RAX = the base, R11 = the offset, or jumps to a `decline` edge
    /// unless the base is a one-dimensional PRIMITIVE array whose payload
    /// holds `[offset - 16, offset - 16 + w)`. Writes RCX, RDX, R10, R11.
    pub(in crate::x64::op_invoke) fn emit_sp_prim_array_screen(&mut self, w: u8, offset: StackSlot, decline: &mut Vec<usize>) {
        const _: () = assert!(
            cratonvm_types::ARRAY_DATA_OFFSET == 16,
            "the Unsafe inline prefix assumes Unsafe.arrayBaseOffset (16) == ARRAY_DATA_OFFSET"
        );
        const KIND_TAGS: u8 = cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8; // Cast: x86-64 disp8
        const LEN: u8 =
            crate::x64::disp::disp8_const(cratonvm_types::ARRAY_LENGTH_OFFSET as i64) as u8;
        const ARRAY_KIND: u8 = cratonvm_types::ObjectKind::Array as u8;
        // The primitive element tags are 4 (`Boolean`) ..= 11 (`Long`): the
        // whole tag byte of a primitive array is 0x11 ..= 0x2D, and
        // `element_type & 3` is log2 of its element size (the optimizing
        // tier's prefix pins the same constants).
        const PRIM_LO: u8 = ARRAY_KIND | ((cratonvm_types::ArrayElementType::Boolean as u8) << 2);
        const PRIM_HI: u8 = ARRAY_KIND | ((cratonvm_types::ArrayElementType::Long as u8) << 2);
        const _: () = assert!(PRIM_LO == 0x11 && PRIM_HI == 0x2D);
        const _: () = assert!(cratonvm_types::KIND_TAGS_BYTE_OFFSET <= 127);
        // ECX = the kind/element tag byte; EDX = its kind bits.
        self.buf.emit(&[0x0F, 0xB6, 0x48, KIND_TAGS]); // MOVZX ECX, BYTE [RAX+KIND_TAGS]
        self.buf.emit(&[0x89, 0xCA]); // MOV EDX, ECX
        self.buf
            .emit(&[0x83, 0xE2, cratonvm_types::KIND_TAG_BYTE_MASK]); // AND EDX, kind mask
        self.buf.emit(&[0x83, 0xFA, ARRAY_KIND]); // CMP EDX, Array
        decline.push(self.emit_jcc_rel32_patch(0x85)); // JNE: not an array
        self.buf.emit(&[0x83, 0xF9, PRIM_LO]); // CMP ECX, PRIM_LO
        decline.push(self.emit_jcc_rel32_patch(0x82)); // JB: reference array
        self.buf.emit(&[0x83, 0xF9, PRIM_HI]); // CMP ECX, PRIM_HI
        decline.push(self.emit_jcc_rel32_patch(0x87)); // JA: no such element type
        self.buf.emit(&[0xC1, 0xE9, 0x02]); // SHR ECX, 2
        self.buf.emit(&[0x83, 0xE1, 0x03]); // AND ECX, 3 (log2 element size)
                                            // RDX = payload bytes = length << log2 (a u32 shifted by <= 3: no wrap).
        self.buf.emit(&[0x8B, 0x50, LEN]); // MOV EDX, [RAX+LEN] (zero-extends)
        self.buf.emit(&[0x48, 0xD3, 0xE2]); // SHL RDX, CL
                                            // R11 = offset (a `long`).
        self.load_slot_to_reg(R11, offset);
        // CMP R11, ARRAY_DATA_OFFSET ; JL decline (signed: below the payload).
        self.buf
            .emit(&[0x49, 0x83, 0xFB, cratonvm_types::ARRAY_DATA_OFFSET as u8]); // Cast: 16
        decline.push(self.emit_jcc_rel32_patch(0x8C));
        // R10 = offset - ARRAY_DATA_OFFSET + width, the access's END in the
        // payload: positive here, and no wrap (offset < 2^63).
        let end_disp = (i32::from(w) - cratonvm_types::ARRAY_DATA_OFFSET as i32) as i8; // Cast: -15..=-8
        self.buf.emit(&[0x4D, 0x8D, 0x53, end_disp as u8]); // LEA R10, [R11 + disp8]
        self.buf.emit(&[0x49, 0x39, 0xD2]); // CMP R10, RDX
        decline.push(self.emit_jcc_rel32_patch(0x87)); // JA (unsigned): past the payload
    }

    /// `JMP rel32` to the already-emitted instruction at `target` (a
    /// backward jump; nio21 wave 3 L3, the off-heap arms' join).
    pub(in crate::x64::op_invoke) fn emit_jmp_back_to(&mut self, target: usize) {
        self.buf.emit_byte(0xE9);
        // Cast: x86-64 rel32 displacement inside one compile's buffer.
        let rel = target as i64 - (self.buf.pos() as i64 + 4);
        match i32::try_from(rel) {
            Ok(rel) => self.buf.emit(&rel.to_le_bytes()),
            Err(_) => {
                self.buf.emit(&[0, 0, 0, 0]);
                self.buf.mark_overflowed();
            }
        }
        self.slot_mirror = None;
    }

    /// One forward `Jcc` of the `Arrays.equals` intrinsic (ARRAYS_OPS) to its
    /// true / false label: `cc8` is the short-form opcode (`0x74` JE/JZ,
    /// `0x75` JNE). `rel32 == false` emits the `rel8` form the arm always
    /// emitted and records its displacement byte in `list8`; `rel32 == true`
    /// (JIT r22 wave 1, lane intrinsics, `CRATONVM_JIT_ARRAYS_EQUALS_SSE2`) the
    /// near form (`0F 80+cc`), recorded in `list32` -- the SSE2 compare loop
    /// puts the labels past `rel8` reach.
    pub(in crate::x64::op_invoke) fn emit_arrays_equals_branch(
        &mut self,
        cc8: u8,
        rel32: bool,
        list8: &mut Vec<usize>,
        list32: &mut Vec<usize>,
    ) {
        if rel32 {
            list32.push(self.emit_jcc_rel32_patch(cc8 + 0x10));
        } else {
            self.buf.emit_byte(cc8);
            list8.push(self.buf.pos());
            self.buf.emit_byte(0x00);
        }
    }

    /// The 16-byte SSE2 stage of the single-pass `String.indexOf(int)` arm
    /// (JIT round 22 wave 2, lane intrinsics, INTRINSICS21-2,
    /// `CRATONVM_JIT_SP_STRING_INDEXOF_CHAR`). In: R8 = the `value` array, R10D
    /// = the coder, R9D = the needle (`0..=0xFFFF`), R11D = the length in chars,
    /// EDX = 0. While 16 payload bytes remain, one `MOVDQU` + `PCMPEQB` (LATIN1)
    /// / `PCMPEQW` (UTF16) + `PMOVMSKB` against the needle broadcast in XMM1; a
    /// hit leaves EAX = the char index and takes one of the returned forward
    /// jumps (the caller binds them where its own `found` answer joins).
    /// Otherwise the stage falls through with EDX = the first unscanned char.
    /// A LATIN1 receiver with a needle above 0xFF skips the stage (no byte can
    /// equal it; the scalar loop answers -1). Writes RAX, XMM0, XMM1 (free at
    /// an invoke).
    pub(in crate::x64::op_invoke) fn emit_sp_index_of_char_sse_stage(&mut self) -> Vec<usize> {
        let disp = HEADER_SIZE as u8; // Truncation: usize -> u8 (small fixed header size)
        let mut hits: Vec<usize> = Vec::new();
        let mut skip: Vec<usize> = Vec::new();
        self.buf.emit(&[0x45, 0x85, 0xD2]); // TEST R10D, R10D
        let utf16 = self.emit_jcc_rel32_patch(0x85); // JNZ
        // ---- LATIN1 ----
        self.buf.emit(&[0x41, 0x81, 0xF9, 0xFF, 0x00, 0x00, 0x00]); // CMP R9D, 0xFF
        skip.push(self.emit_jcc_rel32_patch(0x87)); // JA: cannot be in a LATIN1 payload
        self.buf.emit(&[0x66, 0x41, 0x0F, 0x6E, 0xC9]); // MOVD XMM1, R9D
        self.buf.emit(&[0x66, 0x0F, 0x60, 0xC9]); // PUNPCKLBW XMM1, XMM1
        self.buf.emit(&[0x66, 0x0F, 0x61, 0xC9]); // PUNPCKLWD XMM1, XMM1
        self.buf.emit(&[0x66, 0x0F, 0x70, 0xC9, 0x00]); // PSHUFD XMM1, XMM1, 0
        let byte_loop = self.buf.pos();
        self.buf.emit(&[0x44, 0x89, 0xD8]); // MOV EAX, R11D
        self.buf.emit(&[0x29, 0xD0]); // SUB EAX, EDX
        self.buf.emit(&[0x83, 0xF8, 0x10]); // CMP EAX, 16
        skip.push(self.emit_jcc_rel32_patch(0x82)); // JB: the scalar tail
        self.buf.emit(&[0xF3, 0x41, 0x0F, 0x6F, 0x44, 0x10, disp]); // MOVDQU XMM0, [R8+RDX+data]
        self.buf.emit(&[0x66, 0x0F, 0x74, 0xC1]); // PCMPEQB XMM0, XMM1
        self.buf.emit(&[0x66, 0x0F, 0xD7, 0xC0]); // PMOVMSKB EAX, XMM0
        self.buf.emit(&[0x85, 0xC0]); // TEST EAX, EAX
        let byte_hit = self.emit_jcc_rel32_patch(0x85); // JNZ
        self.buf.emit(&[0x83, 0xC2, 0x10]); // ADD EDX, 16
        self.emit_jmp_back_to(byte_loop);
        self.patch_rel32_to_here(byte_hit);
        self.buf.emit(&[0x0F, 0xBC, 0xC0]); // BSF EAX, EAX (the first matching byte)
        self.buf.emit(&[0x01, 0xD0]); // ADD EAX, EDX
        hits.push(self.emit_jmp_rel32_patch());
        // ---- UTF16 ----
        self.patch_rel32_to_here(utf16);
        self.buf.emit(&[0x66, 0x41, 0x0F, 0x6E, 0xC9]); // MOVD XMM1, R9D
        self.buf.emit(&[0x66, 0x0F, 0x61, 0xC9]); // PUNPCKLWD XMM1, XMM1
        self.buf.emit(&[0x66, 0x0F, 0x70, 0xC9, 0x00]); // PSHUFD XMM1, XMM1, 0
        let word_loop = self.buf.pos();
        self.buf.emit(&[0x44, 0x89, 0xD8]); // MOV EAX, R11D
        self.buf.emit(&[0x29, 0xD0]); // SUB EAX, EDX
        self.buf.emit(&[0x83, 0xF8, 0x08]); // CMP EAX, 8 (chars)
        skip.push(self.emit_jcc_rel32_patch(0x82)); // JB: the scalar tail
        self.buf.emit(&[0xF3, 0x41, 0x0F, 0x6F, 0x44, 0x50, disp]); // MOVDQU XMM0, [R8+RDX*2+data]
        self.buf.emit(&[0x66, 0x0F, 0x75, 0xC1]); // PCMPEQW XMM0, XMM1
        self.buf.emit(&[0x66, 0x0F, 0xD7, 0xC0]); // PMOVMSKB EAX, XMM0
        self.buf.emit(&[0x85, 0xC0]); // TEST EAX, EAX
        let word_hit = self.emit_jcc_rel32_patch(0x85); // JNZ
        self.buf.emit(&[0x83, 0xC2, 0x08]); // ADD EDX, 8
        self.emit_jmp_back_to(word_loop);
        self.patch_rel32_to_here(word_hit);
        self.buf.emit(&[0x0F, 0xBC, 0xC0]); // BSF EAX, EAX (the first byte of the first match)
        self.buf.emit(&[0xD1, 0xE8]); // SHR EAX, 1 (byte offset -> char)
        self.buf.emit(&[0x01, 0xD0]); // ADD EAX, EDX
        hits.push(self.emit_jmp_rel32_patch());
        for p in skip {
            self.patch_rel32_to_here(p);
        }
        hits
    }

    /// `Math.round(double)J` (`dbl`) / `Math.round(float)I` in the single-pass
    /// backend (JIT round 22 wave 2, lane intrinsics, INTRINSICS22-4,
    /// `CRATONVM_JIT_SP_MATH_ROUND`): the optimizing tier's
    /// `ir_lower::Lowerer::emit_math_round_scalar` byte for byte, with this
    /// backend's own `emit_fp_to_int_nan_fixup` (the same bytes as the IR
    /// tier's `emit_fp_to_int_fixup`). In: XMM0 = the argument, RAX (EAX) =
    /// its raw bits. Out: RAX = the result (the `int` sign-extended). Writes
    /// RCX, RDX and, on the fallback, XMM1 -- all free at an invoke
    /// (`flush_scratch_registers`).
    pub(in crate::x64::op_invoke) fn emit_sp_math_round(&mut self, dbl: bool) {
        if dbl {
            self.buf.emit(&[0x48, 0x89, 0xC1]); // MOV RCX, RAX
            self.buf.emit(&[0x48, 0xC1, 0xE9, 0x34]); // SHR RCX, 52
            self.buf.emit(&[0x81, 0xE1, 0xFF, 0x07, 0x00, 0x00]); // AND ECX, 0x7FF
            self.buf.emit(&[0xBA]); // MOV EDX, imm32
            self.buf.emit(&1074u32.to_le_bytes());
        } else {
            self.buf.emit(&[0x89, 0xC1]); // MOV ECX, EAX
            self.buf.emit(&[0xC1, 0xE9, 0x17]); // SHR ECX, 23
            self.buf.emit(&[0x81, 0xE1, 0xFF, 0x00, 0x00, 0x00]); // AND ECX, 0xFF
            self.buf.emit(&[0xBA]); // MOV EDX, imm32
            self.buf.emit(&149u32.to_le_bytes());
        }
        self.buf.emit(&[0x29, 0xCA]); // SUB EDX, ECX (EDX = shift)
        self.buf.emit(&[0x83, 0xFA, if dbl { 0x3F } else { 0x1F }]); // CMP EDX, 63 / 31
        self.buf.emit_byte(0x77); // JA .slow (unsigned: a negative shift too)
        let ja_slow = self.buf.pos();
        self.buf.emit_byte(0x00);
        if dbl {
            self.buf.emit(&[0x48, 0x89, 0xC1]); // MOV RCX, RAX
            self.buf.emit(&[0x48, 0xC1, 0xE1, 0x0C]); // SHL RCX, 12
            self.buf.emit(&[0x48, 0xC1, 0xE9, 0x0C]); // SHR RCX, 12
            self.buf.emit(&[0x48, 0x0F, 0xBA, 0xE9, 0x34]); // BTS RCX, 52
            self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
            self.buf.emit(&[0x79, 0x03]); // JNS +3
            self.buf.emit(&[0x48, 0xF7, 0xD9]); // NEG RCX
            self.buf.emit(&[0x48, 0x89, 0xC8]); // MOV RAX, RCX
            self.buf.emit(&[0x89, 0xD1]); // MOV ECX, EDX
            self.buf.emit(&[0x48, 0xD3, 0xF8]); // SAR RAX, CL
            self.buf.emit(&[0x48, 0x83, 0xC0, 0x01]); // ADD RAX, 1
            self.buf.emit(&[0x48, 0xD1, 0xF8]); // SAR RAX, 1
        } else {
            self.buf.emit(&[0x89, 0xC1]); // MOV ECX, EAX
            self.buf.emit(&[0x81, 0xE1, 0xFF, 0xFF, 0x7F, 0x00]); // AND ECX, 0x7FFFFF
            self.buf.emit(&[0x81, 0xC9, 0x00, 0x00, 0x80, 0x00]); // OR ECX, 0x800000
            self.buf.emit(&[0x85, 0xC0]); // TEST EAX, EAX
            self.buf.emit(&[0x79, 0x02]); // JNS +2
            self.buf.emit(&[0xF7, 0xD9]); // NEG ECX
            self.buf.emit(&[0x89, 0xC8]); // MOV EAX, ECX
            self.buf.emit(&[0x89, 0xD1]); // MOV ECX, EDX
            self.buf.emit(&[0xD3, 0xF8]); // SAR EAX, CL
            self.buf.emit(&[0x83, 0xC0, 0x01]); // ADD EAX, 1
            self.buf.emit(&[0xD1, 0xF8]); // SAR EAX, 1
        }
        self.buf.emit_byte(0xEB); // JMP .done
        let jmp_done = self.buf.pos();
        self.buf.emit_byte(0x00);
        let slow = self.buf.pos();
        // Widening: usize offsets -> i64 (no truncation; displacement math)
        Self::patch_rel8_or_bail(&mut self.buf, ja_slow, slow as i64 - ja_slow as i64 - 1);
        if dbl {
            self.buf.emit(&[0xF2, 0x48, 0x0F, 0x2C, 0xC0]); // CVTTSD2SI RAX, XMM0
        } else {
            self.buf.emit(&[0xF3, 0x0F, 0x2C, 0xC0]); // CVTTSS2SI EAX, XMM0
        }
        self.emit_fp_to_int_nan_fixup(dbl, dbl);
        let done = self.buf.pos();
        // Widening: usize offsets -> i64 (no truncation; displacement math)
        Self::patch_rel8_or_bail(&mut self.buf, jmp_done, done as i64 - jmp_done as i64 - 1);
        if !dbl {
            self.buf.emit(&[0x48, 0x63, 0xC0]); // MOVSXD RAX, EAX
        }
    }

    /// nio21 wave 3, lane L3 (W2L3-3): R11 = an address operand whose base is
    /// null; leaves RDX = the real address of a `w`-byte access, or jumps to a
    /// `decline` edge, exactly where the thin helpers' `raw_accessor_*_bytes`
    /// (`obj == 0`) would not answer from a slotted block: `addr <= 0`
    /// declines; a positive untagged address is a real pointer (`addr < 2^62`,
    /// so `addr + w` cannot wrap); a tagged one (bit 62) resolves through the
    /// JIT-readable arena table
    /// (`ir_lower::scoped_arena_resolve_decline_bytes`, the optimizing tier's
    /// protocol with declines for deopts), and declines when this compile has
    /// no table, for a legacy handle, a freed or resized block, or an access
    /// that does not fit the block. Writes RAX, RDX, R10.
    pub(in crate::x64::op_invoke) fn emit_sp_offheap_address(&mut self, w: u8, decline: &mut Vec<usize>) {
        self.buf.emit(&[0x4D, 0x85, 0xDB]); // TEST R11, R11
        decline.push(self.emit_jcc_rel32_patch(0x8E)); // JLE: addr <= 0
        // A minted static offset is not a pointer (see the bytes twin in `ir_lower`).
        self.buf.emit(&[0x49, 0x8D, 0x93, 0x00, 0x00, 0x00, 0xE0]); // LEA RDX, [R11 - 0x2000_0000]
        self.buf.emit(&[0x48, 0x81, 0xFA, 0xFF, 0xFF, 0xFF, 0x1F]); // CMP RDX, 0x1FFF_FFFF
        decline.push(self.emit_jcc_rel32_patch(0x86)); // JBE: inside the band
        self.buf.emit(&[0x48, 0xB8]); // MOV RAX, imm64
        self.buf.emit(&(1u64 << 62).to_le_bytes());
        self.buf.emit(&[0x49, 0x85, 0xC3]); // TEST R11, RAX
        match crate::ir_lower::scoped_arena_jit_table(&self.direct_helpers) {
            Some(table) => {
                let untagged = self.emit_jcc_rel32_patch(0x84); // JZ
                let (bytes, edges) = crate::ir_lower::scoped_arena_resolve_decline_bytes(table, w);
                let at = self.buf.pos();
                self.buf.emit(&bytes);
                decline.extend(edges.into_iter().map(|e| at + e));
                let resolved = self.emit_jmp_rel32_patch();
                self.patch_rel32_to_here(untagged);
                self.buf.emit(&[0x4C, 0x89, 0xDA]); // MOV RDX, R11
                self.patch_rel32_to_here(resolved);
            }
            None => {
                decline.push(self.emit_jcc_rel32_patch(0x85)); // JNZ: a handle
                self.buf.emit(&[0x4C, 0x89, 0xDA]); // MOV RDX, R11
            }
        }
        self.slot_mirror = None;
    }

    /// nio21 wave 3, lane L3 (W2L3-3): the access itself at RDX (a real
    /// address), as the thin helpers perform it: a put stores the low `w`
    /// bytes of `value` (loaded into RAX); a get leaves the value in RAX
    /// widened as the helpers widen it (`byte` / `short` / `int` sign-extend,
    /// `char` zero-extends, `long` whole). With `order` (a `bigEndian`
    /// operand) the bytes are swapped when it is non-zero, as the helpers'
    /// `to_be_bytes` / `from_be_bytes` do on this little-endian machine.
    /// Writes RAX, and RCX when `order` is given; RDX is preserved.
    pub(in crate::x64::op_invoke) fn emit_sp_raw_access_at_rdx(
        &mut self,
        w: u8,
        signed: bool,
        value: Option<StackSlot>,
        order: Option<StackSlot>,
    ) {
        if let Some(value) = value {
            self.load_slot_to_reg(RAX, value);
            if let Some(order) = order {
                self.emit_sp_swap_rax_if(w, order);
            }
            match w {
                1 => self.buf.emit(&[0x88, 0x02]),       // MOV [RDX], AL
                2 => self.buf.emit(&[0x66, 0x89, 0x02]), // MOV [RDX], AX
                4 => self.buf.emit(&[0x89, 0x02]),       // MOV [RDX], EAX
                _ => self.buf.emit(&[0x48, 0x89, 0x02]), // MOV [RDX], RAX
            }
            self.slot_mirror = None;
            return;
        }
        match w {
            1 => self.buf.emit(&[0x48, 0x0F, 0xBE, 0x02]), // MOVSX RAX, BYTE [RDX]
            2 => self.buf.emit(&[0x0F, 0xB7, 0x02]),       // MOVZX EAX, WORD [RDX]
            4 => self.buf.emit(&[0x8B, 0x02]),             // MOV EAX, [RDX]
            _ => self.buf.emit(&[0x48, 0x8B, 0x02]),       // MOV RAX, [RDX]
        }
        if let Some(order) = order {
            self.emit_sp_swap_rax_if(w, order);
        }
        match w {
            2 if signed => self.buf.emit(&[0x48, 0x0F, 0xBF, 0xC0]), // MOVSX RAX, AX
            2 => self.buf.emit(&[0x0F, 0xB7, 0xC0]),                 // MOVZX EAX, AX
            4 => self.buf.emit(&[0x48, 0x63, 0xC0]),                 // MOVSXD RAX, EAX
            _ => {}
        }
        self.slot_mirror = None;
    }

    /// `RCX = order ; TEST ECX,ECX ; JZ skip ; <swap the low w bytes of RAX> ;
    /// skip:` (nio21 wave 3 L3). A 1-byte access has nothing to swap.
    pub(in crate::x64::op_invoke) fn emit_sp_swap_rax_if(&mut self, w: u8, order: StackSlot) {
        if !matches!(w, 2 | 4 | 8) {
            return;
        }
        self.load_slot_to_reg(RCX, order);
        self.buf.emit(&[0x85, 0xC9]); // TEST ECX, ECX
        let native = self.emit_jcc_rel32_patch(0x84); // JZ: little-endian
        match w {
            2 => self.buf.emit(&[0x66, 0xC1, 0xC0, 0x08]), // ROL AX, 8
            4 => self.buf.emit(&[0x0F, 0xC8]),             // BSWAP EAX
            _ => self.buf.emit(&[0x48, 0x0F, 0xC8]),       // BSWAP RAX
        }
        self.patch_rel32_to_here(native);
    }

    /// nio21 wave 3, lane L3 (proposal W2L3-3 of
    /// `jit-r21-nio-proposals-w2-L3.md`; page
    /// `r20-directbb-every-access-is-two-calls-and-an-arena-lookup-CLOSED-20261012.md`):
    /// a thin-bound `ScopedMemoryAccess.{get,put}{Short,Char,Int,Long}Unaligned`
    /// (or its `*Internal` twin) in a SINGLE-PASS body, as a guarded prefix of
    /// the site's unchanged helper CALL ([`Self::sp_thin_helper_prefix_admitted`]
    /// for the contract). The single-pass twin of the optimizing tier's scoped
    /// fold (`ir_lower::Lowerer::lower_scoped_raw`), with declines where that
    /// fold deopts. Returns the hit edge's `JMP rel32` patch site; a get's
    /// value is in RAX, widened as the helper widens it.
    ///
    /// Served inline, exactly the helper's fast arm
    /// (`scoped_memory_{read,write}_bytes` -> `raw_accessor_*_bytes`):
    ///
    /// * a non-null `ScopedMemoryAccess` receiver (null declines: the call
    ///   raises the NPE);
    /// * a NULL session, or (nio21 wave 4) one whose exact class never closes
    ///   ([`sp_scoped_never_closing_session_enabled`]); any other session
    ///   declines: the helper's liveness verdict, the confined-owner test and
    ///   the shared-close window stay the call's;
    /// * a non-null base that is a one-dimensional PRIMITIVE array whose
    ///   payload holds `[offset - 16, offset - 16 + w)` (the `Unsafe` prefix's
    ///   bytes), or a null base and an off-heap address
    ///   ([`Self::emit_sp_offheap_address`]: a real pointer, or a live slotted
    ///   arena handle);
    /// * the bytes swapped when `bigEndian` is non-zero.
    ///
    /// Why skipping the helper's per-call policy is sound: the bind itself was
    /// admitted by the VM's bind-time policy (`scoped_memory_bind_admitted`,
    /// the same predicate the helper's `jit_direct_helper_refused` asks), and
    /// the `Unsafe` capability gate the helper asks for this width is the VM's
    /// compile-time admission word (`DirectHelperTable::unsafe_inline_admitted`,
    /// required here). With a null session the helper's `AccessWindow` guards
    /// nothing (it orders shared-session closes). A slotted block cannot be
    /// freed under the access for the reasons the optimizing fold gives (the
    /// page's "Soundness": the buffer is held by its caller's
    /// `reachabilityFence`, and the prefix has no safepoint between the
    /// lookup and the access).
    ///
    /// Declines at compile time under `CRATONVM_JIT_SP_SCOPED_ACCESS_PREFIX=0`
    /// or `CRATONVM_JIT_SP_THIN_PREFIX=0`, with no admission bit for the width,
    /// for a helper this prefix does not know, and for an operand in an XMM
    /// register. Writes RAX, RCX, RDX, R10 and R11.
    pub(in crate::x64::op_invoke) fn try_emit_sp_scoped_access_prefix(
        &mut self,
        pc: usize,
        callee_entry: usize,
        arg_slots: &[StackSlot],
    ) -> Option<usize> {
        if self.failed || !sp_thin_prefix_enabled() || !sp_scoped_access_prefix_enabled() {
            return None;
        }
        let access = sp_scoped_access_of(&self.direct_helpers, callee_entry)?;
        if self.direct_helpers.unsafe_inline_admitted & access.unsafe_gate_bit() == 0
            || arg_slots.len() != 1 + access.num_params()
            || arg_slots
                .iter()
                .any(|slot| matches!(slot, StackSlot::Xmm(..)))
        {
            return None;
        }
        let (sma, session) = (arg_slots[0], arg_slots[1]);
        let (base, offset) = (arg_slots[2], arg_slots[3]);
        let value = access.is_put.then(|| arg_slots[4]);
        let order = arg_slots[arg_slots.len() - 1];
        let w = access.width;
        if crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] scoped-access-inline (sp) {} @pc={pc} width={w} put={} arena_table={}",
                self.method_label,
                access.is_put,
                crate::ir_lower::scoped_arena_jit_table(&self.direct_helpers).is_some(),
            );
        }
        let mut decline: Vec<usize> = Vec::new();
        // R10 = the receiver; null declines.
        self.load_slot_to_reg(R10, sma);
        self.emit_test_r64_r64(R10);
        decline.push(self.emit_jcc_rel32_patch(0x84)); // JZ
                                                       // A non-null session declines -- unless (nio21 wave 4 L3, proposal
                                                       // W3L3-1, `sp_scoped_never_closing_session_enabled`) its EXACT class
                                                       // is one the optimizing fold admits as never closing
                                                       // (`ir_lower::scoped_session_never_closing_class_ids`: the JDK's
                                                       // `GlobalSession`, `GlobalSession$HeapSession`, `ImplicitSession`),
                                                       // whose `checkValidStateRaw` is a constant pass and which the
                                                       // helper's `AccessWindow` does not order. A confined session still
                                                       // declines (its owner test needs the current `Thread`).
        self.load_slot_to_reg(RAX, session);
        self.emit_test_r64_r64(RAX);
        let never_closing = if sp_scoped_never_closing_session_enabled() {
            crate::ir_lower::scoped_session_never_closing_class_ids(&self.direct_helpers)
        } else {
            Vec::new()
        };
        if let Some((&last_id, rest)) = never_closing.split_last() {
            let mut session_ok: Vec<usize> = Vec::with_capacity(never_closing.len());
            session_ok.push(self.emit_jcc_rel32_patch(0x84)); // JZ: a null session
                                                              // MOV EAX, [RAX] -- the session's class id (`ObjectHeader` offset
                                                              // 0); the parameter is typed `MemorySessionImpl`, a plain instance.
            self.buf.emit(&[0x8B, 0x00]);
            for &id in rest {
                self.buf.emit_byte(0x3D); // CMP EAX, imm32
                self.buf.emit(&id.to_le_bytes());
                session_ok.push(self.emit_jcc_rel32_patch(0x84)); // JE: admitted
            }
            self.buf.emit_byte(0x3D); // CMP EAX, imm32
            self.buf.emit(&last_id.to_le_bytes());
            decline.push(self.emit_jcc_rel32_patch(0x85)); // JNE: any other session
            for p in session_ok {
                self.patch_rel32_to_here(p);
            }
            if crate::x64::dbg_jitc_enabled() {
                eprintln!(
                    "[cratonvm-jitc] scoped-access-inline (sp) {} @pc={pc} never-closing sessions={never_closing:?}",
                    self.method_label,
                );
            }
        } else {
            decline.push(self.emit_jcc_rel32_patch(0x85)); // JNZ
        }
        // RAX = the base.
        self.load_slot_to_reg(RAX, base);
        self.emit_test_r64_r64(RAX);
        let to_offheap = self.emit_jcc_rel32_patch(0x84); // JZ: null base
                                                          // ---- heap: the `Unsafe` prefix's primitive-array screen (loads R11).
        self.emit_sp_prim_array_screen(w, offset, &mut decline);
        self.buf.emit(&[0x4A, 0x8D, 0x14, 0x18]); // LEA RDX, [RAX + R11]
        let to_access = self.emit_jmp_rel32_patch();
        // ---- off-heap.
        self.patch_rel32_to_here(to_offheap);
        self.load_slot_to_reg(R11, offset);
        self.emit_sp_offheap_address(w, &mut decline);
        // ---- the access at RDX.
        self.patch_rel32_to_here(to_access);
        self.emit_sp_raw_access_at_rdx(w, access.signed, value, Some(order));
        let hit = self.emit_jmp_rel32_patch();
        for p in decline {
            self.patch_rel32_to_here(p);
        }
        self.slot_mirror = None;
        Some(hit)
    }

    /// gci i1/sp, under [`sp_zero_dead_arg_sources_enabled`]: overwrite with 0
    /// every popped reference-argument source slot
    /// ([`dead_arg_source_offsets`]) of the call being staged, once the
    /// staging has read them. Emits nothing off the switch or when the pops
    /// reclaimed nothing. Returns the zeroed offsets (for the `[sp-band]`
    /// census line and the tests). `MOV qword [rbp - off], imm32 0` per word:
    /// no register is written, so the ABI registers and RAX the staging left
    /// are untouched.
    pub(in crate::x64) fn emit_zero_dead_arg_sources(
        &mut self,
        arg_slots: &[StackSlot],
        arg_oops: &[bool],
        post_pop_spill: i32,
        pre_pop_spill: i32,
    ) -> Vec<i32> {
        if !sp_zero_dead_arg_sources_enabled() {
            return Vec::new();
        }
        let offs = dead_arg_source_offsets(
            arg_slots,
            arg_oops,
            self.base_spill_offset,
            post_pop_spill,
            pre_pop_spill,
        );
        for &off in &offs {
            self.emit_zero_frame_word(off);
        }
        if !offs.is_empty() && super::safepoint::sp_band_owners_wanted(&self.method_label) {
            // `splice` (gci i2/sp): 0 for a top-level call arm, >= 1 for
            // a call inside a spliced body (`inlining.rs`, C1).
            eprintln!(
                "[sp-band-zero] method={} sp={} post_pop={} pre_pop={} zeroed={:?} splice={}",
                self.method_label,
                self.cur_bc_pc,
                post_pop_spill,
                pre_pop_spill,
                offs,
                self.inline_oop_scopes.len()
            );
        }
        offs
    }

    /// ARGWIN21-9 (the section above
    /// [`sp_owned_call_dead_local_clears_enabled`]): at an owned direct call,
    /// after the marshal, zero every reference local dead at the invoke and at
    /// the next instruction. Returns the local indices zeroed.
    pub(in crate::x64) fn emit_owned_call_dead_local_clears(&mut self, owned: bool) -> Vec<usize> {
        let mut cleared: Vec<usize> = Vec::new();
        if !owned
            || self.debugger_observes_locals
            || !self.inline_scope_stack.is_empty()
            || !self.inline_oop_scopes.is_empty()
            || !sp_owned_call_dead_local_clears_enabled()
        {
            return cleared;
        }
        let pc = self.cur_bc_pc;
        if !self.local_oop_reached.get(pc).copied().unwrap_or(false) {
            return cleared;
        }
        let Some(mask) = self.local_oop_masks.get(pc).copied() else {
            return cleared;
        };
        let mut dead = self.dead_oop_locals_rule(0, mask, true);
        while dead != 0 {
            // Cast: a bit index below 64.
            let k = dead.trailing_zeros() as usize;
            dead &= dead - 1;
            if k >= self.num_locals {
                continue;
            }
            let off = self.local_offset(k);
            let reg = self.reg_for_local(k);
            let aliased = self.stack.iter().any(|slot| match *slot {
                StackSlot::Frame(o) => o == off,
                StackSlot::CalleeSaved(r) => Some(r) == reg,
                _ => false,
            });
            if aliased || off <= 0 {
                continue;
            }
            if let Some(r) = reg {
                let shared = (0..self.num_locals).any(|j| j != k && self.reg_for_local(j) == Some(r));
                if shared || ARG_REGS.contains(&r) {
                    continue;
                }
                self.emit_xor_reg_self(r);
            }
            self.emit_zero_frame_word(off);
            cleared.push(k);
        }
        if !cleared.is_empty() && super::safepoint::sp_band_owners_wanted(&self.method_label) {
            eprintln!(
                "[sp-owned-local-zero] method={} pc={pc} locals={cleared:?}",
                self.method_label
            );
        }
        cleared
    }

    /// `MOV qword [rbp - off], 0` (REX.W C7 /0 imm32). The slot mirror is
    /// dropped: the word no longer holds what a register was last paired with.
    /// Callers pass only `off > 0` ([`dead_arg_source_offsets`]).
    pub(in crate::x64) fn emit_zero_frame_word(&mut self, off: i32) {
        self.buf.emit_byte(0x48); // REX.W
        self.buf.emit_byte(0xC7); // MOV r/m64, imm32 (sign-extended)
        self.modrm_rbp_disp(0, off);
        self.buf.emit(&0i32.to_le_bytes());
        self.slot_mirror = None;
    }

    /// Marshal an inline-cache hit's `n` staged arguments into the compiled
    /// entry ABI: `vm_ptr` first when `with_ctx`, then the Java arguments in
    /// order, from the descending args buffer at `args_base_offset`.
    ///
    /// Arguments past the register file go to `[RSP + stack_base + 8k]`, the
    /// layout `emit_stack_arg_setup` writes for a direct call and every
    /// compiled prologue reads (`emit_load_caller_arg`); the caller has
    /// already lowered RSP by the block `ic_stack_arg_block` sized. With
    /// `stack_base == None` (the only shape before round 11 wave 12) every
    /// argument must fit, and the emitted bytes are exactly the two loops this
    /// replaced: the context word first, then `ARG_REGS[lead..lead + n]`.
    ///
    /// Stack words are written FIRST and through RAX (dead here: it held the
    /// receiver's class id), so no register argument is loaded before a
    /// store that could need its register. R11, the call target, is never
    /// written.
    pub(in crate::x64::op_invoke) fn emit_ic_hit_arg_marshal(
        &mut self,
        n: usize,
        args_base_offset: i32,
        with_ctx: bool,
        stack_base: Option<i32>,
    ) {
        let lead = usize::from(with_ctx);
        let staged = |j: usize| args_base_offset + ((n - 1 - j) as i32) * 8; // Cast: x86-64 immediate encoding
        if let Some(base) = stack_base {
            for j in 0..n {
                let Some(k) = (j + lead).checked_sub(ARG_REGS.len()) else {
                    continue;
                };
                self.emit_load_local(RAX, staged(j));
                self.emit_mov_rsp_disp_from_reg(base + (k as i32) * 8, RAX); // Cast: x86-64 immediate encoding
            }
        }
        if with_ctx {
            self.emit_load_vm_context(ARG_REGS[0]);
        }
        for j in 0..n {
            if let Some(&reg) = ARG_REGS.get(j + lead) {
                self.emit_load_local(reg, staged(j));
            }
        }
    }

    /// Does any `invokedynamic` site in this method lower to the reason-8
    /// uncommon trap rather than to the bridge call?
    ///
    /// One question, asked of the whole body, by the self-recursive arm in
    /// `walk_invokestatic`: a raw `CALL` to this method's own entry is
    /// unsound only while a trap inside the inner invocation can stash a
    /// frame under this method's identity. It is the negation of
    /// [`indy_site_is_bridged`] over `indy_info`, i.e. the same three terms
    /// the 0xba arm admits a bridged site on, so the two cannot drift.
    ///
    /// `driver.rs` computes the identical predicate for
    /// `CompiledMethod::has_indy_trap`, which is the CALLER-side half of the
    /// same fact; if that copy is ever unified with this one, this is the
    /// function to call.
    pub(in crate::x64) fn has_trapping_indy_site(&self) -> bool {
        // j27 wave 7 lane T1DISP20 (SA6-3): exactly the predicate `driver.rs`
        // computes `CompiledMethod::has_indy_trap` with.
        crate::indy_rows_have_trap(&self.indy_info, self.direct_helpers.indy_bridge)
    }

    /// nio21 wave 10, lane L3 (W9L2-1, `CRATONVM_JIT_INDY_CONSTANT_LAMBDA`):
    /// the CALL-free half of a non-capturing lambda's bridged `invokedynamic`.
    ///
    /// ```text
    ///   MOV  RAX, imm64 site
    ///   MOV  RAX, [RAX + 8]      ; the site's constant word: slot address or 0
    ///   TEST RAX, RAX
    ///   JZ   miss
    ///   MOV  RAX, [RAX]          ; the singleton, GC-maintained, or 0 (retired)
    ///   TEST RAX, RAX
    ///   JNZ  done                ; answered: patched by the caller
    /// miss:
    ///   <the bridge sequence, unchanged>
    /// done:
    /// ```
    ///
    /// Answers the `JNZ`'s patch offset. The word at `site + 8`
    /// (`vm::runtime::invokedynamic::JIT_INDY_SITE_CONSTANT_WORD_OFFSET`) is
    /// `0` until the VM learns the instruction's `LAMBDA_SINGLETON_CACHE` row
    /// (the bridge's first warm evaluation), then the address of that row's
    /// slot -- the same row the interpreter answers the instruction from, so
    /// the instance is the interpreter's. The slot is a GC root rewritten in
    /// place only while mutators are parked, which a thread running this
    /// sequence is not: the `ldc` slot's argument (`objects.rs`
    /// `emit_ldc_cp_site`). Emitted after the scratch flush and before the
    /// spill, so the two arms meet with one operand-stack model; it clobbers
    /// RAX and the flags only.
    pub(in crate::x64) fn emit_indy_constant_probe(&mut self, site: usize) -> usize {
        // Cast: a process-lived site address as the imm64 operand of MOVABS.
        self.emit_mov_imm64(RAX, site as i64);
        self.emit_mov_r64_mem_disp32(RAX, RAX, INDY_SITE_CONSTANT_WORD_OFFSET);
        self.emit_test_r64_r64(RAX);
        let miss = self.emit_jcc_rel32_patch(0x84); // JZ miss
        self.emit_mov_r64_mem_disp32(RAX, RAX, 0);
        self.emit_test_r64_r64(RAX);
        let hit = self.emit_jcc_rel32_patch(0x85); // JNZ done
        self.patch_rel32_to_here(miss);
        hit
    }


    /// Tier 1 x64 fast path for `Method.invoke(obj, null)` via `reflect_method_invoke_cache`.
    /// On cache hit, calls the cached compiled target directly and jumps to `.done`.
    /// On cache miss, falls through to the `invoke_virtual_mic` slow path so
    /// `try_populate_method_invoke_cache` can install the entry once the target compiles.
    pub(in crate::x64) fn emit_method_invoke_cache_prefix(
        &mut self,
        info: *const JitInvokeInfo,
        info_ref: &JitInvokeInfo,
        args_base_offset: i32,
        lazy_ic_hit_republish: bool,
        done_patches32: &mut Vec<usize>,
    ) {
        self.slot_mirror = None;
        let mut miss_patches: Vec<usize> = Vec::new();

        // 1. args[2] == null (0-arg reflective invoke)
        self.emit_load_local(RAX, args_base_offset);
        self.emit_test_r64_r64(RAX);
        miss_patches.push(self.emit_jcc_rel32_patch(0x85)); // JNZ .miss

        // 2. args[0] (Method) != null -> RCX
        self.emit_load_local(RCX, args_base_offset + 16);
        self.emit_test_r64_r64(RCX);
        miss_patches.push(self.emit_jcc_rel32_patch(0x84)); // JZ .miss

        // 3. hidden frames guard
        self.emit_mov_imm64(RAX, self.direct_helpers.reflect_hidden_observed_addr as i64);
        self.buf.emit(&[0x80, 0x38, 0x00]); // cmp byte ptr [rax], 0
        miss_patches.push(self.emit_jcc_rel32_patch(0x85)); // JNZ .miss

        // 4. R10 = slot_addr = cache + ((method_ref >> 4) & 63) * 32
        self.emit_mov_r64_r64(RAX, RCX);
        self.buf.emit(&[0x48, 0xC1, 0xE8, 0x04]); // shr rax, 4
        self.buf.emit(&[0x83, 0xE0, 0x3F]); // and eax, 63
        self.buf.emit(&[0x48, 0xC1, 0xE0, 0x05]); // shl rax, 5
        self.emit_mov_imm64(R10, self.direct_helpers.reflect_method_invoke_cache as i64);
        self.buf.emit(&[0x49, 0x01, 0xC2]); // add r10, rax

        // 5. meta = [r10] -> RDX; valid bit
        self.buf.emit(&[0x49, 0x8B, 0x12]); // mov rdx, [r10]
        self.buf.emit(&[0xF6, 0xC2, 0x01]); // test dl, 1
        miss_patches.push(self.emit_jcc_rel32_patch(0x84)); // JZ .miss

        // 6. gc_epoch check
        self.emit_mov_imm64(RAX, self.direct_helpers.reflect_gc_epoch_addr as i64);
        self.buf.emit(&[0x48, 0x8B, 0x00]); // mov rax, [rax]
        self.buf.emit(&[0x49, 0x89, 0xD3]); // mov r11, rdx
        self.buf.emit(&[0x49, 0xC1, 0xEB, 0x10]); // shr r11, 16
        self.buf.emit(&[0x49, 0x39, 0xC3]); // cmp r11, rax
        miss_patches.push(self.emit_jcc_rel32_patch(0x85)); // JNE .miss

        // 7. handle_bits == RCX
        self.buf.emit(&[0x49, 0x39, 0x4A, 0x08]); // cmp [r10 + 8], rcx
        miss_patches.push(self.emit_jcc_rel32_patch(0x85)); // JNE .miss

        // 8. caller_cid == info_ref.declaring_class_id
        self.buf.emit(&[0x41, 0x81, 0x7A, 0x14]); // cmp dword ptr [r10 + 20], imm32
        self.buf.emit(&info_ref.declaring_class_id.to_le_bytes());
        miss_patches.push(self.emit_jcc_rel32_patch(0x85)); // JNE .miss

        // 9. ret_kind in RAX <= 3
        self.buf.emit(&[0x48, 0x89, 0xD0]); // mov rax, rdx
        self.buf.emit(&[0x48, 0xC1, 0xE8, 0x08]); // shr rax, 8
        self.buf.emit(&[0x25, 0xFF, 0x00, 0x00, 0x00]); // and eax, 0xff
        self.buf.emit(&[0x83, 0xF8, 0x03]); // cmp eax, 3
        miss_patches.push(self.emit_jcc_rel32_patch(0x87)); // JA .miss

        // 10. entry_ptr_tagged = [r10 + 24] -> R11
        self.buf.emit(&[0x4D, 0x8B, 0x5A, 0x18]); // mov r11, [r10 + 24]
        self.buf.emit(&[0x4D, 0x85, 0xDB]); // test r11, r11
        miss_patches.push(self.emit_jcc_rel32_patch(0x84)); // JZ .miss

        // 11. is_static check (dl & 2)
        self.buf.emit(&[0xF6, 0xC2, 0x02]); // test dl, 2
        let static_patch = self.emit_jcc_rel32_patch(0x85); // JNZ .static

        // Instance path: check args[1] (receiver) != null, non-array, class_id == [r10 + 16]
        self.emit_load_local(RCX, args_base_offset + 8);
        self.emit_test_r64_r64(RCX);
        miss_patches.push(self.emit_jcc_rel32_patch(0x84)); // JZ .miss
        self.buf.emit(&[
            0xF6,
            0x41,
            cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8,
            cratonvm_types::KIND_TAG_BYTE_MASK,
        ]);
        miss_patches.push(self.emit_jcc_rel32_patch(0x85)); // JNZ .miss
        self.buf.emit(&[0x8B, 0x11]); // mov edx, [rcx]
        self.buf.emit(&[0x41, 0x3B, 0x52, 0x10]); // cmp edx, [r10 + 16]
        miss_patches.push(self.emit_jcc_rel32_patch(0x85)); // JNE .miss

        // Save ret_kind in R10 across call
        self.emit_mov_r64_r64(R10, RAX);
        self.buf.emit(&[0x49, 0x0F, 0xBA, 0xF3, 0x00]); // btr r11, 0
        let no_ctx_inst = self.emit_jcc_rel32_patch(0x83); // JNC .no_ctx_inst
        self.emit_mov_r64_r64(ARG_REGS[1], RCX);
        self.emit_load_vm_context(ARG_REGS[0]);
        let call_inst = self.emit_jmp_rel32_patch();
        self.patch_rel32_to_here(no_ctx_inst);
        self.emit_mov_r64_r64(ARG_REGS[0], RCX);
        let call_inst2 = self.emit_jmp_rel32_patch();

        // Static path
        self.patch_rel32_to_here(static_patch);
        self.emit_mov_r64_r64(R10, RAX);
        self.buf.emit(&[0x49, 0x0F, 0xBA, 0xF3, 0x00]); // btr r11, 0
        let no_ctx_stat = self.emit_jcc_rel32_patch(0x83); // JNC .do_call
        self.emit_load_vm_context(ARG_REGS[0]);

        self.patch_rel32_to_here(no_ctx_stat);
        self.patch_rel32_to_here(call_inst);
        self.patch_rel32_to_here(call_inst2);

        // Call R11 while preserving R10 (ret_kind) on 16-byte aligned stack
        #[cfg(not(target_os = "windows"))]
        {
            self.buf.emit(&[0x48, 0x83, 0xEC, 0x10]); // sub rsp, 16
            self.buf.emit(&[0x4C, 0x89, 0x14, 0x24]); // mov [rsp], r10
            self.buf.emit(&[0x41, 0xFF, 0xD3]); // call r11
            self.buf.emit(&[0x4C, 0x8B, 0x14, 0x24]); // mov r10, [rsp]
            self.buf.emit(&[0x48, 0x83, 0xC4, 0x10]); // add rsp, 16
        }
        #[cfg(target_os = "windows")]
        {
            self.buf.emit(&[0x48, 0x83, 0xEC, 0x30]); // sub rsp, 48
            self.buf.emit(&[0x4C, 0x89, 0x54, 0x24, 0x20]); // mov [rsp+32], r10
            self.buf.emit(&[0x41, 0xFF, 0xD3]); // call r11
            self.buf.emit(&[0x4C, 0x8B, 0x54, 0x24, 0x20]); // mov r10, [rsp+32]
            self.buf.emit(&[0x48, 0x83, 0xC4, 0x30]); // add rsp, 48
        }

        if !lazy_ic_hit_republish {
            self.emit_post_call_rbp_republish();
        }
        self.emit_inline_callee_deopt_check_site_open(info, 3, args_base_offset);

        // Check exception sentinel (RAX == i64::MIN)
        self.emit_mov_imm64(R11, i64::MIN);
        self.buf.emit(&[0x4C, 0x39, 0xD8]); // cmp rax, r11
        let exc_patch = self.emit_jcc_rel32_patch(0x84); // JE .exc_sentinel

        // Box/convert return value based on ret_kind in R10d
        self.buf.emit(&[0x41, 0x83, 0xFA, 0x01]); // cmp r10d, 1 (OBJ)
        done_patches32.push(self.emit_jcc_rel32_patch(0x84)); // JE .done

        self.buf.emit(&[0x41, 0x83, 0xFA, 0x00]); // cmp r10d, 0 (VOID)
        let not_void = self.emit_jcc_rel32_patch(0x85);
        self.emit_xor_reg_self(RAX);
        done_patches32.push(self.emit_jmp_rel32_patch());

        self.patch_rel32_to_here(not_void);
        self.buf.emit(&[0x41, 0x83, 0xFA, 0x02]); // cmp r10d, 2 (BOOL)
        let not_bool = self.emit_jcc_rel32_patch(0x85);
        self.buf.emit(&[0x85, 0xC0]); // test eax, eax
        let bool_false = self.emit_jcc_rel32_patch(0x84);
        self.emit_mov_imm64(RAX, self.direct_helpers.reflect_boolean_true_addr as i64);
        self.buf.emit(&[0x48, 0x8B, 0x00]); // mov rax, [rax]
        done_patches32.push(self.emit_jmp_rel32_patch());

        self.patch_rel32_to_here(bool_false);
        self.emit_mov_imm64(RAX, self.direct_helpers.reflect_boolean_false_addr as i64);
        self.buf.emit(&[0x48, 0x8B, 0x00]); // mov rax, [rax]
        done_patches32.push(self.emit_jmp_rel32_patch());

        self.patch_rel32_to_here(not_bool);
        // ret_kind == 3 (INT): Integer.valueOf(vm_ptr, val)
        self.emit_mov_r64_r64(ARG_REGS[1], RAX);
        self.emit_load_vm_context(ARG_REGS[0]);
        self.emit_call_absolute(self.direct_helpers.integer_value_of);
        done_patches32.push(self.emit_jmp_rel32_patch());

        // Exception sentinel: call reflect_method_invoke_sentinel(vm_ptr, info, args_ptr, 3)
        self.patch_rel32_to_here(exc_patch);
        self.emit_load_vm_context(ARG_REGS[0]);
        self.emit_mov_imm64(ARG_REGS[1], info as *const _ as i64);
        self.emit_lea_frame_slot(ARG_REGS[2], args_base_offset + 16);
        self.emit_mov_imm32_sx(ARG_REGS[3], 3);
        self.emit_call_absolute(self.direct_helpers.reflect_method_invoke_sentinel);
        done_patches32.push(self.emit_jmp_rel32_patch());

        for p in miss_patches {
            self.patch_rel32_to_here(p);
        }
        self.slot_mirror = None;
    }
}
