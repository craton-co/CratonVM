#[allow(unused_imports)]
use super::*;

impl<'a> Lowerer<'a> {
    /// The base / address / access half of [`Self::lower_scoped_raw`] (split
    /// out unchanged by JIT r20 wave 19, lane yFfmShared, so the shared arm
    /// can emit it a second time): the session check has passed (or been
    /// classified shared, `window`). With `window`, the access itself runs
    /// inside this thread's FFM access window, opened after every guard of
    /// the address computation and closed right after the load or store
    /// ([`Self::emit_scoped_window_open`]).
    #[allow(clippy::too_many_arguments)]
    pub(in crate::ir_lower) fn emit_scoped_raw_tail(
        &mut self,
        node: &Node,
        width: u8,
        signed: bool,
        put_value: Option<NodeId>,
        order_input: Option<NodeId>,
        bci: usize,
        slot: i32,
        window: Option<ScopedSharedWindow>,
        proven: bool,
    ) {
        let w = i8::try_from(width).unwrap_or(8);
        let base = node.inputs[4];
        let cse = true;

        // nio21 wave 1 L3: a LITERAL-null base (a direct buffer's `null,
        // address + i`, a native segment's) has no heap arm at all; the
        // builder admits one only when the JIT-readable arena table is known
        // (`IrBuilder::set_scoped_offheap_fold`), and the off-heap arm below
        // resolves an arena handle through it.
        let base_is_null = self
            .graph
            .nodes
            .get(base as usize)
            .is_some_and(crate::ir::is_null_const);
        let arena_table = scoped_arena_jit_table(&self.direct_helpers);
        if !base_is_null {
            self.gp_load_value(RDX, base); // RDX = base
        }
        self.gp_load_value(R11, node.inputs[5]); // R11 = offset
                                                 // A base this block already proved non-null (or one that cannot be
                                                 // null) has no off-heap arm to branch to.
                                                 // JIT r23 wave 8 (lane W8-L4): an FFM expansion's heap access has a
                                                 // proven in-bounds primitive-array base (`Graph::ffm_proven_heap_accesses`).
        let proven = proven && !base_is_null;
        let base_non_null = !base_is_null
            && (proven
                || store_receiver_cannot_be_null(self.graph, base)
                || (cse && self.receiver_already_non_null(base)));
        let to_direct = if base_non_null || base_is_null {
            None
        } else {
            self.buf.emit(&[0x48, 0x85, 0xD2]); // TEST RDX, RDX
            Some(self.emit_jcc_rel32(0x84)) // JZ -> direct (base == null)
        };
        if proven {
            // JIT r23 wave 8 (lane W8-L4): no screen, no bounds -- the
            // expansion's guards proved both. RDX = the element address.
            self.buf.emit(&[0x4A, 0x8D, 0x14, 0x1A]); // LEA RDX, [RDX + R11]
            if crate::x64::dbg_jitc_enabled() {
                eprintln!(
                    "[cratonvm-jitc] scoped-heap-proven (ir) bci={bci} n{base} in={}",
                    self.graph.scope_method_key
                );
            }
        } else if !base_is_null {
            // nio21 wave 5, lane L4: a second heap arm on the same base node
            // in this block skips the kind screen's deopt (see
            // `scoped_screened_bases`).
            //
            // nio21 wave 7, lane L4 (W5L4-3): a base the graph knows is a
            // `byte[]` or null (`Graph::byte_array_nodes`: `ByteBuffer.hb`)
            // takes no screen and no element-size read at all.
            if crate::ir::ir_scoped_byte_array_base_enabled()
                && (self.graph.byte_array_nodes.contains(&base)
                    || (ir_scoped_byte_array_base_widen_enabled()
                        && scoped_base_is_byte_array(self.graph, base)))
            {
                self.emit_scoped_byte_array_address(w, bci);
                if crate::x64::dbg_jitc_enabled() {
                    eprintln!(
                        "[cratonvm-jitc] scoped-byte-array-base (ir) bci={bci} n{base} in={}",
                        self.graph.scope_method_key
                    );
                }
            } else {
                let screen_cse = true;
                let screened = screen_cse && self.scoped_screened_bases.contains(&base);
                self.emit_scoped_heap_array_address(w, bci, screened);
                if screen_cse && !screened {
                    self.scoped_screened_bases.push(base);
                }
                // JIT round 20 (lane memory): the m7 pass saw no
                // `scoped-byte-array-base` line on `Nio20RawAlias heapbb` and
                // nothing said what the base was instead. Name it.
                if crate::x64::dbg_jitc_enabled() {
                    let op = self
                        .graph
                        .nodes
                        .get(base as usize)
                        .map(|n| format!("{:?}", n.op));
                    eprintln!(
                        "[cratonvm-jitc] scoped-heap-base-screened (ir) bci={bci} n{base} op={} \
                         screened={screened} byte_array_nodes={} in={}",
                        op.as_deref().unwrap_or("?"),
                        self.graph.byte_array_nodes.len(),
                        self.graph.scope_method_key,
                    );
                }
            }
        }

        // ---- off-heap: a positive real address, or (nio21 wave 1 L3) a live
        // `Unsafe` arena handle the JIT-readable table resolves.
        if base_is_null || to_direct.is_some() {
            let to_access = match to_direct {
                Some(to_direct) => {
                    let j = self.emit_jmp_rel32();
                    self.patch_rel32_to_here(to_direct);
                    Some(j)
                }
                None => None,
            };
            self.buf.emit(&[0x4D, 0x85, 0xDB]); // TEST R11, R11
            self.emit_deopt_unless(0x8F, bci, DeoptReason::ReceiverTypeChanged); // JG: offset > 0
            self.emit_mov_reg_imm64(RAX, 1u64 << 62);
            self.buf.emit(&[0x49, 0x85, 0xC3]); // TEST R11, RAX
            match arena_table {
                Some(table) => {
                    let untagged = self.emit_jcc_rel32(0x84); // JZ -> untagged
                    self.emit_scoped_arena_resolve(table, w, bci);
                    let resolved = self.emit_jmp_rel32();
                    self.patch_rel32_to_here(untagged);
                    self.buf.emit(&[0x4C, 0x89, 0xDA]); // MOV RDX, R11
                    self.patch_rel32_to_here(resolved);
                }
                None => {
                    self.emit_deopt_unless(0x84, bci, DeoptReason::ReceiverTypeChanged); // JZ: untagged
                    self.buf.emit(&[0x4C, 0x89, 0xDA]); // MOV RDX, R11
                }
            }
            if let Some(to_access) = to_access {
                self.patch_rel32_to_here(to_access);
            }
        }

        // A constant `bigEndian` (nio20 wave 3) decides the swap here: `Some(false)`
        // emits none, `Some(true)` an unconditional one, `None` the run-time test.
        let order_const: Option<bool> =
            order_input.and_then(|o| match self.graph.nodes.get(o as usize).map(|n| &n.op) {
                Some(Op::Const(v)) => Some(*v as i32 != 0),
                _ => None,
            });
        // JIT r20 wave 19 (lane yFfmShared, MEM20-2): every guard above has
        // passed; open the window, prove the session open inside it, access,
        // close it right after the load or store.
        if let Some(win) = window {
            self.emit_scoped_window_open(win, bci);
        }
        self.emit_scoped_access_at_rdx(
            width,
            signed,
            put_value,
            order_input,
            order_const,
            slot,
            window.is_some(),
        );
    }

    /// JIT r20 wave 19 (lane yFfmShared, MEM20-2 of
    /// `docs/feature-designs/jit-ir-optimizations.md`): open this
    /// thread's FFM access window (`native-builtins` `ffm_fast::AccessWindow`)
    /// for a SHARED model session and prove the session open INSIDE it; on a
    /// closed session close the window again and deopt (the interpreter then
    /// raises `IllegalStateException: Already closed` through the accessor).
    /// RDX (the element address), RCX and R11 are preserved; writes RAX, R10
    /// (= the word, which [`Self::emit_scoped_window_close`] reads) and the flags.
    ///
    /// ```text
    ///   MOV  RAX, seg:[disp] ; MOV R10, RAX   ; &seq of this thread's leased AccessWord
    ///   TEST R10, R10 ; deopt if zero         ; no word leased yet (the deopt leases one)
    ///   TEST BYTE [R10], 1 ; deopt unless 0   ; a native window is open (nested callback)
    ///   LOCK INC QWORD [R10]                  ; open: odd
    ///   MOV  RAX, session
    ///   TEST BYTE [RAX+GC_FLAGS], COMPACT ; JZ legacy
    ///   MOV  EAX, [RAX+state_c] ; JMP have
    /// legacy:
    ///   MOV  EAX, [RAX+state_l]
    /// have:
    ///   CMP  EAX, 1 ; JE open
    ///   INC  QWORD [R10] ; XOR EAX, EAX ; deopt   ; closed: close the window, leave
    /// open:
    /// ```
    ///
    /// # Why the access cannot outlive `close()`
    ///
    /// The closer (`foreign_ffm::p67_session_just_close` ->
    /// `panama::pe_close_handshake` -> `ffm_fast::close_handshake_begin`)
    /// flips `state` 1 -> 0 by CAS, then probes every leased word with a
    /// value-preserving `LOCK XADD 0` and waits, in a GC-safe region, for each
    /// word it read odd to change. The JDK's `SharedSession` (JIT round 22)
    /// closes the same way: `justClose` stores `state = CLOSED` (-1,
    /// `STATE.setVolatile`) and only then calls `closeScope0`, whose native
    /// is the same `pe_close_handshake(.., shared = true, ..)` probe; its
    /// `ResourceList` cleanup (the free) runs after that returns. The open here and that probe are locked
    /// RMWs on the same word, so one precedes the other in its modification
    /// order:
    ///
    /// * the open first: the probe reads odd and the closer waits until the
    ///   `INC` after the access (the only writer of that word's value is this
    ///   thread, and an x86 store is a release), so the access happens-before
    ///   the close returns, the Java close actions run and any block is freed;
    /// * the probe first: the open reads from it, so the closer's state flip
    ///   (before its probe) is visible to the `state` load here (x86 never
    ///   moves a load above a locked instruction), which reads `0` and deopts.
    ///
    /// A word leased after a closer's scan is the second case through the
    /// registry lock (`AccessWindow`'s own argument). The address resolved
    /// before the window (an arena block) is dereferenced only after `state`
    /// read open inside it, so no free can have preceded the access. Nothing
    /// between the open and the close can call, allocate or safepoint; a
    /// stop-the-world freeze inside it only delays the close, and the closer
    /// waits GC-safe. The nested case deopts rather than toggling the word:
    /// an outer native window (depth > 0) already covers this access, but its
    /// close must stay the outer window's.
    pub(in crate::ir_lower) fn emit_scoped_window_open(
        &mut self,
        win: ScopedSharedWindow,
        bci: usize,
    ) {
        let reason = DeoptReason::ReceiverTypeChanged;
        // A thread's first compiled shared access (no word leased yet) and a
        // nested one are per-thread transients, not failed speculations: they
        // leave uncharged (`TransferToInterpreter`, re-executed), so a pool of
        // fresh threads cannot de-specialise the site.
        let transient = DeoptReason::TransferToInterpreter;
        self.emit_mov_rax_tls_disp32(win.disp);
        self.buf.emit(&[0x49, 0x89, 0xC2]); // MOV R10, RAX
        self.buf.emit(&[0x4D, 0x85, 0xD2]); // TEST R10, R10
        self.emit_deopt_unless(0x85, bci, transient); // JNZ: a word is leased
        self.buf.emit(&[0x41, 0xF6, 0x02, 0x01]); // TEST BYTE [R10], 1
        self.emit_deopt_unless(0x84, bci, transient); // JZ: no window open on this thread
        self.buf.emit(&[0xF0, 0x49, 0xFF, 0x02]); // LOCK INC QWORD [R10]
        self.gp_load_value(RAX, win.session);
        // TEST BYTE [RAX + GC_FLAGS], COMPACT (F6 /0 ib, disp8) ; JZ legacy
        self.buf.emit(&[
            0xF6,
            0x40,
            cratonvm_types::GC_FLAGS_BYTE_OFFSET as u8, // Cast: x86-64 disp8
            cratonvm_types::GC_FLAG_COMPACT,
        ]);
        let legacy = self.emit_jcc_rel32(0x84);
        self.buf.emit(&[0x8B, 0x80]); // MOV EAX, [RAX + disp32]
        self.buf.emit(&win.state_compact.to_le_bytes());
        let have = self.emit_jmp_rel32();
        self.patch_rel32_to_here(legacy);
        self.buf.emit(&[0x8B, 0x80]); // MOV EAX, [RAX + disp32]
        self.buf.emit(&win.state_legacy.to_le_bytes());
        self.patch_rel32_to_here(have);
        let open = match win.open {
            ScopedSessionOpen::EqualsOne => {
                self.buf.emit(&[0x83, 0xF8, 0x01]); // CMP EAX, 1
                self.emit_jcc_rel32(0x84) // JE: open
            }
            // JIT round 22: the JDK's encoding (`SharedSession.justClose`
            // stores `CLOSED == -1` before `closeScope0` probes the words).
            ScopedSessionOpen::NonNegative => {
                self.buf.emit(&[0x85, 0xC0]); // TEST EAX, EAX
                self.emit_jcc_rel32(0x89) // JNS: state >= OPEN
            }
        };
        self.emit_scoped_window_close();
        self.buf.emit(&[0x31, 0xC0]); // XOR EAX, EAX (ZF = 1)
        self.emit_deopt_if_zero(bci, reason);
        self.patch_rel32_to_here(open);
    }

    /// Close the window [`Self::emit_scoped_window_open`] opened (R10 = the
    /// word): `INC QWORD [R10]`, the next even value. No `LOCK`: only this
    /// thread changes the word's value (a closer's probe adds 0), and an x86
    /// store is a release -- `AccessWindow`'s `store(seq + 1, Release)`.
    pub(in crate::ir_lower) fn emit_scoped_window_close(&mut self) {
        self.buf.emit(&[0x49, 0xFF, 0x02]); // INC QWORD [R10]
    }

    /// JIT r20 wave 19 (lane yFfmShared, MEM20-2): may this site lower a
    /// SHARED model session through the access window? `Some` when the switch
    /// is on, the VM answers this thread-invariant TLS displacement of the
    /// access-word mirror ([`scoped_access_word_tls_disp`]), the model row is
    /// among `confined` (its `state` addresses), and no value the tail reads or
    /// this node defines is register-carried (the tail is emitted twice, and a
    /// carry is consumed by its first read).
    pub(in crate::ir_lower) fn scoped_shared_window_for(
        &self,
        node: &Node,
        confined: &[ScopedConfinedRow],
        put_value: Option<NodeId>,
        order_input: Option<NodeId>,
    ) -> Option<ScopedSharedWindow> {
        // JIT round 22 (lane memory): the JDK's `SharedSession` first -- since
        // the `JdkForeign` alignment row it is every `Arena.ofShared()`
        // session in both modes, and the model row then never holds one.
        let row = confined.iter().find(|r| r.shared_only).or_else(|| {
            confined
                .iter()
                .find(|r| r.open == ScopedSessionOpen::EqualsOne)
        })?;
        let disp = scoped_access_word_tls_disp(&self.direct_helpers)?;
        let tail_inputs = [
            Some(node.inputs[4]),
            Some(node.inputs[5]),
            put_value,
            order_input,
        ];
        let carried = |id: NodeId| {
            matches!(self.live_carry, Some((prod, ..)) if prod == id)
                || matches!(self.deferred_rcx, Some((prod, ..)) if prod == id)
        };
        if tail_inputs.iter().flatten().any(|&id| carried(id)) {
            return None;
        }
        if let Some(def) = self.cur_def {
            if self.carry_of.get(def as usize).copied().flatten().is_some() {
                return None;
            }
        }
        Some(ScopedSharedWindow {
            disp,
            session: node.inputs[3],
            class_id: row.class_id,
            open: row.open,
            state_compact: row.state_compact,
            state_legacy: row.state_legacy,
        })
    }

    /// The heap-array arm of [`Self::lower_scoped_raw`] (moved out of it
    /// unchanged by nio21 wave 1 L3): RDX = a non-null base, R11 = the offset;
    /// leaves RDX = the element address, or deopts unless `base` is a
    /// one-dimensional primitive array whose payload holds `[offset - 16,
    /// offset - 16 + w)`. Writes RAX, RCX, R10.
    ///
    /// `screened` (nio21 wave 5, lane L4): an earlier heap arm of this block
    /// already proved `base` a one-dimensional primitive array
    /// (`Lowerer::scoped_screened_bases`), so the kind screen and its deopt are
    /// left out and only the element-size shift is read off the tag byte
    /// (`(tag >> 2) & 3`, the same value as the screen's `((tag - lo) >> 2) &
    /// 3` for the eight primitive tags: the kind sits in bits 0..1 and
    /// `Boolean` is 4). The bounds compares are emitted either way.
    pub(in crate::ir_lower) fn emit_scoped_heap_array_address(
        &mut self,
        w: i8,
        bci: usize,
        screened: bool,
    ) {
        // ---- heap array: kind == Array, primitive element type, in bounds.
        // `[R11 - 16]` below bakes `Unsafe.arrayBaseOffset` (published as a
        // literal 16, `native-io`'s `ARRAY_BYTE_BASE_OFFSET`) AS the payload
        // start, so `base + offset` is the element address only while the
        // array data begins at 16. A header shrink must revisit both.
        const _: () = assert!(
            cratonvm_types::ARRAY_DATA_OFFSET == 16,
            "lower_scoped_raw assumes Unsafe.arrayBaseOffset (16) == ARRAY_DATA_OFFSET"
        );
        const KIND_TAGS: u8 = cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8;
        // One compare for "a one-dimensional PRIMITIVE array" (nio20 wave 3):
        // an array header's tag byte is exactly `Array | elem << 2` (its bits
        // 6..7 are reserved zero, `MARK_RESERVED_MASK`), and the primitive
        // element types are the contiguous `Boolean ..= Long` (4..=11), so the
        // byte minus `SCOPED_PRIM_ARRAY_TAG_LO` is one of `0, 4, .., 28` for
        // exactly those eight headers. Any other kind leaves low bits set
        // (`kind - Array` is not a multiple of 4), a reference array (elem 0)
        // or a smaller tag borrows into the high bits, and a larger one has a
        // bit above `0x1C`. Was: AND/CMP/JNE on the kind, SHR/AND/CMP/JB on
        // the element type, SUB/AND for the size -- two deopt branches.
        if screened {
            // MOVZX ECX, BYTE [RDX + KIND_TAGS] ; SHR ECX, 2 ; AND ECX, 3
            self.buf.emit(&scoped_screened_shift_bytes());
        } else {
            self.buf.emit(&[0x0F, 0xB6, 0x42, KIND_TAGS]); // MOVZX EAX, BYTE [RDX + KIND_TAGS]
            self.buf.emit(&[0x83, 0xE8, SCOPED_PRIM_ARRAY_TAG_LO]); // SUB EAX, lo
            self.buf.emit_byte(0xA9); // TEST EAX, imm32
            self.buf.emit(&(!SCOPED_PRIM_ARRAY_TAG_SPAN).to_le_bytes());
            self.emit_deopt_unless(0x84, bci, DeoptReason::ReceiverTypeChanged); // JZ: primitive array
                                                                                 // `(elem - 4) & 3 == elem & 3` is log2 of the element size for each of
                                                                                 // the eight (`scoped_prim_array_tag_byte_screen_matches_the_layout`).
            self.buf.emit(&[0x89, 0xC1]); // MOV ECX, EAX
            self.buf.emit(&[0xC1, 0xE9, 0x02]); // SHR ECX, 2
            self.buf.emit(&[0x83, 0xE1, 0x03]); // AND ECX, 3  (log2 element size)
        }
        self.emit_scoped_heap_bounds(w, bci, true);
    }

    /// nio21 wave 7, lane L4 (W5L4-3): the heap arm for a base the graph
    /// knows is a `byte[]` ([`crate::ir::Graph::byte_array_nodes`]; the null
    /// case never reaches a heap arm). Its tag byte is `Array | Byte << 2`, so
    /// the screen [`Self::emit_scoped_heap_array_address`] runs would pass and
    /// its shift would be 0: neither is emitted, and the length is the payload
    /// size in bytes. Only the bounds compares stay. Writes RAX, R10.
    pub(in crate::ir_lower) fn emit_scoped_byte_array_address(&mut self, w: i8, bci: usize) {
        self.emit_scoped_heap_bounds(w, bci, false);
    }

    /// The bounds half of the scoped heap arm: RDX = a one-dimensional
    /// primitive array, R11 = the offset, and (`scaled`) CL = log2 of its
    /// element size, else a `byte[]`. Leaves RDX = the element address, or
    /// deopts unless the payload holds `[offset - 16, offset - 16 + w)`.
    pub(in crate::ir_lower) fn emit_scoped_heap_bounds(&mut self, w: i8, bci: usize, scaled: bool) {
        // `[R11 - 16]` is the payload start (`Unsafe.arrayBaseOffset`), as
        // [`Self::emit_scoped_heap_array_address`] asserts.
        const _: () = assert!(cratonvm_types::ARRAY_DATA_OFFSET == 16);
        // Compile-time checked: a layout constant past 127 would encode a
        // NEGATIVE disp8 and read before the array.
        const LEN_DISP8: i8 = crate::x64::disp::disp8_const(ARRAY_LENGTH_OFFSET as i64);
        self.buf.emit(&[0x8B, 0x42, LEN_DISP8 as u8]); // MOV EAX, [RDX + len]
        if scaled {
            self.buf.emit(&[0x48, 0xD3, 0xE0]); // SHL RAX, CL  (payload bytes)
        }
        self.buf.emit(&[0x4D, 0x8D, 0x53, 0xF0]); // LEA R10, [R11 - 16]  (start in payload)
        self.buf.emit(&[0x48, 0x83, 0xE8, w as u8]); // SUB RAX, width  (last valid start)
        self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
        self.emit_deopt_unless(0x89, bci, DeoptReason::ReceiverTypeChanged); // JNS: payload >= width
        self.buf.emit(&[0x49, 0x39, 0xC2]); // CMP R10, RAX
        self.emit_deopt_unless(0x86, bci, DeoptReason::ReceiverTypeChanged); // JBE (unsigned)
        self.buf.emit(&[0x4A, 0x8D, 0x14, 0x1A]); // LEA RDX, [RDX + R11]
    }

    /// nio21 wave 1 L3: resolve the `Unsafe` arena handle in R11 (bit 62 set,
    /// positive) through the JIT-readable arena table at `table`, for an access
    /// of `w` bytes: leaves RDX = the real address, or deopts
    /// (`ReceiverTypeChanged`, the fold's reason; the interpreter then runs the
    /// call and its locked slow path) for a legacy handle, a freed or resized
    /// block, or an access that does not fit the block. Writes RAX, RDX, R10;
    /// R11 is preserved. See `native-builtins` `unsafe_natives_ext.rs`, the
    /// section comment above `unsafe_arena::JIT_SLOT_SHIFT`, for why the two
    /// key compares make `len` / `ptr` the block's own.
    ///
    /// ```text
    ///   MOV R10, R11 ; SHR R10, 30 ; AND R10D, 0x7FFF ; SHL R10, 5
    ///   MOV RAX, table ; ADD R10, RAX                 ; R10 = &entry
    ///   MOV RAX, R11 ; SHR RAX, 30 ; SHL RAX, 30      ; RAX = the block's key
    ///   CMP RAX, [R10+key] ; deopt unless JE          ; (a legacy handle has bit 61 set: never a key)
    ///   MOV EDX, R11D ; AND EDX, 0x3FFF_FFFF          ; offset in the block
    ///   LEA RAX, [RDX + w] ; CMP RAX, [R10+len] ; deopt unless JBE
    ///   ADD RDX, [R10+ptr]                            ; the real address
    ///   MOV RAX, R11 ; SHR RAX, 30 ; SHL RAX, 30
    ///   CMP RAX, [R10+key] ; deopt unless JE          ; still that block
    /// ```
    pub(in crate::ir_lower) fn emit_scoped_arena_resolve(&mut self, table: u64, w: i8, bci: usize) {
        const _: () = assert!(SCOPED_ARENA_SLOT_SHIFT == 30 && SCOPED_ARENA_ENTRY_SHIFT == 5);
        const _: () = assert!(
            SCOPED_ARENA_KEY_OFFSET == 0
                && SCOPED_ARENA_LEN_OFFSET == 8
                && SCOPED_ARENA_PTR_OFFSET == 16
        );
        let reason = DeoptReason::ReceiverTypeChanged;
        let index_mask: u32 = (1u32 << SCOPED_ARENA_INDEX_BITS) - 1;
        let offset_mask: u32 = (1u32 << SCOPED_ARENA_SLOT_SHIFT) - 1;
        self.buf.emit(&[0x4D, 0x89, 0xDA]); // MOV R10, R11
        self.buf.emit(&[0x49, 0xC1, 0xEA, 0x1E]); // SHR R10, 30
        self.buf.emit(&[0x41, 0x81, 0xE2]); // AND R10D, imm32 (zero-extends)
        self.buf.emit(&index_mask.to_le_bytes());
        self.buf.emit(&[0x49, 0xC1, 0xE2, 0x05]); // SHL R10, 5
        self.emit_mov_reg_imm64(RAX, table);
        self.buf.emit(&[0x49, 0x01, 0xC2]); // ADD R10, RAX
        self.emit_scoped_arena_key_check(bci, reason);
        self.buf.emit(&[0x44, 0x89, 0xDA]); // MOV EDX, R11D (zero-extends)
        self.buf.emit(&[0x81, 0xE2]); // AND EDX, imm32
        self.buf.emit(&offset_mask.to_le_bytes());
        // Cast: `w` is the access width, 1..=8.
        self.buf.emit(&[0x48, 0x8D, 0x42, w as u8]); // LEA RAX, [RDX + w]
        self.buf.emit(&[0x49, 0x3B, 0x42, 0x08]); // CMP RAX, [R10 + 8]  (len)
        self.emit_deopt_unless(0x86, bci, reason); // JBE: offset + w <= len
        self.buf.emit(&[0x49, 0x03, 0x52, 0x10]); // ADD RDX, [R10 + 16]  (ptr)
        self.emit_scoped_arena_key_check(bci, reason);
    }

    /// `RAX = R11 & !(2^30 - 1) ; CMP RAX, [R10] ; deopt unless JE` -- one of
    /// the two key compares of [`Self::emit_scoped_arena_resolve`].
    pub(in crate::ir_lower) fn emit_scoped_arena_key_check(
        &mut self,
        bci: usize,
        reason: DeoptReason,
    ) {
        self.buf.emit(&[0x4C, 0x89, 0xD8]); // MOV RAX, R11
        self.buf.emit(&[0x48, 0xC1, 0xE8, 0x1E]); // SHR RAX, 30
        self.buf.emit(&[0x48, 0xC1, 0xE0, 0x1E]); // SHL RAX, 30
        self.buf.emit(&[0x49, 0x3B, 0x02]); // CMP RAX, [R10]  (key)
        self.emit_deopt_unless(0x84, bci, reason); // JE
    }

