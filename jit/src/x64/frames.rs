// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Frame layout, prologue and epilogue.
//!
//! Everything that decides the shape of the stack frame and emits the code
//! that builds and tears it down: the locals/spill/callee-save areas, the
//! stack-bang probes that turn a deep recursion into a fault at a known
//! instruction rather than a wild write, the outgoing stack-argument block,
//! and the prologue/epilogue themselves.
//!
//! The frame layout is a contract, not a private detail: the GC oop maps, the
//! deopt snapshots and the OSR trampoline all address slots by the offsets
//! computed here.

use super::*;

impl Compiler {
    // -----------------------------------------------------------------------
    // Operand-stack simulation
    // -----------------------------------------------------------------------
    //
    // Moved to `x64/operand_stack.rs`: the push/pop/dup/flush bookkeeping and
    // the local load/store helpers that read the same slot model.

    /// Reserve the direct-call argument-service range, ABOVE the argument slots.
    ///
    /// A baked direct call has no dispatch-helper frame, so the cold
    /// exception-table service needs the callee's Java arguments copied into a
    /// contiguous frame range that outlives the CALL. The copy reads
    /// `arg_slots[i]` and writes `base + (len-1-i)`.
    ///
    /// `pop_stack` rewinds `next_spill_offset` past a top-of-stack `Frame`
    /// slot, but it still HANDS THE SLOT BACK and the popped `StackSlot`s stay
    /// live until `emit_stack_arg_setup` marshals them. So a bare
    /// `reserve_spill_slots` here is handed the argument slots themselves, and
    /// that copy becomes a reversing copy into itself: with two int arguments
    /// it stored arg0 over arg1 before the marshalling read it, and the callee
    /// got arg0 in BOTH parameter slots — every comparison in it evaluating
    /// `cmp a, a`. `fixed-suite-bugs/jit-direct-call-arg1-clobbered-by-arg0-FIXED.md`.
    ///
    /// `args_frame_top` is `next_spill_offset` as it stood BEFORE the pops.
    /// The overlap check afterwards is not redundant with the bump: it is what
    /// makes this safe against a future change to either allocator. If the
    /// range would alias an argument the reservation fails, the caller's
    /// `and_then` yields `None`, and the direct call is emitted with no service
    /// range — the cold path loses argument recovery, which is a degradation,
    /// not a miscompile. `reset_spills` recycles the extra slots at the next
    /// bytecode boundary.
    pub(super) fn reserve_direct_call_service_slots(
        &mut self,
        args_frame_top: i32,
        arg_slots: &[StackSlot],
    ) -> Option<i32> {
        if self.next_spill_offset < args_frame_top {
            self.next_spill_offset = args_frame_top;
        }
        let base = self.reserve_spill_slots(arg_slots.len(), SpillReason::CallService)?;
        let end = base.checked_add((arg_slots.len() as i32).checked_mul(8)?)?;
        for slot in arg_slots {
            if let StackSlot::Frame(off) = slot {
                if *off >= base && *off < end {
                    return None;
                }
            }
        }
        Some(base)
    }

    /// Round-8 wave-3 HIGH fix (round-4 #15 / round-5 #9 / round-7 #5):
    /// defensive callee-saved register spill before a safepoint.
    ///
    /// Any Java local assigned to a callee-saved GPR (RBX, R12-R15,
    /// plus RSI/RDI on Windows) holds its live value EXCLUSIVELY in
    /// the register between aload/astore opcodes. The slot-based oop
    /// map and the conservative frame-region sweep both walk stack
    /// memory only — they never read register values. Until per-local
    /// oop typing lands (prerequisite for precise reg-oop encoding),
    /// spill every register-resident local back to its canonical
    /// frame slot `[rbp - (idx+1)*8]` immediately BEFORE the
    /// safepoint-causing CALL. The conservative scanner will then
    /// observe the live oop in the frame slot at GC time.
    ///
    /// The spill is conservative (non-oop locals are also flushed)
    /// but correct: `conservative_roots::scan_one_frame_precise`
    /// filters frame qwords via `heap.is_object_address`, so non-oop
    /// values are ignored. Cost: ~3 bytes (REX + opcode + modrm/disp8)
    /// per used callee-saved local per safepoint.
    ///
    /// TODO(round-12): precise reg-oop encoding to avoid spill cost.
    /// Requires (a) per-local oop typing (currently only operand
    /// stack has `stack_oop_marks`) and (b) `OopMapEntry::reg_oops`
    /// bitmap consumed by the GC scanner walking saved-register slots
    /// in the JIT prologue.
    /// Publish this frame's storage-class partition for the GC
    /// (`CompiledMethod::frame_layout`). Derived from the same values the
    /// prologue and the slot emitters use, so it cannot drift from the code
    /// that is actually generated. All offsets are positive `[rbp - off]`.
    pub(super) fn frame_layout(&self) -> crate::FrameLayout {
        let span = |offs: &[i32]| -> (i32, i32) {
            match (offs.iter().min(), offs.iter().max()) {
                (Some(&lo), Some(&hi)) => (lo, hi + 8),
                _ => (0, 0),
            }
        };
        let (ref_hoist_lo, ref_hoist_hi) = span(&self.hoist_offsets);
        // The arith hoist slots and the shared arith scratch are contiguous and
        // sit directly after the ref-hoist slots; the scratch's depth is not
        // retained, so bound the region by the next region that IS known (the
        // scalar-replacement fields, else the end of the reserved locals).
        let (arith_lo, arith_hi) = if self.arith_hoist_offsets.is_empty() {
            if self.arith_scratch_base > 0 {
                (self.arith_scratch_base, self.arith_scratch_base)
            } else {
                (0, 0)
            }
        } else {
            let (lo, _) = span(&self.arith_hoist_offsets);
            (lo, self.arith_scratch_base.max(lo))
        };
        let mut scalar_lo = 0i32;
        let mut scalar_hi = 0i32;
        for obj in self.scalar_replaced.values() {
            let lo = obj.field_base_offset;
            // Cast: field count is bounded by 16 (see `plan_scalar_replacement`).
            let hi = lo + (obj.num_fields as i32) * (SLOT_SIZE as i32);
            if scalar_hi == 0 || lo < scalar_lo {
                scalar_lo = lo;
            }
            if hi > scalar_hi {
                scalar_hi = hi;
            }
        }
        let reg_spill_slots = if self.reg_spill_base == 0 || !self.safepoint_reg_spill {
            0
        } else if self.safepoint_reg_spill_all {
            ALL_SPILL_GPRS.len() as i32 // Cast: fixed 14-entry table
        } else {
            self.alloc_used_regs.len() as i32 // Cast: register count fits i32
        };
        // Cast: register counts, all far below i32::MAX.
        let callee_saved_hi = self.callee_saved_base + self.alloc_used_regs.len() as i32 * 8;
        let xmm_saved_hi = self.xmm_saved_base + self.alloc_used_xmms.len() as i32 * 8;
        crate::FrameLayout {
            // Cast: local counts are bounded by the classfile format.
            java_locals_hi: (self.num_locals as i32 + 1) * 8,
            ref_hoist_lo,
            ref_hoist_hi,
            arith_lo,
            arith_hi,
            scalar_lo,
            scalar_hi,
            locals_hi: self.base_spill_offset,
            spill_lo: self.base_spill_offset,
            spill_hi: self.spill_limit_offset,
            callee_saved_lo: self.callee_saved_base,
            callee_saved_hi,
            // x86-64 geometry: the save area is the DEEPEST region.
            callee_saved_shallow: false,
            xmm_saved_lo: self.xmm_saved_base,
            xmm_saved_hi,
            reg_spill_lo: self.reg_spill_base,
            reg_spill_hi: self.reg_spill_base + reg_spill_slots * 8,
            frame_size: self.frame_size,
        }
    }

