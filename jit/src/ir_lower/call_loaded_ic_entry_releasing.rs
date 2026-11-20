#[allow(unused_imports)]
use super::*;

impl<'a> Lowerer<'a> {
    /// [`Self::emit_call_loaded_ic_entry`] for a hit that lowered RSP by
    /// `stack_block` bytes of outgoing stack arguments (an over-wide site,
    /// [`Self::emit_ic_wide_hit_call`]): the `ADD RSP` goes between the
    /// return-address row and the frame republish. `0` emits exactly the
    /// register-only sequence.
    pub(in crate::ir_lower) fn emit_call_loaded_ic_entry_releasing(
        &mut self,
        stack_block: i32,
        map: &mut Option<SafepointMapToken>,
    ) {
        self.buf.emit(&[0x41, 0xFF, 0xD3]); // CALL R11
                                            // Round 9 wave 2: THIS is a return address a stack walk can find in
                                            // this frame (the callee is compiled Java and may throw, collect or
                                            // take a stack trace). The shared `.done`'s `emit_call_return_check`
                                            // records only the slow helper's return site, so an inline-cache hit
                                            // inside a spliced body walked as the enclosing method alone.
        self.anchor_safepoint_return(map.as_mut(), self.buf.pos());
        self.note_inline_frame_return_site();
        if stack_block > 0 {
            self.emit_add_rsp_imm32(stack_block);
        }
        // The callee is COMPILED JAVA, so its prologue published ITS (rbp,
        // compile id) into the innermost-frame mirror and nothing on the return
        // path of a raw JIT->JIT call restores this frame's. Both inline-cache
        // arms in this tier went without it until 2026-08-24, so after any
        // monomorphic or polymorphic hit the mirror named a frame that had
        // already returned -- measured on H2 `TestMVStoreTool`, where ONE rbp
        // with ONE saved return address inside `TestMVStoreTool.testCompact`
        // was claimed across collections by four different methods, one of them
        // `RootReference.isLocked` with `maps=0`, which emits no safepoint and
        // therefore cannot be the frame at a collection at all.
        //
        // `moving_young_frame_coverage_complete` then reads
        // `[stale_rbp - stale_method.sp_id_slot_off]`, finds a word matching no
        // map, and refuses the whole cycle (`ACTIVE_FRAME_MAP`,
        // `frame_cov=(no_map=N incomplete=0)`), which is what kept ZGC from
        // compacting on `bug-h2-testkillprocess-zgc-oom-at-97-percent-free`.
        // The single-pass backend republishes at both of its equivalent arms,
        // and the shared hashed/vtable stub below is handed `self.frame_record`
        // for exactly this -- these two arms were the gap.
        //
        // It lives INSIDE this helper rather than at the two call sites so a
        // third arm cannot be added without it. RAX (the callee's Java return
        // value) is preserved by both forms of the republish, and the security
        // invariant above is about the MOV/CALL pair, which nothing here comes
        // between.
        if !ic_frame_republish_disabled() {
            // W7-C: a sunk hit republishes on its sentinel side instead
            // (`emit_ic_hit_exit`, the callee-deopt service's cold entry).
            if self.tail_sink.cur {
                self.tail_sink.sunk_sites += 1;
            } else {
                self.emit_post_call_frame_record();
                IC_FRAME_REPUBLISH_SITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }

    /// A compile-time recursion cycle's CLOSING edge, through its
    /// [`crate::JitCycleEdgeCell`] (round 9 wave 8, irl8; the IR half of
    /// `perf-mutual-recursion-forces-every-call-through-invoke-dispatch`, whose
    /// single-pass half is `x64/op_invoke.rs::emit_cycle_edge_cell_call`).
    ///
    /// ```text
    ///   <jit_self_call_stack_guard(vm)> ; TEST RAX,RAX ; JNE exception stub
    ///   MOV  R10, imm64 cell
    ///   MOV  R11, [R10 + JIT_CYCLE_EDGE_CELL_ENTRY_OFFSET] ; TEST R11,R11
    ///   JZ   .slow                          ; ancestor not published (yet/any more)
    ///   <marshal the context entry ABI>     ; ENTRY_ABI_REGS only
    ///   CALL R11                            ; + frame republish
    ///   <callee deopt service> ; JMP .done
    /// .slow:
    ///   <the unchanged jit_invoke_dispatch sequence>
    /// .done:
    ///   <emit_call_return_check>            ; shared sentinel + result spill
    /// ```
    ///
    /// It is the monomorphic inline-cache arm of [`Self::emit_inline_cache_call`]
    /// with the class-id guard replaced by "the word is non-zero", and every
    /// safety argument is that arm's:
    ///
    /// * **Lifetime.** The word is loaded ONCE and nothing between the load
    ///   and the `CALL` can block (the marshal only reads frame words and
    ///   registers into `ENTRY_ABI_REGS`), so a thread that saw a body is in
    ///   compiled code until that body's frame exists; the cell's owning `Arc`
    ///   and the retirement queue do the rest (`JitCycleEdgeCell`'s doc).
    /// * **ABI.** A cell only ever admits a context-ABI body, so the marshal
    ///   is the context layout, unconditionally. The caller admits the site
    ///   only when `num_args + 1` fits the entry-ABI register file.
    /// * **Stack depth.** The baked edges of the cycle all point at OLDER
    ///   artifacts, so unbounded recursion crosses a cell edge every
    ///   traversal, and every crossing checks the exact floor FIRST. An inline
    ///   published-floor compare skips the helper only when safely above it;
    ///   this never uses the self call's 64 KiB sampling window: that window is exact only when each step down the
    ///   stack is this frame's own size, and a traversal of a cycle is the sum
    ///   of several frames. The helper is a TLS read and a compare; the
    ///   dispatch it replaces is the whole Rust round trip. A trip (SOE
    ///   pending) leaves through the call-exception stub, which restores the
    ///   shadow top the caller's map pushed.
    pub(in crate::ir_lower) fn emit_cycle_edge_cell_call(
        &mut self,
        inputs: &[NodeId],
        slot: i32,
        num_args: usize,
        cell: usize,
        info_ptr: usize,
        ty: IrType,
        map: &mut Option<SafepointMapToken>,
    ) {
        self.emit_exact_stack_floor_guard(None, map);

        // The cell's entry word, loaded once into R11.
        self.emit_mov_reg_imm64(R10, cell as u64);
        const _: () = assert!(
            crate::JIT_CYCLE_EDGE_CELL_ENTRY_OFFSET >= 0
                && crate::JIT_CYCLE_EDGE_CELL_ENTRY_OFFSET <= 127,
            "the cell's entry word is addressed with a disp8"
        );
        // Cast: in 0..=127 by the assertion above.
        self.emit_load_ic_entry_word(crate::JIT_CYCLE_EDGE_CELL_ENTRY_OFFSET as u8);
        let slow = self.emit_jcc_rel32(0x84); // JZ .slow
        let wide_stack = Self::ic_wide_stack_block(num_args);
        let force_slow = if num_args + 1 > ENTRY_ABI_REGS.len() && wide_stack.is_none() {
            Some(self.emit_jmp_rel32())
        } else {
            None
        };
        self.emit_strip_ic_context_tag();
        if let Some((total_sub, base_disp)) = wide_stack {
            // A late static cell has the same tagged entry word as an inline
            // cache. Use its wide ABI path when the context plus Java
            // arguments do not fit in the register file.
            self.emit_ic_wide_hit_call(inputs, num_args, total_sub, base_disp, map);
        } else if num_args + 1 <= ENTRY_ABI_REGS.len() {
            let noctx = self.emit_jcc_rel32(0x83); // JNC .entry_noctx
            self.emit_ic_abi_marshal(inputs, num_args, true);
            let call = self.emit_jmp_rel32();
            self.patch_rel32_to_here(noctx);
            self.emit_ic_abi_marshal(inputs, num_args, false);
            self.patch_rel32_to_here(call);
            self.emit_call_loaded_ic_entry(map);
        }
        // THROWS4-1 (lane throws5): a cell site is `invokestatic` (the arm's
        // admission), so statically bound.
        self.service_lean_bit_pending = self.direct_call_sentinel_lean_on;
        self.emit_inline_callee_deopt_service(info_ptr, num_args, inputs, false, map);
        self.service_lean_bit_pending = false;
        let done = self.emit_jmp_rel32();

        // .slow: exactly the generic `Op::Call` dispatch tail.
        self.patch_rel32_to_here(slow);
        if let Some(force_slow) = force_slow {
            self.patch_rel32_to_here(force_slow);
        }
        for i in 0..num_args {
            let arg = inputs[2 + i];
            self.gp_load_abi_value(RAX, arg);
            // Cast: an argument index is bounded by the callee's parameter
            // count, so `i * 8` cannot overflow an x86-64 displacement.
            self.store_rax(self.args_stage_top_off - (i as i32) * 8);
        }
        self.load_context(CALL_ARG_REGS[0]); // vm_ptr
        self.emit_mov_reg_imm64(CALL_ARG_REGS[1], info_ptr as u64); // info_ptr
        self.lea_reg_from_frame(CALL_ARG_REGS[2], self.args_stage_top_off); // args_ptr
        self.emit_mov_reg_imm64(CALL_ARG_REGS[3], num_args as u64); // num_args
        self.emit_mov_reg_imm64(RAX, self.invoke_dispatch as u64);
        self.buf.emit(&[0xFF, 0xD0]); // CALL RAX

        // .done: shared with every other call route.
        self.patch_rel32_to_here(done);
        self.anchor_safepoint_return(map.as_mut(), self.buf.pos());
        self.emit_call_return_check(slot, ty);
    }

    /// Branch on the VM-published exact floor. Missing mirrors and unavailable
    /// floor words must fall back to the caller's existing guard, never a sample.
    /// Returns (missing-thread, above-floor) patches; clobbers RAX and flags only.
    pub(in crate::ir_lower) fn emit_published_stack_floor_test(&mut self) -> Option<(usize, usize)> {
        let tls = crate::x64::jit_thread_tls_disp();
        if self.stack_floor_off_in_thread <= 0 || tls == 0 {
            return None;
        }
        self.emit_mov_rax_tls_disp32(tls as u32);
        self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
        let missing = self.emit_jcc_rel32(0x84);
        self.buf.emit(&[0x48, 0x3B, 0xA0]); // CMP RSP, [RAX + floor]
        self.buf.emit(&self.stack_floor_off_in_thread.to_le_bytes());
        let above = self.emit_jcc_rel32(0x87);
        Some((missing, above))
    }

    /// Exact constructor/cycle guard. Every slow path retains the original
    /// helper, pending constructor receiver, exception exit and root copy-back.
    pub(in crate::ir_lower) fn emit_exact_stack_floor_guard(
        &mut self,
        receiver: Option<NodeId>,
        map: &mut Option<SafepointMapToken>,
    ) {
        let above = self
            .emit_published_stack_floor_test()
            .map(|(missing, above)| {
                self.patch_rel32_to_here(missing);
                above
            });
        self.load_context(CALL_ARG_REGS[0]);
        if let Some(receiver) = receiver {
            self.gp_load_abi_value(CALL_ARG_REGS[1], receiver);
        } else {
            self.emit_mov_reg_imm_smart(CALL_ARG_REGS[1], 0);
        }
        self.emit_mov_reg_imm64(RAX, self.self_call_stack_guard as u64);
        self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
        self.anchor_safepoint_return(map.as_mut(), self.buf.pos());
        self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
        let trip = self.emit_jcc_rel32(0x85);
        self.push_stack_guard_exc_patch(trip);
        self.emit_pending_shadow_copy_back();
        if let Some(above) = above {
            self.patch_rel32_to_here(above);
        }
    }

    /// The native-stack check of [`Self::emit_inline_cache_call`] (round 13
    /// wave 2, lane mega; `CRATONVM_JIT_IC_STACK_GUARD`, default on).
    ///
    /// Where the VM publishes the thread's floor word (round 13 wave 4, M13-3,
    /// `CRATONVM_JIT_STACK_FLOOR_WORD`) and the thread has a mirror, the site
    /// compares RSP with it exactly and calls the guard helper only below it.
    /// Otherwise it samples the way a direct self-call does
    /// (`emit_self_call_stack_sample`): `MOVZX EAX, SP; CMP EAX, span; JA .ok`,
    /// i.e. the guard helper runs only when THIS frame (`frame_size` plus the
    /// return address and saved RBP) straddles a 64 KiB boundary. The helper compares RSP with the thread's real floor and
    /// answers `0` or the sentinel with a `StackOverflowError` stashed; the
    /// sentinel leaves through the site's call-exception exit, exactly as the
    /// self-call's does.
    ///
    /// Soundness is per frame, not per call: a boundary inside a frame that
    /// makes no inline-cache call on the recursion's path (a helper or direct
    /// call continues it) is not sampled here. The single-pass tier's sites
    /// compare against the exact floor (`driver.rs::reserve_stack_floor`), the
    /// dispatch helpers check it (`enter_jit_dispatch`), and the floor keeps at
    /// least three 64 KiB strides of headroom, so a cycle has to avoid every
    /// sampled frame for 48 KiB+ to get past it (sampled form only).
    pub(in crate::ir_lower) fn emit_ic_stack_sample(&mut self, map: &mut Option<SafepointMapToken>) {
        if self.self_call_stack_guard == 0 || !ir_ic_stack_guard_enabled() {
            return;
        }
        // Round 13 wave 4 (lane mega3, M13-3): an exact compare against the
        // thread's floor word where it is published; the 64 KiB sample stays
        // for a thread with no mirror (`JZ .sample`) and when the word is off.
        let mut exact_ok_patch: Option<usize> = None;
        let mut exact_low_patch: Option<usize> = None;
        if let Some((no_thread, above)) = self.emit_published_stack_floor_test() {
            exact_ok_patch = Some(above);
            self.buf.emit_byte(0xE9); // JMP .helper
            exact_low_patch = Some(self.buf.pos());
            self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
            // .sample:
            let rel = self.buf.pos() as i32 - (no_thread as i32 + 4);
            Self::patch_or_bail(&mut self.buf, no_thread, rel);
        }
        self.buf.emit(&[0x0F, 0xB7, 0xC4]); // MOVZX EAX, SP
        self.buf.emit_byte(0x3D); // CMP EAX, imm32
        let span = (self.frame_size as u32).saturating_add(16).min(0xffff);
        self.buf.emit(&span.to_le_bytes());
        self.buf.emit(&[0x0F, 0x87]); // JA .ok
        let fast_skip_patch = self.buf.pos();
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
        // .helper:
        if let Some(p) = exact_low_patch {
            let rel = self.buf.pos() as i32 - (p as i32 + 4);
            Self::patch_or_bail(&mut self.buf, p, rel);
        }
        self.load_context(CALL_ARG_REGS[0]);
        self.emit_mov_reg_imm_smart(CALL_ARG_REGS[1], 0);
        self.emit_mov_reg_imm64(RAX, self.self_call_stack_guard as u64);
        self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
        self.anchor_safepoint_return(map.as_mut(), self.buf.pos());
        self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
        self.buf.emit(&[0x0F, 0x85]); // JNE call-exception exit
        let patch = self.buf.pos();
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
        self.push_stack_guard_exc_patch(patch);
        // As the self-call sample: the marshal below reads the homes, so
        // refresh them from this site's shadow publication (the guard reaches
        // no safepoint today; this keeps the route independent of that).
        self.emit_pending_shadow_copy_back();
        // .ok (tolerated on an overflowed buffer; see `Self::patch_or_bail`):
        for p in [Some(fast_skip_patch), exact_ok_patch]
            .into_iter()
            .flatten()
        {
            let rel = self.buf.pos() as i32 - (p as i32 + 4);
            Self::patch_or_bail(&mut self.buf, p, rel);
        }
    }

    /// IR inline-cache lowering — serve a virtual / interface `Op::Call` from a
    /// monomorphic (`JitMICSlot`) guard, then a 4-way polymorphic
    /// (`JitPICSlot`) cascade, then the resolving `jit_invoke_virtual_mic`
    /// helper. This is the IR analogue of the single-pass backend's MIC/PIC
    /// codegen in `x64.rs`.
    ///
    /// # Why this matters
    ///
    /// `emit_direct_cross_call` removed the per-call helper tax for the
    /// STATICALLY BOUND kinds, but left `invokevirtual` / `invokeinterface`
    /// paying a full `jit_invoke_dispatch` round trip *plus* a dynamic
    /// vtable/itable lookup on every single call — something the single-pass
    /// backend has never done. That asymmetry is why virtual-call lowering was
    /// gated opt-IN (`CRATONVM_JIT_IR_CALL_VIRTUAL`) and why
    /// `c2_upgrade_would_engage` refused any method containing a virtual call:
    /// admitting them made the "optimizing" body slower than the C1 body it
    /// replaced. With this, an IR virtual site costs a load, a compare and an
    /// indirect call on the monomorphic path — the same as single-pass.
    ///
    /// # Shape
    ///
    /// ```text
    ///   MOV  RAX, [rbp - recv_slot]                  ; receiver = arg0
    ///   TEST RAX, RAX ; JZ .slow                     ; null → helper raises NPE
    ///   MOV  EAX, dword [RAX]                        ; class_id (ObjectHeader+0)
    ///   ; ── monomorphic ──
    ///   MOV  R10, imm64 mic
    ///   CMP  EAX, [R10 + CACHED_CLASS_ID_OFFSET]     ; JNE .pic
    ///   MOV  R11, [R10 + CACHED_ENTRY_PTR_OFFSET]    ; tagged word, loaded ONCE
    ///   TEST R11, R11 ; JZ .slow
    ///   BTR  R11, 0                                  ; CF = needs-context
    ///   JNC  .mic_noctx ; <marshal context ABI> ; JMP .mic_call
    /// .mic_noctx: <marshal context-free ABI>
    /// .mic_call: CALL R11 ; JMP .done
    ///   ; ── polymorphic, 4-way ──
    /// .pic:
    ///   MOV  R10, imm64 pic
    ///   for i in 0..4:
    ///     CMP EAX, [R10+CLASS_ID_OFFSETS[i]]   ; JNE .pic_{i+1} (last: .slow)
    ///     MOV R11, [R10+ENTRY_PTR_OFFSETS[i]]  ; TEST R11,R11 ; JZ .slow
    ///     BTR R11, 0 ; JNC .entry_noctx ; <marshal context ABI> ; JMP .entry_call
    ///   .entry_noctx: <marshal context-free ABI>
    ///   .entry_call: CALL R11 ; JMP .done
    ///   ; ── megamorphic / cold ──
    /// .slow:
    ///   <marshal args into the frame staging region>
    ///   jit_invoke_virtual_mic(vm, info, args_ptr, num_args, mic, pic)
    /// .done:
    ///   <emit_call_return_check>                     ; shared sentinel + spill
    /// ```
    ///
    /// EAX carries the receiver class id from the single header load through
    /// the whole cascade: no guard writes RAX, and the ABI marshalling only
    /// touches [`ENTRY_ABI_REGS`] (which excludes RAX, R10 and R11 on both
    /// platforms), so both the class id and the slot base survive to their uses.
    ///
    /// Cold sites cost nothing: an unpopulated MIC holds
    /// `JitMICSlot::EMPTY_CLASS_ID` and an unpopulated PIC way
    /// `EMPTY_WAY_CLASS_ID`, values above every allocatable class id, so no
    /// receiver matches them -- not even a `ClassId(0)` (`java/lang/Object`)
    /// one, which IS a legal receiver; a profile seed stores a class id with
    /// entry word 0, which the `JZ .slow` after the entry load sends to the
    /// helper. Every guard therefore falls through to the helper, which
    /// populates the caches, after which later invocations take the inline
    /// path with no recompile. Exactly the eager-allocation strategy `lib.rs`
    /// already uses for single-pass (HIGH-7). (Wave 15, lane hashmap: this
    /// said an empty slot held `0` and that class 0 is never a receiver; both
    /// stopped being true in round 9 wave 2.)
    ///
    /// All forward branches use rel32. The single-pass inline caches have had
    /// two separate CRIT bugs from rel8 displacement overflow (CRIT-3, and the
    /// inter-slot `jne`); a few extra bytes per site is the right trade.
    ///
    /// Every branch is patched before this function returns, so no per-site
    /// patch state escapes into the shared stub-patching phase.
    #[allow(clippy::too_many_arguments)]
    pub(in crate::ir_lower) fn emit_method_invoke_cache_call(
        &mut self,
        inputs: &[NodeId],
        slot: i32,
        num_args: usize,
        info_ptr: usize,
        ty: IrType,
        map: &mut Option<SafepointMapToken>,
    ) {
        let mut done_patches = Vec::new();
        let mut raw_slow_patches = Vec::new();
        let hits_republished = !ic_frame_republish_disabled();
        let mut hit_patches = Vec::new();
        let mut tail_patches = Vec::new();
        self.emit_ic_stack_sample(map);
        self.emit_method_invoke_cache_fast_path(
            inputs,
            num_args,
            info_ptr,
            hits_republished,
            &mut raw_slow_patches,
            &mut done_patches,
            &mut tail_patches,
            &mut hit_patches,
            map,
        );
        self.emit_inline_cache_mega_region(
            inputs,
            num_args,
            0,
            0,
            info_ptr,
            false,
            Vec::new(),
            Vec::new(),
            raw_slow_patches,
            done_patches,
            tail_patches,
            hit_patches,
            ty,
            slot,
            map,
        );
    }

    pub(in crate::ir_lower) fn is_method_invoke_0(&self, info_ptr: usize, num_args: usize) -> bool {
        if info_ptr == 0
            || num_args != 3
            || self.direct_helpers.reflect_method_invoke_cache == 0
            || self.direct_helpers.reflect_method_invoke_sentinel == 0
            || crate::x64::zgc_read_barrier_blocks_inline_fields()
        {
            return false;
        }
        // SAFETY: the planner keeps each nonzero call row alive through lowering.
        let info = unsafe { &*(info_ptr as *const crate::JitInvokeInfo) };
        matches!(info.invoke_kind, 0 | 1)
            && info.declaring_class_id != 0
            && info.class_name == "java/lang/reflect/Method"
            && info.method_name == "invoke"
            && info.descriptor == "(Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object;"
    }

    #[allow(clippy::too_many_arguments)]
    pub(in crate::ir_lower) fn emit_method_invoke_cache_fast_path(
        &mut self,
        inputs: &[NodeId],
        num_args: usize,
        info_ptr: usize,
        hits_republished: bool,
        raw_slow_patches: &mut Vec<usize>,
        done_patches: &mut Vec<usize>,
        tail_patches: &mut Vec<usize>,
        hit_patches: &mut Vec<usize>,
        map: &mut Option<SafepointMapToken>,
    ) {
        // SAFETY: callers admit the planner-owned call row with `is_method_invoke_0`.
        let info = unsafe { &*(info_ptr as *const crate::JitInvokeInfo) };
        let caller_cid = info.declaring_class_id;
        if self.direct_helpers.reflect_hidden_observed_addr != 0 {
            self.emit_mov_reg_imm64(RAX, self.direct_helpers.reflect_hidden_observed_addr as u64);
            self.buf.emit(&[0xF6, 0x00, 0x01]);
            raw_slow_patches.push(self.emit_jcc_rel32(0x85));
        }
        // args (inputs[4]) must be null
        self.gp_load_value(RAX, inputs[4]);
        self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
        raw_slow_patches.push(self.emit_jcc_rel32(0x85)); // JNZ .slow

        self.gp_load_value(RAX, inputs[2]); // Method
        self.buf.emit(&[0x49, 0x89, 0xC3]); // MOV R11, RAX
        self.buf.emit(&[0x49, 0xC1, 0xEB, 0x04]); // SHR R11, 4
        self.buf.emit(&[0x41, 0x83, 0xE3, 0x3F]); // AND R11D, 63
        self.buf.emit(&[0x41, 0xC1, 0xE3, 0x05]); // SHL R11D, 5
        self.emit_mov_reg_imm64(R10, self.direct_helpers.reflect_method_invoke_cache as u64);
        self.buf.emit(&[0x4D, 0x01, 0xDA]); // ADD R10, R11
        self.buf.emit(&[0x49, 0x3B, 0x42, 0x08]); // CMP RAX, [R10 + 8]
        raw_slow_patches.push(self.emit_jcc_rel32(0x85));
        // CMP DWORD PTR [R10 + 20], caller_cid -> 41 81 7A 14 <imm32>
        self.buf.emit(&[0x41, 0x81, 0x7A, 0x14]);
        self.buf.emit(&caller_cid.to_le_bytes());
        raw_slow_patches.push(self.emit_jcc_rel32(0x85));

        self.buf.emit(&[0x49, 0x8B, 0x0A]); // MOV RCX, [R10]
        self.buf.emit(&[0xF6, 0xC1, 0x01]); // TEST CL, 1
        raw_slow_patches.push(self.emit_jcc_rel32(0x84));
        self.buf.emit(&[0x48, 0x89, 0xC8]); // MOV RAX, RCX
        self.buf.emit(&[0x48, 0xC1, 0xE8, 0x10]); // SHR RAX, 16
        self.emit_mov_reg_imm64(R11, self.direct_helpers.reflect_gc_epoch_addr as u64);
        self.buf.emit(&[0x49, 0x3B, 0x03]); // CMP RAX, [R11]
        raw_slow_patches.push(self.emit_jcc_rel32(0x85));

        self.buf.emit(&[0x89, 0xC8]);
        self.buf.emit(&[0xC1, 0xE8, 0x08]);
        self.buf.emit(&[0x25, 0xFF, 0x00, 0x00, 0x00]);
        self.store_rax(self.args_stage_top_off);
        self.buf.emit(&[0xF6, 0xC1, 0x02]); // TEST CL, 2
        let is_static_patch = self.emit_jcc_rel32(0x85);

        self.gp_load_value(RAX, inputs[3]); // receiver
        self.buf.emit(&[0x48, 0x85, 0xC0]);
        raw_slow_patches.push(self.emit_jcc_rel32(0x84));
        self.buf.emit(&[
            0xF6,
            0x40,
            cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8,
            cratonvm_types::KIND_TAG_BYTE_MASK,
        ]);
        raw_slow_patches.push(self.emit_jcc_rel32(0x85));
        self.buf.emit(&[0x8B, 0x00]);
        self.buf.emit(&[0x41, 0x3B, 0x42, 0x10]);
        raw_slow_patches.push(self.emit_jcc_rel32(0x85));
        self.buf.emit(&[0x4D, 0x8B, 0x5A, 0x18]);
        self.buf.emit(&[0x4D, 0x85, 0xDB]);
        raw_slow_patches.push(self.emit_jcc_rel32(0x84));
        self.buf.emit(&[0x49, 0x0F, 0xBA, 0xF3, 0x00]);
        let inst_noctx = self.emit_jcc_rel32(0x83);
        self.load_context(ENTRY_ABI_REGS[0]);
        self.gp_load_abi_value(ENTRY_ABI_REGS[1], inputs[3]);
        let inst_call = self.emit_jmp_rel32();
        self.patch_rel32_to_here(inst_noctx);
        self.gp_load_abi_value(ENTRY_ABI_REGS[0], inputs[3]);
        let to_call_from_inst = self.emit_jmp_rel32();

        self.patch_rel32_to_here(is_static_patch);
        self.buf.emit(&[0x4D, 0x8B, 0x5A, 0x18]);
        self.buf.emit(&[0x4D, 0x85, 0xDB]);
        raw_slow_patches.push(self.emit_jcc_rel32(0x84));
        self.buf.emit(&[0x49, 0x0F, 0xBA, 0xF3, 0x00]);
        let stat_noctx = self.emit_jcc_rel32(0x83);
        self.load_context(ENTRY_ABI_REGS[0]);
        self.patch_rel32_to_here(stat_noctx);
        self.patch_rel32_to_here(inst_call);
        self.patch_rel32_to_here(to_call_from_inst);

        self.emit_call_loaded_ic_entry(map);
        self.emit_cmp_rax_sentinel();
        let not_sentinel = self.emit_jcc_rel32(0x81); // JNO .box
        // Sentinel cold: stage inputs[2..2+num_args] and call reflect_method_invoke_sentinel
        for i in 0..num_args {
            let arg = inputs[2 + i];
            self.gp_load_abi_value(RAX, arg);
            self.store_rax(self.args_stage_top_off - (i as i32) * 8);
        }
        self.load_context(CALL_ARG_REGS[0]);
        self.emit_mov_reg_imm64(CALL_ARG_REGS[1], info_ptr as u64);
        self.lea_reg_from_frame(CALL_ARG_REGS[2], self.args_stage_top_off);
        self.emit_mov_reg_imm64(CALL_ARG_REGS[3], num_args as u64);
        self.emit_mov_reg_imm64(RAX, self.direct_helpers.reflect_method_invoke_sentinel as u64);
        self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
        self.anchor_safepoint_return(map.as_mut(), self.buf.pos());
        self.emit_post_call_frame_record();
        tail_patches.push(self.emit_jmp_rel32());
        self.patch_rel32_to_here(not_sentinel);

        self.load_reg_from_frame(RCX, self.args_stage_top_off);
        self.buf.emit(&[0x83, 0xF9, crate::direct_helpers::REFLECT_RET_OBJ]);
        if hits_republished {
            hit_patches.push(self.emit_jcc_rel32(0x84));
        } else {
            done_patches.push(self.emit_jcc_rel32(0x84));
        }
        self.buf.emit(&[0x83, 0xF9, crate::direct_helpers::REFLECT_RET_BOOL]);
        let not_bool = self.emit_jcc_rel32(0x85);
        self.buf.emit(&[0xA8, 0x01]);
        self.emit_mov_reg_imm64(R11, self.direct_helpers.reflect_boolean_false_addr as u64);
        self.buf.emit(&[0x49, 0x8B, 0x03]);
        let bool_done = self.emit_jcc_rel32(0x84);
        self.emit_mov_reg_imm64(R11, self.direct_helpers.reflect_boolean_true_addr as u64);
        self.buf.emit(&[0x49, 0x8B, 0x03]);
        self.patch_rel32_to_here(bool_done);
        if hits_republished {
            hit_patches.push(self.emit_jmp_rel32());
        } else {
            done_patches.push(self.emit_jmp_rel32());
        }
        self.patch_rel32_to_here(not_bool);
        self.buf.emit(&[0x85, 0xC9]);
        let not_void = self.emit_jcc_rel32(0x85);
        self.buf.emit(&[0x31, 0xC0]);
        if hits_republished {
            hit_patches.push(self.emit_jmp_rel32());
        } else {
            done_patches.push(self.emit_jmp_rel32());
        }
        self.patch_rel32_to_here(not_void);
        #[cfg(target_os = "windows")]
        self.buf.emit(&[0x48, 0x63, 0xD0]);
        #[cfg(not(target_os = "windows"))]
        self.buf.emit(&[0x48, 0x63, 0xF0]);
        self.load_context(CALL_ARG_REGS[0]);
        self.emit_mov_reg_imm64(RAX, self.direct_helpers.integer_value_of as u64);
        self.buf.emit(&[0xFF, 0xD0]);
        self.anchor_safepoint_return(map.as_mut(), self.buf.pos());
        tail_patches.push(self.emit_jmp_rel32());
    }

    pub(in crate::ir_lower) fn is_dmha_invoke_exact_0(&self, info_ptr: usize, num_args: usize) -> bool {
        if info_ptr == 0
            || num_args != 2
            || self.direct_helpers.dmha_class_id == 0
            || self.direct_helpers.reflect_invoke_cache == 0
            || crate::x64::zgc_read_barrier_blocks_inline_fields()
        {
            return false;
        }
        // SAFETY: the planner keeps each nonzero call row alive through lowering.
        let info = unsafe { &*(info_ptr as *const crate::JitInvokeInfo) };
        info.invoke_kind == 0
            && info.declaring_class_id == self.direct_helpers.dmha_class_id
            && info.class_name == "java/lang/invoke/MethodHandle"
            && info.method_name == "invokeExact"
            && info.descriptor == "(Ljava/lang/Object;)Ljava/lang/Object;"
    }

    #[allow(clippy::too_many_arguments)]
    pub(in crate::ir_lower) fn emit_reflect_invoke_cache_fast_path(
        &mut self,
        inputs: &[NodeId],
        num_args: usize,
        info_ptr: usize,
        hits_republished: bool,
        raw_slow_patches: &mut Vec<usize>,
        done_patches: &mut Vec<usize>,
        tail_patches: &mut Vec<usize>,
        hit_patches: &mut Vec<usize>,
        map: &mut Option<SafepointMapToken>,
    ) {
        if self.direct_helpers.reflect_hidden_observed_addr != 0 {
            self.emit_mov_reg_imm64(RAX, self.direct_helpers.reflect_hidden_observed_addr as u64);
            self.buf.emit(&[0xF6, 0x00, 0x01]); // TEST BYTE PTR [RAX], 1
            raw_slow_patches.push(self.emit_jcc_rel32(0x85)); // JNZ .slow
        }
        self.gp_load_value(RAX, inputs[2]);
        self.buf.emit(&[0x49, 0x89, 0xC3]); // MOV R11, RAX
        self.buf.emit(&[0x49, 0xC1, 0xEB, 0x04]); // SHR R11, 4
        self.buf.emit(&[0x41, 0x83, 0xE3, 0x3F]); // AND R11D, 63
        self.buf.emit(&[0x41, 0xC1, 0xE3, 0x05]); // SHL R11D, 5
        self.emit_mov_reg_imm64(R10, self.direct_helpers.reflect_invoke_cache as u64);
        self.buf.emit(&[0x4D, 0x01, 0xDA]); // ADD R10, R11
        self.buf.emit(&[0x49, 0x3B, 0x42, 0x08]); // CMP RAX, [R10 + 8]
        raw_slow_patches.push(self.emit_jcc_rel32(0x85)); // JNE .slow
        self.buf.emit(&[0x49, 0x8B, 0x0A]); // MOV RCX, [R10]
        self.buf.emit(&[0xF6, 0xC1, 0x01]); // TEST CL, 1
        raw_slow_patches.push(self.emit_jcc_rel32(0x84)); // JZ .slow
        self.buf.emit(&[0x48, 0x89, 0xC8]); // MOV RAX, RCX
        self.buf.emit(&[0x48, 0xC1, 0xE8, 0x10]); // SHR RAX, 16
        self.emit_mov_reg_imm64(R11, self.direct_helpers.reflect_gc_epoch_addr as u64);
        self.buf.emit(&[0x49, 0x3B, 0x03]); // CMP RAX, [R11]
        raw_slow_patches.push(self.emit_jcc_rel32(0x85)); // JNE .slow
        self.buf.emit(&[0x89, 0xC8]); // MOV EAX, ECX
        self.buf.emit(&[0xC1, 0xE8, 0x08]); // SHR EAX, 8
        self.buf.emit(&[0x25, 0xFF, 0x00, 0x00, 0x00]); // AND EAX, 0xFF
        self.store_rax(self.args_stage_top_off);
        self.buf.emit(&[0xF6, 0xC1, 0x02]); // TEST CL, 2
        let is_static_patch = self.emit_jcc_rel32(0x85); // JNZ .is_static
        self.gp_load_value(RAX, inputs[3]);
        self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
        raw_slow_patches.push(self.emit_jcc_rel32(0x84)); // JZ .slow
        self.buf.emit(&[
            0xF6,
            0x40,
            cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8,
            cratonvm_types::KIND_TAG_BYTE_MASK,
        ]);
        raw_slow_patches.push(self.emit_jcc_rel32(0x85)); // JNE .slow
        self.buf.emit(&[0x8B, 0x00]); // MOV EAX, [RAX]
        self.buf.emit(&[0x41, 0x3B, 0x42, 0x10]); // CMP EAX, [R10 + 16]
        raw_slow_patches.push(self.emit_jcc_rel32(0x85)); // JNE .slow
        self.buf.emit(&[0x4D, 0x8B, 0x5A, 0x18]); // MOV R11, [R10 + 24]
        self.buf.emit(&[0x4D, 0x85, 0xDB]); // TEST R11, R11
        raw_slow_patches.push(self.emit_jcc_rel32(0x84)); // JZ .slow
        self.buf.emit(&[0x49, 0x0F, 0xBA, 0xF3, 0x00]); // BTR R11, 0
        let inst_noctx = self.emit_jcc_rel32(0x83); // JNC .inst_noctx
        self.load_context(ENTRY_ABI_REGS[0]);
        self.gp_load_abi_value(ENTRY_ABI_REGS[1], inputs[3]);
        let inst_call = self.emit_jmp_rel32();
        self.patch_rel32_to_here(inst_noctx);
        self.gp_load_abi_value(ENTRY_ABI_REGS[0], inputs[3]);
        let to_call_from_inst = self.emit_jmp_rel32();

        self.patch_rel32_to_here(is_static_patch);
        self.buf.emit(&[0x4D, 0x8B, 0x5A, 0x18]); // MOV R11, [R10 + 24]
        self.buf.emit(&[0x4D, 0x85, 0xDB]); // TEST R11, R11
        raw_slow_patches.push(self.emit_jcc_rel32(0x84)); // JZ .slow
        self.buf.emit(&[0x49, 0x0F, 0xBA, 0xF3, 0x00]); // BTR R11, 0
        let stat_noctx = self.emit_jcc_rel32(0x83); // JNC .call
        self.load_context(ENTRY_ABI_REGS[0]);
        self.patch_rel32_to_here(stat_noctx);
        self.patch_rel32_to_here(inst_call);
        self.patch_rel32_to_here(to_call_from_inst);

        self.emit_call_loaded_ic_entry(map);
        if self.service_callee_deopt != 0 {
            self.emit_cmp_rax_sentinel();
            let not_sentinel = self.emit_jcc_rel32(0x81); // JNO .box
            self.emit_callee_deopt_service_cold(info_ptr, num_args, inputs, false, map);
            tail_patches.push(self.emit_jmp_rel32());
            self.patch_rel32_to_here(not_sentinel);
        }
        self.load_reg_from_frame(RCX, self.args_stage_top_off);
        self.buf.emit(&[0x83, 0xF9, crate::direct_helpers::REFLECT_RET_OBJ]);
        if hits_republished {
            hit_patches.push(self.emit_jcc_rel32(0x84));
        } else {
            done_patches.push(self.emit_jcc_rel32(0x84));
        }
        self.buf.emit(&[0x83, 0xF9, crate::direct_helpers::REFLECT_RET_BOOL]);
        let not_bool = self.emit_jcc_rel32(0x85);
        self.buf.emit(&[0xA8, 0x01]); // TEST AL, 1
        self.emit_mov_reg_imm64(R11, self.direct_helpers.reflect_boolean_false_addr as u64);
        self.buf.emit(&[0x49, 0x8B, 0x03]); // MOV RAX, [R11]
        let bool_done = self.emit_jcc_rel32(0x84); // JZ
        self.emit_mov_reg_imm64(R11, self.direct_helpers.reflect_boolean_true_addr as u64);
        self.buf.emit(&[0x49, 0x8B, 0x03]); // MOV RAX, [R11]
        self.patch_rel32_to_here(bool_done);
        if hits_republished {
            hit_patches.push(self.emit_jmp_rel32());
        } else {
            done_patches.push(self.emit_jmp_rel32());
        }
        self.patch_rel32_to_here(not_bool);
        self.buf.emit(&[0x85, 0xC9]); // TEST ECX, ECX
        let not_void = self.emit_jcc_rel32(0x85);
        self.buf.emit(&[0x31, 0xC0]); // XOR EAX, EAX
        if hits_republished {
            hit_patches.push(self.emit_jmp_rel32());
        } else {
            done_patches.push(self.emit_jmp_rel32());
        }
        self.patch_rel32_to_here(not_void);
        #[cfg(target_os = "windows")]
        self.buf.emit(&[0x48, 0x63, 0xD0]);
        #[cfg(not(target_os = "windows"))]
        self.buf.emit(&[0x48, 0x63, 0xF0]);
        self.load_context(CALL_ARG_REGS[0]);
        self.emit_mov_reg_imm64(RAX, self.direct_helpers.integer_value_of as u64);
        self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
        self.anchor_safepoint_return(map.as_mut(), self.buf.pos());
        tail_patches.push(self.emit_jmp_rel32());
    }

    pub(in crate::ir_lower) fn emit_inline_cache_call(
        &mut self,
        inputs: &[NodeId],
        slot: i32,
        num_args: usize,
        mic: usize,
        pic: usize,
        info_ptr: usize,
        ty: IrType,
        map: &mut Option<SafepointMapToken>,
    ) {
        use crate::JitPICSlot;

        debug_assert!(
            num_args >= 1,
            "a virtual/interface site always marshals a receiver as arg0"
        );
        // The `Op::Call` arm admits only this shape; refuse rather than index
        // past `ENTRY_ABI_REGS` below if a future caller does not.
        if !ir_ic_admits_arity(num_args) {
            self.latch_bailout(Bailout::new(BailoutReason::Internal(
                "ir_lower: an inline cache was asked for a site ir_ic_admits_arity refuses",
            )));
            return;
        }
        // This is the ONLY helper the IR lowerer calls with more than four
        // arguments, so it is the only place the Win64 stack-argument area
        // `[RSP+32, RSP+48)` is ever written. Pin that the arg-staging region
        // starts above it — if the two ever overlap again, `mic`/`pic` land on
        // the receiver and first argument of the call being dispatched. See the
        // frame-layout fix in `Lowerer::new`.
        #[cfg(target_os = "windows")]
        debug_assert!(
            self.args_stage_top_off <= self.frame_size - 48,
            "ir_lower: arg-staging region at rbp-{} overlaps the Win64 stack-argument \
             area for the 6-argument jit_invoke_virtual_mic call (frame_size {})",
            self.args_stage_top_off,
            self.frame_size,
        );
        let mut done_patches: Vec<usize> = Vec::new();
        let mut slow_patches: Vec<usize> = Vec::new();
        // Round 13 wave 5 (lane callcost2, M13-2): the two `.slow` edges that
        // do NOT hold the receiver's class id in EAX (a null receiver, a
        // non-plain-object one). Every other edge into the megamorphic region
        // does, and with `gate_entry` it enters the hashed stub's gate entry
        // instead of its full one (see the stub section below).
        let mut raw_slow_patches: Vec<usize> = Vec::new();
        let gate_entry = crate::runtime_lowering::mega_gate_fast_entry_enabled()
            && crate::runtime_lowering::hashed_stub_admitted(pic, num_args);
        // Round 11 wave 2 (lane lower, `r11-irlower-ic-hit-republishes-and-
        // tests-the-sentinel-twice`): a cache hit has already republished this
        // frame's record right after its `CALL R11` (`emit_call_loaded_ic_entry`),
        // so it joins `.done` AFTER the republish there instead of paying the
        // two TLS stores a second time. Only when that republish was emitted:
        // under the kill switch the hits keep landing on `.done`'s own.
        let hits_republished = !ic_frame_republish_disabled();
        let mut hit_patches: Vec<usize> = Vec::new();
        // A hit whose callee-deopt service ran (and may have answered the
        // sentinel again) joins at the sentinel tail, after `.done`'s republish
        // and before `.hit_join`. See `emit_ic_hit_exit`.
        let mut tail_patches: Vec<usize> = Vec::new();
        // Round 13 wave 2 (lane mega, `r13w1-crash-ic-recursion-stack-guard-patch`
        // patch 3): every path out of this site -- MIC, PIC, the hashed stub --
        // CALLs a compiled body raw, so a recursion closed through such sites
        // needs a stack check here. Emitted before the pre-marshal: it clobbers
        // RAX and, on its cold side, every caller-saved register.
        self.emit_ic_stack_sample(map);
        if self.is_method_invoke_0(info_ptr, num_args) {
            self.emit_method_invoke_cache_fast_path(
                inputs,
                num_args,
                info_ptr,
                hits_republished,
                &mut raw_slow_patches,
                &mut done_patches,
                &mut tail_patches,
                &mut hit_patches,
                map,
            );
            return self.emit_inline_cache_mega_region(
                inputs,
                num_args,
                mic,
                pic,
                info_ptr,
                false,
                Vec::new(),
                Vec::new(),
                raw_slow_patches,
                done_patches,
                tail_patches,
                hit_patches,
                ty,
                slot,
                map,
            );
        }
        if self.is_dmha_invoke_exact_0(info_ptr, num_args) {
            self.emit_reflect_invoke_cache_fast_path(
                inputs,
                num_args,
                info_ptr,
                hits_republished,
                &mut raw_slow_patches,
                &mut done_patches,
                &mut tail_patches,
                &mut hit_patches,
                map,
            );
            return self.emit_inline_cache_mega_region(
                inputs,
                num_args,
                mic,
                pic,
                info_ptr,
                false,
                Vec::new(),
                Vec::new(),
                raw_slow_patches,
                done_patches,
                tail_patches,
                hit_patches,
                ty,
                slot,
                map,
            );
        }
        // The staging loop that used to run HERE — N loads and N stores ahead
        // of the guards, on every execution of every virtual call site — is
        // gone. Its three readers now each populate the block on their own cold
        // side:
        //
        //   * a MIC or PIC hit that returns the deopt sentinel — staged inside
        //     `emit_inline_callee_deopt_service`, past its `JNE .done`;
        //   * the shared hashed/vtable stub — staged immediately before it, in
        //     the megamorphic region rather than ahead of the monomorphic
        //     guard;
        //   * the resolving slow helper — which already re-staged for itself,
        //     which is what made the copy up here redundant even before this.
        //
        // All three read the same frame slots, and nothing between this point
        // and any of them writes those slots.

        // Marshal the Java arguments ONCE, in the no-context layout, before the
        // guards. Each cache arm then either calls straight through or shifts
        // the layout up one register — see `emit_ic_premarshal_no_context` for
        // why a layout established here survives the cascade (its guards touch
        // only RAX, R10 and R11).
        // Under the eager shape the whole pre-marshal/shift scheme is off and
        // each arm marshals for itself, exactly as before 2026-09-02.
        //
        // Round 11 wave 13 (lane `icwide`): an OVER-WIDE site (its arguments
        // plus the context word overflow `ENTRY_ABI_REGS`) takes neither: the
        // no-context layout does not fit the file, and the context shift
        // assumes it does. Every hit of such a site instead jumps to ONE
        // marshalling tail (`emit_ic_wide_hit_call`) that reserves the
        // outgoing stack block, writes the overflow words and the registers
        // for the ABI the hit selected, and CALLs. Sites that fit are
        // byte-identical to before.
        let wide_block = Self::ic_wide_stack_block(num_args);
        let cold_stage = true;
        let premarshal = cold_stage && wide_block.is_none();
        // JIT round 18 wave 11 (lane mega5) / round 19 wave 2 (lane mega2):
        // the megamorphic gate's admission and target, decided once (see
        // below, where the gate is emitted). Pure functions of the site.
        let is_bound_special = info_ptr != 0
            // SAFETY: the planner keeps each nonzero call row alive through lowering.
            && unsafe { (*(info_ptr as *const crate::JitInvokeInfo)).invoke_kind } == 1;
        let gate_admitted =
            !is_bound_special && crate::runtime_lowering::megamorphic_gate_admitted(pic, num_args);
        let stage_once =
            gate_admitted && crate::runtime_lowering::mega_stage_once_admitted(pic, num_args);
        let gate_to_probe = gate_admitted
            && (stage_once
                || crate::runtime_lowering::mega_vtable_column_admitted(
                    pic,
                    num_args,
                    Self::ic_site_is_interface(info_ptr),
                ));
        // JIT round 19 wave 2 (lane mega2, MEGA19-4,
        // `CRATONVM_JIT_MEGA_SEEDED_SKIP_INLINE`): a slot already megamorphic
        // at emission (seeded from the receiver profile) never clears, so its
        // gate would be taken on every call: emit neither the MIC nor the
        // rungs, and go straight to the megamorphic region. Not for an
        // over-wide site, whose hit tail (`wide_tail`) lives in the MIC arm.
        let skip_inline = gate_admitted
            && wide_block.is_none()
            && crate::runtime_lowering::mega_seeded_skips_inline_ways(pic, num_args);
        // MEGA19-4: a skipped site that has no register / column probe
        // makes no call from the pre-marshalled registers (the stub
        // re-marshals from the frame), so it pre-marshals nothing.
        if premarshal && !(skip_inline && !gate_to_probe) {
            self.emit_ic_premarshal_no_context(inputs, num_args);
        } else if !cold_stage {
            for i in 0..num_args {
                let arg = inputs[2 + i];
                self.gp_load_abi_value(RAX, arg);
                self.store_rax(self.args_stage_top_off - (i as i32) * 8);
            }
        }

        // Receiver = arg0. Load it and its class id ONCE for the whole cascade.
        // RAX is not an `ENTRY_ABI_REGS` member on either ABI, so this does not
        // disturb the layout just built.
        self.gp_load_value(RAX, inputs[2]);
        self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
        raw_slow_patches.push(self.emit_jcc_rel32(0x84)); // JZ .slow (raw)
                                                          // Array-receiver guard — `ObjectHeader.class_id` (offset 0) holds a
                                                          // reference array's COMPONENT class id, so `Foo[]` and `Foo` share the
                                                          // guard word every cached entry below is compared against. Without the
                                                          // `kind` check a `Foo[]` receiver is dispatched into `Foo`'s method
                                                          // body. See the matching guard in the single-pass cascade
                                                          // (`x64/op_invoke.rs`).
                                                          //   TEST BYTE [RAX+KIND_TAGS_BYTE_OFFSET], KIND_TAG_BYTE_MASK (F6 /0 ib: kind bits only)
                                                          //
                                                          // Round 13 wave 11 (lane mega9, proposal M13-6): skipped where no
                                                          // array can be the receiver, as the single-pass cascade has done since
                                                          // round 11 (`ic_receiver_may_be_array`): a real `invokevirtual` whose
                                                          // owner is neither `Object`, `Cloneable`, `Serializable` nor an array
                                                          // type has a receiver the verifier held to that class, so the screen
                                                          // is a compare that never fails. Every edge into the megamorphic
                                                          // region still arrives with a non-array receiver's class id in EAX,
                                                          // the stub gate entry's contract, and the stub's full entry keeps its
                                                          // own screen for the null edge.
        // t1 w7 NBD, DISPATCH21-4 (`CRATONVM_JIT_IR_IC_CAST_KIND_SCREEN_ELIDE`,
        // default ON since j27 2026-10-28): an interface (or `Object`-owner) site whose receiver a
        // dominating `checkcast` to a non-array type has proven a plain
        // object skips the screen as well (`ic_receiver_cast_excludes_arrays`).
        let cast_proves_plain = Self::ic_receiver_may_be_array(info_ptr)
            && ic_receiver_cast_excludes_arrays(self.graph, inputs[1], inputs[2]);
        if cast_proves_plain && crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] ir ic kind screen elided by a dominating checkcast: receiver n{}",
                inputs[2]
            );
        }
        if Self::ic_receiver_may_be_array(info_ptr) && !cast_proves_plain {
            self.buf.emit(&[
                0xF6,
                0x40,
                cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8,
                cratonvm_types::KIND_TAG_BYTE_MASK,
            ]);
            raw_slow_patches.push(self.emit_jcc_rel32(0x85)); // JNE .slow (raw)
        }
        self.buf.emit(&[0x8B, 0x00]); // MOV EAX, dword [RAX]