    /// The access of [`Self::lower_scoped_raw`] (moved out of it unchanged by
    /// nio21 wave 1 L3): RDX is the element address. `close_window` (JIT r20
    /// wave 19, lane yFfmShared): an access window is open (R10 = its word);
    /// close it right after the load or store, before the result is stored.
    #[allow(clippy::too_many_arguments)]
    pub(in crate::ir_lower) fn emit_scoped_access_at_rdx(
        &mut self,
        width: u8,
        signed: bool,
        put_value: Option<NodeId>,
        order_input: Option<NodeId>,
        order_const: Option<bool>,
        slot: i32,
        close_window: bool,
    ) {
        // ---- the access itself; RDX is the element address.
        if let Some(value) = put_value {
            self.gp_load_value(RAX, value);
            if let Some(order) = order_input {
                self.emit_scoped_byte_swap(width, order, order_const);
            }
            match width {
                1 => self.buf.emit(&[0x88, 0x02]),       // MOV [RDX], AL
                2 => self.buf.emit(&[0x66, 0x89, 0x02]), // MOV [RDX], AX
                4 => self.buf.emit(&[0x89, 0x02]),       // MOV [RDX], EAX
                _ => self.buf.emit(&[0x48, 0x89, 0x02]), // MOV [RDX], RAX
            }
            if close_window {
                self.emit_scoped_window_close();
            }
            return;
        }
        match width {
            1 => self.buf.emit(&[0x0F, 0xBE, 0x02]), // MOVSX EAX, BYTE [RDX]
            2 => self.buf.emit(&[0x0F, 0xB7, 0x02]), // MOVZX EAX, WORD [RDX]
            4 => self.buf.emit(&[0x8B, 0x02]),       // MOV EAX, [RDX]
            _ => self.buf.emit(&[0x48, 0x8B, 0x02]), // MOV RAX, [RDX]
        }
        if close_window {
            self.emit_scoped_window_close();
        }
        if let Some(order) = order_input {
            self.emit_scoped_byte_swap(width, order, order_const);
        }
        if width == 2 {
            if signed {
                self.buf.emit(&[0x0F, 0xBF, 0xC0]); // MOVSX EAX, AX
            } else {
                self.buf.emit(&[0x0F, 0xB7, 0xC0]); // MOVZX EAX, AX
            }
        }
        self.store_rax(slot);
    }

    /// The big-endian half of [`Self::lower_scoped_raw`]: byte-swap the low
    /// `width` bytes of RAX when `bigEndian` (the node input `order`) is true.
    /// `known` is that input's compile-time value, when it is a constant:
    /// `Some(false)` emits nothing, `Some(true)` the swap alone. Otherwise the
    /// flag is tested at run time in RCX (never live across a guard here).
    pub(in crate::ir_lower) fn emit_scoped_byte_swap(
        &mut self,
        width: u8,
        order: NodeId,
        known: Option<bool>,
    ) {
        if known == Some(false) || !matches!(width, 2 | 4 | 8) {
            return;
        }
        let native = if known.is_none() {
            self.gp_load_value(RCX, order);
            self.buf.emit(&[0x85, 0xC9]); // TEST ECX, ECX
            Some(self.emit_jcc_rel32(0x84)) // JZ -> little-endian
        } else {
            None
        };
        match width {
            2 => self.buf.emit(&[0x66, 0xC1, 0xC0, 0x08]), // ROL AX, 8
            4 => self.buf.emit(&[0x0F, 0xC8]),             // BSWAP EAX
            _ => self.buf.emit(&[0x48, 0x0F, 0xC8]),       // BSWAP RAX
        }
        if let Some(native) = native {
            self.patch_rel32_to_here(native);
        }
    }

    /// Emit a deopt-on-zero branch, given the caller has already emitted a
    /// `TEST` that sets `ZF=1` exactly when the deopt condition holds (the
    /// tested value was zero). Builds + boxes a `DeoptimizationPoint` for `bci`
    /// (frame state from its safepoint snapshot), then emits
    /// `JNZ continue; <mov DEOPT_ARG0, point; JMP deopt_stub>; continue:`.
    /// Shares the Phase-A deopt stub via `deopt_stub_patches`.
    ///
    /// NOTE: on Windows `DEOPT_ARG0` is RCX, which a div/rem site uses for the
    /// divisor — but the `mov` only executes on the deopt branch (after the
    /// `JNZ`), so the fall-through path keeps RCX intact for the `IDIV`.
    pub(in crate::ir_lower) fn emit_deopt_if_zero(&mut self, bci: usize, reason: DeoptReason) {
        // Continue (skip deopt) when the tested value is NON-zero — `JNZ` (the
        // near-Jcc second byte 0x85). Deopt when zero (ZF=1, JNZ not taken).
        self.emit_deopt_unless(0x85, bci, reason);
    }

    /// The `Op::Store` arm's receiver null check: `TEST RAX,RAX` and a
    /// `NullCheck` deopt at `bci`, with the receiver ALREADY in RAX (every route
    /// loads it there first, and the gated reference store reads it from RAX).
    ///
    /// Emits nothing for a receiver that cannot be null
    /// ([`store_receiver_cannot_be_null`], round 9 wave 4) -- RAX is still
    /// loaded by the caller either way. [`store_cannot_deopt`] predicts from
    /// the same predicate, and `ir_optimize` relies on that prediction to call
    /// the store's bci unconsultable, so the two must never disagree: this is
    /// the only place the arm decides.
    ///
    /// Round 16 (lane fieldguard, `CRATONVM_JIT_IR_STORE_NULL_CHECK_CSE`,
    /// default ON since round 16): a receiver the block has already proven non-null (the
    /// `this` seed, or a null test emitted earlier in this block by an inline
    /// `getfield` or by this check) is not re-tested, and a test emitted here
    /// becomes such a proof for the rest of the block -- the inline
    /// `getfield`'s `receiver_already_non_null` rule, applied to stores. Every
    /// `this.f = ...` of a method re-tested `this` before (`R16SyncSyncM`'s
    /// `plainStep`: four `TEST`/`JZ` deopt guards per call). Only ever removes
    /// a deopt [`store_cannot_deopt`] counted, never adds one, which is the
    /// direction the prediction tolerates: a store it calls deoptable that
    /// emits no deopt only keeps a snapshot consultable for nothing.
    pub(in crate::ir_lower) fn emit_store_receiver_null_check(&mut self, base: NodeId, bci: usize) {
        self.emit_store_receiver_null_check_for(base, bci, NO_NODE);
    }

    /// [`Self::emit_store_receiver_null_check`] for the `Op::Store` node
    /// `node` (`NO_NODE`: none to ask). nio21 wave 6 (lane L2, proposal
    /// L2W5-1): a `putfield` armed with implicit exception edges
    /// (`implicit_throw_plans`) sends a null receiver to the inline pad that
    /// asks this method's own handlers ([`Self::emit_implicit_throw_pad`]: a
    /// `NullPointerException` whose message `jit_npe_message` rebuilds from
    /// the `putfield` at the trap site) instead of deopting. The receiver is in RAX as before; the pad never falls
    /// through, so the fall-through still proves `base` non-null.
    pub(in crate::ir_lower) fn emit_store_receiver_null_check_for(
        &mut self,
        base: NodeId,
        bci: usize,
        node: NodeId,
    ) {
        self.emit_store_receiver_null_check_at(base, bci, node, RAX);
    }

    pub(in crate::ir_lower) fn emit_store_receiver_null_check_at(
        &mut self,
        base: NodeId,
        bci: usize,
        node: NodeId,
        receiver: u8,
    ) {
        if store_receiver_cannot_be_null(self.graph, base) {
            return;
        }
        let cse = true;
        if cse && self.receiver_already_non_null(base) {
            crate::metrics::note_ir_receiver_null_check_elided();
            return;
        }
        if receiver == RAX {
            self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
        } else {
            self.emit_field_receiver_test(receiver);
        }
        let plan = if node == NO_NODE {
            None
        } else {
            self.implicit_throw_plans.get(&node).cloned()
        };
        match plan.filter(|_| self.implicit_pad_frame_ok(bci)) {
            Some(plan) => {
                self.emit_implicit_throw_branch(0x84, ImplicitKind::Npe, bci, node, &plan);
            }
            None => self.emit_deopt_if_zero(bci, DeoptReason::NullCheck),
        }
        if cse {
            // The deopt leaves on null: the fall-through proves `base`.
            self.note_receiver_non_null(base);
        }
    }

    /// Leave for the interpreter unconditionally at `bci`.
    ///
    /// `XOR EAX, EAX` sets ZF, so the `JNZ` [`emit_deopt_if_zero`] emits is
    /// never taken and control always reaches the deopt stub. Callers must
    /// `return` immediately: nothing after this executes, and the node's result
    /// slot (if it has one) is never read because no consumer runs.
    ///
    /// Used where a lowering discovers it has no correct encoding to emit. The
    /// alternative — emitting SOMETHING and continuing — is how a wrong field
    /// slot becomes a punned heap cell that faults somewhere else entirely.
    pub(in crate::ir_lower) fn emit_unconditional_deopt(&mut self, bci: usize) {
        self.buf.emit(&[0x31, 0xC0]); // XOR EAX, EAX  (sets ZF)
                                      // Round 15 wave 4 (lane chain5; F5 of `r15w4-chain4-review-findings`):
                                      // a guard that never passes has no passing arrival to force, so no
                                      // `CRATONVM_DEOPT_EAGER_CHAINS` block: it was dead code whose cell the
                                      // coverage census would count `never-forced` forever.
        let forced_chain_deopts = std::mem::replace(&mut self.forced_chain_deopts, false);
        self.emit_deopt_if_zero(bci, DeoptReason::UnreachedCode);
        self.forced_chain_deopts = forced_chain_deopts;
    }

    /// Emit a guard that deopts at `bci` UNLESS the just-set flags satisfy
    /// `jcc_continue` (the near-`Jcc` second byte, e.g. `0x85`=JNZ, `0x82`=JB).
    /// The "continue" condition falls through to the following code; otherwise
    /// control jumps to the shared deopt stub with this point's pointer in
    /// `DEOPT_ARG0`. Generalises `emit_deopt_if_zero` so a bounds check can
    /// continue on `JB` (unsigned index < length) and deopt otherwise.
    pub(in crate::ir_lower) fn emit_deopt_unless(
        &mut self,
        jcc_continue: u8,
        bci: usize,
        reason: DeoptReason,
    ) {
        self.emit_deopt_unless_as(jcc_continue, bci, reason, false);
    }

    /// [`Self::emit_deopt_unless`], with `compact_chain_id`: JIT round 25
    /// (item 4 residual 1) -- the guard is the deopting compactness test
    /// ([`Self::emit_compact_receiver_trap`]), and when its point publishes
    /// an inlined chain it carries
    /// [`crate::deopt::chain_compact_speculation_id`] of the innermost scope
    /// at its own bci, so the VM's chain sinks charge it at the call site
    /// under that name (`deopt_resume::chain_named_charge_id`) and the next
    /// compile withdraws this one spliced site
    /// (`ir_optimize::proven_compact_withdrawn_pcs`) rather than the splice.
    /// A flat point keeps the derived id.
    pub(in crate::ir_lower) fn emit_deopt_unless_as(
        &mut self,
        jcc_continue: u8,
        bci: usize,
        reason: DeoptReason,
        compact_chain_id: bool,
    ) {
        // Round 13 wave 3 (lane framestate): a guard inside a spliced body
        // whose program point a chain snapshot describes publishes the callee's
        // own frame over its parked callers (`splice_chain_for_site`); the
        // point's `bci` is then the callee's, as its frame's must be.
        let raw_bci = bci;
        // A guard inside a spliced body resumes at the enclosing `invoke`, whose
        // snapshot is the caller's state before the call — see `resume_bci`.
        let bci = self.resume_bci(bci);
        // This bci CAN transfer. See `transfer_bcis`.
        self.transfer_bcis.insert(bci);
        let chain = self.splice_chain_for_site(raw_bci);
        let is_chain_point = chain.is_some();
        if !is_chain_point {
            self.refuse_flat_at_chain_only_pc(raw_bci);
        }
        let (point_bci, frame_state) = match chain {
            Some(chain) => (chain.bci, chain),
            None => (bci as u32, self.resolve_frame_state_for_site(bci)),
        };
        // JIT r20 wave 19 (LOOPOPT20-6): a transfer that read the flat frame
        // at `bci` -- not the node's own named one, which
        // `resolve_frame_state_for_site` prefers exactly when its bci is
        // `bci` -- is a by-bci transfer (`by_bci_transfer_bcis`).
        if is_chain_point || !self.cur_node_frame_is_at(bci) {
            self.note_flat_transfer(bci, "deopt_unless", false);
        }
        self.latch_if_site_frame_undefined(is_chain_point, raw_bci, bci);
        // JIT round 19 wave 3 (lane splice3, SPLICE2-2): a chain guard whose
        // innermost scope the planner listed as an expected throw
        // (`Graph::splice_expected_throw`, empty unless
        // `CRATONVM_JIT_IR_SPLICE_EXPECTED_THROW`) is marked, so the VM resumes
        // its trips without charging them. Same pad, same resume.
        let expected_throw = is_chain_point
            && !self.graph.splice_expected_throw.is_empty()
            && self.graph.splice_expected_throw.iter().any(|(key, b, r)| {
                *r == reason && *b == frame_state.bci && *key == frame_state.method_key
            });
        // Derived — same argument as the `Op::Guard` arm. `bci` here is
        // already the RESUME bci (`self.resume_bci(bci)` above) and is the
        // value published in the `bci` field, so the id this stamps is the
        // one a consumer re-derives from the point it recovered. A spliced
        // compactness guard's chain point is named by its scope instead
        // (see `emit_deopt_unless_as`).
        let speculation_id = if compact_chain_id
            && is_chain_point
            && reason == DeoptReason::SpeculationFailed
            && !frame_state.method_key.is_empty()
        {
            crate::deopt::chain_compact_speculation_id(&frame_state.method_key, frame_state.bci)
        } else {
            crate::deopt::speculation_id(bci as u32, reason)
        };
        let point = Box::new(DeoptimizationPoint {
            native_offset: self.buf.pos() as u32,
            bci: point_bci,
            reason,
            action: DeoptAction::Reinterpret,
            speculation_id,
            frame_state,
            // Null / bounds / div-by-zero guards all fire before the bytecode
            // they protect; `PendingException` (the one rethrow reason) never
            // reaches here. See `ResumeSemantics::for_reason`.
            semantics: ResumeSemantics::for_reason(reason).with_expected_throw(expected_throw),
        });
        let point_ptr = point.as_ref() as *const DeoptimizationPoint as u64;
        self.deopt_boxes.push(point);
        // Tier proposal 27 phase 3: an EXACT guard's failure makes the
        // re-executed bytecode throw (`osr_exit::guard_reason_is_exact`).
        if crate::osr_exit::guard_reason_is_exact(reason) {
            self.classified_guard_exits += 1;
        }
        // J<fail> to an out-of-line pad (`^ 1` inverts an x86 condition).
        self.emit_deopt_jcc_to_pad(jcc_continue ^ 1, point_ptr);
        // j27 wave 4 (lane DEOPTROWS, CH16-5): flat points too under
        // `CRATONVM_DEOPT_EAGER_GUARDS`.
        self.emit_forced_guard_deopt(point_ptr, is_chain_point, false, reason);
    }

    /// Round 13 wave 11 (lane chain5, proposal CH-1): under
    /// `CRATONVM_DEOPT_EAGER_CHAINS` ([`crate::ir::ir_forced_chain_deopts_enabled`]),
    /// on the PASSING path of a chain point's guard, take the point's pad on
    /// every 64th arrival anyway (since round 15 wave 4, lane chain5: and on
    /// its FIRST, `deopt::ForcedChainTripCell::for_new_point`). Emits nothing
    /// with the switch off.
    ///
    /// Emitted right after the guard's own `Jcc`, so the machine state is the
    /// one the point describes (nothing executes in between but this block),
    /// and it disturbs nothing on the path that continues: the flags are saved
    /// and restored around the count (a caller may still branch on the
    /// guard's compare), RAX is pushed and popped, and nothing else is
    /// written. The forced path pops the flags word before it jumps, so the
    /// pad sees the stack pointer the guard left.
    ///
    /// ```text
    ///   PUSHFQ ; PUSH RAX ; MOV RAX, &cell ; ADD BYTE [RAX], 4
    ///   JNZ keep                 ; ZF: the byte wrapped (every 64th arrival)
    ///   POP RAX ; POPFQ ; JMP forced_pad
    /// keep:
    ///   POP RAX ; POPFQ
    /// ...
    /// forced_pad:                ; emitted with the other pads
    ///   MOV DEOPT_ARG0, point | FORCED_CHAIN_PAD_TAG ; JMP deopt_stub
    /// ```
    ///
    /// The arrival count is a plain read-modify-write: two threads racing on
    /// it only shift when the next forced trip lands. `POP` changes no flag, so
    /// both paths restore exactly the flags and RAX the guard left.
    ///
    /// Round 16 lane chain (CH5W4-1 of `jit-r15w4-chain5-proposals-RETIRED-20261003.md`): the
    /// forced trip is told apart from a natural failure of the same point by
    /// the pad it takes, not by a cell count. The forced pad is this point's
    /// own second pad, identical to its natural one but for bit 0 of the point
    /// pointer ([`crate::deopt::FORCED_CHAIN_PAD_TAG`]), which
    /// `deopt::ir_deopt_entry` strips before it reads the point and records as
    /// the stash entry's forced mark. A compile-time property of the pad: the
    /// shared stub and every natural pad are unchanged. It replaced the
    /// round-15 `LOCK INC` in-flight count, which a sink could only claim
    /// through the trapping body (`r15w4-chain5-review-findings` G1, G2).
    pub(in crate::ir_lower) fn emit_forced_chain_deopt(&mut self, point_ptr: u64) {
        if !self.forced_chain_deopts {
            return;
        }
        // Round 15 wave 4 (lane chain5, CH4W4-3): its first arrival forced too
        // unless `CRATONVM_DEOPT_EAGER_CHAINS_FIRST=0` (the code is the same).
        // j27 wave 4 (lane DEOPTROWS, CH16-3): or the seeded phase of this
        // point's emission index under `CRATONVM_DEOPT_EAGER_CHAINS_SEED`.
        let cell = Box::new(crate::deopt::ForcedChainTripCell::for_new_point(
            self.forced_chain_deopt_counters.len(),
        ));
        let cell_addr = cell.as_ref() as *const crate::deopt::ForcedChainTripCell as u64;
        self.forced_chain_deopt_counters.push(cell);
        self.forced_chain_deopt_points.push(point_ptr);
        self.buf.emit(&[0x9C, 0x50]); // PUSHFQ ; PUSH RAX
        self.emit_mov_reg_imm64(RAX, cell_addr);
        self.buf.emit(&[0x80, 0x00, 0x04]); // ADD BYTE [RAX], 4 (arrivals)
                                            // JNZ keep: over POP RAX; POPFQ (2) + JMP rel32 (5).
        self.buf.emit(&[0x75, 0x07]);
        self.buf.emit(&[0x58, 0x9D]); // POP RAX ; POPFQ
        self.buf.emit_byte(0xE9); // JMP rel32 -> the point's forced pad
        let pad_patch = self.buf.pos();
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
        // Round 16 lane chain (CH5W4-1): the pad hands the stub the TAGGED
        // point (a boxed point is 8-aligned, so bit 0 is free).
        debug_assert_eq!(point_ptr & crate::deopt::FORCED_CHAIN_PAD_TAG, 0);
        self.deopt_pads
            .push((pad_patch, point_ptr | crate::deopt::FORCED_CHAIN_PAD_TAG));
        // OSR2-3 (lane osr5): the forced twin of a natural pad writes the
        // same cells back (not counted: it adds no point).
        if let Some(wbs) = &self.pad_writebacks {
            self.deopt_pad_writebacks.insert(pad_patch, wbs.clone());
        }
        self.buf.emit(&[0x58, 0x9D]); // keep: POP RAX ; POPFQ
    }

    /// [`Self::emit_deopt_unless`] for a guard whose failure continuation the
    /// caller vouches for: when `value_equivalent`, the interpreter re-executing
    /// the bytecode at `bci` computes exactly what the guarded fast path would
    /// have, so an OSR'd body may resume there and later be re-entered (tier
    /// proposal 27 phase 3, `osr_exit::guard_exit_fact`). Otherwise it is the
    /// plain call, which leaves a non-exact reason unclassified.
    pub(in crate::ir_lower) fn emit_value_equivalent_deopt_unless(
        &mut self,
        jcc_continue: u8,
        bci: usize,
        reason: DeoptReason,
        value_equivalent: bool,
    ) {
        self.emit_deopt_unless(jcc_continue, bci, reason);
        if value_equivalent && !crate::osr_exit::guard_reason_is_exact(reason) {
            self.classified_guard_exits += 1;
        }
    }

    /// Is the `Op::Guard` node `id`, anchored at RAW bci `bci`, one of the
    /// builder's own JVMS checks rather than a speculation — does its failure
    /// mean the bytecode at `bci` THROWS when the interpreter re-executes it?
    /// (Round 11 wave 13, lane irexc: item 3 of
    /// `r11w9-tier-optimizing-osr-exception-exits-door-contract-patch`.)
    ///
    /// `Op::Guard` carries no kind and lowers to `UncommonTrap` whatever it
    /// guards, so the shape is recognised instead: `cond` is
    /// `Cmp(Ne, x, Const 0)` (either operand order), `x` is the TOP of the
    /// operand stack in the snapshot the point resumes into (the same one
    /// [`Self::resolve_frame_state_for_site`] picks), and a live node at the
    /// same bci consumes `x` as
    ///
    /// * the divisor of an integer `Div` / `Rem` — `add_div_zero_guard`'s
    ///   guard. Only `idiv`/`ldiv`/`irem`/`lrem` build those, so the
    ///   re-executed bytecode divides by the zero on top of its stack and
    ///   throws `ArithmeticException`;
    /// * the object of a `MonitorEnter` — `enter_receiver_monitor`'s null
    ///   guard. The bci is the `invoke` whose receiver `x` is, and with `x` on
    ///   top of the stack the re-executed invoke's receiver is `x` too, so it
    ///   throws `NullPointerException` before anything runs. (With arguments
    ///   above the receiver the top is not `x` and the guard stays
    ///   unclassified.)
    ///
    /// A guard inside a spliced body resumes at the enclosing invoke and is
    /// never exact here. Everything else — pruned branches, range predicates,
    /// traps, the exact-class guard, the String-intrinsic guards — stays
    /// speculative, so the fact fails closed exactly as before. LICM never
    /// re-anchors an `Op::Guard` (`ir_optimize`'s LICM tests pin that), so a
    /// guard found at its division's bci is still inside the loop it guards.
    pub(in crate::ir_lower) fn op_guard_is_exact(&self, id: NodeId, bci: usize) -> bool {
        if self.pc_is_in_a_spliced_body(bci) {
            return false;
        }
        let nodes = &self.graph.nodes;
        let Some(cond) = nodes
            .get(id as usize)
            .and_then(|g| g.inputs.get(1))
            .copied()
        else {
            return false;
        };
        let Some(cn) = nodes.get(cond as usize) else {
            return false;
        };
        if cn.op != Op::Cmp(CmpOp::Ne) || cn.inputs.len() != 2 {
            return false;
        }
        let is_zero =
            |v: NodeId| matches!(nodes.get(v as usize).map(|n| &n.op), Some(Op::Const(0)));
        let (l, r) = (cn.inputs[0], cn.inputs[1]);
        let tested = if is_zero(r) && !is_zero(l) {
            l
        } else if is_zero(l) && !is_zero(r) {
            r
        } else {
            return false;
        };
        let site = self
            .cur_node_frame
            .and_then(|si| self.graph.safepoints.get(si as usize))
            .filter(|sp| sp.bci == bci)
            .or_else(|| {
                self.safepoint_first_by_bci
                    .get(&bci)
                    .and_then(|&i| self.graph.safepoints.get(i))
            });
        if site.and_then(|sp| sp.stack.last().copied()) != Some(tested) {
            return false;
        }
        nodes.iter().any(|n| {
            n.bytecode_pc == Some(bci)
                && match n.op {
                    Op::Div | Op::Rem => {
                        matches!(n.ty, IrType::Int | IrType::Long)
                            && n.inputs.get(1) == Some(&tested)
                    }
                    Op::MonitorEnter => n.inputs.get(2) == Some(&tested),
                    _ => false,
                }
        })
    }

    /// `J<jcc_fail> rel32` to an OUT-OF-LINE deopt pad for `point_ptr`,
    /// emitted after the body by [`Self::emit_deopt_pads`].
    ///
    /// Round 9. Every guard used to put its deopt path INLINE —
    /// `J<continue> +15; MOV DEOPT_ARG0, imm64; JMP stub` — so the FAST path
    /// (the one every execution but a failing one takes) was a TAKEN forward
    /// branch over 15 cold bytes, at every null check, bounds check, receiver
    /// guard and `Op::Guard`: two taken branches per unelided array access in
    /// a loop, and the cold bytes diluting the i-cache. The pad is the same
    /// two instructions, so the stub still receives the point in `DEOPT_ARG0`
    /// with every other register exactly as the guard left it (only the
    /// untaken-on-success `Jcc` lies between), and the point's `native_offset`
    /// still names the guard site. The same shape `emit_outlined_polls`
    /// already uses for the poll slow path.
    pub(in crate::ir_lower) fn emit_deopt_jcc_to_pad(&mut self, jcc_fail: u8, point_ptr: u64) {
        self.buf.emit(&[0x0F, jcc_fail]);
        let pad_patch = self.buf.pos();
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
        self.deopt_pads.push((pad_patch, point_ptr));
        // OSR2-3 (lane osr5): inside a promoted loop with kept stores.
        if let Some(wbs) = &self.pad_writebacks {
            self.deopt_pad_writebacks.insert(pad_patch, wbs.clone());
            self.pad_writebacks_attached += 1;
            self.covered_pad_points.insert(point_ptr);
        }
    }