    /// PUSH rbp
    pub(super) fn emit_push_rbp(&mut self) {
        self.buf.emit_byte(0x55);
    }

    /// MOV rbp, rsp
    pub(super) fn emit_mov_rbp_rsp(&mut self) {
        self.rex_w();
        self.buf.emit(&[0x89, 0xE5]);
    }

    /// SUB rsp, imm (uses imm8 when possible)
    pub(super) fn emit_sub_rsp_imm(&mut self, imm: i32) {
        self.rex_w();
        if (0..=127).contains(&imm) {
            self.buf.emit_byte(0x83); // SUB r/m64, imm8
            self.modrm_reg(5, RSP);
            self.buf.emit_byte(imm as u8); // Cast: x86-64 immediate encoding
        } else {
            self.buf.emit_byte(0x81); // SUB r/m64, imm32
            self.modrm_reg(5, RSP);
            self.buf.emit(&imm.to_le_bytes());
        }
    }

    /// ADD rsp, imm (uses imm8 when possible)
    pub(super) fn emit_add_rsp_imm(&mut self, imm: i32) {
        self.rex_w();
        if (0..=127).contains(&imm) {
            self.buf.emit_byte(0x83); // ADD r/m64, imm8
            self.modrm_reg(0, RSP);
            self.buf.emit_byte(imm as u8); // Cast: x86-64 immediate encoding
        } else {
            self.buf.emit_byte(0x81); // ADD r/m64, imm32
            self.modrm_reg(0, RSP);
            self.buf.emit(&imm.to_le_bytes());
        }
    }

    /// MOV EAX, [RSP + disp32].
    ///
    /// Used only for stack banging. EAX is caller-saved and is not an incoming
    /// Java argument on either supported x64 ABI, so clobbering it in the
    /// prologue is safe before parameter shuffling starts.
    pub(super) fn emit_stack_bang_load(&mut self, disp: i32) {
        self.buf.emit(&[0x8B, 0x84, 0x24]);
        self.buf.emit(&disp.to_le_bytes());
    }

    fn emit_stack_bang_before_frame_alloc(&mut self, frame_size: i32) {
        if !jit_stack_bang_enabled() {
            return;
        }
        let Some(disps) = stack_bang_frame_probe_disps(frame_size) else {
            self.fail("singlepass-codegen/stack-bang-probe-unrepresentable");
            return;
        };
        for disp in disps {
            self.emit_stack_bang_load(disp);
        }
    }

    fn emit_stack_bang_headroom(&mut self) {
        if jit_stack_bang_enabled() {
            self.emit_stack_bang_load(-STACK_BANG_PAGE_SIZE);
        }
    }

    /// POP rbp
    pub(super) fn emit_pop_rbp(&mut self) {
        self.buf.emit_byte(0x5D);
    }

    /// RET
    pub(super) fn emit_ret(&mut self) {
        self.buf.emit_byte(0xC3);
    }

    // -----------------------------------------------------------------------
    // Direct-call stack-arg setup (round-8 wave-3 HIGH fix)
    //
    // For direct CALL targets whose JIT entry uses Java-arg-in-ARG_REGS
    // calling convention (with an optional hidden VM ctx in ARG_REGS[0]),
    // we previously bailed when total args exceeded the register file.
    // Now we materialize stack args using the platform ABI:
    //
    //   * Windows x64: caller reserves 32 bytes of shadow space *above*
    //     stack args (the callee owns home slots for its first 4 reg
    //     args). Stack args live at [rsp + 32], [rsp + 40], ...
    //   * SysV (Linux/macOS): no shadow space. Stack args at [rsp],
    //     [rsp + 8], ...
    //
    // Both ABIs require RSP ≡ 0 mod 16 immediately before the CALL.
    // Our frame_size guarantees that on method entry RSP ≡ 0 mod 16
    // (see emit_prologue alignment math). The total bytes subtracted
    // for stack-arg setup must therefore also be 16-byte aligned: we
    // round up by adding an 8-byte alignment pad when needed.
    // -----------------------------------------------------------------------