        // JIT round 18 wave 11 (lane mega5): the megamorphic gate's admission
        // and target, decided once (above, ahead of the pre-marshal since
        // round 19 wave 2). `gate_to_probe`: the gate goes to an
        // inline probe after the rungs (the round-17 vtable column, or under
        // `CRATONVM_JIT_MEGA_STAGE_ONCE` the stub's whole probe in registers,
        // THROWS18-5) instead of to `.slow`. `gate_first`
        // (`CRATONVM_JIT_MEGA_GATE_FIRST`, THROWS18-7): the gate is tested
        // HERE, ahead of the MIC, with R10 = the PIC -- the state the rungs'
        // gate has, so both targets take it unchanged.
        let gate_first =
            gate_admitted && !skip_inline && crate::runtime_lowering::mega_gate_first_enabled();
        // JIT round 19 (lane mega19, MEGA5-18-3,
        // `CRATONVM_JIT_MEGA_PROBE_MISS_TO_HELPER`): a register-probe miss
        // stages and goes to the resolving helper, skipping the stub's second
        // probe of the same tables.
        let probe_miss_to_helper =
            stage_once && crate::runtime_lowering::mega_probe_miss_to_helper_enabled();
        let mut probe_miss_patches: Vec<usize> = Vec::new();
        let mut gate_patch: Option<usize> = None;
        if gate_first {
            // JIT round 19 (lane mega19, MEGA5-18-4): this site's code reads
            // neither its MIC nor an inline way once the gate is set, so the
            // helper may stop publishing there
            // (`CRATONVM_JIT_MEGA_GATE_FIRST_NO_INLINE_PUBLISH`).
            if crate::runtime_lowering::mega_gate_first_no_inline_publish_enabled() {
                // SAFETY: `gate_admitted` implies a non-null `pic`
                // (`hashed_stub_admission` refuses 0): the slot this compile's
                // site owns, alive for the compile and retained by the artifact.
                unsafe { (*(pic as *const JitPICSlot)).note_gate_tested_first() };
            }
            self.emit_mov_reg_imm64(R10, pic as u64);
            gate_patch = Some(crate::runtime_lowering::emit_megamorphic_gate(
                &mut self.buf,
            ));
        }
        // MEGA19-4: the MIC and the rungs below are skipped whole; the
        // megamorphic region that follows them is entered by falling
        // through, with EAX = the class id of a screened receiver and (for
        // the register / column probe) R10 = the slot: the gate edge's state.
        let mut wide_tail: Option<usize> = None;
        let mut column_gate: Option<usize> = None;
        if skip_inline {
            if crate::runtime_lowering::mega_gate_first_no_inline_publish_enabled() {
                // As gate-first: this site's code reads neither its MIC nor
                // an inline way (MEGA5-18-4's mark, read by the helper).
                // SAFETY: as the gate-first mark above.
                unsafe { (*(pic as *const JitPICSlot)).note_gate_tested_first() };
            }
            if gate_to_probe {
                self.emit_mov_reg_imm64(R10, pic as u64);
            }
        } else {
            self.emit_inline_cache_inline_ways(
                inputs,
                num_args,
                mic,
                pic,
                info_ptr,
                wide_block,
                premarshal,
                gate_admitted,
                gate_first,
                gate_to_probe,
                hits_republished,
                &mut gate_patch,
                &mut column_gate,
                &mut wide_tail,
                &mut slow_patches,
                &mut done_patches,
                &mut tail_patches,
                &mut hit_patches,
                map,
            );
        }