    /// OSR2-3 (lane osr5): after every block is emitted, no deopt point a
    /// promoted loop's body published may lack its write-back -- whichever
    /// node or terminator emitted it (the per-node check in
    /// [`Self::end_promoted_pending`] sees only data nodes). A block's code
    /// is `[block_offsets[b], block_offsets[b + 1])`; the pads come after the
    /// last block. Refuses the compile otherwise.
    pub(in crate::ir_lower) fn check_promoted_pending_coverage(&self) {
        if self.graph.promoted_sunk_stores.is_empty() {
            return;
        }
        let Ok(Some(plan)) = self.promoted_pending_plan() else {
            return; // an `Err` plan has already refused the compile
        };
        let end = self.buf.pos();
        for &b in &plan.body_blocks {
            let Some(&lo) = self.block_offsets.get(b) else {
                continue;
            };
            let hi = self.block_offsets.get(b + 1).copied().unwrap_or(end);
            let uncovered = self.deopt_boxes.iter().any(|p| {
                let off = p.native_offset as usize;
                let ptr = p.as_ref() as *const DeoptimizationPoint as u64;
                off >= lo && off < hi && !self.covered_pad_points.contains(&ptr)
            });
            if uncovered {
                self.latch_bailout(Bailout::with_context(
                    BailoutReason::UnsupportedShape(
                        "promoted field cells: a deopt without a write-back",
                    ),
                    format!("block {b}: a point outside the per-node check"),
                ));
                return;
            }
        }
    }

    /// OSR5-4 (lane osr6): the XMM registers a write-back pad must save for
    /// the point `point_ptr` (a forced pad's tagged pointer included) -- every
    /// one its frame names, from the poll exits' boxes (`poll`) or the deopt
    /// boxes. Empty with `CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_PAD_XMM_SAVE`
    /// off (such a frame was refused). A point not found refuses the compile
    /// (fail closed: nothing proves its frame names no XMM register).
    pub(in crate::ir_lower) fn pad_point_xmms(&self, point_ptr: u64, poll: bool) -> Vec<u8> {
        if !ir_loop_field_promote_pad_xmm_save_enabled() {
            return Vec::new();
        }
        let want = point_ptr & !crate::deopt::FORCED_CHAIN_PAD_TAG;
        let boxes = if poll {
            &self.poll_exit_boxes
        } else {
            &self.deopt_boxes
        };
        match boxes
            .iter()
            .find(|p| p.as_ref() as *const DeoptimizationPoint as u64 == want)
        {
            Some(p) => frame_named_xmms(&p.frame_state),
            None => {
                self.latch_bailout(Bailout::new(BailoutReason::UnsupportedShape(
                    "promoted field cells: a write-back pad's point is not boxed",
                )));
                Vec::new()
            }
        }
    }

    /// Emit every pad [`Self::emit_deopt_jcc_to_pad`] deferred: patch its
    /// `Jcc` here, load the point, `JMP` to the shared deopt stub.
    ///
    /// Must run after the body and BEFORE `lower_inner` reads
    /// `deopt_stub_patches` for `osr_sentinel_free` (and before
    /// `emit_deopt_stub` consumes them): a pad's `JMP` is what makes the body
    /// not sentinel-free, exactly as the inline form's was.
    pub(in crate::ir_lower) fn emit_deopt_pads(&mut self) {
        for (pad_patch, point_ptr) in std::mem::take(&mut self.deopt_pads) {
            // Tolerated on an overflowed buffer; see `patch_rel32_to_here`.
            self.patch_rel32_to_here(pad_patch);
            // JIT rx27 wave 6 (lane regalloc): first, while every register the
            // guard's `TEST` read is still as the `Jcc` left it.
            if let Some(r) = self.deopt_pad_remats.remove(&pad_patch) {
                self.emit_pad_remat(r);
            }
            if let Some(extras) = self.extra_pad_remats.remove(&pad_patch) {
                for r in extras {
                    self.emit_pad_remat(r);
                }
            }
            // OSR2-3 (lane osr5): a promoted loop's cells reach memory before
            // the frame leaves, exactly as at a poll exit (OSR2-1): the
            // write-backs clobber only RAX, R11 and the helper call's
            // caller-saved set, and a frame naming an XMM register was
            // refused (`end_promoted_pending`; PROMOTE21-2: not when every
            // write-back is a static cell's, which calls nothing) -- unless (OSR5-4, lane osr6,
            // `CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_PAD_XMM_SAVE`) the pad saves
            // and restores every XMM register its point names around them.
            // The pad runs at the guard's own RSP, where every body helper
            // call runs.
            if let Some(wbs) = self.deopt_pad_writebacks.remove(&pad_patch) {
                let xmms = self.pad_point_xmms(point_ptr, false);
                emit_pad_xmm_save(&mut self.buf, &xmms);
                for wb in &wbs {
                    emit_cell_writeback(&mut self.buf, wb);
                }
                emit_pad_xmm_restore(&mut self.buf, &xmms);
            }
            self.emit_mov_reg_imm64(DEOPT_ARG0, point_ptr);
            self.buf.emit_byte(0xE9); // JMP rel32 → the shared deopt stub
            let jmp_patch = self.buf.pos();
            self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
            self.deopt_stub_patches.push(jmp_patch);
        }
    }

    /// Emit every reason-9 rethrow pad [`Self::push_rethrow_pad`] deferred
    /// (round 11 wave 10, lane irexc). Each pad, reached with the
    /// callee's pending exception still set:
    ///
    /// 1. copies a shadow publication its site had not reloaded yet back into
    ///    the frame homes (the relocated references the deopt entry is about
    ///    to read), then retracts this frame's shadow `top` exactly as the
    ///    call-exception stub's epilogue would — the shared deopt stub does
    ///    not, and the frame is leaving;
    /// 2. stamps the throw-site bci (`set_throw_bci`), as
    ///    [`Self::emit_call_exc_stub`] does, so "every exit leaves an
    ///    `athrow_bci` belonging to THIS method" holds on this exit too;
    /// 3. loads its point into `DEOPT_ARG0` and `JMP`s to the shared deopt
    ///    stub, whose `ir_deopt_entry` routes a `rethrow_exception` point to
    ///    `LAST_EXCEPTIONAL` and returns the sentinel with the exception still
    ///    pending.
    ///
    /// None of the three touches a register a frame value can name: the copy
    /// clobbers R10/R11/RCX, the restore R10/R11, the stamp call only
    /// caller-saved registers, and `deopt_nameable` names `IR_LOWER_LS_GPRS`
    /// alone, every one callee-saved. Must run BEFORE `emit_deopt_stub`, like
    /// [`Self::emit_deopt_pads`]: each pad's `JMP` is a `deopt_stub_patches`
    /// row.
    pub(in crate::ir_lower) fn emit_rethrow_pads(&mut self) {
        for pad in std::mem::take(&mut self.rethrow_pads) {
            // Tolerated on an overflowed buffer; see `patch_rel32_to_here`.
            self.patch_rel32_to_here(pad.patch);
            if let Some((thread_slot_off, savebase_slot_off, offsets)) = &pad.shadow {
                crate::runtime_lowering::emit_shadow_copy_back(
                    &mut self.buf,
                    &crate::runtime_lowering::ShadowCopyBack {
                        thread_slot_off: *thread_slot_off,
                        savebase_slot_off: *savebase_slot_off,
                        offsets,
                    },
                );
            }
            self.emit_shadow_savetop_restore();
            if self.set_throw_bci != 0 {
                self.emit_mov_reg_imm64(CALL_ARG_REGS[0], self.throw_stamp_word(pad.bci as u64));
                self.emit_mov_reg_imm64(RAX, self.set_throw_bci as u64);
                self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
            }
            self.emit_mov_reg_imm64(DEOPT_ARG0, pad.point_ptr);
            self.buf.emit_byte(0xE9); // JMP rel32 → the shared deopt stub
            let jmp_patch = self.buf.pos();
            self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
            self.deopt_stub_patches.push(jmp_patch);
        }
    }

    /// IR FP tier (Slice B) — emit the JVMS null + bounds deopt guards for an
    /// array element access, with the array pointer in RAX and the index in RCX
    /// (the layout the `MOVSS`/`MOVSD` SIB access below expects). On a null
    /// array or an out-of-bounds index, deopt at `bci`: the interpreter
    /// re-executes the array opcode and throws the exact NPE / AIOOBE with full
    /// semantics (including any in-method handler). Mirrors the single-pass
    /// inline checks, but routes the fault through the deopt path (which, with
    /// FP-slot resume — Slice C — reconstructs any live FP value precisely).
    /// The length is compared straight from memory (round 11; it used to be
    /// staged through R10). A non-faulting access continues with RAX/RCX
    /// unchanged.
    /// The array access guard pair, with each half skipped when
    /// [`crate::ir_check_elim`] has PROVEN it unnecessary at this node.
    ///
    /// `node` is the `ArrayLoad`/`ArrayStore` being emitted; passing `NO_NODE`
    /// asks for the unconditional pair, which is what every caller did before
    /// the analysis existed and what the kill switch restores.
    ///
    /// Both halves are counted, elided AND emitted, always. A count of
    /// elisions on its own says nothing: it cannot distinguish a working pass
    /// from a workload with no array accesses in this tier, and that
    /// distinction has produced wrong conclusions in this file before.
    pub(in crate::ir_lower) fn emit_array_null_bounds_guards_for(
        &mut self,
        bci: usize,
        node: NodeId,
    ) {
        self.emit_array_null_bounds_guards_at(bci, node, RAX, RCX);
    }

    pub(in crate::ir_lower) fn emit_array_null_bounds_guards_at(
        &mut self,
        bci: usize,
        node: NodeId,
        base: u8,
        index: u8,
    ) {
        use crate::x64::isel::{select, Mem, Op as MachineOp, Operand, Req, Ty};
        let receiver = Operand::Gpr(base);
        let test = select(&Req::new(MachineOp::Test, Ty::I64, receiver, receiver));
        let bounds = select(&Req::new(
            MachineOp::Cmp,
            Ty::I32,
            Operand::Gpr(index),
            Operand::Mem(Mem::base_disp(base, ARRAY_LENGTH_OFFSET as i64)),
        ));
        let (Ok(test), Ok(bounds)) = (test, bounds) else {
            self.latch_bailout(Bailout::new(BailoutReason::Internal(
                "allocated array guard has no encoding",
            )));
            return;
        };
        // The compare reads 32 bits, but the SIB reads all 64 bits.
        // An allocated index already carries the required zero extension.
        if index == RCX {
            self.normalize_rcx_index();
        }
        // nio21 wave 3 (lane L5, `CRATONVM_JIT_IR_MASK_GUARD_ELIDE`, default
        // ON since nio21 wave 7): `a[p & m]` under a pre-header
        // `Guard(c < a.length)`, `c >= m`, proven by graph shape alone
        // (`ir_optimize::mask_guard_proves_access`) -- the predicate
        // `node_cannot_deopt` shares. Read through the per-compile cache
        // (`mask_guard_proven`), which `node_cannot_deopt_here` reads too.
        let mask_guarded = node != NO_NODE
            && self
                .graph
                .nodes
                .get(node as usize)
                .is_some_and(|n| self.mask_guard_proven(n));
        let elide_null = node != NO_NODE && (self.check_elision.null_elided(node) || mask_guarded);
        let elide_bounds =
            node != NO_NODE && (self.check_elision.bounds_elided(node) || mask_guarded);
        crate::ir_check_elim::note_check(0, elide_null);
        crate::ir_check_elim::note_check(1, elide_bounds);
        if elide_bounds && self.check_elision.bounds_range_proved(node) {
            crate::ir_check_elim::note_range_proved();
            // JIT round 19 wave 5 (lane osr5, IROPS3-2).
            if self.check_elision.bounds_clamp_proved(node) {
                crate::ir_check_elim::note_clamp_proved();
            }
        }
        if !elide_bounds && node != NO_NODE {
            crate::ir_check_elim::note_range_refusal(self.check_elision.bounds_refusal(node));
        }
        // nio21 wave 5 (lane L2): an access armed with implicit exception
        // edges sends each failing check to an inline pad that asks this
        // method's own handlers (`emit_implicit_throw_pad`) instead of
        // deopting. `TEST; JZ npe ; CMP; JAE aioobe ; JMP over ; pads ; over:`
        // -- the passing path pays the one `JMP`.
        let implicit = if node == NO_NODE {
            None
        } else {
            self.implicit_throw_plans.get(&node).cloned()
        };
        if let Some(plan) = implicit.filter(|_| self.implicit_pad_frame_ok(bci)) {
            // nio21 wave 6 (L2, proposal L2W5-3): with the fall-through layout
            // the LAST check jumps over on success and falls into its own pad,
            // so the passing path is the earlier untaken `JZ` plus one taken
            // `JB` -- no `JMP`: `TEST; JZ npe ; CMP; JB over ; aioobe pad ;
            // npe pad ; over:`.
            let fallthrough = true;
            let mut pads: Vec<(usize, ImplicitKind)> = Vec::with_capacity(2);
            let mut last: Option<(usize, ImplicitKind)> = None;
            if !elide_null {
                self.buf.emit(&test.encoded.bytes);
                if fallthrough && elide_bounds {
                    last = Some((self.emit_jcc_rel32(0x85), ImplicitKind::Npe));
                // JNZ over
                } else {
                    pads.push((self.emit_jcc_rel32(0x84), ImplicitKind::Npe)); // JZ
                }
            }
            if !elide_bounds {
                // CMP ECX, [base + length]: the bounds check's own compare,
                // through the shared build-time checked disp8.
                self.buf.emit(&bounds.encoded.bytes);
                if fallthrough {
                    last = Some((self.emit_jcc_rel32(0x82), ImplicitKind::Aioobe));
                // JB over
                } else {
                    pads.push((self.emit_jcc_rel32(0x83), ImplicitKind::Aioobe));
                    // JAE
                }
            }
            if let Some((over, kind)) = last {
                // Entered by falling in: the failing condition of `last`.
                if kind == ImplicitKind::Aioobe && base != RAX {
                    self.emit_mov_reg_reg64(RAX, base);
                }
                self.emit_implicit_throw_pad(kind, bci, node, &plan);
                for (patch, kind) in pads {
                    self.patch_rel32_to_here(patch);
                    if kind == ImplicitKind::Aioobe && base != RAX {
                        self.emit_mov_reg_reg64(RAX, base);
                    }
                    self.emit_implicit_throw_pad(kind, bci, node, &plan);
                }
                self.patch_rel32_to_here(over);
            } else if !pads.is_empty() {
                let over = self.emit_jmp_rel32();
                for (patch, kind) in pads {
                    self.patch_rel32_to_here(patch);
                    if kind == ImplicitKind::Aioobe && base != RAX {
                        self.emit_mov_reg_reg64(RAX, base);
                    }
                    self.emit_implicit_throw_pad(kind, bci, node, &plan);
                }
                self.patch_rel32_to_here(over);
            }
            return;
        }
        if !elide_null {
            // Null check: TEST base,base → ZF=1 iff array == null. Continue on JNZ.
            self.buf.emit(&test.encoded.bytes);
            self.emit_deopt_if_zero(bci, DeoptReason::NullCheck);
        }
        if !elide_bounds {
            // Bounds check: CMP ECX, [base + ARRAY_LENGTH_OFFSET]. An UNSIGNED
            // `index < length` (JB, CF=1) continues; otherwise (index >=
            // length, OR a negative index whose unsigned value is huge) deopt →
            // AIOOBE.
            //
            // Round 11 (irlower): one memory-operand compare. This was `MOV
            // R10D, [RAX+len]; CMP ECX, R10D` -- the same flags from the same
            // two operands, one instruction and one scratch register more, at
            // every unelided array access. Nothing read R10 afterwards.
            self.buf.emit(&bounds.encoded.bytes);
            self.emit_deopt_unless(0x82, bci, DeoptReason::BoundsCheck); // JB continue
        }
    }

    pub(in crate::ir_lower) fn bind_checked_array_load_address(
        &mut self,
        id: NodeId,
        node: &Node,
        bci: usize,
    ) -> (u8, u8) {
        let array = node.inputs[2];
        let resident = if self.reg_alloc_on && !self.carry_names(array) {
            self.resident_gpr(array)
        } else {
            None
        };
        let base = if let Some(reg) = resident {
            self.gp_reg_reads += 1;
            reg
        } else {
            self.gp_load_value(RAX, array);
            RAX
        };
        // The implicit bounds pad marshals its failing index from RCX.
        let allow_resident = !self.implicit_throw_plans.contains_key(&id);
        let index = self.load_array_index(node.inputs[3], allow_resident);
        self.emit_array_null_bounds_guards_at(bci, id, base, index);
        (base, index)
    }

    /// The unconditional pair, for callers that have no node to ask about.
    pub(in crate::ir_lower) fn emit_array_null_bounds_guards(&mut self, bci: usize) {
        self.emit_array_null_bounds_guards_for(bci, NO_NODE);
    }

    /// zt t19/zb2 (zr1br A2b read side, JIT routing): the `ArrayLoad(Ref)`
    /// node `id` under an ARMED ZGC load barrier, as a call of `jit_aaload`.
    ///
    /// ```text
    ///   RAX = array, RCX = index (zero-extended); the null + bounds guards
    ///   ARG2 = RCX, ARG1 = RAX, ARG0 = VM context
    ///   MOV RAX, jit_aaload ; CALL RAX      ; element through the barrier seam
    ///   <helper sentinel check>             ; RAX = element (address or 0)
    ///   store RAX to the node's home
    /// ```
    ///
    /// `jit_aaload` reads the element through `VmHeap::load_ref_slot_barriered`
    /// (`jit_load_ref_slot`), so the read is routed -- the raw
    /// `MOV RAX, [RAX + RCX*8 + d]` of [`Self::emit_gpr_array_elem_load`] is
    /// the unrouted read row 30 of the t18/zb1 inventory names. The guards
    /// stay inline, exactly as every other element width emits them, so the
    /// JVMS exceptions keep this node's deopt semantics; the helper's own
    /// checks can then only fire on an implausible array word, through the
    /// `i64::MIN` sentinel the `jit_getfield` arm already handles the same way.
    ///
    /// Not a GC point (`jit_aaload` allocates nothing), so no oop map, like the
    /// `Op::Load` helper arm. It IS a call: `regalloc::ir_op_is_call` names an
    /// armed `ArrayLoad(Ref)` so no caller-saved register is assumed to survive
    /// it, and `scan_frame_needs` reserves the context slot. Both read the same
    /// process flag as this arm, which production sets once at heap
    /// construction, before the first compile.
    ///
    /// A pc-less node (an unrolled copy proven by its mask guard) is refused:
    /// the sentinel exit needs a bytecode pc to describe. Refused too with no
    /// `aaload` helper wired (partial helper tables): never the raw load.
    pub(in crate::ir_lower) fn lower_ref_array_load_through_barrier_helper(
        &mut self,
        id: NodeId,
        node: &Node,
        slot: i32,
        bci: usize,
    ) {
        if self.aaload == 0 || node.bytecode_pc.is_none() {
            self.latch_bailout(Bailout::with_context(
                BailoutReason::UnsupportedShape(
                    "ir_lower: ArrayLoad(Ref) under an armed ZGC load barrier needs the aaload helper and a pc",
                ),
                format!(
                    "n{id}; helper={} pc={:?}",
                    self.aaload, node.bytecode_pc
                ),
            ));
            return;
        }
        // zt t22/zb5 (stage 2e): under the color flip the element is loaded
        // inline and only a BAD word takes the call below.
        if let (Some(base), Some(cell)) = (
            crate::x64::zgc_colored_inline_load_barrier_base(),
            crate::x64::zgc_colored_flip_bad_mask_cell(),
        ) {
            self.lower_ref_array_load_colored_flip(id, node, slot, bci, base, cell);
            return;
        }
        self.gp_load_value(RAX, node.inputs[2]); // array -> RAX
        self.gp_load_index_rcx(node.inputs[3]); // index -> RCX, zero-extended
        self.emit_array_null_bounds_guards_for(bci, id);
        self.emit_aaload_helper_call_rax_rcx();
        self.store_rax(slot);
    }

    /// The `jit_aaload` call with the array in RAX and the (zero-extended)
    /// index in RCX, then the helper's sentinel check: RAX = the element. The
    /// zt t19/zb2 helper arm's call, shared with the color flip's slow branch
    /// (zt t22/zb5).
    pub(in crate::ir_lower) fn emit_aaload_helper_call_rax_rcx(&mut self) {
        // RCX first (it is ARG0 on Win64), then RAX, then the context: no move
        // overwrites a later one's source on either ABI.
        self.emit_mov_reg_reg64(CALL_ARG_REGS[2], RCX);
        self.emit_mov_reg_reg64(CALL_ARG_REGS[1], RAX);
        self.load_context(CALL_ARG_REGS[0]);
        self.emit_mov_reg_imm64(RAX, self.aaload as u64);
        // CALL RAX; `i64::MIN` is never a reference, so the plain
        // compare-and-bail follows it.
        self.buf.emit(&[0xFF, 0xD0]);
        self.emit_helper_sentinel_check(IrType::Ref);
    }

    /// zt t22/zb5 (stage 2e): the `ArrayLoad(Ref)` node `id` under the ZGC
    /// COLOR FLIP (`x64::zgc_colored_flip_bad_mask_cell`), the optimizing
    /// tier's twin of the single-pass `walk_aaload_colored_flip`:
    ///
    /// ```text
    ///   RAX = array, RCX = index (zero-extended); the null + bounds guards
    ///   MOV RAX, [checked array base + checked index*8 + d]
    ///   <x64::zgc_colored_flip_head_bytes>  ; plain / null -> .done,
    ///                                       ; good colored -> decode, JMP .done
    /// .slow:
    ///   RAX = array, RCX = index again; the helper arm's jit_aaload call
    /// .done:
    ///   store RAX to the node's home
    /// ```
    ///
    /// The node is a call for the allocator (`regalloc::ir_op_is_call` asks
    /// `zgc_ref_element_load_calls_helper`, `true` under the flip) and the
    /// scan reserves the context slot, so the slow branch may clobber every
    /// caller-saved register and the fast path's fewer clobbers are a subset.
    /// The guards ran on the fast path's operands, so the slow branch's
    /// reload of the same two values needs no second check. Preconditions
    /// (helper wired, a bytecode pc) are the helper arm's, checked by the
    /// caller.
    pub(in crate::ir_lower) fn lower_ref_array_load_colored_flip(
        &mut self,
        id: NodeId,
        node: &Node,
        slot: i32,
        bci: usize,
        heap_base: u64,
        cell: u64,
    ) {
        let (array_base, index) = self.bind_checked_array_load_address(id, node, bci);
        self.emit_ref_array_elem_load_at(array_base, index, true);
        let (plain_to_done, good_to_done) =
            crate::x64::emit_zgc_colored_flip_head(&mut self.buf, cell, heap_base);
        // .slow
        self.gp_load_value(RAX, node.inputs[2]); // array -> RAX
        self.gp_load_index_rcx(node.inputs[3]); // index -> RCX, zero-extended
        self.emit_aaload_helper_call_rax_rcx();
        // .done
        self.patch_rel32_to_here(plain_to_done);
        self.patch_rel32_to_here(good_to_done);
        if crate::x64::dbg_jitc_enabled() {
            eprintln!("[cratonvm-jitc] ir zgc-color-flip-aaload");
        }
        self.store_rax(slot);
    }

    /// Load a primitive element from the allocated base and checked index.
    /// References and FP values keep their existing lowering paths.
    pub(in crate::ir_lower) fn emit_gpr_primitive_array_elem_load_to(
        &mut self,
        kind: MemKind,
        dst: u8,
        base: u8,
        index: u8,
    ) {
        use crate::x64::isel::{select, Mem, Op as MachineOp, Operand, Req, Ty};
        let (op, ty, scale) = match kind {
            MemKind::Int => (MachineOp::Movsxd, Ty::I32, 4),
            MemKind::Long => (MachineOp::Mov, Ty::I64, 8),
            MemKind::Byte => (MachineOp::Movsx, Ty::I8, 1),
            MemKind::Char => (MachineOp::Movzx, Ty::I16, 2),
            MemKind::Short => (MachineOp::Movsx, Ty::I16, 2),
            _ => {
                self.latch_bailout(Bailout::new(BailoutReason::Internal(
                    "only primitive integer array loads use this path",
                )));
                return;
            }
        };
        match select(&Req::new(
            op,
            ty,
            Operand::Gpr(dst),
            Operand::Mem(Mem::base_index(
                base,
                index,
                scale,
                i64::from(ARRAY_DATA_DISP8),
            )),
        )) {
            Ok(load) => self.buf.emit(&load.encoded.bytes),
            Err(_) => self.latch_bailout(Bailout::new(BailoutReason::Internal(
                "allocated array load has no encoding",
            ))),
        }
    }

    /// Plain, full-width reference element; its caller excludes narrow oops
    /// and every collector configuration requiring a load barrier.
    pub(in crate::ir_lower) fn emit_plain_ref_array_elem_load_to(
        &mut self,
        dst: u8,
        base: u8,
        index: u8,
    ) {
        use crate::x64::isel::{Mem, Op as MachineOp, Operand, Req, Ty};
        self.emit_field_instruction(Req::new(
            MachineOp::Mov,
            Ty::I64,
            Operand::Gpr(dst),
            Operand::Mem(Mem::base_index(base, index, 8, i64::from(ARRAY_DATA_DISP8))),
        ));
    }

    /// Decode a String byte or UTF-16 char with the normalized index.
    pub(in crate::ir_lower) fn emit_str_char_load_to(
        &mut self,
        kind: MemKind,
        dst: u8,
        base: u8,
        index: u8,
    ) {
        use crate::x64::isel::{select, Mem, Op as MachineOp, Operand, Req, Ty};
        let (ty, scale) = match kind {
            MemKind::Byte => (Ty::I8, 1),
            MemKind::Char => (Ty::I16, 2),
            _ => {
                self.latch_bailout(Bailout::new(BailoutReason::Internal(
                    "string elements are a byte or a char",
                )));
                return;
            }
        };
        let address = Mem::base_index(base, index, scale, i64::from(ARRAY_DATA_DISP8));
        match select(&Req::new(
            MachineOp::Movzx,
            ty,
            Operand::Gpr(dst),
            Operand::Mem(address),
        )) {
            Ok(instruction) => self.buf.emit(&instruction.encoded.bytes),
            Err(_) => self.latch_bailout(Bailout::new(BailoutReason::Internal(
                "allocated String char address has no encoding",
            ))),
        }
    }