    /// MOV [rsp + disp32], reg — store 64-bit GPR to RSP-relative slot.
    /// Used to materialize stack-passed args after `sub rsp, N`.
    pub(super) fn emit_mov_rsp_disp_from_reg(&mut self, disp: i32, reg: u8) {
        // Encoding: REX.W [+R] 89 /r SIB
        // [rsp + disp] requires a SIB byte (rm field = 100b means SIB follows);
        // `base_requires_sib(RSP)` is that rule, asserted rather than assumed.
        // SIB: scale=00, index=100b (none), base=100b (rsp).
        //
        // The three-way `disp == 0` / `(-128..=127)` / else split is exactly
        // what `Disp::encode` computes, so it is no longer restated here.
        // RSP is not an RBP/R13-class base: SIB base=100 is a real base, so
        // the displacement-free mod=00 form stays legal at disp == 0.
        debug_assert!(base_requires_sib(RSP));
        let Ok(d) = Disp::encode(disp as i64) else {
            self.buf
                .mark_codegen_unencodable("rsp-displacement-unencodable");
            return;
        };
        self.rex_w_r(reg);
        self.buf.emit_byte(0x89); // MOV r/m64, r64
        self.buf.emit_byte(d.modrm(reg, 0b100)); // r/m=100 → SIB follows
        self.buf.emit_byte(0x24); // SIB: scale=00, index=100 (none), base=100 (rsp)
        let (bytes, len) = d.bytes();
        self.buf.emit(&bytes[..len]);
    }

    /// Compute the total bytes to subtract from RSP for a direct-call
    /// stack-arg block carrying `stack_arg_count` qword args.
    ///
    /// Includes Win64 shadow space and 16-byte alignment pad. Returns
    /// `(total_sub, stack_arg_disp_base)` where `stack_arg_disp_base`
    /// is the RSP-relative displacement where arg[reg_count] lives
    /// (subsequent args ascend by 8).
    fn stack_arg_block_size(stack_arg_count: usize) -> (i32, i32) {
        #[cfg(target_os = "windows")]
        let (shadow, base) = (32i32, 32i32);
        #[cfg(not(target_os = "windows"))]
        let (shadow, base) = (0i32, 0i32);
        let raw = shadow + (stack_arg_count as i32) * 8; // Cast: x86-64 immediate encoding
                                                         // Round up to 16 bytes to preserve RSP alignment at the CALL.
        let total = (raw + 15) & !15;
        (total, base)
    }

    /// Set up stack args for a direct JIT call.
    ///
    /// `arg_slots` are the source frame slots for the args (already
    /// reversed, so `arg_slots[0]` is the first Java arg). `has_ctx`
    /// indicates whether the callee expects the VM ctx pointer as a
    /// hidden first ARG_REGS[0]; the Java args then go into
    /// ARG_REGS[1..]. Returns the total bytes subtracted from RSP
    /// (caller must pass this to `emit_stack_arg_cleanup` after CALL).
    ///
    /// Behaviour when all args fit in registers: emits no SUB RSP and
    /// returns 0 (so callers in the small-arg fast path observe no
    /// behavioural change).
    pub(super) fn emit_stack_arg_setup(&mut self, arg_slots: &[StackSlot], has_ctx: bool) -> i32 {
        let ctx_offset = if has_ctx { 1 } else { 0 };
        let total_regs_for_java = ARG_REGS.len() - ctx_offset;
        let n = arg_slots.len();

        // Reserve stack space first so that subsequent register loads
        // from frame slots (via [rbp - off]) are not invalidated — RBP
        // is unchanged by SUB RSP.
        let stack_arg_count = n.saturating_sub(total_regs_for_java);
        let (total_sub, base_disp) = Self::stack_arg_block_size(stack_arg_count);
        if total_sub > 0 {
            self.emit_sub_rsp_imm(total_sub);
        }

        // Materialize stack args first (these may use RAX as a
        // scratch, which we restore for the reg-arg pass below).
        // Iterate forward — order doesn't matter since each store
        // targets a distinct RSP slot.
        if stack_arg_count > 0 {
            for k in 0..stack_arg_count {
                let java_idx = total_regs_for_java + k;
                let disp = base_disp + (k as i32) * 8; // Cast: x86-64 immediate encoding
                self.load_slot_to_reg(RAX, arg_slots[java_idx]);
                self.emit_mov_rsp_disp_from_reg(disp, RAX);
            }
        }

        // Now load reg-passed args. Do the ctx load LAST so it
        // overwrites RCX/RDI cleanly even if a Java arg happened to
        // be sourced from that register before frame promotion.
        let reg_arg_count = n.min(total_regs_for_java);
        for i in 0..reg_arg_count {
            self.load_slot_to_reg(ARG_REGS[i + ctx_offset], arg_slots[i]);
        }
        if has_ctx {
            self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
        }

        total_sub
    }

    /// Tear down the stack-arg block emitted by `emit_stack_arg_setup`.
    /// Must be called immediately after the CALL returns.
    pub(super) fn emit_stack_arg_cleanup(&mut self, total_sub: i32) {
        if total_sub > 0 {
            self.emit_add_rsp_imm(total_sub);
        }
    }