        // ── Megamorphic vtable column (round 17 wave 1, lane mega, MW13-4) ──
        // Out of line: every rung above ends in its hit exit's jump, so only
        // the gate reaches this. EAX = the class id (zero-extended), R10 =
        // the PIC, the pre-marshalled layout intact: the probe's entry
        // contract. A hit takes a rung's own tail with the cell's word in R11;
        // every miss joins `.slow` with EAX = the class id, the state every
        // other edge there has (`gate_entry`'s contract included: R10 free).
        // Round 18 wave 11 (lane mega5, THROWS18-5): under
        // `CRATONVM_JIT_MEGA_STAGE_ONCE` the probe is the stub's whole probe
        // (cell, hashed ways, shared table) with the same entry and exit
        // contract, so the tail below serves both.
        // MEGA19-4: a skipped site falls into it instead of jumping.
        if column_gate.is_some() || (skip_inline && gate_to_probe) {
            if let Some(gate) = column_gate {
                self.patch_rel32_to_here(gate);
            }
            if stage_once {
                let misses = crate::runtime_lowering::emit_mega_register_probe(
                    &mut self.buf,
                    pic,
                    crate::runtime_lowering::mega_register_probe_cell_first(),
                );
                if probe_miss_to_helper {
                    probe_miss_patches.extend(misses);
                } else {
                    slow_patches.extend(misses);
                }
            } else if let Some(binding) = crate::runtime_lowering::mega_vtable_column_baked_for(
                pic,
                Self::ic_site_is_interface(info_ptr),
            ) {
                // JIT round 21 wave 4, lane ic (IC21-2): the compile-time bind
                // baked as immediates, same contract.
                slow_patches.extend(crate::runtime_lowering::emit_mega_vtable_column_probe_baked(
                    &mut self.buf,
                    binding,
                ));
            } else {
                slow_patches.extend(crate::runtime_lowering::emit_mega_vtable_column_probe(
                    &mut self.buf,
                ));
            }
            self.emit_strip_ic_context_tag();
            if let Some(tail) = wide_tail {
                let back = self.emit_jmp_rel32();
                self.patch_rel32_to_target(back, tail);
            } else {
                if premarshal {
                    let noctx = self.emit_jcc_rel32(0x83); // JNC .entry_ready (no shift)
                    self.emit_ic_shift_for_context(num_args);
                    self.patch_rel32_to_here(noctx);
                } else {
                    let noctx = self.emit_jcc_rel32(0x83); // JNC .entry_noctx
                    self.emit_ic_abi_marshal(inputs, num_args, true);
                    let call = self.emit_jmp_rel32();
                    self.patch_rel32_to_here(noctx);
                    self.emit_ic_abi_marshal(inputs, num_args, false);
                    self.patch_rel32_to_here(call);
                }
                self.emit_call_loaded_ic_entry(map);
                self.emit_ic_hit_exit(
                    info_ptr,
                    num_args,
                    inputs,
                    hits_republished,
                    &mut done_patches,
                    &mut tail_patches,
                    &mut hit_patches,
                    map,
                );
            }
        }
        self.emit_inline_cache_mega_region(
            inputs,
            num_args,
            mic,
            pic,
            info_ptr,
            gate_entry,
            probe_miss_patches,
            slow_patches,
            raw_slow_patches,
            done_patches,
            tail_patches,
            hit_patches,
            ty,
            slot,
            map,
        );
    }

    /// The MIC and the four PIC rungs of [`Self::emit_inline_cache_call`]
    /// (split out in JIT round 19 wave 2, lane mega2, so MEGA19-4 can skip
    /// them whole; byte for byte what the caller emitted inline before).
    #[allow(clippy::too_many_arguments)]
    pub(in crate::ir_lower) fn emit_inline_cache_inline_ways(
        &mut self,
        inputs: &[NodeId],
        num_args: usize,
        mic: usize,
        pic: usize,
        info_ptr: usize,
        wide_block: Option<(i32, i32)>,
        premarshal: bool,
        gate_admitted: bool,
        gate_first: bool,
        gate_to_probe: bool,
        hits_republished: bool,
        gate_patch: &mut Option<usize>,
        column_gate: &mut Option<usize>,
        wide_tail: &mut Option<usize>,
        slow_patches: &mut Vec<usize>,
        done_patches: &mut Vec<usize>,
        tail_patches: &mut Vec<usize>,
        hit_patches: &mut Vec<usize>,
        map: &mut Option<SafepointMapToken>,
    ) {
        use crate::{JitMICSlot, JitPICSlot, JIT_PIC_ENTRIES};

        // ── Monomorphic inline cache ────────────────────────────────
        self.emit_mov_reg_imm64(R10, mic as u64);
        self.emit_cmp_eax_r10_disp(JitMICSlot::CACHED_CLASS_ID_OFFSET as u8);
        let mic_miss = self.emit_jcc_rel32(0x85); // JNE .pic
        self.emit_load_ic_entry_word(JitMICSlot::CACHED_ENTRY_PTR_OFFSET as u8);
        slow_patches.push(self.emit_jcc_rel32(0x84)); // JZ .slow
        self.emit_strip_ic_context_tag();
        // Falls THROUGH on the context case (carry set) and jumps on the
        // no-context one, because the no-context layout is already in place:
        // the fall-through shifts it, the jump does nothing at all. That
        // inverts the old sense of this branch, which is why the target is
        // named for what it skips.
        //
        // An over-wide site's MIC falls into the shared wide tail instead,
        // and every PIC rung below jumps back to it (`wide_tail`) with its
        // own entry in R11 and its own tag in CF.
        if let Some((total_sub, base_disp)) = wide_block {
            *wide_tail = Some(self.buf.pos());
            self.emit_ic_wide_hit_call(inputs, num_args, total_sub, base_disp, map);
        } else if premarshal {
            let mic_noctx = self.emit_jcc_rel32(0x83); // JNC .mic_ready (no shift)
            self.emit_ic_shift_for_context(num_args);
            self.patch_rel32_to_here(mic_noctx);
            self.emit_call_loaded_ic_entry(map);
        } else {
            let mic_noctx = self.emit_jcc_rel32(0x83); // JNC .mic_noctx
            self.emit_ic_abi_marshal(inputs, num_args, true);
            let mic_call = self.emit_jmp_rel32();
            self.patch_rel32_to_here(mic_noctx);
            self.emit_ic_abi_marshal(inputs, num_args, false);
            self.patch_rel32_to_here(mic_call);
            self.emit_call_loaded_ic_entry(map);
        }
        self.emit_ic_hit_exit(
            info_ptr,
            num_args,
            inputs,
            hits_republished,
            done_patches,
            tail_patches,
            hit_patches,
            map,
        );

        // ── Polymorphic 4-way cascade ───────────────────────────────
        // .pic:
        self.patch_rel32_to_here(mic_miss);
        self.emit_mov_reg_imm64(R10, pic as u64);
        // Round 12 wave 6 (lane mega5, W5-4): a site whose PIC has overflowed
        // skips the four rungs and goes to `.slow`, whose hashed stub probes
        // the receiver's class cell first. Flags only; `.slow` is where the
        // last rung's miss lands with this same state. Emitted only where that
        // stub is (`megamorphic_gate_admitted` is its admission; the stub
        // below is emitted for every `num_args >= 1`).
        // JIT round 17 wave 1 (lane mega, MW13-4): under
        // `CRATONVM_JIT_MEGA_VTABLE_COLUMN` the gate goes to the inline
        // vtable-column probe emitted after the rungs, not to `.slow`.
        // Round 18 wave 11 (lane mega5): emitted here unless `gate_first`
        // already tested it ahead of the MIC (see above).
        if gate_admitted && !gate_first {
            *gate_patch = Some(crate::runtime_lowering::emit_megamorphic_gate(
                &mut self.buf,
            ));
        }
        if let Some(gate) = *gate_patch {
            if gate_to_probe {
                *column_gate = Some(gate);
            } else {
                slow_patches.push(gate);
            }
        }
        let mut next_entry: Option<usize> = None;
        for i in 0..JIT_PIC_ENTRIES {
            if let Some(p) = next_entry.take() {
                self.patch_rel32_to_here(p);
            }
            self.emit_cmp_eax_r10_disp(JitPICSlot::CLASS_ID_OFFSETS[i] as u8);
            let miss = self.emit_jcc_rel32(0x85); // JNE → next entry / .slow
            if i + 1 == JIT_PIC_ENTRIES {
                slow_patches.push(miss);
            } else {
                next_entry = Some(miss);
            }
            self.emit_load_ic_entry_word(JitPICSlot::ENTRY_PTR_OFFSETS[i] as u8);
            slow_patches.push(self.emit_jcc_rel32(0x84)); // JZ .slow
            self.emit_strip_ic_context_tag();
            // Over-wide: back to the MIC arm's wide tail. `JMP` leaves CF (the
            // tag `BTR` just moved there) alone, and the tail opens with the
            // `JNC` that reads it; R11 holds THIS rung's bare entry.
            if let Some(tail) = *wide_tail {
                let back = self.emit_jmp_rel32();
                self.patch_rel32_to_target(back, tail);
                continue;
            }
            // Same inversion as the MIC arm above.
            if premarshal {
                let noctx = self.emit_jcc_rel32(0x83); // JNC .entry_ready (no shift)
                self.emit_ic_shift_for_context(num_args);
                self.patch_rel32_to_here(noctx);
            } else {
                let noctx = self.emit_jcc_rel32(0x83); // JNC .entry_noctx
                self.emit_ic_abi_marshal(inputs, num_args, true);
                let call = self.emit_jmp_rel32();
                self.patch_rel32_to_here(noctx);
                self.emit_ic_abi_marshal(inputs, num_args, false);
                self.patch_rel32_to_here(call);
            }
            self.emit_call_loaded_ic_entry(map);
            self.emit_ic_hit_exit(
                info_ptr,
                num_args,
                inputs,
                hits_republished,
                done_patches,
                tail_patches,
                hit_patches,
                map,
            );
        }
        debug_assert!(next_entry.is_none());
    }

    /// The megamorphic region of [`Self::emit_inline_cache_call`]: the
    /// staging edges, the shared hashed stub, the resolving helper and the
    /// site's `.done` / `.tail` / `.hit_join` (split out in JIT round 19 wave
    /// 2, lane mega2; byte for byte what the caller emitted inline before).
    #[allow(clippy::too_many_arguments)]
    pub(in crate::ir_lower) fn emit_inline_cache_mega_region(
        &mut self,
        inputs: &[NodeId],
        num_args: usize,
        mic: usize,
        pic: usize,
        info_ptr: usize,
        gate_entry: bool,
        mut probe_miss_patches: Vec<usize>,
        mut slow_patches: Vec<usize>,
        mut raw_slow_patches: Vec<usize>,
        mut done_patches: Vec<usize>,
        tail_patches: Vec<usize>,
        mut hit_patches: Vec<usize>,
        ty: IrType,
        slot: i32,
        map: &mut Option<SafepointMapToken>,
    ) {
        // ── Shared compact hashed/vtable stub ─────────────────────────────
        // Round 13 wave 5 (lane callcost2, M13-2): every edge in
        // `slow_patches` arrives with EAX = the class id of a non-null,
        // kind-screened receiver (the MIC's and each rung's zero-word `JZ`,
        // the megamorphic gate, the last rung's miss), R10 a cache slot and
        // R11 a dead entry word. Under `gate_entry` they stage the arguments
        // through R11 -- not RAX, which would lose the class id -- and jump
        // to the stub's gate entry, skipping its receiver reload, null test,
        // kind screen and class-id reload (two dependent loads). The raw
        // edges fall into the historical staging and the stub's full entry,
        // which sends both of them on to the resolving helper. The staging
        // block the stub, its callee-deopt service and the helper read is the
        // same on both routes.
        // MEGA5-18-3: the register probe's misses stage (through R11, as the
        // gate-entry edge below does) and jump to the resolving helper, which
        // reads only the staging block. The probe read the stub's three tables
        // with the stub's keys; only a publication racing it is missed, and the
        // helper resolves that receiver correctly.
        let mut to_helper: Option<usize> = None;
        if !probe_miss_patches.is_empty() {
            for p in std::mem::take(&mut probe_miss_patches) {
                self.patch_rel32_to_here(p);
            }
            for i in 0..num_args {
                let arg = inputs[2 + i];
                self.gp_load_abi_value(R11, arg);
                self.store_abi_reg(R11, self.args_stage_top_off - (i as i32) * 8);
            }
            to_helper = Some(self.emit_jmp_rel32());
        }
        let mut to_gate_entry: Option<usize> = None;
        if gate_entry {
            for p in std::mem::take(&mut slow_patches) {
                self.patch_rel32_to_here(p);
            }
            for i in 0..num_args {
                let arg = inputs[2 + i];
                self.gp_load_abi_value(R11, arg);
                // Cast: as the staging loop below.
                self.store_abi_reg(R11, self.args_stage_top_off - (i as i32) * 8);
            }
            to_gate_entry = Some(self.emit_jmp_rel32());
        }
        slow_patches.append(&mut raw_slow_patches);
        for p in slow_patches {
            self.patch_rel32_to_here(p);
        }
        // Where the stub loads each argument from.
        //
        // A value the register allocator kept wholly in a register has NO home
        // word (`home_dropped`), and `slot_of` refuses to name one rather than
        // emit a read of whatever the last tenant of that frame word left
        // there. That refusal failed the whole compile — this site is on the
        // megamorphic edge of EVERY virtual call the optimizing tier lowers, so
        // one register-resident argument anywhere in a method dropped that
        // method to the single-pass backend. `StringRegexOnly.run` is one such
        // method, and the refusal is what kept the `String/Regex` row's hot
        // method out of the optimizing tier entirely.
        //
        // The staging block written immediately below already holds every
        // argument, and it is written with `gp_load_value`, which reads a
        // register-resident value FROM its register. So a homeless argument has
        // a perfectly good frame address to name: its staging slot. Nothing new
        // is emitted for it, and the stores land before the stub runs (the
        // stub's own emission follows them).
        //
        // Only the homeless case is redirected. An argument with a real home
        // keeps naming it, so a megamorphic site's generated code is unchanged
        // for every shape that compiled before this.
        //
        // An over-wide site gets the stub too since round 12 wave 2 (lane
        // calls): it marshals its overflow on the stack, up to the same cap as
        // these inline caches (`emit_hashed_vtable_stub_reloading`, which still
        // refuses a wider site and, under `CRATONVM_JIT_HASHED_STUB_WIDE=0`,
        // any overflow; a refused site's receivers go to the resolving helper).
        //
        // JIT round 19 (lane mega19; a correctness fix, unconditional): a
        // non-constant `int` argument is read from its staging word too. The
        // stub copies each word it reads into an ABI register AS a 64-bit word,
        // and a single-pass callee reads an `int` parameter as the
        // sign-extended i64, while this tier leaves an `int` home zero-extended
        // or worse (`CRATONVM_DBG_IR_POISON_INT_HIGH`). Every staging store
        // goes through `gp_load_abi_value`, which sign-extends. Until now the
        // home was extended in place by the loop below -- which only the raw
        // (null / kind) edges run: under the default `gate_entry` every edge
        // that reaches a stub CALL (the gate, the last rung's miss, the zero
        // words) jumps past it to the stub's gate entry, so the stub marshalled
        // the unextended home. See
        // `docs/feature-designs/jit-ir-optimizations.md`.
        let arg_offsets: Vec<i32> = (0..num_args)
            .map(|i| {
                let arg = inputs[2 + i];
                if self.ic_stub_arg_reads_staging_word(arg) {
                    // Cast: an argument index is bounded by the callee's
                    // parameter count, so `i * 8` cannot overflow an x86-64
                    // displacement — the same cast the staging stores make.
                    self.args_stage_top_off - (i as i32) * 8
                } else {
                    self.slot_of(arg)
                }
            })
            .collect();
        // Populate the staging block for the stub's own callee-deopt service.
        // This is the megamorphic region — reached only after the MIC and all
        // four PIC entries missed — so the copy costs nothing on a
        // monomorphic site, which is where it used to be paid.
        for i in 0..num_args {
            let arg = inputs[2 + i];
            self.gp_load_abi_value(RAX, arg);
            // Cast: an argument index is bounded by the callee's parameter
            // count, so `i * 8` cannot overflow an x86-64 displacement.
            self.store_rax(self.args_stage_top_off - (i as i32) * 8);
        }
        // SBR4 osr: the stub below copies each home word into an ABI register
        // AS a 64-bit word, and a single-pass callee reads an `int` parameter
        // as the sign-extended i64 (`CRATONVM_DBG_IR_POISON_INT_HIGH`). This
        // tier leaves an `int` home zero-extended or worse, so extend, in the
        // home, every non-constant `int` argument the stub reads from one (a
        // homeless argument is read from its staging word, written above with
        // the ABI load). Semantics-neutral for this tier, whose consumers read
        // the low 32 bits only.
        //
        // Round 19 (lane mega19): with `ic_stub_arg_reads_staging_word` every
        // non-constant `int` is read from its (extended) staging word, so this
        // loop no longer finds one to extend; it stays as the guard it was.
        //
        // JIT round 19 proposal wave 5 (lane mega5, MEGA19-1,
        // `CRATONVM_JIT_IR_MEGA_RETIRE_INT_HOME_EXTENSION`): retired under the
        // switch. `needs_extension` is `!staged && int && !const`, and
        // `staged` is true for every non-constant `int`, so the loop emits
        // nothing in either arm; the switch only stops walking it (the code
        // is byte-identical). Off: the guard runs, as before.
        let retire_extension = ir_mega_retire_int_home_extension_enabled();
        debug_assert!(
            (0..num_args).all(|i| {
                let arg = inputs[2 + i];
                self.ic_stub_arg_reads_staging_word(arg)
                    || !self
                        .graph
                        .nodes
                        .get(arg as usize)
                        .is_some_and(|n| n.ty == IrType::Int && !matches!(n.op, Op::Const(_)))
            }),
            "MEGA19-1: a non-constant int argument is read from its home by the hashed stub"
        );
        for i in (0..num_args).filter(|_| !retire_extension) {
            let arg = inputs[2 + i];
            let staged = self.ic_stub_arg_reads_staging_word(arg);
            let needs_extension = !staged
                && self
                    .graph
                    .nodes
                    .get(arg as usize)
                    .is_some_and(|n| n.ty == IrType::Int && !matches!(n.op, Op::Const(_)));
            if needs_extension {
                self.load_reg_from_frame(RAX, arg_offsets[i]);
                self.buf.emit(&[0x48, 0x63, 0xC0]); // MOVSXD RAX, EAX
                self.store_abi_reg(RAX, arg_offsets[i]);
            }
        }
        // `arg_offsets` are each argument's own register-allocated home slot,
        // which the stub loads into the ABI registers one at a time. They are
        // NOT a contiguous block, so the callee-deopt service -- which reads
        // `num_args` consecutive slots to rebuild the callee's incoming
        // locals -- must be pointed at the staging block just written instead.
        //
        // Round 9 wave 2: the stub's service used to read that block exactly
        // as written HERE, before the `CALL` — pre-GC words for any reference
        // a moving collection relocated during the callee. It now copies this
        // site's shadow publication back into the homes and re-stages from
        // them on its cold side first (`emit_hashed_vtable_stub_reloading`).
        let mut stub_gate_entry: Option<usize> = None;
        // The stub's `CALL R11` return address (interpreter round i1 wave 48,
        // lane L3): a site inside a spliced body files its inline-frame row
        // under it, as every other call this lowerer emits there does. Until
        // then a receiver the site's caches missed ran its callee with the
        // splice's levels lost from every trace and walk through it
        // (`L6W37HiddenFramesHot.java`, `hidden-loop`:
        // `leaf < hot`, HotSpot `leaf < drive < hot`).
        let mut stub_return: Option<crate::runtime_lowering::HashedStubCallReturns> = None;
        if !arg_offsets.is_empty() {
            let shadow = self.pending_shadow_copy_back();
            let reload = shadow
                .as_ref()
                .map(|(thread_slot_off, savebase_slot_off, offsets)| {
                    crate::runtime_lowering::ShadowCopyBack {
                        thread_slot_off: *thread_slot_off,
                        savebase_slot_off: *savebase_slot_off,
                        offsets,
                    }
                });
            // JIT round 18 wave 11 (lane mega5, THROWS18-6,
            // `CRATONVM_JIT_MEGA_LEAN_STUB_HIT`): a stub hit republishes THIS
            // compile's identity and its not-sentinel edge joins `.hit_join`
            // (an inline-cache hit's protocol); only a sentinel goes on to
            // `.done`. Only on the inline-TLS shape, where `.done`'s own
            // republish is the RBP store plus `emit_frame_record_identity`.
            let lean_identity = (crate::runtime_lowering::mega_lean_stub_hit_enabled()
                && crate::x64::precise_jit_maps_enabled()
                && self.frame_record != 0
                && crate::x64::inline_rbp_tls_disp() != 0
                && self.inline_cm_tls_disp != 0)
                .then(|| self.compile_id.id());
            // JIT round 21 wave 13 (lane trace,
            // `CRATONVM_JIT_MEGA_STUB_SERVICE_IDENTITY`, default ON): without
            // the lean protocol, the stub's callee-deopt service still runs
            // under this compile's identity, not the hit's `0`. The lean entry
            // with no lean identity emits what the two branches below emit.
            let service_identity = (lean_identity.is_none()
                && crate::runtime_lowering::mega_stub_service_identity_enabled()
                && crate::x64::precise_jit_maps_enabled()
                && self.frame_record != 0
                && crate::x64::inline_rbp_tls_disp() != 0
                && self.inline_cm_tls_disp != 0)
                .then(|| self.compile_id.id());
            if lean_identity.is_some() || service_identity.is_some() {
                let (hits, fast, lean) =
                    crate::runtime_lowering::emit_hashed_vtable_stub_reloading_lean(
                        &mut self.buf,
                        pic,
                        self.context_slot_off,
                        &arg_offsets,
                        self.frame_record,
                        self.service_callee_deopt,
                        info_ptr,
                        self.args_stage_top_off,
                        reload.as_ref(),
                        Some(&mut stub_return),
                        gate_entry,
                        lean_identity,
                        service_identity,
                    );
                done_patches.extend(hits);
                hit_patches.extend(lean);
                if gate_entry {
                    stub_gate_entry = fast;
                }
            } else if gate_entry {
                let (hits, fast) =
                    crate::runtime_lowering::emit_hashed_vtable_stub_reloading_with_gate_entry(
                        &mut self.buf,
                        pic,
                        self.context_slot_off,
                        &arg_offsets,
                        self.frame_record,
                        self.service_callee_deopt,
                        info_ptr,
                        self.args_stage_top_off,
                        reload.as_ref(),
                        Some(&mut stub_return),
                    );
                done_patches.extend(hits);
                stub_gate_entry = fast;
            } else {
                done_patches.extend(crate::runtime_lowering::emit_hashed_vtable_stub_reloading(
                    &mut self.buf,
                    pic,
                    self.context_slot_off,
                    &arg_offsets,
                    self.frame_record,
                    self.service_callee_deopt,
                    info_ptr,
                    self.args_stage_top_off,
                    reload.as_ref(),
                    Some(&mut stub_return),
                ));
            }
        }
        if let Some(returns) = stub_return {
            self.anchor_safepoint_return(map.as_mut(), returns.callee);
            if let Some(service) = returns.service {
                self.anchor_safepoint_return(map.as_mut(), service);
            }
            {
                self.note_inline_frame_return_site_at(returns.callee);
            }
        }
        // The screened edges' jump: to the gate entry, or -- should the stub
        // not have been emitted after all -- straight to the resolving helper
        // below, which is where the stub's every miss lands and which reads
        // the staging block those edges have already written.
        if let Some(p) = to_gate_entry {
            match stub_gate_entry {
                Some(at) => self.patch_rel32_to_target(p, at),
                None => self.patch_rel32_to_here(p),
            }
        }

        // ── Slow path: the resolving + cache-populating helper ────────────
        // MEGA5-18-3's register-probe misses land here, past the stub.
        if let Some(p) = to_helper {
            self.patch_rel32_to_here(p);
        }
        // Args 1..4 use the same ABI as `jit_invoke_dispatch`, so the staging
        // marshalling is identical to the generic path below.
        //
        // Round 9: the staging block is NOT re-written here. The loop just
        // above the hashed stub already stored every argument into exactly
        // these slots, it runs unconditionally on every path that reaches
        // this point, and the stub's miss path (the only way here) makes no
        // call and writes no frame word — so a second copy of the same loop
        // re-stored identical values on every megamorphic miss.
        self.load_context(CALL_ARG_REGS[0]); // vm_ptr
        self.emit_mov_reg_imm64(CALL_ARG_REGS[1], info_ptr as u64); // info_ptr
        self.lea_reg_from_frame(CALL_ARG_REGS[2], self.args_stage_top_off); // args_ptr
        self.emit_mov_reg_imm64(CALL_ARG_REGS[3], num_args as u64); // num_args
                                                                    // Args 5 and 6 (mic, pic). Win64 passes them in the caller's stack
                                                                    // argument area at [RSP+32]/[RSP+40] — the 16-byte reserve
                                                                    // `Lowerer::new` budgets immediately above the 32-byte shadow space,
                                                                    // sized for exactly this worst-case 6-argument helper (see the
                                                                    // `jit_invoke_virtual_mic` note there). SysV passes them in R8/R9.
        #[cfg(target_os = "windows")]
        {
            self.emit_mov_reg_imm64(RAX, mic as u64);
            self.buf.emit(&[0x48, 0x89, 0x44, 0x24, 0x20]); // MOV [RSP+32], RAX
            self.emit_mov_reg_imm64(RAX, pic as u64);
            self.buf.emit(&[0x48, 0x89, 0x44, 0x24, 0x28]); // MOV [RSP+40], RAX
        }
        #[cfg(not(target_os = "windows"))]
        {
            self.emit_mov_reg_imm64(8, mic as u64); // R8
            self.emit_mov_reg_imm64(9, pic as u64); // R9
        }
        self.emit_mov_reg_imm64(RAX, self.invoke_virtual_mic as u64);
        self.buf.emit(&[0xFF, 0xD0]); // CALL RAX

        // .done:
        for p in done_patches {
            self.patch_rel32_to_here(p);
        }
        // Shared with the dispatch and direct-call paths: the compiled-entry
        // protocol is the same `i64::MIN` sentinel either way. This is
        // `emit_call_return_check` spelled out, with the sentinel tail moved
        // AHEAD of the shadow reload so a hit that already knows it did not
        // get the sentinel can skip the second compare (round 11 wave 3):
        //
        //   .done: note-row; republish
        //   .tail: sentinel tail  -- an exceptional exit leaves here, skipping
        //                            the reload exactly as every call-exception
        //                            exit does (the epilogue restores `top`)
        //   .hit_join: shadow reload; store
        //
        // The reload (which retracts this site's publication) still runs on
        // every path that continues, and the wide-result arm's `dispatch_threw`
        // peek is a leaf call that neither collects nor reads a home.
        self.anchor_safepoint_return(map.as_mut(), self.buf.pos());
        self.note_inline_frame_return_site();
        self.emit_post_call_frame_record();
        // .tail:
        for p in tail_patches {
            self.patch_rel32_to_here(p);
        }
        self.emit_call_return_sentinel_tail(ty);
        // .hit_join:
        for p in hit_patches {
            self.patch_rel32_to_here(p);
        }
        self.emit_shadow_reload();
        self.store_rax(slot);
    }

    /// `CRATONVM_JIT_IR_MEGA_GATE_ENTRY` (default on; `0` sends every edge of
    /// an optimizing-tier inline-cache site's megamorphic region through the
    /// RAX staging loop into the hashed stub's full entry, byte for byte the
    /// round-13-wave-4 site). Round 13 wave 5, lane callcost2, proposal M13-2:
    /// the class-id-carrying edges stage through R11 and enter the stub's gate
    /// entry. Also off whenever `CRATONVM_JIT_MEGA_GATE_FAST_ENTRY=0` (the
    /// single-pass twin's switch). Read per emitted site.


    /// Whether the hashed stub of [`Self::emit_inline_cache_call`] reads
    /// argument `arg` from its staging word instead of its home: a homeless
    /// value (it has no home), and since JIT round 19 (lane mega19) every
    /// non-constant `int`, whose staging word is the sign-extended ABI value
    /// and whose home may not be (see the call site).
    pub(in crate::ir_lower) fn ic_stub_arg_reads_staging_word(&self, arg: NodeId) -> bool {
        let homeless = self
            .home_dropped
            .get(arg as usize)
            .copied()
            .unwrap_or(false)
            && staged_arg_slot_enabled();
        let int_value = self
            .graph
            .nodes
            .get(arg as usize)
            .is_some_and(|n| n.ty == IrType::Int && !matches!(n.op, Op::Const(_)));
        homeless || int_value
    }

    /// Record the inline chain for the call that has JUST been emitted, if it
    /// came from inside a spliced body.
    ///
    /// Must be called with `self.buf.pos()` sitting immediately after the CALL
    /// instruction: that offset IS the return address a walker will find in
    /// this frame, and it is the exact key
    /// `x64::InlineFrameMap::chain_for_native_offset` looks up. Exactness is
    /// the whole design — see [`crate::ir::IrInlineFrameSites`] for why the
    /// bci cannot serve as the key for a nested splice.
    ///
    /// Records NOTHING when this compile spliced nothing (the overwhelming
    /// majority, and one `is_empty` check), when the node's pc is not inside a
    /// spliced body (an ordinary call in the compiling method's own code, which
    /// already reports itself correctly), or when the map is switched off.
    pub(in crate::ir_lower) fn note_inline_frame_return_site(&mut self) {
        let here = self.buf.pos();
        self.note_inline_frame_return_site_at(here);
    }

    /// [`Self::note_inline_frame_return_site`] for a call whose return address
    /// is NOT the current position: a shared `runtime_lowering` stub, which
    /// emits its post-call republish after the `CALL` and hands back the offset
    /// just past it (round 11 wave 2, lane lower;
    /// `r11-irlower-stub-calls-in-splices-record-no-inline-frame-row`).
    pub(in crate::ir_lower) fn note_inline_frame_return_site_at(&mut self, return_address: usize) {
        // Round 11 wave 14 (lane fib, `r11w14-rt-ir-lower-oop-free-frame-patch`):
        // the same return address places a `frame_holds_no_references` frame
        // suspended in this call, by line rather than by inline chain, and the
        // walk that asks (`conservative_roots::activation_bci`) reaches such a
        // frame by exactly the key this function exists to record. The bci is
        // the one `emit_safepoint_map` pairs with the id (`sp_id_bcis`), so the
        // line cannot depend on which of the two answers it.
        // Interpreter round i5 wave 102, lane L2: every body, so a walk can
        // place a caller frame whose safepoint-id slot it refuses
        // (`crate::return_site_line_fallback_enabled`).
        if self.body_publishes_no_roots() || crate::return_site_line_fallback_enabled() {
            if let (Ok(off), Ok(bci)) = (
                u32::try_from(return_address),
                u32::try_from(self.resume_bci(self.cur_bci)),
            ) {
                self.return_site_rows.push((off, bci));
            }
        }
        if self.inline_frame_sites.is_empty() || !crate::x64::inline_frame_map_enabled() {
            return;
        }
        // `cur_node_pc`, never `cur_bci`: see the field's own note.
        let Some(node_pc) = self.cur_node_pc else {
            return;
        };
        let chain = self.inline_frame_sites.chain_at(node_pc);
        if chain.is_empty() {
            return;
        }
        // The compiling method's own bci for this program point. Absent it the
        // row would have to invent one, and a bci-keyed row carrying a
        // combined-buffer pc is exactly the "not a spec-legal bytecode index"
        // shape the emitter refuses everywhere else.
        let Some(safepoint_bci) = self.inline_frame_sites.enclosing_bci_at(node_pc) else {
            return;
        };
        let Ok(native_offset) = u32::try_from(return_address) else {
            return;
        };
        self.inline_frame_rows.push(crate::x64::InlineFrameRow {
            native_offset,
            safepoint_bci,
            chain: chain
                .into_iter()
                .map(|l| crate::x64::InlineFrameLevel {
                    label: l.method_key.into(),
                    bci: l.bci,
                    class_id: l.class_id,
                    hidden: None,
                })
                .collect(),
        });
    }

    /// Open the inline-frame-row census for this compile (interpreter round i1
    /// wave 49, lane L3;
    /// `docs/feature-designs/deopt-osr.md`):
    /// only when it spliced something, the map is on, and
    /// `CRATONVM_DBG_INLINE_ROW_CENSUS=1` (read once per such compile). The
    /// buffer then records every `CALL` it is handed as one instruction, and
    /// each lowered node marks where its code starts and whether it lies inside
    /// a spliced body ([`Self::note_row_census_node`]).
    pub(in crate::ir_lower) fn begin_inline_row_census(&mut self) {
        if self.inline_frame_sites.is_empty()
            || !crate::x64::inline_frame_map_enabled()
            || !false
        {
            return;
        }
        self.buf.begin_call_census();
        self.row_census = Some(Box::default());
    }

    /// Mark the start of the node whose pc was just made current: the code
    /// from here to the next mark is its own. One `is_some` test when no
    /// census is open.
    #[inline]
    pub(in crate::ir_lower) fn note_row_census_node(&mut self) {
        if self.row_census.is_some() {
            self.note_row_census_node_slow();
        }
    }

    #[cold]
    #[inline(never)]
    pub(in crate::ir_lower) fn note_row_census_node_slow(&mut self) {
        let at = u32::try_from(self.buf.pos()).unwrap_or(u32::MAX);
        let spliced = self
            .cur_node_pc
            .filter(|&pc| !self.inline_frame_sites.chain_at(pc).is_empty());
        if let Some(census) = self.row_census.as_mut() {
            if census.closed_at.is_none() {
                census.marks.push((at, spliced));
            }
        }
    }

    /// The last node has been lowered: code emitted from here on (the
    /// out-of-line stubs, pads and tails) belongs to no node the census can
    /// name, and is counted apart.
    pub(in crate::ir_lower) fn close_inline_row_census(&mut self) {
        let at = u32::try_from(self.buf.pos()).unwrap_or(u32::MAX);
        if let Some(census) = self.row_census.as_mut() {
            census.closed_at.get_or_insert(at);
        }
    }

    /// Classify every recorded call against `rows` (the compile's inline-frame
    /// rows) and print one line per compile, plus one per call inside a
    /// spliced body with no row: its return address, the node pc and the
    /// innermost spliced method it was emitted for. Reports, never files: a
    /// row is only ever filed where the call was emitted with one (the
    /// fail-closed rule of `conservative_roots::compiled_frame_inline_chain`).
    pub(in crate::ir_lower) fn report_inline_row_census(&mut self, rows: &[crate::x64::InlineFrameRow]) {
        let Some(census) = self.row_census.take() else {
            return;
        };
        let calls = self.buf.take_call_census();
        let filed: std::collections::HashSet<u32> = rows.iter().map(|r| r.native_offset).collect();
        let verdict = classify_inline_row_census(&census, &calls, &filed);
        let innermost = |pc: usize| {
            self.inline_frame_sites
                .chain_at(pc)
                .first()
                .map_or_else(|| "?".to_string(), |l| l.method_key.to_string())
        };
        eprintln!(
            "[cratonvm-jit] inline-row census: calls={} spliced={} rowless={} out-of-line={} rows={}",
            calls.len(),
            verdict.spliced,
            verdict.rowless.len(),
            verdict.out_of_line,
            rows.len(),
        );
        for &(ret, pc) in &verdict.rowless {
            eprintln!(
                "[cratonvm-jit] inline-row census: ROWLESS call returning to +{ret:#x} \
                 node-pc={pc} in {}",
                innermost(pc)
            );
        }
    }

    /// Describe one NPE trap site and return its key, or `0` for "not
    /// described" — which every caller passes straight through to the helper
    /// argument, where zero means the historical unmessaged NPE.
    ///
    /// This is [`crate::x64::inlining::record_npe_trap_site`]'s optimizing-tier
    /// twin, and it answers the same two questions from this backend's own
    /// data: the ENCLOSING method's bci for the program point
    /// (`IrInlineFrameSites::enclosing_bci_at`, or `node_pc` itself when the
    /// point is not inside a spliced body), and the callees spliced there
    /// (`chain_at`, innermost first).
    ///
    /// Both halves respect the same kill switches the single-pass twin does, so
    /// `CRATONVM_JIT_NO_NPE_TRAP_LINES=1` and
    /// `CRATONVM_JIT_NO_INLINE_FRAME_MAP=1` revert this tier and that one
    /// together rather than leaving one of them describing sites the walk will
    /// not read.
    pub(in crate::ir_lower) fn record_npe_trap_site(&mut self, node_pc: usize) -> u32 {
        if !crate::x64::npe_trap_lines_enabled() || !crate::x64::inline_frame_map_enabled() {
            return 0;
        }
        // The compiling method's own bci. Inside a splice that is the outermost
        // caller pc; outside one the node's pc already IS it.
        let own_pc = match u32::try_from(node_pc) {
            Ok(v) => v,
            Err(_) => return 0,
        };
        let bci = if self.inline_frame_sites.is_empty() {
            own_pc
        } else {
            // `None` means the point is not inside any spliced body, so the
            // node's own pc already IS the compiling method's bci.
            self.inline_frame_sites
                .enclosing_bci_at(node_pc)
                .unwrap_or(own_pc)
        };
        // JVMS 4.9.1: `Code.code_length` is below 65536. A bci at or above it
        // is not a bytecode index and describing a site with one would hand the
        // consumer a confidently wrong opcode. The SAME constant the
        // single-pass twin screens against, not a second copy of the bound.
        if bci as usize >= crate::x64::INLINE_FRAME_MAX_BCI {
            return 0;
        }
        let chain: Vec<crate::x64::InlineFrameLevel> = self
            .inline_frame_sites
            .chain_at(node_pc)
            .into_iter()
            .map(|l| crate::x64::InlineFrameLevel {
                label: l.method_key.into(),
                bci: l.bci,
                class_id: l.class_id,
                hidden: None,
            })
            .collect();
        self.npe_trap_next_id = self.npe_trap_next_id.wrapping_add(1);
        let id = self.npe_trap_next_id;
        // The key rides in 24 bits of one helper argument; a compile with more
        // sites than that describes no more of them.
        if id >= (1 << 24) {
            return 0;
        }
        self.npe_trap_sites
            .push((id, crate::x64::NpeTrapSite { bci, chain }));
        id
    }

    /// JIT round 19 proposal wave 5, lane concat5 (STRGAP4-1,
    /// `CRATONVM_JIT_IR_CONCAT_INLINE`, default OFF): the optimizing tier's twin
    /// of the single-pass inline concat (STRGAP19-3). Lowers the `Op::IndyBridge`
    /// node `id` (a bridged `StringConcatFactory` site the VM's plan admits)
    /// as the inline concat AHEAD of the unchanged bridge call, and answers
    /// `true`; answers `false` with NOTHING emitted when the site is not
    /// lowered, and the caller then emits the bridge call alone, byte for byte.
    ///
    /// ```text
    ///   stage operands into the bridge's argument words      (as the call does)
    ///   length pass, bump, pre-checks, headers, fill, commit (concat_emit)
    ///   JMP join                                             ; RAX = the String
    /// decline:                                               (every edge, before any store)
    ///   map ; vm, site, args, n ; CALL bridge
    ///   inline-frame row ; frame republish ; shadow reload ; sentinel edge
    /// join:
    ///   store RAX -> the node's home
    /// ```
    ///
    /// # Frame state and GC
    ///
    /// The fast path makes no call, polls nothing, cannot throw and runs no
    /// Java code, so it owes no safepoint map and no deopt point: the map is
    /// taken on the decline edge only, BEFORE `alloc_slot` exactly as the
    /// bridge-only arm takes it (`Op::New`'s map sink, the same argument: no
    /// collector can observe the frame between the node's start and the join
    /// on the fast path). The decline edge is the bridge-only arm with the
    /// staging hoisted above the map: the staged words are frame words the map
    /// and its shadow push do not touch, and the bridge decodes them into
    /// pinned values before it can allocate, as before. Its exception edge,
    /// inline-frame row and frame republish are the bridge arm's, at this
    /// node's bci, so a throwing operand -- unreachable here, every admitted
    /// operand is a `String` or a primitive -- or a `BootstrapMethodError`
    /// leaves exactly as it did.
    ///
    /// Both paths meet BEFORE the one `store_rax` of the node's home, so the
    /// home store, its carry and the register publish are emitted once.
    pub(in crate::ir_lower) fn lower_indy_bridge_concat_inline(
        &mut self,
        id: NodeId,
        node: &Node,
        site: usize,
        bridge: usize,
        num_args: usize,
    ) -> bool {
        use crate::x64::concat_emit as ce;
        // JIT round 21 wave 20 (lane sbfuse, STRINTR21-4): a fused
        // `StringBuilder` chain's concat is inline under its own switch
        // (`CRATONVM_JIT_IR_SB_CHAIN_FUSE`, which built the node).
        let fused_chain = self.graph.sb_chain_fused_sites.contains(&site);
        if node.ty != IrType::Ref || !(ce::ir_concat_inline_enabled() || fused_chain) {
            return false;
        }
        let pc = node.bytecode_pc;
        let offsets: Vec<i32> = (0..num_args)
            // Cast: bounded by `args_stage_words`, checked by the caller.
            .map(|i| self.args_stage_top_off - (i as i32) * 8)
            .collect();
        let frame = ce::ConcatIrFrame {
            tls_disp: crate::x64::jit_thread_tls_disp(),
            thread_slot_off: self.shadow_thread_slot_off,
            cursor_off: self.tlab_cursor_off,
            end_off: self.tlab_end_off,
            class_id_off: self.concat_class_id_off,
            g1_table: self.g1_barrier_addr,
            region_bounds_addr: self.concat_region_bounds_addr,
        };
        let plan = self.direct_helpers.concat_inline_plan(site);
        let boxed_layouts = plan
            .map(|p| {
                ce::resolve_boxed_layouts(p, |wk| {
                    self.direct_helpers.well_known_class_id_for(wk)
                })
            })
            .unwrap_or_default();
        let csite = match ce::concat_ir_site(
            plan,
            self.graph.string_layout,
            self.direct_helpers.compact_strings_flag_addr(),
            offsets,
            frame,
            boxed_layouts,
        ) {
            Ok(csite) => csite,
            Err(why) => {
                if crate::x64::dbg_jitc_enabled() {
                    eprintln!(
                        "[cratonvm-jitc] ir concat-inline DECLINED {} pc={pc:?}: {why}",
                        self.graph.scope_method_key
                    );
                }
                return false;
            }
        };
        if crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] ir concat-inline EMITTED {} pc={pc:?} args={} literal={} segments={}",
                self.graph.scope_method_key,
                csite.shape.plan.arg_count,
                csite.shape.plan.literal_len,
                csite.shape.plan.segments.len()
            );
        }
        // The operands, staged where the bridge reads them (and the passes do).
        for i in 0..num_args {
            let arg = node.inputs[2 + i];
            self.gp_load_value(RAX, arg);
            // Cast: bounded by `args_stage_words`.
            self.store_rax(self.args_stage_top_off - (i as i32) * 8);
        }
        let mut decline: Vec<usize> = Vec::new();
        // JIT round 21 wave 4, lane concat (CONCAT21-1,
        // `CRATONVM_JIT_CONCAT_CODER_PROMOTE`, default ON since r23): a run-time UTF16
        // operand of a LATIN1 site is built inline by the plan's wide twin
        // (a second tail, its own declines to the same bridge call) instead
        // of declining the evaluation. Off, the bytes are unchanged.
        // CONCAT21-3 (`CRATONVM_JIT_CONCAT_FP_INLINE`, default ON since r22): an FP
        // plan renders its float / double operands into an RSP-relative
        // scratch area first; every decline from there on leaves with RSP
        // moved, so they land on a restore stub below (each tail restores RSP
        // itself before its join). No FP operand: nothing emitted.
        if csite.shape.plan.has_boxed() && self.layout_guard_deopt_on {
            decline.extend(self.emit_layout_epoch_guard());
        }
        let fp = csite.shape.plan.has_fp();
        ce::emit_fp_prologue(&mut self.buf, &csite.shape, &mut decline);
        let twin = ce::coder_promotion_twin(csite.shape.plan);
        let mut fast_joins: Vec<usize> = Vec::new();
        match twin {
            Some(twin) => {
                let mut promote: Vec<usize> = Vec::new();
                ce::emit_ir_tail_promoting(&mut self.buf, &csite, &mut decline, &mut promote);
                fast_joins.push(self.emit_jmp_rel32());
                if !promote.is_empty() {
                    for p in promote {
                        self.patch_rel32_to_here(p);
                    }
                    ce::emit_ir_tail(&mut self.buf, &csite.with_plan(twin), &mut decline);
                    fast_joins.push(self.emit_jmp_rel32());
                }
                if crate::x64::dbg_jitc_enabled() {
                    eprintln!(
                        "[cratonvm-jitc] ir concat-inline coder-promote {} pc={pc:?} twin_segments={}",
                        self.graph.scope_method_key,
                        twin.segments.len()
                    );
                }
            }
            None => {
                ce::emit_ir_tail(&mut self.buf, &csite, &mut decline);
                fast_joins.push(self.emit_jmp_rel32());
            }
        }
        for p in decline {
            self.patch_rel32_to_here(p);
        }
        if fp {
            // The restore stub every FP-area decline lands on, falling into
            // the bridge call below.
            ce::emit_fp_restore(&mut self.buf, &csite.shape);
        }
        // ---- decline: the bridge call over the same staged operands --------
        let sp_live_hi = self.spill_high_water;
        let mut map = self.emit_safepoint_map(sp_live_hi);
        let slot = self.alloc_slot(id);
        self.load_context(CALL_ARG_REGS[0]); // vm_ptr
        self.emit_mov_reg_imm64(CALL_ARG_REGS[1], site as u64); // site_ptr
        self.lea_reg_from_frame(CALL_ARG_REGS[2], self.args_stage_top_off); // args_ptr
        self.emit_mov_reg_imm64(CALL_ARG_REGS[3], num_args as u64); // arg_count
        self.emit_mov_reg_imm64(RAX, bridge as u64);
        self.buf.emit(&[0xFF, 0xD0]);
        self.anchor_safepoint_return(map.as_mut(), self.buf.pos());
        // `emit_call_return_check` with the fast path's join placed before
        // its home store.
        self.note_inline_frame_return_site();
        self.emit_post_call_frame_record();
        self.emit_shadow_reload();
        self.emit_call_return_sentinel_tail(node.ty);
        if fused_chain {
            if let Some(bci) = pc {
                self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
                self.emit_deopt_if_zero(bci, DeoptReason::UncommonTrap);
            }
        }
        for j in fast_joins {
            self.patch_rel32_to_here(j);
        }
        self.store_rax(slot);
        true
    }

    /// Post-call exception sentinel + result spill, shared by the
    /// `jit_invoke_dispatch` path and the direct cross-method call path.
    ///
    /// A callee that threw (or deopted) returns the `i64::MIN` sentinel. For an
    /// int / reference / void result that is unambiguous (no legitimate value is
    /// `i64::MIN`), so a plain `CMP RAX, 1 ; JO bail` suffices (`RAX - 1`
    /// overflows exactly when `RAX == i64::MIN`). For a
    /// `J`/`D`/`F` result a legitimate `Long.MIN_VALUE` is bit-identical to the
    /// sentinel, so on the (rare) `RAX == i64::MIN` branch we peek the
    /// out-of-band signal via `jit_dispatch_threw`: bail only when a genuine
    /// exception/deopt is pending, else keep the real value. The result is then
    /// spilled to `slot` (harmless for a void call: the slot is allocated but
    /// never read).
    pub(in crate::ir_lower) fn emit_call_return_check(&mut self, slot: i32, ty: IrType) {
        // The return address for the call just emitted is HERE, before any of
        // the post-call sequence below moves `buf`.
        self.note_inline_frame_return_site();
        self.emit_call_return_check_after_note(slot, ty);
    }

    /// The rest of [`Self::emit_call_return_check`], for a caller that already
    /// recorded the inline-frame row itself -- immediately after its own
    /// `CALL`, before emitting anything else that would move `buf` past the
    /// real return address (`emit_direct_cross_call`'s non-merged branch, and
    /// the `Op::LoadStatic` helper route, whose optional load fence sits
    /// between its `CALL` and this). Calling
    /// `emit_call_return_check` there instead would record a second row at
    /// this (wrong, later) offset on top of the correct one.
    pub(in crate::ir_lower) fn emit_call_return_check_after_note(&mut self, slot: i32, ty: IrType) {
        // The call has just returned. Copy back any relocated shadow values
        // BEFORE anything else touches the frame — and before the sentinel
        // compare below, which clobbers R10. Uses RCX, never RAX, so the return
        // value in RAX is untouched (see `emit_shadow_reload`). This is the one
        // site all three dispatch routes share; the self-recursive route does
        // not reach here and does not publish.
        //
        // The RBP republish comes first because the reload's own correctness
        // does not depend on it, but the frame's identity to the collector
        // does: the callee overwrote the mirror with its own RBP on entry, and
        // any safepoint reached between here and the next call would otherwise
        // resolve this frame's maps against a dead frame's base.
        //
        // W7-C: a sunk call reaches no such safepoint before its frame
        // returns, so the republish runs on the sentinel side only
        // (`emit_sunk_call_return_check`).
        if self.tail_sink.cur {
            self.emit_sunk_call_return_check(slot, ty);
            return;
        }
        self.emit_post_call_frame_record();
        self.emit_shadow_reload();
        self.emit_call_return_sentinel_tail(ty);
        self.store_rax(slot);
    }

    /// The `i64::MIN` sentinel half of [`Self::emit_call_return_check`],
    /// without the frame republish, the shadow reload or the result store:
    /// what a call site emits on the COLD side of
    /// [`Self::emit_call_sentinel_fast_skip`], where the common non-sentinel
    /// return has already branched past it.
    pub(in crate::ir_lower) fn emit_call_return_sentinel_tail(&mut self, ty: IrType) {
        self.emit_cmp_rax_sentinel();
        if matches!(ty, IrType::Long | IrType::Double | IrType::Float) {
            // JNO .keep — common path: not the sentinel, keep real RAX.
            self.buf.emit(&[0x0F, 0x81]);
            let keep_patch = self.buf.pos();
            self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
            // Cold: RAX == i64::MIN. Peek whether a real exception/deopt
            // is pending — MOV RAX, dispatch_threw ; CALL RAX (RAX = 0/1).
            self.emit_mov_reg_imm64(RAX, self.dispatch_threw as u64);
            self.buf.emit(&[0xFF, 0xD0]);
            // TEST RAX, RAX — ZF=1 iff no signal pending (legit value).
            self.buf.emit(&[0x48, 0x85, 0xC0]);
            // Restore the sentinel/value into RAX before branching: the
            // shared bail stub returns RAX unchanged (so it must be
            // `i64::MIN`), and the keep path needs the genuine
            // `Long.MIN_VALUE`. `MOV` does not disturb ZF.
            self.emit_mov_reg_imm64(RAX, i64::MIN as u64);
            // JNE bail_stub — ZF==0 ⇒ exception/deopt ⇒ propagate sentinel.
            self.buf.emit(&[0x0F, 0x85]);
            let patch = self.buf.pos();
            self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
            self.push_call_exc_patch(patch);
            // .keep: patch the JNE above to land here.
            let keep_off = self.buf.pos();
            let rel = keep_off as i32 - (keep_patch as i32 + 4);
            // ir_lower call-sentinel keep -- tolerated on an overflowed buffer; see
            // `Self::patch_or_bail` / `patch_rel32_to_here`.
            Self::patch_or_bail(&mut self.buf, keep_patch, rel);
        } else {
            self.buf.emit(&[0x0F, 0x80]); // JO rel32 (patched to the stub)
            let patch = self.buf.pos();
            self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
            self.push_call_exc_patch(patch);
        }
    }

    /// The hot half of a merged post-call sentinel check: `RAX != i64::MIN`
    /// branches past BOTH the callee-deopt service and the exception tail in
    /// one compare, where the two used to each materialise the 10-byte
    /// immediate and compare again. Returns the rel32 patch the caller lands
    /// on `.keep`. The cold side keeps its own compares -- they run only when
    /// the callee actually returned the sentinel.
    pub(in crate::ir_lower) fn emit_call_sentinel_fast_skip(&mut self) -> usize {
        self.emit_cmp_rax_sentinel();
        self.emit_jcc_rel32(0x81) // JNO .keep
    }

    /// `CMP RAX, 1` (`48 83 F8 01`): the `i64::MIN` sentinel test in four
    /// bytes. `RAX - 1` overflows iff `RAX == i64::MIN`, so the flag to branch
    /// on is **OF**: `JO` where the old form said `JE`, `JNO` where it said
    /// `JNE`. ZF is NOT the sentinel answer after this compare.
    ///
    /// Round 9 wave 2 (deopt #3): replaces `MOV R10, imm64(i64::MIN); CMP RAX,
    /// R10` -- 13 bytes and an R10 write -- at every post-call and
    /// helper-sentinel site, the same rewrite the single-pass backend took in
    /// round 9. No site reads R10 after the compare.
    pub(in crate::ir_lower) fn emit_cmp_rax_sentinel(&mut self) {
        self.buf.emit(&[0x48, 0x83, 0xF8, 0x01]);
    }

    /// COV-03 — the `i64::MIN` sentinel check for a *helper* that returns a
    /// value in RAX and may instead return the deopt/NPE sentinel.
    ///
    /// The same two-shape decision `emit_call_return_check` makes, minus the
    /// post-call frame/shadow republication a dispatched Java call needs and a
    /// leaf helper does not: `jit_getfield` reaches no safepoint, so nothing has
    /// moved and no oop needs copying back.
    ///
    /// * `Int`/`Ref`/anything else — `i64::MIN` is never a legitimate result (no
    ///   plausible heap pointer equals it), so `CMP ; JE bail`.
    /// * `Long`/`Double`/`Float` — `Long.MIN_VALUE`, and the `-0.0` bit pattern,
    ///   ARE legitimate results bit-identical to the sentinel. On that (rare)
    ///   branch, peek the out-of-band signal via `jit_dispatch_threw` and bail
    ///   only when a genuine exception/deopt is pending; otherwise keep the real
    ///   value. `jit_getfield` sets the pending-NPE flag before returning the
    ///   sentinel, which is one of the signals that peek reports.
    ///
    /// Leaves RAX holding the value to spill in both shapes.
    pub(in crate::ir_lower) fn emit_helper_sentinel_check(&mut self, ty: IrType) {
        self.emit_cmp_rax_sentinel();
        if !matches!(ty, IrType::Long | IrType::Double | IrType::Float) {
            self.buf.emit(&[0x0F, 0x80]); // JO rel32 → shared bail stub
            let exc_patch = self.buf.pos();
            self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
            self.push_call_exc_patch(exc_patch);
            return;
        }
        // JNO .keep — common path: not the sentinel, keep the real RAX.
        self.buf.emit(&[0x0F, 0x81]);
        let keep_patch = self.buf.pos();
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
        // Cold: RAX == i64::MIN. MOV RAX, dispatch_threw ; CALL RAX (RAX = 0/1).
        self.emit_mov_reg_imm64(RAX, self.dispatch_threw as u64);
        self.buf.emit(&[0xFF, 0xD0]);
        self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX — ZF=1 ⇒ no signal
                                            // Restore the sentinel/value before branching: the shared bail stub
                                            // returns RAX unchanged, and the keep path needs the genuine value.
                                            // `MOV` does not disturb ZF.
        self.emit_mov_reg_imm64(RAX, i64::MIN as u64);
        self.buf.emit(&[0x0F, 0x85]); // JNE bail_stub
        let patch = self.buf.pos();
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
        self.push_call_exc_patch(patch);
        let keep_off = self.buf.pos();
        let rel = keep_off as i32 - (keep_patch as i32 + 4);
        // ir_lower helper-sentinel keep -- tolerated on an overflowed buffer;
        // see `Self::patch_or_bail` / `patch_rel32_to_here`.
        Self::patch_or_bail(&mut self.buf, keep_patch, rel);
    }

    /// fib44-fix follow-up: patch every direct self-recursive `CALL` (invoke_kind
    /// 4) so its rel32 targets this method's own entry — code offset 0.
    pub(in crate::ir_lower) fn patch_self_calls(&mut self) {
        let patches = std::mem::take(&mut self.self_call_patches);
        for p in patches {
            // rel32 = target - (rel32_field_offset + 4); target = entry = 0.
            let rel = 0i32 - (p as i32 + 4);
            // ir_lower self-call rel32 -- tolerated on an overflowed buffer; see
            // `Self::patch_or_bail` / `patch_rel32_to_here`.
            Self::patch_or_bail(&mut self.buf, p, rel);
        }
    }

    // ── Level 2: the machine list, emitting ──────────────────────────────
    //
    // Increment 2 of
    // `docs/feature-designs/jit-machine-level-and-instruction-selection.md`:
    // the selector's tiles become bytes, through the same `PATTERNS` table
    // whose every row is anchored to a hand-written emitter, against the
    // trivial allocation `ir_lower` has always used — every value in its frame
    // word, RAX the destination, RCX the second operand.
    //
    // The whole increment is the byte-equality oracle. `MirMode::Verify` runs
    // the per-opcode arm AND the tile encoder and compares; `MirMode::Emit`
    // runs only the encoder. A mismatch in either mode refuses the compile.

    /// The one entry point `lower_block` calls. Falls through to
    /// [`Self::lower_data_node`] whenever the machine list has nothing to say
    /// about `id`, which is every node on every compile that has not set a
    /// machine-level flag.
    pub(in crate::ir_lower) fn lower_data_node_through_mir(&mut self, block_idx: usize, id: NodeId) {
        // W7-C: as `lower_data_node` (a tile may stand in for it).
        self.tail_sink.cur = false;
        // JIT round 19 wave 10 (lane tok10, i64-L2): the token-read guard's
        // `mir` scope, over the nodes the tile covers, around the encoder's
        // operand reads (`frame_operand` -> `slot_of_checked`). Both encoder
        // calls run inside it; the per-opcode arm below installs its own
        // `data` scope. Nothing computed while the guard or MIR is off.
        let scope = if self.token_read_guard && self.mir_mode != MirMode::Off {
            match self.mir_tile_covered(block_idx, id) {
                Some(ids) => self.token_scope_enter(&ids, TokenScope::Mir),
                None => None,
            }
        } else {
            None
        };
        let bytes = match self.mir_mode {
            MirMode::Off => None,
            MirMode::Verify | MirMode::Emit => self.mir_tile_bytes(block_idx, id),
        };
        // In verify mode a tile that is not emittable is still asked what the
        // encoder WOULD have produced; asked here (before the arm, as it always
        // was) so the ask runs inside the `mir` scope.
        let shadow = if bytes.is_none() && self.mir_mode == MirMode::Verify {
            self.mir_shadow_tile_bytes(block_idx, id)
        } else {
            None
        };
        self.token_scope_exit(scope);
        let Some(bytes) = bytes else {
            // Not emittable. In verify mode, still ask what the encoder WOULD
            // have produced, and price it against what the arm writes — that
            // difference is the size of the next increment, and it is the one
            // number that decides whether there should be one.
            if self.mir_mode == MirMode::Verify {
                let before = self.buf.pos();
                self.lower_data_node_tracked(id);
                let after = self.buf.pos();
                if let Some(shadow) = shadow {
                    self.mir_shadow_tiles += 1;
                    self.mir_arm_bytes += after.saturating_sub(before);
                    self.mir_enc_bytes += shadow.as_slice().len();
                }
                return;
            }
            self.lower_data_node_tracked(id);
            return;
        };
        self.mir_tiles += 1;
        if self.mir_mode == MirMode::Verify {
            // The per-opcode arm is still the thing that emits, so this mode
            // cannot produce a wrong instruction — only a wrong verdict.
            let before = self.buf.pos();
            self.lower_data_node_tracked(id);
            let after = self.buf.pos();
            let agreed = self
                .buf
                .as_slice()
                .get(before..after)
                .is_some_and(|emitted| emitted == bytes.as_slice());
            if !agreed {
                self.mir_mismatches += 1;
            }
            return;
        }
        // `MirMode::Emit`. Everything the per-opcode arm does besides emitting
        // has to happen here too, in the same order: the bci anchor is read at
        // the position the first byte lands on, and the slot allocation is what
        // publishes the node to `defined_nodes` and moves the spill watermark.
        self.cur_node_pc = self.graph.nodes[id as usize].bytecode_pc;
        self.note_row_census_node();
        if let Some(pc) = self.graph.nodes[id as usize].bytecode_pc {
            // j27 wave 2 (lane GCMAP): the reported bci, as `lower_data_node`.
            self.cur_bci = self.graph.reported_call_bci(id, pc);
            self.note_bci_anchor(id, pc);
        }
        // Per-copy deopt metadata: a node belonging to a specific copy of a
        // cloned body anchors its OWN snapshot, at an offset inside that copy.
        // `None` (every node of every graph that did not unroll) leaves both
        // the map and every frame resolution exactly as they were.
        self.cur_node_frame = self.graph.nodes[id as usize].frame_snapshot;
        if let Some(si) = self.cur_node_frame {
            let here = self.buf.pos();
            self.snapshot_native
                .entry(si)
                .and_modify(|e| {
                    if here < *e {
                        *e = here;
                    }
                })
                .or_insert(here);
        }
        let slot = self.alloc_slot(id);
        // The encoder wrote its store against `planned_slot_off`; if the
        // allocating path just disagreed, the artifact is already wrong and no
        // later check would notice. Refuse.
        if self.planned_slot_off(id).ok() != Some(slot) {
            self.mir_mismatches += 1;
            self.latch_bailout(Bailout::new(BailoutReason::Internal(
                "ir_lower: the level-2 encoder and alloc_slot disagreed on a destination slot",
            )));
            return;
        }
        self.buf.emit(bytes.as_slice());
    }

    /// The bytes the level-2 encoder produces for the tile rooted at `id`, or
    /// `None` when there is no such tile or the encoder declines it.
    ///
    /// Declining is the normal answer and costs nothing: the caller lowers the
    /// node the way it always did. What must never happen is a tile that
    /// *absorbed* other nodes being emitted here — those nodes would then be
    /// computed nowhere — so the cover list is checked against the root and
    /// nothing else.
    pub(in crate::ir_lower) fn mir_tile_bytes(&self, block_idx: usize, id: NodeId) -> Option<FrameAccessList> {
        let plan = self.mir.as_ref()?;
        let (tb, ti) = (*plan.tile_of.get(id as usize)?)?;
        if usize::try_from(tb).ok()? != block_idx {
            return None;
        }
        let tile = plan
            .blocks
            .get(usize::try_from(tb).ok()?)?
            .tiles
            .get(usize::try_from(ti).ok()?)?;
        if !mir_tile_is_emittable(tile, id) {
            return None;
        }
        self.encode_tile_frame_homed(tile)
    }

    /// The frame word `id` lives in, **and** the fact that it lives there.
    ///
    /// The level-2 encoder's whole premise is that an operand can be read out
    /// of its frame word. The linear-scan read cache makes that false for a
    /// value it has published into a register — today only for `Float`/`Double`
    /// (the file is XMM-only), which no integer tile can name. Asked rather
    /// than assumed, per operand: the day the allocator grows a GP class, this
    /// returns `None` and the tile is declined, instead of silently encoding a
    /// load of a stale word.
    pub(in crate::ir_lower) fn frame_operand(&self, id: NodeId) -> Option<i32> {
        if self.resident_xmm(id).is_some() {
            return None;
        }
        self.slot_of_checked(id).ok()
    }

    /// The frame word a tile will STORE its result into.
    ///
    /// `planned_slot_off` rather than `slot_of_checked` because the destination
    /// has not been allocated yet when the encoder runs — the emit path calls
    /// `alloc_slot` afterwards and checks the two agree. The residency gate is
    /// the same one [`Self::frame_operand`] applies, for the same reason: a
    /// value the allocator publishes into a register is a value whose frame
    /// word is not the whole truth.
    pub(in crate::ir_lower) fn frame_destination(&self, id: NodeId) -> Option<i32> {
        if self.resident_xmm(id).is_some() {
            return None;
        }
        self.planned_slot_off(id).ok()
    }

    /// What the level-2 encoder *would* emit for `id`, whatever the rule.
    ///
    /// Verify mode only, and it emits nothing: this is how the next increment
    /// gets sized with a number instead of an argument. `mir_tile_bytes` is
    /// restricted to `Rule::AluReg` because that is the only rule whose bytes
    /// are provably identical to the per-opcode arm's — and byte equality is
    /// increment 2's whole oracle. The rules that would *improve* the code
    /// (`AluImm` drops a frame load) cannot ride that oracle by construction,
    /// so what they are worth has to be measured separately, against the bytes
    /// the arms actually wrote.
    ///
    /// Restricted to tiles covering exactly their own root, for the same reason
    /// [`mir_tile_is_emittable`] is: a tile that absorbed other nodes cannot be
    /// compared against one node's byte range.
    pub(in crate::ir_lower) fn mir_shadow_tile_bytes(&self, block_idx: usize, id: NodeId) -> Option<FrameAccessList> {
        let plan = self.mir.as_ref()?;
        let (tb, ti) = (*plan.tile_of.get(id as usize)?)?;
        if usize::try_from(tb).ok()? != block_idx {
            return None;
        }
        let tile = plan
            .blocks
            .get(usize::try_from(tb).ok()?)?
            .tiles
            .get(usize::try_from(ti).ok()?)?;
        if tile.root != id || tile.covered.as_slice() != [id] {
            return None;
        }
        self.encode_tile_frame_homed(tile)
    }

    /// Encode one tile against the frame-homed allocation.
    ///
    /// "Frame-homed" is not a simplification of a register allocation — it *is*
    /// this backend's allocation, the one `ir_lower::frame_word_off` states by
    /// returning `Err` for `ValueLoc::Reg`. Every value lives in its frame word;
    /// RAX carries the tile's destination and RCX its second operand, exactly as
    /// the per-opcode arms use them. That is what makes byte equality reachable
    /// at all, and it is why increment 3 (real registers) is a separate step
    /// with a prologue prerequisite.
    ///
    /// The two-address `MInst::Move` prefix costs nothing here and is not
    /// dropped: "copy the left operand into the destination's register" and
    /// "load the left operand" are the same instruction when the destination's
    /// register is RAX, so the tracker below emits it once whether the selector
    /// asked for the copy or coalesced it away.
    pub(in crate::ir_lower) fn encode_tile_frame_homed(&self, tile: &crate::x64::isel::Tile) -> Option<FrameAccessList> {
        use crate::x64::isel::{encode_allocated_alu, MInst, Operand, Ty};

        let mut out = FrameAccessList::new();
        // Which value RAX currently holds, if the tile has already loaded one.
        let mut rax_holds: Option<NodeId> = None;
        for inst in &tile.insts {
            match *inst {
                MInst::Move {
                    dst,
                    ty: Ty::I64,
                    src,
                } => {
                    if dst != tile.root {
                        return None;
                    }
                    let mut acc = FrameAccess::new();
                    enc_frame_load(RAX, self.frame_operand(src)?, &mut acc);
                    out.push(&acc)?;
                    rax_holds = Some(src);
                }
                MInst::AluRR {
                    op,
                    ty,
                    dst,
                    lhs,
                    rhs,
                } => {
                    if dst != tile.root {
                        return None;
                    }
                    if rax_holds != Some(lhs) {
                        let mut acc = FrameAccess::new();
                        enc_frame_load(RAX, self.frame_operand(lhs)?, &mut acc);
                        out.push(&acc)?;
                    }
                    let mut acc = FrameAccess::new();
                    enc_frame_load(RCX, self.frame_operand(rhs)?, &mut acc);
                    out.push(&acc)?;
                    // Level 3. `select` refuses rather than inventing an
                    // encoding, and every row it can answer with names the
                    // hand-written emitter it reproduces byte for byte.
                    //
                    // `Req::new`, not `.preserving_flags()`: EFLAGS are dead
                    // at every node boundary in this backend (each flag
                    // consumer -- a guard, a branch, a fused compare -- emits
                    // its own `CMP`/`TEST`), and an ALU tile writes them
                    // anyway. A tile that ever sits between a compare and its
                    // consumer must ask with `.preserving_flags()`
                    // (r11-x64gen-isel-select-ignores-flag-effects).
                    let encoded = encode_allocated_alu(op, ty, RAX, RAX, Operand::Gpr(RCX))?;
                    out.push_bytes(&encoded)?;
                    let mut acc = FrameAccess::new();
                    enc_frame_store(RAX, self.frame_destination(dst)?, &mut acc);
                    out.push(&acc)?;
                    rax_holds = None;
                }
                // `dst <- lhs op imm`. Reachable only from the SHADOW path —
                // `mir_tile_is_emittable` admits `Rule::AluReg` and nothing
                // else — because this form is not byte-equal to anything: the
                // per-opcode arm materialises the constant into RCX and uses
                // the register form, and dropping that load is the whole point.
                // It is encoded here so the saving can be *measured* before it
                // is spent.
                MInst::AluRI {
                    op,
                    ty,
                    dst,
                    lhs,
                    imm,
                    form: _,
                } => {
                    if dst != tile.root {
                        return None;
                    }
                    if rax_holds != Some(lhs) {
                        let mut acc = FrameAccess::new();
                        enc_frame_load(RAX, self.frame_operand(lhs)?, &mut acc);
                        out.push(&acc)?;
                    }
                    let encoded = encode_allocated_alu(op, ty, RAX, RAX, Operand::Imm(imm))?;
                    out.push_bytes(&encoded)?;
                    let mut acc = FrameAccess::new();
                    enc_frame_store(RAX, self.frame_destination(dst)?, &mut acc);
                    out.push(&acc)?;
                    rax_holds = None;
                }
                // Anything else is a shape this encoder has not been proved
                // byte-equal for. Refusing is free; guessing is not.
                _ => return None,
            }
        }
        if out.is_empty() {
            return None;
        }
        #[cfg(test)]
        if mir_injecting_a_wrong_byte() {
            out.corrupt_last_byte();
        }
        Some(out)
    }

    /// The `(phi, dst_slot, src_slot)` copies the edge `pred_block ->
    /// succ_block` carries: one per value phi of the successor's merge whose
    /// input for this edge is a real node. Pure; `emit_phi_copies` emits
    /// exactly these and `edge_has_phi_copies` asks whether there are any.
    pub(in crate::ir_lower) fn gather_phi_copies(&self, pred_block: usize, succ_block: usize) -> Vec<PhiCopy> {
        let merge_ctrl = self.schedule.blocks[succ_block].ctrl;
        if !matches!(
            self.graph.nodes[merge_ctrl as usize].op,
            Op::Merge | Op::Region
        ) {
            return Vec::new();
        }
        let mut out: Vec<PhiCopy> = Vec::new();
        // Only value phis materialise a frame slot; memory/control phis are
        // bookkeeping tokens with no machine value to copy. `phis_by_merge`
        // already applied that filter and keyed each phi by its merge
        // (`phi.inputs = [merge, val_0, val_1, …]`), in ascending id order.
        let Some(phis) = self.phis_by_merge.get(&merge_ctrl) else {
            return out;
        };
        // merge.inputs[k] is the control token for phi value k (= input k+1).
        // Which of them come from `pred_block` is the same for every phi of
        // this merge, so resolve it once rather than once per phi.
        let merge_node = &self.graph.nodes[merge_ctrl as usize];
        let from_pred: Vec<usize> = merge_node
            .inputs
            .iter()
            .enumerate()
            .filter(|&(_, &ctrl_in)| self.block_of_ctrl(ctrl_in) == Some(pred_block))
            .map(|(k, _)| k)
            .collect();
        if from_pred.is_empty() {
            return out;
        }
        for &phi_id in phis {
            let id = phi_id as usize;
            let node = &self.graph.nodes[id];
            for &k in &from_pred {
                if let Some(&val_id) = node.inputs.get(k + 1) {
                    if val_id != NO_NODE {
                        // `slot_of_unwritten`, not `slot_of`: this is the one
                        // consumer that legitimately wants the ADDRESS of a
                        // home word it may never write. Everything else that
                        // reaches `slot_of` is reading a VALUE, and for a
                        // dropped home that is exactly what must fail.
                        out.push(PhiCopy {
                            phi: id as NodeId,
                            dst: self.slot_of_unwritten(id as NodeId),
                            src: val_id,
                            src_slot: self.slot_of_unwritten(val_id),
                        });
                    }
                }
            }
        }
        out
    }

    /// Whether the edge `pred_block -> succ_block` carries any phi copy.
    ///
    /// Also `true` for an edge that carries split-residency RESOLUTION copies
    /// (round 9 wave 6, [`LsEdgeCopies`]): every caller uses the answer to
    /// decide whether the edge gets straight-line code of its own, and a
    /// resolution copy needs exactly that. (The switch arm refuses any edge
    /// this answers `true` for; the planner never gives a switch edge a copy --
    /// see `ls_edge_copy_site_exists`.)
    pub(in crate::ir_lower) fn edge_has_phi_copies(&self, pred_block: usize, succ_block: usize) -> bool {
        !self.gather_phi_copies(pred_block, succ_block).is_empty()
            || self
                .ls_edge_copies
                .iter()
                .any(|e| e.pred == pred_block && e.succ == succ_block)
    }

    /// The operands a fused compare branches on when one side of
    /// `Cmp(cc, lhs, rhs)` is a FUSED `lcmp` (see `fused_lcmp`): `(x, y)` for
    /// `Cmp(cc, LCmp(x, y), 0)` and `(y, x)` for `Cmp(cc, 0, LCmp(x, y))` --
    /// `0 <cc> sign(x - y)` is `y <cc> x`. `None` when neither side is one.
    pub(in crate::ir_lower) fn fused_lcmp_operands(&self, lhs: NodeId, rhs: NodeId) -> Option<(NodeId, NodeId)> {
        let is_fused = |id: NodeId| self.fused_lcmp.get(id as usize).copied().unwrap_or(false);
        let operands = |id: NodeId| -> Option<(NodeId, NodeId)> {
            let n = self.graph.nodes.get(id as usize)?;
            Some((*n.inputs.first()?, *n.inputs.get(1)?))
        };
        if is_fused(lhs) {
            return operands(lhs);
        }
        if is_fused(rhs) {
            return operands(rhs).map(|(x, y)| (y, x));
        }
        None
    }

    /// The `UCOMIS` a fused compare branches on when one side of
    /// `Cmp(cc, lhs, rhs)` is a FUSED `fcmp`/`dcmp` (see `fused_fcmp`):
    /// `(first, second, double, taken_jcc)`, where the branch is
    /// `UCOMIS first, second; J<taken_jcc>`. `Cmp(cc, 0, fcmp)` is
    /// `fcmp cc.swapped() 0`. `None` when neither side is one, or when the
    /// condition is not one `fcmp_fused_branch` folds (never the case for a
    /// node `prepare_fusion_tables` marked).
    pub(in crate::ir_lower) fn fused_fcmp_branch(
        &self,
        cc: CmpOp,
        lhs: NodeId,
        rhs: NodeId,
    ) -> Option<(NodeId, NodeId, bool, u8)> {
        let is_fused = |id: NodeId| self.fused_fcmp.get(id as usize).copied().unwrap_or(false);
        let (f, fcc) = if is_fused(lhs) {
            (lhs, cc)
        } else if is_fused(rhs) {
            (rhs, cc.swapped())
        } else {
            return None;
        };
        let n = self.graph.nodes.get(f as usize)?;
        let Op::FCmp {
            double,
            nan_greater,
        } = n.op
        else {
            return None;
        };
        let (x, y) = (*n.inputs.first()?, *n.inputs.get(1)?);
        let (swap, jcc) = fcmp_fused_branch(fcc, nan_greater)?;
        Some(if swap {
            (y, x, double, jcc)
        } else {
            (x, y, double, jcc)
        })
    }

    pub(in crate::ir_lower) fn value_inputs(&self, id: NodeId) -> impl Iterator<Item = NodeId> + '_ {
        self.value_flow.operands(id, &self.graph.nodes[id as usize], false)
    }

    /// Fill `use_count`, `deopt_named` and `fused_cmp` before the first block
    /// lowers. A compare is fused into its `If` when that `If` is its ONLY
    /// use, no safepoint snapshot names it, and the switch is on.
    pub(in crate::ir_lower) fn prepare_fusion_tables(&mut self) {
        let n = self.graph.nodes.len();
        let mut use_count = vec![0u32; n];
        for (id, node) in self.graph.nodes.iter().enumerate() {
            for input in self.value_flow.operands(id as NodeId, node, false) {
                if input != NO_NODE {
                    if let Some(c) = use_count.get_mut(input as usize) {
                        *c = c.saturating_add(1);
                    }
                }
            }
        }
        let mut deopt_named = vec![false; n];
        for sp in &self.graph.safepoints {
            for v in sp
                .locals
                .iter()
                .chain(sp.stack.iter())
                .chain(sp.monitors.iter())
                .copied()
                .chain(sp.sb_virtual_nodes())
            {
                if v != NO_NODE {
                    if let Some(cell) = deopt_named.get_mut(v as usize) {
                        *cell = true;
                    }
                }
            }
        }
        for v in loop_carried_recipe_values(self.sr_map) {
            if let Some(cell) = deopt_named.get_mut(v as usize) {
                *cell = true;
            }
        }
        let mut fused_cmp = vec![false; n];
        let mut fused_lcmp = vec![false; n];
        let mut fused_fcmp = vec![false; n];
        // Which values a deopt that can actually happen names -- asked only
        // when an `lcmp` candidate needs it, and computed at most once.
        let mut lcmp_reach: Option<Vec<bool>> = None;
        // JIT round 25 item 1a: compares admitted to the `If` fusion although a
        // frame state names them (no reachable deopt does). Marked in
        // `fused_cmp_unnamed` below once the fusion's last word is in.
        let mut unnamed_fuse_candidates: Vec<NodeId> = Vec::new();
        // Which values a deopt that can actually happen names, for this
        // round's two widenings. The residency plan's copy when it made one
        // (`set_residency`), else computed here: a body the linear scan
        // declined still has frame states to ask.
        self.elide_reach = if !self.elide_unnamed_homes_on {
            Vec::new()
        } else if self.deopt_named_reachable.len() == n {
            self.deopt_named_reachable.clone()
        } else {
            self.compute_deopt_named_reachable()
        };
        // And which values some frame state holds in a LOCAL or a monitor
        // rather than only on the operand stack. A stack entry lives from its
        // producer to the bytecode that pops it; a local lives on, into frame
        // states at bcis -- a back-edge poll's mode exit, a later call's
        // post-call exit -- that describe what they name by location and drop
        // the exit when a value has none. Neither widening takes such a value.
        self.elide_held = vec![false; if self.elide_unnamed_homes_on { n } else { 0 }];
        // Wave 2: the values an earlier attempt of this compile refused.
        self.elide_excluded = vec![false; n];
        // JIT rx27 wave 9: the same retry set, for the skipped reads.
        if self.publish_reload_elide_on {
            self.publish_reload_elided = vec![false; n];
            self.publish_reload_excluded = vec![false; n];
            for id in elide_excluded_ids() {
                if let Some(cell) = self.publish_reload_excluded.get_mut(id as usize) {
                    *cell = true;
                }
            }
        }
        if self.elide_unnamed_homes_on {
            for id in elide_excluded_ids() {
                if let Some(cell) = self.elide_excluded.get_mut(id as usize) {
                    *cell = true;
                }
            }
        }
        if self.elide_unnamed_homes_on {
            for sp in &self.graph.safepoints {
                for &v in sp.locals.iter().chain(sp.monitors.iter()) {
                    if v != NO_NODE {
                        if let Some(cell) = self.elide_held.get_mut(v as usize) {
                            *cell = true;
                        }
                    }
                }
            }
        }
        // The `If`s at the end of a block with a back edge: their back-edge
        // poll (and its mode exit) runs at the branch's bci, before the
        // branch reads its condition.
        let back_edge_terms: std::collections::HashSet<NodeId> = self
            .schedule
            .blocks
            .iter()
            .enumerate()
            .filter(|(b, blk)| blk.successors.iter().any(|&s| s <= *b))
            .filter_map(|(_, blk)| blk.terminator)
            .collect();
        if self.fused_branch_on {
            // Fusing moves the reads of the compare's inputs from the
            // compare's own position to the terminator's, so it is legal
            // exactly while nothing scheduled in between can overwrite where
            // those inputs live. Two things can:
            //
            // * a value sharing an input's HOME COLOUR — `plan_slots` packs
            //   values into shared frame words by live range, and the
            //   allocator's model ends an input's range at the compare;
            // * a value promoted into an input's REGISTER, for the same
            //   reason. `test_lower_ternary_lt_phi` caught exactly that: `a`
            //   and the constant `1` were handed the same RBX.
            //
            // Both are checked against the plans this lowering was handed,
            // rather than approximated by requiring the compare to be the last
            // scheduled node — which is what the first cut did, and it
            // declined `fib`, whose two subtractions sit between its compare
            // and its branch.
            let color_of = |plan: &SlotPlan, id: NodeId| -> Option<u32> {
                plan.node_color.get(id as usize).copied().flatten()
            };
            // JIT round 18 wave 11 (lane osrfollow): asked once per lowering.
            let fused_class_test = ir_fused_class_test_branch_enabled();
            // JIT rx27 wave 1 (lane scanloop): what became of each `If` on an
            // `ExactClassIs`, for the `CRATONVM_DBG_JITC` line below: seen,
            // fused, and refused for the switch, a test in another block, a
            // second use or a frame state naming it, a clobbered input.
            let mut class_census = [0usize; 6];
            for block in &self.schedule.blocks {
                let Some(term) = block.terminator else {
                    continue;
                };
                let Some(if_node) = self.graph.nodes.get(term as usize) else {
                    continue;
                };
                if !matches!(if_node.op, Op::If) {
                    continue;
                }
                let Some(&cond) = if_node.inputs.get(1) else {
                    continue;
                };
                if cond == NO_NODE {
                    continue;
                }
                let class_test = matches!(
                    self.graph.nodes.get(cond as usize).map(|n| &n.op),
                    Some(Op::ExactClassIs { .. })
                );
                if class_test {
                    class_census[0] += 1;
                }
                let Some(pos) = block.nodes.iter().position(|&id| id == cond) else {
                    if class_test {
                        class_census[3] += 1;
                    }
                    continue;
                };
                let Some(cmp) = self.graph.nodes.get(cond as usize) else {
                    continue;
                };
                // JIT round 18 wave 11 (lane osrfollow, OSRPOLY18-1): an
                // `ExactClassIs` whose only use is this `If` -- every class
                // test of a guarded receiver splice -- branches on the class
                // compare's own flags (`lower_terminator`'s `Op::If` arm)
                // instead of materialising 0/1, storing, reloading and testing
                // it. The same three questions as a compare: one use, no frame
                // state naming it, and nothing scheduled after it that shares
                // its one input's home colour or register. Opt-in
                // (`ir_fused_class_test_branch_enabled`).
                if matches!(cmp.op, Op::ExactClassIs { .. }) {
                    let deopt_pinned = deopt_named[cond as usize];
                    let unreached = deopt_pinned
                        && self.elide_unnamed_homes_on
                        && !self.elide_reach.get(cond as usize).copied().unwrap_or(true)
                        && !self.elide_held.get(cond as usize).copied().unwrap_or(true)
                        && !self
                            .elide_excluded
                            .get(cond as usize)
                            .copied()
                            .unwrap_or(true)
                        && !back_edge_terms.contains(&term);
                    if !fused_class_test {
                        class_census[2] += 1;
                    } else if cmp.inputs.len() != 1
                        || cmp.inputs[0] == NO_NODE
                        || use_count[cond as usize] != 1
                        || (deopt_pinned && !unreached)
                    {
                        class_census[4] += 1;
                    }
                    if fused_class_test
                        && cmp.inputs.len() == 1
                        && cmp.inputs[0] != NO_NODE
                        && use_count[cond as usize] == 1
                        && (!deopt_pinned || unreached)
                    {
                        let r = cmp.inputs[0];
                        let r_color = color_of(&self.slot_plan, r);
                        let r_gp = self.assigned_gpr(r);
                        let clobbered = block.nodes[pos + 1..].iter().any(|&later| {
                            let c = color_of(&self.slot_plan, later);
                            let g = self.assigned_gpr(later);
                            (c.is_some() && c == r_color) || (g.is_some() && g == r_gp)
                        });
                        if !clobbered {
                            fused_cmp[cond as usize] = true;
                            if deopt_pinned {
                                unnamed_fuse_candidates.push(cond);
                            }
                            class_census[1] += 1;
                        } else {
                            class_census[5] += 1;
                        }
                    }
                    continue;
                }
                if !matches!(cmp.op, Op::Cmp(_)) || cmp.inputs.len() < 2 {
                    continue;
                }
                if use_count[cond as usize] != 1 {
                    continue;
                }
                // JIT round 25 item 1a (`CRATONVM_JIT_IR_ELIDE_UNNAMED_HOMES`):
                // a frame state naming the compare pins it only if a deopt that
                // can actually happen reads that frame state -- the question
                // the `lcmp` fusion below already asks of the `lcmp`. A
                // materialised boolean that a spliced `return a < b` hands to
                // the caller's `ifeq` is on the caller's operand stack at the
                // `ifeq`, which cannot trap, so it was `SETcc; MOVZX; store;
                // reload; TEST; Jcc` for nothing. `frame_value_for` refuses a
                // point that would describe one (`fused_cmp_unnamed`).
                if deopt_named[cond as usize] {
                    if self.elide_unnamed_homes_on {
                        let i = cond as usize;
                        let why = [
                            self.elide_reach.get(i).copied().unwrap_or(true),
                            self.elide_held.get(i).copied().unwrap_or(true),
                            self.elide_excluded.get(i).copied().unwrap_or(true),
                            back_edge_terms.contains(&term),
                        ];
                        for (k, hit) in why.iter().enumerate() {
                            if *hit {
                                self.cmp_unnamed_refused[k] += 1;
                            }
                        }
                    }
                    let unreached = self.elide_unnamed_homes_on
                        && !self.elide_reach.get(cond as usize).copied().unwrap_or(true)
                        && !self.elide_held.get(cond as usize).copied().unwrap_or(true)
                        && !self
                            .elide_excluded
                            .get(cond as usize)
                            .copied()
                            .unwrap_or(true)
                        && !back_edge_terms.contains(&term);
                    if !unreached {
                        continue;
                    }
                    unnamed_fuse_candidates.push(cond);
                }
                let (a, b) = (cmp.inputs[0], cmp.inputs[1]);
                let in_colors = [color_of(&self.slot_plan, a), color_of(&self.slot_plan, b)];
                // Through the sanctioned accessors, never `reg_of` directly:
                // `the_register_read_path_is_gated_on_publication` is what
                // keeps "assigned" and "published" from drifting apart.
                let in_gp = [self.assigned_gpr(a), self.assigned_gpr(b)];
                let in_fp = [self.assigned_xmm(a), self.assigned_xmm(b)];
                let clobbered = block.nodes[pos + 1..].iter().any(|&later| {
                    let c = color_of(&self.slot_plan, later);
                    let g = self.assigned_gpr(later);
                    let f = self.assigned_xmm(later);
                    (c.is_some() && in_colors.contains(&c))
                        || (g.is_some() && in_gp.contains(&g))
                        || (f.is_some() && in_fp.contains(&f))
                });
                // Round 9 wave 6 (irlower6): a clobbered input no longer ends
                // the question here. When the compare is `Cmp(cc, LCmp(x, y), 0)`
                // and the `lcmp` fuses below, the branch reads NEITHER of the
                // compare's own inputs -- `lower_terminator` compares `x` and
                // `y` (`fused_lcmp_operands`), which the `lcmp` block checks
                // over everything scheduled after the `lcmp` -- so the frame
                // colour of the `lcmp` result or of the constant `0` being
                // reused later in the block is irrelevant. That reuse is the
                // common case, not a corner: the `lcmp`'s range ends at the
                // compare, so `plan_slots` hands its word to the loop body's
                // first temporary, and a `for (long i ...)` loop whose body the
                // scheduler placed ahead of the exit test lost BOTH fusions
                // (`ArithOsr.mulOnly`'s optimizing OSR body on the wave-5
                // binary: `setg; setl; sub; movsxd; store; reload; cmp; setge;
                // store` ... `reload; test; jne` per iteration). On that path
                // the compare is marked fused only TOGETHER with its `lcmp`,
                // at the bottom of this block.
                if !clobbered {
                    fused_cmp[cond as usize] = true;
                }
                // ── And the `lcmp` under it, when there is one ──────────
                //
                // Round 9 wave 3 (irlower3). `lcmp; if<cc>` builds
                // `If(Cmp(cc, LCmp(x, y), 0))`, and `lcmp` is exactly the sign
                // of `x - y` with no overflow, so `Cmp(cc, LCmp(x, y), 0)` is
                // `x <cc> y` as signed longs for all six conditions (and
                // `Cmp(cc, 0, LCmp(x, y))` is `y <cc> x`). Fusing it moves the
                // reads of `x` and `y` from the LCmp's position to the branch,
                // so the same two questions are asked of them over everything
                // scheduled after the LCmp, plus the one the compare above
                // does not need: nothing that can actually deopt may name the
                // LCmp, because after this it has no location at all
                // (`frame_value_for` refuses the compile if one does, so a
                // wrong prediction costs the method, never the answer).
                let zero_at = |id: NodeId| {
                    matches!(
                        self.graph.nodes.get(id as usize).map(|n| &n.op),
                        Some(Op::Const(0))
                    )
                };
                let lc = if zero_at(b) {
                    a
                } else if zero_at(a) {
                    b
                } else {
                    continue;
                };
                let Some(lnode) = self.graph.nodes.get(lc as usize) else {
                    continue;
                };
                if lnode.inputs.len() < 2 {
                    continue;
                }
                // Round 9 wave 8 (irl8): the `fcmp<l|g>` / `dcmp<l|g>` twin.
                // `Cmp(cc, FCmp(x, y), 0)` folds to ONE `UCOMIS; Jcc` exactly
                // when the condition is an ordering (`<`, `<=`, `>`, `>=`): the
                // NaN answer the `l`/`g` bias picks is then a single flags
                // predicate (`fcmp_fused_branch`). `==`/`!=` need ZF and PF
                // together, which one Jcc byte cannot express, so those keep
                // the materialised {-1,0,1}. The condition is taken from the
                // three-way result's side: `Cmp(cc, 0, fcmp)` is
                // `fcmp cc.swapped() 0`.
                let lc_is_fcmp = match lnode.op {
                    Op::LCmp => false,
                    Op::FCmp { nan_greater, .. } => {
                        let Op::Cmp(outer_cc) = cmp.op else {
                            continue;
                        };
                        let lc_cc = if lc == a {
                            outer_cc
                        } else {
                            outer_cc.swapped()
                        };
                        if fcmp_fused_branch(lc_cc, nan_greater).is_none()
                        {
                            continue;
                        }
                        true
                    }
                    _ => continue,
                };
                let (la, lb) = (lnode.inputs[0], lnode.inputs[1]);
                if la == NO_NODE || lb == NO_NODE {
                    continue;
                }
                if use_count.get(lc as usize).copied().unwrap_or(0) != 1 {
                    continue;
                }
                let Some(lpos) = block.nodes[..pos].iter().position(|&id| id == lc) else {
                    continue;
                };
                if deopt_named.get(lc as usize).copied().unwrap_or(true) {
                    // The builder's frame state at the `if<cc>` bci lists the
                    // `lcmp` result on the operand stack, so this is the
                    // common case, and the refinement is what makes it fire.
                    let reach = lcmp_reach.get_or_insert_with(|| {
                        if self.deopt_named_reachable.is_empty() {
                            self.compute_deopt_named_reachable()
                        } else {
                            self.deopt_named_reachable.clone()
                        }
                    });
                    if reach.get(lc as usize).copied().unwrap_or(true) {
                        continue;
                    }
                }
                let l_colors = [color_of(&self.slot_plan, la), color_of(&self.slot_plan, lb)];
                let l_gp = [self.assigned_gpr(la), self.assigned_gpr(lb)];
                let l_fp = [self.assigned_xmm(la), self.assigned_xmm(lb)];
                let l_clobbered = block.nodes[lpos + 1..].iter().any(|&later| {
                    let c = color_of(&self.slot_plan, later);
                    let g = self.assigned_gpr(later);
                    let f = self.assigned_xmm(later);
                    (c.is_some() && l_colors.contains(&c))
                        || (g.is_some() && l_gp.contains(&g))
                        || (f.is_some() && l_fp.contains(&f))
                });
                if l_clobbered {
                    continue;
                }
                // Both, together: when the compare's own inputs were clobbered
                // above it was left unfused, and it is sound to fuse it now
                // precisely because its branch reads `x` and `y` instead.
                fused_cmp[cond as usize] = true;
                if lc_is_fcmp {
                    fused_fcmp[lc as usize] = true;
                } else {
                    fused_lcmp[lc as usize] = true;
                }
            }
            // JIT rx27 wave 1 (lane scanloop): one line per lowering with a
            // class-test branch (the regex scan loops' lambda and node class
            // tests), so `CRATONVM_JIT_IR_FUSED_CLASS_TEST_BRANCH=1` can be
            // seen to engage, or not, on the host.
            if class_census[0] > 0 && crate::x64::dbg_jitc_enabled() {
                eprintln!(
                    "[cratonvm-jitc] ir-fused-class-test {}: class-test branches={} fused={} \
                     refused: switch-off={} other-block={} used-or-named={} clobbered={}",
                    self.graph.scope_method_key,
                    class_census[0],
                    class_census[1],
                    class_census[2],
                    class_census[3],
                    class_census[4],
                    class_census[5],
                );
            }
            // ── Round 11 (irlower): the same fusion for `Op::Guard` ──────
            //
            // `Guard(Cmp(cc, a, b))` is the builder's shape for every
            // div-by-zero guard (`add_div_zero_guard`), every receiver null
            // guard it plants and every pruned branch (`plant_uncommon_trap`),
            // and each one materialised its boolean (`SETcc; MOVZX; store`)
            // only for the guard to reload it and `TEST` it -- five
            // instructions and a store-to-load round trip per guard, in the hot
            // path by construction. The questions are exactly the `If`'s, with
            // the window ending at the GUARD instead of at the block's end: one
            // use, no frame state naming the compare, and nothing scheduled
            // between the two that shares an input's home colour or register.
            // No `lcmp`/`fcmp` refinement here: a `Cmp` over one is compared as
            // the plain `int` it materialised.
            for block in &self.schedule.blocks {
                for (gpos, &gid) in block.nodes.iter().enumerate() {
                    let Some(gnode) = self.graph.nodes.get(gid as usize) else {
                        continue;
                    };
                    if !matches!(gnode.op, Op::Guard { .. }) {
                        continue;
                    }
                    let Some(&cond) = gnode.inputs.get(1) else {
                        continue;
                    };
                    if cond == NO_NODE {
                        continue;
                    }
                    let Some(before) = block.nodes.get(..gpos) else {
                        continue;
                    };
                    let Some(cpos) = before.iter().position(|&id| id == cond) else {
                        continue;
                    };
                    let Some(cmp) = self.graph.nodes.get(cond as usize) else {
                        continue;
                    };
                    // An `ExactClassIs` (a hoisted class predicate's test) fuses
                    // as the `If` arm's does: its one input is read at the guard.
                    let class_test = fused_class_test
                        && matches!(cmp.op, Op::ExactClassIs { .. })
                        && cmp.inputs.len() == 1;
                    if !class_test && (!matches!(cmp.op, Op::Cmp(_)) || cmp.inputs.len() < 2) {
                        continue;
                    }
                    if use_count.get(cond as usize).copied().unwrap_or(0) != 1
                        || deopt_named.get(cond as usize).copied().unwrap_or(true)
                    {
                        continue;
                    }
                    let (a, b) = (cmp.inputs[0], cmp.inputs.get(1).copied().unwrap_or(cmp.inputs[0]));
                    if a == NO_NODE || b == NO_NODE {
                        continue;
                    }
                    let in_colors = [color_of(&self.slot_plan, a), color_of(&self.slot_plan, b)];
                    let in_gp = [self.assigned_gpr(a), self.assigned_gpr(b)];
                    let in_fp = [self.assigned_xmm(a), self.assigned_xmm(b)];
                    let Some(between) = before.get(cpos + 1..) else {
                        continue;
                    };
                    let clobbered = between.iter().any(|&later| {
                        let c = color_of(&self.slot_plan, later);
                        let g = self.assigned_gpr(later);
                        let f = self.assigned_xmm(later);
                        (c.is_some() && in_colors.contains(&c))
                            || (g.is_some() && in_gp.contains(&g))
                            || (f.is_some() && in_fp.contains(&f))
                    });
                    if !clobbered {
                        fused_cmp[cond as usize] = true;
                    }
                }
            }
        }
        // ── Single-use intermediates, carried to their consumer ──────
        //
        // Same three questions the fusion above asks, and the same answers
        // make it safe: exactly one use, no frame state naming it, and nothing
        // scheduled in between. The difference is only where the value ends up
        // — a fused compare is recomputed at the branch, a carried value is
        // simply left where the arm already put it.
        let n_nodes = self.graph.nodes.len();
        let mut carry_of: Vec<Option<(u8, NodeId)>> = vec![None; n_nodes];
        // Per-cause, because a bare zero from this planner cannot be acted on:
        // [not-single-use, wrong-type, producer-arm, already-resident,
        //  consumer-arm, wrong-operand-position].
        let mut carry_skips = [0usize; 6];
        let mut carry_named = 0usize;
        // Values whose only pin was a frame state nothing can reach.
        let mut carry_unreachable = 0usize;
        let graph_trap_free = self.graph_cannot_deopt();
        let mut carry_droppable: Vec<NodeId> = Vec::new();
        if ir_carry_single_use_enabled() {
            for block in &self.schedule.blocks {
                for w in 1..block.nodes.len() {
                    let prod = block.nodes[w - 1];
                    let cons = block.nodes[w];
                    if use_count.get(prod as usize).copied().unwrap_or(0) != 1 {
                        carry_skips[0] += 1;
                        continue;
                    }
                    let Some(pn) = self.graph.nodes.get(prod as usize) else {
                        continue;
                    };
                    if !matches!(pn.ty, IrType::Int | IrType::Long) {
                        carry_skips[1] += 1;
                        continue;
                    }
                    // The producer's result must be in RAX at its home store,
                    // which is exactly what this predicate certifies.
                    if !op_home_is_one_store_rax(&pn.op) {
                        carry_skips[2] += 1;
                        continue;
                    }
                    // A value the residency file gave a register publishes
                    // there instead; that path already removes the reload and
                    // the two must not both claim the store.
                    if self.assigned_gpr(prod).is_some() {
                        carry_skips[3] += 1;
                        continue;
                    }
                    let Some(cn) = self.graph.nodes.get(cons as usize) else {
                        continue;
                    };
                    if !op_reads_rax_then_rcx(&cn.op) {
                        carry_skips[4] += 1;
                        continue;
                    }
                    // Those arms take an FP path for an FP result, and that
                    // path reads through `fp_load_value`, which knows nothing
                    // about a carry.
                    if !matches!(cn.ty, IrType::Int | IrType::Long) {
                        carry_skips[4] += 1;
                        continue;
                    }
                    let reg = if self.value_inputs(cons).next() == Some(prod) {
                        RAX
                    } else if self.value_inputs(cons).nth(1) == Some(prod) {
                        RCX
                    } else {
                        carry_skips[5] += 1;
                        continue;
                    };
                    carry_of[prod as usize] = Some((reg, cons));
                    // ── And, separately, may the home STORE go too? ──
                    //
                    // Eliding the LOAD needs nothing from the frame: the home
                    // word is still written and every frame state still
                    // resolves through it. Dropping the STORE needs the
                    // stronger claim that nothing else can ever want the word
                    // — and `graph.safepoints` records the FULL OPERAND STACK
                    // at every bci, so an intermediate is named by a frame
                    // state from its definition until its consumer pops it.
                    //
                    // That is why this is a separate question with its own
                    // census line rather than a clause above. On a loop kernel
                    // it answers no every time, and the reason is worth having
                    // in front of you: the method emits no deopt stub at all
                    // (`ir_osr_sentinel_free`), so all of those frame states
                    // are unreachable — and they still pin every intermediate
                    // to memory.
                    //
                    // The consumer must also be unable to TRAP. `Op::Div` and
                    // `Op::Rem` emit their zero guard AFTER reading both
                    // operands, and that guard is a deopt point whose frame
                    // state names them — so a dropped home there would hand
                    // the resume a word nothing wrote. Today `deopt_named`
                    // already answers no for those operands (the bci's frame
                    // state lists them), and this clause is what makes that
                    // agreement a rule rather than a coincidence.
                    //
                    // `deopt_named` alone is the wrong question: a frame state
                    // that names this value is a reason to keep its home word
                    // only if some deopt can consult that frame state. When the
                    // graph can trap nowhere, none of them can — and
                    // `lower_inner` checks that prediction against the emission
                    // before any point is skipped.
                    // (Round 9 wave 3: a division by a non-zero constant
                    // emits no guard at all, so it is not a trapping consumer.)
                    let consumer_traps = matches!(cn.op, Op::Div | Op::Rem)
                        && !int_division_cannot_trap(self.graph, cn);
                    let named = deopt_named.get(prod as usize).copied().unwrap_or(true);
                    let reachable = named && !graph_trap_free;
                    if consumer_traps || reachable {
                        carry_named += 1;
                    } else {
                        if named {
                            carry_unreachable += 1;
                        }
                        carry_droppable.push(prod);
                    }
                }
            }
        }
        // A value whose home store is dropped has ONE readable location, so
        // every other reader must refuse rather than take the word.
        // `home_dropped` is that refusal and it already exists; sizing it here
        // matters because residency may be off entirely, and this path does not
        // depend on it.
        if !carry_droppable.is_empty() {
            if self.home_dropped.len() < n_nodes {
                self.home_dropped.resize(n_nodes, false);
            }
            for id in &carry_droppable {
                self.home_dropped[*id as usize] = true;
            }
        }
        // ── The SECOND operand, one position further back ────────────────
        //
        // `ir_schedule::pair_single_use_operands` leaves a consumer's two
        // single-use operands as `[input1, input0, cons]`. The loop above is
        // strictly adjacent, so it sees `input0` and cannot see `input1` at
        // all. This picks up that one extra position and only that one.
        //
        // `input1` is copied to RCX at its home store and has to survive
        // `input0`'s arm. `op_preserves_rcx` is that promise and it is two ops
        // long, because every binary arm loads its own second operand into RCX.
        // The emitter re-checks it against what was actually LOWERED
        // (`lower_data_node_tracked`) rather than trusting this plan.
        //
        // Everything else is the adjacent rule verbatim: one use, an `Int` or
        // `Long`, a producer whose home store is one `store_rax`, a consumer
        // that reads RAX then RCX, and no residency claim on the value.
        let mut deferred_skips = [0usize; 8];
        let mut deferred_mid_foldable = 0usize;
        let mut deferred_candidates = 0usize;
        if ir_carry_single_use_enabled() && ir_carry_second_operand_enabled() {
            if self.carry_deferred.len() < n_nodes {
                self.carry_deferred.resize(n_nodes, false);
            }
            for block in &self.schedule.blocks {
                for w in 2..block.nodes.len() {
                    let cons = block.nodes[w];
                    let mid = block.nodes[w - 1];
                    let prod = block.nodes[w - 2];
                    // The middle node must already be carrying into THIS
                    // consumer in RAX. That is what proves the three were put
                    // together deliberately, rather than dependence order
                    // happening to look the same.
                    if carry_of.get(mid as usize).copied().flatten() != Some((RAX, cons)) {
                        deferred_skips[0] += 1;
                        continue;
                    }
                    // From here the window IS the shape the second slot exists
                    // for — a consumer already taking its first operand in RAX
                    // — so everything below is a CANDIDATE declined, and the
                    // causes are split finely enough to be acted on. Above it is
                    // every other triple of scheduled nodes, which is a
                    // denominator no decision depends on.
                    deferred_candidates += 1;
                    let (Some(pn), Some(cn)) = (
                        self.graph.nodes.get(prod as usize),
                        self.graph.nodes.get(cons as usize),
                    ) else {
                        continue;
                    };
                    // RCX is the second operand's register, so this only ever
                    // applies to a value read there. When the node two back is
                    // not that operand, the operand is somewhere the pairing
                    // pass could not bring it — another block, a phi, a
                    // parameter, or behind a node it may not cross.
                    if self.value_inputs(cons).nth(1) != Some(prod) {
                        deferred_skips[5] += 1;
                        continue;
                    }
                    if carry_of.get(prod as usize).copied().flatten().is_some() {
                        deferred_skips[1] += 1;
                        continue;
                    }
                    if use_count.get(prod as usize).copied().unwrap_or(0) != 1 {
                        deferred_skips[2] += 1;
                        continue;
                    }
                    if !matches!(pn.ty, IrType::Int | IrType::Long)
                        || !op_home_is_one_store_rax(&pn.op)
                    {
                        deferred_skips[3] += 1;
                        continue;
                    }
                    if self.assigned_gpr(prod).is_some() {
                        deferred_skips[4] += 1;
                        continue;
                    }
                    if !matches!(cn.ty, IrType::Int | IrType::Long)
                        || !op_reads_rax_then_rcx(&cn.op)
                    {
                        deferred_skips[6] += 1;
                        continue;
                    }
                    // The one arm in between has to leave RCX alone. Asked of
                    // the NODE, not the op: a binary arm reaches RCX only
                    // through the register form of its own second operand, and
                    // a folded operand never takes that branch.
                    if !self.node_preserves_rcx(mid) {
                        deferred_skips[7] += 1;
                        // …and how many of those are the folded shape, so a run
                        // with the widening switched OFF still prices it.
                        if self.mid_would_fold(mid) {
                            deferred_mid_foldable += 1;
                        }
                        continue;
                    }
                    carry_of[prod as usize] = Some((RCX, cons));
                    self.carry_deferred[prod as usize] = true;
                    // The home STORE, on exactly the terms the adjacent case
                    // uses.
                    // (Round 9 wave 3: a division by a non-zero constant
                    // emits no guard at all, so it is not a trapping consumer.)
                    let consumer_traps = matches!(cn.op, Op::Div | Op::Rem)
                        && !int_division_cannot_trap(self.graph, cn);
                    let named = deopt_named.get(prod as usize).copied().unwrap_or(true);
                    if consumer_traps || (named && !graph_trap_free) {
                        carry_named += 1;
                    } else {
                        if named {
                            carry_unreachable += 1;
                        }
                        carry_droppable.push(prod);
                    }
                }
            }
        }
        // A value whose home store is dropped has ONE readable location, so
        // every other reader must refuse rather than take the word.
        if !carry_droppable.is_empty() {
            if self.home_dropped.len() < n_nodes {
                self.home_dropped.resize(n_nodes, false);
            }
            for id in &carry_droppable {
                self.home_dropped[*id as usize] = true;
            }
        }
        self.deferred_skips = deferred_skips;
        self.deferred_mid_foldable = deferred_mid_foldable;
        self.deferred_candidates = deferred_candidates;
        self.carry_skips = carry_skips;
        self.carry_named = carry_named;
        self.unreachable_homes += carry_unreachable;
        self.carry_of = carry_of;
        self.use_count = use_count;
        self.deopt_named = deopt_named;
        self.fuse_final_instanceof_sites(&self.use_count, &self.deopt_named, &mut fused_cmp);
        // JIT round 25 item 1a: the compares the widened rule actually fused.
        self.fused_cmp_unnamed = vec![false; n];
        self.fused_cmp_unnamed_count = 0;
        for cond in unnamed_fuse_candidates {
            if fused_cmp.get(cond as usize).copied().unwrap_or(false) {
                if let Some(cell) = self.fused_cmp_unnamed.get_mut(cond as usize) {
                    if !*cell {
                        *cell = true;
                        self.fused_cmp_unnamed_count += 1;
                    }
                }
            }
        }
        self.fused_cmp = fused_cmp;
        self.fused_lcmp = fused_lcmp;
        self.fused_fcmp = fused_fcmp;
        // JIT rx27 wave 4 (lane regalloc): `(x & mask) == 0` branches as one
        // `TEST`, before the tail carries (which must not plan the `And`).
        self.plan_and_zero_tests();
        self.plan_fused_i2l_adds();
        // JIT rx27 wave 6 (lane regalloc): and the guards' tests.
        self.plan_and_zero_guard_tests();
        self.extend_home_drops_to_carried_values();
        self.plan_tail_carries();
        self.plan_site_named_homes();
        self.prepare_unsigned_byte_array_loads();
        self.plan_fused_index_ands();
        // JIT round 25 wave 6 (lane W6-B): the `ir-home-why` census, after
        // every plan that drops a home (`CRATONVM_DBG_JITC` only).
        self.plan_home_keep_census();
    }

    /// JIT rx27 wave 4 (lane regalloc, [`ir_and_zero_test_branch_enabled`]):
    /// plan the `If`s whose fused condition is `Cmp(Eq | Ne, And(x, mask), 0)`
    /// (either operand order), to branch on `TEST x, mask` instead of
    /// computing the `And` into RAX, storing it home and comparing the home
    /// word with 0.
    ///
    /// # What it removes
    ///
    /// The regex predicates test a character's class bits once per character
    /// (`(ch & 0xFFFFFF80) == 0`, `(type & mask) != 0`). rx27 wave 4's profile of
    /// the spliced `Pattern$Start.match` loop shows the shape
    /// `mov rax,[x]; and eax,0FFFFFF80h; mov [w],rax; cmp dword [w],0; jcc`:
    /// four instructions and a store-to-load round trip where `test` and the
    /// `jcc` do. The `And`'s value is read by nothing but that compare.
    ///
    /// # When
    ///
    /// The `If`'s compare is already fused (one use, no frame state naming it,
    /// inputs not clobbered before the branch); the `And` is in the same block,
    /// `Int` or `Long`, has a mask the immediate forms take (`alu_imm32`: an
    /// `i32`, sign-extended for a `Long`, which is what the `And` itself
    /// computes with), exactly ONE use (that compare), no snapshot names it
    /// (`deopt_named`), no register and no carry in or out. And `x` is not
    /// overwritten where it lives (home colour or register) by any node
    /// scheduled after the `And` in the block, so reading it at the branch
    /// reads the value the `And` would have read: the compare-fusion rule,
    /// moved one node earlier.
    ///
    /// # Why it is exact
    ///
    /// `TEST` sets ZF exactly when `x & mask` is zero, at the operand width
    /// the `And` has, and the `If` branches on ZF for `Eq` / `Ne` as it did
    /// on the compare with 0. No other reader of the `And` exists (one use),
    /// and no frame state names it, so its value is needed nowhere else.
    pub(in crate::ir_lower) fn plan_and_zero_tests(&mut self) {
        let n = self.graph.nodes.len();
        self.fused_test_and = vec![false; n];
        self.test_fused_cond = vec![None; n];
        self.and_zero_widened = 0;
        if !ir_and_zero_test_branch_enabled()
            || !self.fused_branch_on
            || self.mir_mode != MirMode::Off
        {
            return;
        }
        // seen, fused, then refused: not single-use or named, carried or
        // resident, clobbered, another shape (no immediate mask, another block).
        let mut census = [0usize; 6];
        let flag = |v: &[bool], id: NodeId| v.get(id as usize).copied().unwrap_or(false);
        let color = |id: NodeId| {
            self.slot_plan
                .node_color
                .get(id as usize)
                .copied()
                .flatten()
        };
        // (cond, a, x, imm, wide, named, promote_cmp, cond_named, cancel)
        let mut plan: Vec<(
            NodeId,
            NodeId,
            NodeId,
            i32,
            bool,
            bool,
            bool,
            bool,
            Vec<NodeId>,
        )> = Vec::new();
        for (b, block) in self.schedule.blocks.iter().enumerate() {
            let Some(term) = block.terminator else {
                continue;
            };
            let Some(tn) = self.graph.nodes.get(term as usize) else {
                continue;
            };
            if tn.op != Op::If {
                continue;
            }
            let Some(&cond) = tn.inputs.get(1) else {
                continue;
            };
            let promote_cmp = !flag(&self.fused_cmp, cond);
            let mut cond_named = false;
            if promote_cmp {
                if flag(&self.fused_fcmp, cond)
                    || flag(&self.fused_lcmp, cond)
                    || self.use_count.get(cond as usize).copied() != Some(1)
                    || !block.nodes.contains(&cond)
                {
                    continue;
                }
                cond_named = flag(&self.deopt_named, cond);
                if cond_named {
                    let back_edge = block.successors.iter().any(|&s| s <= b);
                    let unreached = self.elide_unnamed_homes_on
                        && !flag(&self.elide_reach, cond)
                        && !flag(&self.elide_held, cond)
                        && !flag(&self.elide_excluded, cond)
                        && !back_edge;
                    if !unreached {
                        continue;
                    }
                }
            }
            let Some(cn) = self.graph.nodes.get(cond as usize) else {
                continue;
            };
            if !matches!(cn.op, Op::Cmp(CmpOp::Eq) | Op::Cmp(CmpOp::Ne)) || cn.inputs.len() != 2 {
                continue;
            }
            let is_zero = |id: NodeId| {
                matches!(self.graph.nodes.get(id as usize).map(|n| &n.op), Some(Op::Const(0)))
            };
            let a = if is_zero(cn.inputs[1]) {
                cn.inputs[0]
            } else if is_zero(cn.inputs[0]) {
                cn.inputs[1]
            } else {
                continue;
            };
            let Some(an) = self.graph.nodes.get(a as usize) else {
                continue;
            };
            if an.op != Op::And
                || !matches!(an.ty, IrType::Int | IrType::Long)
                || an.inputs.len() != 2
            {
                continue;
            }
            census[0] += 1;
            let (x, imm) = match (self.alu_imm32(an.inputs[1]), self.alu_imm32(an.inputs[0])) {
                (Some(imm), _) => (an.inputs[0], imm),
                (None, Some(imm)) => (an.inputs[1], imm),
                (None, None) => {
                    census[5] += 1;
                    continue;
                }
            };
            // A load the `And`'s own instruction absorbed is emitted by nothing
            // once the fold drops the `And`.
            let x_ok = x != NO_NODE
                && !self.value_flow.is_absorbed(x)
                && self
                    .graph
                    .nodes
                    .get(x as usize)
                    .is_some_and(|xn| xn.ty == an.ty);
            // JIT rx27 wave 8b: the `And` in the branch's block, or (pure
            // nodes are scheduled early) in a block that dominates it, when
            // `x`'s home colour is held through the branch (its slot range
            // covers the terminator's position), so nothing between can have
            // overwritten it; a register `x` lost on the way is read from that
            // home, and a dropped one refuses the attempt (`slot_of`).
            let pos = block.nodes.iter().position(|&id| id == a);
            let held_to_term = || {
                let t = self.slot_plan.pos_of.get(term as usize).copied().flatten();
                let r = self
                    .slot_plan
                    .colour_range
                    .get(x as usize)
                    .copied()
                    .flatten()
                    .or_else(|| self.slot_plan.range.get(x as usize).copied().flatten());
                matches!((t, r), (Some(t), Some(r)) if r.lo <= t && t <= r.hi)
            };
            let dominating = pos.is_none()
                && self.schedule.node_to_block.get(a as usize).is_some_and(|&ab| {
                    ab != b && ab < self.schedule.blocks.len() && self.schedule.dom.dominates(ab, b)
                })
                && held_to_term();
            if pos.is_none() && !dominating {
                census[5] += 1;
                continue;
            }
            if !x_ok {
                census[5] += 1;
                continue;
            }
            // JIT rx27 wave 6b: a frame state may name the `And` as long as no
            // deopt that can actually happen does (the snapshot at the
            // branch's own bci, where nothing traps): the item-1a widening
            // (`elide_reach`, `fused_cmp_unnamed`). A frame state that IS read
            // then meets `fused_test_and` in `frame_value_for` and refuses the
            // attempt, which is lowered again with every home kept.
            let named = flag(&self.deopt_named, a);
            let unread = self.elide_unnamed_homes_on
                && !self.elide_reach.get(a as usize).copied().unwrap_or(true)
                && !self.elide_held.get(a as usize).copied().unwrap_or(true)
                && !self.elide_excluded.get(a as usize).copied().unwrap_or(true);
            if self.use_count.get(a as usize).copied() != Some(1) || (named && !unread) {
                census[2] += 1;
                continue;
            }
            let Some(cancel) = self.and_zero_moot_carries(a, x, cond) else {
                census[3] += 1;
                continue;
            };
            // JIT rx27 wave 8c: a register the residency plan gave the `And`
            // is no obstacle: the fold never computes it, nothing reads it
            // (one use, the folded compare), and a frame state that names it
            // meets `fused_test_and` in `frame_value_for` before any register
            // lookup. A split-managed value is still refused (its transitions
            // read the home word).
            if flag(&self.ls_managed, a) || flag(&self.ls_managed, x) {
                census[3] += 1;
                continue;
            }
            let (x_color, x_gp) = (color(x), self.assigned_gpr(x));
            let clobbered = pos.is_some_and(|pos| {
                block.nodes[pos + 1..].iter().any(|&later| {
                    let c = color(later);
                    let g = self.assigned_gpr(later);
                    (c.is_some() && c == x_color) || (g.is_some() && g == x_gp)
                })
            });
            if clobbered {
                census[4] += 1;
                continue;
            }
            plan.push((
                cond,
                a,
                x,
                imm,
                an.ty == IrType::Long,
                named,
                promote_cmp,
                cond_named,
                cancel,
            ));
        }
        for (cond, a, x, imm, wide, named, promote_cmp, cond_named, cancel) in plan.iter().cloned()
        {
            if promote_cmp {
                if let Some(cell) = self.fused_cmp.get_mut(cond as usize) {
                    *cell = true;
                }
                if cond_named {
                    if let Some(cell) = self.fused_cmp_unnamed.get_mut(cond as usize) {
                        if !*cell {
                            *cell = true;
                            self.fused_cmp_unnamed_count += 1;
                        }
                    }
                }
            }
            if let Some(cell) = self.fused_test_and.get_mut(a as usize) {
                *cell = true;
            }
            if let Some(cell) = self.test_fused_cond.get_mut(cond as usize) {
                *cell = Some((x, imm, wide));
            }
            self.cancel_and_zero_carries(cancel);
            if named {
                self.and_zero_widened += 1;
            }
        }
        census[1] = plan.len();
        if census[0] > 0 && crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] ir-and-zero-test {}: and-zero branches={} fused={} refused: \
                 used-or-named={} carried-or-resident={} clobbered={} shape={}",
                self.graph.scope_method_key,
                census[0],
                census[1],
                census[2],
                census[3],
                census[4],
                census[5],
            );
        }
    }

    /// JIT rx27 wave 6b: may an and-zero fold of `a = And(x, _)` into the
    /// compare `cond` take `a` and `x` although a carry is planned for them?
    /// Yes when the carry is only the hand-off the fold removes: `a` carried
    /// into `cond` (which the fold answers without reading `a`), or `x`
    /// carried into `a` (which the fold does not emit; `x` then writes its
    /// home and the `TEST` reads it there). Returns the producers whose carry
    /// the fold must cancel, or `None` when another carry is in the way.
    pub(in crate::ir_lower) fn and_zero_moot_carries(&self, a: NodeId, x: NodeId, cond: NodeId) -> Option<Vec<NodeId>> {
        let deferred = |id: NodeId| self.carry_deferred.get(id as usize).copied().unwrap_or(false);
        let carry = |id: NodeId| self.carry_of.get(id as usize).copied().flatten();
        let mut cancel = Vec::new();
        for (producer, consumer) in [(a, cond), (x, a)] {
            match carry(producer) {
                None if !deferred(producer) => {}
                Some((_, c)) if c == consumer && !deferred(producer) => cancel.push(producer),
                _ => return None,
            }
        }
        // A tail carry is planned later (`plan_tail_carries`, which skips a
        // folded `And` as producer and consumer); an RCX carry INTO `a` from
        // its other operand cannot exist (that operand is the constant mask).
        Some(cancel)
    }

    /// Cancel the carry hand-offs an and-zero fold removes, and restore the
    /// producer homes those carries alone made unwritten.
    ///
    /// `plan_carries` marks a carry's home dropped before either and-zero
    /// planner runs.  Merely clearing `carry_of` therefore leaves a producer
    /// with neither a carry nor a home.  The nested-mask shape
    /// `((v >>> k) & m1) & m2` exposed that disagreement: the outer fold
    /// cancelled the inner `And`'s carry, its lowering published no resident
    /// register, and `emit_and_zero_test` then refused the unwritten home.
    /// Keeping the home is conservative even when another optimization could
    /// have dropped it; the later site-named-home planner may still do so with
    /// its own register proof.
    pub(in crate::ir_lower) fn cancel_and_zero_carries(&mut self, cancel: Vec<NodeId>) {
        for p in cancel {
            if let Some(cell) = self.carry_of.get_mut(p as usize) {
                *cell = None;
            }
            if let Some(cell) = self.home_dropped.get_mut(p as usize) {
                *cell = false;
            }
        }
    }

    /// The `If`'s flags for a [`Self::plan_and_zero_tests`] branch: ZF set
    /// exactly when `x & mask` is zero. `x` in its register, in its home word,
    /// or (carried, or no longer readable in place) through RAX.
    pub(in crate::ir_lower) fn emit_and_zero_test(&mut self, x: NodeId, imm: i32, wide: bool) -> XLoc {
        if !self.carry_names(x) {
            if let Some(r) = self.resident_gpr(x) {
                let (bytes, len) = test_reg_imm_bytes(r, imm, wide);
                self.buf.emit(&bytes[..len]);
                return XLoc::Reg(r);
            }
            if let Ok(off) = self.slot_of_checked(x) {
                let (bytes, len) = test_frame_imm_bytes(off, imm, wide);
                self.buf.emit(&bytes[..len]);
                return XLoc::Frame(off);
            }
        }
        self.gp_load_value(RAX, x);
        let (bytes, len) = test_reg_imm_bytes(RAX, imm, wide);
        self.buf.emit(&bytes[..len]);
        XLoc::Rax
    }

    /// JIT rx27 wave 6 (lane regalloc, [`ir_and_zero_test_guard_enabled`]):
    /// the `Guard` twin of [`Self::plan_and_zero_tests`]. A guard whose fused
    /// condition is `Cmp(Eq | Ne, And(x, mask), 0)` deopts on `TEST x, mask`;
    /// the `And` emits nothing inline.
    ///
    /// # The pruned branch's frame
    ///
    /// The builder turns a never-taken `ifeq` / `ifne` into such a guard
    /// (`prune_never_taken_branch`), and the guard's frame state is the one the
    /// interpreter re-executes the branch from: the masked value is on its
    /// operand stack, so a snapshot names the `And`. The `If` rule refuses that
    /// (`used-or-named`). Here it is taken when:
    ///
    /// * every safepoint snapshot naming the `And` is at the guard's own bci,
    ///   and no other node at that bci can deopt -- so the guard's pad is the
    ///   only exit that reads the `And`'s home;
    /// * no loop-carried or promoted recipe names it, and no forced (soak) exit
    ///   reuses the guard's point;
    /// * the `And` has a planned home word (the frame state describes it as
    ///   that word, `frame_value_for`).
    ///
    /// The pad then rebuilds the word before the stub reads the frame
    /// ([`PadRemat`]): `x` is where the `TEST` read it (a register, its home,
    /// or RAX) because the pad is entered straight from the guard's `Jcc`, so
    /// `mov rax, x; and rax/eax, mask; mov [home], rax` stores exactly the
    /// value the `And` would have stored. It clobbers RAX and the flags, which
    /// no frame state names (`deopt_nameable` names only callee-saved
    /// registers). An `And` no snapshot names at all needs no remat and is
    /// planned as the `If` rule plans it.
    ///
    /// A mismatch at emission (the frame state cannot describe the word) is a
    /// refused attempt, lowered again with every home kept (`image_dropped`,
    /// and the plan runs only while unnamed homes may be elided, which the
    /// retry turns off).
    /// An elided guard still participates: its saved frame may keep an And
    /// graph-live although no exit reads it. The same audits retain any value
    /// another exit needs. Without a guard pad, no rematerialization executes.
    pub(in crate::ir_lower) fn plan_and_zero_guard_tests(&mut self) {
        let n = self.graph.nodes.len();
        self.test_and_remat = vec![false; n];
        self.test_guard_cond = vec![None; n];
        self.deopt_pad_remats.clear();
        self.extra_pad_remats.clear();
        self.and_zero_guard_planned = 0;
        if !ir_and_zero_test_guard_enabled()
            || !self.fused_branch_on
            || self.mir_mode != MirMode::Off
            || !self.elide_unnamed_homes_on
        {
            return;
        }
        // seen, plain, remat, then refused: single-use / carried / resident,
        // clobbered, named elsewhere, another shape.
        let mut census = [0usize; 7];
        let flag = |v: &[bool], id: NodeId| v.get(id as usize).copied().unwrap_or(false);
        let color = |id: NodeId| self.slot_plan.node_color.get(id as usize).copied().flatten();
        let recipes: std::collections::HashSet<NodeId> = loop_carried_recipe_values(self.sr_map)
            .into_iter()
            .chain(promoted_pending_deopt_named(self.graph, self.schedule))
            .collect();
        // (cond, and, x, mask, wide, remat, carries to cancel)
        let mut plan: Vec<(NodeId, NodeId, NodeId, i32, bool, bool, Vec<NodeId>)> = Vec::new();
        for block in &self.schedule.blocks {
            for (gpos, &g) in block.nodes.iter().enumerate() {
                let Some(gn) = self.graph.nodes.get(g as usize) else {
                    continue;
                };
                let Op::Guard { bci } = gn.op else {
                    continue;
                };
                let Some(&cond) = gn.inputs.get(1) else {
                    continue;
                };
                if !flag(&self.fused_cmp, cond)
                    || self.implicit_throw_plans.contains_key(&g)
                {
                    continue;
                }
                let Some(cn) = self.graph.nodes.get(cond as usize) else {
                    continue;
                };
                if !matches!(cn.op, Op::Cmp(CmpOp::Eq) | Op::Cmp(CmpOp::Ne)) || cn.inputs.len() != 2
                {
                    continue;
                }
                let is_zero = |id: NodeId| {
                    matches!(self.graph.nodes.get(id as usize).map(|n| &n.op), Some(Op::Const(0)))
                };
                let a = if is_zero(cn.inputs[1]) {
                    cn.inputs[0]
                } else if is_zero(cn.inputs[0]) {
                    cn.inputs[1]
                } else {
                    continue;
                };
                let Some(an) = self.graph.nodes.get(a as usize) else {
                    continue;
                };
                if an.op != Op::And
                    || !matches!(an.ty, IrType::Int | IrType::Long)
                    || an.inputs.len() != 2
                {
                    continue;
                }
                census[0] += 1;
                let (x, imm) =
                    match (self.alu_imm32(an.inputs[1]), self.alu_imm32(an.inputs[0])) {
                        (Some(imm), _) => (an.inputs[0], imm),
                        (None, Some(imm)) => (an.inputs[1], imm),
                        (None, None) => {
                            census[6] += 1;
                            continue;
                        }
                    };
                let x_ok = x != NO_NODE
                    && !self.value_flow.is_absorbed(x)
                    && self
                        .graph
                        .nodes
                        .get(x as usize)
                        .is_some_and(|xn| xn.ty == an.ty);
                let Some(apos) = block.nodes[..gpos].iter().position(|&id| id == a) else {
                    census[6] += 1;
                    continue;
                };
                if !x_ok {
                    census[6] += 1;
                    continue;
                }
                let Some(cancel) = self.and_zero_moot_carries(a, x, cond) else {
                    census[3] += 1;
                    continue;
                };
                if self.use_count.get(a as usize).copied() != Some(1)
                    || flag(&self.ls_managed, a)
                    || flag(&self.ls_managed, x)
                    || flag(&self.fused_test_and, a)
                {
                    census[3] += 1;
                    continue;
                }
                let (x_color, x_gp) = (color(x), self.assigned_gpr(x));
                let clobbered = block.nodes[apos + 1..gpos].iter().any(|&later| {
                    let c = color(later);
                    let r = self.assigned_gpr(later);
                    (c.is_some() && c == x_color) || (r.is_some() && r == x_gp)
                });
                if clobbered {
                    census[4] += 1;
                    continue;
                }
                let named = flag(&self.deopt_named, a);
                if named {
                    // Only this guard's own frame may name it.
                    let elsewhere = self.forced_chain_deopts
                        || recipes.contains(&a)
                        || self.graph.safepoints.iter().any(|sp| {
                            sp.bci != bci
                                && sp
                                    .locals
                                    .iter()
                                    .chain(sp.stack.iter())
                                    .chain(sp.monitors.iter())
                                    .copied()
                                    .chain(sp.sb_virtual_nodes())
                                    .any(|v| v == a)
                        })
                        || self.graph.nodes.iter().enumerate().any(|(id, node)| {
                            // Cast: an index into the node arena is a `NodeId`.
                            id as NodeId != g
                                && node.bytecode_pc == Some(bci)
                                && !self.node_cannot_deopt_here(node)
                        })
                        || flag(&self.home_dropped, a)
                        // The pad rebuilds the home word; a frame state would
                        // name a register the fold never fills.
                        || self.assigned_gpr(a).is_some()
                        || self.assigned_xmm(a).is_some()
                        || self.planned_slot_off(a).is_err();
                    if elsewhere {
                        census[5] += 1;
                        continue;
                    }
                }
                plan.push((cond, a, x, imm, an.ty == IrType::Long, named, cancel));
            }
        }
        for (cond, a, x, imm, wide, remat, cancel) in plan.iter().cloned() {
            self.cancel_and_zero_carries(cancel);
            if remat {
                if let Some(cell) = self.test_and_remat.get_mut(a as usize) {
                    *cell = true;
                }
                census[2] += 1;
            } else {
                if let Some(cell) = self.fused_test_and.get_mut(a as usize) {
                    *cell = true;
                }
                census[1] += 1;
            }
            if let Some(cell) = self.test_guard_cond.get_mut(cond as usize) {
                *cell = Some((x, imm, wide));
            }
        }
        self.and_zero_guard_planned = plan.len();
        self.plan_dead_add_pad_remats(&recipes);
        if census[0] > 0 && crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] ir-and-zero-test-guard {}: and-zero guards={} fused={} \
                 fused-with-pad-remat={} refused: used-carried-or-resident={} clobbered={} \
                 named-elsewhere={} shape={}",
                self.graph.scope_method_key,
                census[0],
                census[1],
                census[2],
                census[3],
                census[4],
                census[5],
                census[6],
            );
        }
    }

    /// JIT round 25 item 1a (`CRATONVM_JIT_IR_ELIDE_UNNAMED_HOMES`, default ON since round 26):
    /// plan the TAIL carries -- definitions whose home store is not emitted
    /// because the one reader of that word is the very next thing lowered, and
    /// it can take the value in RAX instead.
    ///
    /// # What this removes, and why the ordinary carry did not
    ///
    /// `mov [rbp-170h],rax; mov rax,[rbp-170h]; test eax,eax` (r24's
    /// `regex-groups` anatomy): a `boolean` call result stored to its home and
    /// reloaded by the `If` right behind it. The carry above takes only
    /// producers whose arm is ONE `store_rax` (`op_home_is_one_store_rax`) and
    /// only ALU consumers (`op_reads_rax_then_rcx`), and `lower_terminator`
    /// refuses any carry still live at a terminator. So a call or field-load
    /// result, and any value an `If` or a `Return` reads, kept the pair.
    ///
    /// # Why it is sound for a producer with several routes
    ///
    /// The plan does not trust the producer's arm; the emission checks it:
    ///
    /// * `store_rax` elides the home write only for the definition's OWN home
    ///   and only ONCE -- a second home write of the same value refuses;
    /// * the consumer must read it into RAX at the very position the elided
    ///   store would have ended (`gp_load_value`): RAX is the value there on
    ///   the fall-through path, because the store it replaces stored RAX;
    /// * no instruction of the finished body may branch to that position
    ///   ([`Self::verify_tail_carry_points`], the scan
    ///   `apply_home_reload_forwards` already trusts), so no other route can
    ///   arrive with a different RAX;
    /// * every other reader of the word refuses: the home is `home_dropped`,
    ///   so `slot_of` refuses an emitted read and `frame_value_for` refuses a
    ///   deopt point naming it at a bci something transfers from.
    ///
    /// # Who else reads a home, and why none of them can here
    ///
    /// Only `Int` / `Long` values: no GC map, no shadow publication, no
    /// reference root names one. Not resident, not split-managed, not a phi or
    /// a coalesced phi source: no residency transition or edge copy reloads
    /// it. Exactly one use, by the consumer. And no REACHABLE deopt names it
    /// (`deopt_named_reachable`, which counts every bci that can trap or reach
    /// a GC point, every loop header -- the OSR entry seeds homes there -- and
    /// the loop-carried and promoted-pending recipes): a by-bci point, a
    /// handler entry, an OSR exit and a debugger's local read at a call or
    /// poll all resolve through those frame states, and a point the prediction
    /// missed meets `home_dropped` in `frame_value_for` and refuses.
    ///
    /// # Cost of a misprediction
    ///
    /// Every refusal is of THIS ATTEMPT: planning a tail carry reports a
    /// dropped home (`image_dropped`), and the lowering ladder lowers once
    /// more with every home kept, the fallback the constant-home drop uses.
    pub(in crate::ir_lower) fn plan_tail_carries(&mut self) {
        let n = self.graph.nodes.len();
        self.flag_store_elided = vec![false; n];
        self.tail_carry_of = vec![None; n];
        self.tail_carry_seen = vec![false; n];
        self.tail_carries_planned = 0;
        if !self.elide_unnamed_homes_on || self.mir_mode != MirMode::Off {
            return;
        }
        let mut plan: Vec<(NodeId, NodeId)> = Vec::new();
        for (b, block) in self.schedule.blocks.iter().enumerate() {
            // A data consumer: the next scheduled node, reading the value as
            // its FIRST operand -- the arms the ordinary RAX carry already
            // relies on to read operand 0 into RAX before anything else.
            for w in 1..block.nodes.len() {
                let prod = block.nodes[w - 1];
                let cons = block.nodes[w];
                let Some(cn) = self.graph.nodes.get(cons as usize) else {
                    continue;
                };
                if self.value_inputs(cons).next() != Some(prod)
                    || !op_reads_rax_then_rcx(&cn.op)
                    || !matches!(cn.ty, IrType::Int | IrType::Long)
                    // JIT rx27 wave 4: an `And` its `If`'s `TEST` answers
                    // reads nothing here.
                    || self.fused_test_and.get(cons as usize).copied().unwrap_or(false)
                    || self.fused_i2l.get(cons as usize).copied().unwrap_or(false)
                    || self.test_and_remat.get(cons as usize).copied().unwrap_or(false)
                {
                    continue;
                }
                // A trapping division's deopt names its operands.
                if matches!(cn.op, Op::Div | Op::Rem) && !int_division_cannot_trap(self.graph, cn) {
                    continue;
                }
                // Planned clears are emitted at the consumer's first byte,
                // ahead of its operand read (`lower_block`).
                if self.dead_home_clears.contains_key(&cons) {
                    continue;
                }
                if self.tail_carry_producer_ok(prod) {
                    plan.push((prod, cons));
                }
            }
            // The terminator, reading the block's LAST value. An `If` with no
            // back edge (a back-edge poll is emitted before its operand read)
            // and no constant fold (which reads nothing), or a `Return`.
            let (Some(&prod), Some(term)) = (block.nodes.last(), block.terminator) else {
                continue;
            };
            let Some(tn) = self.graph.nodes.get(term as usize) else {
                continue;
            };
            let reads_first = match tn.op {
                Op::If => {
                    let back = block.successors.iter().any(|&s| s <= b);
                    if back && tn.inputs.get(1) == Some(&prod) && self.tail_carry_producer_ok(prod) {
                        self.tail_backedge_skipped += 1;
                    }
                    tn.inputs.get(1) == Some(&prod)
                        && !back
                        && self.constant_if_live_block(term, prod, b).is_none()
                }
                Op::Return => tn.inputs.get(1) == Some(&prod),
                _ => false,
            };
            if reads_first
                && !self.dead_home_clears.contains_key(&term)
                && self.tail_carry_producer_ok(prod)
            {
                plan.push((prod, term));
            }
        }
        if plan.is_empty() {
            return;
        }
        if self.home_dropped.len() < n {
            self.home_dropped.resize(n, false);
        }
        // The home is marked dropped when `store_rax` actually elides the
        // store, not here (round 25 review fix): the inline call prefixes
        // decline a call whose result home is already dropped, so marking it
        // at plan time silently traded every `String` / `ArrayList` / `Arrays`
        // intrinsic prefix on a call feeding an `If` / `Return` for its full
        // dispatch. Nothing reads the home before the definition's own store.
        for (prod, cons) in plan {
            let i = prod as usize;
            if self.tail_carry_of.get(i).copied().flatten().is_some() {
                continue;
            }
            if let Some(cell) = self.tail_carry_of.get_mut(i) {
                *cell = Some(cons);
                self.tail_carries_planned += 1;
            }
        }
    }

    /// May `prod` be a tail carry's producer? Everything here is a fact about
    /// the PLAN (see [`Self::plan_tail_carries`]); what the emission did is
    /// checked where it happens.
    pub(in crate::ir_lower) fn tail_carry_producer_ok(&self, prod: NodeId) -> bool {
        let i = prod as usize;
        let Some(pn) = self.graph.nodes.get(i) else {
            return false;
        };
        if !matches!(pn.ty, IrType::Int | IrType::Long) {
            return false;
        }
        // Arms that end by writing the result home through `store_rax` with
        // RAX holding it. Not `Cmp` / `LCmp` / `FCmp` / `ExactClassIs` (a
        // fused one emits nothing), not `Const` / `Param` / `Phi` (their home
        // is written by another route, or by edge copies), not `CallThrew` /
        // `CatchEntryFlag` (an `If` over one reads nothing), not `LoadStatic`
        // (its post-static-helper exit site would name the value in a register
        // no clause describes, and would be dropped). A wrong entry is a
        // refused attempt, not a wrong answer: an arm that never reaches
        // `store_rax` trips "published no register", and one that reaches it
        // twice trips the second-write refusal.
        // JIT round 26: `LoadStatic` too, under its own switch
        // (`ir_elide_static_tail_enabled`): its post-static-helper exit site
        // names the carried result as RAX like a call's site does.
        let shaped = op_home_is_one_store_rax(&pn.op)
            || matches!(pn.op, Op::Call { .. } | Op::Load(_))
            || (matches!(pn.op, Op::LoadStatic { .. }));
        if !shaped {
            return false;
        }
        self.use_count.get(i).copied() == Some(1)
            && !self.home_dropped.get(i).copied().unwrap_or(false)
            && self.carry_of.get(i).copied().flatten().is_none()
            && !self.carry_deferred.get(i).copied().unwrap_or(false)
            && self.assigned_gpr(prod).is_none()
            && self.assigned_xmm(prod).is_none()
            && !self.ls_managed.get(i).copied().unwrap_or(false)
            && !self.phi_coalesced_src.get(i).copied().unwrap_or(false)
            && !self.fused_cmp.get(i).copied().unwrap_or(false)
            && !self.fused_test_and.get(i).copied().unwrap_or(false)
            && !self.fused_i2l.get(i).copied().unwrap_or(false)
            && !self.test_and_remat.get(i).copied().unwrap_or(false)
            && !self.elide_reach.get(i).copied().unwrap_or(true)
            && !self.elide_held.get(i).copied().unwrap_or(true)
            && !self.elide_excluded.get(i).copied().unwrap_or(true)
    }

    /// JIT round 25 item 1a: the body is finished and every branch patched;
    /// refuse the attempt if any instruction may transfer control to a
    /// position where a tail carry was read. On the fall-through path RAX is
    /// the value there (the elided store stored RAX); on a branch into it,
    /// RAX is whatever that route left. The scan is
    /// [`home_reload_targets`]', conservative in the safe direction: a byte
    /// pattern that merely LOOKS like a branch to the position refuses too.
    /// JIT rx27 wave 9 (`CRATONVM_JIT_IR_PUBLISH_RELOAD_ELIDE`): every read
    /// [`Self::gp_load_value`] skipped sits right behind its value's publish
    /// on the fall-through path; a branch landing on it would arrive with
    /// another RAX. Scan control-transfer fields, as routed definitions do;
    /// ordinary data and frame displacements cannot enter a skipped read.
    /// Block entries clear both mirrors, so switch tables cannot bypass a
    /// publish either. One refusal per value, so the retry
    /// lowers again without that value's skip. Unlike the tail carry, nothing
    /// is exempt: a post-call exit's way back did not run the publish.
    pub(in crate::ir_lower) fn verify_publish_reload_elides(&mut self) {
        if self.publish_reload_elides.is_empty() || self.buf.overflowed() {
            return;
        }
        let len = self.buf.pos();
        let base = self.buf.as_ptr() as u64;
        let wanted: std::collections::HashSet<usize> =
            self.publish_reload_elides.iter().map(|&(pos, _)| pos).collect();
        let exempt = std::collections::HashSet::new();
        let targeted = match self.buf.as_slice().get(..len) {
            Some(code) => branch_targets_except(code, base, &wanted, &exempt),
            None => {
                self.latch_bailout(Bailout::new(BailoutReason::Internal(
                    "ir_lower: the finished body could not be scanned for skipped reads",
                )));
                return;
            }
        };
        let mut hits: Vec<(usize, NodeId)> = self
            .publish_reload_elides
            .iter()
            .copied()
            .filter(|(pos, _)| targeted.contains(pos))
            .collect();
        hits.sort_unstable();
        for (at, id) in hits {
            self.publish_reload_refused += 1;
            self.latch_bailout(Bailout::with_context(
                BailoutReason::UnallocatedValue { node: id },
                format!("n{id}'s skipped read behind its publish: an instruction may branch to code offset {at}"),
            ));
        }
    }

    pub(in crate::ir_lower) fn verify_tail_carry_points(&mut self) {
        if self.tail_carry.is_some() {
            self.refuse_tail_carry("the body ended with it unread");
        }
        if self.tail_carry_points.is_empty() || self.buf.overflowed() {
            return;
        }
        let len = self.buf.pos();
        let base = self.buf.as_ptr() as u64;
        let wanted: std::collections::HashSet<usize> =
            self.tail_carry_points.iter().map(|&(pos, _)| pos).collect();
        // A post-call exit stub's way back lands on its site's end with every
        // GPR preserved by the verdict tail, so RAX is still the value there.
        // JIT round 26: and a proven prefix hit edge carries the result in RAX
        // (`prefix_hit_proves_rax`), provided nothing branches to its JMP.
        let mut exempt: std::collections::HashSet<usize> =
            self.post_call_exit_backs.iter().copied().collect();
        if !self.prefix_rax_proofs.is_empty() {
            let jmps: std::collections::HashSet<usize> =
                self.prefix_rax_proofs.iter().map(|&(j, _, _)| j).collect();
            let none = std::collections::HashSet::new();
            let into_jmps = match self.buf.as_slice().get(..len) {
                Some(code) => home_reload_targets_except(code, base, &jmps, &none),
                None => jmps.clone(),
            };
            let mut bad: Vec<NodeId> = Vec::new();
            for &(j, op, prod) in &self.prefix_rax_proofs {
                if into_jmps.contains(&j) {
                    bad.push(prod);
                } else {
                    exempt.insert(op);
                }
            }
            bad.sort_unstable();
            bad.dedup();
            for prod in bad {
                self.tail_carries_refused += 1;
                self.latch_bailout(Bailout::with_context(
                    BailoutReason::UnallocatedValue { node: prod },
                    format!("n{prod}'s elided home store: a branch reaches its prefix's hit JMP"),
                ));
            }
        }
        let targeted = match self.buf.as_slice().get(..len) {
            Some(code) => home_reload_targets_except(code, base, &wanted, &exempt),
            None => {
                self.latch_bailout(Bailout::new(BailoutReason::Internal(
                    "ir_lower: the finished body could not be scanned for tail-carry branches",
                )));
                return;
            }
        };
        // One refusal per producer whose read a branch may reach, each naming
        // the value (`note_elide_refusal`), so the retry drops exactly those.
        let mut hits: Vec<(usize, NodeId)> = self
            .tail_carry_points
            .iter()
            .copied()
            .filter(|(pos, _)| targeted.contains(pos))
            .collect();
        hits.sort_unstable();
        for (at, prod) in hits {
            self.tail_carries_refused += 1;
            self.latch_bailout(Bailout::with_context(
                BailoutReason::UnallocatedValue { node: prod },
                format!("n{prod}'s elided home store: an instruction may branch to its read at code offset {at}"),
            ));
        }
    }

    /// Did this attempt plan anything [`ir_elide_unnamed_homes_enabled`]
    /// governs? Reported as a dropped home, so a refused attempt is lowered
    /// once more with every home kept.
    pub(in crate::ir_lower) fn elided_unnamed_homes(&self) -> bool {
        self.tail_carries_planned > 0 || self.fused_cmp_unnamed_count > 0 || self.site_named_count > 0
    }

    /// JIT round 25 item 1a, wave 2 (`CRATONVM_JIT_IR_ELIDE_UNNAMED_HOMES`):
    /// drop the write-through home store of a register-resident `int` / `long`
    /// value whose every frame-state naming at a reachable bci is resolved at
    /// a deopt SITE -- a guard's pad, a rethrow pad, an exit stub -- where the
    /// register is named instead ([`Self::site_named_frame_value`]).
    ///
    /// # Which bcis a value may be named at
    ///
    /// A frame state is read by a site's pad (resolved while the site is
    /// emitted, so the register image of that instruction is known) or by
    /// something that inspects the frame with no register file: a parked
    /// thread at a call, a poll or an allocation (stack walks, debugger local
    /// reads), the OSR entry (which seeds HOME words at loop headers), the
    /// loop-carried and promoted-pending recipes. So a value named at any bci
    /// where a node traps through something other than a pad
    /// ([`op_traps_only_through_pads`]), at a loop header, at a back-edge
    /// branch, at the entry, or by either recipe, keeps its home
    /// ([`Self::site_named_blocked`]).
    ///
    /// # Why the register is the value at a site
    ///
    /// The plan gave the value one register for its whole live range, and the
    /// allocation is verified: no value whose range overlaps holds it. The
    /// site check asks the allocator's LIVENESS at the site's position (a
    /// read strictly after it, or live out of its block), not mere range
    /// containment: in SSA a value live at `p` is live on every path from
    /// its definition to `p`, so every definition on such a path overlaps it
    /// and holds another register. The register must be callee-saved and in
    /// the plan's file (not item 1b's caller-saved ones, which keep their
    /// homes), no split-managed value may pass through it, the deopt image
    /// must hold it, and `resident_gpr` must agree at emission. Any site that
    /// fails the check refuses the attempt naming the value, and the retry
    /// excludes it (`ElideRetryScope`).
    pub(in crate::ir_lower) fn plan_site_named_homes(&mut self) {
        let n = self.graph.nodes.len();
        self.site_named = vec![false; n];
        self.site_named_count = 0;
        self.reg_alloc_coalesced_named = 0;
        if !self.elide_unnamed_homes_on || self.mir_mode != MirMode::Off || self.site_live.is_none() {
            return;
        }
        if !(ir_publish_at_def_enabled()
            && ir_deopt_regs_enabled()
            && ir_skip_live_republish_enabled()
            && self.phi_copy_regs_on)
        {
            return;
        }
        // A register a managed value passes through holds different values at
        // different positions: none of its holders is named by it.
        let mut poisoned = [false; 16];
        for id in 0..self.ls_managed.len() {
            if !self.ls_managed[id] {
                continue;
            }
            for reg in self.ls_split_registers(id as NodeId, true) {
                if let Some(cell) = poisoned.get_mut(reg as usize) {
                    *cell = true;
                }
            }
        }
        let (blocked, spill_bcis) =
            self.site_named_blocked(self.spill_at_inspection_on, self.site_named_loop_on, None);
        // W7 fix 5: what a fall-through back-edge poll will name dead (empty
        // with the allocator off).
        let fallthrough_dead = self.reg_alloc_fallthrough_poll_dead();
        self.site_named_phis = 0;
        let mut chosen: Vec<usize> = Vec::new();
        for id in 0..self.gp_reg_of.len() {
            let Some(reg) = self.gp_reg_of[id] else {
                continue;
            };
            if !IR_LOWER_LS_GPRS.contains(&reg)
                || IR_LS_CALLER_SAVED_GPRS.contains(&reg)
                || reg == DEOPT_ARG0
                || poisoned.get(reg as usize).copied().unwrap_or(true)
                || !self.deopt_image_holds_gpr(reg)
            {
                continue;
            }
            // W7-A (`CRATONVM_JIT_IR_REG_ALLOC`): a coalesced back-edge source
            // is admitted (`Self::reg_alloc_coalesced_live_at_edge`).
            let coalesced_refused =
                self.phi_coalesced_src.get(id).copied().unwrap_or(false) && !self.reg_alloc_on;
            if self.home_dropped.get(id).copied().unwrap_or(true)
                || self.ls_managed.get(id).copied().unwrap_or(false)
                || coalesced_refused
                || self.elide_excluded.get(id).copied().unwrap_or(true)
                || blocked.get(id).copied().unwrap_or(true)
                || fallthrough_dead.get(id).copied().unwrap_or(false)
            {
                continue;
            }
            let Some(node) = self.graph.nodes.get(id) else {
                continue;
            };
            // W6-B: a phi is admitted under `site_named_loop_on`. Its home is
            // written by edge copies, each of which honours `home_dropped`
            // (`emit_copy_op`), not by one `store_rax`.
            // `CRATONVM_JIT_IR_DROP_PHI_HOME=0` keeps every phi home, whoever
            // would drop it: honoured here as `phi_home_droppable` honours it.
            let is_phi = matches!(node.op, Op::Phi);
            let phi_admitted = self.site_named_loop_on;
            if (is_phi && !phi_admitted)
                || !matches!(node.ty, IrType::Int | IrType::Long)
                || (!is_phi && !self.reg_alloc_home_is_one_store(node))
            {
                continue;
            }
            if !self
                .site_live
                .as_ref()
                .is_some_and(|s| s.facts.contains_key(&(id as NodeId)))
            {
                continue;
            }
            chosen.push(id);
        }
        // Wave 3: for every inspection bci, the candidates a frame state there
        // names -- the stores `emit_inspection_spills` would owe.
        let mut spill_named: HashMap<usize, Vec<NodeId>> = HashMap::new();
        let mut candidate = vec![false; n];
        for &id in &chosen {
            if let Some(cell) = candidate.get_mut(id) {
                *cell = true;
            }
        }
        if !spill_bcis.is_empty() {
            for sp in &self.graph.safepoints {
                if !spill_bcis.contains(&sp.bci) {
                    continue;
                }
                for v in sp
                    .locals
                    .iter()
                    .chain(sp.stack.iter())
                    .chain(sp.monitors.iter())
                    .copied()
                    .chain(sp.sb_virtual_nodes())
                {
                    if v == NO_NODE || !candidate.get(v as usize).copied().unwrap_or(false) {
                        continue;
                    }
                    let list = spill_named.entry(sp.bci).or_default();
                    if !list.contains(&v) {
                        list.push(v);
                    }
                }
            }
        }
        // ...and not where the spills would cost more than the store they
        // replace: the summed frequency of the blocks that would spill must
        // be BELOW the frequency of the defining block (a value defined
        // before a loop and parked at a call inside it keeps its one store).
        if !spill_named.is_empty() {
            // W6-B: a back-edge poll's spill runs in its slow path only, so
            // its block is no cost.
            let spill_blocks = self.inspection_blocks_by_bci(&spill_bcis, !self.site_named_loop_on);
            let freq = &self.schedule.freq.freq;
            let block_of = |id: usize| -> Option<usize> {
                self.schedule
                    .node_to_block
                    .get(id)
                    .copied()
                    .filter(|&b| b < self.schedule.blocks.len())
            };
            let mut per_value: HashMap<NodeId, Vec<usize>> = HashMap::new();
            for (bci, values) in &spill_named {
                for &v in values {
                    let blocks = per_value.entry(v).or_default();
                    for &b in spill_blocks.get(bci).map(|v| v.as_slice()).unwrap_or(&[]) {
                        if !blocks.contains(&b) {
                            blocks.push(b);
                        }
                    }
                }
            }
            for (v, blocks) in per_value {
                let def = block_of(v as usize).and_then(|b| freq.get(b).copied());
                // W7-A fix: a loop phi's site naming also removes its coalesced
                // back-edge source's definition store (`i + 1`), so a spill at
                // a call of the loop's own frequency replaces two stores.
                let def = def.map(|d| d + self.reg_alloc_coalesced_source_freq(v, &candidate));
                let cost: f64 = blocks.iter().map(|&b| freq.get(b).copied().unwrap_or(f64::MAX)).sum();
                let worth = def.is_some_and(|d| cost < d);
                if !worth {
                    if let Some(cell) = candidate.get_mut(v as usize) {
                        *cell = false;
                    }
                }
            }
            for values in spill_named.values_mut() {
                values.retain(|&v| candidate.get(v as usize).copied().unwrap_or(false));
            }
            spill_named.retain(|_, values| !values.is_empty());
        }
        for &id in &chosen {
            if !candidate.get(id).copied().unwrap_or(false) {
                continue;
            }
            if let Some(cell) = self.home_dropped.get_mut(id) {
                *cell = true;
            }
            if let Some(cell) = self.site_named.get_mut(id) {
                *cell = true;
            }
            self.site_named_count += 1;
            if self
                .graph
                .nodes
                .get(id)
                .is_some_and(|node| matches!(node.op, Op::Phi))
            {
                self.site_named_phis += 1;
            }
            if self.phi_coalesced_src.get(id).copied().unwrap_or(false) {
                self.reg_alloc_coalesced_named += 1;
            }
        }
        self.site_spills.clear();
        self.site_spill_named = spill_named;
        self.site_spill_bcis = spill_bcis;
    }

    /// JIT round 25 wave 6 (lane W6-B): the `ir-home-why` census. Under
    /// `CRATONVM_DBG_JITC` only (else nothing is computed and the vector stays
    /// empty, so `note_home_keep` is a no-op): per node, the first reason the
    /// item-1a planning keeps its home store, in the order the plan asks.
    ///
    /// * `ref` / `fp`: the type (reference homes are what the GC maps name;
    ///   FP lives in the other file);
    /// * `no_reg`: an `int` / `long` the residency plan gave no register (a
    ///   single-use intermediate, a spilled or split value) -- its home is its
    ///   only location;
    /// * `reg_filter`: a register the site naming cannot name (caller-saved,
    ///   split-managed, a coalesced phi source, `DEOPT_ARG0`, not in the
    ///   deopt image);
    /// * `excluded`: refused by an earlier attempt of this compile;
    /// * `entry` / `header` / `inspect` / `not_live` / `recipe`: named by a
    ///   frame state where `site_named_blocked` (with this attempt's switches)
    ///   blocks it;
    /// * `op`: the defining arm is not one `store_rax`;
    /// * `phi`: a phi, without `CRATONVM_JIT_IR_SITE_NAMED_LOOP_VALUES`;
    /// * `unplanned`: no blocking reason, not taken (the switches are off,
    ///   no liveness facts, or the spill-cost rule).
    ///
    /// Reads only; the emission is unchanged by it.
    pub(in crate::ir_lower) fn plan_home_keep_census(&mut self) {
        self.plan_home_keep_census_with(crate::x64::dbg_jitc_enabled());
    }

    /// [`Self::plan_home_keep_census`] with the census switched by `on` (the
    /// debug switch is read once per process, so tests pass it directly).
    pub(in crate::ir_lower) fn plan_home_keep_census_with(&mut self, on: bool) {
        self.home_keep_why.clear();
        self.home_why_static = [0; HK_N];
        self.home_why_weighted = [0.0; HK_N];
        self.phi_edge_home_stores = 0;
        self.phi_edge_home_weighted = 0.0;
        if !on {
            return;
        }
        let n = self.graph.nodes.len();
        let mut why = vec![HK_NONE; n];
        let _ = self.site_named_blocked(
            self.spill_at_inspection_on,
            self.site_named_loop_on,
            Some(&mut why),
        );
        let mut out = vec![HK_OTHER; n];
        for (id, cell) in out.iter_mut().enumerate() {
            let Some(node) = self.graph.nodes.get(id) else {
                continue;
            };
            let is_phi = matches!(node.op, Op::Phi);
            *cell = match node.ty {
                IrType::Ref => HK_REF,
                IrType::Float | IrType::Double => HK_FP,
                IrType::Int | IrType::Long => match self.gp_reg_of.get(id).copied().flatten() {
                    None => HK_NO_REG,
                    Some(_) if self.site_named.get(id).copied().unwrap_or(false) => HK_OTHER,
                    Some(reg) => {
                        if !IR_LOWER_LS_GPRS.contains(&reg)
                            || IR_LS_CALLER_SAVED_GPRS.contains(&reg)
                            || reg == DEOPT_ARG0
                            || !self.deopt_image_holds_gpr(reg)
                            || self.ls_managed.get(id).copied().unwrap_or(false)
                            || self.phi_coalesced_src.get(id).copied().unwrap_or(false)
                        {
                            HK_REG_FILTER
                        } else if self.elide_excluded.get(id).copied().unwrap_or(false) {
                            HK_EXCLUDED
                        } else if why.get(id).copied().unwrap_or(HK_NONE) != HK_NONE {
                            why[id]
                        } else if !is_phi && !self.reg_alloc_home_is_one_store(node) {
                            HK_OP
                        } else if is_phi
                            && !(self.site_named_loop_on)
                        {
                            HK_PHI
                        } else {
                            HK_UNPLANNED
                        }
                    }
                },
                _ => HK_OTHER,
            };
        }
        self.home_keep_why = out;
    }

    /// W6-B census: count one emitted home store of `id`'s, under its reason
    /// and weighted by the frequency of the block being lowered. A no-op
    /// without the census.
    pub(in crate::ir_lower) fn note_home_keep(&mut self, id: NodeId) {
        if self.home_keep_why.is_empty() {
            return;
        }
        let k = usize::from(self.home_keep_why.get(id as usize).copied().unwrap_or(HK_OTHER));
        let k = k.min(HK_N - 1);
        let w = self
            .schedule
            .freq
            .freq
            .get(self.cur_lower_block)
            .copied()
            .filter(|w| w.is_finite())
            .unwrap_or(0.0);
        self.home_why_static[k] += 1;
        self.home_why_weighted[k] += w;
    }

    /// W6-B census: the `ir-home-why` line's reason fields,
    /// `name=static/weighted` for every reason with a store.
    pub(in crate::ir_lower) fn home_why_fields(&self) -> String {
        let mut fields: Vec<String> = Vec::new();
        for (k, name) in HK_NAMES.iter().enumerate() {
            let c = self.home_why_static[k];
            if c == 0 {
                continue;
            }
            fields.push(format!("{name}={c}/{:.0}", self.home_why_weighted[k]));
        }
        fields.join(" ")
    }

    /// Wave 3: for each inspection bci, the blocks of the points that inspect
    /// a frame there -- the nodes `emit_inspection_spills` spills before, and
    /// the back-edge branches. The same predicates `site_named_blocked` uses.
    pub(in crate::ir_lower) fn inspection_blocks_by_bci(
        &self,
        spill_bcis: &std::collections::HashSet<usize>,
        back_edges: bool,
    ) -> HashMap<usize, Vec<usize>> {
        let graph = self.graph;
        let mut out: HashMap<usize, Vec<usize>> = HashMap::new();
        let add = |bci: usize, b: usize, out: &mut HashMap<usize, Vec<usize>>| {
            if !spill_bcis.contains(&bci) {
                return;
            }
            let list = out.entry(bci).or_default();
            if !list.contains(&b) {
                list.push(b);
            }
        };
        for (nid, node) in graph.nodes.iter().enumerate() {
            let Some(pc) = node.bytecode_pc else {
                continue;
            };
            if self.node_cannot_deopt_here(node) || op_traps_only_through_pads(&node.op) {
                continue;
            }
            let Some(b) = self
                .schedule
                .node_to_block
                .get(nid)
                .copied()
                .filter(|&b| b < self.schedule.blocks.len())
            else {
                continue;
            };
            add(pc, b, &mut out);
            add(self.resume_bci(pc), b, &mut out);
        }
        for (b, block) in self.schedule.blocks.iter().enumerate() {
            if !back_edges || !block.successors.iter().any(|&s| s <= b) {
                continue;
            }
            if let Some(pc) = block
                .terminator
                .and_then(|t| graph.nodes.get(t as usize))
                .and_then(|t| t.bytecode_pc)
            {
                add(pc, b, &mut out);
            }
        }
        out
    }

    /// JIT round 25 item 1a, wave 3 (`CRATONVM_JIT_IR_ELIDE_HOMES_SPILL_AT_INSPECTION`):
    /// store-at-inspection instead of store-at-definition. Before node `at`
    /// (or, `term`, before the back-edge poll of terminator `at`) can park
    /// its frame where something reads it without a register file -- a call,
    /// an allocation, a monitor, a safepointing helper, a poll -- write every
    /// site-named value a frame state at its bci names into its home from its
    /// register, unless a store of it already happened on every path here
    /// (one in this block earlier, or in a dominating block: SSA values never
    /// change, and the home word is the value's for its whole colouring range).
    /// The frame states at those bcis keep naming the HOME
    /// ([`Self::site_named_frame_value`]), so every reader -- table points,
    /// stack walks, debugger local reads, post-call and back-edge exits,
    /// reason-9 frames, OSR -- reads it exactly as before. No VM change.
    ///
    /// A value not live at the node (its register may already be another's)
    /// was blocked by the plan; if one gets here anyway the attempt refuses
    /// it and the retry keeps its home.
    ///
    /// This is also the location answer item 2 can reuse for a reference in
    /// a callee-saved register at a GC point: [`Self::site_location_of`].
    pub(in crate::ir_lower) fn emit_inspection_spills(&mut self, at: NodeId, term: bool, block_idx: usize) {
        if !self.spill_at_inspection_on || self.site_spill_named.is_empty() {
            return;
        }
        let graph = self.graph;
        let Some(node) = graph.nodes.get(at as usize) else {
            return;
        };
        let Some(pc) = node.bytecode_pc else {
            return;
        };
        // Only the nodes that made their bci an inspection bci (the same
        // predicate `site_named_blocked` used), and back-edge branches.
        if !term && (self.node_cannot_deopt_here(node) || op_traps_only_through_pads(&node.op)) {
            return;
        }
        let resume = self.resume_bci(pc);
        let mut owed: Vec<NodeId> = Vec::new();
        for bci in [pc, resume] {
            if let Some(list) = self.site_spill_named.get(&bci) {
                for &v in list {
                    if !owed.contains(&v) {
                        owed.push(v);
                    }
                }
            }
        }
        for v in owed {
            if self.spilled_on_every_path(v, block_idx) {
                self.inspection_spills_deduped += 1;
                continue;
            }
            let reg = self.gp_reg_of.get(v as usize).copied().flatten();
            let home = self.home_offset_of(v);
            match (reg, home) {
                (Some(reg), Some(off))
                    if self.resident_gpr(v) == Some(reg) && self.site_live_at(v, at, term) =>
                {
                    if term && self.site_named_loop_on {
                        // W6-B: a back-edge poll's spill goes into its slow
                        // path, the only way a frame is parked here. Not a
                        // store on every path, so not recorded for the dedup.
                        self.poll_slow_spills.push((reg, off));
                    } else {
                        self.store_abi_reg_raw(reg, off);
                        self.site_spills.entry(v).or_default().push(block_idx);
                        self.inspection_spills_emitted += 1;
                    }
                }
                _ => {
                    self.latch_bailout(Bailout::with_context(
                        BailoutReason::UnallocatedValue { node: v },
                        format!(
                            "n{v} is named at inspection bci {pc} but its register is not provably its there"
                        ),
                    ));
                }
            }
        }
    }

}