    /// Load an element using the checked RAX/RCX address.
    pub(in crate::ir_lower) fn emit_gpr_array_elem_load(&mut self, kind: MemKind) {
        let d = ARRAY_DATA_DISP8;
        match kind {
            // MOVSXD RAX, DWORD [RAX + RCX*4 + HEADER_SIZE]
            MemKind::Int => self.buf.emit(&[0x48, 0x63, 0x44, 0x88, d]),
            // MOV RAX, QWORD [RAX + RCX*8 + HEADER_SIZE]
            MemKind::Long => self.buf.emit(&[0x48, 0x8B, 0x44, 0xC8, d]),
            // MOVSX RAX, BYTE [RAX + RCX*1 + HEADER_SIZE]. Round 11 wave 5:
            // the REX.W form, one instruction for the value the old
            // `MOVSX EAX ; MOVSXD RAX, EAX` pair produced -- the single-pass
            // `emit_byte_aload_regs` took the same rewrite in round 9.
            MemKind::Byte => self.buf.emit(&[0x48, 0x0F, 0xBE, 0x44, 0x08, d]),
            // MOVZX EAX, WORD [RAX + RCX*2 + HEADER_SIZE] (already zero-extends
            // through the full RAX — a `char` is unsigned, so no MOVSXD).
            MemKind::Char => self.buf.emit(&[0x0F, 0xB7, 0x44, 0x48, d]),
            // MOVSX RAX, WORD [RAX + RCX*2 + HEADER_SIZE] (REX.W, as `Byte`).
            MemKind::Short => self.buf.emit(&[0x48, 0x0F, 0xBF, 0x44, 0x48, d]),
            MemKind::Ref => self.emit_ref_array_elem_load_at(RAX, RCX, false),
            // Structurally unreachable — the caller routes FP to the XMM path.
            // A `debug_assert!` here would be a FAIL-OPEN: it vanishes in
            // release, this function would emit nothing, and the caller's
            // `store_rax(slot)` would still run and spill whatever RAX happens
            // to hold (the array pointer) as the element's value. Latch the
            // bailout so release refuses the compile instead.
            MemKind::Float | MemKind::Double => {
                self.latch_bailout(Bailout::with_context(
                    BailoutReason::Internal("ir_lower: FP element load reached the GPR emitter"),
                    format!("{kind:?}"),
                ));
            }
        }
    }

    pub(in crate::ir_lower) fn emit_fp_array_elem_access(
        &mut self,
        is_d: bool,
        is_store: bool,
        base: u8,
        index: u8,
    ) {
        let prefix = if is_d { 0xF2 } else { 0xF3 };
        self.buf.emit_byte(prefix);
        let rex_x = if index >= 8 { 2 } else { 0 };
        let rex_b = if base >= 8 { 1 } else { 0 };
        if rex_x != 0 || rex_b != 0 {
            self.buf.emit_byte(0x40 | rex_x | rex_b);
        }
        let opcode = if is_store { 0x11 } else { 0x10 };
        self.buf.emit(&[0x0F, opcode, 0x44]);
        let scale = if is_d { 3 } else { 2 };
        let sib = (scale << 6) | ((index & 7) << 3) | (base & 7);
        self.buf.emit(&[sib, ARRAY_DATA_DISP8]);
    }

    /// Load a reference word through the checked address and decode it in RAX.
    /// The color-flip caller appends its own decode and slow branch.
    pub(in crate::ir_lower) fn emit_ref_array_elem_load_at(
        &mut self,
        base: u8,
        index: u8,
        flip_head_follows: bool,
    ) {
        use crate::x64::isel::{Mem, Op as MachineOp, Operand, Req, Ty};
        let raw_word = |ty, scale| {
            Req::new(
                MachineOp::Mov,
                ty,
                Operand::Gpr(RAX),
                Operand::Mem(Mem::base_index(
                    base,
                    index,
                    scale,
                    i64::from(ARRAY_DATA_DISP8),
                )),
            )
        };
        // zt t19/zb2: an armed ZGC load barrier forbids the raw
        // element load. The `ArrayLoad(Ref)` arm routes such a node to
        // `jit_aaload` before reaching here and the OSR entry check
        // refuses one (`osr_check_compile`); any other caller fails
        // CLOSED rather than emit the unrouted read.
        //
        // zt t21/zb4 (stage 2d): except under colored stores, where the
        // load is followed by the inline colored-slot decode (see
        // `x64::zgc_colored_inline_load_barrier_base` for why no slow
        // path is owed while the good color is pinned at `Remapped`).
        // `ArrayLoad(Ref)` reaches here then instead of the helper arm.
        if crate::x64::zgc_read_barrier_blocks_inline_fields() {
            if let Some(heap_base) = crate::x64::zgc_colored_inline_load_barrier_base() {
                self.emit_field_instruction(raw_word(Ty::I64, 8));
                // zt t22/zb5 (stage 2e): under the color flip the
                // pinned decode below would pass a BAD word; the flip
                // lowering appends its own head (bad-mask test and
                // slow branch). Any other caller under the flip fails
                // closed (none is reached: `ArrayLoad(Ref)` routes to
                // the flip lowering, the OSR check refuses).
                if flip_head_follows {
                    return;
                }
                if crate::x64::zgc_colored_flip_bad_mask_cell().is_some() {
                    self.latch_bailout(Bailout::new(BailoutReason::UnsupportedShape(
                        "ir_lower: reference element load without the ZGC color-flip head",
                    )));
                    return;
                }
                crate::x64::emit_zgc_colored_slot_decode(&mut self.buf, heap_base);
                if crate::x64::dbg_jitc_enabled() {
                    eprintln!("[cratonvm-jitc] ir zgc-colored-inline-aaload");
                }
                return;
            }
            self.latch_bailout(Bailout::new(BailoutReason::UnsupportedShape(
                "ir_lower: raw reference element load under an armed ZGC load barrier",
            )));
            return;
        }
        if narrow_oops_enabled() {
            // 4-byte `(addr - base) >> 3`, 0 == null. Decode to a full
            // pointer so every consumer downstream is unchanged, and
            // keep null at 0 rather than rebasing it to `base` — `SHL`
            // sets ZF from its result, so the null test is free.
            self.emit_field_instruction(raw_word(Ty::I32, 4));
            self.buf.emit(&[0x48, 0xC1, 0xE0, 0x03]); // SHL RAX, 3
            self.buf.emit(&[0x74, 0x0D]); // JZ +13 (null stays 0)
            self.buf.emit(&[0x49, 0xBB]); // MOV R11, imm64
            self.buf.emit(&narrow_base().to_le_bytes());
            self.buf.emit(&[0x4C, 0x01, 0xD8]); // ADD RAX, R11
        } else {
            self.emit_field_instruction(raw_word(Ty::I64, 8));
        }
    }

    /// Emit the GPU input-residency barrier after an inline primitive
    /// array store, if a `--gpu` run armed it.
    ///
    /// Assumes RAX holds the array pointer, which both `ArrayStore` arms
    /// guarantee. Clobbers R10, R11 and the flags: R10 is already this
    /// backend's guard scratch, R11 is never register-resident, and this
    /// backend's linear-scan residency uses only callee-saved GPRs
    /// (`IR_LOWER_LS_GPRS`), so nothing live is at risk.
    ///
    /// Emits nothing at all unless armed -- see [`crate::gpu_barrier`],
    /// which is also where the reason a compiled store marks a bucket
    /// instead of calling `input_cache::invalidate` is written down.
    pub(in crate::ir_lower) fn emit_gpu_input_cache_barrier(&mut self) {
        if let Some(bytes) = crate::gpu_barrier::barrier_bytes() {
            self.buf.emit(&bytes);
        }
    }

    /// The CALL-free half of an `ldc <String>` / `ldc <Class>` site (round 9
    /// wave 8, irl8; request G of
    /// `perf-compiled-ldc-string-is-a-helper-call-per-execution`). With a
    /// slot, emits
    ///
    /// ```text
    /// mov  rax, imm64 slot
    /// mov  rax, [rax]
    /// test rax, rax
    /// jnz  resolved          ; patched by `patch_ldc_slot_hit`
    /// ```
    ///
    /// and answers the `jnz`'s rel32 position; `0` emits nothing and answers
    /// `None`. It clobbers RAX and the flags only, and runs before the
    /// arm's map, so the helper arm below it is byte-for-byte today's code.
    /// The word is `vm::jit::helpers::compiled_ldc_slot_for`'s: a JNI global
    /// root, rewritten in place only while mutators are parked, so the value
    /// read is current; `0` means "not recorded yet" and takes the helper,
    /// which is also what fills it. The single-pass twin is
    /// `x64/objects.rs::emit_ldc_cp_site`.
    pub(in crate::ir_lower) fn emit_ldc_slot_probe(&mut self, slot_addr: usize) -> Option<usize> {
        if slot_addr == 0 {
            return None;
        }
        self.emit_mov_reg_imm64(RAX, slot_addr as u64);
        self.buf.emit(&[0x48, 0x8B, 0x00]); // MOV RAX, [RAX]
        self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
        self.buf.emit(&[0x0F, 0x85]); // JNZ resolved
        let patch = self.buf.pos();
        self.buf.emit(&[0; 4]);
        Some(patch)
    }

    /// Land an [`Self::emit_ldc_slot_probe`] hit here: the position where
    /// both arms hold the constant in RAX, just before its home store.
    pub(in crate::ir_lower) fn patch_ldc_slot_hit(&mut self, hit: Option<usize>) {
        if let Some(patch) = hit {
            let here = self.buf.pos();
            let rel = here as i32 - (patch as i32 + 4);
            Self::patch_or_bail(&mut self.buf, patch, rel);
        }
    }

    /// `bastore` needs no `boolean[]` mask for this value: its low byte
    /// already equals `value & 1`, so the `byte[]` truncation and the
    /// `boolean[]` narrowing store the same byte (round 9 wave 8, irl8). True
    /// for a constant with bits 1..8 clear (javac's `iconst_0`/`iconst_1`)
    /// and for a compare's 0/1 result; everything else is asked at run time.
    pub(in crate::ir_lower) fn int_value_is_boolean_bits(&self, id: NodeId) -> bool {
        match self.graph.nodes.get(id as usize).map(|n| &n.op) {
            Some(Op::Const(c)) => (*c & 0xFE) == 0,
            Some(Op::Cmp(_)) => true,
            _ => false,
        }
    }

    /// COV-02 — the integral element **store**: array in RAX, index in RCX,
    /// value in RDX or a selected int operand. The single-pass twins are `emit_{int,long,byte,short}_
    /// astore_regs`; `castore` and `sastore` share one 16-bit store, exactly as
    /// they do there.
    ///
    /// `MemKind::Ref` never reaches here (`aastore` owes barriers; its gated
    /// inline form, `emit_gated_ir_aastore`, asks for the `Long` encoding,
    /// which is the same 8-byte element store) and the FP kinds take the XMM
    /// path. One shared header displacement, for the reason given on
    /// [`Self::emit_gpr_array_elem_load`].
    pub(in crate::ir_lower) fn emit_gpr_array_elem_store(
        &mut self,
        kind: MemKind,
        operand: ArrayStoreSource,
    ) {
        self.emit_gpr_array_elem_store_at(kind, RAX, RCX, operand);
    }

    pub(in crate::ir_lower) fn emit_gpr_array_elem_store_at(
        &mut self,
        kind: MemKind,
        base: u8,
        index: u8,
        operand: ArrayStoreSource,
    ) {
        if base != RAX
            || index != RCX
            || !matches!(
                (kind, operand),
                (MemKind::Int, _)
                    | (
                        MemKind::Long | MemKind::Byte | MemKind::Char | MemKind::Short,
                        ArrayStoreSource::Reg(RDX)
                    )
            )
        {
            use crate::x64::isel::{select, Mem, Op as MachineOp, Operand, Req, Ty};
            let (ty, scale) = match kind {
                MemKind::Int => (Ty::I32, 4),
                MemKind::Long => (Ty::I64, 8),
                MemKind::Byte => (Ty::I8, 1),
                MemKind::Char | MemKind::Short => (Ty::I16, 2),
                MemKind::Ref | MemKind::Float | MemKind::Double => {
                    self.latch_bailout(Bailout::with_context(
                        BailoutReason::Internal(
                            "ir_lower: a non-integral element store reached the GPR emitter",
                        ),
                        format!("{kind:?}"),
                    ));
                    return;
                }
            };
            let addr = Operand::Mem(Mem::base_index(
                base,
                index,
                scale,
                i64::from(ARRAY_DATA_DISP8),
            ));
            let src = match operand {
                ArrayStoreSource::Reg(reg) => Operand::Gpr(reg),
                ArrayStoreSource::IntImm(imm) => Operand::Imm(i64::from(imm)),
            };
            match select(&Req::new(MachineOp::Mov, ty, addr, src)) {
                Ok(store) => self.buf.emit(&store.encoded.bytes),
                Err(_) => self.latch_bailout(Bailout::new(BailoutReason::Internal(
                    "allocated array store has no encoding",
                ))),
            }
            return;
        }
        let d = ARRAY_DATA_DISP8;
        let source = match operand {
            ArrayStoreSource::Reg(reg) => reg,
            ArrayStoreSource::IntImm(imm) => {
                debug_assert_eq!(kind, MemKind::Int);
                self.buf.emit(&[0xC7, 0x44, 0x88, d]); // MOV dword [RAX + RCX*4 + d], imm32
                self.buf.emit(&imm.to_le_bytes());
                return;
            }
        };
        debug_assert!(kind == MemKind::Int || source == RDX);
        match kind {
            // MOV DWORD [RAX + RCX*4 + HEADER_SIZE], r32
            MemKind::Int => {
                if source >= 8 {
                    self.buf.emit_byte(0x44); // REX.R
                }
                self.buf.emit(&[0x89, 0x44 | ((source & 7) << 3), 0x88, d]);
            }
            // MOV QWORD [RAX + RCX*8 + HEADER_SIZE], RDX
            MemKind::Long => self.buf.emit(&[0x48, 0x89, 0x54, 0xC8, d]),
            // MOV BYTE [RAX + RCX*1 + HEADER_SIZE], DL
            MemKind::Byte => self.buf.emit(&[0x88, 0x54, 0x08, d]),
            // MOV WORD [RAX + RCX*2 + HEADER_SIZE], DX  (0x66 = 16-bit operand)
            MemKind::Char | MemKind::Short => self.buf.emit(&[0x66, 0x89, 0x54, 0x48, d]),
            // Structurally unreachable — see the load emitter's note. Same
            // fail-open, worse consequence: a silently dropped array store.
            MemKind::Ref | MemKind::Float | MemKind::Double => {
                self.latch_bailout(Bailout::with_context(
                    BailoutReason::Internal(
                        "ir_lower: a non-integral element store reached the GPR emitter",
                    ),
                    format!("{kind:?}"),
                ));
            }
        }
    }

    /// Must the OSR entry at `bci` be withheld because an exception handler of
    /// this method can reach it and this body can hand an exception to one?
    /// (round 11 wave 11, lane irexc;
    /// `r11w10-orch-osr-exc-exits-loses-handler-updated-locals-FIXED-20260924.md`.)
    ///
    /// The graph has no handler in it, so every header fact the optimizer
    /// derived — a φ collapsed to a constant, a range, a type, a null-ness —
    /// holds on handler-free paths only (`deopt::pcs_reachable_from_handlers`
    /// says why that is exact for a method-entry body and not for a re-entered
    /// one). A rethrow pad is how this body hands the interpreter an exception
    /// its own handler catches; the handler then runs, loops back, and the
    /// interpreter would re-enter here with state no graph path produces:
    /// `R11SyncCallerHoldsCalleeLock.throwLoop` re-entered with `caught` read
    /// as the collapsed constant 0 after every `caught++`.
    ///
    /// No rethrow pad, no hazard: an exit outside every protected range
    /// propagates out of the frame, which is then discarded, and a body with
    /// no exit at all can never have run a handler before it was entered.
    /// Seeding a handler-written local would NOT be enough (a local the try
    /// body wrote before the throwing call arrives with its throw-time value
    /// too), and is not what this does: the entry is withheld, and the loop
    /// keeps the single-pass OSR body.
    pub(in crate::ir_lower) fn osr_entry_closed_by_a_handler(&self, bci: usize) -> bool {
        !self.rethrow_points.is_empty()
            && crate::deopt::pc_reachable_from_handlers(self.osr_handler_reachable.as_deref(), bci)
    }

    /// Count one bci that could not be given an OSR entry.
    pub(in crate::ir_lower) fn note_osr_refusal(&mut self, why: &'static str) {
        match self.osr_refusals.iter_mut().find(|(w, _)| *w == why) {
            Some((_, n)) => *n += 1,
            None => self.osr_refusals.push((why, 1)),
        }
        // OD16-4 (round 16, lane osrdoor): the same reason, run-wide.
        note_osr_entry_census(why);
    }

    /// `MOV RAX, [R11 + disp32]` — read one word of the interpreter's locals
    /// array. R11 is caller-saved and nothing between its load and the seeding
    /// loop calls out, which is the whole reason the seeding runs last.
    pub(in crate::ir_lower) fn emit_mov_rax_from_r11_disp(&mut self, disp: i32) {
        self.buf.emit(&[0x49, 0x8B, 0x83]);
        self.buf.emit(&disp.to_le_bytes());
    }

    /// May a value of this shape be excused from an OSR entry's seeding
    /// obligation, on the grounds that **the stub can produce it itself**?
    ///
    /// One function because there are two duties and they must agree. The
    /// eligibility test below excuses such a value from "every value live
    /// across this block start must be named by a local"; the emission loop
    /// then has to WRITE it. Excusing without emitting is exactly what
    /// `ir-osr-entry-miscompiles-a-spliced-merge-FIXED-20260909.md` was: the
    /// two halves were written in different places and agreed only by
    /// accident, and when the accident ran out a literal loop arm read a frame
    /// word nothing had written.
    ///
    /// Anything added here needs an arm in that emission loop too, and
    /// `an_osr_entry_emits_every_shape_its_eligibility_test_excuses` fails
    /// until it has one.
    pub(in crate::ir_lower) fn osr_entry_can_produce(op: &Op) -> bool {
        matches!(op, Op::Const(_) | Op::ConstF(_))
    }

    /// Round 16, lane osrdoor: does an ordinary OSR entry at `entry_block` owe
    /// NO seed to `id`, a value the linear intervals say is live across the
    /// block start, that the header snapshot does not name and that is not a
    /// constant? `false` keeps the value in the refusal set, which is the
    /// answer every such value got before (and the answer with both switches
    /// off).
    ///
    /// # Why (`r16-alloc-hashmap-put-get-premium-is-the-call-path-not-allocation`)
    ///
    /// `R16AllocPremium.hashmapCached`'s optimizing OSR body was accepted and
    /// never entered: `osr block 1 (bci 9) refused: n16 Some(Phi)`, and `n16`
    /// is the header's MEMORY φ. `regalloc::build_live_model` gives every φ a
    /// location and an interval, memory φs included, and the phi-edge
    /// interference rule then stretches a φ's interval down to its
    /// forward-edge source -- but only when that source itself wants a
    /// location. So the refusal hung on the node TYPE of the memory chain's
    /// entry value: a `LoadStatic` (`hashmapCached`: refused) versus a
    /// `Store` (`R16Bench.hashmap` bci 13: entered, with the same unseeded
    /// memory φ). 24 of the 26 `Some(Phi)` refusals of the census were memory
    /// φs (HashMap `putVal`/`resize`/`getNode`/`treeifyBin`,
    /// `SingleByte.initC2B` -- the `Phi` IR2-4 attributed to an outer header).
    ///
    /// # The two excuses, each behind its own switch
    ///
    /// * `Memory` / `Control` / `Void` ([`ir_osr_excuse_memory_tokens_enabled`]):
    ///   no machine value. The edge copies skip such a φ (`gather_phi_copies`,
    ///   `phis_by_merge`), so nothing ever writes or reads its home; a node
    ///   that names it as an input reads it as an ordering edge only.
    /// * Not live into the block by the converged backward dataflow
    ///   ([`ir_osr_dataflow_live_in_enabled`], `LiveModel::live_in_at`, the
    ///   model the opaque arms are admitted by): no data read of it is
    ///   reachable from the entry before its definition runs again. The shape
    ///   is an OUTER loop's φ at an inner header (`R16ChainGetNode.run` bci
    ///   18, `sum`'s outer φ), whose interval the phi-edge interference rule
    ///   extends to the outer latch. Three exclusions keep it to that: a φ
    ///   of the entry block itself (written by the edge copies the stub jumps
    ///   past -- it is live from the block start whatever `live_in_at` says),
    ///   a reference (a register or home a safepoint map may publish; the
    ///   stub writes it nothing -- JIT round 21's reference half,
    ///   [`Self::osr_entry_ref_needs_no_seed`], writes it null instead), and
    ///   "unknown" (`None`: not converged). A
    ///   deopt frame that names the value is a separate question, still asked
    ///   by `osr_entry_skipped_frame_value` with the value NOT counted as
    ///   written.
    pub(in crate::ir_lower) fn osr_entry_value_needs_no_seed(
        &self,
        id: NodeId,
        entry_block: usize,
        live: &crate::regalloc::LiveModel,
        excuse_tokens: bool,
        dataflow: bool,
    ) -> bool {
        let Some(node) = self.graph.nodes.get(id as usize) else {
            return false;
        };
        Self::osr_entry_value_needs_no_seed_by(
            node,
            self.schedule.node_to_block.get(id as usize).copied() == Some(entry_block),
            || live.live_in_at(entry_block, id),
            excuse_tokens,
            dataflow,
        )
    }

    /// [`Self::osr_entry_value_needs_no_seed`]'s decision, over its inputs
    /// (unit-tested without a lowerer). `in_entry_block`: `node` is scheduled
    /// in the entry block; `live_in`: the dataflow answer, asked last.
    pub(in crate::ir_lower) fn osr_entry_value_needs_no_seed_by(
        node: &Node,
        in_entry_block: bool,
        live_in: impl FnOnce() -> Option<bool>,
        excuse_tokens: bool,
        dataflow: bool,
    ) -> bool {
        if excuse_tokens && matches!(node.ty, IrType::Memory | IrType::Control | IrType::Void) {
            return true;
        }
        if !dataflow
            || !matches!(
                node.ty,
                IrType::Int | IrType::Long | IrType::Float | IrType::Double
            )
            || (node.op == Op::Phi && in_entry_block)
        {
            return false;
        }
        live_in() == Some(false)
    }

    /// JIT round 21 wave 17, lane osrrefs (OD16-2 of
    /// `jit-r16-osrdoor-proposals-RETIRED-20261006.md`;
    /// [`ir_osr_dataflow_live_in_refs_enabled`]): does an ordinary OSR entry at
    /// `entry_block` owe the unnamed REFERENCE `id` no seed, because the
    /// converged backward dataflow says it is not live into the block? The
    /// stub then writes it NULL instead ([`Self::emit_osr_null_ref`]).
    pub(in crate::ir_lower) fn osr_entry_ref_needs_no_seed(
        &self,
        id: NodeId,
        entry_block: usize,
        live: &crate::regalloc::LiveModel,
    ) -> bool {
        let Some(node) = self.graph.nodes.get(id as usize) else {
            return false;
        };
        Self::osr_entry_ref_needs_no_seed_by(
            node,
            self.schedule.node_to_block.get(id as usize).copied() == Some(entry_block),
            || live.live_in_at(entry_block, id),
        )
    }

    /// [`Self::osr_entry_ref_needs_no_seed`]'s decision, over its inputs
    /// (unit-tested without a lowerer).
    ///
    /// The primitive rule of [`Self::osr_entry_value_needs_no_seed_by`],
    /// for `Ref` alone, with the same two exclusions (a φ of the entry block
    /// itself, written by the edge copies the stub jumps past; an unconverged
    /// model). What made a reference unexcusable there -- a safepoint map, the
    /// frame block or the band scan may publish its home word or register, and
    /// an unseeded word there is a wild root -- is answered by the stub, not
    /// by this test: it writes null to both (`emit_osr_null_ref`), which every
    /// root consumer already handles (`zero_ref_value_slots`' argument). Its
    /// colour and register are its own at the entry (its interval covers the
    /// entry, which is how it got here; a split-managed value refuses the
    /// entry later). No data read of it is reachable from the entry before
    /// its definition runs again, so nothing reads the null. A deopt frame
    /// past the entry that names it is still refused by
    /// `osr_entry_skipped_frame_value`, which does not count it as written.
    pub(in crate::ir_lower) fn osr_entry_ref_needs_no_seed_by(
        node: &Node,
        in_entry_block: bool,
        live_in: impl FnOnce() -> Option<bool>,
    ) -> bool {
        if node.ty != IrType::Ref || node.op == Op::Dead || (node.op == Op::Phi && in_entry_block) {
            return false;
        }
        live_in() == Some(false)
    }