    pub(super) fn emit_post_call_rbp_republish(&mut self) {
        if !self.precise_maps || self.helpers.frame_record == 0 {
            return;
        }
        if self.inline_rbp_tls_disp != 0 {
            // A compiled callee publishes its own RBP on entry. Restore this
            // caller's RBP into the same mirror after return, using the exact
            // inline mechanism as the prologue. This is value-equivalent to
            // `jit_frame_record(rbp)` and preserves RAX without a save/restore.
            self.emit_mov_tls_disp32_rbp(self.inline_rbp_tls_disp as u32);
            // The callee published ITS identity too, so restoring only the RBP
            // would leave the pair naming two different frames. Both halves
            // move together or neither does.
            if self.inline_cm_tls_disp != 0 {
                self.emit_mov_tls_disp32_imm32(self.inline_cm_tls_disp as u32, self.compile_id);
            }
            return;
        }
        self.buf.emit_byte(0x50); // PUSH RAX (callee return value)
        let (abi_reserve, _) = Self::stack_arg_block_size(0);
        let reserve = abi_reserve + 8; // restore 16-byte call-site alignment
        self.emit_sub_rsp_imm(reserve);
        self.emit_mov_reg_reg(ARG_REGS[0], RBP);
        self.emit_call_absolute(self.helpers.frame_record);
        self.emit_add_rsp_imm(reserve);
        self.buf.emit_byte(0x58); // POP RAX
    }