    /// t1 wave 8 (lane OSRREFS; OSRREFS21-1/-2): the census key under which
    /// an entry refused for "a value live here is named by no local" would
    /// have passed THAT check with the reference half
    /// ([`ir_osr_dataflow_live_in_refs_enabled`], default ON) on, or `None`;
    /// counted only with that switch at `=0`. `unnamed` is
    /// the refusal's set, already filtered by the switches that are on.
    pub(in crate::ir_lower) fn osr_entry_would_admit_refs_key(
        &self,
        entry_block: usize,
        unnamed: &[NodeId],
        live: &crate::regalloc::LiveModel,
        dataflow_on: bool,
    ) -> Option<&'static str> {
        let verdicts: Vec<(bool, bool)> = unnamed
            .iter()
            .map(|&id| {
                let by_ref = self.osr_entry_ref_needs_no_seed(id, entry_block, live);
                let by_prim = !by_ref
                    && !dataflow_on
                    && self.osr_entry_value_needs_no_seed(id, entry_block, live, false, true);
                (by_ref, by_prim)
            })
            .collect();
        Self::osr_entry_would_admit_refs_key_by(&verdicts)
    }

    /// [`Self::osr_entry_would_admit_refs_key`]'s decision over each unnamed
    /// value's `(excused by the reference half, excused by the primitive
    /// half)`. At least one reference must be among them: an entry the
    /// primitive half alone opens is `would-admit:dataflow-live-in`'s.
    pub(in crate::ir_lower) fn osr_entry_would_admit_refs_key_by(
        verdicts: &[(bool, bool)],
    ) -> Option<&'static str> {
        if !verdicts.iter().any(|&(r, _)| r) || !verdicts.iter().all(|&(r, p)| r || p) {
            return None;
        }
        if verdicts.iter().all(|&(r, _)| r) {
            Some("would-admit:dataflow-live-in-refs")
        } else {
            Some("would-admit:dataflow-live-in-refs+dataflow-live-in")
        }
    }

    /// Interpreter round i2 wave 58, lane L2
    /// (`CRATONVM_JIT_IR_OSR_EXCUSE_MEMORY_ORDER_USES`): does an OSR entry at
    /// `entry_block` owe `id` no seed because every reader the entered path can
    /// reach before `id`'s defining block runs again reads it only as a memory
    /// token?
    ///
    /// The shape is `while (!stop) { for (..) {..} burnt += x; }` with `stop`
    /// volatile: the outer header's `getstatic stop` is an `Int` value AND the
    /// memory chain's producer (a volatile read orders what follows it), so the
    /// `getstatic burnt` after the inner loop names it in its token slot. The
    /// interval model counts that slot as a read and stretches the value's
    /// range across the inner header, whose entry then refused it as "named by
    /// no local". The lowering reads no machine value through a token slot.
    ///
    /// Fail-closed rules, each a refusal:
    ///
    /// * only `Int` / `Long` / `Float` / `Double` values: a reference's
    ///   register or home can be published by a safepoint map, and an unseeded
    ///   word there is a wild root;
    /// * a value read by a VALUE φ anywhere (the read happens on an edge this
    ///   test does not place), or by a node that has no block;
    /// * a value read as a value in a block the entered path reaches before it
    ///   first enters the value's own block (`osr_entry_runs_before_entering`;
    ///   the entry block itself always counts as reached).
    ///
    /// A deopt frame that names the value is a separate question, still asked
    /// by [`Self::osr_entry_skipped_frame_value`] with the value not seeded.
    pub(crate) fn osr_entry_value_read_only_as_memory_order(
        graph: &Graph,
        schedule: &Schedule,
        id: NodeId,
        entry_block: usize,
    ) -> bool {
        let Some(node) = graph.nodes.get(id as usize) else {
            return false;
        };
        if !matches!(
            node.ty,
            IrType::Int | IrType::Long | IrType::Float | IrType::Double
        ) {
            return false;
        }
        let nblocks = schedule.blocks.len();
        let Some(def_block) = schedule
            .node_to_block
            .get(id as usize)
            .copied()
            .filter(|&b| b < nblocks)
        else {
            return false;
        };
        let reached_bits = Self::osr_entry_reach_bits(schedule, entry_block, Some(def_block));
        let reached_vec = if reached_bits.is_none() {
            Some(Self::osr_entry_runs_before_entering(schedule, entry_block, Some(def_block)))
        } else {
            None
        };
        let is_reached = |b: usize| -> bool {
            if let Some(ref bits) = reached_bits {
                if b < nblocks {
                    (bits[b >> 6] & (1u64 << (b & 63))) != 0
                } else {
                    true
                }
            } else {
                reached_vec.as_ref().and_then(|v| v.get(b).copied()).unwrap_or(true)
            }
        };
        // A block's terminator (an `If` testing the value) is placed by the
        // block it ends, which `node_to_block` need not record.
        let terminated_by: rustc_hash::FxHashMap<NodeId, usize> = schedule
            .blocks
            .iter()
            .enumerate()
            .filter_map(|(b, blk)| blk.terminator.map(|t| (t, b)))
            .collect();
        let mut any_token_use = false;
        for (uid, user) in graph.nodes.iter().enumerate() {
            if user.op == Op::Dead {
                continue;
            }
            for (idx, &inp) in user.inputs.iter().enumerate() {
                if inp != id {
                    continue;
                }
                if crate::ir::is_memory_token_slot(user, idx) {
                    any_token_use = true;
                    continue;
                }
                if user.op == Op::Phi {
                    return false;
                }
                // Cast: an index into the node arena is a `NodeId`.
                let Some(use_block) = terminated_by.get(&(uid as NodeId)).copied().or_else(|| {
                    schedule
                        .node_to_block
                        .get(uid)
                        .copied()
                        .filter(|&b| b < nblocks)
                }) else {
                    return false;
                };
                if is_reached(use_block) {
                    return false;
                }
            }
        }
        // A value with no token reader is not this excuse's shape (it is
        // live across for some other reason, or not at all).
        any_token_use
    }

    /// Round 16 wave 14 (lane osrir): the census key of the first opt-in OSR
    /// excuse arm -- `CRATONVM_JIT_IR_OSR_EXCUSE_MEMORY_TOKENS` alone,
    /// `CRATONVM_JIT_IR_OSR_DATAFLOW_LIVE_IN` alone, then both -- under which
    /// an ordinary entry at `entry_block` refused for "a value live here is
    /// named by no local" would have passed THAT check, or `None`. `unnamed`
    /// is the refusal's set, already filtered by the switches that are on; an
    /// arm those switches already run is skipped. Later checks of
    /// [`Self::emit_osr_entry_stubs`] (split-managed values, deopt frames) can
    /// still refuse such an entry.
    ///
    /// Why: the triage measured the two switches only as a pair
    /// (`jit-optin-switch-triage-r16-RETIRED-20261006.md`: poly -22 %, three OSR rows
    /// +25-35 % in one of two runs), so which switch admitted which loop was
    /// never known. With this, the exit `OSR entry census` of a default run
    /// names the headers each switch would open (`R16Bench.poly` bci 58,
    /// refused for a memory phi, should count under
    /// `would-admit:excuse-memory-tokens`). Compile path only; one census lock
    /// per refused entry, as the refusal itself.
    #[allow(clippy::too_many_arguments)]
    pub(in crate::ir_lower) fn osr_entry_would_admit_key(
        &self,
        entry_block: usize,
        locals: &[NodeId],
        unnamed: &[NodeId],
        live: &crate::regalloc::LiveModel,
        excuse_tokens: bool,
        dataflow: bool,
        order_uses: bool,
        remat_on: bool,
    ) -> Option<&'static str> {
        // Interpreter round i2 wave 58 (lane L2): the memory-order excuse
        // (`CRATONVM_JIT_IR_OSR_EXCUSE_MEMORY_ORDER_USES`) alone and with the
        // dataflow one, the pair the proposal's nested shape needs.
        const ARMS: [(&str, bool, bool, bool); 5] = [
            ("would-admit:excuse-memory-tokens", true, false, false),
            ("would-admit:dataflow-live-in", false, true, false),
            (
                "would-admit:excuse-memory-tokens+dataflow-live-in",
                true,
                true,
                false,
            ),
            ("would-admit:excuse-memory-order-uses", false, false, true),
            (
                "would-admit:excuse-memory-order-uses+dataflow-live-in",
                false,
                true,
                true,
            ),
        ];
        for (key, tok, flow, order) in ARMS {
            let (tok, flow, order) = (tok || excuse_tokens, flow || dataflow, order || order_uses);
            if tok == excuse_tokens && flow == dataflow && order == order_uses {
                continue;
            }
            let rest: Vec<NodeId> = unnamed
                .iter()
                .copied()
                .filter(|&id| {
                    !self.osr_entry_value_needs_no_seed(id, entry_block, live, tok, flow)
                        && !(order
                            && Self::osr_entry_value_read_only_as_memory_order(
                                &self.graph,
                                &self.schedule,
                                id,
                                entry_block,
                            ))
                })
                .collect();
            if rest.len() == unnamed.len() {
                continue;
            }
            if rest.is_empty()
                || (remat_on
                    && self
                        .osr_entry_remat_program(entry_block, locals, &rest)
                        .is_some())
            {
                return Some(key);
            }
        }
        None
    }

    /// `CRATONVM_JIT_IR_OSR_HEADER_ENTRIES_ONLY`, **default ON** (`=0` is the
    /// kill switch): plan an ordinary OSR entry only at a block a backward
    /// branch can land on ([`Self::osr_entry_block_is_back_edge_target`]).
    /// Round 12 wave 1 (lane osr, irback proposal W18-2). No `static`
    /// (`process_global_statics_ratchet`): asked once per
    /// `emit_osr_entry_stubs`.

    /// The bci of block `b`'s control node, if it carries one.
    pub(in crate::ir_lower) fn osr_entry_block_bci(&self, b: usize) -> Option<usize> {
        let ctrl = self.schedule.blocks.get(b)?.ctrl;
        self.graph.nodes.get(ctrl as usize)?.bytecode_pc
    }

    /// Can the door ever ask for an entry at block `bi`?
    ///
    /// The door (`jit_bridge::try_osr`) enters only at the pc a TAKEN
    /// BACKWARD BRANCH landed on. An entry planned anywhere else — a loop
    /// exit, a join whose live values the snapshot happens to name — is
    /// never asked for: a dead stub (a prologue, one zeroing store per JVM
    /// local, the frame record and the seeds), a `(bci, offset, needed)` row
    /// the door scans, and a memo row per pc in the door's fan-out.
    ///
    /// Two tests, either admits, and an unknown bci admits:
    ///
    /// * a back edge in the graph (a reachable predecessor `bi` dominates):
    ///   the natural-loop header of a javac top-test loop;
    /// * a predecessor whose bci is not below `bi`'s: the edge is backward in
    ///   bytecode order. That is the backward-branch target of a BOTTOM-test
    ///   loop (`goto cond; body: ..; cond: if (..) goto body`, as ecj and
    ///   other compilers emit), where the graph's natural header is `cond`
    ///   while the interpreter arrives at `body`, which is not a header.
    ///
    /// The block holding a backward branch at `P` to `T` starts at or after
    /// `T` (a target always starts a block), so the second test never
    /// refuses a real target. A predecessor inside a splice is compared at
    /// its invoke ([`resolve_spliced_pc`]): the callee's return to the pc
    /// after the call is forward, and a block that branches back to `T` from
    /// a splice has its invoke at or after `T` (else it would run through `T`,
    /// a leader). It is a superset on purpose: unrolled copies pass it, and
    /// are left to the checks below exactly as before.
    pub(in crate::ir_lower) fn osr_entry_block_is_back_edge_target(&self, bi: usize) -> bool {
        let Some(block) = self.schedule.blocks.get(bi) else {
            return false;
        };
        let dom = &self.schedule.dom;
        let own = self.osr_entry_block_bci(bi);
        block.predecessors.iter().any(|&p| {
            (dom.is_reachable(p) && dom.dominates(bi, p))
                || match (own, self.osr_entry_block_bci(p)) {
                    // A spliced predecessor stands at its invoke: the splice's
                    // return into this method's code is a forward edge.
                    (Some(own), Some(pred)) => resolve_spliced_pc(self.spliced_ranges, pred) >= own,
                    // The method's entry block carries no bci, and holds no
                    // backward branch: it runs once, before any loop.
                    _ => !self.osr_entry_block_is_method_entry(p),
                }
        })
    }

    /// Is block `b` the method's entry block (its control is `Start`, or
    /// `Start`'s control projection)?
    pub(in crate::ir_lower) fn osr_entry_block_is_method_entry(&self, b: usize) -> bool {
        let Some(ctrl) = self
            .schedule
            .blocks
            .get(b)
            .and_then(|blk| self.graph.nodes.get(blk.ctrl as usize))
        else {
            return false;
        };
        match ctrl.op {
            Op::Start => true,
            Op::Proj(_) => ctrl
                .inputs
                .first()
                .and_then(|&s| self.graph.nodes.get(s as usize))
                .is_some_and(|s| s.op == Op::Start),
            _ => false,
        }
    }

    /// `CRATONVM_JIT_IR_OSR_FRAME_VALUE_REFUSAL`, **default ON** (`=0` is the
    /// kill switch): refuse an OSR entry past which a deopt frame names a value
    /// the entry skips ([`Self::osr_entry_skipped_frame_value`]). Round 13 wave 13
    /// (lane jitfix), `r13w12-snapliv-loop-header-snapshot-doubles-as-the-osr-seed-list`.
    /// Asked once per [`Self::emit_osr_entry_stubs`].
    pub(in crate::ir_lower) fn osr_entry_frame_value_refusal_enabled() -> bool {
        true
    }

    /// `CRATONVM_JIT_IR_OSR_FRAME_SITES_PATH_AWARE`, **default ON** (`=0` is
    /// the kill switch; JIT round 17 wave 2, lane monitor2,
    /// `r17-monitor-forked-block-coarsening-is-not-the-body-that-runs`): the
    /// two halves of the OSR entry's frame-value refusal read a site's frame by
    /// SNAPSHOT, not by bci.
    ///
    /// * [`Self::osr_entry_frame_sites`] no longer lists the first snapshot at
    ///   a site's bci for a site that publishes a frame of its OWN there (a
    ///   per-copy frame of the unroller or of the loop block coarsening, LICM's
    ///   pre-header frame). That site never publishes the by-bci one, and its
    ///   node never anchors the bci's table point (`note_bci_anchor`), so the
    ///   pair described a frame at a block where nothing resumes into it --
    ///   in the forked coarsening, the ORIGINAL (slow-arm) frame at a fast-arm
    ///   block, naming values the fast arm never computes.
    /// * WITHDRAWN (JIT round 18 wave 11 close, lane testfix18): the second
    ///   half made [`Self::osr_entry_skipped_frame_value`]'s dominance fast
    ///   path wait for the value's block to dominate the site's block. A pair
    ///   that breaks that is no entry-skipped value (the method-entry path
    ///   misses the value there too), and the check withdrew the opaque entry
    ///   of `AtomicProtectedOsrProbe.protectedLoop` (a loop whose header opens
    ///   a try range); see the comment at the fast path.
    ///
    /// Off: the by-bci candidate for every site, as before. Read once per
    /// [`Self::emit_osr_entry_stubs`].

    /// Every `(block, snapshot index)` a deopt site of this body may publish
    /// from, for [`Self::osr_entry_skipped_frame_value`]. A SUPERSET on purpose (a
    /// wider list refuses more entries, never fewer): per site of
    /// [`Self::deopt_sites_by_bci`], the snapshot [`Self::snapshot_published_by_site`]
    /// picks, the first flat snapshot at its resume bci (the table point the
    /// by-bci resume readers find; since round 17 wave 2 not for a site that
    /// publishes its own named frame there, see
    /// [`Self::osr_frame_sites_path_aware_enabled`]), the snapshot the node
    /// names, and the first chain snapshot at its raw pc
    /// ([`Self::splice_chain_snapshot_for_site`]'s fallback). A site the
    /// schedule did not place emits nothing and is left out.
    pub(in crate::ir_lower) fn osr_entry_frame_sites(&self) -> Vec<(usize, usize)> {
        let nblocks = self.schedule.blocks.len();
        let path_aware = true;
        let mut out: Vec<(usize, usize)> = Vec::new();
        for (&bci, sites) in &self.deopt_sites_by_bci {
            for &site in sites {
                let Some(n) = self.graph.nodes.get(site as usize) else {
                    continue;
                };
                if n.op == Op::Dead {
                    continue;
                }
                let Some(b) = self
                    .schedule
                    .node_to_block
                    .get(site as usize)
                    .copied()
                    .filter(|&b| b < nblocks)
                else {
                    continue;
                };
                let raw_pc = match n.op {
                    Op::Guard { bci: g } => Some(g),
                    _ => n.bytecode_pc,
                };
                let published = self.snapshot_published_by_site(n, bci);
                // A site publishing its own named frame at `bci` never
                // publishes the by-bci one (round 17 wave 2, lane monitor2).
                let publishes_own = path_aware
                    && n.frame_snapshot
                        .is_some_and(|s| Some(s as usize) == published);
                // A site that names its own flat snapshot at this bci (the
                // pre-header copy of a hoisted read or a range predicate,
                // whose frame is the header's with every phi at its entry
                // value) publishes THAT snapshot and no other: its pads bake
                // the box `resolve_frame_state_for_site` built from it, and
                // the by-bci readers of the table (`trap_reason_at_bci`, the
                // provenance checks) read a point's reason and semantics,
                // never its values. The first snapshot at the bci -- the
                // header's own, naming the header phis -- is then not a frame
                // this site can resume with, and listing it refused every
                // opaque OSR arm whose path reaches a hoisted read before the
                // header it was hoisted from (`R16Bench.matrix` at bci 33:
                // the j-header phi of the multiply nest, round 16 matrix).
                let names_own_flat = n.frame_snapshot.is_some_and(|sj| {
                    self.graph
                        .safepoints
                        .get(sj as usize)
                        .is_some_and(|s| s.bci == bci && !s.is_splice_state())
                });
                let candidates = [
                    published,
                    if publishes_own || names_own_flat {
                        None
                    } else {
                        self.safepoint_first_by_bci.get(&bci).copied()
                    },
                    n.frame_snapshot.map(|s| s as usize),
                    raw_pc.and_then(|pc| self.splice_state_first_by_pc.get(&pc).copied()),
                    // A spliced call's post-call chain (wave 47, lane L3).
                    self.post_call_splice_state_by_call.get(&site).copied(),
                ];
                out.extend(candidates.into_iter().flatten().map(|si| (b, si)));
            }
        }
        out.sort_unstable();
        out.dedup();
        out
    }

    /// The first value a deopt frame reachable from an OSR entry names that the
    /// entered path never writes, or `None` when every such value is available.
    ///
    /// Round 13 wave 13 (lane jitfix;
    /// `r13w12-snapliv-loop-header-snapshot-doubles-as-the-osr-seed-list`). The
    /// entry admission in [`Self::emit_osr_entry_stubs`] counts DATA liveness
    /// only (`LiveModel`'s ranges ignore frame-state uses), so a value whose
    /// only reader past the entry is a deopt frame -- an in-loop recomputation
    /// GVN merged into a pre-loop node, a LICM-hoisted temp, a pre-loop value a
    /// pruned header no longer names -- was admitted, and the trap then read a
    /// home word (or register) the stub never wrote.
    ///
    /// * `frame_sites`: [`Self::osr_entry_frame_sites`].
    /// * `entry`: the block the stub jumps to.
    /// * `avail_root`: a block every entered path runs before anything else it
    ///   runs past `entry` -- `entry` itself for an ordinary entry, the merge
    ///   an opaque arm jumps into for an opaque one. Only the fast path reads it.
    /// * `root_phis_written`: whether `avail_root`'s φs are written on the
    ///   entered path (an opaque arm's edge copies write them; an ordinary
    ///   entry lands past its header's edge copies).
    /// * `seeded`: the values the stub writes itself (the header snapshot's
    ///   locals for an ordinary entry; nothing for an opaque arm).
    ///
    /// A value `v` named by a frame at a site in block `D` is available when
    /// it is seeded, a constant (every frame describes one by value),
    /// `Op::Dead` (described as unresumable, never read), a scalar-replaced
    /// object whose field values are all available, or when every entered
    /// path to `D` runs `v`'s defining block `d` first -- enters `d` through a
    /// real edge, which also writes a φ of `d`. That last test is a walk from
    /// `entry` that never enters `d` ([`Self::osr_entry_runs_before_entering`]),
    /// memoised per `d`; `v` is unavailable exactly when the walk reaches `D`.
    /// It is what lets an inner loop's entry keep an outer loop's φ that a
    /// frame in the outer body names: every path back there crosses the outer
    /// header.
    ///
    /// Fast path, taken first: `d == entry` (not a φ of an ordinary entry's
    /// header), or `avail_root` dominates `d` (a φ of `avail_root` only when
    /// `root_phis_written`). Sound because a frame's value dominates its site
    /// (`ir_schedule`'s `FrameState` check): with `R` dominating `d` and `d`
    /// dominating `D`, a path `R -> D` avoiding `d` would extend a simple path
    /// `start -> R` (which cannot pass `d`: every path to `d` passes `R`
    /// first) into a path `start -> D` avoiding `d`.
    ///
    /// Anything else -- a value defined in a block the entry skips, or never
    /// scheduled -- refuses the entry.
    #[allow(clippy::too_many_arguments)]
    pub(in crate::ir_lower) fn osr_entry_skipped_frame_value(
        graph: &Graph,
        schedule: &Schedule,
        sr: Option<&ScalarReplacementMap>,
        frame_sites: &[(usize, usize)],
        entry: usize,
        avail_root: usize,
        root_phis_written: bool,
        seeded: &std::collections::HashSet<NodeId>,
    ) -> Option<NodeId> {
        if frame_sites.is_empty() {
            return None;
        }
        let nblocks = schedule.blocks.len();
        // The blocks the entered path can reach: a forward walk from `entry`.
        let reached_bits = Self::osr_entry_reach_bits(schedule, entry, None);
        let reached_vec = if reached_bits.is_none() {
            Some(Self::osr_entry_runs_before_entering(schedule, entry, None))
        } else {
            None
        };
        let is_reached = |b: usize| -> bool {
            if let Some(ref bits) = reached_bits {
                b < nblocks && (bits[b >> 6] & (1u64 << (b & 63))) != 0
            } else {
                reached_vec.as_ref().and_then(|v| v.get(b).copied()).unwrap_or(false)
            }
        };
        // `(value, block of a site whose frame names it)`.
        let mut values: Vec<(NodeId, usize)> = Vec::new();
        let mut snapshots_seen: rustc_hash::FxHashSet<(usize, usize)> =
            rustc_hash::FxHashSet::default();
        for &(b, si) in frame_sites {
            if !is_reached(b) || !snapshots_seen.insert((b, si)) {
                continue;
            }
            if let Some(sp) = graph.safepoints.get(si) {
                values.extend(
                    sp.locals
                        .iter()
                        .chain(sp.stack.iter())
                        .chain(sp.monitors.iter())
                        .copied()
                        .chain(sp.sb_virtual_nodes())
                        .map(|v| (v, b)),
                );
            }
        }
        let dom = &schedule.dom;
        let mut seen: rustc_hash::FxHashSet<(NodeId, usize)> = rustc_hash::FxHashSet::default();
        // Per defining block `d`: the blocks an entered path can run before it
        // first enters `d`.
        let mut before_bits: rustc_hash::FxHashMap<usize, [u64; 16]> = rustc_hash::FxHashMap::default();
        let mut before: rustc_hash::FxHashMap<usize, Vec<bool>> = rustc_hash::FxHashMap::default();
        while let Some((v, site_block)) = values.pop() {
            if v == NO_NODE || seeded.contains(&v) || !seen.insert((v, site_block)) {
                continue;
            }
            // A scalar-replaced object is described by its field values.
            if let Some(obj) = sr.and_then(|m| m.objects.get(&v)) {
                values.extend(obj.field_values.iter().flatten().map(|&f| (f, site_block)));
                values.extend(
                    obj.store_fields
                        .iter()
                        .flatten()
                        .filter_map(|&(_, value)| value)
                        .map(|f| (f, site_block)),
                );
                continue;
            }
            let Some(node) = graph.nodes.get(v as usize) else {
                return Some(v);
            };
            if matches!(node.op, Op::Const(_) | Op::ConstF(_) | Op::Dead) {
                continue;
            }
            let d = schedule
                .node_to_block
                .get(v as usize)
                .copied()
                .unwrap_or(usize::MAX);
            if d >= nblocks || !dom.is_reachable(d) {
                return Some(v);
            }
            let is_phi = node.op == Op::Phi;
            // No `d dominates site_block` premise here. Round 17 wave 2 (lane
            // monitor2) required one and asked the walk when it failed; JIT
            // round 18 wave 11 close (lane testfix18,
            // `r18-osr-frame-premise-withdraws-protected-loop-entry-FIXED-20261006.md`)
            // withdrew it, because it never names a value the ENTRY skips:
            // when `d` does not dominate the site, some method-entry path
            // already reaches the site without running `d`, and every entered
            // path past `avail_root` (which dominates `d`) is the tail of such a
            // method-entry path, so the entry is no worse than the method's own
            // start. What it refused were pairs whose frame does not dominate
            // the site -- superset candidates of [`Self::osr_entry_frame_sites`]
            // no site publishes there, or a frame the method-entry body
            // publishes there just as dishonestly -- and that withdrew the
            // opaque entry of a loop whose header opens a protected range
            // (`AtomicProtectedOsrProbe.protectedLoop` pc=56).
            // The forked coarsening's `(fast block, original frame)` pair it was
            // written for is no longer listed at all (the candidate half of
            // [`Self::osr_frame_sites_path_aware_enabled`]).
            let fast = (d == entry && !(is_phi && entry == avail_root && !root_phis_written))
                || (dom.dominates(avail_root, d)
                    && !(is_phi && d == avail_root && !root_phis_written));
            if fast {
                continue;
            }
            let runs_first_hit = if nblocks <= 1024 {
                let bits = before_bits.entry(d).or_insert_with(|| {
                    Self::osr_entry_reach_bits(schedule, entry, Some(d)).unwrap_or([0; 16])
                });
                if site_block < nblocks {
                    (bits[site_block >> 6] & (1u64 << (site_block & 63))) != 0
                } else {
                    true
                }
            } else {
                let runs_first = before
                    .entry(d)
                    .or_insert_with(|| Self::osr_entry_runs_before_entering(schedule, entry, Some(d)));
                runs_first.get(site_block).copied().unwrap_or(true)
            };
            if runs_first_hit {
                return Some(v);
            }
        }
        None
    }

    /// The blocks an OSR-entered path starting at `entry` can run before it
    /// first ENTERS `avoid` through an edge (every block it can reach when
    /// `avoid` is `None`). `entry` itself is always run first -- the stub jumps
    /// into it without taking an edge -- so `avoid == Some(entry)` is the walk
    /// from `entry`'s successors that never re-enters it, plus `entry`. For
    /// [`Self::osr_entry_skipped_frame_value`].
    /// Fast single-target reachability check matching
    /// `Self::osr_entry_runs_before_entering(schedule, entry, avoid).get(target).copied().unwrap_or(fallback)`
    /// without heap allocation for CFGs up to 256 blocks and with early exit.
    /// Stack-allocated reachability bitset for `blocks.len() <= 256`, avoiding
    /// `vec![false; n]` heap allocations in `osr_entry_runs_before_entering`.
    pub(in crate::ir_lower) fn osr_entry_reach_bits(
        schedule: &Schedule,
        entry: usize,
        avoid: Option<usize>,
    ) -> Option<[u64; 16]> {
        let n = schedule.blocks.len();
        if n > 1024 {
            return None;
        }
        let mut reached = [0u64; 16];
        let mut work = [0usize; 1024];
        let mut sp = 1usize;
        work[0] = entry;
        let mut first = true;
        while sp > 0 {
            sp -= 1;
            let b = work[sp];
            if !first && avoid == Some(b) {
                continue;
            }
            first = false;
            if b >= n {
                continue;
            }
            let word = b >> 6;
            let bit = 1u64 << (b & 63);
            if (reached[word] & bit) != 0 {
                continue;
            }
            reached[word] |= bit;
            if let Some(block) = schedule.blocks.get(b) {
                for &succ in &block.successors {
                    if succ < n && (reached[succ >> 6] & (1u64 << (succ & 63))) == 0 && sp < 1024 {
                        work[sp] = succ;
                        sp += 1;
                    }
                }
            }
        }
        Some(reached)
    }

    pub(in crate::ir_lower) fn osr_entry_can_reach(
        schedule: &Schedule,
        entry: usize,
        avoid: Option<usize>,
        target: usize,
        fallback: bool,
    ) -> bool {
        let n = schedule.blocks.len();
        if target >= n {
            return fallback;
        }
        if entry >= n {
            return false;
        }
        if entry == target {
            return true;
        }
        if n <= 1024 {
            let mut visited = [0u64; 16];
            let mut stack = [0usize; 1024];
            let mut sp = 1usize;
            stack[0] = entry;
            let mut first = true;
            while sp > 0 {
                sp -= 1;
                let b = stack[sp];
                if !first && avoid == Some(b) {
                    continue;
                }
                first = false;
                if b >= n {
                    continue;
                }
                let word = b >> 6;
                let bit = 1u64 << (b & 63);
                if (visited[word] & bit) != 0 {
                    continue;
                }
                if b == target {
                    return true;
                }
                visited[word] |= bit;
                if let Some(block) = schedule.blocks.get(b) {
                    for &succ in &block.successors {
                        if succ < n && (visited[succ >> 6] & (1u64 << (succ & 63))) == 0 {
                            if sp < 1024 {
                                stack[sp] = succ;
                                sp += 1;
                            }
                        }
                    }
                }
            }
            return false;
        }
        Self::osr_entry_runs_before_entering(schedule, entry, avoid)
            .get(target)
            .copied()
            .unwrap_or(fallback)
    }

    pub(in crate::ir_lower) fn osr_entry_runs_before_entering(
        schedule: &Schedule,
        entry: usize,
        avoid: Option<usize>,
    ) -> Vec<bool> {
        let n = schedule.blocks.len();
        let mut reached = vec![false; n];
        if n <= 1024 {
            let mut work = [0usize; 1024];
            let mut sp = 1usize;
            work[0] = entry;
            let mut first = true;
            while sp > 0 {
                sp -= 1;
                let b = work[sp];
                if !first && avoid == Some(b) {
                    continue;
                }
                first = false;
                if b >= n || reached[b] {
                    continue;
                }
                reached[b] = true;
                if let Some(block) = schedule.blocks.get(b) {
                    for &succ in &block.successors {
                        if succ < n && !reached[succ] && sp < 1024 {
                            work[sp] = succ;
                            sp += 1;
                        }
                    }
                }
            }
            return reached;
        }
        let mut work: Vec<usize> = vec![entry];
        let mut first = true;
        while let Some(b) = work.pop() {
            if !first && avoid == Some(b) {
                continue;
            }
            first = false;
            match reached.get_mut(b) {
                Some(r) if !*r => *r = true,
                _ => continue,
            }
            if let Some(block) = schedule.blocks.get(b) {
                work.extend(block.successors.iter().copied());
            }
        }
        reached
    }

    /// Emit an entry stub for each bci this body can be entered at part-way.
    ///
    /// ABI: `extern "C" fn(ctx: i64, locals: *const i64) -> i64`, i.e. the
    /// entry ABI's first two registers. It builds this tier's own frame — which
    /// is the point. `osr_trampoline` cannot: it takes some twenty layout
    /// parameters because it builds the SINGLE-PASS frame from outside, and
    /// that rests on a fixed per-method local→home map. This tier has no such
    /// map; its locals are SSA values on colour-assigned slots that differ from
    /// bci to bci. The lowerer, by contrast, knows the layout because it built
    /// it, and the safepoint snapshot already says where each local lives at
    /// each bci — the very table `build_deopt_points` reads, used in the
    /// opposite direction.
    ///
    /// **A bci is refused unless every value live on entry is one the
    /// interpreter can supply.** The snapshot names the locals; anything else
    /// live across that point — a hoisted invariant, a CSE'd subexpression —
    /// exists only because earlier compiled code computed it, and no seeding
    /// can reconstruct it. `live.range` gives that check exactly: a value whose
    /// range STARTS before the entry position and extends past it is live-in,
    /// and every such value must be named by the snapshot.
    ///
    /// # Opaque entries (round 11 wave 14, lane osrmerge)
    ///
    /// A loop header the builder gave an opaque OSR merge
    /// (`IrBuilder::build_opaque_osr_merge`, `CRATONVM_JIT_OSR_OPAQUE_ENTRY`)
    /// is entered differently: the stub jumps to the merge's OSR ARM, parks
    /// the locals pointer in the phi-copy scratch word for the arm's
    /// `Op::OsrLocal`s and seeds nothing — the arm's edge copies do. Its
    /// admission is the arm's DATAFLOW live-in set (`LiveModel::live_in_at`),
    /// which may hold constants only. Such a header is never withheld for a
    /// handler (`osr_entry_closed_by_a_handler`): nothing the optimizer
    /// derived at it assumes a graph path.
    ///
    /// Returns how many entries it emitted that are NOT opaque, which is what
    /// `lower_inner` asks before it publishes `osr_exit::guard_exit_fact_opaque`.
    pub(in crate::ir_lower) fn emit_osr_entry_stubs(
        &mut self,
        live: &crate::regalloc::LiveModel,
    ) -> usize {
        if !ir_osr_entry_enabled() {
            return 0;
        }
        if !live.converged {
            self.note_osr_refusal("liveness did not converge");
            return 0;
        }
        // The opaque arms, by header bci and block, and every block of their
        // `If` (both projections), which the ordinary plan below skips. Empty
        // with `CRATONVM_JIT_OSR_OPAQUE_ENTRY` off: no graph has the flag.
        let mut opaque_arms: Vec<(usize, usize)> = Vec::new();
        let mut opaque_blocks: std::collections::HashSet<usize> = std::collections::HashSet::new();
        for bi in 0..self.schedule.blocks.len() {
            let ctrl = self.schedule.blocks[bi].ctrl;
            let Some(proj) = self.graph.nodes.get(ctrl as usize) else {
                continue;
            };
            let Op::Proj(which) = proj.op else {
                continue;
            };
            let flag = proj
                .inputs
                .first()
                .and_then(|&iff| self.graph.nodes.get(iff as usize))
                .filter(|iff| iff.op == Op::If)
                .and_then(|iff| iff.inputs.get(1))
                .and_then(|&cond| self.graph.nodes.get(cond as usize));
            let Some(Op::OsrEntryFlag { header_bci }) = flag.map(|n| &n.op) else {
                continue;
            };
            opaque_blocks.insert(bi);
            if which == 0 {
                opaque_arms.push((*header_bci, bi));
            }
        }
        let opaque_bcis: std::collections::HashSet<usize> =
            opaque_arms.iter().map(|&(bci, _)| bci).collect();
        let headers_only = true;
        // Round 15 wave 6 (lane irosr): recompute an unnamed hoisted invariant
        // in the stub rather than refuse the entry (`osr_entry_remat_program`).
        let remat_on = ir_osr_remat_invariants_enabled();
        // Round 16 (lane osrdoor): the values an entry owes no seed although
        // their interval covers it (`osr_entry_value_needs_no_seed`).
        let excuse_tokens = ir_osr_excuse_memory_tokens_enabled();
        let dataflow_live_in = ir_osr_dataflow_live_in_enabled();
        // Interpreter round i2 wave 58 (lane L2): a value read past the entry
        // only as a memory token (`osr_entry_value_read_only_as_memory_order`).
        let order_uses = true;
        // JIT round 21 wave 17 (lane osrrefs, OD16-2): an unnamed reference
        // not live into the entry block, written null by the stub
        // (`osr_entry_ref_needs_no_seed`).
        let dataflow_refs = ir_osr_dataflow_live_in_refs_enabled();
        // Round 16 (lane matrix, MX16-1): re-execute, in the stub, the hoisted
        // array reads and range predicates an entry jumps past
        // (`osr_entry_recheck_program`). Empty with the switch off.
        let recheck = crate::ir_optimize::osr_remat_array_load_enabled();
        // JIT round 25 wave 9 (lane W9-5,
        // `CRATONVM_JIT_IR_OSR_RECHECK_CLASS_PREDICATES`, default ON since j27): and the
        // loop CLASS predicates (`op_guard_is_class_predicate`), so an entry
        // past one -- the inner-header entry W8-7 refused -- re-tests the
        // seeded receiver instead of being refused.
        let recheck_class = recheck;
        let recheck_nodes: Vec<(NodeId, usize)> = if recheck {
            self.osr_recheck_nodes(recheck_class)
        } else {
            Vec::new()
        };
        let recheck_reads: std::collections::HashSet<NodeId> = recheck_nodes
            .iter()
            .map(|&(id, _)| id)
            .filter(|&id| {
                matches!(
                    self.graph.nodes.get(id as usize).map(|n| &n.op),
                    Some(Op::ArrayLoad(_))
                )
            })
            .collect();
        // Round 13 wave 13 (lane jitfix): the deopt frames each entry must be
        // able to describe. Empty with the kill switch off, which admits as
        // before. See `osr_entry_skipped_frame_value`.
        let frame_sites: Vec<(usize, usize)> = if Self::osr_entry_frame_value_refusal_enabled() {
            self.osr_entry_frame_sites()
        } else {
            Vec::new()
        };
        // Planned first, emitted second: the plan reads `self` immutably and
        // the emission needs it mutably.
        // The trailing `usize` is the BLOCK this plan came from, kept only so
        // the one-entry-per-bci filter below can ask what kind of control node
        // the block starts with. It is dropped before emission.
        // A seed is `(local index, value, live at the entry)`. The last element
        // is what the stub recomputes (round 15 wave 6, lane irosr; empty
        // unless an unnamed invariant is live across the entry).
        #[allow(clippy::type_complexity)]
        let mut plans: Vec<(
            u32,
            usize,
            Vec<(usize, NodeId, bool)>,
            Vec<NodeId>,
            usize,
            OsrRemat,
        )> = Vec::new();
        let mut refusals: Vec<&'static str> = Vec::new();
        // ── Per-position counts, built once (round 11 wave 18, lane irback) ──
        //
        // The plan below asks three questions of every candidate block, and
        // each used to be a scan of the whole arena (`0..live.range.len()`):
        // "is any unproducible value live across this start and named by no
        // local", "which constants are live across it", "is any split-managed
        // value live across it". The builder records a snapshot at every
        // bytecode boundary, so nearly every block is a candidate, and the plan
        // cost O(blocks × nodes) three times over. "Live across `p`" is
        // `lo < p <= hi`, an interval stab, so a difference array answers the
        // two existence questions in O(1) per block (a count of the stabbing
        // unproducible values, compared with how many of them the snapshot
        // names), and the constants are drawn from a list prefiltered once.
        // Same answers, same order: the lists are built in ascending id order,
        // as the scans ran.
        let across_len = live
            .range
            .iter()
            .flatten()
            .map(|r| r.hi.saturating_add(2))
            .max()
            .unwrap_or(0);
        let mut unproducible_across: Vec<i64> = vec![0; across_len];
        let mut managed_across: Vec<i64> = vec![0; across_len];
        let mut producible_ranged: Vec<(NodeId, crate::regalloc::PosRange)> = Vec::new();
        for (id, r) in live.range.iter().enumerate() {
            let Some(r) = *r else {
                continue;
            };
            let can_produce = self
                .graph
                .nodes
                .get(id)
                .is_some_and(|n| Self::osr_entry_can_produce(&n.op));
            if can_produce {
                // Cast: an index into the node arena is a `NodeId`.
                producible_ranged.push((id as NodeId, r));
            }
            // `(lo, hi]`: empty when `lo >= hi`.
            if r.lo >= r.hi {
                continue;
            }
            let (start, end) = (r.lo + 1, r.hi + 1);
            if !can_produce {
                unproducible_across[start] += 1;
                unproducible_across[end] -= 1;
            }
            if self.ls_managed.get(id).copied().unwrap_or(false) {
                managed_across[start] += 1;
                managed_across[end] -= 1;
            }
        }
        for counts in [&mut unproducible_across, &mut managed_across] {
            let mut acc = 0i64;
            for c in counts.iter_mut() {
                acc += *c;
                *c = acc;
            }
        }
        // How many values of each kind are live across position `p`.
        let stabbing = |counts: &[i64], p: usize| counts.get(p).copied().unwrap_or(0);
        // Round 12 wave 3 (lane replay; the defence-in-depth patch of
        // `r12w2-iropt-osr-entry-skips-range-predicate`): the blocks an entry
        // would reach PAST an `Op::Guard` planted at the block's own bci in a
        // block that dominates it. That is `ir_optimize::install_range_predicates`'
        // pre-header guard (`n <= a.length`), whose fact licenses
        // `ir_check_elim`'s bounds-check elision inside the loop; an entry at
        // the header never runs it, and the predicate produces no value the
        // loop reads, so the liveness test below cannot see it. A builder
        // guard at a header bci sits in the header block itself (`bi ==
        // g_block`), and a refusal is always sound.
        // `CRATONVM_JIT_IR_OSR_PREDICATE_REFUSAL=0` disables it.
        let mut predicate_headers: std::collections::HashSet<usize> =
            std::collections::HashSet::new();
        if true {
            let nblocks = self.schedule.blocks.len();
            let guards: Vec<(usize, usize)> = self
                .graph
                .nodes
                .iter()
                .enumerate()
                .filter_map(|(id, n)| match n.op {
                    // A predicate the stub re-checks licenses nothing it skips
                    // (round 16, lane matrix, MX16-1).
                    Op::Guard { bci }
                        if recheck && self.op_guard_is_range_predicate(id as NodeId, bci) =>
                    {
                        None
                    }
                    // W9-5: a class predicate the stub re-checks likewise.
                    Op::Guard { bci }
                        if recheck_class && self.op_guard_is_class_predicate(id as NodeId, bci) =>
                    {
                        None
                    }
                    Op::Guard { bci } => self
                        .schedule
                        .node_to_block
                        .get(id)
                        .copied()
                        .filter(|&b| b < nblocks)
                        .map(|b| (bci, b)),
                    _ => None,
                })
                .collect();
            let dom = &self.schedule.dom;
            for bi in 0..nblocks {
                if guards.is_empty() {
                    break;
                }
                let Some(own) = self.osr_entry_block_bci(bi) else {
                    continue;
                };
                let skipped = guards
                    .iter()
                    .any(|&(gb, g_block)| gb == own && g_block != bi && dom.dominates(g_block, bi));
                if skipped && dom.is_reachable(bi) {
                    predicate_headers.insert(bi);
                }
            }
        }
        // JIT round 25 wave 8 (lane W8-7,
        // `docs/feature-designs/jit-osr-entry-metadata.md`,
        // `CRATONVM_JIT_IR_OSR_INNER_PREDICATE_REFUSAL`, default ON, `=0` is
        // the kill switch): the rule above keys on the guard's bci equalling
        // the ENTRY's, so an entry at an INNER loop header (another bci)
        // jumped past an outer loop's pre-header guard unrefused, while the
        // body inside the outer loop still read what that guard licensed --
        // a loop class predicate's folded `ExactClassIs` tests, a null-base /
        // mask / length fact, a W7-D field-class read, or a range fact with
        // the MX16-1 re-check off -- for a receiver the stub seeded from the
        // interpreter's local, which no compiled code ever tested. Refusing
        // an entry is always sound.
        let mut enclosing_predicate_blocks: std::collections::HashSet<usize> =
            std::collections::HashSet::new();
        if true && ir_osr_inner_predicate_refusal_enabled() {
            let nblocks = self.schedule.blocks.len();
            let guards: Vec<(usize, usize)> = self
                .graph
                .nodes
                .iter()
                .enumerate()
                .filter_map(|(id, n)| match n.op {
                    // Re-executed by the MX16-1 stub at every dominated entry.
                    Op::Guard { bci }
                        if recheck && self.op_guard_is_range_predicate(id as NodeId, bci) =>
                    {
                        None
                    }
                    // W9-5: a class predicate the stub re-checks likewise.
                    Op::Guard { bci }
                        if recheck_class && self.op_guard_is_class_predicate(id as NodeId, bci) =>
                    {
                        None
                    }
                    Op::Guard { bci } => self
                        .schedule
                        .node_to_block
                        .get(id)
                        .copied()
                        .filter(|&b| b < nblocks)
                        .map(|b| (bci, b)),
                    _ => None,
                })
                .collect();
            enclosing_predicate_blocks =
                osr_entries_inside_a_predicated_loop(self.schedule, &guards, &|b| {
                    self.osr_entry_block_bci(b)
                });
        }
        // Keyed by BLOCK, not by bci. Two reasons, and the first is fatal on
        // its own: `bci_native` anchors DATA nodes, and a loop header's bci is
        // an `iload` of a phi, which lowers to no node at all — eleven of the
        // fifteen snapshots in a counted loop had no offset under it. The
        // second is that a block boundary is the only place entering MEANS
        // anything: the phi copies run on the predecessors' edges, so at a
        // header's first instruction the phis are exactly the values a seeding
        // can supply, and nothing else is half-computed.
        for bi in 0..self.schedule.blocks.len() {
            // The block's bci is its CONTROL node's, not its first data node's.
            // A loop header's `Merge` carries the header bci (4 in a javac
            // counted loop); its first scheduled data node is whatever the
            // scheduler put there, which in that same loop is the `iinc` at 13.
            // Keying on the data node found the bci-13 snapshot, where the
            // header's `s` phi has already been superseded by the add and so is
            // live-across-but-unnamed — a refusal for entirely the wrong
            // reason.
            if opaque_blocks.contains(&bi) {
                continue;
            }
            let ctrl = self.schedule.blocks[bi].ctrl;
            let Some(bci) = self
                .graph
                .nodes
                .get(ctrl as usize)
                .and_then(|n| n.bytecode_pc)
            else {
                refusals.push("block control carries no bci");
                continue;
            };
            // An opaque header is entered through its arm, planned below; its
            // own block and the merge in front of it are never entry points.
            if opaque_bcis.contains(&bci) {
                continue;
            }
            // The door enters where a backward branch of THIS method's own code
            // landed; a spliced callee's pc (at or past its code length) is
            // never one, and `field_proof_entry_blocks` counts on this refusal.
            if self.pc_is_in_a_spliced_body(bci) {
                refusals.push("a spliced callee's pc: the door never asks for this bci");
                continue;
            }
            if headers_only && !self.osr_entry_block_is_back_edge_target(bi) {
                refusals.push("no backward edge lands here: the door never asks for this bci");
                continue;
            }
            if self.osr_entry_closed_by_a_handler(bci) {
                refusals.push("a handler reaches this bci and the body has a rethrow pad");
                continue;
            }
            if predicate_headers.contains(&bi) {
                refusals
                    .push("a pre-header range-check predicate licenses this loop's elided checks");
                continue;
            }
            if enclosing_predicate_blocks.contains(&bi) {
                refusals.push(
                    "an enclosing loop's pre-header predicate licenses facts this entry skips",
                );
                continue;
            }
            let Some(sp) = self
                .safepoint_first_by_bci
                .get(&bci)
                .and_then(|&idx| self.graph.safepoints.get(idx))
            else {
                refusals.push("no safepoint snapshot at this block start");
                continue;
            };
            // An inner region `ir_optimize::elide_nested_monitors` deleted is
            // held here by the INTERPRETER (it ran the inner `monitorenter`),
            // and the body has no exit for it. `osr_entry.rs` refuses such an
            // entry only when a deopt point describes this bci; a header bci
            // often has none and the contract falls back to register homes.
            // Refuse it here, where the snapshot is (round 11 wave 16, lane
            // `replay`).
            if sp.monitor_elided_mask != 0 {
                refusals.push("an elided nested monitor is held at this bci");
                continue;
            }
            let native = self.block_offsets[bi];
            // A javac loop header has an empty operand stack. A bci that does
            // not is refused rather than reasoned about: the interpreter's
            // stack would have to be seeded too, and the snapshot's stack slots
            // are not JVM-local-indexed.
            if !sp.stack.is_empty() {
                refusals.push("operand stack not empty at this bci");
                continue;
            }
            // Where this block begins, in the liveness model's own positions.
            let Some(&(entry_pos, _)) = live.span.get(bi) else {
                refusals.push("no liveness span for this block");
                continue;
            };
            let named: std::collections::HashSet<NodeId> = sp
                .locals
                .iter()
                .copied()
                .filter(|v| *v != NO_NODE)
                .collect();
            // Live across this block's start, in the liveness model's own
            // positions: defined before it, and read at or after it.
            let live_across = |id: usize| {
                live.range
                    .get(id)
                    .copied()
                    .flatten()
                    .is_some_and(|r| r.lo < entry_pos && entry_pos <= r.hi)
            };
            // Live-in, and not something the entered code can produce for
            // itself.
            //
            // A CONSTANT is the one exception, and it is an exception because
            // THE STUB PRODUCES IT -- see `const_seeds` below. It did not
            // always: this clause used to excuse constants on the grounds that
            // "every reader materialises it as an immediate
            // (`ir_const_imm_enabled`)", and that is not true of every reader.
            // `emit_phi_copies` and the merge stores read a value from its HOME
            // WORD, and a constant's home word is written once, by its own
            // definition, in a block this stub jumps past.
            //
            // `probes/Min0.java` is the shape: a three-armed clamp inside a
            // counted loop, whose two literal arms (100 and 900) reach a phi.
            // Entered through this door, `r = 100` read `[rbp-0D8h]` -- a frame
            // word nothing had written -- and the program returned a wrong,
            // run-to-run VARYING sum from deterministic input, which is the
            // signature of exactly that. Excusing a constant is right; excusing
            // it without emitting it was the defect.
            //
            // Some unproducible value is live across and not named ⟺ more
            // unproducible values stab `entry_pos` than the snapshot names
            // (`named` is a set, so each is counted once).
            let named_unproducible_across = named
                .iter()
                .filter(|&&v| {
                    let id = v as usize;
                    !self
                        .graph
                        .nodes
                        .get(id)
                        .is_some_and(|n| Self::osr_entry_can_produce(&n.op))
                        && live_across(id)
                })
                .count();
            // Cast: a count of arena nodes fits i64.
            let unseedable = stabbing(unproducible_across.as_slice(), entry_pos)
                > named_unproducible_across as i64;
            // Round 15 wave 6 (lane irosr): what the stub recomputes. Empty
            // unless `unseedable`; see `osr_entry_remat_program`.
            let mut remat = OsrRemat::default();
            // The hoisted reads live across this entry that the re-check
            // program, not `osr_entry_remat_program`, writes (MX16-1).
            let mut recheck_roots: Vec<NodeId> = Vec::new();
            if unseedable {
                // The unnamed values themselves, in ascending id order.
                let mut unnamed: Vec<NodeId> = Vec::new();
                for (id, r) in live.range.iter().enumerate() {
                    if self
                        .graph
                        .nodes
                        .get(id)
                        .is_some_and(|n| Self::osr_entry_can_produce(&n.op))
                    {
                        continue;
                    }
                    // Cast: an index into the node arena is a `NodeId`.
                    if r.is_some_and(|r| {
                        r.lo < entry_pos && entry_pos <= r.hi && !named.contains(&(id as NodeId))
                    }) {
                        unnamed.push(id as NodeId);
                    }
                }
                let dbg = false;
                // Round 16 (lane osrdoor): drop the ones this entry owes no
                // seed. Both switches off: nothing is dropped.
                if excuse_tokens || dataflow_live_in {
                    unnamed.retain(|&id| {
                        let excused = self.osr_entry_value_needs_no_seed(
                            id,
                            bi,
                            live,
                            excuse_tokens,
                            dataflow_live_in,
                        );
                        if excused && dbg {
                            eprintln!(
                                "[ir-ls]   osr block {bi} (bci {bci}): n{id} {:?} : {:?} needs no \
                                 seed (no machine value, or not live into the block)",
                                self.graph.nodes.get(id as usize).map(|n| &n.op),
                                self.graph.nodes.get(id as usize).map(|n| n.ty),
                            );
                        }
                        !excused
                    });
                }
                if order_uses {
                    unnamed.retain(|&id| {
                        let excused = Self::osr_entry_value_read_only_as_memory_order(
                            &self.graph,
                            &self.schedule,
                            id,
                            bi,
                        );
                        if excused {
                            note_osr_entry_census("excused:memory-order-uses");
                            if dbg {
                                eprintln!(
                                    "[ir-ls]   osr block {bi} (bci {bci}): n{id} {:?} : {:?} needs no \
                                     seed (read past the entry only as a memory token)",
                                    self.graph.nodes.get(id as usize).map(|n| &n.op),
                                    self.graph.nodes.get(id as usize).map(|n| n.ty),
                                );
                            }
                        }
                        !excused
                    });
                }
                // JIT round 21 wave 17 (lane osrrefs, OD16-2): the references
                // the dataflow calls dead into this block, nulled by the stub.
                let mut null_refs: Vec<NodeId> = Vec::new();
                if dataflow_refs {
                    unnamed.retain(|&id| {
                        let excused = self.osr_entry_ref_needs_no_seed(id, bi, live);
                        if excused {
                            null_refs.push(id);
                            note_osr_entry_census("excused:dataflow-live-in-refs");
                            if dbg {
                                eprintln!(
                                    "[ir-ls]   osr block {bi} (bci {bci}): n{id} {:?} : Ref needs no \
                                     seed (not live into the block) -- nulled in the stub",
                                    self.graph.nodes.get(id as usize).map(|n| &n.op),
                                );
                            }
                        }
                        !excused
                    });
                }
                if recheck {
                    unnamed.retain(|&id| {
                        if recheck_reads.contains(&id) {
                            recheck_roots.push(id);
                            false
                        } else {
                            true
                        }
                    });
                }
                let program = if unnamed.is_empty()
                    && (!recheck_roots.is_empty()
                        || !null_refs.is_empty()
                        || excuse_tokens
                        || dataflow_live_in
                        || order_uses)
                {
                    // Every unnamed value was excused: nothing to recompute.
                    Some(OsrRemat::default())
                } else if remat_on {
                    self.osr_entry_remat_program(bi, &sp.locals, &unnamed)
                } else {
                    None
                };
                match program {
                    Some(program) => {
                        if dbg {
                            for &id in &program.roots {
                                eprintln!(
                                    "[ir-ls]   osr block {bi} (bci {bci}): n{id} {:?} is live \
                                     across it and no local names it -- recomputed in the stub",
                                    self.graph.nodes.get(id as usize).map(|n| &n.op),
                                );
                            }
                        }
                        remat = program;
                        remat.null_refs = null_refs;
                    }
                    None => {
                        if dbg {
                            for &id in &unnamed {
                                // The type too (round 16, lane osrdoor): a
                                // bare `Some(Phi)` hid that most of these
                                // were memory φs.
                                eprintln!(
                                    "[ir-ls]   osr block {bi} (bci {bci}) refused: n{id} {:?} is live \
                                     across it and no local names it (type {:?})",
                                    self.graph.nodes.get(id as usize).map(|n| &n.op),
                                    self.graph.nodes.get(id as usize).map(|n| n.ty),
                                );
                            }
                        }
                        // OD16-4 (round 16, lane osrdoor): which shapes, run-wide.
                        for &id in &unnamed {
                            if let Some(n) = self.graph.nodes.get(id as usize) {
                                note_osr_entry_census(&osr_entry_census_shape_key(&n.op, n.ty));
                            }
                        }
                        // Round 16 wave 14 (lane osrir): and which opt-in
                        // excuse would have passed this check.
                        if let Some(key) = self.osr_entry_would_admit_key(
                            bi,
                            &sp.locals,
                            &unnamed,
                            live,
                            excuse_tokens,
                            dataflow_live_in,
                            order_uses,
                            remat_on,
                        ) {
                            note_osr_entry_census(key);
                        }
                        // t1 wave 8 (lane OSRREFS, OSRREFS21-1/-2): and the
                        // population the reference half (off here: `=0`) would open --
                        // alone, or paired with the primitive half. Later
                        // checks (deopt frames, split values) can still
                        // refuse such an entry.
                        if !dataflow_refs {
                            if let Some(key) = self.osr_entry_would_admit_refs_key(
                                bi,
                                &unnamed,
                                live,
                                dataflow_live_in,
                            ) {
                                note_osr_entry_census(key);
                            }
                        }
                        refusals.push("a value live here is named by no local");
                        continue;
                    }
                }
            }
            // Round 9 wave 6 (irlower6): a SPLIT-managed value live across the
            // entry cannot be seeded. The seeding below publishes into
            // `assigned_gpr(v)`, which for a managed value is wherever its
            // LAST transition in emission order left it -- not the register
            // the entered block's code was emitted against, which is the
            // state carried out of that block's layout predecessor. Only
            // reachable under `CRATONVM_JIT_IR_LS_SPLITS`; refusing the entry
            // is always correct (the interpreter keeps running its frame).
            if stabbing(managed_across.as_slice(), entry_pos) > 0 {
                refusals.push("a split-managed value is live here");
                continue;
            }
            // The constants whose definition this stub jumps past and whose
            // home word a reader at or after the entry still loads.
            //
            // Restricted to those LIVE ACROSS the entry, and that is not
            // tightening for its own sake: spill slots are COLOURED
            // (`slot_plan.node_color`), so a constant that is NOT live here may
            // share its word with a value that is, and writing it would clobber
            // a seed. Live-across is exactly the set whose colour is exclusive
            // at this point.
            let const_seeds: Vec<NodeId> = producible_ranged
                .iter()
                .filter(|(_, r)| r.lo < entry_pos && entry_pos <= r.hi)
                .map(|&(id, _)| id)
                .collect();
            let mut seeds: Vec<(usize, NodeId, bool)> = Vec::new();
            let mut homeless = false;
            for (i, &v) in sp.locals.iter().enumerate() {
                if v == NO_NODE {
                    continue;
                }
                // CG16P-4: a dropped constant is folded by every reader, so
                // the entry has nothing to seed for it.
                if self.is_dropped_const(v) {
                    continue;
                }
                let has_slot = self.node_slot.get(v as usize).copied().flatten().is_some()
                    && !self.home_dropped.get(v as usize).copied().unwrap_or(false);
                if !has_slot && self.assigned_gpr(v).is_none() {
                    homeless = true;
                    break;
                }
                // LIVE AT the entry, inclusive of a range that starts exactly
                // there (a header phi): the partition below only has to keep a
                // dead local's register write ahead of every live one's.
                let live_here = live
                    .range
                    .get(v as usize)
                    .copied()
                    .flatten()
                    .is_some_and(|r| r.lo <= entry_pos && entry_pos <= r.hi);
                seeds.push((i, v, live_here));
            }
            if homeless {
                refusals.push("a named local has nowhere to be seeded");
                continue;
            }
            // Round 16 (lane matrix, MX16-1): the hoisted reads and range
            // predicates in the blocks this stub jumps past, re-executed after
            // the seeding; any failure leaves for the interpreter here.
            if recheck {
                match self.osr_entry_recheck_program(
                    bi,
                    bci,
                    &sp.locals,
                    &recheck_nodes,
                    &recheck_roots,
                ) {
                    Ok(Some((checked, leaves, frame))) => {
                        remat.checked = checked;
                        remat.leaf_locals.extend(leaves);
                        remat.leaf_locals.sort_unstable();
                        remat.leaf_locals.dedup();
                        remat.roots.extend(recheck_roots.iter().copied());
                        remat.fail_frame = Some(frame);
                    }
                    Ok(None) => {}
                    Err(why) => {
                        refusals.push(why);
                        continue;
                    }
                }
            }
            // Item 4 residual 2: the field proofs a site read across this
            // entry from a guard it jumps past, re-checked after the seeding
            // (and after MX16-1's program, sharing its failure exit).
            match self.osr_entry_field_proof_checks(bi, &sp.locals) {
                Ok(Some((checks, leaves))) => {
                    if remat.fail_frame.is_none() {
                        match self.osr_entry_recheck_fail_frame(bci) {
                            Ok(frame) => remat.fail_frame = Some(frame),
                            Err(why) => {
                                refusals.push(why);
                                continue;
                            }
                        }
                    }
                    remat.checked.extend(checks);
                    remat.leaf_locals.extend(leaves);
                    remat.leaf_locals.sort_unstable();
                    remat.leaf_locals.dedup();
                    self.proven_field_census.osr_entry_rechecks += 1;
                }
                Ok(None) => {}
                Err(why) => {
                    refusals.push(why);
                    continue;
                }
            }
            // Round 13 wave 13 (lane jitfix): the admission above counts data
            // uses only. A value a deopt frame past this entry names, defined in
            // a block the stub jumps past and not seeded from `named`, would be
            // read from a word nothing wrote.
            //
            // A recomputed value is written exactly as a seed is (home word and
            // assigned register), so a frame may name it like one (round 15
            // wave 6, lane irosr).
            let written: std::collections::HashSet<NodeId> =
                named.iter().chain(remat.roots.iter()).copied().collect();
            if let Some(v) = Self::osr_entry_skipped_frame_value(
                self.graph,
                self.schedule,
                self.sr_map,
                &frame_sites,
                bi,
                bi,
                false,
                &written,
            ) {
                if false {
                    eprintln!(
                        "[ir-ls]   osr block {bi} (bci {bci}) refused: a deopt frame past it names \
                         n{v} {:?} (type {:?}, block {:?}), which the entry skips",
                        self.graph.nodes.get(v as usize).map(|n| &n.op),
                        self.graph.nodes.get(v as usize).map(|n| n.ty),
                        self.schedule.node_to_block.get(v as usize),
                    );
                }
                refusals.push("a deopt frame past this entry names a value the entry skips");
                continue;
            }
            // Cast: a bci fits u32 (`IR_MAX_BYTECODE_SIZE` is far below it).
            plans.push((bci as u32, native, seeds, const_seeds, bi, remat));
        }

        // ── One entry per bci, or none ───────────────────────────────
        //
        // `CompiledMethod::ir_osr_entry_addr` resolves a bci by `.find()`, so
        // the FIRST plan at a bci wins and any others are unreachable — but
        // "first" is block order, which is not a property the interpreter
        // knows anything about. A bci claimed by two blocks is a resume point
        // that is not a function, and entering the wrong one runs a prefix of
        // the loop body that the interpreter has already run.
        //
        // Nothing could claim a bci twice until a transform CLONED a control
        // node: the partial unroller gives every copy its own `If`/`Proj`
        // triple at the loop test's own bci (it has to — the pc is also the
        // site key), so a four-way unroll offers four blocks at the header bci
        // and three of them are mid-group. That was measured as a wrong answer:
        // `C2PartialUnrollProbe` returned `sum(trip + factor - 1)` on roughly
        // 40% of runs, and vanished with `CRATONVM_JIT_IR_OSR_ENTRY=0`.
        //
        // The tie-break is not "take the first" but "take the LOOP HEADER":
        // an interpreter arriving at a loop's bci is at that loop's header, and
        // a header is a `Merge`/`Region` while every copy's block is a `Proj`.
        // Where that does not single one block out, the bci is refused
        // entirely — no OSR entry is always correct, and the interpreter simply
        // keeps running the frame it is in.
        {
            let mut claims: std::collections::HashMap<u32, usize> =
                std::collections::HashMap::new();
            for (bci, _, _, _, _, _) in &plans {
                *claims.entry(*bci).or_insert(0) += 1;
            }
            if claims.values().any(|&n| n > 1) {
                let is_header = |bi: usize| {
                    let ctrl = self.schedule.blocks[bi].ctrl;
                    matches!(
                        self.graph.nodes.get(ctrl as usize).map(|n| &n.op),
                        Some(Op::Merge) | Some(Op::Region)
                    )
                };
                let mut headers: std::collections::HashMap<u32, usize> =
                    std::collections::HashMap::new();
                for (bci, _, _, _, bi, _) in &plans {
                    if is_header(*bi) {
                        *headers.entry(*bci).or_insert(0) += 1;
                    }
                }
                plans.retain(|(bci, _, _, _, bi, _)| {
                    if claims.get(bci).copied().unwrap_or(0) <= 1 {
                        return true;
                    }
                    headers.get(bci).copied() == Some(1) && is_header(*bi)
                });
                refusals.push("bci claimed by more than one block");
            }
        }

        // ── The opaque arms (round 11 wave 14, lane osrmerge) ────────────
        //
        // `(bci, native, const seeds, locals needed)`. The admission is the
        // arm's DATAFLOW live-in set, not the linear `live_across` above: the
        // merge's φs are written by the arm's own edge copies, and a value the
        // layout happens to place across the arm (an entry-edge source, a φ
        // whose range reaches back to its other edge) is on no path from it.
        // What IS live into the arm must be produced by the stub, so only a
        // constant is admitted; anything else — an outer loop's hoisted
        // invariant, a value GVN merged across the `If` — refuses the entry.
        #[allow(clippy::type_complexity)]
        let mut opaque_plans: Vec<(u32, usize, Vec<NodeId>, u32, Box<[u8]>, OsrRemat)> = Vec::new();
        // Every arm's `Op::OsrLocal`s `(local, type)`, keyed by the arm's
        // control node, in arena order: one pass over the arena for all arms
        // (round 12 wave 1, lane osr). Each arm used to scan the whole arena
        // twice, once for its locals bound and once for their kinds, which is
        // O(headers × nodes) with every loop header given an arm.
        let mut osr_locals_by_arm: std::collections::HashMap<NodeId, Vec<(u16, IrType)>> =
            std::collections::HashMap::new();
        if !opaque_arms.is_empty() {
            for n in &self.graph.nodes {
                if let Op::OsrLocal(i) = n.op {
                    if let Some(&c) = n.inputs.first() {
                        osr_locals_by_arm.entry(c).or_default().push((i, n.ty));
                    }
                }
            }
        }
        for &(bci, bi) in &opaque_arms {
            if opaque_arms.iter().filter(|&&(b, _)| b == bci).count() > 1 {
                refusals.push("bci claimed by more than one opaque arm");
                continue;
            }
            let Ok(bci32) = u32::try_from(bci) else {
                refusals.push("an opaque arm's bci does not fit u32");
                continue;
            };
            // The same elided-nested-monitor refusal as the plain entries
            // above (round 11 wave 16, lane `replay`).
            if self
                .safepoint_first_by_bci
                .get(&bci)
                .and_then(|&idx| self.graph.safepoints.get(idx))
                .is_some_and(|sp| sp.monitor_elided_mask != 0)
            {
                refusals.push("an elided nested monitor is held at this bci");
                continue;
            }
            let (Some(&native), Some(block), Some(&(entry_pos, _))) = (
                self.block_offsets.get(bi),
                self.schedule.blocks.get(bi),
                live.span.get(bi),
            ) else {
                refusals.push("no liveness span for this block");
                continue;
            };
            let arm_ctrl = block.ctrl;
            let mut const_seeds: Vec<NodeId> = Vec::new();
            let mut unnamed: Vec<NodeId> = Vec::new();
            let mut unseedable = false;
            for id in 0..live.range.len() {
                // Cast: an index into the node arena is a `NodeId`.
                match live.live_in_at(bi, id as NodeId) {
                    None => {
                        unseedable = true;
                        break;
                    }
                    Some(false) => {}
                    Some(true) => {
                        if self
                            .graph
                            .nodes
                            .get(id)
                            .is_some_and(|n| Self::osr_entry_can_produce(&n.op))
                        {
                            // Cast: an index into the node arena is a `NodeId`.
                            const_seeds.push(id as NodeId);
                        } else {
                            unnamed.push(id as NodeId);
                        }
                    }
                }
            }
            let mut remat = OsrRemat::default();
            let mut remat_sp_locals: Option<&[NodeId]> = None;
            if !unnamed.is_empty() && !unseedable {
                let sp_locals = remat_on
                    .then(|| {
                        self.safepoint_first_by_bci
                            .get(&bci)
                            .and_then(|&idx| self.graph.safepoints.get(idx))
                    })
                    .flatten()
                    .map(|sp| sp.locals.as_slice());
                if let (Some(locals), Some(r)) = (
                    sp_locals,
                    sp_locals.and_then(|locals| self.osr_entry_remat_program(bi, locals, &unnamed)),
                ) {
                    remat = r;
                    remat_sp_locals = Some(locals);
                } else {
                    if cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_IR_LINEAR_SCAN") {
                        for &id in &unnamed {
                            eprintln!(
                                "[ir-ls]   osr opaque arm {bi} (bci {bci}) refused: n{id} {:?} (type {:?}, block {:?}) live-in",
                                self.graph.nodes.get(id as usize).map(|n| &n.op),
                                self.graph.nodes.get(id as usize).map(|n| n.ty),
                                self.schedule.node_to_block.get(id as usize),
                            );
                        }
                    }
                    unseedable = true;
                }
            }
            if unseedable {
                refusals.push("a value live into the opaque OSR arm is not a constant");
                continue;
            }
            // Round 13 wave 13 (lane jitfix): the same frame-use check as an
            // ordinary entry's. The arm seeds nothing; its edge copies write
            // the merge's φs, so what runs on its path is the arm and every
            // block that merge dominates. An arm with any other shape keeps
            // the arm alone as that root, which only refuses more.
            let arm_root = match block.successors.as_slice() {
                [merge] => *merge,
                _ => bi,
            };
            if let Some(v) = Self::osr_entry_skipped_frame_value(
                self.graph,
                self.schedule,
                self.sr_map,
                &frame_sites,
                bi,
                arm_root,
                true,
                &std::collections::HashSet::new(),
            ) {
                if false {
                    eprintln!(
                        "[ir-ls]   osr opaque arm {bi} (bci {bci}) refused: a deopt frame past it \
                         names n{v} {:?} (type {:?}, block {:?}), which the arm skips",
                        self.graph.nodes.get(v as usize).map(|n| &n.op),
                        self.graph.nodes.get(v as usize).map(|n| n.ty),
                        self.schedule.node_to_block.get(v as usize),
                    );
                }
                refusals.push("a deopt frame past this entry names a value the entry skips");
                continue;
            }
            // Same reason as for an ordinary entry: a split-managed value's
            // register at this block start is its layout predecessor's.
            if stabbing(managed_across.as_slice(), entry_pos) > 0 {
                refusals.push("a split-managed value is live here");
                continue;
            }
            let arm_locals: &[(u16, IrType)] = osr_locals_by_arm
                .get(&arm_ctrl)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            // The highest JVM local the arm reads, plus one: what
            // `ir_osr_enter` bounds the interpreter's slice by.
            let remat_needed = remat
                .leaf_locals
                .iter()
                .map(|&i| u32::try_from(i + 1).unwrap_or(u32::MAX))
                .max()
                .unwrap_or(0);
            let needed = arm_locals
                .iter()
                .map(|&(i, _)| u32::from(i) + 1)
                .max()
                .unwrap_or(0)
                .max(remat_needed);
            // The kind each of those locals is read as (wave 10): the type of
            // its `Op::OsrLocal`. See `CompiledMethod::ir_osr_entry_kinds`.
            let mut kinds = vec![0u8; needed as usize];
            for &(i, ty) in arm_locals {
                if let Some(k) = kinds.get_mut(usize::from(i)) {
                    *k = crate::ir_osr_slot_kind(ty);
                }
            }
            if let Some(locals) = remat_sp_locals {
                for &i in &remat.leaf_locals {
                    if let (Some(k), Some(&v)) = (kinds.get_mut(i), locals.get(i)) {
                        if *k == 0 {
                            if let Some(ty) = self.graph.nodes.get(v as usize).map(|n| n.ty) {
                                *k = crate::ir_osr_slot_kind(ty);
                            }
                        }
                    }
                }
            }
            if self
                .field_proof_osr_obligations
                .borrow()
                .iter()
                .any(|&(e, _, _)| e == bi)
            {
                if remat.fail_frame.is_none() {
                    match self.osr_entry_recheck_fail_frame(bci) {
                        Ok(frame) => remat.fail_frame = Some(frame),
                        Err(why) => {
                            refusals.push(why);
                            continue;
                        }
                    }
                }
                remat.checked.push(OsrCheckInsn::LayoutEpoch);
                self.proven_field_census.osr_entry_rechecks += 1;
            }
            opaque_plans.push((
                bci32,
                native,
                const_seeds,
                needed,
                kinds.into_boxed_slice(),
                remat,
            ));
        }

        for why in refusals {
            self.note_osr_refusal(why);
        }
        // Emitted entries that are not opaque; see this function's doc.
        let mut non_opaque = 0usize;
        // One emission loop for both kinds. `opaque_needed` is `Some` for an
        // opaque arm: no seeds (the arm's edge copies are the seeding), the
        // locals pointer LEFT in the scratch word for the arm's `OsrLocal`s,
        // and the arm's own locals bound. `None` is the ordinary entry, whose
        // bytes are exactly what they were before wave 14.
        let all_plans = plans
            .into_iter()
            .map(|(bci, native, seeds, const_seeds, _bi, remat)| {
                (
                    bci,
                    native,
                    seeds,
                    const_seeds,
                    None::<u32>,
                    None::<Box<[u8]>>,
                    remat,
                )
            })
            .chain(opaque_plans.into_iter().map(
                |(bci, native, const_seeds, needed, kinds, remat)| {
                    (
                        bci,
                        native,
                        Vec::new(),
                        const_seeds,
                        Some(needed),
                        Some(kinds),
                        remat,
                    )
                },
            ));
        for (bci, native, seeds, const_seeds, opaque_needed, opaque_kinds, remat) in all_plans {
            // JIT round 25 wave 3 (`CRATONVM_JIT_IR_SNAPSHOT_RANGE_COLOURS`):
            // the frame states this stub builds read ranged homes at this bci,
            // and a seed writes one only where the plan made the value live
            // at the block start (else it could overwrite another value of
            // the shared colour). Cleared by the caller after the last stub.
            self.snapshot_range_osr_bci.set(Some(bci as usize));
            if let Some(sr) = self.slot_plan.snapshot_ranged.as_deref() {
                for &(_, v, _) in &seeds {
                    let ranged_here = sr
                        .entry_bcis
                        .get(&v)
                        .map(|bcis| bcis.binary_search(&(bci as usize)).is_ok());
                    if ranged_here == Some(false) {
                        self.latch_bailout(Bailout::with_context(
                            BailoutReason::UnallocatedValue { node: v },
                            format!(
                                "n{v} is snapshot-ranged and an OSR entry at bci {bci} seeds it \
                                 where the slot plan did not make it live"
                            ),
                        ));
                    }
                }
            }
            // Cast: a JVM local index plus one; `max_locals` is u16.
            let seeds_hi = seeds
                .iter()
                .map(|(i, _, _)| *i as u32 + 1)
                .max()
                .unwrap_or(0);
            // The kind each seeded local is read as (wave 10): its value's IR
            // type. A reference is a constraint dead or alive, because its
            // word lands in a home the oop maps publish as a root. A dead
            // primitive is not: its slot may legitimately hold another type
            // (javac reuses a slot whose verifier type at the header is top,
            // and the φ then takes `PHI_TYPE_FALLBACK`), and nothing reads it.
            // See `CompiledMethod::ir_osr_entry_kinds`.
            //
            // A local the stub's recomputation reads is read whatever the body
            // does with it afterwards, so its kind is a constraint too (round 15
            // wave 6, lane irosr).
            let kinds = opaque_kinds.unwrap_or_else(|| {
                let mut kinds = vec![0u8; seeds_hi as usize];
                for &(i, v, live_here) in &seeds {
                    let ty = self.graph.nodes.get(v as usize).map(|n| n.ty);
                    if let (Some(k), Some(ty)) = (kinds.get_mut(i), ty) {
                        let kind = crate::ir_osr_slot_kind(ty);
                        let read = live_here || remat.leaf_locals.contains(&i);
                        *k = if read || kind == b'L' { kind } else { 0 };
                    }
                }
                kinds.into_boxed_slice()
            });
            let stub = self.buf.pos();
            // push rbp ; mov rbp, rsp ; sub rsp, frame_size
            self.buf.emit_byte(0x55);
            self.buf.emit(&[0x48, 0x89, 0xE5]);
            self.buf.emit(&[0x48, 0x81, 0xEC]);
            self.buf.emit(&self.frame_size.to_le_bytes());
            // Round 16 wave 7 (lane frames): the fresh-frame clear, on the
            // prologue's terms (`emit_prologue`, gen r5w4/jit8). This entry
            // carves the same frame and never cleared it: every word the stub
            // and the loop do not write -- the deopt GPR block, unused save
            // slots, the staging region, spill colours not yet defined -- kept
            // the previous occupant's value for the whole OSR'd loop, rooted by
            // the band scan. Right after `SUB RSP`, before the saves, for the
            // prologue's reason; it writes XMM5, R10, R11 and the flags only,
            // and the stub's inputs arrive in `ENTRY_ABI_REGS[0..2]`. The
            // per-word zero stores below are kept: this is not the prologue's
            // zeroing stretch, so none is elided. Kill switch
            // `CRATONVM_JIT_IR_ZERO_FRESH_OSR_FRAMES=0`.
            if self.frame_size > 0 && self.frame_size % 8 == 0 {
                {
                    let _osr_skip = self.emit_fresh_frame_clear_sse(false);
                }
            }
            // The caller's registers, exactly as `emit_prologue` saves them —
            // every exit restores them, including the ones this stub's body
            // reaches.
            let xmm_saves: Vec<(u8, i32)> = self.saved_xmm_regs().collect();
            for (reg, off) in xmm_saves {
                self.emit_xmm_frame_move(reg, off, true);
            }
            let gpr_saves: Vec<(u8, i32)> = self.saved_gpr_regs().collect();
            for (reg, off) in gpr_saves {
                self.emit_gpr_frame_move(reg, off, true);
            }
            // Park the locals pointer before anything can call out.
            // `emit_frame_record`'s fallback path CALLs, and the second entry
            // ABI register is caller-saved on both ABIs. The phi-copy scratch
            // is the right parking space: it is a bookkeeping word no value is
            // ever coloured onto, and the body only ever WRITES it before
            // reading it, so a value left here is consumed and gone before the
            // first parallel copy runs.
            self.store_abi_reg(ENTRY_ABI_REGS[1], self.phi_copy_scratch_slot_off);
            if self.needs_context {
                self.store_abi_reg(ENTRY_ABI_REGS[0], self.context_slot_off);
            }
            // A clean frame, on the same terms the prologue establishes it —
            // and BEFORE the seeding, because `zero_ref_phi_slots` would
            // otherwise erase a reference this stub had just placed.
            //
            // A frame block names the reference-parameter homes, which this
            // entry (unlike the prologue) never writes from an argument, so
            // with a block the locals are zeroed whatever the switch says.
            let frame_block = !self.frame_block.is_empty();
            if !prologue_zero_unset_locals_disabled() || frame_block {
                for i in 0..self.num_locals {
                    // Cast: a JVM local index times 8; `max_locals` is u16.
                    self.emit_zero_frame_slot(((i as i32) + 1) * 8);
                }
            }
            if self.get_current_thread != 0 && self.shadow_thread_slot_off > 0 {
                self.emit_zero_frame_slot(self.shadow_thread_slot_off);
                self.emit_zero_frame_slot(self.shadow_savetop_slot_off);
                // The saved-base word is a frame block's "published" flag.
                if (!prologue_zero_reserved_tail_disabled() || frame_block)
                    && self.shadow_savebase_slot_off > 0
                {
                    self.emit_zero_frame_slot(self.shadow_savebase_slot_off);
                }
            }
            if self.sp_id_slot_off > 0 && Self::zero_sp_id_slot_enabled() {
                self.emit_zero_frame_slot(self.sp_id_slot_off);
            }
            self.emit_frame_record();
            self.fetch_current_thread();
            self.zero_ref_phi_slots();
            // The entry block does NOT run on this path, so its references
            // are as unwritten as any other block's: no refinement.
            self.zero_ref_value_slots(/* entry_runs */ false);
            // ── Re-materialise the constants the skipped blocks defined ──
            //
            // Byte-for-byte what `Op::Const` / `Op::ConstF` emit at their own
            // definition site: the home word, and for a float or double the
            // register copy `publish_fp_from_slot` makes. An INTEGER constant
            // publishes to no register at its definition, so neither does this
            // -- a reader only ever takes one from its home word.
            //
            // Before the live locals' seeding rather than after, and RAX-only,
            // so the two cannot interfere: the seeding parks the interpreter's
            // locals pointer in R11, which nothing here touches.
            //
            // ── ...and the DEAD locals go first of all (round 11, irlower) ──
            //
            // The snapshot names every local, live or not, and a local whose
            // last use is behind this entry has no claim on its REGISTER any
            // more: the allocator's ranges do not count frame-state uses, so
            // that register can belong to a value that IS live across the
            // entry. Seeded in local-index order, a dead local with the higher
            // index overwrote the live value's register after the live value
            // had been written there, and the entered body read the dead
            // value. Every register write for a dead local therefore happens
            // before any for a live one; the home words are pinned per value
            // (`plan_slots`), so their order never mattered. A dead local whose
            // register is its OWN (a deopt may name it there) still gets it.
            self.load_reg_from_frame(R11, self.phi_copy_scratch_slot_off);
            for &(i, v, live_here) in &seeds {
                if !live_here {
                    self.emit_osr_seed(i, v);
                }
            }
            for id in const_seeds {
                if self.is_dropped_const(id) {
                    continue;
                }
                let Some(off) = self.node_slot.get(id as usize).copied().flatten() else {
                    continue;
                };
                // Cast: a frame offset inside this method's own frame.
                let off = off.get() as i32;
                let (bits, is_fp, is_double) = match self.graph.nodes.get(id as usize) {
                    Some(n) => match n.op {
                        Op::Const(v) => (v, false, false),
                        // Cast: an IEEE-754 bit pattern into the imm64 encoder.
                        Op::ConstF(b) => (b as i64, true, n.ty == IrType::Double),
                        _ => continue,
                    },
                    None => continue,
                };
                self.emit_mov_rax_imm64(bits);
                self.store_abi_reg(RAX, off);
                // `publish_fp_from_slot`, not an open-coded load, because that
                // is the function `Op::ConstF`'s own definition calls -- and
                // because `assigned_xmm`'s contract is that only the publishing
                // sites read it.
                if is_fp {
                    self.publish_fp_from_slot(id, off, is_double);
                }
            }

            // ── Recompute the unnamed invariants (round 15 wave 6, lane irosr) ──
            //
            // After the dead locals, for the reason they go first: each value
            // recomputed here is LIVE across the entry, so its register is its
            // own at this point and a dead local's write must not land after
            // it. RAX/RCX/RDX and the stack only; R11 still holds the locals
            // pointer the program reads. See `osr_entry_remat_program`.
            for &insn in &remat.prog {
                self.emit_osr_remat_insn(insn);
            }

            // ── Null the dead unnamed references (JIT round 21, osrrefs) ──
            //
            // After the dead locals and the recomputation, for the reason the
            // live seeds go last: each is live across the entry by its
            // interval, so its colour and register are its own here. Before
            // the live seeds; RAX only. See `osr_entry_ref_needs_no_seed_by`.
            for &id in &remat.null_refs {
                self.emit_osr_null_ref(id);
            }

            // ── Seed the live locals, last ───────────────────────────
            for &(i, v, live_here) in &seeds {
                if live_here {
                    self.emit_osr_seed(i, v);
                }
            }
            // ── Re-execute what the skipped pre-headers did (MX16-1) ──
            //
            // After every seed: a failure leaves through an `OsrExit` point
            // whose frame is the header snapshot, which names the seeded
            // values. Before the scratch word is cleared: the program reads
            // locals through R11, reloaded from it.
            if !remat.checked.is_empty() && !self.emit_osr_entry_rechecks(bci as usize, &remat) {
                return non_opaque.max(1);
            }
            // The scratch held a pointer; leave it as the prologue would. NOT
            // for an opaque arm: its `Op::OsrLocal`s read the pointer from
            // there, and the arm's own edge copies are what next write it.
            if opaque_needed.is_none()
                && !prologue_zero_reserved_tail_disabled()
                && self.phi_copy_scratch_slot_off > 0
            {
                self.emit_zero_frame_slot(self.phi_copy_scratch_slot_off);
            }
            // JMP into the body at this bci.
            self.buf.emit_byte(0xE9);
            let patch = self.buf.pos();
            self.buf.emit(&[0, 0, 0, 0]);
            let rel = native as i32 - (patch as i32 + 4);
            if !Self::patch_or_bail(&mut self.buf, patch, rel) {
                // The compile is discarded (the buffer overflowed); answer
                // "not all opaque" so no relaxed fact rides on a partial list.
                return non_opaque.max(1);
            }
            // The highest JVM local index this stub reads, plus one. The
            // caller offers a locals snapshot and must not be asked for more
            // than the interpreter frame holds — `ir_osr_enter` refuses a short
            // one rather than reading past its end.
            let locals_needed = opaque_needed.unwrap_or(seeds_hi);
            if opaque_needed.is_none() {
                non_opaque += 1;
            } else {
                // The door's per-entry bit (`ir_osr_opaque_entry_bcis`).
                self.osr_opaque_entry_bcis.push(bci);
            }
            // Cast: an offset inside a bounded executable buffer.
            self.osr_entries.push((bci, stub as u32, locals_needed));
            self.osr_entry_kinds.push((bci, kinds));
        }
        non_opaque
    }

    /// Seed local `i` of the interpreter's locals array (pointer in R11) as
    /// value `v`: its home word, and the register(s) it is published in. One
    /// seed of [`Self::emit_osr_entry_stubs`]; see there for why the dead
    /// locals are seeded before the live ones.
    pub(in crate::ir_lower) fn emit_osr_seed(&mut self, i: usize, v: NodeId) {
        // Cast: a JVM local index times 8.
        self.emit_mov_rax_from_r11_disp((i as i32) * 8);
        if !self.home_dropped.get(v as usize).copied().unwrap_or(false) {
            if let Some(off) = self.node_slot.get(v as usize).copied().flatten() {
                // Cast: a frame offset inside this method's own frame.
                let off = off.get() as i32;
                self.store_abi_reg(RAX, off);
                // -- ...and into its XMM register, when it has one --
                //
                // The home word is not the whole truth for an FP value.
                // `fp_load_value` -- which every FP operand read in the
                // body goes through -- takes a value from its RESIDENT
                // XMM and never looks at the frame when one exists, and
                // an FP definition publishes one: `Op::Param` through
                // `publish_fp_from_slot`, every FP arithmetic node
                // through `fp_store_value`. Those definitions sit in
                // the blocks this stub jumps past.
                //
                // So this is the constants defect above, in the other
                // register file, and it needs neither a splice nor a
                // merge to reach: `double f(int n, double k) { double
                // a = k*3.0+1.5; ... for (..) s += x*k + a; }` names
                // `k` and `a` at the header, both live across it, both
                // read from XMMs the entry stub had never written.
                // Seeding only the home word left the PREVIOUS frame's
                // floating-point registers standing in for them.
                //
                // A GPR-resident local was already handled below; that
                // half was written and this one was not, which is the
                // same asymmetry `osr_entry_can_produce` now exists to
                // stop repeating.
                //
                // Reading the word back is safe under this `if`: an FP
                // home is never dropped (`value_home_droppable` admits
                // `Int` and `Long` alone), so the branch that skips the
                // store is not one an FP value can take.
                if let Some(ty) = self.graph.nodes.get(v as usize).map(|n| n.ty) {
                    if matches!(ty, IrType::Float | IrType::Double) {
                        self.publish_fp_from_slot(v, off, ty == IrType::Double);
                    }
                }
            }
        }
        if let Some(dst) = self.assigned_gpr(v) {
            self.emit_mov_reg_reg64(dst, RAX);
            self.mark_gp_reg_live(v);
        }
    }

    /// JIT round 21 wave 17 (lane osrrefs, OD16-2): write null to the home
    /// word and the assigned register of `v`, an unnamed reference the entry
    /// owes no seed ([`Self::osr_entry_ref_needs_no_seed_by`]). A safepoint
    /// map, the frame block and the band scan may publish either while the
    /// value's interval covers the loop; null is the unwritten state every
    /// root consumer handles. RAX is the only scratch, as in
    /// [`Self::emit_osr_seed`].
    pub(in crate::ir_lower) fn emit_osr_null_ref(&mut self, v: NodeId) {
        self.emit_mov_rax_imm64(0);
        if !self.home_dropped.get(v as usize).copied().unwrap_or(false) {
            if let Some(off) = self.node_slot.get(v as usize).copied().flatten() {
                // Cast: a frame offset inside this method's own frame.
                self.store_abi_reg(RAX, off.get() as i32);
            }
        }
        if let Some(dst) = self.assigned_gpr(v) {
            self.emit_mov_reg_reg64(dst, RAX);
            self.mark_gp_reg_live(v);
        }
    }

    /// What an ordinary OSR entry at block `entry_block` must recompute so
    /// that `unnamed` -- the values live across its start that the header
    /// snapshot (`locals`) does not name and that are not constants -- are
    /// written before the stub jumps in. `None` refuses the entry, which is
    /// the answer every such value got before round 15 wave 6.
    ///
    /// # Why (lane irosr, `r15w5-osrtier-accepted-optimizing-osr-body-never-entered`)
    ///
    /// The schedule places a pure node EARLY, in the deepest block its inputs
    /// dominate, and the late sink moves it only to a strictly shallower loop
    /// depth. So a post-loop expression of loop-invariant locals --
    /// `(long) (n - 3) * (n - 4) / 2` after the loop of
    /// `R15OsrtierAllocLoop.ring`, `n * (n - 1) / 2` in `lazy`, the four of
    /// `chain` -- is computed in front of the loop and is live across its
    /// header, where no local names it. That refused the entry of every one of
    /// those bodies (`osr block 1 (bci 14) refused: n108 Some(Div)`), and the
    /// door then kept the single-pass OSR body forever although it had
    /// accepted the optimizing one.
    ///
    /// # Why it is sound
    ///
    /// * Each root is an `int`/`long` value defined in a block that STRICTLY
    ///   dominates the entry, so it is computed on every path to the header
    ///   before the loop runs, from inputs that dominate it in turn.
    /// * The program reads only values the header snapshot names -- the
    ///   interpreter's local `i` holds exactly that value at this bci, the
    ///   fact every seed already rests on -- and constants. Their kinds become
    ///   entry constraints (`OsrRemat::leaf_locals`).
    /// * Every operation is a total function with Java's semantics: 32-bit
    ///   arithmetic wraps, shifts mask their count in hardware exactly as JLS
    ///   15.19 does, and a division is admitted only by a constant that is
    ///   neither 0 nor -1, so `IDIV` neither traps nor overflows. Nothing
    ///   calls, allocates, reads the heap or can deopt.
    /// * A root is written exactly where its own definition writes it: its
    ///   home word and its register (a non-split value's register is fixed for
    ///   its whole range, and the range covers the entry). A split-managed
    ///   root, or one with neither, refuses.
    ///
    /// # The wider shapes (round 15 wave 6, lane irosr2, IRO-4)
    ///
    /// The w5a census of this refusal also named `Cmp` (a loop-invariant
    /// guard condition such as `divisor != 0` or `x != null`, scheduled in
    /// front of the loop while its `Op::Guard` stays inside), `LCmp`,
    /// `ExactClassIs` and `ArrayLength`. A REFERENCE may now be a leaf -- only
    /// as such an operand, and only read from the local that names it or as
    /// the null constant; the seeds copy the same raw word into the body, so
    /// the stub compares and dereferences exactly what the body would.
    ///
    /// * `Cmp` / `LCmp` are total (`SETcc`, the `Op::Cmp` / `Op::LCmp` arms'
    ///   own sequences). `CRATONVM_JIT_IR_OSR_REMAT_COMPARES=0` withdraws
    ///   them and `ExactClassIs`.
    /// * `ExactClassIs` is total too: a null or an array answers `0` with no
    ///   load, and the class id and kind byte it reads are fixed at
    ///   allocation, so the answer at the entry is the answer the skipped
    ///   block computed for the same object.
    /// * `ArrayLength` is NOT total: its body arm deopts on a null array, and
    ///   a stub cannot deopt. By default only an array the IR proves non-null
    ///   on every path (`ir_check_elim::definitely_non_null`: a `NewArray` the
    ///   snapshot names) is recomputed. `CRATONVM_JIT_IR_OSR_REMAT_ARRAY_LENGTH
    ///   =any` widens it to any named array, on LICM's own precondition for a
    ///   hoisted `arraylength` (it executes on every entry to its header, or
    ///   its base is non-null on entry), which the interpreter therefore met
    ///   before it reached this back edge; the stub still tests for null and
    ///   answers `0` rather than fault. `=0` withdraws `ArrayLength`.
    pub(in crate::ir_lower) fn osr_entry_remat_program(
        &self,
        entry_block: usize,
        locals: &[NodeId],
        unnamed: &[NodeId],
    ) -> Option<OsrRemat> {
        if unnamed.is_empty() || unnamed.len() > OSR_REMAT_MAX_ROOTS {
            return None;
        }
        let nblocks = self.schedule.blocks.len();
        let dom = &self.schedule.dom;
        if entry_block >= nblocks || !dom.is_reachable(entry_block) {
            return None;
        }
        let mut out = OsrRemat::default();
        let mut budget = OSR_REMAT_MAX_NODES;
        // Round 15 wave 6 (lane irosr2, IRO-4): the compare / class-test /
        // array-length shapes, each behind its own switch.
        let policy = OsrRematPolicy {
            compares: ir_osr_remat_compares_enabled(),
            array_length: ir_osr_remat_array_length(),
            static_read: ir_osr_remat_static_read_enabled(),
            // JIT round 19 wave 4 (lane osr4, IROPS2-4): on top of `static_read`.
            static_read_wide: ir_osr_remat_static_read_enabled()
                && ir_osr_remat_static_read_wide_enabled(),
        };
        for &v in unnamed {
            let node = self.graph.nodes.get(v as usize)?;
            // JIT round 19 wave 4 (lane osr4, IROPS2-4): a `float` / `double`
            // / reference root only when it is a hoisted direct static read
            // the stub re-reads ([`Self::osr_remat_wide_static_insn`]), and
            // only into a home word (an FP value is then published to its
            // XMM register from there; a reference is never register-resident).
            let wide_static = policy.static_read_wide
                && matches!(node.ty, IrType::Float | IrType::Double | IrType::Ref)
                && self.osr_remat_wide_static_insn(v).is_some();
            if !matches!(node.ty, IrType::Int | IrType::Long) && !wide_static {
                return None;
            }
            let d = self
                .schedule
                .node_to_block
                .get(v as usize)
                .copied()
                .unwrap_or(usize::MAX);
            if d >= nblocks
                || d == entry_block
                || !dom.is_reachable(d)
                || !dom.dominates(d, entry_block)
            {
                return None;
            }
            if self.ls_managed.get(v as usize).copied().unwrap_or(false) {
                return None;
            }
            let has_home = self.node_slot.get(v as usize).copied().flatten().is_some()
                && !self.home_dropped.get(v as usize).copied().unwrap_or(false);
            if !has_home && (wide_static || self.assigned_gpr(v).is_none()) {
                return None;
            }
            if !self.osr_remat_compile(v, locals, policy, &mut out, &mut budget, 0) {
                return None;
            }
            out.prog.push(OsrRematInsn::Publish(v));
            out.roots.push(v);
        }
        out.leaf_locals.sort_unstable();
        out.leaf_locals.dedup();
        Some(out)
    }

    /// JIT round 19 wave 4 (lane osr4, IROPS2-4): the stub instruction that
    /// re-reads `v` when it is a non-volatile direct static read of a `float`
    /// (`StaticLoadF32`), `double` or reference (`StaticLoad`, 64-bit) that the
    /// direct-static LICM hoisted (`Graph::licm_hoisted_static_reads`), on the
    /// same precondition as IROPS19-7's `int` / `long` arm: nothing in the
    /// loop writes the cell, so the re-read at an entry inside it is the
    /// skipped pre-header read. Widths are `emit_inline_getstatic`'s. `None`
    /// for anything else.
    pub(in crate::ir_lower) fn osr_remat_wide_static_insn(
        &self,
        v: NodeId,
    ) -> Option<OsrRematInsn> {
        let node = self.graph.nodes.get(v as usize)?;
        let Op::LoadStatic {
            class_id,
            field_index,
            type_tag,
            is_volatile: false,
        } = node.op
        else {
            return None;
        };
        if !self.graph.licm_hoisted_static_reads.contains(&v)
            || !crate::x64::inline_getstatic_enabled()
        {
            return None;
        }
        let f32_read = match (type_tag, node.ty) {
            (b'F', IrType::Float) => true,
            (b'D', IrType::Double) | (b'L' | b'[', IrType::Ref) => false,
            _ => return None,
        };
        let base_cell = self
            .direct_helpers
            .resolve_static_base(class_id, field_index as usize)?;
        let (_, payload32, payload64) = static_cell_offsets(field_index)?;
        // Cast: the baked cell address, as an imm64 bit pattern.
        let base_cell = base_cell as u64;
        Some(if f32_read {
            OsrRematInsn::StaticLoadF32 {
                base_cell,
                disp: payload32,
            }
        } else {
            OsrRematInsn::StaticLoad {
                base_cell,
                disp: payload64,
                wide: true,
            }
        })
    }

    /// Append to `out.prog` the instructions that leave value `v` in RAX, or
    /// answer `false` for a shape [`Self::osr_entry_remat_program`] does not
    /// recompute. A value the snapshot names is read from its local, even when
    /// it is also an expression: the local is what the interpreter holds.
    pub(in crate::ir_lower) fn osr_remat_compile(
        &self,
        v: NodeId,
        locals: &[NodeId],
        policy: OsrRematPolicy,
        out: &mut OsrRemat,
        budget: &mut u32,
        depth: u32,
    ) -> bool {
        if *budget == 0 || depth > OSR_REMAT_MAX_DEPTH {
            return false;
        }
        *budget -= 1;
        let Some(node) = self.graph.nodes.get(v as usize) else {
            return false;
        };
        let ty = node.ty;
        // A reference only as a LEAF (round 15 wave 6, lane irosr2): no arm
        // below produces one, and the typed arms that take one (`Cmp`,
        // `ExactClassIs`, `ArrayLength`) check their operand types first, so a
        // reference never reaches an arithmetic instruction.
        let is_ref = ty == IrType::Ref;
        // JIT round 19 wave 4 (lane osr4, IROPS2-4): a hoisted `float` /
        // `double` / reference direct static read is re-read, before any
        // local is consulted (the cell is unchanged since the pre-header, so
        // the read IS the value; the interpreter's local, if one named it,
        // holds the same).
        if policy.static_read_wide && matches!(ty, IrType::Float | IrType::Double | IrType::Ref) {
            if let Some(insn) = self.osr_remat_wide_static_insn(v) {
                out.prog.push(insn);
                return true;
            }
        }
        if !matches!(ty, IrType::Int | IrType::Long) && !is_ref {
            return false;
        }
        let wide = ty == IrType::Long;
        // `v` is an arena node (the `get` above), so never `NO_NODE`, which an
        // unnamed local slot holds.
        if let Some(i) = locals.iter().position(|&l| l == v) {
            out.prog.push(OsrRematInsn::Local(i));
            out.leaf_locals.push(i);
            return true;
        }
        if is_ref {
            // The null constant is the one reference the stub can name itself.
            if crate::ir::is_null_const(node) {
                out.prog.push(OsrRematInsn::Imm(0));
                return true;
            }
            return false;
        }
        let ty_of = |id: NodeId| self.graph.nodes.get(id as usize).map(|n| n.ty);
        match node.op {
            Op::Const(c) => {
                out.prog.push(OsrRematInsn::Imm(c));
                true
            }
            Op::Add | Op::Sub | Op::Mul | Op::And | Op::Or | Op::Xor => {
                let op = match node.op {
                    Op::Add => OsrRematAlu::Add,
                    Op::Sub => OsrRematAlu::Sub,
                    Op::Mul => OsrRematAlu::Mul,
                    Op::And => OsrRematAlu::And,
                    Op::Or => OsrRematAlu::Or,
                    _ => OsrRematAlu::Xor,
                };
                let &[a, b] = node.inputs.as_slice() else {
                    return false;
                };
                if ty_of(a) != Some(ty) || ty_of(b) != Some(ty) {
                    return false;
                }
                if !self.osr_remat_compile_pair(a, b, locals, policy, out, budget, depth) {
                    return false;
                }
                out.prog.push(OsrRematInsn::Alu { op, wide });
                true
            }
            Op::Shl | Op::Shr | Op::UShr => {
                let op = match node.op {
                    Op::Shl => OsrRematShift::Shl,
                    Op::Shr => OsrRematShift::Shr,
                    _ => OsrRematShift::UShr,
                };
                let &[a, b] = node.inputs.as_slice() else {
                    return false;
                };
                if ty_of(a) != Some(ty) || ty_of(b) != Some(IrType::Int) {
                    return false;
                }
                if !self.osr_remat_compile_pair(a, b, locals, policy, out, budget, depth) {
                    return false;
                }
                out.prog.push(OsrRematInsn::Shift { op, wide });
                true
            }
            Op::Neg => {
                let &[a] = node.inputs.as_slice() else {
                    return false;
                };
                if ty_of(a) != Some(ty) {
                    return false;
                }
                if !self.osr_remat_compile(a, locals, policy, out, budget, depth + 1) {
                    return false;
                }
                out.prog.push(OsrRematInsn::Neg { wide });
                true
            }
            Op::I2L | Op::L2I => {
                let (from, insn) = if node.op == Op::I2L {
                    (IrType::Int, OsrRematInsn::I2L)
                } else {
                    (IrType::Long, OsrRematInsn::L2I)
                };
                let &[a] = node.inputs.as_slice() else {
                    return false;
                };
                // `I2L` is a `long`, `L2I` an `int`: the result type is the
                // other one.
                if ty == from
                    || ty_of(a) != Some(from)
                    || !self.osr_remat_compile(a, locals, policy, out, budget, depth + 1)
                {
                    return false;
                }
                out.prog.push(insn);
                true
            }
            Op::Div | Op::Rem => {
                // `[a, b]` or `[a, b, guard token]` (`ir::with_guard_token`).
                // A constant divisor other than 0 and -1 needs no guard at
                // all, so the token's block is not a precondition.
                let (a, divisor) = match node.inputs.as_slice() {
                    &[a, b] | &[a, b, _] => (a, b),
                    _ => return false,
                };
                let Some(d) = const_int_divisor(self.graph, ty, divisor) else {
                    return false;
                };
                if d == 0 || d == -1 || ty_of(a) != Some(ty) {
                    return false;
                }
                if !self.osr_remat_compile(a, locals, policy, out, budget, depth + 1) {
                    return false;
                }
                out.prog.push(OsrRematInsn::DivImm {
                    d,
                    rem: node.op == Op::Rem,
                    wide,
                });
                true
            }
            // ── Round 15 wave 6 (lane irosr2, IRO-4) ──────────────────────
            //
            // A compare's operands are both `int`, both `long` or both
            // references (`if_acmp*`, `ifnull`: a local and the null
            // constant), the `Op::Cmp` arm's width rule; a mixed pair is not
            // reasoned about. The result is the arm's `SETcc; MOVZX` 0/1.
            Op::Cmp(cc) if policy.compares => {
                let &[a, b] = node.inputs.as_slice() else {
                    return false;
                };
                let (Some(ta), Some(tb)) = (ty_of(a), ty_of(b)) else {
                    return false;
                };
                if ty != IrType::Int
                    || ta != tb
                    || !matches!(ta, IrType::Int | IrType::Long | IrType::Ref)
                {
                    return false;
                }
                if !self.osr_remat_compile_pair(a, b, locals, policy, out, budget, depth) {
                    return false;
                }
                out.prog.push(OsrRematInsn::Cmp {
                    setcc: cc.x64_cc() + 0x10,
                    wide: ta != IrType::Int,
                });
                true
            }
            Op::LCmp if policy.compares => {
                let &[a, b] = node.inputs.as_slice() else {
                    return false;
                };
                if ty != IrType::Int
                    || ty_of(a) != Some(IrType::Long)
                    || ty_of(b) != Some(IrType::Long)
                {
                    return false;
                }
                if !self.osr_remat_compile_pair(a, b, locals, policy, out, budget, depth) {
                    return false;
                }
                out.prog.push(OsrRematInsn::LCmp);
                true
            }
            Op::ExactClassIs { class_id } if policy.compares => {
                let &[obj] = node.inputs.as_slice() else {
                    return false;
                };
                if ty != IrType::Int
                    || ty_of(obj) != Some(IrType::Ref)
                    || !self.osr_remat_compile(obj, locals, policy, out, budget, depth + 1)
                {
                    return false;
                }
                out.prog.push(OsrRematInsn::ExactClassIs { class_id });
                true
            }
            // JIT round 19 (lane osr19, OSRPOLY19-2): a class-word chain head
            // live across the entry. Total like `ExactClassIs` (a null or an
            // array answers `0` with no fault; the header word is fixed at
            // allocation), so it rides the same switch.
            Op::ClassWordOf if policy.compares => {
                let &[obj] = node.inputs.as_slice() else {
                    return false;
                };
                if ty != IrType::Int
                    || ty_of(obj) != Some(IrType::Ref)
                    || !self.osr_remat_compile(obj, locals, policy, out, budget, depth + 1)
                {
                    return false;
                }
                out.prog.push(OsrRematInsn::ClassWordOf);
                true
            }
            // `[ctrl, mem, array]`. See `osr_entry_remat_program` for why the
            // array must be proven non-null unless the switch says `any`.
            Op::ArrayLength => {
                let &[_, _, arr] = node.inputs.as_slice() else {
                    return false;
                };
                if ty != IrType::Int || ty_of(arr) != Some(IrType::Ref) {
                    return false;
                }
                let proven = self
                    .graph
                    .nodes
                    .get(arr as usize)
                    .is_some_and(|n| crate::ir_check_elim::definitely_non_null(&n.op));
                let admitted = match policy.array_length {
                    OsrRematArrayLength::Off => false,
                    OsrRematArrayLength::NonNull => proven,
                    OsrRematArrayLength::Any => true,
                };
                if !admitted || !self.osr_remat_compile(arr, locals, policy, out, budget, depth + 1)
                {
                    return false;
                }
                out.prog.push(OsrRematInsn::ArrayLength);
                true
            }
            // JIT round 19 wave 2 (lane irops2, IROPS19-7): a direct static
            // read the direct-static LICM hoisted to a pre-header
            // (`Graph::licm_hoisted_static_reads`). The hoist's precondition
            // is that nothing in the loop writes the cell, so every path from
            // the pre-header to an entry inside the loop leaves it unchanged
            // (other threads aside, which the JMM lets a plain read miss or
            // see either way), and re-reading it here is the value the
            // skipped pre-header read. Two `MOV`s through the class's baked
            // statics-base cell, `emit_inline_getstatic`'s own sequence and
            // widths; total (no trap: the class was initialised at compile
            // time and its block is published). `int`-category and `long`
            // only: the stub's accumulator is a GPR (`float`/`double`/
            // reference reads keep refusing the entry, as before).
            Op::LoadStatic {
                class_id,
                field_index,
                type_tag,
                is_volatile: false,
            } if policy.static_read && self.graph.licm_hoisted_static_reads.contains(&v) => {
                let wide = match (type_tag, ty) {
                    (b'J', IrType::Long) => true,
                    (b'I' | b'Z' | b'B' | b'C' | b'S', IrType::Int) => false,
                    _ => return false,
                };
                if !crate::x64::inline_getstatic_enabled() {
                    return false;
                }
                let Some(base_cell) = self
                    .direct_helpers
                    .resolve_static_base(class_id, field_index as usize)
                else {
                    return false;
                };
                let Some((_, payload32, payload64)) = static_cell_offsets(field_index) else {
                    return false;
                };
                out.prog.push(OsrRematInsn::StaticLoad {
                    base_cell: base_cell as u64,
                    disp: if wide { payload64 } else { payload32 },
                    wide,
                });
                true
            }
            _ => false,
        }
    }

    /// Round 16 (lane matrix, MX16-1): is the `Op::Guard` node `id` at `bci`
    /// a RANGE PREDICATE (`ir_optimize::install_range_predicates`)? The shape
    /// that pass builds and nothing else does: a guard at a loop header's bci
    /// naming a frame at that bci, whose condition is `Cmp(Le | Lt)[x, len]`
    /// with `len` an `ArrayLength` at the same bci. Its failure resumes at the
    /// header with the loop-entry state, which the interpreter then runs.
    pub(in crate::ir_lower) fn op_guard_is_range_predicate(&self, id: NodeId, bci: usize) -> bool {
        if self.pc_is_in_a_spliced_body(bci) || !self.is_loop_header_bci(bci) {
            return false;
        }
        let nodes = &self.graph.nodes;
        let Some(g) = nodes.get(id as usize) else {
            return false;
        };
        if g.bytecode_pc != Some(bci)
            || !g
                .frame_snapshot
                .and_then(|si| self.graph.safepoints.get(si as usize))
                .is_some_and(|sp| sp.bci == bci)
        {
            return false;
        }
        let Some(cn) = g.inputs.get(1).and_then(|&c| nodes.get(c as usize)) else {
            return false;
        };
        if !matches!(cn.op, Op::Cmp(CmpOp::Le) | Op::Cmp(CmpOp::Lt)) || cn.inputs.len() != 2 {
            return false;
        }
        nodes.get(cn.inputs[1] as usize).is_some_and(|len| {
            len.op == Op::ArrayLength && len.bytecode_pc == Some(bci) && len.inputs.len() == 3
        })
    }
}