    pub(super) fn emit_prologue(&mut self) {
        let fs = self.frame_size;
        self.emit_push_rbp();
        self.emit_mov_rbp_rsp();
        self.emit_stack_bang_before_frame_alloc(fs);
        if self.failed {
            return;
        }
        self.emit_sub_rsp_imm(fs);
        self.emit_stack_bang_headroom();

        // Save callee-saved GPR registers using MOV into frame slots (not PUSH).
        // Only save registers actually assigned by the allocator.
        let used_regs = self.alloc_used_regs.clone();
        for (i, &reg) in used_regs.iter().enumerate() {
            let offset = self.callee_saved_base + i as i32 * 8; // Cast: x86-64 immediate encoding
            self.emit_store_local(offset, reg);
        }

        // Save callee-saved XMM registers (used for float/double locals).
        // Direct `MOVQ [rbp-offset], XMM` — avoids the RAX round-trip so
        // ABI args that landed in RAX-adjacent regs aren't disturbed and
        // the prologue is 3 bytes smaller per saved XMM.
        let used_xmms = self.alloc_used_xmms.clone();
        for (i, &xmm) in used_xmms.iter().enumerate() {
            let offset = self.xmm_saved_base + i as i32 * 8; // Cast: x86-64 immediate encoding
            self.emit_movq_mem_rbp_from_xmm(offset, xmm);
        }

        // Layout of caller-passed args:
        //   * Register-passed: ARG_REGS[ctx_offset..ctx_offset+reg_arg_count]
        //     (when needs_heap, ARG_REGS[0] carries the hidden VM/heap ptr).
        //   * Stack-passed: at positive offsets above rbp. After
        //     `push rbp; mov rbp, rsp`, the saved rbp lives at [rbp+0] and
        //     the return address at [rbp+8]. On Windows the next 32 bytes
        //     ([rbp+0x10..0x28]) are the caller's shadow space (home slots
        //     for the 4 register args); stack args start at [rbp+0x30].
        //     On SysV there is no shadow space and stack args start at
        //     [rbp+0x10]. The caller's `emit_stack_arg_setup` pushes
        //     args in ascending ABI-index order, so arg `k` (where
        //     `k >= reg_arg_count + ctx_offset`) lives at
        //     `[rbp + stack_arg_base + (k - ctx_offset - reg_arg_count) * 8]`.
        //
        // ROUND-12 fix: previously the prologue only loaded
        // `ARG_REGS.iter().skip(ctx_offset).take(num_params)`, silently
        // dropping any param beyond the register file. The result was
        // garbage in the corresponding local slot (whatever the frame
        // location happened to hold from the prior call), surfacing as a
        // `ClassCastException` when the JIT'd lambda's `aload` consumed
        // the missing reference. The fix below loads register-passed
        // params (clamped to the available register count) and then
        // loads the remaining stack-passed params via `emit_load_caller_arg`.
        let ctx_offset = if self.needs_heap { 1 } else { 0 };
        if self.needs_heap {
            // First ABI arg is the heap pointer — save to frame
            self.emit_store_local(self.heap_local_offset, ARG_REGS[0]);
        }
        let reg_capacity = ARG_REGS.len() - ctx_offset;
        let reg_arg_count = self.num_params.min(reg_capacity);

        // JVM local slots actually occupied by parameters, and the first slot
        // past the parameter region. For category-1-only methods (and the
        // test call sites that pass no slot map) `param_jvm_slots` is empty,
        // so these reduce to the legacy "arg index == slot, span == count"
        // behavior. For methods with long/double parameters they account for
        // category-2 values spanning two JVM slots — the deposit slot below
        // and the zero-init floor must skip the dead high-half slots so a
        // long/double parameter is neither mis-placed nor clobbered.
        let param_slots: Vec<usize> = if self.param_jvm_slots.is_empty() {
            (0..self.num_params).collect()
        } else {
            self.param_jvm_slots.clone()
        };
        let zero_floor = if self.param_slot_span > 0 {
            self.param_slot_span
        } else {
            self.num_params
        };

        // Load register-passed Java params (java idx 0..reg_arg_count) from
        // ARG_REGS[ctx_offset + i] into the destination local slot.
        for i in 0..reg_arg_count {
            let reg = ARG_REGS[ctx_offset + i];
            // Argument `i` is read from JVM slot `param_jvm_slots[i]` (identity
            // when no slot map is supplied).
            let slot = self.param_jvm_slots.get(i).copied().unwrap_or(i);
            if let Some(xmm) = self.xmm_for_local(slot) {
                // Float/double param: arg arrives as i64 bit pattern in GPR; move to XMM
                self.emit_mov_reg_reg(RAX, reg);
                self.emit_movq_xmm_from_rax(xmm);
            } else if let Some(local_reg) = self.reg_for_local(slot) {
                self.emit_mov_reg_reg(local_reg, reg);
            } else {
                let offset = self.local_offset(slot);
                self.emit_store_local(offset, reg);
            }
        }

        // Load stack-passed Java params (java idx reg_arg_count..num_params)
        // from the caller's stack frame at [rbp + positive_disp]. This path
        // is exercised on Windows x64 when needs_heap is true and
        // num_params == 4 (heap consumes ARG_REGS[0], leaving 3 register
        // slots for the 4 Java args), and on either platform if num_params
        // ever exceeds the register file's Java-arg capacity.
        if self.num_params > reg_arg_count {
            // The caller's `emit_stack_arg_setup` materializes stack
            // args at `[rsp + shadow]` (shadow=32 on Windows, 0 on SysV)
            // immediately before the CALL. After the call sequence
            // (CALL pushes 8B return addr; prologue pushes 8B rbp), the
            // first stack arg lives at `[rbp + 16 + shadow]`.
            #[cfg(target_os = "windows")]
            let stack_arg_base: i32 = 16 + 32; // 0x30 — past saved rbp + retaddr + 32B shadow space
            #[cfg(not(target_os = "windows"))]
            let stack_arg_base: i32 = 16; // 0x10 — past saved rbp + retaddr

            for i in reg_arg_count..self.num_params {
                let stack_idx = i - reg_arg_count; // 0-based index among stack args
                let positive_disp = stack_arg_base + (stack_idx as i32) * 8; // Cast: x86-64 immediate encoding
                let slot = self.param_jvm_slots.get(i).copied().unwrap_or(i);
                // Load via RAX scratch so XMM-mapped float/double params
                // can still be moved through the existing GPR→XMM helper.
                self.emit_load_caller_arg(RAX, positive_disp);
                if let Some(xmm) = self.xmm_for_local(slot) {
                    self.emit_movq_xmm_from_rax(xmm);
                } else if let Some(local_reg) = self.reg_for_local(slot) {
                    self.emit_mov_reg_reg(local_reg, RAX);
                } else {
                    let offset = self.local_offset(slot);
                    self.emit_store_local(offset, RAX);
                }
            }
        }

        // Zero-initialize register-mapped GPR locals beyond the parameter
        // region. `zero_floor` skips the whole parameter span (including the
        // dead high-half slots of category-2 params), and `param_slots`
        // guards against zeroing a register a parameter was coalesced into.
        for i in zero_floor..self.num_locals {
            if let Some(reg) = self.reg_for_local(i) {
                let already_param = param_slots
                    .iter()
                    .any(|&j| self.reg_for_local(j) == Some(reg));
                if !already_param {
                    self.emit_xor_reg_self(reg);
                }
            }
        }
        // Zero-initialize XMM-mapped locals beyond the parameter region.
        // Skip if the XMM register is already initialized for a param (shared live range).
        for i in zero_floor..self.num_locals {
            if let Some(xmm) = self.xmm_for_local(i) {
                let already_param = param_slots
                    .iter()
                    .any(|&j| self.xmm_for_local(j) == Some(xmm));
                if !already_param {
                    self.emit_pxor_xmm_self(xmm);
                }
            }
        }
        // Zero-initialize frame-based locals beyond the parameter region
        // (neither GPR nor XMM assigned)
        for i in zero_floor..self.num_locals {
            if self.reg_for_local(i).is_none() && self.xmm_for_local(i).is_none() {
                let offset = self.local_offset(i);
                self.emit_xor_reg_self(RAX);
                self.emit_store_local(offset, RAX);
            }
        }
        // Stage 3 — register this JIT frame's EXACT RBP with the GC so the
        // root walker can address oop-map slots precisely (the Rust-side
        // JitEntryGuard only captures an approximate SP; release builds omit
        // frame pointers so the real RBP can't be recovered by walking). Done
        // once per invocation, after params are saved so the call doesn't lose
        // them. RBP → ABI arg0; the helper records it into the top JIT chain
        // entry. The default inline path stores RBP into the mirror TLS cell;
        // unsupported targets or a failed TLS probe use the helper. Skipped if
        // the helper pointer isn't wired.
        // Frame-record recording is "configured" iff the helper pointer is
        // wired (`build_helpers` sets it whenever precise maps are on). Gate
        // BOTH the inline and CALL forms on that single signal so a context
        // without a wired helper (e.g. the JIT unit tests, `frame_record == 0`)
        // emits neither — keeping those byte-golden even with inline default-on.
        if self.precise_maps && self.helpers.frame_record != 0 {
            if self.inline_rbp_tls_disp != 0 {
                // Step 1 (inline frame-record) — store RBP straight into the
                // mirror TLS slot with one segment-relative `mov`, no CALL. The
                // VM-side mirror accessor reads the SAME slot (single source of
                // truth via `inline_rbp_tls_disp()`), so the GC root walk sees
                // the innermost RBP exactly as with the helper path.
                self.emit_mov_tls_disp32_rbp(self.inline_rbp_tls_disp as u32);
                // …and the identity of the frame that RBP names, so the GC does
                // not have to decode the call that created it — which it cannot
                // do when the caller reached us through `CALL R11`.
                if self.inline_cm_tls_disp != 0 {
                    self.emit_mov_tls_disp32_imm32(self.inline_cm_tls_disp as u32, self.compile_id);
                }
                // Debug self-check: also call the verify helper (wired into
                // `frame_record` by `build_helpers` when the knob is on), which
                // reads the slot back and asserts it equals RBP.
                if self.verify_inline_frame_record {
                    self.emit_mov_reg_reg(ARG_REGS[0], RBP);
                    self.emit_call_absolute(self.helpers.frame_record);
                }
            } else {
                self.emit_mov_reg_reg(ARG_REGS[0], RBP);
                self.emit_call_absolute(self.helpers.frame_record);
            }
        }

        // Shadow-stack precise roots — cache this invocation's `*mut JvmThread`
        // in a frame slot so each safepoint's inline push/reload can reach the
        // shadow `top` without a per-safepoint helper call (which would clobber
        // staged ABI arg registers). Done last in the prologue, after params are
        // saved to their homes, so `get_current_thread` (caller-saved clobbers)
        // can't lose an argument. RAX holds the returned thread pointer.
        // Initialise the cached-thread slot whenever it EXISTS, independently of
        // whether `get_current_thread` is wired. Both consumers -- the safepoint
        // push in `emit_shadow_push_for_safepoint` and the epilogue savetop
        // restore -- gate only on `shadow_enabled && shadow_thread_slot_off != 0`
        // and rely on the slot READING NULL to skip themselves. Folding this
        // zeroing into the `get_current_thread != 0` arm broke that invariant:
        // with the helper unwired the slot kept whatever stack garbage occupied
        // the frame, the push's null test passed, and it stored a live oop
        // through a wild pointer (SIGSEGV at `mov %rax,0(%r11)`). Production
        // always wires the helper, so this was latent there.
        // Establish the safepoint-id sentinel BEFORE anything in this frame can
        // park. Without it the slot holds whatever the previous frame at this
        // stack depth left, and the collector reads that word and matches an
        // oop map on it -- see `SP_ID_UNSET_BC_PC` for why the value is not
        // `0` here and is `0` in the IR backend. RAX is free: parameters are
        // already homed and no ABI argument is staged in the prologue.
        if self.precise_maps
            && self.sp_id_slot_off != 0
            && crate::sp_id_slot_init_enabled()
        {
            // Cast: the sentinel is `u32::MAX - 1`, which round-trips through
            // the sign-extended imm32 store and the collector's `as u32` read.
            self.emit_mov_imm32_sx(RAX, crate::x64::safepoint::SP_ID_UNSET_BC_PC as u32 as i32);
            self.emit_store_local(self.sp_id_slot_off, RAX);
        }
        if self.shadow_enabled && self.shadow_thread_slot_off != 0 {
            self.emit_xor_reg_self(RAX); // RAX = 0
            self.emit_store_local(self.shadow_thread_slot_off, RAX);
        }
        if self.shadow_enabled
            && self.helpers.get_current_thread != 0
            && self.shadow_thread_slot_off != 0
        {
            // Lazy-prologue perf lever (SB-CRASH-04 shadow default-on work):
            // FIRST zero the thread slot unconditionally (one XOR+store) so it
            // reads null if the fetch below is later NOP'd out. Then emit the
            // `get_current_thread` fetch INTO a recorded byte range. After
            // codegen, `maybe_nop_out_shadow_fetch` overwrites that range with a
            // JMP-over when the method never published a register-resident oop
            // (`!shadow_pushed_any`) — so the per-invocation thread-fetch CALL
            // disappears for the vast majority of methods, killing the ~2.8x
            // fib44 / ~7x call-heavy shadow regression. A NOP'd fetch leaves the
            // slot null ⇒ every (absent) push and the epilogue savetop-restore
            // skip safely. `get_current_thread` is a caller-saved clobber but
            // we are still in the prologue (params already homed), so this is a
            // register-safe place to keep the actual fetch.
            self.shadow_fetch_start = self.buf.pos();
            // 2026-09-02: one `mov rax, gs:[disp]` through the `JIT_THREAD`
            // mirror where the VM publishes it; the helper call otherwise.
            let tls_disp = jit_thread_tls_disp();
            if tls_disp != 0 {
                self.emit_mov_rax_tls_disp32(tls_disp as u32);
            } else {
                self.emit_call_absolute(self.helpers.get_current_thread);
            }
            self.emit_store_local(self.shadow_thread_slot_off, RAX);
            // Save the shadow `top` watermark (RAX = thread). The epilogue
            // restores it, unwinding any unbalanced safepoint push this method
            // made. Skip on null thread (slot holds null → epilogue skips too).
            self.emit_test_r64_r64(RAX);
            let skip = self.emit_jcc_rel32_patch(0x84); // JE skip (RAX == 0)
            self.emit_mov_r64_mem_disp32(R11, RAX, self.shadow_off_in_thread);
            self.emit_store_local(self.shadow_savetop_slot_off, R11);
            self.patch_rel32_to_here(skip);
            self.shadow_fetch_end = self.buf.pos();
        }
        // Inline TLAB allocation and the self-recursion guard need two
        // immutable per-OS-thread values: JvmThread* and native-stack floor.
        // A direct self-call already has both in its caller's same-layout frame.
        // Prove self-entry by checking the native return address against this
        // method's PRIVATE executable allocation, then follow saved RBP and copy
        // the slots. External/interpreter entries have a return address outside
        // that allocation and retain the normal TLS helpers below.
        let has_thread_cache =
            self.jit_thread_slot_off != 0 && self.helpers.get_current_thread != 0;
        let has_floor_cache =
            self.stack_floor_slot_off != 0 && self.helpers.native_stack_floor_fn != 0;
        // 2026-09-02: where the `JIT_THREAD` mirror is published, the thread
        // cache is ONE segment-prefixed load on every entry -- cheaper than the
        // inherit proof below (two imm64 compares and a frame chase), so the
        // thread slot leaves the inherit path entirely and only the stack
        // floor still inherits.
        let tls_disp = jit_thread_tls_disp();
        let thread_via_tls = has_thread_cache && tls_disp != 0;
        if thread_via_tls {
            self.emit_mov_rax_tls_disp32(tls_disp as u32);
            self.emit_store_local(self.jit_thread_slot_off, RAX);
        }
        let has_thread_cache = has_thread_cache && !thread_via_tls;
        let can_inherit = self_cache_inherit_enabled() && (has_thread_cache || has_floor_cache);
        let mut external_entry_patches = Vec::new();
        let mut inherited_done = None;
        if can_inherit {
            // [RBP+8] = caller return address after this prologue's PUSH RBP.
            self.emit_load_caller_arg(RAX, 8);
            let code_lo = self.buf.as_ptr() as usize;
            let code_hi = code_lo.saturating_add(self.buf.capacity());
            self.emit_mov_imm64(R10, code_lo as i64);
            self.emit_cmp_r64_r64(RAX, R10);
            external_entry_patches.push(self.emit_jcc_rel32_patch(0x82)); // JB below buffer
            self.emit_mov_imm64(R10, code_hi as i64);
            self.emit_cmp_r64_r64(RAX, R10);
            external_entry_patches.push(self.emit_jcc_rel32_patch(0x83)); // JAE past buffer

            // [RBP] is the caller frame pointer. The return-address proof above
            // guarantees it is a same-method frame with identical slot offsets.
            self.emit_mov_r64_mem_disp32(R10, RBP, 0);
            if has_thread_cache {
                self.emit_mov_r64_mem_disp32(RAX, R10, -self.jit_thread_slot_off);
                self.emit_store_local(self.jit_thread_slot_off, RAX);
            }
            if has_floor_cache {
                self.emit_mov_r64_mem_disp32(RAX, R10, -self.stack_floor_slot_off);
                self.emit_store_local(self.stack_floor_slot_off, RAX);
            }
            inherited_done = Some(self.emit_jmp_rel32_patch());
            for patch in external_entry_patches.drain(..) {
                self.patch_rel32_to_here(patch);
            }
        }

        if has_thread_cache {
            self.emit_call_absolute(self.helpers.get_current_thread);
            self.emit_store_local(self.jit_thread_slot_off, RAX);
        }
        if has_floor_cache {
            self.emit_call_absolute(self.helpers.native_stack_floor_fn);
            self.emit_store_local(self.stack_floor_slot_off, RAX);
        }
        if let Some(done) = inherited_done {
            self.patch_rel32_to_here(done);
        }
        // spring-bug-10 watchpoint: arm a HW data breakpoint on this frame's
        // savebase slot (rbp - savebase_off) by calling the registered helper.
        // Prologue position = no ABI args staged yet, so clobbering ARG_REGS[0]
        // / caller-saved here is safe (locals live in callee-saved regs). The
        // arm-helper one-shots / dedups internally.
        if shadow_watch() && self.shadow_savebase_slot_off != 0 {
            let h = ARM_SAVEBASE_WATCH_FN.load(std::sync::atomic::Ordering::Relaxed);
            if h != 0 {
                self.emit_lea_r64_mem_disp32(ARG_REGS[0], RBP, -self.shadow_savebase_slot_off);
                self.emit_call_absolute(h);
            }
        }
        // Cooperative JIT safepoint poll (CRATONVM_JIT_SAFEPOINT_POLLS) —
        // method entry, context methods only. Emitted last in the prologue
        // so every earlier prologue effect (param homing, frame-record,
        // shadow-stack thread cache) is already committed before this
        // thread could possibly park at the barrier. No-op unless the env
        // flag is set AND the helper table wired the flag address (see
        // `emit_safepoint_poll_prologue` / `emit_safepoint_poll`).
        if !self.gc_inert_selfrec {
            self.emit_safepoint_poll_prologue();
        }
    }

    /// Lazy-prologue perf lever — call AFTER the whole body is compiled. If the
    /// method never published a register-resident oop (`!shadow_pushed_any`),
    /// the prologue's `get_current_thread` fetch sequence is dead weight on
    /// every invocation; overwrite its recorded byte range with a `JMP`-over so
    /// the CALL never runs. The thread slot was zeroed before the fetch, so a
    /// NOP'd fetch leaves it null and every (absent) push + the epilogue
    /// savetop-restore skip via their null guards. Patching writes within the
    /// already-emitted range only — no offsets move, no branch target lands
    /// inside the prologue fetch.
    pub(super) fn maybe_nop_out_shadow_fetch(&mut self) {
        if !self.shadow_enabled
            || self.shadow_pushed_any
            || self.shadow_fetch_end <= self.shadow_fetch_start
        {
            return;
        }
        // Shared with the IR backend's `finish_lazy_thread_fetch`, which erases
        // the same dead fetch for the same reason. It open-coded only the
        // NOP-fill half of this for a while, and a 46-NOP entry path cost
        // `CratonBench fib` ~2x; one helper keeps them from drifting again.
        self.buf
            .erase_range_with_jump_over(self.shadow_fetch_start, self.shadow_fetch_end);
    }

    /// Emit function epilogue: restore callee-saved regs; add rsp; pop rbp; ret
    pub(super) fn emit_epilogue(&mut self) {
        // spring-bug-10 watchpoint: disarm the savebase HW breakpoint, closing
        // this frame's live window so a -2 written to the (now-reused) stack slot
        // after we return is not mistaken for the corruptor. RAX holds the return
        // value here; stash it in the (now-dead) savebase slot across the call.
        if shadow_watch() && self.shadow_savebase_slot_off != 0 {
            let h = DISARM_SAVEBASE_WATCH_FN.load(std::sync::atomic::Ordering::Relaxed);
            if h != 0 {
                self.emit_store_local(self.shadow_savebase_slot_off, RAX);
                self.emit_call_absolute(h);
                self.emit_load_local(RAX, self.shadow_savebase_slot_off);
            }
        }
        // Shadow-stack: restore the `top` watermark saved in the prologue,
        // unwinding any push this method did not pop (e.g. the unbalanced
        // `invokespecial <init>` push). Correct under nesting: each method
        // restores top to its own entry value on return. Null-guarded so an OSR
        // entry (which zero-inits the thread slot) skips this safely. R10/R11
        // are caller-saved scratch (free at return); RAX (return value) untouched.
        //
        // The `helpers.get_current_thread != 0` check MUST mirror the
        // prologue's gate (see `emit_prologue`'s shadow-stack block). The
        // prologue only zero-initializes `shadow_thread_slot_off` when that
        // helper is wired; when it isn't (e.g. the JIT unit tests' stub
        // `test_helpers()`, which leaves `get_current_thread` null), the
        // prologue skips its block ENTIRELY and the slot is never written.
        // Without this matching guard, the epilogue still ran, loaded
        // whatever uninitialized stack garbage happened to occupy that
        // frame slot, treated it as a live `*mut JvmThread` when nonzero,
        // and wrote through it — a wild pointer store that crashed
        // deterministically-but-content-dependently (STATUS_ACCESS_VIOLATION
        // on Windows), reproducing only when prior stack usage happened to
        // leave a nonzero value there. The real VM never hit this because
        // `build_helpers()` always wires `get_current_thread` when precise
        // maps are on.
        if self.shadow_enabled
            && self.helpers.get_current_thread != 0
            && self.shadow_thread_slot_off != 0
        {
            self.emit_load_local(R10, self.shadow_thread_slot_off);
            self.emit_test_r64_r64(R10);
            let skip = self.emit_jcc_rel32_patch(0x84); // JE skip (R10 == 0)
            self.emit_load_local(R11, self.shadow_savetop_slot_off);
            self.emit_mov_mem_disp32_r64(R10, R11, self.shadow_off_in_thread);
            self.patch_rel32_to_here(skip);
        }
        // Restore callee-saved GPR registers from frame slots (matching prologue MOV saves)
        let used_regs = self.alloc_used_regs.clone();
        for (i, &reg) in used_regs.iter().enumerate() {
            let offset = self.callee_saved_base + i as i32 * 8; // Cast: x86-64 immediate encoding
            self.emit_load_local(reg, offset);
        }
        // Restore callee-saved XMM registers.
        // Direct `MOVQ XMMn, [rbp-offset]` — no GPR scratch needed, so
        // RAX (return value) and R11 are both preserved. Each restore
        // shrinks from ~9 bytes (MOV+MOVQ) to ~6 bytes (single MOVQ).
        let used_xmms = self.alloc_used_xmms.clone();
        for (i, &xmm) in used_xmms.iter().enumerate() {
            let offset = self.xmm_saved_base + i as i32 * 8; // Cast: x86-64 immediate encoding
            self.emit_movq_xmm_from_mem_rbp(xmm, offset);
        }
        let fs = self.frame_size;
        self.emit_add_rsp_imm(fs);
        self.emit_pop_rbp();
        self.emit_ret();
    }

    /// T5.2.16 — emit an epilogue suitable for a sibling tail-call: restore
    /// callee-saved regs and tear down our frame, but DO NOT emit the
    /// final RET. The caller follows up with a `JMP <callee_entry>` so
    /// that the callee returns directly to our caller. Arguments for
    /// the callee must already be live in ABI registers at the point
    /// of this call.
    pub(super) fn emit_epilogue_without_ret(&mut self) {
        // Shadow-stack: same watermark restore the real epilogue does. A tail
        // call is a method exit — the callee returns straight to OUR caller, so
        // nothing downstream would ever put `top` back where this activation
        // found it, and a caller that tail-calls from inside a loop would
        // accumulate one leak per iteration. R10/R11 are caller-saved and the
        // callee's arguments live in ARG_REGS, so neither is disturbed.
        if self.shadow_enabled
            && self.helpers.get_current_thread != 0
            && self.shadow_thread_slot_off != 0
        {
            self.emit_load_local(R10, self.shadow_thread_slot_off);
            self.emit_test_r64_r64(R10);
            let skip = self.emit_jcc_rel32_patch(0x84); // JE skip (R10 == 0)
            self.emit_load_local(R11, self.shadow_savetop_slot_off);
            self.emit_mov_mem_disp32_r64(R10, R11, self.shadow_off_in_thread);
            self.patch_rel32_to_here(skip);
        }
        let used_regs = self.alloc_used_regs.clone();
        for (i, &reg) in used_regs.iter().enumerate() {
            let offset = self.callee_saved_base + i as i32 * 8; // Cast: x86-64 immediate encoding
            self.emit_load_local(reg, offset);
        }
        let used_xmms = self.alloc_used_xmms.clone();
        for (i, &xmm) in used_xmms.iter().enumerate() {
            let offset = self.xmm_saved_base + i as i32 * 8; // Cast: x86-64 immediate encoding
                                                             // Direct `MOVQ XMMn, [rbp-offset]` (see emit_epilogue notes).
            self.emit_movq_xmm_from_mem_rbp(xmm, offset);
        }
        let fs = self.frame_size;
        self.emit_add_rsp_imm(fs);
        self.emit_pop_rbp();
    }
}
