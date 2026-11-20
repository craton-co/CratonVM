#[allow(unused_imports)]
use super::*;

impl Compiler {
    pub(in crate::x64) fn emit_sp_byte_array_view_body(
        &mut self,
        op: crate::ir_lower::ByteArrayViewOp,
        arg_slots: &[StackSlot],
    ) -> Option<usize> {
        let mut decline: Vec<usize> = Vec::with_capacity(4);
        // RAX = byte[] array; null declines.
        self.load_slot_to_reg(RAX, arg_slots[0]);
        self.emit_test_r64_r64(RAX);
        decline.push(self.emit_jcc_rel32_patch(0x84)); // JZ decline
        // ECX = offset (zero-extended into RCX); negative declines.
        self.load_slot_to_reg(RCX, arg_slots[1]);
        self.buf.emit(&[0x89, 0xC9]); // MOV ECX, ECX
        self.buf.emit(&[0x85, 0xC9]); // TEST ECX, ECX
        decline.push(self.emit_jcc_rel32_patch(0x88)); // JS decline

        // EDX = array.length; require array.length >= width && offset <= array.length - width.
        const ARRAY_LENGTH_DISP: u8 =
            crate::x64::disp8_const(cratonvm_types::ARRAY_LENGTH_OFFSET as i64) as u8;
        const ARRAY_DATA_DISP: u8 =
            crate::x64::disp8_const(cratonvm_types::HEADER_SIZE as i64) as u8;
        self.buf.emit(&[0x8B, 0x50, ARRAY_LENGTH_DISP]);
        self.buf.emit(&[0x83, 0xEA, op.width]); // SUB EDX, width
        decline.push(self.emit_jcc_rel32_patch(0x82)); // JB decline
        self.buf.emit(&[0x39, 0xD1]); // CMP ECX, EDX
        decline.push(self.emit_jcc_rel32_patch(0x87)); // JA decline
        let hdr = ARRAY_DATA_DISP;
        if op.is_put {
            self.load_slot_to_reg(RDX, arg_slots[2]);
            match op.width {
                2 => {
                    self.buf.emit(&[0x66, 0xC1, 0xC2, 0x08]); // ROL DX, 8
                    self.buf.emit(&[0x66, 0x89, 0x54, 0x08, hdr]); // MOV [RAX+RCX+hdr], DX
                }
                4 => {
                    self.buf.emit(&[0x0F, 0xCA]); // BSWAP EDX
                    self.buf.emit(&[0x89, 0x54, 0x08, hdr]); // MOV [RAX+RCX+hdr], EDX
                }
                _ => {
                    self.buf.emit(&[0x48, 0x0F, 0xCA]); // BSWAP RDX
                    self.buf.emit(&[0x48, 0x89, 0x54, 0x08, hdr]); // MOV [RAX+RCX+hdr], RDX
                }
            }
        } else {
            match op.width {
                2 => {
                    self.buf.emit(&[0x0F, 0xB7, 0x44, 0x08, hdr]); // MOVZX EAX, WORD [RAX+RCX+hdr]
                    self.buf.emit(&[0x66, 0xC1, 0xC0, 0x08]); // ROL AX, 8
                    if op.signed {
                        self.buf.emit(&[0x48, 0x0F, 0xBF, 0xC0]); // MOVSX RAX, AX
                    } else {
                        self.buf.emit(&[0x0F, 0xB7, 0xC0]); // MOVZX EAX, AX
                    }
                }
                4 => {
                    self.buf.emit(&[0x8B, 0x44, 0x08, hdr]); // MOV EAX, [RAX+RCX+hdr]
                    self.buf.emit(&[0x0F, 0xC8]); // BSWAP EAX
                    self.buf.emit(&[0x48, 0x63, 0xC0]); // MOVSXD RAX, EAX
                }
                _ => {
                    self.buf.emit(&[0x48, 0x8B, 0x44, 0x08, hdr]); // MOV RAX, [RAX+RCX+hdr]
                    self.buf.emit(&[0x48, 0x0F, 0xC8]); // BSWAP RAX
                }
            }
        }
        let hit = self.emit_jmp_rel32_patch();
        for p in decline {
            self.patch_rel32_to_here(p);
        }
        self.slot_mirror = None;
        Some(hit)
    }

    pub(in crate::x64) fn try_emit_sp_byte_array_view_dispatch_prefix(
        &mut self,
        _pc: usize,
        info: &crate::JitInvokeInfo,
    ) -> Option<usize> {
        if self.failed || !sp_thin_prefix_enabled() || info.invoke_kind != 3 {
            return None;
        }
        let op = crate::ir_lower::byte_array_view_site_shape_parts(
            info.class_name,
            info.method_name,
            info.descriptor,
        )?;
        let want_args = if op.is_put { 3 } else { 2 };
        if info.num_jit_args != want_args || self.stack.len() < want_args {
            return None;
        }
        let args = &self.stack[self.stack.len() - want_args..];
        if args.iter().any(|s| matches!(s, StackSlot::Xmm(..)))
            || self
                .stack
                .iter()
                .any(|s| matches!(s, StackSlot::Scratch(..)))
        {
            return None;
        }
        let arg_copy: Vec<StackSlot> = args.to_vec();
        self.emit_sp_byte_array_view_body(op, &arg_copy)
    }

    /// Lower one bytecode of this family at `pc`. The result is the pc the walk
    /// continues at, or the value `compile_bytecode` returns.
    #[allow(clippy::too_many_arguments)]
    pub(in crate::x64) fn walk_invoke(
        &mut self,
        code: &[u8],
        code_len: usize,
        op: u8,
        pc: usize,
        dead: &mut bool,
        branch_targets: &[bool],
    ) -> WalkStep {
        // A site the compile door found unresolved resolves at its first
        // execution, whatever lowering follows (interpreter round i1 wave 50,
        // lane L1; `crate::JitFirstCallCell`). Register-transparent, so it
        // goes before every arm below, the splices and direct binds included.
        if let Some(&cell) = self.first_call_cells.get(&pc) {
            if matches!(op, 0xb6..=0xb9) {
                crate::runtime_lowering::emit_first_call_guard(&mut self.buf, cell);
                if false {
                    eprintln!("[first-call] single-pass guard pc={pc} op={op:#04x} cell={cell:#x}");
                }
            }
        }
        match op {
            0xb8 => {
                // A callee dividing by its own parameter splices behind a
                // zero test of that argument; the ordinary lowering below is
                // its miss edge (`try_emit_divisor_guarded_splice`).
                if self.inline_sites.contains_key(&pc) {
                    if let Some(step) = self.try_emit_divisor_guarded_splice(pc, |c| {
                        c.walk_invokestatic(code, code_len, op, pc, dead, branch_targets)
                    }) {
                        return step;
                    }
                }
                self.walk_invokestatic(code, code_len, op, pc, dead, branch_targets)
            }
            0xb6 | 0xb7 | 0xb9 => {
                self.walk_invoke_instance(code, code_len, op, pc, dead, branch_targets)
            }
            0xba => self.walk_invokedynamic(code, code_len, op, pc, dead, branch_targets),
            _ => {
                // `compile_bytecode` routed an opcode here that this family
                // does not lower: a dispatch table bug, refused rather than
                // emitted.
                self.fail("singlepass-codegen/walk-family-misdispatch");
                WalkStep::Return(false)
            }
        }
    }

    /// Does the `invokestatic; invokeinterface; checkcast; invokevirtual` run
    /// at `pc` name the four members `jit_lambda_int_to_double` hard-codes
    /// (`Integer.valueOf`, `Function.apply`, `checkcast Double`,
    /// `Double.doubleValue`), with no branch landing inside it?
    ///
    /// The peephole replaces 14 bytes of bytecode with one helper call that
    /// performs exactly those four operations, so a run that merely has the
    /// same OPCODES would be miscompiled into them. A site whose table row is
    /// absent (the resolver registered it as a direct call or intrinsic) is not
    /// evidence against the shape and is accepted, which keeps every run that
    /// matched before matching; a row that names a different member is.
    pub(in crate::x64::op_invoke) fn tdigest_adapter_members_match(&self, pc: usize, branch_targets: &[bool]) -> bool {
        // A jump to pc+3/8/11 would land on a bci this peephole never lowers.
        if [pc + 3, pc + 8, pc + 11]
            .iter()
            .any(|&p| branch_targets.get(p).copied().unwrap_or(true))
        {
            return false;
        }
        // A call site the compile door found unresolved keeps its call and
        // its first-execution guard (`walk_invoke`), which resolves it -- and
        // records its owner for the class's loader -- when it first runs; the
        // fused run would run all three with no guard (2026-10-07, item 2 of
        // `interpreter-L5-getinitiatedclasses-residual-recording-differences-FIXED-20261007`).
        if [pc, pc + 3, pc + 11]
            .iter()
            .any(|p| self.first_call_cells.contains_key(p))
        {
            return false;
        }
        let invoke_names_ok = |at: usize, class: &str, method: &str| -> bool {
            match self
                .invoke_info_idx
                .get(&at)
                .and_then(|&i| self.invoke_info.get(i))
            {
                Some(&(_, ptr)) if !ptr.is_null() => {
                    // SAFETY: `invoke_info` owns every pointer it hands out for
                    // the life of this compile.
                    let info = unsafe { &*ptr };
                    info.class_name == class && info.method_name == method
                }
                _ => true,
            }
        };
        let cast_ok = match self
            .typecheck_info_idx
            .get(&(pc + 8))
            .and_then(|&i| self.typecheck_info.get(i))
        {
            // SAFETY: `name_ptr`/`name_len` are an `intern_typecheck_target`
            // pair, leaked for the life of the process.
            Some(&(_, name_ptr, name_len)) => {
                // SAFETY: this interned target pair is leaked for the process lifetime.
                match unsafe { crate::typecheck_site_name(name_ptr, name_len) } {
                    Some(name) => name == "java/lang/Double",
                    None => true,
                }
            }
            None => true,
        };
        cast_ok
            && invoke_names_ok(pc, "java/lang/Integer", "valueOf")
            && invoke_names_ok(pc + 3, "java/util/function/Function", "apply")
            && invoke_names_ok(pc + 11, "java/lang/Double", "doubleValue")
    }

    /// Lower `invokestatic`: a self-call, a direct call, an inlined body, an intrinsic or the dispatch helper at `pc`.
    #[allow(clippy::too_many_arguments)]
    pub(in crate::x64::op_invoke) fn walk_invokestatic(
        &mut self,
        code: &[u8],
        code_len: usize,
        op: u8,
        mut pc: usize,
        _dead: &mut bool,
        branch_targets: &[bool],
    ) -> WalkStep {
        match op {
            // invokestatic — self-call, direct call, inline, or dispatch helper
            0xb8 => {
                self.flush_scratch_registers();

                // Round 13 wave 13 (lane syncres, proposal M3-1 (a)):
                // `Thread.holdsLock(this)` in an instance self-locking body is
                // `true` at every pc (`Compiler::self_lock_holds_lock_fold_at`):
                // pop the argument, push the answer, no call.
                if self.self_lock_holds_lock_fold_at(code, code_len, pc, branch_targets) {
                    let _ = self.pop_stack();
                    self.push_imm32(1);
                    pc += 3;
                    return WalkStep::Next(pc);
                }

                // The erased adapter Integer.valueOf(i) -> Function.apply(Object)
                // -> checkcast Double -> Double.doubleValue() (TDigest's
                // quantile/cdf kernels are the shape it was written for),
                // scalarized into one `jit_lambda_int_to_double` call.
                //
                // r11 x64gen: the four opcodes were matched by opcode alone,
                // while `jit_lambda_int_to_double` hard-codes all four
                // MEMBERS; `tdigest_adapter_members_match` refuses a run whose
                // resolved names say otherwise, or into whose middle a branch
                // lands. Interpreter round i1 wave 10: armed by the constant
                // pool (`lambda_int_to_double_proven`, from
                // `crate::lambda_int_to_double_runs_proven`), no longer by the
                // `org/elasticsearch/tdigest/Dist` method label.
                if self.lambda_int_to_double_proven
                    && pc + 14 <= code.len()
                    && code[pc + 3] == 0xb9
                    && code[pc + 8] == 0xc0
                    && code[pc + 11] == 0xb6
                    && self.tdigest_adapter_members_match(pc, branch_targets)
                {
                    if self.stack.len() >= 2 {
                        // Keep the lambda receiver on the simulated stack
                        // through the safepoint so the oop map roots it.
                        let Some(&index_slot) = self.stack.last() else {
                            // The specialized pattern was recognized but
                            // its simulated stack no longer matches. Bail
                            // out of JIT compilation; the interpreter can
                            // execute the ordinary invoke path safely.
                            self.fail("singlepass-codegen/lambda-int-to-double-stack-shape");
                            return WalkStep::Return(false);
                        };
                        let lambda_slot = self.stack[self.stack.len() - 2];
                        self.emit_load_vm_context(ARG_REGS[0]);
                        self.load_slot_to_reg(ARG_REGS[1], lambda_slot);
                        self.load_slot_to_reg(ARG_REGS[2], index_slot);
                        self.emit_pre_safepoint_spill();
                        // ABI-3 — (vm_ptr, proxy_raw, index). `emit_call_absolute` takes a
                        // bare address, so this assertion is the only thing connecting the
                        // three registers written above to the helper's declared arity.
                        cratonvm_jit_api::assert_helper_call_shape!(
                            "lambda_int_to_double",
                            int_args = 3,
                            returns_value = true
                        );
                        self.emit_call_absolute(self.helpers.lambda_int_to_double);
                        self.emit_oop_map_for_safepoint();
                        let _ = self.pop_stack();
                        let _ = self.pop_stack();
                        self.emit_post_invoke_exception_check(b'D');
                        self.push_from_rax_as_xmm0();
                        pc += 14;
                        return WalkStep::Next(pc);
                    }
                }

                // Check for invoke_info (fallback to jit_invoke_dispatch)
                let info_ptr = self
                    .invoke_info_idx
                    .get(&pc)
                    .map(|&i| self.invoke_info[i].1);

                // A GPU kernel call keeps its dispatch helper.
                //
                // The offload hook lives in `jit_invoke_dispatch`, and
                // this arm has TWO doors that never reach it: the
                // inliner (checked first, "most profitable") and this
                // backend's own direct-call map. Splicing the kernel's
                // body into the caller, or binding a raw CALL to its
                // compiled entry, both compile the caller and silently
                // end offload at that site -- which is what
                // `offload_jit_gate` used to refuse the whole method to
                // prevent.
                //
                // Read off the site's own `JitInvokeInfo`. No info means
                // this walk cannot name the callee, so it does not
                // filter: failing open leaves the site exactly as it was
                // before this existed.
                //
                // Unarmed (no `--gpu`) `is_kernel` is one relaxed bool,
                // so a CPU-only run pays a load per compiled
                // invokestatic SITE at compile time and nothing at all
                // at run time.
                let site_is_gpu_kernel = info_ptr.is_some_and(|ip| {
                    // SAFETY: `invoke_info` owns every pointer it hands
                    // out for the life of this compile.
                    let info = unsafe { &*(ip as *const crate::JitInvokeInfo) };
                    // `keeps_dispatch_helper`, not `is_kernel`: inlining is
                    // one-way, and a registry miss can mean "could not have
                    // known yet". See its AUDIT 2026-09-07 note.
                    (info.invoke_kind == 3
                        && crate::offload_hook::keeps_dispatch_helper(
                            info.class_name,
                            info.method_name,
                            info.descriptor,
                        ))
                        || crate::offload_hook::is_reflect_ctor_dispatch(
                            info.class_name,
                            info.method_name,
                            info.descriptor,
                        )
                });

                // Check for inline site first (most profitable)
                if !site_is_gpu_kernel && self.inline_sites.contains_key(&pc) {
                    if self.try_emit_inline(pc) {
                        pc += 3;
                        return WalkStep::Next(pc);
                    }
                }

                // Check for direct call target
                // MED-4 / Fix 3 — O(1) pc-indexed lookup.
                // Round 9 wave 7 (`mutrec7`): a row marked
                // `JIT_CYCLE_EDGE_CELL_GUARD` carries a `JitCycleEdgeCell`
                // ADDRESS, not an entry. It is lowered by
                // `emit_cycle_edge_cell_call` below and must never reach the
                // direct-call ladder, which would `CALL` the cell.
                let cycle_cell = if site_is_gpu_kernel {
                    None
                } else {
                    self.direct_calls_idx.get(&pc).and_then(|&i| {
                        let dc = &self.direct_calls[i].1;
                        (dc.guard_class_id == crate::JIT_CYCLE_EDGE_CELL_GUARD).then_some(dc.entry)
                    })
                };
                // Round 9 wave 9 (`evict9`): a row marked
                // `JIT_RETIRE_CELL_GUARD` is an ordinary baked direct call
                // whose target is read through a `JitCycleEdgeCell` PINNED to
                // that one entry, so an invalidation that retires the callee
                // sends this (possibly still running) caller to dispatch
                // instead of back into the withdrawn body. Its `entry` is the
                // cell's ADDRESS; the entry it guards is the cell's pinned one,
                // which is what the direct-call ladder below is handed.
                let retire_cell = if site_is_gpu_kernel || cycle_cell.is_some() {
                    None
                } else {
                    self.direct_calls_idx.get(&pc).and_then(|&i| {
                        let dc = &self.direct_calls[i].1;
                        (dc.guard_class_id == crate::JIT_RETIRE_CELL_GUARD).then_some(dc.entry)
                    })
                };
                let direct = if site_is_gpu_kernel || cycle_cell.is_some() {
                    None
                } else {
                    self.direct_calls_idx.get(&pc).and_then(|&i| {
                        let dc = &self.direct_calls[i].1;
                        // Round 9 wave 10 (`call10`): an INSTANCE self-call
                        // marker (`walk_invoke_instance` hands its site here)
                        // names no entry; it takes the self-recursive arm
                        // below, which it reaches with no direct and no info.
                        if dc.guard_class_id == crate::JIT_SELF_CALL_GUARD {
                            return None;
                        }
                        let entry = if dc.guard_class_id == crate::JIT_RETIRE_CELL_GUARD {
                            // SAFETY: the planner that marked this row owns the
                            // cell (`SinglePassParts::owned_cycle_cells`, or the
                            // OSR door's cell vector) for the whole compile.
                            unsafe { crate::JitCycleEdgeCell::pinned_entry_at(dc.entry) }
                        } else {
                            dc.entry
                        };
                        // A retire row whose cell pins nothing would CALL 0:
                        // leave the site to the dispatch arm (its info is
                        // registered) rather than emit that.
                        if entry == 0 {
                            return None;
                        }
                        note_emit_direct(&self.method_key, pc, entry);
                        Some((entry, dc.needs_context, dc.num_params, dc.return_type))
                    })
                };
                // The guarded cell call needs the frame's VM context (the
                // stack guard and the dispatch fallback both take it), both
                // helpers, and the site's `JitInvokeInfo` (the fallback
                // dispatches through it). Missing any of them, the site takes
                // the plain dispatch arm below -- `direct` is already `None`.
                if let (Some(cell_addr), Some(info)) = (cycle_cell, info_ptr) {
                    if self.needs_heap
                        && self.helpers.self_call_stack_guard != 0
                        && self.helpers.invoke_dispatch != 0
                    {
                        if !self.emit_cycle_edge_cell_call(pc, cell_addr, info) {
                            return WalkStep::Return(false);
                        }
                        pc += 3;
                        return WalkStep::Next(pc);
                    }
                } else if cycle_cell.is_some() {
                    // A cell row with no `JitInvokeInfo` would fall to the
                    // SELF-recursive arm (no info, no direct) and CALL this
                    // method's own entry. `try_compile` always registers one,
                    // so this is a planner/emitter disagreement: refuse.
                    self.fail("singlepass-codegen/cycle-edge-cell-without-invoke-info");
                    return WalkStep::Return(false);
                }

                let arraycopy_row = direct.is_some_and(|(entry, _, _, _)| {
                    entry == crate::JitIntrinsic::ArraycopyPrimitive.as_entry()
                });
                let direct = direct.filter(|(entry, _, _, _)| {
                    if *entry != crate::JitIntrinsic::ArraycopyPrimitive.as_entry() {
                        return true;
                    }
                    !self
                        .despec
                        .as_ref()
                        .is_some_and(|registry| registry.contains(&self.method_key, pc as u32))
                });
                // SBR4 osr: a de-specialised `System.arraycopy` site is
                // lowered by the generic dispatch arm, which needs the
                // site's `JitInvokeInfo`. With no row and no info the walk
                // would fall through to the SELF-RECURSIVE arm below and
                // CALL this method's own entry with the arraycopy operands
                // (`TimSort.binarySort` through the OSR door, whose table
                // builder once registered the intrinsic without the info).
                // Refuse the compile instead of emitting that call.
                if arraycopy_row && direct.is_none() && info_ptr.is_none() {
                    self.fail("singlepass-codegen/despecialised-arraycopy-without-invoke-info");
                    return WalkStep::Return(false);
                }
                // E27-1 N2b: `indexOf(I)` is intrinsified ONLY where the
                // needle is a compile-time constant in `0..=0xFFFF`, which
                // is the range on which the inline single-code-unit scan
                // and `code_point_needle` are the same function. Filtered
                // HERE, before the intrinsic ladder, so a declined site
                // takes the ordinary dispatch it takes today — the same
                // shape as the `ArraycopyPrimitive` despec filter above.
                // Deliberately NOT a runtime screen: see
                // `prev_insn_int_const` for why that would be a cliff.
                let direct = direct.filter(|(entry, _, _, _)| {
                    if *entry != crate::JitIntrinsic::StringIndexOfChar.as_entry() {
                        return true;
                    }
                    matches!(prev_insn_int_const(code, code_len, pc), Some(0..=0xFFFF))
                });
                // Round 11 wave 13 (lane `spsync`, page
                // `r11w12-calls-single-pass-sync-direct-patch`): a caller-held
                // synchronized direct CALL. Its entry is the callee's
                // MONITOR-FREE body, so the generic arm below takes the class
                // monitor around the CALL. Two pcs take the dispatch arm
                // instead (the site's `JitInvokeInfo` is registered, and the
                // helper locks by name):
                //
                // * one a handler of this method protects: the enter's
                //   exception check would be the first writer of the bci's
                //   precise exceptional frame, naming argument words that are
                //   dead after the CALL;
                // * one whose frame has no room for the mirror word. The enter
                //   pushes it above the arguments and the normal-edge exit
                //   above the result, so a zero-argument callee with a result
                //   needs two words past the cursor and anything else one.
                //   `max_stack` does not count them; asked here, a probe that
                //   cannot fail the compile, rather than let the push fail it.
                let sync_row = if site_is_gpu_kernel {
                    None
                } else {
                    self.direct_calls_idx
                        .get(&pc)
                        .and_then(|&i| self.direct_calls.get(i))
                        .and_then(|(_, dc)| {
                            (dc.guard_class_id == crate::JIT_SYNC_DIRECT_GUARD)
                                .then_some((dc.num_params, dc.return_type))
                        })
                };
                let sync_direct_site = sync_row.is_some_and(|(params, ret)| {
                    let words = if params == 0 && ret != b'V' { 2 } else { 1 };
                    !self.pc_is_protected(pc)
                        && self.spill_range_fits(self.next_spill_offset, words)
                });
                let direct = if sync_row.is_some() && !sync_direct_site {
                    None
                } else {
                    direct
                };
                // Round 13 wave 1 (lane `sync`): the INSTANCE twin's row
                // (`crate::JIT_SYNC_DIRECT_INSTANCE_TAG`). Only
                // `walk_invoke_instance` hands one here, with the receiver's
                // null check or exact-class guard already emitted and only when
                // `sync_direct_instance_site` admits the site; the arm below
                // then locks the RECEIVER where the static row locks the class
                // mirror, and pops it with the arguments. Reached any other
                // way the row names a monitor-free body nothing here may CALL
                // unlocked, so the method is refused.
                let sync_instance_row = self
                    .direct_calls_idx
                    .get(&pc)
                    .and_then(|&i| self.direct_calls.get(i))
                    .is_some_and(|(_, dc)| {
                        crate::sync_direct_instance_guard_class(dc.guard_class_id).is_some()
                    });
                if sync_instance_row
                    && (!matches!(code.get(pc).copied(), Some(0xb6 | 0xb7))
                        || site_is_gpu_kernel
                        || direct.is_none()
                        || self.sync_direct_instance_site(pc).is_none())
                {
                    self.fail("singlepass-codegen/sync-direct-instance-row-misrouted");
                    return WalkStep::Return(false);
                }
                let sync_direct_site = sync_direct_site || sync_instance_row;

                if let Some((callee_entry, callee_needs_ctx, callee_params, ret_type)) = direct {
                    if callee_entry == crate::MATH_SQRT_INTRINSIC {
                        // Math.sqrt(double) intrinsic: inline SQRTSD — no call overhead
                        let arg_slot = self.pop_stack();
                        // Flush any OTHER Xmm(0) slots that would be clobbered by SQRTSD
                        // (the arg itself is fine — it's consumed). `_keeping`,
                        // here and in the five sibling arms below: a bare flush
                        // may relocate a live Xmm(0) INTO the scratch register
                        // `pop_stack` just freed — the popped operand's own —
                        // before the arm reads it
                        // (`single-pass-math-intrinsics-flush-xmm0-after-pop-20260918.md`).
                        // Unreachable today, since `flush_scratch_registers`
                        // at the top of this arm leaves no XMM0-7 entry on the
                        // stack, but not by anything the arm itself states.
                        self.flush_xmm0_slots_keeping(&[arg_slot]);
                        match arg_slot {
                            StackSlot::Xmm(xmm) => {
                                if xmm != 0 {
                                    // MOVSD XMM0, XMMn
                                    let modrm = 0xC0 | (xmm & 7);
                                    if xmm >= 8 {
                                        self.buf.emit(&[0xF2, 0x41, 0x0F, 0x10, modrm]);
                                    } else {
                                        self.buf.emit(&[0xF2, 0x0F, 0x10, modrm]);
                                    }
                                }
                                // else: already in XMM0
                            }
                            _ => {
                                self.load_slot_to_reg(RAX, arg_slot);
                                self.emit_movq_xmm_from_rax(0);
                            }
                        }
                        self.emit_sqrtsd_xmm0();
                        self.stack_push(StackSlot::Xmm(0), false);
                    } else if callee_entry == crate::MATH_FLOOR_INTRINSIC
                        || callee_entry == crate::MATH_CEIL_INTRINSIC
                        || callee_entry == crate::MATH_RINT_INTRINSIC
                    {
                        // Math.floor/ceil/rint intrinsic: ROUNDSD XMM0, XMM0, imm8
                        let arg_slot = self.pop_stack();
                        self.flush_xmm0_slots_keeping(&[arg_slot]);
                        match arg_slot {
                            StackSlot::Xmm(xmm) => {
                                if xmm != 0 {
                                    let modrm = 0xC0 | (xmm & 7);
                                    if xmm >= 8 {
                                        self.buf.emit(&[0xF2, 0x41, 0x0F, 0x10, modrm]);
                                    } else {
                                        self.buf.emit(&[0xF2, 0x0F, 0x10, modrm]);
                                    }
                                }
                            }
                            _ => {
                                self.load_slot_to_reg(RAX, arg_slot);
                                self.emit_movq_xmm_from_rax(0);
                            }
                        }
                        // ROUNDSD XMM0, XMM0, imm8
                        // Encoding: 66 0F 3A 0B C0 imm8
                        let imm8 = if callee_entry == crate::MATH_FLOOR_INTRINSIC {
                            0x09u8 // round toward -inf, inexact suppress
                        } else if callee_entry == crate::MATH_CEIL_INTRINSIC {
                            0x0Au8 // round toward +inf, inexact suppress
                        } else {
                            0x08u8 // round to nearest even, inexact suppress
                        };
                        self.buf.emit(&[0x66, 0x0F, 0x3A, 0x0B, 0xC0, imm8]);
                        self.stack_push(StackSlot::Xmm(0), false);
                    } else if callee_entry == crate::MATH_ABS_DOUBLE_INTRINSIC {
                        // Math.abs(double): clear sign bit (bit 63)
                        let arg_slot = self.pop_stack();
                        self.flush_xmm0_slots_keeping(&[arg_slot]);
                        match arg_slot {
                            StackSlot::Xmm(xmm) => {
                                if xmm != 0 {
                                    let modrm = 0xC0 | (xmm & 7);
                                    if xmm >= 8 {
                                        self.buf.emit(&[0xF2, 0x41, 0x0F, 0x10, modrm]);
                                    } else {
                                        self.buf.emit(&[0xF2, 0x0F, 0x10, modrm]);
                                    }
                                }
                            }
                            _ => {
                                self.load_slot_to_reg(RAX, arg_slot);
                                self.emit_movq_xmm_from_rax(0);
                            }
                        }
                        // Load sign mask 0x7FFFFFFFFFFFFFFF into RCX, then MOVQ XMM1, RCX, ANDPD XMM0, XMM1
                        // MOV RCX, imm64
                        self.buf.emit_byte(0x48); // REX.W
                        self.buf.emit_byte(0xB9); // MOV RCX, imm64
                        self.buf.emit(&0x7FFFFFFFFFFFFFFFu64.to_le_bytes());
                        // MOVQ XMM1, RCX
                        self.emit_movq_xmm_from_gpr(1, RCX);
                        // ANDPD XMM0, XMM1: 66 0F 54 C1
                        self.buf.emit(&[0x66, 0x0F, 0x54, 0xC1]);
                        self.stack_push(StackSlot::Xmm(0), false);
                    } else if callee_entry == crate::MATH_ABS_FLOAT_INTRINSIC {
                        // Math.abs(float): clear sign bit (bit 31)
                        let arg_slot = self.pop_stack();
                        self.flush_xmm0_slots_keeping(&[arg_slot]);
                        match arg_slot {
                            StackSlot::Xmm(xmm) => {
                                if xmm != 0 {
                                    let modrm = 0xC0 | (xmm & 7);
                                    if xmm >= 8 {
                                        self.buf.emit(&[0xF3, 0x41, 0x0F, 0x10, modrm]);
                                    } else {
                                        self.buf.emit(&[0xF3, 0x0F, 0x10, modrm]);
                                    }
                                }
                            }
                            _ => {
                                self.load_slot_to_reg(RAX, arg_slot);
                                // MOVD XMM0, EAX: 66 0F 6E C0
                                self.buf.emit(&[0x66, 0x0F, 0x6E, 0xC0]);
                            }
                        }
                        // Load sign mask 0x7FFFFFFF into ECX, MOVD XMM1, ECX, ANDPS XMM0, XMM1
                        // MOV ECX, imm32
                        self.buf.emit_byte(0xB9);
                        self.buf.emit(&0x7FFFFFFFu32.to_le_bytes());
                        // MOVD XMM1, ECX: 66 0F 6E C9
                        self.buf.emit(&[0x66, 0x0F, 0x6E, 0xC9]);
                        // ANDPS XMM0, XMM1: 0F 54 C1
                        self.buf.emit(&[0x0F, 0x54, 0xC1]);
                        self.stack_push(StackSlot::Xmm(0), false);
                    } else if callee_entry == crate::MATH_ABS_INT_INTRINSIC {
                        // Math.abs(int): branchless absolute value
                        let arg_slot = self.pop_stack();
                        self.load_slot_to_reg(RAX, arg_slot);
                        // MOV ECX, EAX: 89 C1
                        self.buf.emit(&[0x89, 0xC1]);
                        // SAR EAX, 31 (sign-extend to all bits): C1 F8 1F
                        self.buf.emit(&[0xC1, 0xF8, 0x1F]);
                        // XOR ECX, EAX: 31 C1
                        self.buf.emit(&[0x31, 0xC1]);
                        // SUB ECX, EAX: 29 C1
                        self.buf.emit(&[0x29, 0xC1]);
                        // MOVSXD RAX, ECX: 48 63 C1. NOT `MOV EAX, ECX`: an
                        // `int` is held sign-extended in its 64-bit slot (see
                        // the Math.min/max int arm below), and the one input
                        // whose result is negative — `Math.abs(MIN_VALUE)`,
                        // which the JLS defines as MIN_VALUE — came out of
                        // the 32-bit move as +2^31 to every 64-bit consumer
                        // (a spliced `i2l`, a 64-bit compare, a helper's i64
                        // argument).
                        self.buf.emit(&[0x48, 0x63, 0xC1]);
                        self.push_from_rax();
                    } else if callee_entry == crate::MATH_ABS_LONG_INTRINSIC {
                        // Math.abs(long): branchless 64-bit absolute value
                        let arg_slot = self.pop_stack();
                        self.load_slot_to_reg(RAX, arg_slot);
                        // MOV RCX, RAX: 48 89 C1
                        self.buf.emit(&[0x48, 0x89, 0xC1]);
                        // SAR RAX, 63: 48 C1 F8 3F
                        self.buf.emit(&[0x48, 0xC1, 0xF8, 0x3F]);
                        // XOR RCX, RAX: 48 31 C1
                        self.buf.emit(&[0x48, 0x31, 0xC1]);
                        // SUB RCX, RAX: 48 29 C1
                        self.buf.emit(&[0x48, 0x29, 0xC1]);
                        // MOV RAX, RCX: 48 89 C8
                        self.buf.emit(&[0x48, 0x89, 0xC8]);
                        self.push_from_rax();
                    } else if callee_entry == crate::MATH_FMA_DOUBLE_INTRINSIC {
                        // T1.1.28 — Math.fma(double, double, double), and
                        // StrictMath.fma, which resolves to the same intrinsic.
                        //
                        // Per JLS, `Math.fma(a, b, c)` computes `a*b + c`
                        // as if with unlimited intermediate precision and
                        // then rounded once. Round 12 wave 8 (lane rt3): with
                        // FMA3 that is one `VFMADD213SD`, emitted inline;
                        // otherwise (or `CRATONVM_JIT_FMA_INLINE=0`) a call to
                        // `jit_math_fma_double`. That helper used to be
                        // `f64::mul_add`, which this build (no `+fma` target
                        // feature) lowers to a CALL into the C runtime's
                        // `fma` -- correctly rounded only if the CRT is; it
                        // now uses the FMA3 instruction or an exact software
                        // form (`phases_late::java_fma_f64`).
                        //
                        // Operands in XMM0, XMM1, XMM2 either way: the helper
                        // is `extern "C" fn(f64, f64, f64) -> f64` on both
                        // SysV and Win64. The flush below also leaves no
                        // operand-stack value in XMM0-7, which the inline
                        // form clobbers (XMM0 only) as the call would.
                        self.flush_scratch_registers();
                        let c_slot = self.pop_stack();
                        let b_slot = self.pop_stack();
                        let a_slot = self.pop_stack();
                        // Load a into RAX then → XMM0.
                        self.load_slot_to_reg(RAX, a_slot);
                        self.emit_movq_xmm_from_rax(0);
                        self.load_slot_to_reg(RAX, b_slot);
                        self.emit_movq_xmm_from_rax(1);
                        self.load_slot_to_reg(RAX, c_slot);
                        self.emit_movq_xmm_from_rax(2);
                        let fma_inline = super::cpu_features::inline_fma_enabled();
                        // ABI-3, deliberately NOT `assert_helper_call_shape!`: that macro
                        // asserts `float_args == 0`, because every site it guards marshals
                        // through the integer ARG_REGS file, and `jit_math_fma_double` is
                        // `(f64, f64, f64) -> f64` with its arguments in
                        // XMM0/XMM1/XMM2 on both ABIs. Asserting the integer shape here would
                        // be worse than asserting nothing, so what is pinned is the fact the
                        // sequence above actually depends on.
                        const _: () = {
                            let sig =
                                cratonvm_jit_api::helpers_abi::helper_sig_of("math_fma_double");
                            assert!(
                                sig.arity == 3 && sig.float_args == 3 && sig.returns_value,
                                "`math_fma_double` no longer takes three floating-point \
                                 operands; this site stages them into XMM0/XMM1/XMM2 and \
                                 would now be loading the wrong register file",
                            );
                        };
                        if fma_inline {
                            // VFMADD213SD xmm0, xmm1, xmm2
                            // (VEX.LIG.66.0F38.W1 A9 /r: C4 | RXB=111
                            // mmmmm=00010 | W=1 vvvv=~1 L=0 pp=01 | A9 |
                            // mod=11 reg=0 rm=2): xmm0 = xmm1*xmm0 + xmm2
                            // = b*a + c, one rounding (MXCSR is
                            // round-to-nearest, as every other FP arm assumes).
                            self.buf.emit(&[0xC4, 0xE2, 0xF1, 0xA9, 0xC2]);
                        } else {
                            self.emit_call_absolute(self.helpers.math_fma_double);
                        }
                        // Result in XMM0 → move to RAX and push as FP.
                        self.emit_movq_rax_from_xmm(0);
                        self.push_from_rax_as_xmm0();
                    } else if callee_entry == crate::MATH_FMA_FLOAT_INTRINSIC {
                        // T1.1.28 — Math.fma(float, float, float) — same
                        // plan: `VFMADD213SS` inline with FMA3, else
                        // `jit_math_fma_float` (`phases_late::java_fma_f32`,
                        // no longer the CRT's `fmaf`, which may round twice).
                        self.flush_scratch_registers();
                        let c_slot = self.pop_stack();
                        let b_slot = self.pop_stack();
                        let a_slot = self.pop_stack();
                        self.load_slot_to_reg(RAX, a_slot);
                        self.emit_movq_xmm_from_rax(0);
                        self.load_slot_to_reg(RAX, b_slot);
                        self.emit_movq_xmm_from_rax(1);
                        self.load_slot_to_reg(RAX, c_slot);
                        self.emit_movq_xmm_from_rax(2);
                        let fma_inline = super::cpu_features::inline_fma_enabled();
                        // ABI-3, deliberately NOT `assert_helper_call_shape!`: that macro
                        // asserts `float_args == 0`, because every site it guards marshals
                        // through the integer ARG_REGS file, and `jit_math_fma_float` is
                        // `(f32, f32, f32) -> f32` with its arguments in
                        // XMM0/XMM1/XMM2 on both ABIs. Asserting the integer shape here would
                        // be worse than asserting nothing, so what is pinned is the fact the
                        // sequence above actually depends on.
                        const _: () = {
                            let sig =
                                cratonvm_jit_api::helpers_abi::helper_sig_of("math_fma_float");
                            assert!(
                                sig.arity == 3 && sig.float_args == 3 && sig.returns_value,
                                "`math_fma_float` no longer takes three floating-point \
                                 operands; this site stages them into XMM0/XMM1/XMM2 and \
                                 would now be loading the wrong register file",
                            );
                        };
                        if fma_inline {
                            // VFMADD213SS xmm0, xmm1, xmm2
                            // (VEX.LIG.66.0F38.W0 A9 /r; as above with W=0):
                            // xmm0 = b*a + c in single precision, one rounding.
                            self.buf.emit(&[0xC4, 0xE2, 0x71, 0xA9, 0xC2]);
                        } else {
                            self.emit_call_absolute(self.helpers.math_fma_float);
                        }
                        self.emit_movq_rax_from_xmm(0);
                        self.push_from_rax_as_xmm0();
                    } else if callee_entry == crate::MATH_MIN_INT_INTRINSIC
                        || callee_entry == crate::MATH_MAX_INT_INTRINSIC
                        || callee_entry == crate::MATH_MIN_LONG_INTRINSIC
                        || callee_entry == crate::MATH_MAX_LONG_INTRINSIC
                    {
                        // Round-8 Bug 8 — branchless int/long Math.min/max
                        // via CMOV; one emitter shared with the splice arm
                        // (`inlining.rs`, `emit_int_long_minmax_cmov`).
                        self.emit_int_long_minmax_cmov(callee_entry);
                    } else if callee_entry == crate::MATH_MIN_FLOAT_INTRINSIC
                        || callee_entry == crate::MATH_MAX_FLOAT_INTRINSIC
                        || callee_entry == crate::MATH_MIN_DOUBLE_INTRINSIC
                        || callee_entry == crate::MATH_MAX_DOUBLE_INTRINSIC
                    {
                        // `Math.min`/`Math.max` for float and double; one emitter
                        // shared with the splice arm (`inlining.rs`, `emit_float_double_minmax`).
                        self.emit_float_double_minmax(callee_entry);
                    } else if callee_entry == crate::MATH_MULTIPLY_HIGH_INTRINSIC
                        || callee_entry == crate::MATH_UNSIGNED_MULTIPLY_HIGH_INTRINSIC
                    {
                        // Math.multiplyHigh(JJ)J / unsignedMultiplyHigh(JJ)J —
                        // high 64 bits of the 128-bit product. The hottest leaf
                        // in the SunEC P-256 Montgomery field multiply, called
                        // once per limb pair. One-operand `IMUL r64` (signed) /
                        // `MUL r64` (unsigned) compute RDX:RAX = RAX * r64; the
                        // high half lands in RDX. Multiplication is commutative,
                        // so operand order is irrelevant to the result.
                        //
                        // flush_scratch_registers() above already spilled every
                        // value-stack slot out of RAX/RCX/RDX (locals live only
                        // in callee-saved R12-R15/RBX/RSI/RDI, deferred-spill
                        // slots only in R8/R9), so clobbering RAX/RCX/RDX here is
                        // safe — same contract the Math.min/max long path relies
                        // on, extended to RDX.
                        let b_slot = self.pop_stack();
                        let a_slot = self.pop_stack();
                        self.load_slot_to_reg(RAX, a_slot);
                        self.load_slot_to_reg(RCX, b_slot);
                        // IMUL RCX (48 F7 E9) signed / MUL RCX (48 F7 E1) unsigned.
                        let modrm = if callee_entry == crate::MATH_MULTIPLY_HIGH_INTRINSIC {
                            0xE9u8 // /5 IMUL
                        } else {
                            0xE1u8 // /4 MUL
                        };
                        self.buf.emit(&[0x48, 0xF7, modrm]);
                        // MOV RAX, RDX (48 89 D0) — high half is the result.
                        self.buf.emit(&[0x48, 0x89, 0xD0]);
                        self.push_from_rax();
                    }
                    // --- invokestatic intrinsic family regions ---
                    // A follow-up agent for family <TAG> appends its
                    // codegen as `else if callee_entry ==
                    // crate::JitIntrinsic::Foo.as_entry() { ... }`
                    // strictly between that family's BEGIN/END markers.
                    // An empty region contributes nothing, so the
                    // `if let Some(...) = direct` chain stays valid.
                    //
                    // ===== INTRINSIC REGION BEGIN: INT_BITS =====
                    // java.lang.Integer bit-manipulation intrinsics
                    // (Phase 1a). Each pops `num_params` ints off the
                    // operand stack, computes into EAX and pushes the
                    // result. All emitted code is bit-identical to the
                    // JDK semantics (verified by intrinsic_int_bits.rs).
                    // --- FP_BITS: Double bit reinterpretation ---
                    //
                    // One `MOVQ` each. These replaced 361 million checked
                    // native-bridge crossings in one run of
                    // `PSquarePercentileTest`; see the FP_BITS region in
                    // `jit/src/lib.rs` for the census.
                    //
                    // RAW semantics come free: `MOVQ` moves all 64 bits,
                    // NaN payload included, which is exactly what
                    // `doubleToRawLongBits` is specified to return. The
                    // canonicalising `doubleToLongBits` is not matched by
                    // the resolver and so cannot reach here.
                    else if callee_entry == crate::JitIntrinsic::DoubleToRawLongBits.as_entry() {
                        // Double.doubleToRawLongBits(d): the argument's
                        // 64 bits, unchanged, as a long.
                        let arg = self.pop_stack();
                        match arg {
                            StackSlot::Xmm(xmm) => self.emit_movq_rax_from_xmm(xmm),
                            // A frame slot or GPR already holds the raw
                            // 64-bit pattern -- the JIT stores a double
                            // as its bits -- so this is a plain load and
                            // no XMM round trip is needed.
                            _ => self.load_slot_to_reg(RAX, arg),
                        }
                        self.push_from_rax();
                    } else if callee_entry == crate::JitIntrinsic::LongBitsToDouble.as_entry() {
                        // Double.longBitsToDouble(bits): the mirror.
                        let arg = self.pop_stack();
                        self.flush_xmm0_slots_keeping(&[arg]);
                        self.load_slot_to_reg(RAX, arg);
                        self.emit_movq_xmm_from_rax(0);
                        self.stack_push(StackSlot::Xmm(0), false);
                    } else if callee_entry == crate::JitIntrinsic::FloatToRawIntBits.as_entry() {
                        // Float.floatToRawIntBits(f) (round 15 wave 7, lane
                        // drainB; IR2-2): the argument's 32 bits as an int.
                        // A float lives in the low 32 bits of its XMM / slot
                        // (the `Math.abs(F)F` arm's convention).
                        //
                        // JIT round 21 wave 3 (lane x64emit): the RESULT is an
                        // `int`, and this backend holds an int sign-extended
                        // through all 64 bits (`Math.abs(I)I`'s arm says why:
                        // a spliced `i2l` is a no-op, and a 64-bit consumer
                        // reads the whole word). The `MOVD` / `MOV EAX, EAX`
                        // below zero-extend, so every NEGATIVE float (sign bit
                        // set, i.e. a negative int) read back as `+2^32 + bits`
                        // through such a consumer:
                        // `widen(Float.floatToRawIntBits(-1f))` with
                        // `static long widen(int x) { return x; }` spliced gave
                        // 3212836864 where Java says -1082130432. A `MOVSXD`
                        // now restores the convention;
                        // `CRATONVM_JIT_X64_RAW_INT_BITS_SEXT=0` emits the old
                        // bytes.
                        let sext = true;
                        let arg = self.pop_stack();
                        self.emit_float_raw_int_bits_to_rax(arg, sext);
                        self.push_from_rax();
                    } else if callee_entry == crate::JitIntrinsic::IntBitsToFloat.as_entry() {
                        // Float.intBitsToFloat(bits): the mirror, MOVD XMM0, EAX.
                        let arg = self.pop_stack();
                        self.flush_xmm0_slots_keeping(&[arg]);
                        self.load_slot_to_reg(RAX, arg);
                        // MOVD XMM0, EAX: 66 0F 6E C0
                        self.buf.emit(&[0x66, 0x0F, 0x6E, 0xC0]);
                        self.stack_push(StackSlot::Xmm(0), false);
                    }
                    // ===== INTRINSIC REGION BEGIN: SCALAR_ROWS_SP =====
                    // JIT round 22 wave 2, lane intrinsics: the single-pass
                    // twins of the optimizing tier's `ScalarOp::RoundD` /
                    // `RoundF` and code-point rows (registered by
                    // `lib.rs` `sp_scalar_row` only where that tier claims
                    // the triple too). Same instruction sequences.
                    else if let Some(m) = crate::JitIntrinsic::as_math_call_from_entry(callee_entry) {
                        self.flush_scratch_registers();
                        if m.arity() == 2 {
                            let b_slot = self.pop_stack();
                            let a_slot = self.pop_stack();
                            self.load_slot_to_reg(RAX, a_slot);
                            self.emit_movq_xmm_from_rax(0);
                            self.load_slot_to_reg(RAX, b_slot);
                            self.emit_movq_xmm_from_rax(1);
                        } else {
                            let a_slot = self.pop_stack();
                            self.load_slot_to_reg(RAX, a_slot);
                            self.emit_movq_xmm_from_rax(0);
                        }
                        self.emit_call_absolute(m.fn_ptr());
                        self.emit_movq_rax_from_xmm(0);
                        self.push_from_rax_as_xmm0();
                    }
                    else if callee_entry == crate::JitIntrinsic::MathRoundDouble.as_entry()
                        || callee_entry == crate::JitIntrinsic::MathRoundFloat.as_entry()
                    {
                        // Math/StrictMath.round: XMM0 = the argument (for the
                        // saturating fallback), RAX / EAX = its raw bits.
                        let dbl = callee_entry == crate::JitIntrinsic::MathRoundDouble.as_entry();
                        let arg = self.pop_stack();
                        self.flush_xmm0_slots_keeping(&[arg]);
                        match arg {
                            StackSlot::Xmm(xmm) => {
                                if xmm != 0 {
                                    // MOVSD XMM0, XMMn (the low 32 bits carry a float)
                                    let modrm = 0xC0 | (xmm & 7);
                                    if xmm >= 8 {
                                        self.buf.emit(&[0xF2, 0x41, 0x0F, 0x10, modrm]);
                                    } else {
                                        self.buf.emit(&[0xF2, 0x0F, 0x10, modrm]);
                                    }
                                }
                                if dbl {
                                    self.buf.emit(&[0x66, 0x48, 0x0F, 0x7E, 0xC0]); // MOVQ RAX, XMM0
                                } else {
                                    self.buf.emit(&[0x66, 0x0F, 0x7E, 0xC0]); // MOVD EAX, XMM0
                                }
                            }
                            _ => {
                                self.load_slot_to_reg(RAX, arg);
                                if dbl {
                                    self.emit_movq_xmm_from_rax(0);
                                } else {
                                    self.buf.emit(&[0x66, 0x0F, 0x6E, 0xC0]); // MOVD XMM0, EAX
                                }
                            }
                        }
                        self.emit_sp_math_round(dbl);
                        self.push_from_rax();
                    } else if callee_entry == crate::JitIntrinsic::CharIsHighSurrogate.as_entry()
                        || callee_entry == crate::JitIntrinsic::CharIsLowSurrogate.as_entry()
                    {
                        // `lo <= ch < lo + 0x400` as one unsigned compare of
                        // `ch - lo`; 0 / 1 in EAX, upper half zero.
                        let lo: u32 =
                            if callee_entry == crate::JitIntrinsic::CharIsHighSurrogate.as_entry() {
                                0xD800
                            } else {
                                0xDC00
                            };
                        let arg = self.pop_stack();
                        self.load_slot_to_reg(RAX, arg);
                        self.buf.emit(&[0x2D]); // SUB EAX, imm32
                        self.buf.emit(&lo.to_le_bytes());
                        self.buf.emit(&[0x3D]); // CMP EAX, imm32
                        self.buf.emit(&0x400u32.to_le_bytes());
                        self.buf.emit(&[0x0F, 0x92, 0xC0]); // SETB AL
                        self.buf.emit(&[0x0F, 0xB6, 0xC0]); // MOVZX EAX, AL
                        self.push_from_rax();
                    } else if callee_entry == crate::JitIntrinsic::CharCharCount.as_entry() {
                        // JDK: `cp >= 0x10000 ? 2 : 1`, a SIGNED compare.
                        let arg = self.pop_stack();
                        self.load_slot_to_reg(RAX, arg);
                        self.buf.emit(&[0x3D]); // CMP EAX, imm32
                        self.buf.emit(&0x1_0000u32.to_le_bytes());
                        self.buf.emit(&[0x0F, 0x9D, 0xC0]); // SETGE AL
                        self.buf.emit(&[0x0F, 0xB6, 0xC0]); // MOVZX EAX, AL
                        self.buf.emit(&[0xFF, 0xC0]); // INC EAX
                        self.push_from_rax();
                    } else if callee_entry == crate::JitIntrinsic::CharIsBmpCodePoint.as_entry()
                        || callee_entry == crate::JitIntrinsic::CharIsValidCodePoint.as_entry()
                    {
                        // JDK: `cp >>> 16 == 0` / `cp >>> 16 < 0x11`, i.e. an
                        // UNSIGNED `cp < 0x10000` / `cp < 0x110000`.
                        let bound: u32 =
                            if callee_entry == crate::JitIntrinsic::CharIsBmpCodePoint.as_entry() {
                                0x1_0000
                            } else {
                                0x11_0000
                            };
                        let arg = self.pop_stack();
                        self.load_slot_to_reg(RAX, arg);
                        self.buf.emit(&[0x3D]); // CMP EAX, imm32
                        self.buf.emit(&bound.to_le_bytes());
                        self.buf.emit(&[0x0F, 0x92, 0xC0]); // SETB AL
                        self.buf.emit(&[0x0F, 0xB6, 0xC0]); // MOVZX EAX, AL
                        self.push_from_rax();
                    } else if callee_entry == crate::JitIntrinsic::CharIsIsoControl.as_entry() {
                        self.flush_scratch_registers();
                        let arg = self.pop_stack();
                        self.load_slot_to_reg(RAX, arg);
                        self.buf.emit(&[0x83, 0xF8, 0x1F]); // CMP EAX, 0x1F
                        self.buf.emit(&[0x0F, 0x96, 0xC1]); // SETBE CL
                        self.buf.emit(&[0x83, 0xE8, 0x7F]); // SUB EAX, 0x7F
                        self.buf.emit(&[0x83, 0xF8, 0x20]); // CMP EAX, 0x20
                        self.buf.emit(&[0x0F, 0x96, 0xC0]); // SETBE AL
                        self.buf.emit(&[0x08, 0xC8]);       // OR AL, CL
                        self.buf.emit(&[0x0F, 0xB6, 0xC0]); // MOVZX EAX, AL
                        self.push_from_rax();
                    }
                    // ===== INTRINSIC REGION END: SCALAR_ROWS_SP =====
                    else if callee_entry == crate::JitIntrinsic::IntBitCount.as_entry() {
                        // Integer.bitCount(i): POPCNT EAX, EAX. The
                        // matcher only registers this when has_popcnt()
                        // is true, so the instruction is always valid.
                        let arg = self.pop_stack();
                        self.load_slot_to_reg(RAX, arg);
                        // POPCNT EAX, EAX: F3 0F B8 C0
                        self.buf.emit(&[0xF3, 0x0F, 0xB8, 0xC0]);
                        self.push_from_rax();
                    } else if callee_entry
                        == crate::JitIntrinsic::IntNumberOfLeadingZeros.as_entry()
                    {
                        // Integer.numberOfLeadingZeros(i): result is 32
                        // for input 0, else 31 - floor(log2(i)).
                        let arg = self.pop_stack();
                        self.load_slot_to_reg(RAX, arg);
                        if crate::x64::has_lzcnt() {
                            // LZCNT EAX, EAX: F3 0F BD C0 — defined to
                            // return 32 for a zero input, matching JDK.
                            self.buf.emit(&[0xF3, 0x0F, 0xBD, 0xC0]);
                        } else {
                            // Fallback: BSR ECX, EAX gives the MSB index
                            // and sets ZF iff the input is zero. We pick
                            // ECX = -1 on a zero input so the subsequent
                            // `31 - ECX` formula yields 32.
                            //   MOV EDX, -1
                            self.buf.emit(&[0xBA]);
                            self.buf.emit(&(-1i32).to_le_bytes());
                            // BSR ECX, EAX: 0F BD C8 (ZF=1 if EAX==0)
                            self.buf.emit(&[0x0F, 0xBD, 0xC8]);
                            // CMOVZ ECX, EDX: 0F 44 CA (consumes BSR's ZF)
                            self.buf.emit(&[0x0F, 0x44, 0xCA]);
                            // MOV EAX, 31: B8 1F 00 00 00
                            self.buf.emit(&[0xB8]);
                            self.buf.emit(&31i32.to_le_bytes());
                            // SUB EAX, ECX: 29 C8  → EAX = 31 - index
                            self.buf.emit(&[0x29, 0xC8]);
                        }
                        self.push_from_rax();
                    } else if callee_entry
                        == crate::JitIntrinsic::IntNumberOfTrailingZeros.as_entry()
                    {
                        // Integer.numberOfTrailingZeros(i): result is 32
                        // for input 0, else the LSB index.
                        let arg = self.pop_stack();
                        self.load_slot_to_reg(RAX, arg);
                        if crate::x64::has_bmi1() {
                            // TZCNT EAX, EAX: F3 0F BC C0 — defined to
                            // return 32 for a zero input, matching JDK.
                            self.buf.emit(&[0xF3, 0x0F, 0xBC, 0xC0]);
                        } else {
                            // Fallback: BSF ECX, EAX gives the LSB index
                            // and sets ZF iff the input is zero; pick 32
                            // for the zero case.
                            //   MOV EDX, 32
                            self.buf.emit(&[0xBA]);
                            self.buf.emit(&32i32.to_le_bytes());
                            // BSF ECX, EAX: 0F BC C8 (ZF=1 if EAX==0)
                            self.buf.emit(&[0x0F, 0xBC, 0xC8]);
                            // CMOVZ ECX, EDX: 0F 44 CA
                            self.buf.emit(&[0x0F, 0x44, 0xCA]);
                            // MOV EAX, ECX: 89 C8
                            self.buf.emit(&[0x89, 0xC8]);
                        }
                        self.push_from_rax();
                    } else if callee_entry == crate::JitIntrinsic::IntReverseBytes.as_entry() {
                        // Integer.reverseBytes(i): BSWAP EAX (0F C8).
                        let arg = self.pop_stack();
                        self.load_slot_to_reg(RAX, arg);
                        self.buf.emit(&[0x0F, 0xC8]);
                        // MOVSXD RAX, EAX: the swapped value is negative
                        // whenever the input's low byte has bit 7 set
                        // (`reverseBytes(0x80) == 0x80000000`, `(-1) == -1`),
                        // and the 32-bit BSWAP zero-extended it. Same
                        // sign-extension contract as `rotateLeft` below.
                        self.buf.emit(&[0x48, 0x63, 0xC0]);
                        self.push_from_rax();
                    } else if callee_entry == crate::JitIntrinsic::IntHighestOneBit.as_entry() {
                        // Integer.highestOneBit(i): the value with only
                        // the highest set bit of `i`, or 0 when i == 0.
                        let arg = self.pop_stack();
                        self.load_slot_to_reg(RAX, arg);
                        // MOV EDX, EAX: 89 C2 — preserve the original.
                        self.buf.emit(&[0x89, 0xC2]);
                        // BSR ECX, EAX: 0F BD C8 — ECX = MSB index
                        // (undefined for input 0, handled below).
                        self.buf.emit(&[0x0F, 0xBD, 0xC8]);
                        // MOV EAX, 1: B8 01 00 00 00
                        self.buf.emit(&[0xB8]);
                        self.buf.emit(&1i32.to_le_bytes());
                        // SHL EAX, CL: D3 E0 — EAX = 1 << index.
                        self.buf.emit(&[0xD3, 0xE0]);
                        // TEST EDX, EDX: 85 D2 — set ZF iff input was 0.
                        self.buf.emit(&[0x85, 0xD2]);
                        // CMOVZ EAX, EDX: 0F 44 C2 — input 0 → result 0.
                        self.buf.emit(&[0x0F, 0x44, 0xC2]);
                        // MOVSXD RAX, EAX: every negative input answers
                        // MIN_VALUE, which the 32-bit ops left zero-extended.
                        self.buf.emit(&[0x48, 0x63, 0xC0]);
                        self.push_from_rax();
                    } else if callee_entry == crate::JitIntrinsic::IntLowestOneBit.as_entry() {
                        // Integer.lowestOneBit(i): i & -i. Naturally
                        // yields 0 for input 0, matching the JDK.
                        let arg = self.pop_stack();
                        self.load_slot_to_reg(RAX, arg);
                        // MOV ECX, EAX: 89 C1
                        self.buf.emit(&[0x89, 0xC1]);
                        // NEG EAX: F7 D8  → EAX = -i
                        self.buf.emit(&[0xF7, 0xD8]);
                        // AND EAX, ECX: 21 C8  → EAX = -i & i
                        self.buf.emit(&[0x21, 0xC8]);
                        // MOVSXD RAX, EAX: `lowestOneBit(MIN_VALUE)` is
                        // MIN_VALUE, negative.
                        self.buf.emit(&[0x48, 0x63, 0xC0]);
                        self.push_from_rax();
                    } else if callee_entry == crate::JitIntrinsic::IntReverse.as_entry() {
                        // Integer.reverse(i): reverse the bit order via
                        // the standard SWAR sequence. The JDK does five
                        // stages; the final two (swap byte pairs, then
                        // swap halves) are exactly BSWAP, so we emit
                        // three SWAR stages followed by BSWAP.
                        let arg = self.pop_stack();
                        self.load_slot_to_reg(RAX, arg);
                        // One SWAR stage for (mask, shift):
                        //   MOV ECX, EAX
                        //   SHR EAX, shift
                        //   AND EAX, mask
                        //   AND ECX, mask
                        //   SHL ECX, shift
                        //   OR  EAX, ECX
                        for &(mask, shift) in
                            &[(0x5555_5555u32, 1u8), (0x3333_3333, 2), (0x0F0F_0F0F, 4)]
                        {
                            // MOV ECX, EAX: 89 C1
                            self.buf.emit(&[0x89, 0xC1]);
                            // SHR EAX, imm8: C1 E8 ib
                            self.buf.emit(&[0xC1, 0xE8, shift]);
                            // AND EAX, imm32: 25 id
                            self.buf.emit(&[0x25]);
                            self.buf.emit(&mask.to_le_bytes());
                            // AND ECX, imm32: 81 E1 id
                            self.buf.emit(&[0x81, 0xE1]);
                            self.buf.emit(&mask.to_le_bytes());
                            // SHL ECX, imm8: C1 E1 ib
                            self.buf.emit(&[0xC1, 0xE1, shift]);
                            // OR EAX, ECX: 09 C8
                            self.buf.emit(&[0x09, 0xC8]);
                        }
                        // BSWAP EAX: 0F C8 — swaps the four bytes, which
                        // completes the 8- and 16-bit reversal stages.
                        self.buf.emit(&[0x0F, 0xC8]);
                        // MOVSXD RAX, EAX: any odd input reverses to a
                        // negative int.
                        self.buf.emit(&[0x48, 0x63, 0xC0]);
                        self.push_from_rax();
                    } else if callee_entry == crate::JitIntrinsic::IntCompare.as_entry() {
                        // Integer.compare(x, y): branchless (x>y)-(x<y).
                        // The stack holds x (deeper) then y.
                        let y = self.pop_stack();
                        let x = self.pop_stack();
                        self.load_slot_to_reg(RAX, x);
                        self.load_slot_to_reg(RCX, y);
                        // CMP EAX, ECX: 39 C8 — signed compare x vs y.
                        self.buf.emit(&[0x39, 0xC8]);
                        // SETG AL:  0F 9F C0 — AL = 1 if x > y.
                        self.buf.emit(&[0x0F, 0x9F, 0xC0]);
                        // SETL DL:  0F 9C C2 — DL = 1 if x < y.
                        self.buf.emit(&[0x0F, 0x9C, 0xC2]);
                        // MOVZX EAX, AL: 0F B6 C0
                        self.buf.emit(&[0x0F, 0xB6, 0xC0]);
                        // MOVZX EDX, DL: 0F B6 D2
                        self.buf.emit(&[0x0F, 0xB6, 0xD2]);
                        // SUB EAX, EDX: 29 D0 → -1, 0 or 1.
                        self.buf.emit(&[0x29, 0xD0]);
                        // MOVSXD RAX, EAX: `-1` came out of the 32-bit SUB as
                        // 0x00000000_FFFFFFFF. An `int` slot is held
                        // sign-extended, and the inline mini-emitter treats
                        // `i2l` as a no-op on that promise.
                        self.buf.emit(&[0x48, 0x63, 0xC0]);
                        self.push_from_rax();
                    } else if callee_entry == crate::JitIntrinsic::IntRotateLeft.as_entry()
                        || callee_entry == crate::JitIntrinsic::IntRotateRight.as_entry()
                    {
                        // Integer.rotateLeft(i, distance) / rotateRight:
                        // ROL/ROR EAX, CL. x86 masks CL & 0x1f for a 32-bit
                        // rotate, byte-identical to the JDK (rotation mod 32),
                        // so no explicit distance masking is needed. Stack:
                        // i (deeper), distance (top).
                        let dist = self.pop_stack();
                        let val = self.pop_stack();
                        self.load_slot_to_reg(RAX, val);
                        self.load_slot_to_reg(RCX, dist);
                        // ROL EAX, CL: D3 /0 = D3 C0 ; ROR EAX, CL: D3 /1 = D3 C8
                        let modrm = if callee_entry == crate::JitIntrinsic::IntRotateLeft.as_entry()
                        {
                            0xC0u8
                        } else {
                            0xC8u8
                        };
                        self.buf.emit(&[0xD3, modrm]);
                        // MOVSXD RAX, EAX (48 63 C0): a 32-bit rotate may set
                        // the high bit (negative int); re-extend to the
                        // canonical sign-extended 64-bit int form the value
                        // ABI expects (mirrors the Math.min int path).
                        self.buf.emit(&[0x48, 0x63, 0xC0]);
                        self.push_from_rax();
                    }
                    // ===== INTRINSIC REGION END: INT_BITS =====

                    // ===== INTRINSIC REGION BEGIN: LONG_BITS =====
                    // java.lang.Long bit ops (Phase 1b). Each long operand
                    // occupies one 64-bit JIT stack slot; load_slot_to_reg
                    // loads the full 64 bits. All instructions below are
                    // REX.W-prefixed (0x48) so they operate on the whole
                    // 64-bit value. bitCount/numberOfLeadingZeros/
                    // numberOfTrailingZeros return an int (0..=64) which
                    // is left in EAX with the upper 32 bits cleared.
                    else if callee_entry == crate::JitIntrinsic::LongBitCount.as_entry() {
                        // Long.bitCount(j): 64-bit POPCNT. Matcher gates
                        // this on has_popcnt(), so the instruction is
                        // always valid here. Result 0..=64 fits in EAX.
                        let arg = self.pop_stack();
                        self.load_slot_to_reg(RAX, arg);
                        // POPCNT RAX, RAX: F3 48 0F B8 C0
                        self.buf.emit(&[0xF3, 0x48, 0x0F, 0xB8, 0xC0]);
                        self.push_from_rax();
                    } else if callee_entry
                        == crate::JitIntrinsic::LongNumberOfLeadingZeros.as_entry()
                    {
                        // Long.numberOfLeadingZeros(j).
                        let arg = self.pop_stack();
                        self.load_slot_to_reg(RAX, arg);
                        if crate::x64::has_lzcnt() {
                            // LZCNT RAX, RAX: F3 48 0F BD C0 — defined to
                            // yield 64 for a zero input, matching the JDK.
                            self.buf.emit(&[0xF3, 0x48, 0x0F, 0xBD, 0xC0]);
                        } else {
                            // BSR fallback. BSR RCX, RAX sets ZF iff the
                            // source is zero and otherwise leaves the
                            // highest set-bit index (0..=63) in RCX.
                            //   nlz = 63 - index   (for a non-zero input)
                            //   nlz = 64           (for a zero input)
                            // 63 - index == index ^ 63 for index in 0..=63,
                            // computed with XOR so RAX is left untouched
                            // for the TEST that re-derives the zero case.
                            // BSR RCX, RAX: 48 0F BD C8
                            self.buf.emit(&[0x48, 0x0F, 0xBD, 0xC8]);
                            // XOR ECX, 63: 83 F1 3F — ECX = 63 - index
                            // (garbage if input was 0; fixed up below).
                            self.buf.emit(&[0x83, 0xF1, 0x3F]);
                            // MOV EDX, 64: BA 40 00 00 00
                            self.buf.emit(&[0xBA]);
                            self.buf.emit(&64i32.to_le_bytes());
                            // TEST RAX, RAX: 48 85 C0 — ZF iff input == 0.
                            self.buf.emit(&[0x48, 0x85, 0xC0]);
                            // CMOVZ RCX, RDX: 48 0F 44 CA — input 0 → 64.
                            self.buf.emit(&[0x48, 0x0F, 0x44, 0xCA]);
                            // MOV EAX, ECX: 89 C8
                            self.buf.emit(&[0x89, 0xC8]);
                        }
                        self.push_from_rax();
                    } else if callee_entry
                        == crate::JitIntrinsic::LongNumberOfTrailingZeros.as_entry()
                    {
                        // Long.numberOfTrailingZeros(j).
                        let arg = self.pop_stack();
                        self.load_slot_to_reg(RAX, arg);
                        if crate::x64::has_bmi1() {
                            // TZCNT RAX, RAX: F3 48 0F BC C0 — defined to
                            // yield 64 for a zero input, matching the JDK.
                            self.buf.emit(&[0xF3, 0x48, 0x0F, 0xBC, 0xC0]);
                        } else {
                            // BSF fallback. BSF RCX, RAX sets ZF iff the
                            // source is zero and otherwise leaves the
                            // lowest set-bit index (0..=63) in RCX, which
                            // is exactly ntz for a non-zero input. For a
                            // zero input the result must be 64.
                            // BSF RCX, RAX: 48 0F BC C8
                            self.buf.emit(&[0x48, 0x0F, 0xBC, 0xC8]);
                            // MOV EDX, 64: BA 40 00 00 00
                            self.buf.emit(&[0xBA]);
                            self.buf.emit(&64i32.to_le_bytes());
                            // TEST RAX, RAX: 48 85 C0 — ZF iff input == 0.
                            self.buf.emit(&[0x48, 0x85, 0xC0]);
                            // CMOVZ RCX, RDX: 48 0F 44 CA — input 0 → 64.
                            self.buf.emit(&[0x48, 0x0F, 0x44, 0xCA]);
                            // MOV EAX, ECX: 89 C8
                            self.buf.emit(&[0x89, 0xC8]);
                        }
                        self.push_from_rax();
                    } else if callee_entry == crate::JitIntrinsic::LongReverseBytes.as_entry() {
                        // Long.reverseBytes(j): 64-bit BSWAP.
                        let arg = self.pop_stack();
                        self.load_slot_to_reg(RAX, arg);
                        // BSWAP RAX: 48 0F C8
                        self.buf.emit(&[0x48, 0x0F, 0xC8]);
                        self.push_from_rax();
                    } else if callee_entry == crate::JitIntrinsic::LongHighestOneBit.as_entry() {
                        // Long.highestOneBit(j): 1L << bitIndex of the MSB,
                        // or 0 for a zero input. BSR leaves the index in
                        // RCX; SHL forms the mask; a CMOVZ keyed on the
                        // original input restores 0 for the zero case.
                        let arg = self.pop_stack();
                        self.load_slot_to_reg(RAX, arg);
                        // BSR RCX, RAX: 48 0F BD C8 — RCX = MSB index.
                        self.buf.emit(&[0x48, 0x0F, 0xBD, 0xC8]);
                        // MOV EDX, 1: BA 01 00 00 00 (RDX = 1, upper bits 0).
                        self.buf.emit(&[0xBA]);
                        self.buf.emit(&1i32.to_le_bytes());
                        // SHL RDX, CL: 48 D3 E2 — RDX = 1 << index
                        // (garbage if input was 0; fixed up below).
                        self.buf.emit(&[0x48, 0xD3, 0xE2]);
                        // XOR ECX, ECX: 31 C9 — RCX = 0 (zero-input result).
                        self.buf.emit(&[0x31, 0xC9]);
                        // TEST RAX, RAX: 48 85 C0 — ZF iff input == 0.
                        self.buf.emit(&[0x48, 0x85, 0xC0]);
                        // CMOVZ RDX, RCX: 48 0F 44 D1 — input 0 → 0.
                        self.buf.emit(&[0x48, 0x0F, 0x44, 0xD1]);
                        // MOV RAX, RDX: 48 89 D0
                        self.buf.emit(&[0x48, 0x89, 0xD0]);
                        self.push_from_rax();
                    } else if callee_entry == crate::JitIntrinsic::LongLowestOneBit.as_entry() {
                        // Long.lowestOneBit(j): j & -j. Naturally yields 0
                        // for a zero input, matching the JDK.
                        let arg = self.pop_stack();
                        self.load_slot_to_reg(RAX, arg);
                        // MOV RCX, RAX: 48 89 C1
                        self.buf.emit(&[0x48, 0x89, 0xC1]);
                        // NEG RCX: 48 F7 D9 — RCX = -j
                        self.buf.emit(&[0x48, 0xF7, 0xD9]);
                        // AND RAX, RCX: 48 21 C8 — RAX = j & -j
                        self.buf.emit(&[0x48, 0x21, 0xC8]);
                        self.push_from_rax();
                    } else if callee_entry == crate::JitIntrinsic::LongCompare.as_entry() {
                        // Long.compare(x, y): branchless signed
                        // (x > y) - (x < y). The stack holds x (deeper)
                        // then y. SETcc reads the flags from CMP without
                        // disturbing them; the int result lands in EAX.
                        let y = self.pop_stack();
                        let x = self.pop_stack();
                        self.load_slot_to_reg(RAX, x);
                        self.load_slot_to_reg(RCX, y);
                        // CMP RAX, RCX: 48 39 C8 — signed 64-bit compare.
                        self.buf.emit(&[0x48, 0x39, 0xC8]);
                        // SETG AL:  0F 9F C0 — AL = 1 if x > y.
                        self.buf.emit(&[0x0F, 0x9F, 0xC0]);
                        // SETL DL:  0F 9C C2 — DL = 1 if x < y.
                        self.buf.emit(&[0x0F, 0x9C, 0xC2]);
                        // MOVZX EAX, AL: 0F B6 C0
                        self.buf.emit(&[0x0F, 0xB6, 0xC0]);
                        // MOVZX EDX, DL: 0F B6 D2
                        self.buf.emit(&[0x0F, 0xB6, 0xD2]);
                        // SUB EAX, EDX: 29 D0 → -1, 0 or 1.
                        self.buf.emit(&[0x29, 0xD0]);
                        // MOVSXD RAX, EAX: `-1` came out of the 32-bit SUB as
                        // 0x00000000_FFFFFFFF. An `int` slot is held
                        // sign-extended, and the inline mini-emitter treats
                        // `i2l` as a no-op on that promise.
                        self.buf.emit(&[0x48, 0x63, 0xC0]);
                        self.push_from_rax();
                    } else if callee_entry == crate::JitIntrinsic::LongRotateLeft.as_entry()
                        || callee_entry == crate::JitIntrinsic::LongRotateRight.as_entry()
                    {
                        // Long.rotateLeft(i, distance) / rotateRight:
                        // ROL/ROR RAX, CL. x86 masks CL & 0x3f for a 64-bit
                        // rotate, byte-identical to the JDK (rotation mod 64).
                        // Descriptor is (JI)J: the long value (deeper) then
                        // the int distance (top), each one JIT stack slot.
                        let dist = self.pop_stack();
                        let val = self.pop_stack();
                        self.load_slot_to_reg(RAX, val);
                        self.load_slot_to_reg(RCX, dist);
                        // ROL RAX, CL: 48 D3 C0 ; ROR RAX, CL: 48 D3 C8.
                        let modrm =
                            if callee_entry == crate::JitIntrinsic::LongRotateLeft.as_entry() {
                                0xC0u8
                            } else {
                                0xC8u8
                            };
                        self.buf.emit(&[0x48, 0xD3, modrm]);
                        self.push_from_rax();
                    }
                    // ===== INTRINSIC REGION END: LONG_BITS =====

                    // ===== INTRINSIC REGION BEGIN: ARRAYCOPY =====
                    //
                    // De-spec guard: a call site whose src/dst are ALWAYS
                    // a reference-element array (e.g. `char[][]`) fails
                    // Guard 4 below on every single invocation, so this
                    // speculative fast path deopts every call. Each deopt
                    // evicts the artifact and triggers a background
                    // recompile that re-emits the SAME unconditional
                    // guard, so the method deopts forever — creating a
                    // continuous compile/evict race window. If the
                    // resume's epoch-staleness check ever loses that race
                    // (the live epoch was bumped by an in-flight
                    // recompile), the deopt falls back to the documented
                    // whole-method re-run, which DOUBLE-EXECUTES every
                    // side effect the method already performed before
                    // reaching this call (e.g. a `stack[ptr--]` decrement
                    // already committed to the heap) — the mechanism
                    // behind the JDT `Parser` stack-corruption bug
                    // (jasper-jdt-parser-arrayindexoutofbounds.md).
                    // `real_frame_deopt_resume_and_despeculate` already
                    // records this bci in the de-spec registry after
                    // `PER_BCI_DESPEC_LIMIT` deopts, exactly like the
                    // loop-header speculative-BCE guards (see
                    // `DespecRegistry::contains` above); this intrinsic just
                    // needs to honor it on recompile — bail to the generic
                    // (non-speculative) call dispatch below instead of
                    // re-emitting a guard proven to always fail.
                    else if callee_entry == crate::JitIntrinsic::ArraycopyPrimitive.as_entry()
                        && !self
                            .despec
                            .as_ref()
                            .is_some_and(|registry| registry.contains(&self.method_key, pc as u32))
                    {
                        // Phase 2 — System.arraycopy(src, srcPos, dst,
                        // dstPos, len). The descriptor is type-erased;
                        // the element kind is only known at runtime.
                        //
                        // Strategy (roadmap §3.3/§3.4): inline a
                        // primitive fast path. The emitted code performs
                        // a runtime dispatch — null checks, an
                        // is-array + same-primitive-element-kind check,
                        // and the five fused bounds checks of
                        // `native_system_arraycopy`. Whenever ANY guard
                        // is unsatisfied — null receiver, non-array,
                        // reference array, mismatched/incompatible
                        // element kinds, or an out-of-bounds position —
                        // control branches to the uncommon-trap deopt
                        // stub (DEOPT_REASON_BOUNDS_CHECK = 2). That
                        // re-runs the whole method in the interpreter,
                        // which dispatches `System.arraycopy` through
                        // the native registry. The native impl throws
                        // NullPointerException / ArrayStoreException /
                        // ArrayIndexOutOfBoundsException and applies the
                        // GC store barrier exactly — so correctness for
                        // every bailed case is delegated verbatim and
                        // reference arrays are never inlined.
                        //
                        // The proven-safe primitive case copies via
                        // `REP MOVSB` with memmove semantics: when src
                        // and dst are the SAME array and dstPos > srcPos
                        // the regions may overlap forward, so the copy
                        // runs backward (STD) in that case and forward
                        // (CLD) otherwise. Distinct arrays are distinct
                        // allocations and never overlap.
                        //
                        // Operand stack (deepest first): src, srcPos,
                        // dst, dstPos, len.
                        self.flush_scratch_registers();
                        // Step 6: snapshot the pre-pop JVM operand stack
                        // (src, srcPos, dst, dstPos, len) so a bail from
                        // any null/bounds guard resumes precisely at this
                        // invokestatic bci rather than re-running from entry.
                        if crate::deopt_real_enabled() {
                            self.snapshot_pre_intrinsic_call(
                                pc,
                                crate::deopt::DeoptReason::BoundsCheck,
                            );
                        }
                        let len_slot = self.pop_stack();
                        let dst_pos_slot = self.pop_stack();
                        let dst_slot = self.pop_stack();
                        let src_pos_slot = self.pop_stack();
                        let src_slot = self.pop_stack();

                        // Pin all five operands into owned frame scratch
                        // slots. After the five pops `next_spill_offset`
                        // sits below the slots the operands occupied, so
                        // these offsets are guaranteed in-frame (the
                        // call site had >=5 operand-stack entries, hence
                        // max_stack >= 5). They are scratch-only:
                        // arraycopy pushes nothing, so the next bytecode
                        // re-allocates spill slots from the same base.
                        //
                        // The base is pushed BELOW every operand's own
                        // frame home, so the five stores below cannot land
                        // on a slot one of them still lives in.
                        //
                        // The overlap is real and this is where it comes
                        // from: `pop_stack` reclaims a Frame slot that sits
                        // at the top of the spill region, so the five pops
                        // just above rewound `next_spill_offset` back OVER
                        // the very homes `flush_scratch_registers` spilled
                        // the oop operands into. Taking the base from
                        // `next_spill_offset` therefore aliases them by
                        // construction.
                        //
                        // The emitter used to work around that for its OWN
                        // reads only, by loading all five into distinct
                        // GPRs before storing any (kept below — it costs
                        // nothing and defends the ordering directly). But
                        // the aliasing has a SECOND consumer that the
                        // workaround does not reach: the deopt snapshot
                        // taken above names each operand's ORIGINAL frame
                        // home, and reads it when the bail actually fires —
                        // long after `s_src_pos`'s store has overwritten
                        // `dst`'s home with srcPos. A reference-array
                        // `arraycopy` (which always bails here, by design)
                        // then resumed in the interpreter with `Object(1)`
                        // — the literal srcPos — where `dst` belonged: not
                        // a plausible heap pointer, degraded to null by
                        // `CompactValue::to_value`, and `System.arraycopy`
                        // threw NullPointerException. `RMethodSiteCache`'s
                        // `mixedRefAndPrimitive` is the witness; the bogus
                        // payload tracks srcPos exactly (`srcPos=2` gives
                        // `Object(2)`).
                        //
                        // Removing the overlap fixes both consumers at
                        // once, which is why it is done here rather than by
                        // teaching the snapshot about the scratch homes.
                        let mut scratch_base = self.next_spill_offset;
                        for slot in [src_slot, src_pos_slot, dst_slot, dst_pos_slot, len_slot] {
                            if let crate::x64::StackSlot::Frame(off) = slot {
                                scratch_base = scratch_base.max(off.saturating_add(8));
                            }
                        }
                        if !self.spill_range_fits(scratch_base, 5) {
                            // Named here rather than inside the probe: this
                            // is the arraycopy intrinsic's five scratch
                            // homes, and a bail site of
                            // `spill-range-exhausted` said only that some
                            // range somewhere did not fit.
                            self.fail("singlepass-codegen/arraycopy-scratch-spill-exhausted");
                            return WalkStep::Return(false);
                        }
                        let s_src = scratch_base;
                        let s_src_pos = scratch_base + 8;
                        let s_dst = scratch_base + 16;
                        let s_dst_pos = scratch_base + 24;
                        let s_len = scratch_base + 32;
                        // ALIASING HAZARD: these scratch homes can overlap
                        // the operands' OWN frame homes. `flush_scratch_
                        // registers()` above spills any CalleeSaved *oop*
                        // operand (here src and dst) to frame slots taken
                        // from `next_spill_offset`; the five pops then rewind
                        // `next_spill_offset` back over those very slots, so
                        // e.g. `src_slot`/`dst_slot` may be `Frame(s_src)` /
                        // `Frame(s_src_pos)`. Writing the scratch homes in
                        // operand order would corrupt a not-yet-read operand:
                        // storing s_src_pos (=srcPos) overwrites dst's spilled
                        // home BEFORE s_dst reads it, leaving s_dst = srcPos
                        // (a small int) — guard-2 then bails to native when
                        // srcPos==0, or dereferences the bogus pointer and
                        // SIGSEGVs when srcPos!=0. Defeat the aliasing by
                        // loading ALL five operands into distinct scratch
                        // GPRs FIRST, then storing. RAX/RCX/RDX/R10/R11 are
                        // never local-mapped (LOCAL_REGS is RBX/R12..R15
                        // [+RSI/RDI on SysV]) and hold no deferred-Scratch
                        // value after the flush, so no load can clobber an
                        // operand still pending a read.
                        self.load_slot_to_reg(RAX, src_slot);
                        self.load_slot_to_reg(RCX, src_pos_slot);
                        self.load_slot_to_reg(RDX, dst_slot);
                        self.load_slot_to_reg(R10, dst_pos_slot);
                        self.load_slot_to_reg(R11, len_slot);
                        self.emit_store_local(s_src, RAX);
                        self.emit_store_local(s_src_pos, RCX);
                        self.emit_store_local(s_dst, RDX);
                        self.emit_store_local(s_dst_pos, R10);
                        self.emit_store_local(s_len, R11);

                        // Collect every "bail to native" branch patch
                        // here; they are all wired to one shared deopt
                        // stub (reason 2) after the inline body.
                        let mut bail_patches: Vec<usize> = Vec::new();

                        // --- Guard 1: src != null ---
                        self.emit_load_local(RAX, s_src);
                        self.emit_test_r64_r64(RAX);
                        bail_patches.push(self.emit_jcc_rel32_patch(0x84)); // JZ

                        // --- Guard 2: dst != null ---
                        self.emit_load_local(RCX, s_dst);
                        self.emit_test_r64_r64(RCX);
                        bail_patches.push(self.emit_jcc_rel32_patch(0x84)); // JZ

                        // --- Guards 3 and 4: both are arrays, of the SAME
                        // PRIMITIVE element kind ---
                        //
                        // `kind` and `element_type` are BOTH in the one
                        // byte at `KIND_TAGS_BYTE_OFFSET` — `kind` in bits
                        // 0..2 (`KIND_TAG_BYTE_MASK`), `element_type` in
                        // bits 2..6. They used to be separate bytes at
                        // offsets 4 and 5, and this code still read those
                        // two literals after the header shrank 24 -> 16 on
                        // 2026-08-07 and the quartet moved into the mark
                        // word.
                        //
                        // Offset 4 is now `shape` — an ARRAY'S LENGTH. The
                        // same function reads the length from that very
                        // offset (via `ARRAY_LENGTH_OFFSET`) forty lines
                        // below, so the "is this an array" guard was
                        // testing the length's low byte and the "element
                        // type" was the next length byte. That does not
                        // fail safe: for a length whose low byte is 1 and
                        // whose second byte is >= 4 — 1025 = 0x0401 — both
                        // tests PASS and the element width comes out as
                        // `1 << ((4 - 4) & 3)` = one byte. `arraycopy` on a
                        // `long[1025]` moved 1025 bytes instead of 8200 and
                        // returned normally: no exception, no crash, a
                        // silently truncated copy.
                        // `probes/ArraycopyHeaderOffsetProbe.java` is the
                        // repro — mismatch at index 128 for `long[1025]`
                        // and 256 for `int[1025]`, while 1024 / 300 / 257
                        // pass, which is why this hid.
                        //
                        // Read the constants, as every other header access
                        // in this file already does.
                        const KIND_TAGS: u8 = cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8;

                        // MOVZX EDX, BYTE [RAX + KIND_TAGS]  (src tags)
                        self.buf.emit(&[0x0F, 0xB6, 0x50, KIND_TAGS]);
                        // MOV R10D, EDX — keep the whole byte; EDX is about
                        // to be masked down to the kind bits.
                        self.buf.emit(&[0x41, 0x89, 0xD2]);
                        // AND EDX, KIND_TAG_BYTE_MASK
                        self.buf
                            .emit(&[0x83, 0xE2, cratonvm_types::KIND_TAG_BYTE_MASK]);
                        // CMP EDX, ObjectKind::Array
                        self.buf
                            .emit(&[0x83, 0xFA, cratonvm_types::ObjectKind::Array as u8]);
                        bail_patches.push(self.emit_jcc_rel32_patch(0x85)); // JNE

                        // MOVZX EDX, BYTE [RCX + KIND_TAGS]  (dst tags)
                        self.buf.emit(&[0x0F, 0xB6, 0x51, KIND_TAGS]);
                        // MOV R11D, EDX
                        self.buf.emit(&[0x41, 0x89, 0xD3]);
                        self.buf
                            .emit(&[0x83, 0xE2, cratonvm_types::KIND_TAG_BYTE_MASK]);
                        self.buf
                            .emit(&[0x83, 0xFA, cratonvm_types::ObjectKind::Array as u8]);
                        bail_patches.push(self.emit_jcc_rel32_patch(0x85)); // JNE

                        // element_type = (tags >> 2) & 0xF. ArrayElementType:
                        // Reference=0, Boolean=4, Char=5, Float=6, Double=7,
                        // Byte=8, Short=9, Int=10, Long=11 — four bits.
                        // SHR R10D, 2 ; AND R10D, 0xF   (src)
                        self.buf.emit(&[0x41, 0xC1, 0xEA, 0x02]);
                        self.buf.emit(&[0x41, 0x83, 0xE2, 0x0F]);
                        // SHR R11D, 2 ; AND R11D, 0xF   (dst)
                        self.buf.emit(&[0x41, 0xC1, 0xEB, 0x02]);
                        self.buf.emit(&[0x41, 0x83, 0xE3, 0x0F]);
                        // CMP R10D, R11D → element kinds must be equal
                        self.buf.emit(&[0x45, 0x39, 0xDA]);
                        bail_patches.push(self.emit_jcc_rel32_patch(0x85)); // JNE
                                                                            // CMP R10D, 4 → primitive kinds are 4..=11; a
                                                                            // value < 4 means Reference (0) — bail (the GC
                                                                            // store barrier / ArrayStoreException make
                                                                            // reference copies unsafe to inline).
                        self.buf.emit(&[0x41, 0x83, 0xFA, 0x04]);
                        bail_patches.push(self.emit_jcc_rel32_patch(0x82)); // JB (unsigned <)
                                                                            // MOV EDX, R10D — the shift math below operates
                                                                            // on EDX, as it did when EDX held the element
                                                                            // type directly.
                        self.buf.emit(&[0x44, 0x89, 0xD2]);

                        // shift = (element_type - 4) & 3, where
                        //   width == 1 << shift  for every primitive
                        //   kind (verified: 4→0,5→1,6→2,7→3,8→0,9→1,
                        //   10→2,11→3). Stash the shift in R11 (scratch,
                        //   untouched until the copy below).
                        // SUB EDX, 4
                        self.buf.emit(&[0x83, 0xEA, 0x04]);
                        // AND EDX, 3
                        self.buf.emit(&[0x83, 0xE2, 0x03]);
                        // MOV R11D, EDX
                        self.buf.emit(&[0x41, 0x89, 0xD3]);

                        // --- Guard 5: bounds checks ---
                        // Load srcPos, dstPos, len sign-extended to
                        // 64-bit so the pos+len additions cannot
                        // overflow.
                        // srcPos → RAX
                        self.emit_load_local(RAX, s_src_pos);
                        self.buf.emit(&[0x48, 0x63, 0xC0]); // MOVSXD RAX, EAX
                                                            // dstPos → RDX
                        self.emit_load_local(RDX, s_dst_pos);
                        self.buf.emit(&[0x48, 0x63, 0xD2]); // MOVSXD RDX, EDX
                                                            // len → RCX
                        self.emit_load_local(RCX, s_len);
                        self.buf.emit(&[0x48, 0x63, 0xC9]); // MOVSXD RCX, ECX

                        // srcPos < 0 ?  TEST RAX,RAX; JS bail
                        self.emit_test_r64_r64(RAX);
                        bail_patches.push(self.emit_jcc_rel32_patch(0x88)); // JS
                                                                            // dstPos < 0 ?  TEST RDX,RDX; JS bail
                        self.emit_test_r64_r64(RDX);
                        bail_patches.push(self.emit_jcc_rel32_patch(0x88)); // JS
                                                                            // len < 0 ?  TEST RCX,RCX; JS bail
                        self.emit_test_r64_r64(RCX);
                        bail_patches.push(self.emit_jcc_rel32_patch(0x88)); // JS

                        // srcPos + len > src.length ?
                        // R10 = srcPos + len
                        self.buf.emit(&[0x49, 0x89, 0xC2]); // MOV R10, RAX
                        self.buf.emit(&[0x49, 0x01, 0xCA]); // ADD R10, RCX
                                                            // RAX = src ptr; EAX = src.length (zero-extended,
                                                            // so the 64-bit value is non-negative).
                        self.emit_load_local(RAX, s_src);
                        // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                        self.buf.emit(&[0x8B, 0x40, ARRAY_LENGTH_OFFSET as u8]); // MOV EAX,[RAX+12]
                                                                                 // CMP R10, RAX  (srcPos+len vs src.length)
                        self.buf.emit(&[0x49, 0x39, 0xC2]);
                        bail_patches.push(self.emit_jcc_rel32_patch(0x8F)); // JG (signed >)

                        // dstPos + len > dst.length ?
                        self.buf.emit(&[0x49, 0x89, 0xD2]); // MOV R10, RDX
                        self.buf.emit(&[0x49, 0x01, 0xCA]); // ADD R10, RCX
                        self.emit_load_local(RAX, s_dst);
                        // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                        self.buf.emit(&[0x8B, 0x40, ARRAY_LENGTH_OFFSET as u8]); // MOV EAX,[RAX+12]
                        self.buf.emit(&[0x49, 0x39, 0xC2]); // CMP R10, RAX
                        bail_patches.push(self.emit_jcc_rel32_patch(0x8F)); // JG

                        // --- len == 0 fast exit ---
                        // All bounds are validated; an empty copy is a
                        // no-op. RCX still holds the sign-extended len.
                        self.emit_test_r64_r64(RCX);
                        let zero_len_skip = self.emit_jcc_rel32_patch(0x84); // JZ → done

                        // --- compute byte addresses & count ---
                        // Preserve RSI / RDI: on Windows they are
                        // callee-saved AND used by the local register
                        // allocator (LOCAL_REGS), so they may hold live
                        // locals. PUSH/POP brackets the REP MOVSB; no
                        // CALL occurs in between, so RSP stays balanced.
                        // PUSH RSI ; PUSH RDI
                        self.buf.emit(&[0x56, 0x57]);

                        // shift → CL
                        // MOV ECX, R11D
                        self.buf.emit(&[0x44, 0x89, 0xD9]);

                        // srcAddr = src + HEADER_SIZE + (srcPos << shift)
                        self.emit_load_local(RSI, s_src_pos);
                        self.buf.emit(&[0x48, 0x63, 0xF6]); // MOVSXD RSI, ESI
                        self.buf.emit(&[0x48, 0xD3, 0xE6]); // SHL RSI, CL
                        self.emit_load_local(RAX, s_src);
                        self.buf.emit(&[0x48, 0x01, 0xC6]); // ADD RSI, RAX
                                                            // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                        self.buf.emit(&[0x48, 0x83, 0xC6, HEADER_SIZE as u8]); // ADD RSI, HEADER_SIZE

                        // dstAddr = dst + HEADER_SIZE + (dstPos << shift)
                        self.emit_load_local(RDI, s_dst_pos);
                        self.buf.emit(&[0x48, 0x63, 0xFF]); // MOVSXD RDI, EDI
                        self.buf.emit(&[0x48, 0xD3, 0xE7]); // SHL RDI, CL
                        self.emit_load_local(RAX, s_dst);
                        self.buf.emit(&[0x48, 0x01, 0xC7]); // ADD RDI, RAX
                                                            // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                        self.buf.emit(&[0x48, 0x83, 0xC7, HEADER_SIZE as u8]); // ADD RDI, HEADER_SIZE

                        // byteCount = len << shift  → RDX (kept for the
                        // backward-copy adjust, then copied into RCX
                        // for REP).
                        self.emit_load_local(RDX, s_len);
                        self.buf.emit(&[0x48, 0x63, 0xD2]); // MOVSXD RDX, EDX
                        self.buf.emit(&[0x48, 0xD3, 0xE2]); // SHL RDX, CL

                        // --- direction select ---
                        // Overlap is possible only within the SAME
                        // array; distinct arrays are distinct heap
                        // allocations. Copy backward iff
                        //   src_ptr == dst_ptr && dstPos > srcPos
                        //   && destination starts before the source range ends.
                        // Otherwise forward is always memmove-correct.
                        self.emit_load_local(RAX, s_src);
                        self.emit_load_local(RCX, s_dst);
                        // CMP RAX, RCX
                        self.buf.emit(&[0x48, 0x39, 0xC8]);
                        let fwd_if_diff = self.emit_jcc_rel32_patch(0x85); // JNE → forward
                                                                           // same array: compare dstPos vs srcPos (signed
                                                                           // 32-bit; both already validated >= 0).
                        self.emit_load_local(RAX, s_dst_pos);
                        self.emit_load_local(RCX, s_src_pos);
                        // CMP EAX, ECX  (dstPos vs srcPos)
                        self.buf.emit(&[0x39, 0xC8]);
                        let fwd_if_le = self.emit_jcc_rel32_patch(0x8E); // JLE → forward

                        // A same-array copy to its right is still disjoint
                        // when dst >= src + byteCount. Snappy's one-byte
                        // self-copies hit this case hundreds of millions of
                        // times; reverse REP MOVSB was needlessly selected.
                        // RSI/RDI are validated payload pointers and RDX is
                        // the checked byte count, so the source end cannot
                        // overflow the array's allocation.
                        self.buf.emit(&[0x48, 0x8D, 0x04, 0x16]); // LEA RAX, [RSI + RDX]
                        self.buf.emit(&[0x48, 0x39, 0xC7]); // CMP RDI, RAX
                        let fwd_if_disjoint = self.emit_jcc_rel32_patch(0x83); // JAE
                                                                               // JIT round 20 wave 15 (lane fuzz14d; correctness,
                                                                               // unconditional): the copy moves WHOLE elements --
                                                                               // `REP MOVSW/MOVSD/MOVSQ` by the element width in
                                                                               // R11 (the shift), `REP MOVSB` only for 1-byte
                                                                               // elements -- as HotSpot's `jlong`/`jint`/`jshort`
                                                                               // atomic arraycopy stubs do. `REP MOVSB` (and the
                                                                               // byte-at-a-time `STD` backward form above all) may
                                                                               // move one 2/4/8-byte element in pieces, so a thread
                                                                               // writing the source was observed with TORN elements
                                                                               // (`R20FuzzGTearCopy1` `tear.copyof`). Each width arm
                                                                               // is `[CMP R11D, shift; JNE next]`, then its copy.
                                                                               // (shift, width, REP MOVS* encoding)
                        const MOVS_BY_WIDTH: [(u8, u8, &[u8]); 4] = [
                            (3, 8, &[0xF3, 0x48, 0xA5]), // REP MOVSQ
                            (2, 4, &[0xF3, 0xA5]),       // REP MOVSD
                            (1, 2, &[0x66, 0xF3, 0xA5]), // REP MOVSW
                            (0, 1, &[0xF3, 0xA4]),       // REP MOVSB
                        ];
                        let mut copy_done: Vec<usize> = Vec::with_capacity(8);

                        // --- backward copy (STD) ---
                        // Point RSI/RDI at the LAST ELEMENT of each region:
                        //   addr += byteCount - width.
                        for &(shift, width, movs) in MOVS_BY_WIDTH.iter() {
                            let next = (shift != 0).then(|| {
                                // CMP R11D, shift ; JNE next
                                self.buf.emit(&[0x41, 0x83, 0xFB, shift]);
                                self.emit_jcc_rel32_patch(0x85)
                            });
                            let disp = (width as i8).wrapping_neg() as u8;
                            // LEA RSI, [RSI + RDX - width]
                            self.buf.emit(&[0x48, 0x8D, 0x74, 0x16, disp]);
                            // LEA RDI, [RDI + RDX - width]
                            self.buf.emit(&[0x48, 0x8D, 0x7C, 0x17, disp]);
                            // MOV RCX, RDX ; SHR RCX, shift  (element count)
                            self.buf.emit(&[0x48, 0x89, 0xD1]);
                            if shift != 0 {
                                self.buf.emit(&[0x48, 0xC1, 0xE9, shift]);
                            }
                            // STD ; REP MOVS* ; CLD
                            self.buf.emit(&[0xFD]);
                            self.buf.emit(movs);
                            self.buf.emit(&[0xFC]);
                            copy_done.push(self.emit_jmp_rel32_patch());
                            if let Some(next) = next {
                                self.patch_rel32_to_here(next);
                            }
                        }

                        // --- forward copy (CLD) ---
                        self.patch_rel32_to_here(fwd_if_diff);
                        self.patch_rel32_to_here(fwd_if_le);
                        self.patch_rel32_to_here(fwd_if_disjoint);
                        for &(shift, _width, movs) in MOVS_BY_WIDTH.iter() {
                            let next = (shift != 0).then(|| {
                                // CMP R11D, shift ; JNE next
                                self.buf.emit(&[0x41, 0x83, 0xFB, shift]);
                                self.emit_jcc_rel32_patch(0x85)
                            });
                            // MOV RCX, RDX ; SHR RCX, shift  (element count)
                            self.buf.emit(&[0x48, 0x89, 0xD1]);
                            if shift != 0 {
                                self.buf.emit(&[0x48, 0xC1, 0xE9, shift]);
                            }
                            // CLD ; REP MOVS*
                            self.buf.emit(&[0xFC]);
                            self.buf.emit(movs);
                            copy_done.push(self.emit_jmp_rel32_patch());
                            if let Some(next) = next {
                                self.patch_rel32_to_here(next);
                            }
                        }

                        // Every copy arm converges here.
                        for done in copy_done {
                            self.patch_rel32_to_here(done);
                        }
                        // POP RDI ; POP RSI
                        self.buf.emit(&[0x5F, 0x5E]);

                        // --- done ---
                        self.patch_rel32_to_here(zero_len_skip);

                        // The GPU input-residency barrier, for the
                        // DESTINATION array.
                        //
                        // This intrinsic writes a primitive array with
                        // `REP MOVSB` and no `*astore` opcode anywhere in
                        // the method, so `offload_jit_gate`'s bytecode
                        // scan -- which looks only for those seven
                        // opcodes -- never saw it. A method whose only
                        // array write is a `System.arraycopy` was
                        // therefore admitted to the JIT even while the
                        // gate was refusing every ordinary array writer,
                        // and its inline copy left the device mirror
                        // stale with nothing to notice. That predates the
                        // barrier and is not what the gate was widened
                        // for; it is the same root cause reached by a
                        // path the gate could not see.
                        //
                        // Reloaded into RAX from the pinned frame slot
                        // rather than kept in a register: RSI/RDI were
                        // just popped and RAX/RCX/RDX are the copy's own
                        // scratch. Emitted on the zero-length path too --
                        // it costs one over-eviction in a case that wrote
                        // nothing, and keeping the label single-exit is
                        // worth more than the branch that would avoid it.
                        self.emit_load_local(RAX, s_dst);
                        self.emit_gpu_input_cache_barrier();

                        // Every bail branch (null, non-array,
                        // reference-element array, mismatched kind, or
                        // out-of-bounds) is a NORMAL, valid outcome for
                        // `System.arraycopy` — either a real exception or
                        // a successful reference-array copy — not an
                        // "uncommon" condition that should re-run the
                        // method. Route them through the SAME safe
                        // native-dispatch CALL an ordinary
                        // non-intrinsic `invokestatic` site uses
                        // (registered for this exact pc alongside the
                        // intrinsic — see the `ArraycopyPrimitive` match
                        // arm in `jit_scan`'s caller), so a guard failure
                        // throws/succeeds via normal call semantics
                        // instead of a deopt trap whose only resume
                        // strategy (whole-method re-run) can
                        // DOUBLE-EXECUTE a side effect the method already
                        // performed before reaching this call (e.g. a
                        // `stack[ptr--]` decrement already committed to
                        // the heap) — the mechanism behind the JDT
                        // `Parser` stack-corruption bug (jasper-jdt-parser-arrayindexoutofbounds.md).
                        // Falls back to the historical deopt trap only if
                        // the dispatch info wasn't registered (defensive;
                        // should not happen for this intrinsic).
                        let dispatch_info = self
                            .invoke_info_idx
                            .get(&pc)
                            .map(|&i| self.invoke_info[i].1);
                        match dispatch_info.map(|info| {
                            (info, self.reserve_spill_slots(5, SpillReason::HelperArgs))
                        }) {
                            Some((info, Some(args_base))) => {
                                let skip_dispatch = self.emit_jmp_rel32_patch();
                                for &patch in &bail_patches {
                                    self.patch_rel32_to_here(patch);
                                }
                                // JIT r23 wave 3c, lane L5
                                // (`CRATONVM_JIT_SP_REF_ARRAYCOPY_DIRECT`,
                                // default ON): the bail edge -- every
                                // reference copy lands here -- calls the
                                // optimizing tier's `System.arraycopy` helper
                                // (`jit_system_arraycopy_direct`: the
                                // primitive and reference fast paths, then
                                // the same native dispatch this edge made)
                                // instead of the dispatch directly. Six
                                // integer arguments (context + the five
                                // operands), so the platform stack-argument
                                // shape the direct calls use.
                                let ref_helper = self.direct_helpers.system_arraycopy;
                                let via_helper =
                                    ref_helper != 0 && crate::sp_ref_arraycopy_direct_enabled();
                                if via_helper {
                                    let arg_slots = [
                                        StackSlot::Frame(s_src),
                                        StackSlot::Frame(s_src_pos),
                                        StackSlot::Frame(s_dst),
                                        StackSlot::Frame(s_dst_pos),
                                        StackSlot::Frame(s_len),
                                    ];
                                    self.emit_pre_safepoint_spill();
                                    let total_sub = self.emit_stack_arg_setup(&arg_slots, true);
                                    // NOT `assert_direct_helper_call_shape!`:
                                    // that macro is for HAND-WRITTEN register
                                    // call sites and fails the build for a
                                    // `JavaArgMarshalled` row with Win64 stack
                                    // arguments, which this one is (two on
                                    // Win64) -- and this site does marshal
                                    // them, through `emit_stack_arg_setup`.
                                    const _: () = {
                                        let sig = crate::direct_helpers::direct_helper_sig_of(
                                            "system_arraycopy",
                                        );
                                        assert!(sig.int_args() == 6 && sig.returns_value);
                                        assert!(matches!(
                                            sig.path,
                                            crate::direct_helpers::DirectHelperCallPath::JavaArgMarshalled
                                        ));
                                    };
                                    self.emit_call_absolute(ref_helper);
                                    self.emit_oop_map_for_safepoint();
                                    self.emit_stack_arg_cleanup(total_sub);
                                    self.emit_post_invoke_exception_check(b'V');
                                    self.next_spill_offset = args_base;
                                    self.patch_rel32_to_here(skip_dispatch);
                                }
                                if !via_helper {
                                // Reload the five original operands from
                                // their pinned scratch homes (untouched
                                // by the guard sequence above) into the
                                // freshly reserved, contiguous args
                                // buffer in the layout `jit_invoke_
                                // dispatch` expects: arg[0] at the
                                // highest offset (lowest address),
                                // arg[n-1] at the lowest offset —
                                // mirrors the generic invokestatic
                                // dispatch site's buffer construction.
                                // ALIASING HAZARD: `args_base` reuses the
                                // SAME frame region as `s_src..s_len`
                                // (arraycopy's scratch homes never
                                // advanced `next_spill_offset` — see the
                                // "arraycopy pushes nothing" comment
                                // above), and the target buffer order is
                                // the REVERSE of the scratch-home order,
                                // so a naive per-index load-then-store
                                // would overwrite a not-yet-read home
                                // (e.g. writing arg[0] to `s_len`'s
                                // address before arg[4] has read `s_len`).
                                // Defeat it exactly like the fast-path
                                // guard setup above: load ALL five
                                // operands into distinct registers FIRST,
                                // then store.
                                self.emit_load_local(RAX, s_src);
                                self.emit_load_local(RCX, s_src_pos);
                                self.emit_load_local(RDX, s_dst);
                                self.emit_load_local(R10, s_dst_pos);
                                self.emit_load_local(R11, s_len);
                                self.emit_store_local(args_base + 4 * 8, RAX); // Cast: x86-64 immediate encoding
                                self.emit_store_local(args_base + 3 * 8, RCX); // Cast: x86-64 immediate encoding
                                self.emit_store_local(args_base + 2 * 8, RDX); // Cast: x86-64 immediate encoding
                                self.emit_store_local(args_base + 1 * 8, R10); // Cast: x86-64 immediate encoding
                                self.emit_store_local(args_base, R11);
                                // SAFETY: info comes from self.invoke_info, which holds
                                // pointers to JitInvokeInfo structs kept alive by the
                                // caller for the duration of compilation.
                                self.emit_load_vm_context(ARG_REGS[0]);
                                self.emit_mov_imm64(ARG_REGS[1], info as *const _ as i64); // Cast: function pointer for JIT call target
                                let buf_start = args_base + 4 * 8; // Cast: x86-64 immediate encoding
                                self.emit_lea_frame_slot(ARG_REGS[2], buf_start);
                                self.emit_mov_imm32_sx(ARG_REGS[3], 5);
                                self.emit_pre_safepoint_spill_before_helper_call();
                                // (vm_ptr, info_ptr, args_ptr, num_args) -> result | sentinel.
                                cratonvm_jit_api::assert_helper_call_shape!(
                                    "invoke_dispatch",
                                    int_args = 4,
                                    returns_value = true
                                );
                                self.emit_call_absolute(self.helpers.invoke_dispatch);
                                self.emit_oop_map_for_safepoint();
                                self.emit_post_invoke_exception_check(b'V');
                                // arraycopy returns void: nothing pushed;
                                // the args buffer is dead, reclaim it.
                                self.next_spill_offset = args_base;
                                self.patch_rel32_to_here(skip_dispatch);
                                }
                            }
                            Some((_, None)) => {
                                // Spill region exhausted — bail the whole
                                // compile (always safe: the method falls
                                // back to the interpreter).
                                self.fail("singlepass-codegen/arraycopy-args-spill-exhausted");
                                return WalkStep::Return(false);
                            }
                            None => {
                                // Wire every bail branch to a shared deopt stub
                                // (reason 2 = DEOPT_REASON_BOUNDS_CHECK). The
                                // emit_deopt_stubs pass coalesces equal
                                // (bci, reason) pairs into one stub, so all the
                                // bail edges share a single trap.
                                for patch in bail_patches {
                                    self.deopt_stubs.push((patch, pc, 2));
                                }
                            }
                        }
                    }
                    // ===== INTRINSIC REGION END: ARRAYCOPY =====

                    // ===== INTRINSIC REGION BEGIN: ARRAYS_OPS =====
                    // java.util.Arrays.fill / Arrays.equals — Phase 4a.
                    //
                    // Array layout (cratonvm_types): a `HEADER_SIZE`-byte
                    // object header (16 today; this said 40, a layout two
                    // header shrinks old -- JIT r22 wave 2, lane intrinsics)
                    // with the i32 element count at `ARRAY_LENGTH_OFFSET`
                    // (8 today, not 12); element data begins at
                    // `HEADER_SIZE`. The code below names the constants,
                    // never the numbers. Primitive elements are packed at
                    // their natural width (byte=1, char/short=2, int=4,
                    // long=8).
                    //
                    // Register discipline: `flush_scratch_registers()`
                    // ran at the top of the 0xb8 handler, so no Java
                    // local lives in a scratch GPR — RAX/RCX/RDX/R8/R9/
                    // R10/R11 are all free to clobber. RDI and RSI ARE
                    // in `LOCAL_REGS` (Windows callee-saved), so the
                    // REP STOS/CMPS sequences PUSH/POP them to keep the
                    // owning locals intact. There is no CALL and no
                    // safepoint between the PUSH and the POP, so the
                    // transient RSP adjustment is invisible to the GC.
                    else if callee_entry == crate::JitIntrinsic::ArraysFill1.as_entry()
                        || callee_entry == crate::JitIntrinsic::ArraysFill2.as_entry()
                        || callee_entry == crate::JitIntrinsic::ArraysFill4.as_entry()
                        || callee_entry == crate::JitIntrinsic::ArraysFill8.as_entry()
                    {
                        // Arrays.fill(array, value) : void
                        //
                        // Operand stack (deepest first): [array, value].
                        // Pop value, then array.
                        let value_slot = self.pop_stack();
                        let array_slot = self.pop_stack();

                        // Array pointer → RAX for the null check.
                        self.load_slot_to_reg(RAX, array_slot);
                        // Null array → JDK throws NullPointerException.
                        // Reuse the shared null-check stub (sets
                        // JIT_PENDING_NPE, deopts out): TEST RAX,RAX +
                        // JZ stub. This is an intrinsic, not a plain
                        // array opcode, so no precise JEP-358 array action
                        // applies — record NONE (unmessaged NPE), matching
                        // the prior behaviour. With the site's
                        // `JitInvokeInfo` registered the null edge instead
                        // dispatches the real `Arrays.fill`, whose body throws
                        // HotSpot's NPE (`emit_arrays_intrinsic_null_dispatch`).
                        let fill_null_dispatch = self
                            .arrays_intrinsic_null_dispatch_info(pc, "fill")
                            .map(|info| {
                                self.emit_test_r64_r64(RAX);
                                (self.emit_jcc_rel32_patch(0x84), info) // JZ
                            });
                        if fill_null_dispatch.is_none() {
                            let trap_key =
                                crate::x64::inlining::record_npe_trap_site(self.orig_bci(pc));
                            self.emit_null_check_array_load(npe_action::NONE, trap_key, pc);
                        }

                        // Save array base in R8 (RAX is needed as the
                        // STOS source register).
                        // MOV R8, RAX  (49 89 C0)
                        self.buf.emit(&[0x49, 0x89, 0xC0]);
                        // Element count → ECX (zero-extends to RCX, the
                        // REP counter). MOV ECX, [RAX+len]  (8B 48 len; len = ARRAY_LENGTH_OFFSET, 8 today)
                        // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                        self.buf.emit(&[0x8B, 0x48, ARRAY_LENGTH_OFFSET as u8]);

                        // Fill value → RAX. STOSB/W/D/Q use AL/AX/EAX/
                        // RAX, all sub-registers of RAX, so a single
                        // 64-bit load serves every width.
                        self.load_slot_to_reg(RAX, value_slot);

                        // Save RDI (a Java local may live there), point
                        // it at the element data, run REP STOS, restore.
                        // PUSH RDI  (57)
                        self.buf.emit_byte(0x57);
                        // LEA RDI, [R8 + HEADER_SIZE]  (49 8D 78 hdr; hdr = 16 today)
                        // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                        self.buf.emit(&[0x49, 0x8D, 0x78, HEADER_SIZE as u8]);
                        if callee_entry == crate::JitIntrinsic::ArraysFill1.as_entry() {
                            // REP STOSB  (F3 AA)
                            self.buf.emit(&[0xF3, 0xAA]);
                        } else if callee_entry == crate::JitIntrinsic::ArraysFill2.as_entry() {
                            // REP STOSW  (66 F3 AB)
                            self.buf.emit(&[0x66, 0xF3, 0xAB]);
                        } else if callee_entry == crate::JitIntrinsic::ArraysFill4.as_entry() {
                            // REP STOSD  (F3 AB)
                            self.buf.emit(&[0xF3, 0xAB]);
                        } else {
                            // REP STOSQ  (F3 48 AB)
                            self.buf.emit(&[0xF3, 0x48, 0xAB]);
                        }
                        // POP RDI  (5F)
                        self.buf.emit_byte(0x5F);

                        // The GPU input-residency barrier. Same story as
                        // the `System.arraycopy` intrinsic above: this
                        // writes the whole array with `REP STOS` and no
                        // `*astore` opcode, so the gate's scan never saw
                        // the method as an array writer at all.
                        //
                        // The array base is still in R8, where this arm
                        // parked it before RAX became the STOS source.
                        // MOV RAX, R8  (4C 89 C0)
                        self.buf.emit(&[0x4C, 0x89, 0xC0]);
                        self.emit_gpu_input_cache_barrier();
                        if let Some((null_patch, info)) = fill_null_dispatch {
                            if !self.emit_arrays_intrinsic_null_dispatch(null_patch, info, 2) {
                                self.fail(
                                    "singlepass-codegen/arrays-fill-null-args-spill-exhausted",
                                );
                                return WalkStep::Return(false);
                            }
                        }

                        // void return — nothing pushed onto the operand
                        // stack.
                    } else if callee_entry == crate::JitIntrinsic::ArraysFillRange1.as_entry()
                        || callee_entry == crate::JitIntrinsic::ArraysFillRange2.as_entry()
                        || callee_entry == crate::JitIntrinsic::ArraysFillRange4.as_entry()
                        || callee_entry == crate::JitIntrinsic::ArraysFillRange8.as_entry()
                    {
                        // Arrays.fill(array, fromIndex, toIndex, value) : void
                        // Operand stack (deepest first): [array, fromIndex, toIndex, value].
                        let val_slot = self.pop_stack();
                        let to_slot = self.pop_stack();
                        let from_slot = self.pop_stack();
                        let array_slot = self.pop_stack();

                        let fill_info = self.arrays_intrinsic_null_dispatch_info(pc, "fill");
                        let args_base = if fill_info.is_some() {
                            let Some(base) = self.reserve_spill_slots(4, SpillReason::HelperArgs) else {
                                self.fail("singlepass-codegen/arrays-fill-range-args-spill-exhausted");
                                return WalkStep::Return(false);
                            };
                            Some(base)
                        } else {
                            None
                        };

                        self.load_slot_to_reg(RAX, array_slot);
                        self.emit_test_r64_r64(RAX);
                        let p_null = self.emit_jcc_rel32_patch(0x84); // JZ
                        // MOV R8, RAX (save array base)
                        self.buf.emit(&[0x49, 0x89, 0xC0]);

                        self.load_slot_to_reg(R9, from_slot);
                        // TEST R9D, R9D -> JS if fromIndex < 0
                        self.buf.emit(&[0x45, 0x85, 0xC9]);
                        let p_neg = self.emit_jcc_rel32_patch(0x88);

                        self.load_slot_to_reg(RCX, to_slot);
                        // CMP ECX, R9D -> JL if toIndex < fromIndex
                        self.buf.emit(&[0x44, 0x39, 0xC9]);
                        let p_gt = self.emit_jcc_rel32_patch(0x8C);

                        // MOV R10D, [R8 + ARRAY_LENGTH_OFFSET]
                        self.buf.emit(&[0x45, 0x8B, 0x50, ARRAY_LENGTH_OFFSET as u8]);
                        // CMP R10D, ECX -> JL if array.length < toIndex
                        self.buf.emit(&[0x41, 0x39, 0xCA]);
                        let p_oob = self.emit_jcc_rel32_patch(0x8C);

                        // SUB ECX, R9D (count = toIndex - fromIndex, zero-extended to RCX)
                        self.buf.emit(&[0x44, 0x29, 0xC9]);
                        // MOV R9D, R9D (zero-extend fromIndex to R9)
                        self.buf.emit(&[0x45, 0x89, 0xC9]);

                        self.load_slot_to_reg(RAX, val_slot);
                        self.buf.emit_byte(0x57); // PUSH RDI
                        let sib = if callee_entry == crate::JitIntrinsic::ArraysFillRange1.as_entry() {
                            0x08u8
                        } else if callee_entry == crate::JitIntrinsic::ArraysFillRange2.as_entry() {
                            0x48u8
                        } else if callee_entry == crate::JitIntrinsic::ArraysFillRange4.as_entry() {
                            0x88u8
                        } else {
                            0xC8u8
                        };
                        // LEA RDI, [R8 + R9*scale + HEADER_SIZE]
                        self.buf.emit(&[0x4B, 0x8D, 0x7C, sib, HEADER_SIZE as u8]);
                        if callee_entry == crate::JitIntrinsic::ArraysFillRange1.as_entry() {
                            self.buf.emit(&[0xF3, 0xAA]); // REP STOSB
                        } else if callee_entry == crate::JitIntrinsic::ArraysFillRange2.as_entry() {
                            self.buf.emit(&[0x66, 0xF3, 0xAB]); // REP STOSW
                        } else if callee_entry == crate::JitIntrinsic::ArraysFillRange4.as_entry() {
                            self.buf.emit(&[0xF3, 0xAB]); // REP STOSD
                        } else {
                            self.buf.emit(&[0xF3, 0x48, 0xAB]); // REP STOSQ
                        }
                        self.buf.emit_byte(0x5F); // POP RDI

                        // MOV RAX, R8
                        self.buf.emit(&[0x4C, 0x89, 0xC0]);
                        self.emit_gpu_input_cache_barrier();

                        if let (Some(info), Some(args_base)) = (fill_info, args_base) {
                            let skip = self.emit_jmp_rel32_patch();
                            for patch in [p_null, p_neg, p_gt, p_oob] {
                                self.patch_rel32_to_here(patch);
                            }
                            self.load_slot_to_reg(RAX, array_slot);
                            self.load_slot_to_reg(RCX, from_slot);
                            self.load_slot_to_reg(RDX, to_slot);
                            self.load_slot_to_reg(R10, val_slot);
                            self.emit_store_local(args_base + 3 * 8, RAX);
                            self.emit_store_local(args_base + 2 * 8, RCX);
                            self.emit_store_local(args_base + 1 * 8, RDX);
                            self.emit_store_local(args_base, R10);
                            self.emit_load_vm_context(ARG_REGS[0]);
                            self.emit_mov_imm64(ARG_REGS[1], info as i64);
                            self.emit_lea_frame_slot(ARG_REGS[2], args_base + 3 * 8);
                            self.emit_mov_imm32_sx(ARG_REGS[3], 4);
                            self.emit_pre_safepoint_spill_before_helper_call();
                            cratonvm_jit_api::assert_helper_call_shape!(
                                "invoke_dispatch",
                                int_args = 4,
                                returns_value = true
                            );
                            self.emit_call_absolute(self.helpers.invoke_dispatch);
                            self.emit_oop_map_for_safepoint();
                            self.emit_post_invoke_exception_check(b'V');
                            self.next_spill_offset = args_base;
                            self.patch_rel32_to_here(skip);
                        } else {
                            for patch in [p_null, p_neg, p_gt, p_oob] {
                                self.deopt_stubs.push((patch, pc, 2));
                            }
                        }
                    } else if callee_entry == crate::JitIntrinsic::ArraysEquals1.as_entry()
                        || callee_entry == crate::JitIntrinsic::ArraysEquals2.as_entry()
                        || callee_entry == crate::JitIntrinsic::ArraysEquals4.as_entry()
                        || callee_entry == crate::JitIntrinsic::ArraysEquals8.as_entry()
                    {
                        // Arrays.equals(a, b) : boolean
                        //
                        // JDK semantics (java.util.Arrays):
                        //   a == b               -> true   (both null, or same ref)
                        //   a == null || b==null -> false
                        //   a.length != b.length -> false
                        //   else element-wise equality.
                        // `equals` never throws — it is a pure compare,
                        // so no null-check stub is needed.
                        //
                        // The element-wise compare is a raw byte compare
                        // of `length * elem_size` bytes via REP CMPSB (or
                        // the 16-byte SSE2 loop under
                        // `CRATONVM_JIT_ARRAYS_EQUALS_SSE2`, below).
                        // boolean[] stores 0/1 per byte, so a byte
                        // compare is exact for `equals([Z[Z)Z`.
                        let b_slot = self.pop_stack();
                        let a_slot = self.pop_stack();
                        // a → R8, b → R9.
                        self.load_slot_to_reg(R8, a_slot);
                        self.load_slot_to_reg(R9, b_slot);

                        let shift: u8 = if callee_entry
                            == crate::JitIntrinsic::ArraysEquals1.as_entry()
                        {
                            0
                        } else if callee_entry == crate::JitIntrinsic::ArraysEquals2.as_entry() {
                            1
                        } else if callee_entry == crate::JitIntrinsic::ArraysEquals4.as_entry() {
                            2
                        } else {
                            3
                        };

                        // JIT r22 wave 1, lane intrinsics (INTRINSICS21-5,
                        // `CRATONVM_JIT_ARRAYS_EQUALS_SSE2`, opt-in): the
                        // element compare is a 16-byte `MOVDQU` x2 +
                        // `PCMPEQB` + `PMOVMSKB` loop with an 8-byte and a
                        // byte tail instead of `REP CMPSB` (microcoded,
                        // about a byte per cycle). The same raw byte
                        // compare, so the same answer for every integral
                        // element type; XMM0/XMM1 are free here
                        // (`flush_scratch_registers` leaves no XMM0-7 slot
                        // at an invoke). The loop puts the true / false
                        // labels out of `rel8` reach, so under the switch
                        // every forward branch to them is `rel32`; off,
                        // the emitted bytes are the old ones exactly.
                        let sse2 = crate::arrays_equals_sse2_enabled();
                        let mut to_true8: Vec<usize> = Vec::new();
                        let mut to_false8: Vec<usize> = Vec::new();
                        let mut to_true32: Vec<usize> = Vec::new();
                        let mut to_false32: Vec<usize> = Vec::new();

                        // CMP R8, R9  (4D 39 C8) — same reference?
                        self.buf.emit(&[0x4D, 0x39, 0xC8]);
                        // JE -> true  (74 rel8) — covers both-null and
                        // identical-reference.
                        self.emit_arrays_equals_branch(0x74, sse2, &mut to_true8, &mut to_true32);

                        // TEST R8, R8  (4D 85 C0) — a null (b not)?
                        self.buf.emit(&[0x4D, 0x85, 0xC0]);
                        // JZ -> false  (74 rel8)
                        self.emit_arrays_equals_branch(0x74, sse2, &mut to_false8, &mut to_false32);

                        // TEST R9, R9  (4D 85 C9) — b null (a not)?
                        self.buf.emit(&[0x4D, 0x85, 0xC9]);
                        // JZ -> false  (74 rel8)
                        self.emit_arrays_equals_branch(0x74, sse2, &mut to_false8, &mut to_false32);

                        // Lengths: EAX = a.length, EDX = b.length.
                        // MOV EAX, [R8+len]  (41 8B 40 len; len = ARRAY_LENGTH_OFFSET, 8 today)
                        self.buf
                            // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                            .emit(&[0x41, 0x8B, 0x40, ARRAY_LENGTH_OFFSET as u8]);
                        // MOV EDX, [R9+len]  (41 8B 51 len)
                        self.buf
                            // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                            .emit(&[0x41, 0x8B, 0x51, ARRAY_LENGTH_OFFSET as u8]);
                        // CMP EAX, EDX  (39 D0)
                        self.buf.emit(&[0x39, 0xD0]);
                        // JNE -> false  (75 rel8)
                        self.emit_arrays_equals_branch(0x75, sse2, &mut to_false8, &mut to_false32);

                        // Byte count = length << shift  → RCX (REP
                        // counter). MOVZX is unnecessary: array lengths
                        // are non-negative i32, so a 32-bit MOV
                        // zero-extends cleanly into RCX.
                        // MOV ECX, EAX  (89 C1)
                        self.buf.emit(&[0x89, 0xC1]);
                        if shift != 0 {
                            // SHL RCX, shift  (48 C1 E1 ib)
                            self.buf.emit(&[0x48, 0xC1, 0xE1, shift]);
                        }
                        // Empty arrays (count == 0): equal. Also avoids
                        // running REP CMPSB with RCX==0, whose ZF would
                        // otherwise carry over from the SHL above.
                        // TEST RCX, RCX  (48 85 C9)
                        self.buf.emit(&[0x48, 0x85, 0xC9]);
                        // JZ -> true  (74 rel8)
                        self.emit_arrays_equals_branch(0x74, sse2, &mut to_true8, &mut to_true32);

                        let mut jmp_done_1: Option<usize> = None;
                        if sse2 {
                            // RCX = byte count (> 0). R10 / R11 walk the two
                            // payloads; RAX is scratch. No call, no
                            // safepoint, nothing pushed.
                            // LEA R10, [R8 + HEADER_SIZE]  (4D 8D 50 hdr; hdr = 16 today)
                            // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                            self.buf.emit(&[0x4D, 0x8D, 0x50, HEADER_SIZE as u8]);
                            // LEA R11, [R9 + HEADER_SIZE]  (4D 8D 59 hdr)
                            // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                            self.buf.emit(&[0x4D, 0x8D, 0x59, HEADER_SIZE as u8]);
                            // ---- 16 bytes per step while 16 remain ----
                            let v16 = self.buf.pos();
                            self.buf.emit(&[0x48, 0x83, 0xF9, 0x10]); // CMP RCX, 16
                            let to_tail8 = self.emit_jcc_rel32_patch(0x82); // JB
                            self.buf.emit(&[0xF3, 0x41, 0x0F, 0x6F, 0x02]); // MOVDQU XMM0, [R10]
                            self.buf.emit(&[0xF3, 0x41, 0x0F, 0x6F, 0x0B]); // MOVDQU XMM1, [R11]
                            self.buf.emit(&[0x66, 0x0F, 0x74, 0xC1]); // PCMPEQB XMM0, XMM1
                            self.buf.emit(&[0x66, 0x0F, 0xD7, 0xC0]); // PMOVMSKB EAX, XMM0
                            self.buf.emit(&[0x3D, 0xFF, 0xFF, 0x00, 0x00]); // CMP EAX, 0xFFFF
                            to_false32.push(self.emit_jcc_rel32_patch(0x85)); // JNE
                            self.buf.emit(&[0x49, 0x83, 0xC2, 0x10]); // ADD R10, 16
                            self.buf.emit(&[0x49, 0x83, 0xC3, 0x10]); // ADD R11, 16
                            self.buf.emit(&[0x48, 0x83, 0xE9, 0x10]); // SUB RCX, 16
                            self.emit_jmp_back_to(v16);
                            // ---- one 8-byte step (fewer than 16 remain) ----
                            self.patch_rel32_to_here(to_tail8);
                            self.buf.emit(&[0x48, 0x83, 0xF9, 0x08]); // CMP RCX, 8
                            let to_tail1 = self.emit_jcc_rel32_patch(0x82); // JB
                            self.buf.emit(&[0x49, 0x8B, 0x02]); // MOV RAX, [R10]
                            self.buf.emit(&[0x49, 0x3B, 0x03]); // CMP RAX, [R11]
                            to_false32.push(self.emit_jcc_rel32_patch(0x85)); // JNE
                            self.buf.emit(&[0x49, 0x83, 0xC2, 0x08]); // ADD R10, 8
                            self.buf.emit(&[0x49, 0x83, 0xC3, 0x08]); // ADD R11, 8
                            self.buf.emit(&[0x48, 0x83, 0xE9, 0x08]); // SUB RCX, 8
                            // ---- the last 0..7 bytes, one at a time ----
                            self.patch_rel32_to_here(to_tail1);
                            let t1 = self.buf.pos();
                            self.buf.emit(&[0x48, 0x85, 0xC9]); // TEST RCX, RCX
                            to_true32.push(self.emit_jcc_rel32_patch(0x84)); // JZ: all equal
                            self.buf.emit(&[0x41, 0x0F, 0xB6, 0x02]); // MOVZX EAX, BYTE [R10]
                            self.buf.emit(&[0x41, 0x3A, 0x03]); // CMP AL, BYTE [R11]
                            to_false32.push(self.emit_jcc_rel32_patch(0x85)); // JNE
                            self.buf.emit(&[0x49, 0xFF, 0xC2]); // INC R10
                            self.buf.emit(&[0x49, 0xFF, 0xC3]); // INC R11
                            self.buf.emit(&[0x48, 0xFF, 0xC9]); // DEC RCX
                            self.emit_jmp_back_to(t1);
                        } else {
                            // Save RSI/RDI (Java locals may live there),
                            // point them at the two element-data regions,
                            // run REP CMPSB, restore. POP does not touch
                            // flags, so ZF from CMPSB survives the restore.
                            // PUSH RSI (56) ; PUSH RDI (57)
                            self.buf.emit(&[0x56, 0x57]);
                            // LEA RSI, [R8 + HEADER_SIZE]  (49 8D 70 hdr; hdr = 16 today)
                            // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                            self.buf.emit(&[0x49, 0x8D, 0x70, HEADER_SIZE as u8]);
                            // LEA RDI, [R9 + HEADER_SIZE]  (49 8D 79 hdr)
                            // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                            self.buf.emit(&[0x49, 0x8D, 0x79, HEADER_SIZE as u8]);
                            // REP CMPSB  (F3 A6) — compares RCX bytes;
                            // stops early on the first mismatch. ZF=1 iff
                            // every byte matched.
                            self.buf.emit(&[0xF3, 0xA6]);
                            // POP RDI (5F) ; POP RSI (5E)
                            self.buf.emit(&[0x5F, 0x5E]);
                            // SETE AL  (0F 94 C0) — AL = ZF.
                            self.buf.emit(&[0x0F, 0x94, 0xC0]);
                            // MOVZX EAX, AL  (0F B6 C0) — boolean result.
                            self.buf.emit(&[0x0F, 0xB6, 0xC0]);
                            // JMP -> done  (EB rel8)
                            self.buf.emit_byte(0xEB);
                            jmp_done_1 = Some(self.buf.pos());
                            self.buf.emit_byte(0x00);
                        }

                        // --- true label ---
                        let true_label = self.buf.pos();
                        for p in to_true32 {
                            self.patch_rel32_to_here(p);
                        }
                        // MOV EAX, 1  (B8 01 00 00 00)
                        self.buf.emit(&[0xB8, 0x01, 0x00, 0x00, 0x00]);
                        // JMP -> done  (EB rel8)
                        self.buf.emit_byte(0xEB);
                        let jmp_done_2 = self.buf.pos();
                        self.buf.emit_byte(0x00);

                        // --- false label ---
                        let false_label = self.buf.pos();
                        for p in to_false32 {
                            self.patch_rel32_to_here(p);
                        }
                        // XOR EAX, EAX  (31 C0)
                        self.buf.emit(&[0x31, 0xC0]);

                        // --- done label ---
                        let done_label = self.buf.pos();

                        // Patch all rel8 displacements. Every span here
                        // is a few dozen bytes — comfortably inside the
                        // signed-rel8 range; debug_assert guards it.
                        let mut rel8: Vec<(usize, usize)> = Vec::new();
                        rel8.extend(to_true8.iter().map(|&p| (p, true_label)));
                        rel8.extend(to_false8.iter().map(|&p| (p, false_label)));
                        if let Some(p) = jmp_done_1 {
                            rel8.push((p, done_label));
                        }
                        rel8.push((jmp_done_2, done_label));
                        for (patch, target) in rel8 {
                            // Cast: signed offset to isize for pointer/index arithmetic
                            let rel = target as isize - (patch as isize + 1);
                            debug_assert!(
                                (-128..=127).contains(&rel),
                                "Arrays.equals intrinsic rel8 out of range: {rel}"
                            );
                            // Widening: isize -> i64 (no truncation)
                            Self::patch_rel8_or_bail(&mut self.buf, patch, rel as i64);
                        }

                        // boolean result in RAX → operand stack.
                        self.push_from_rax();
                    }
                    // ===== INTRINSIC REGION END: ARRAYS_OPS =====

                    // ===== INTRINSIC REGION BEGIN: ARRAYS_SORT =====
                    else if callee_entry == crate::JitIntrinsic::ArraysSortInt.as_entry()
                        || callee_entry == crate::JitIntrinsic::ArraysSortLong.as_entry()
                        || callee_entry == crate::JitIntrinsic::ArraysSortChar.as_entry()
                        || callee_entry == crate::JitIntrinsic::ArraysSortShort.as_entry()
                        || callee_entry == crate::JitIntrinsic::ArraysSortByte.as_entry()
                    {
                        // Phase 4b — java.util.Arrays.sort(prim[]) inline
                        // sort. Void return: nothing is pushed.
                        //
                        // The matcher (try_resolve_intrinsic ARRAYS_SORT
                        // region) registers only the five integral
                        // single-arg overloads. Up to 47 elements — the
                        // JDK's own insertion-sort cut-over — this is an
                        // insertion sort; past that, an in-place heapsort,
                        // O(n log n) and still allocation-free. The
                        // insertion sort used to run for EVERY length:
                        // O(n^2), so a compiled `Arrays.sort` of a million
                        // ints took minutes instead of tens of milliseconds
                        // and stalled every thread waiting on a safepoint
                        // for all of it.
                        //
                        // Neither loop polls for a safepoint, and neither
                        // may: the array base lives in R8, which no oop map
                        // names, so a moving collection inside the loop
                        // would leave it pointing at from-space. Heapsort
                        // bounds that window to O(n log n) work, which is
                        // what a native sort costs anyway.
                        //
                        // All work uses caller-saved scratch only
                        // (RAX/RCX/RDX/R8/R9/R10/R11) — `flush_scratch_
                        // registers()` already spilled them and Java
                        // locals live in callee-saved registers, so the
                        // loop never clobbers live state.
                        //
                        // Register file for the emitted routine:
                        //   R8  = array base pointer
                        //   R9  = n (element count)
                        //   R10 = i (outer index)
                        //   R11 = j (inner index)
                        //   RAX = key (= a[i])
                        //   RCX = a[j] scratch
                        //   RDX = j+1 (store index)
                        //
                        // Every element is loaded sign-/zero-extended to
                        // a full 64-bit register, so a single signed
                        // 64-bit CMP orders all five element kinds
                        // correctly (char is unsigned 0..=65535, which is
                        // non-negative, so signed compare still works).

                        // SIB scale bits + load/store encodings per width.
                        let is_int = callee_entry == crate::JitIntrinsic::ArraysSortInt.as_entry();
                        let is_long =
                            callee_entry == crate::JitIntrinsic::ArraysSortLong.as_entry();
                        let is_char =
                            callee_entry == crate::JitIntrinsic::ArraysSortChar.as_entry();
                        let is_short =
                            callee_entry == crate::JitIntrinsic::ArraysSortShort.as_entry();
                        // is_byte is the remaining case.
                        let scale_ss: u8 = if is_long {
                            0b11 // *8
                        } else if is_int {
                            0b10 // *4
                        } else if is_char || is_short {
                            0b01 // *2
                        } else {
                            0b00 // *1 (byte)
                        };
                        // HEADER_SIZE / ARRAY_LENGTH_OFFSET both fit in a
                        // signed disp8 (asserted in cratonvm_types).
                        // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                        let hdr = HEADER_SIZE as u8;
                        // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                        let len_off = ARRAY_LENGTH_OFFSET as u8;

                        // Pop the array reference, null-check it (reusing
                        // the shared NPE stub), then park it in R8.
                        let arr_slot = self.pop_stack();
                        self.load_slot_to_reg(RAX, arr_slot);
                        // Null array: dispatch the real `Arrays.sort` so the
                        // JDK body throws HotSpot's NPE (see
                        // `emit_arrays_intrinsic_null_dispatch`). Without a
                        // registered `JitInvokeInfo`, the shared stub's
                        // unmessaged NPE as before.
                        let sort_null_dispatch = self
                            .arrays_intrinsic_null_dispatch_info(pc, "sort")
                            .map(|info| {
                                self.emit_test_r64_r64(RAX);
                                (self.emit_jcc_rel32_patch(0x84), info) // JZ
                            });
                        if sort_null_dispatch.is_none() {
                            // TEST RAX,RAX / JZ -> shared null-check stub.
                            // Intrinsic array access — no precise array opcode,
                            // so record NONE (unmessaged NPE).
                            let trap_key =
                                crate::x64::inlining::record_npe_trap_site(self.orig_bci(pc));
                            self.emit_null_check_array_load(npe_action::NONE, trap_key, pc);
                        }
                        // MOV R8, RAX  (49 89 C0)
                        self.buf.emit(&[0x49, 0x89, 0xC0]);
                        // MOV R9D, [R8 + ARRAY_LENGTH_OFFSET]  (45 8B 48 dd)
                        // (32-bit load zero-extends n into R9.)
                        self.buf.emit(&[0x45, 0x8B, 0x48, len_off]);
                        // CMP R9, 47  (49 83 F9 2F) ; JG .heapsort
                        // n is a zero-extended array length, so the signed
                        // compare is exact.
                        self.buf.emit(&[0x49, 0x83, 0xF9, 0x2F]);
                        let heapsort_patch = self.emit_jcc_rel32_patch(0x8F);
                        // MOV R10D, 1   (41 BA 01 00 00 00) — i = 1
                        self.buf.emit(&[0x41, 0xBA, 0x01, 0x00, 0x00, 0x00]);

                        // --- inline emitters for width-specific access ---
                        // Access form: [R8 + idx*scale + hdr] with a
                        // ModRM.reg operand `r` (the GPR load dest /
                        // store source). REX bits MUST be derived per
                        // register: REX.R from `r`, REX.X from `idx`,
                        // REX.B is always 1 (base is R8). REX.W comes
                        // from the caller (`w`). A prior version hard-
                        // coded REX.X=1, which silently re-routed a
                        // store through a non-extended index register
                        // (RDX -> R10) and left the array unsorted.
                        let rex = |w: u8, r: u8, idx: u8| -> u8 {
                            0x40 | (w << 3)
                                // Truncation: wider int -> u8 (low 8 bits, intentional)
                                | (((r >= 8) as u8) << 2)
                                // Truncation: wider int -> u8 (low 8 bits, intentional)
                                | (((idx >= 8) as u8) << 1)
                                | 1 // REX.B: base = R8
                        };
                        // load r <- [R8 + idx*scale + hdr]
                        let emit_load = |buf: &mut crate::ExecutableBuffer, r: u8, idx: u8| {
                            let sib = (scale_ss << 6) | ((idx & 7) << 3);
                            let modrm = 0x40 | ((r & 7) << 3) | 0x04;
                            if is_long {
                                // MOV r64,[..]  REX.W, 8B
                                buf.emit(&[rex(1, r, idx), 0x8B, modrm, sib, hdr]);
                            } else if is_int {
                                // MOVSXD r64,[..]  REX.W, 63
                                buf.emit(&[rex(1, r, idx), 0x63, modrm, sib, hdr]);
                            } else if is_char {
                                // MOVZX r32,m16  0F B7. char is
                                // unsigned 0..=65535; zero-extending
                                // to 32 bits also clears RAX[63:32],
                                // so the 64-bit signed CMP is correct.
                                buf.emit(&[rex(0, r, idx), 0x0F, 0xB7, modrm, sib, hdr]);
                            } else if is_short {
                                // MOVSX r64,m16  REX.W 0F BF — MUST
                                // sign-extend to the FULL 64-bit
                                // register: the inner-loop CMP is
                                // 64-bit, and a 32-bit MOVSX would
                                // leave a negative short looking like
                                // a large positive (0x0000_0000_FFFF…).
                                buf.emit(&[rex(1, r, idx), 0x0F, 0xBF, modrm, sib, hdr]);
                            } else {
                                // MOVSX r64,m8  REX.W 0F BE — sign-
                                // extend a signed byte to 64 bits
                                // (same rationale as short above).
                                buf.emit(&[rex(1, r, idx), 0x0F, 0xBE, modrm, sib, hdr]);
                            }
                        };
                        // store [R8 + idx*scale + hdr] <- r
                        let emit_store = |buf: &mut crate::ExecutableBuffer, r: u8, idx: u8| {
                            let sib = (scale_ss << 6) | ((idx & 7) << 3);
                            let modrm = 0x40 | ((r & 7) << 3) | 0x04;
                            if is_long {
                                // MOV [..],r64  REX.W, 89
                                buf.emit(&[rex(1, r, idx), 0x89, modrm, sib, hdr]);
                            } else if is_int {
                                // MOV [..],r32  89
                                buf.emit(&[rex(0, r, idx), 0x89, modrm, sib, hdr]);
                            } else if is_char || is_short {
                                // MOV [..],r16  66 prefix, 89
                                buf.emit(&[0x66, rex(0, r, idx), 0x89, modrm, sib, hdr]);
                            } else {
                                // MOV [..],r8   88
                                buf.emit(&[rex(0, r, idx), 0x88, modrm, sib, hdr]);
                            }
                        };

                        // .outer:
                        let outer_label = self.buf.pos();
                        // CMP R10, R9   (4D 39 CA) — i vs n
                        self.buf.emit(&[0x4D, 0x39, 0xCA]);
                        // JGE .done  (0F 8D rel32) — signed: i >= n
                        let done_patch = self.emit_jcc_rel32_patch(0x8D);
                        // key = a[i]
                        emit_load(&mut self.buf, RAX, R10);
                        // MOV R11, R10  (4D 89 D3) — j = i
                        self.buf.emit(&[0x4D, 0x89, 0xD3]);
                        // DEC R11       (49 FF CB) — j = i - 1
                        self.buf.emit(&[0x49, 0xFF, 0xCB]);

                        // .inner:
                        let inner_label = self.buf.pos();
                        // TEST R11,R11  (4D 85 DB)
                        self.buf.emit(&[0x4D, 0x85, 0xDB]);
                        // JS .insert    (0F 88 rel32) — j < 0 -> stop
                        let insert_patch = self.emit_jcc_rel32_patch(0x88);
                        // RCX = a[j]
                        emit_load(&mut self.buf, RCX, R11);
                        // CMP RCX, RAX  (48 39 C1) — a[j] vs key, signed64
                        self.buf.emit(&[0x48, 0x39, 0xC1]);
                        // JLE .insert   (0F 8E rel32) — a[j] <= key -> stop
                        //   (stable: equal keys are never shifted past.)
                        let insert_patch2 = self.emit_jcc_rel32_patch(0x8E);
                        // a[j+1] = a[j]
                        // LEA RDX, [R11 + 1]  (49 8D 53 01)
                        self.buf.emit(&[0x49, 0x8D, 0x53, 0x01]);
                        emit_store(&mut self.buf, RCX, RDX);
                        // DEC R11  (49 FF CB)  — j--
                        self.buf.emit(&[0x49, 0xFF, 0xCB]);
                        // JMP .inner  (E9 rel32) — backward branch.
                        self.buf.emit_byte(0xE9);
                        {
                            let here = self.buf.pos();
                            // Widening: usize/u32 offset -> i64 (no truncation; for rel/displacement math)
                            let rel = (inner_label as i64) - (here as i64 + 4);
                            // Truncation: i64 -> i32 (rel32 branch displacement, range-checked)
                            self.buf.emit(&(rel as i32).to_le_bytes());
                        }

                        // .insert: both JS and JLE land here.
                        self.patch_rel32_to_here(insert_patch);
                        self.patch_rel32_to_here(insert_patch2);
                        // a[j+1] = key
                        // LEA RDX, [R11 + 1]  (49 8D 53 01)
                        self.buf.emit(&[0x49, 0x8D, 0x53, 0x01]);
                        emit_store(&mut self.buf, RAX, RDX);
                        // INC R10  (49 FF C2)  — i++
                        self.buf.emit(&[0x49, 0xFF, 0xC2]);
                        // JMP .outer  (E9 rel32) — backward branch.
                        self.buf.emit_byte(0xE9);
                        {
                            let here = self.buf.pos();
                            // Widening: usize/u32 offset -> i64 (no truncation; for rel/displacement math)
                            let rel = (outer_label as i64) - (here as i64 + 4);
                            // Truncation: i64 -> i32 (rel32 branch displacement, range-checked)
                            self.buf.emit(&(rel as i32).to_le_bytes());
                        }

                        // .done: the top-of-loop JGE lands here.
                        self.patch_rel32_to_here(done_patch);
                        // JMP .end — skip the heapsort.
                        self.buf.emit_byte(0xE9);
                        let end_patch = self.buf.pos();
                        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);

                        // .heapsort: n > 47. Floyd's bottom-up heap
                        // construction, then repeated extraction of the
                        // maximum. Same register file as above, plus:
                        //   R10 = heap build cursor / extraction end
                        //   R11 = sift root
                        //   RDX = sift child
                        // RAX/RCX hold element values, loaded to 64 bits by
                        // `emit_load`, so the signed 64-bit CMPs order every
                        // element kind exactly as the insertion sort does.
                        self.patch_rel32_to_here(heapsort_patch);
                        macro_rules! jmp_back {
                            ($s:ident, $label:expr) => {{
                                $s.buf.emit_byte(0xE9);
                                let here = $s.buf.pos();
                                // Widening: usize offset -> i64 for rel math
                                let rel = ($label as i64) - (here as i64 + 4);
                                // Truncation: i64 -> i32 (rel32 within one method)
                                $s.buf.emit(&(rel as i32).to_le_bytes());
                            }};
                        }
                        // Sift a[R11] down within [0, limit). `$limit_rm` is
                        // the ModRM byte of `CMP RDX, limit` (limit = R9 → CA,
                        // limit = R10 → D2) under REX 4C.
                        macro_rules! sift_down {
                            ($s:ident, $limit_rm:expr) => {{
                                let sift_loop = $s.buf.pos();
                                // LEA RDX, [R11 + R11 + 1] — child = 2*root + 1
                                $s.buf.emit(&[0x4B, 0x8D, 0x54, 0x1B, 0x01]);
                                $s.buf.emit(&[0x4C, 0x39, $limit_rm]); // CMP RDX, limit
                                let sift_done = $s.emit_jcc_rel32_patch(0x8D); // JGE
                                emit_load(&mut $s.buf, RAX, RDX); // RAX = a[child]
                                $s.buf.emit(&[0x48, 0xFF, 0xC2]); // INC RDX
                                $s.buf.emit(&[0x4C, 0x39, $limit_rm]); // CMP RDX, limit
                                let no_right = $s.emit_jcc_rel32_patch(0x8D); // JGE .left
                                emit_load(&mut $s.buf, RCX, RDX); // RCX = a[child + 1]
                                $s.buf.emit(&[0x48, 0x39, 0xC8]); // CMP RAX, RCX
                                let left_not_smaller = $s.emit_jcc_rel32_patch(0x8D); // JGE .left
                                $s.buf.emit(&[0x48, 0x89, 0xC8]); // MOV RAX, RCX
                                $s.buf.emit_byte(0xE9); // JMP .have_child
                                let have_child = $s.buf.pos();
                                $s.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                                // .left: the left child is the larger one.
                                $s.patch_rel32_to_here(no_right);
                                $s.patch_rel32_to_here(left_not_smaller);
                                $s.buf.emit(&[0x48, 0xFF, 0xCA]); // DEC RDX
                                // .have_child: RDX = larger child, RAX = its value.
                                $s.patch_rel32_to_here(have_child);
                                emit_load(&mut $s.buf, RCX, R11); // RCX = a[root]
                                $s.buf.emit(&[0x48, 0x39, 0xC1]); // CMP RCX, RAX
                                let in_order = $s.emit_jcc_rel32_patch(0x8D); // JGE .sift_done
                                emit_store(&mut $s.buf, RAX, R11); // a[root] = a[child]
                                emit_store(&mut $s.buf, RCX, RDX); // a[child] = old a[root]
                                $s.buf.emit(&[0x49, 0x89, 0xD3]); // MOV R11, RDX
                                jmp_back!($s, sift_loop);
                                $s.patch_rel32_to_here(sift_done);
                                $s.patch_rel32_to_here(in_order);
                            }};
                        }
                        // Build: for start in (0..n/2).rev() { sift(start, n) }
                        self.buf.emit(&[0x4D, 0x89, 0xCA]); // MOV R10, R9
                        self.buf.emit(&[0x49, 0xD1, 0xEA]); // SHR R10, 1
                        let build_loop = self.buf.pos();
                        self.buf.emit(&[0x4D, 0x85, 0xD2]); // TEST R10, R10
                        let build_done = self.emit_jcc_rel32_patch(0x84); // JZ
                        self.buf.emit(&[0x49, 0xFF, 0xCA]); // DEC R10
                        self.buf.emit(&[0x4D, 0x89, 0xD3]); // MOV R11, R10
                        sift_down!(self, 0xCA);
                        jmp_back!(self, build_loop);
                        self.patch_rel32_to_here(build_done);
                        // Extract: for end in (1..n).rev() { swap(0, end); sift(0, end) }
                        self.buf.emit(&[0x4D, 0x89, 0xCA]); // MOV R10, R9
                        let extract_loop = self.buf.pos();
                        self.buf.emit(&[0x49, 0xFF, 0xCA]); // DEC R10
                        self.buf.emit(&[0x4D, 0x85, 0xD2]); // TEST R10, R10
                        let sorted = self.emit_jcc_rel32_patch(0x8E); // JLE
                        self.buf.emit(&[0x45, 0x31, 0xDB]); // XOR R11D, R11D
                        emit_load(&mut self.buf, RAX, R11); // RAX = a[0]
                        emit_load(&mut self.buf, RCX, R10); // RCX = a[end]
                        emit_store(&mut self.buf, RCX, R11); // a[0] = a[end]
                        emit_store(&mut self.buf, RAX, R10); // a[end] = old a[0]
                        sift_down!(self, 0xD2);
                        jmp_back!(self, extract_loop);
                        self.patch_rel32_to_here(sorted);

                        // .end
                        self.patch_rel32_to_here(end_patch);
                        // JIT r21 intrinsics review (intr21): the GPU
                        // input-residency barrier, as the `Arrays.fill` and
                        // `System.arraycopy` arms emit it. This arm writes the
                        // whole array in place with no `*astore` opcode, so
                        // `offload_jit_gate`'s bytecode scan never saw it and
                        // a `--gpu` run left the device mirror stale. R8 still
                        // holds the array base on both the insertion and the
                        // heapsort paths; nothing is emitted (the code is
                        // byte-identical) unless a `--gpu` run armed the
                        // barrier. The null dispatch below is bound after
                        // this point, so a null array (nothing written) skips
                        // it.
                        if crate::gpu_barrier::barrier_bytes().is_some() {
                            // MOV RAX, R8  (4C 89 C0)
                            self.buf.emit(&[0x4C, 0x89, 0xC0]);
                            self.emit_gpu_input_cache_barrier();
                        }
                        if let Some((null_patch, info)) = sort_null_dispatch {
                            if !self.emit_arrays_intrinsic_null_dispatch(null_patch, info, 1) {
                                self.fail(
                                    "singlepass-codegen/arrays-sort-null-args-spill-exhausted",
                                );
                                return WalkStep::Return(false);
                            }
                        }
                        // Void method — nothing pushed; `ret_type` is 'V'.
                        let _ = ret_type;
                    }
                    // ===== INTRINSIC REGION END: ARRAYS_SORT =====
                    // ===== INTRINSIC REGION BEGIN: REACHABILITY_FENCE =====
                    // `Reference.reachabilityFence(Object)V` (JIT round 17
                    // wave 1, lane nio; `CRATONVM_JIT_REACHABILITY_FENCE_ELIDE`).
                    // No code: the operand is dropped exactly as the `pop`
                    // bytecode drops it. The referent stays mapped up to this
                    // pc because the bytecode that fed it is unchanged; see
                    // `JitIntrinsic::ReachabilityFence` for the liveness
                    // argument.
                    else if callee_entry == crate::JitIntrinsic::ReachabilityFence.as_entry() {
                        let _ = self.pop_stack();
                        let _ = ret_type;
                    }
                    // ===== INTRINSIC REGION END: REACHABILITY_FENCE =====
                    else if crate::JitIntrinsic::is_sentinel_entry(callee_entry) {
                        // An intrinsic sentinel no arm above emitted — a
                        // registration/emission disagreement (a CPU-feature
                        // gate, a despec filter, a family added to the
                        // resolver but not to this ladder). The plain arm
                        // below would `CALL` the sentinel address. Refuse the
                        // method; the interpreter runs the call correctly.
                        self.fail("singlepass-codegen/unhandled-intrinsic-sentinel");
                        return WalkStep::Return(false);
                    } else {
                        // value-stack-usize-underflow-nio-worker-panic fix:
                        // snapshot the pre-pop operand stack (see the
                        // matching invokevirtual/interface fix below) so a
                        // post-invoke exception/deopt guard's `Reinterpret`
                        // resume at this bci has the args this invokestatic
                        // needs, instead of underflowing on an empty stack.
                        if crate::deopt_real_enabled() {
                            self.snapshot_pre_intrinsic_call(
                                pc,
                                crate::deopt::DeoptReason::ReceiverTypeChanged,
                            );
                        }
                        // A caller-held synchronized CALL (see `sync_direct_site`):
                        // the class monitor, taken while the arguments are
                        // still on the simulated stack, so every oop among
                        // them stays mapped across a contended enter. The
                        // mirror is pushed above them and popped by the enter.
                        // The instance twin (round 13 wave 1) enters a copy of
                        // the receiver, which is argument 0 of its row.
                        if sync_direct_site
                            && !(self.push_sync_direct_enter_monitor(
                                pc,
                                sync_instance_row.then_some(callee_params + 1),
                            ) && self.emit_monitor_on_stack_top(true, pc))
                        {
                            self.fail("singlepass-codegen/sync-direct-enter");
                            return WalkStep::Return(false);
                        }
                        // Direct call to a JIT-compiled callee. An instance
                        // row's `num_params` excludes its receiver.
                        let n = callee_params + usize::from(sync_instance_row);
                        // `pop_stack` rewinds `next_spill_offset` when it pops a
                        // top-of-stack `Frame` slot, but it still HANDS THE SLOT
                        // BACK, and every `arg_slots` entry stays live until
                        // `emit_stack_arg_setup` marshals it into the entry ABI far
                        // below. Anything that reserves spill space in between is
                        // therefore handed the argument slots themselves. Remember
                        // the pre-pop top so such a reservation can be placed above
                        // them. See
                        // jit-direct-call-arg1-clobbered-by-arg0-FIXED.md.
                        let args_frame_top = self.next_spill_offset;
                        // gcd d9/d (2026-09-28): the callee OWNS this call's
                        // arguments (`crate::direct_callee_owns_its_arguments`):
                        // no service copy is made below, nothing of the caller's
                        // names or images them across the CALL, and the
                        // callee-deopt check hands the service no buffer. The
                        // copy was named in the call's map and read by the band
                        // scan, so it kept every reference argument alive for
                        // the whole call however early the callee dropped it
                        // (`Gcd1ArgPinProbe` warm case: `caller` off=104,
                        // `region=operand-spill tier=sp`). See
                        // `sp_direct_call_args_owned_by_callee` for why each
                        // term is needed; `CRATONVM_JIT_CALLEE_OWNED_ARGS=0`
                        // restores the copy.
                        let site_admits_owned_args = sp_direct_call_args_owned_by_callee(
                            callee_owned_args_enabled(),
                            code.get(pc).copied(),
                            info_ptr.map(|ip| {
                                // SAFETY: `invoke_info` owns every pointer it
                                // hands out for the life of this compile.
                                unsafe { (*ip).invoke_kind }
                            }),
                            sync_direct_site,
                            retire_cell.is_some(),
                            self.pc_is_protected(pc),
                            // gcd d10/f: the hand-off's caller replay must be
                            // exact (`owned_args_handoff_replays_exactly`).
                            || self.owned_args_handoff_replays_exactly(code, code_len, pc),
                        );
                        let args_owned_by_callee = site_admits_owned_args
                            && crate::direct_callee_owns_its_arguments(callee_entry);
                        let call_oop_arg_regs_before_pop = self.pending_call_oop_arg_regs;
                        let (arg_slots, arg_oops) = self.pop_invoke_args(n);
                        if args_owned_by_callee {
                            // `pop_invoke_args` marked the registers holding the
                            // reference arguments live for the CALL's register
                            // mask. They are the callee's now: nothing after the
                            // call reads them as this frame's values, and a live
                            // local in the same register keeps its own bit.
                            self.pending_call_oop_arg_regs = call_oop_arg_regs_before_pop;
                        }
                        // A reference staged where no oop map can name it fails the
                        // safepoint closed. DEFERRED to just after the service-range
                        // reservation below, because whether that is true here is
                        // exactly what the reservation decides: when it succeeds it
                        // copies every argument into a contiguous frame range, and a
                        // frame range IS nameable. See `direct_call_arg_maps_enabled`.
                        // Spill cursor as the bytecode's operand stack sees it
                        // now that this invoke's arguments are popped. The
                        // return value belongs HERE, not wherever the
                        // service-argument reservation below leaves the cursor.
                        let post_pop_spill = self.next_spill_offset;

                        // T5.2.16 — Sibling tail-call optimization.
                        //
                        // When the caller's immediate next bytecode
                        // is an `xreturn` of the same type the
                        // callee produces, we can tear down our
                        // frame and `JMP` into the callee so the
                        // callee returns straight to OUR caller.
                        // Gate on:
                        //   1. pc+3 is an xreturn whose type tag
                        //      matches ret_type, or the callee is
                        //      void AND pc+3 is `return` (0xB1).
                        //   2. callee_needs_ctx == self.needs_heap
                        //      (we have a VM context iff the
                        //      callee wants one) — otherwise the
                        //      ABI shift wouldn't match.
                        //   3. RET intrinsics (MATH_*_INTRINSIC)
                        //      are NOT targeted (already branched
                        //      above), so the callee is a normal
                        //      JIT-compiled method.
                        //   4. `pc + 3` is NOT a branch target. The
                        //      tail form CONSUMES the `xreturn` — it emits
                        //      no code for that PC and leaves
                        //      `pc_to_native[pc + 3]` unset — so any other
                        //      edge into it becomes unresolvable and
                        //      `patch_branches` rejects the whole method
                        //      with `branch-target-not-an-instruction-
                        //      boundary`, a reason whose message blames
                        //      malformed bytecode. It is the ordinary
                        //      shape `return (x != null ? x : missing())`:
                        //      the `else` arm's call sits immediately
                        //      before the shared `areturn`, and the `then`
                        //      arm's `goto` lands on it. Fusing would also
                        //      be wrong on its own terms — the other edge
                        //      arrives with its own value on the operand
                        //      stack and expects a plain return, not "load
                        //      args and JMP to the callee". This is the
                        //      same precondition the const-arith peepholes
                        //      state: never fuse across a merge point.
                        let tail_op_matches = pc + 3 < code_len
                            && !branch_targets[pc + 3]
                            && match (ret_type, code[pc + 3]) {
                                (b'I' | b'Z' | b'B' | b'S' | b'C', 0xAC) => true,
                                (b'J', 0xAD) => true,
                                (b'F', 0xAE) => true,
                                (b'D', 0xAF) => true,
                                (b'L' | b'[', 0xB0) => true,
                                (b'V', 0xB1) => true,
                                _ => false,
                            };
                        // A tail-call inside a try region would tear this
                        // frame down before the callee runs, so anything it
                        // throws escapes the handler that covers this pc
                        // (see `pc_is_protected`). Demote to a normal CALL.
                        // A caller-held synchronized CALL is never a tail: the
                        // frame would be gone before the release.
                        let is_sibling_tail = tail_op_matches
                            && !sync_direct_site
                            && callee_needs_ctx == self.needs_heap
                            && !self.pc_is_protected(pc);

                        // Round-8 wave-3: sibling-tail demotion.
                        // Tail-calling with stack args is non-trivial
                        // — args would have to be re-materialized
                        // *after* the epilogue restores RSP, which
                        // requires an additional shuffle buffer.
                        // Simpler and still correct: demote to a
                        // non-tail CALL when the arg count would
                        // require stack passing. The fall-through
                        // below handles that case with the proper
                        // stack-arg setup helper.
                        let sibling_reg_limit = if callee_needs_ctx {
                            ARG_REGS.len() - 1
                        } else {
                            ARG_REGS.len()
                        };
                        // 5. The callee cannot stash a deopt frame.
                        //
                        // A sibling tail call REPLACES this frame, so the
                        // callee returns straight to OUR caller — and if it
                        // traps, the `i64::MIN` sentinel and the frame it
                        // stashed under the CALLEE's key arrive at a call
                        // site that invoked US. That site's identity gate
                        // (`try_resume_trapped_callee`) correctly refuses a
                        // stash naming a method it did not call, and the
                        // frame becomes an orphan nobody can attribute.
                        // A real CALL keeps this frame alive long enough
                        // for `emit_inline_callee_deopt_check` below to
                        // service the trap at the site that made it.
                        //
                        // `info_ptr.is_some()` IS the "can stash" test:
                        // a `JitInvokeInfo` is registered for exactly the
                        // sites whose callee is a compiled Java artifact
                        // (plus `ArraycopyPrimitive`, the one intrinsic
                        // that deopts). Inline-machine-code intrinsics and
                        // the thin native helpers have no info and no way
                        // to stash, so they keep the tail form.
                        //
                        // 6. The callee takes no VM context (round 18 wave 9,
                        //    lane osrframe): a context-taking thin helper can
                        //    walk the stack while the mirror still names this
                        //    frame. See `sp_tailcall_keeps_vm_helper_frame`.
                        //    Unless (OSRFRAME18-2, opt-in) the per-helper
                        //    classification proves the helper has no arm that
                        //    reaches the VM: see `sp_tail_leaf_helpers_enabled`.
                        //    TAIL7-19-3 (lane tail8, opt-in): four more
                        //    twins under `sp_tail_leaf_helpers_wide_enabled`.
                        let narrow_twins = sp_tail_leaf_helpers_enabled();
                        let wide_twins = sp_tail_leaf_helpers_wide_enabled();
                        let helper_vm_reach = if callee_needs_ctx
                            && info_ptr.is_none()
                            && (narrow_twins || wide_twins)
                        {
                            self.direct_helpers.thin_helper_vm_reach_with(
                                callee_entry,
                                narrow_twins,
                                wide_twins,
                            )
                        } else {
                            crate::direct_helpers::ThinHelperVmReach::MayReachVm
                        };
                        let sibling_tail_ok = is_sibling_tail
                            && arg_slots.len() <= sibling_reg_limit
                            && info_ptr.is_none()
                            && sibling_tail_target_admitted(
                                callee_needs_ctx,
                                sp_tailcall_keeps_vm_helper_frame(),
                                helper_vm_reach.never_reaches_vm(),
                            )
                            && sp_tailcall_enabled();
                        if sibling_tail_ok {
                            // Load args into ABI registers, tear
                            // down our frame, then JMP.
                            if callee_needs_ctx {
                                self.emit_load_vm_context(ARG_REGS[0]);
                                for (i, slot) in arg_slots.iter().enumerate() {
                                    if i + 1 < ARG_REGS.len() {
                                        self.load_slot_to_reg(ARG_REGS[i + 1], *slot);
                                    }
                                }
                            } else {
                                for (i, slot) in arg_slots.iter().enumerate() {
                                    if i < ARG_REGS.len() {
                                        self.load_slot_to_reg(ARG_REGS[i], *slot);
                                    }
                                }
                            }
                            self.emit_epilogue_without_ret();
                            self.emit_jmp_absolute(callee_entry);
                            // Consume the invokestatic (3) and the
                            // xreturn (1) — no fall-through.
                            pc += 3; // invokestatic
                            pc += 1; // xreturn
                            self.reset_spills();
                            return WalkStep::Next(pc);
                        }
                        // OSRFRAME18-2 (lane tail7, opt-in): the tail form
                        // refused above for a helper that CAN reach the VM,
                        // given back through its leaf twin where it has one.
                        // The frame stays up; a decline falls through to the
                        // ordinary call below, which still consumes nothing
                        // but this invoke (the `xreturn` is lowered as usual).
                        if let crate::direct_helpers::ThinHelperVmReach::LeafTwin {
                            twin,
                            decline,
                        } = helper_vm_reach
                        {
                            // Where the helper's fast arm is emitted INLINE
                            // below (the `Thread.currentThread` thin prefix,
                            // `--compatible`), that beats any call: no twin.
                            let inline_prefix_serves = callee_entry
                                == self.direct_helpers.thread_current_thread
                                && self.direct_helpers.thread_java_obj_offset != 0
                                && jit_thread_tls_disp() != 0
                                && SpThinPrefix::ThreadCurrentThread.enabled();
                            // TAIL7-19-3 (lane tail8): a wide row's twin site
                            // keeps the frame and consumes no `xreturn`, so it
                            // owes neither of the JMP form's own screens: its
                            // arguments may take Win64 stack slots (the twin
                            // is an ordinary CALL; the five-argument
                            // `vectorizedHashCode`), and its `xreturn` may be a
                            // merge point (the hit path returns by itself and
                            // the decline lowers the call and the `xreturn` as
                            // usual: every JDK 25 `vectorizedHashCode` tail is
                            // a `lookupswitch` default arm falling into a shared
                            // `ireturn`, e.g. `ArraysSupport.hashCodeOfUTF16`).
                            // The wave-7 rows keep the sibling-tail screens.
                            let wide_row = wide_twins
                                && [
                                    self.direct_helpers.vectorized_hash_code,
                                    self.direct_helpers.thread_local_get,
                                    self.direct_helpers.unsafe_cas_int,
                                    self.direct_helpers.unsafe_cas_long,
                                ]
                                .contains(&callee_entry);
                            let twin_site_shape = if wide_row {
                                pc + 3 < code_len
                                    && tail_return_op_matches(ret_type, code[pc + 3])
                                    && !sync_direct_site
                                    && callee_needs_ctx == self.needs_heap
                                    && !self.pc_is_protected(pc)
                            } else {
                                is_sibling_tail && arg_slots.len() <= sibling_reg_limit
                            };
                            if twin_site_shape
                                && retire_cell.is_none()
                                && !inline_prefix_serves
                                && sp_tailcall_enabled()
                            {
                                self.try_emit_sp_tail_leaf_twin(
                                    code[pc + 3],
                                    twin,
                                    decline,
                                    &arg_slots,
                                );
                            }
                        }

                        // ===== INTRINSIC REGION BEGIN: VALUE_OF_INLINE_BOX =====
                        // Round 12 wave 1 (lane `calls`, proposal W17-1): an
                        // out-of-cache `Integer.valueOf` box allocated inline,
                        // as a guarded PREFIX of this unchanged helper call.
                        // Every decline lands on the call below; the hit edge
                        // joins at the result push with the box in RAX. See
                        // `try_emit_value_of_box_prefix`.
                        //
                        // nio20 wave 3, lane L3 (W2L2-1): one prefix table for
                        // both plain direct arms (`SP_THIN_PREFIX_ROWS`; the
                        // `Long.valueOf` twin is round 12 wave 2's C12-2), and
                        // the instance arm's admission contract
                        // (`sp_thin_helper_prefix_admitted`) asked here too: a
                        // thin helper has no `JitInvokeInfo` and no guard row
                        // or judged record at this arm, and the contract adds
                        // the pending-shadow and XMM-operand screens the old
                        // screens did not ask (an XMM operand below the
                        // argument is rewritten by the spill's flush, which
                        // the hit edge skips).
                        let value_of_box_hit = if retire_cell.is_none()
                            && !sync_direct_site
                            && self.sp_thin_helper_prefix_admitted(
                                info_ptr.is_some(),
                                0,
                                0,
                                args_owned_by_callee,
                                callee_needs_ctx,
                            ) {
                            sp_thin_prefix_row(
                                &self.direct_helpers,
                                callee_entry,
                                false,
                                callee_params,
                                ret_type,
                            )
                            .and_then(|prefix| {
                                self.try_emit_sp_thin_prefix(
                                    prefix,
                                    pc,
                                    callee_entry,
                                    callee_params,
                                    ret_type,
                                    &arg_slots,
                                )
                            })
                        } else if ret_type == b'L'
                            // JIT round 19 proposal wave 4, lane strgap4
                            // (STRGAP19-4, `CRATONVM_JIT_STRING_VALUE_OF_THIN`,
                            // default OFF): `String.valueOf(Object)` of an exact
                            // `String` answers its argument inline; everything
                            // else takes this unchanged call.
                            && retire_cell.is_none()
                            && !sync_direct_site
                            && info_ptr.is_some_and(|ip| {
                                // SAFETY: `invoke_info` owns every pointer it
                                // hands out for the life of this compile.
                                let info = unsafe { &*(ip as *const crate::JitInvokeInfo) };
                                super::op_concat::is_string_value_of_object(info)
                            })
                        {
                            match arg_slots.as_slice() {
                                [value] => self.try_emit_string_value_of_prefix(pc, *value, code),
                                _ => None,
                            }
                        } else {
                            None
                        };
                        // ===== INTRINSIC REGION END: VALUE_OF_INLINE_BOX =====

                        // Preserve Java arguments in a contiguous frame range for
                        // the cold callee-sentinel service. A baked direct call has no
                        // dispatch helper frame to recover them from when its own
                        // exception table must run. (A retire-cell site also uses
                        // this range as its dispatch fallback's args buffer.)
                        // None when the callee owns the arguments (gcd d9/d).
                        let service_info = info_ptr.filter(|_| !args_owned_by_callee);
                        let service_args_base = service_info.and_then(|_| {
                            // See `reserve_direct_call_service_slots`: this range MUST
                            // sit above the argument slots `pop_stack` just handed
                            // back, or the copy below reverses the arguments into
                            // themselves and the callee gets arg0 in every slot.
                            // The instance twin (round 13 wave 1) reads its
                            // receiver back from this range AFTER the result is
                            // pushed at `post_pop_spill`, so its range also starts
                            // above that word (an argument homed in a register
                            // does not rewind the cursor, and the two could meet).
                            let floor = if sync_instance_row {
                                args_frame_top.max(post_pop_spill + 8)
                            } else {
                                args_frame_top
                            };
                            let base = self.reserve_direct_call_service_slots(floor, &arg_slots)?;
                            for (i, slot) in arg_slots.iter().enumerate() {
                                self.load_slot_to_reg(R11, *slot);
                                let off = base + ((arg_slots.len() - 1 - i) as i32) * 8;
                                self.emit_store_local(off, R11);
                                // THE SAME CHANNEL THE DISPATCH SITE USES. Its args
                                // buffer pushes each oop among the arguments to
                                // `pending_staged_arg_oops`, so the safepoint map
                                // NAMES it and `collect_live_oop_homes` publishes it
                                // on the shadow stack. This copy is the same shape --
                                // a contiguous frame range, written before the CALL,
                                // still live after it (`emit_inline_callee_deopt_check`
                                // reads it) -- and it named nothing.
                                if direct_call_arg_maps_enabled() && arg_oops[i] {
                                    self.pending_staged_arg_oops.push(off);
                                }
                            }
                            Some(base)
                        });
                        // Only an argument oop with NO named home fails the
                        // safepoint closed now. With the service range reserved
                        // every one of them has one. An owned argument is not
                        // this frame's to name (gcd d9/d).
                        if !args_owned_by_callee
                            && arg_oops.iter().any(|&o| o)
                            && (!direct_call_arg_maps_enabled() || service_args_base.is_none())
                        {
                            self.pending_staged_args_unmapped = true;
                        }
                        // Round 13 wave 1 (lane `sync`): where the instance
                        // twin's receiver lives from the CALL to the release --
                        // argument 0's word of the range above, named in the
                        // CALL's map (`sync_direct_instance_site` requires the
                        // maps and an oop-marked receiver), so a moving
                        // collection inside the callee rewrites it. No range, no
                        // sound release: refuse the method.
                        let sync_receiver_home = match (sync_instance_row, service_args_base) {
                            (false, _) => None,
                            (true, Some(base)) if !arg_slots.is_empty() => {
                                Some(base + ((arg_slots.len() - 1) as i32) * 8)
                            }
                            (true, _) => {
                                self.fail(
                                    "singlepass-codegen/sync-direct-instance-no-receiver-home",
                                );
                                return WalkStep::Return(false);
                            }
                        };
                        // Round-8 wave-3 HIGH fix: stack-arg setup
                        // for direct calls whose total arg count
                        // exceeds ARG_REGS. Uses platform ABI
                        // (Win64 32-byte shadow + stack; SysV pure
                        // stack), 16-byte aligned at the CALL site.
                        let total_sub = self.emit_stack_arg_setup(&arg_slots, callee_needs_ctx);
                        // gci i1/sp (default ON since i2): the setup above and the service
                        // copy were the sources' last readers; everything
                        // after the CALL reads the service range.
                        self.emit_zero_dead_arg_sources(
                            &arg_slots,
                            &arg_oops,
                            post_pop_spill,
                            args_frame_top,
                        );
                        // ARGWIN21-9: and an owned site's dead reference locals.
                        self.emit_owned_call_dead_local_clears(args_owned_by_callee);
                        // Round-8 wave-3: defensive callee-saved spill
                        // before any GC-triggering CALL -- unless the frame
                        // is provably oop-clean here, in which case the
                        // 14-store blind copy publishes nothing and only the
                        // safepoint id is needed. See
                        // `can_elide_direct_call_register_spill`.
                        // `args_frame_resident` is what lets mode 2 admit a
                        // reference argument: the service range makes it
                        // frame-resident for the CONSERVATIVE walk. That is no
                        // longer the whole obligation -- naming the argument in
                        // the map means a moving cycle will rewrite it, and for
                        // that it must also be PUBLISHED, which only the real
                        // spill path emits. So a call that names its argument
                        // oops declines the elision and pays the spill again;
                        // `CRATONVM_JIT_DIRECT_CALL_ARG_MAPS=0` restores the
                        // cheaper, unrelocatable arrangement.
                        let names_arg_oops = direct_call_arg_maps_enabled()
                            && service_args_base.is_some()
                            && arg_oops.iter().any(|&o| o);
                        // gcd d9/d: an owned argument in an ABI register is the
                        // callee's, which its prologue takes into its own frame
                        // before it can safepoint -- so it neither refuses the
                        // elision nor belongs in this frame's spill image.
                        let elision_arg_oops: Vec<bool> = arg_oops
                            .iter()
                            .map(|&o| o && !args_owned_by_callee)
                            .collect();
                        if self.can_elide_direct_call_register_spill(
                            &elision_arg_oops,
                            service_args_base.is_some() && !names_arg_oops,
                            1,
                        ) {
                            self.emit_safepoint_metadata_only();
                        } else {
                            // ARG_REGS only, and only if the service slots
                            // were actually reserved: the copy above stages
                            // through R11, so RAX is untouched here and its
                            // contents are unpublished. Or when the callee owns
                            // the arguments (gcd d9/d): then ARG_REGS hold
                            // nothing of this frame's.
                            self.emit_pre_safepoint_spill_args_published(
                                service_args_base.is_some() || args_owned_by_callee,
                                false,
                            );
                        }
                        // Emit direct CALL to callee entry point -- or, for a
                        // retire-cell site (round 9 wave 9, `evict9`), the same
                        // CALL made through the cell, with the dispatch helper
                        // as the target once an invalidation has cleared it.
                        // Taken only with everything the fallback needs: the
                        // frame's VM context, the helper, the site's
                        // `JitInvokeInfo` and the service range as its args
                        // buffer. Anything missing keeps the plain CALL to the
                        // pinned entry, i.e. exactly the pre-wave-9 code.
                        let info_arity_matches = info_ptr.is_some_and(|info| {
                            // SAFETY: `invoke_info` owns every pointer it hands
                            // out for the life of this compile.
                            let want = unsafe { (*info).num_jit_args };
                            want == arg_slots.len()
                        });
                        let retire_target = match (retire_cell, info_ptr, service_args_base) {
                            (Some(cell_addr), Some(info), Some(args_base))
                                if self.needs_heap
                                    && self.helpers.invoke_dispatch != 0
                                    && info_arity_matches =>
                            {
                                Some((cell_addr, info, args_base))
                            }
                            _ => None,
                        };
                        if let Some((cell_addr, info, args_base)) = retire_target {
                            self.emit_retire_cell_call_target(
                                cell_addr,
                                info,
                                arg_slots.len(),
                                args_base,
                            );
                        } else {
                            self.emit_call_absolute(callee_entry);
                        }
                        // gkr (OOM21-2): the range is the service's alone after
                        // the CALL, except for the synchronized twins' receiver.
                        // A retire cell's dispatch fallback reads it before its
                        // callee runs, and no compiled callee frame then sits
                        // under this one for the collector to judge.
                        if let (false, false, Some(base)) =
                            (sync_direct_site, sync_instance_row, service_args_base)
                        {
                            let ret = self.buf.pos();
                            self.note_service_arg_copies(ret, base, &arg_oops);
                        }
                        // MEGA619-1 (lane tail7, opt-in): a return-tail call
                        // owes no republish; see `sp_lazy_return_republish_enabled`.
                        let lazy_return_republish = retire_target.is_none()
                            && self.sp_lazy_return_republish_admitted(
                                code,
                                pc,
                                3,
                                branch_targets,
                                sync_direct_site,
                                ret_type,
                            );
                        if !lazy_return_republish {
                            self.emit_post_call_rbp_republish();
                        }
                        // T1.1.2 — direct call to a JIT-compiled
                        // callee is still a safepoint: the callee
                        // may allocate and trigger GC transitively.
                        self.emit_oop_map_for_safepoint();
                        self.emit_stack_arg_cleanup(total_sub);
                        // Round 15 wave 1 (lane calls, C14W3-5): the cold
                        // checks below out of line, so the hot path falls
                        // through its sentinel compare instead of jumping
                        // over them. See `sp_call_cold_tail_admitted`.
                        let cold_tail = !sync_direct_site && self.sp_call_cold_tail_admitted(pc);
                        // 2026-09-02: one sentinel compare on the hot
                        // path; the callee-deopt check and the exception
                        // check below keep their own compares on the cold
                        // side (`merged_call_sentinel_enabled`).
                        let merged_keep = (merged_call_sentinel_enabled() && !cold_tail)
                            .then(|| self.emit_call_sentinel_fast_skip());
                        if cold_tail {
                            let service = if let (Some(info), Some(args_base)) =
                                (info_ptr, service_args_base)
                            {
                                SpCallColdService::Args {
                                    info: info as *const crate::JitInvokeInfo,
                                    n: arg_slots.len(),
                                    args_base,
                                }
                            } else if let Some(info) = info_ptr.filter(|_| args_owned_by_callee) {
                                SpCallColdService::OwnedArgs {
                                    info: info as *const crate::JitInvokeInfo,
                                }
                            } else {
                                self.dbg_unserviced_direct_call(
                                    "invokestatic",
                                    pc,
                                    info_ptr.is_some(),
                                    service_args_base.is_some(),
                                );
                                self.fail_unserviced_java_direct_call(info_ptr, service_args_base);
                                SpCallColdService::Unserviced
                            };
                            self.emit_sp_call_cold_tail_branch(service, ret_type);
                        } else if sync_direct_site {
                            // Cold side of `merged_keep`: RAX == i64::MIN. A
                            // wide result can carry those bits for real: peek,
                            // and keep it when nothing is pending (the normal
                            // edge's exit releases then).
                            if merged_keep.is_none() {
                                self.fail("singlepass-codegen/sync-direct-needs-merged-sentinel");
                                return WalkStep::Return(false);
                            }
                            let wide = matches!(ret_type, b'J' | b'D' | b'F');
                            let mut legit_min = Vec::new();
                            if wide {
                                legit_min.push(self.emit_sync_direct_dispatch_threw_peek());
                            }
                            // Shape 2 (c) of the i41 page
                            // (`crate::caller_held_callee_resume_enabled`): the
                            // callee may have LEFT -- a poll exit the VM granted
                            // this caller-held activation. Ask the callee-deopt
                            // service before anything is released: it resumes the
                            // callee holding nothing of this frame's monitor, and
                            // a value takes the normal edge, whose exit is the
                            // one release. A sentinel answer (the callee, or
                            // its resumed frame, threw) falls through to the
                            // exceptional release below. The service copy of the
                            // arguments is read by the service, so the cursor is
                            // handed back only after it.
                            if crate::caller_held_callee_resume_enabled() {
                                // The VM lets a callee of this site leave only
                                // because every such site asks first: one that
                                // cannot (no service helper, no service copy)
                                // fails the compile closed.
                                let (Some(info), Some(args_base)) = (info_ptr, service_args_base)
                                else {
                                    self.fail(
                                        "singlepass-codegen/sync-direct-without-callee-service",
                                    );
                                    return WalkStep::Return(false);
                                };
                                if self.helpers.service_callee_deopt == 0
                                    || !matches!(
                                        crate::sp_ic_deopt_check_mode(),
                                        crate::SpIcDeoptCheck::On
                                    )
                                {
                                    self.fail(
                                        "singlepass-codegen/sync-direct-without-callee-service",
                                    );
                                    return WalkStep::Return(false);
                                }
                                self.emit_inline_callee_deopt_check(
                                    info as *const crate::JitInvokeInfo,
                                    arg_slots.len(),
                                    args_base,
                                );
                                self.emit_cmp_rax_sentinel();
                                legit_min.push(self.emit_jcc_rel32_patch(0x81)); // JNO .keep: a value
                                if wide {
                                    legit_min.push(self.emit_sync_direct_dispatch_threw_peek());
                                }
                            }
                            self.next_spill_offset = post_pop_spill;
                            // The callee threw: release, then take this site's
                            // exception route with the sentinel. The mirror is
                            // the one the enter locked (the slot probe hits, or
                            // the helper answers from the recorded resolution);
                            // the instance twin's receiver is read back from its
                            // service-range word before the cursor reuses it.
                            if !self.emit_sync_direct_release(pc, sync_receiver_home) {
                                self.fail("singlepass-codegen/sync-direct-exit");
                                return WalkStep::Return(false);
                            }
                            self.emit_mov_imm64(RAX, i64::MIN);
                            self.emit_post_invoke_exception_check(b'V');
                            for p in legit_min {
                                self.patch_rel32_to_here(p);
                            }
                        } else {
                            if let (Some(info), Some(args_base)) = (info_ptr, service_args_base) {
                                self.emit_inline_callee_deopt_check(
                                    info as *const crate::JitInvokeInfo,
                                    arg_slots.len(),
                                    args_base,
                                );
                            } else if let Some(info) = info_ptr.filter(|_| args_owned_by_callee) {
                                // gcd d9/d: serviced, with no buffer to read.
                                self.emit_inline_callee_deopt_check_owned_args(
                                    info as *const crate::JitInvokeInfo,
                                );
                            } else {
                                self.dbg_unserviced_direct_call(
                                    "invokestatic",
                                    pc,
                                    info_ptr.is_some(),
                                    service_args_base.is_some(),
                                );
                                self.fail_unserviced_java_direct_call(info_ptr, service_args_base);
                            }

                            // A directly-called compiled callee that throws
                            // (or deopts) returns the `i64::MIN` sentinel.
                            // Without this guard the JIT would push the
                            // sentinel as the return value — for an L/[
                            // return it would then be tagged as an oop
                            // (`mark_top_as_oop` below) and the next deref
                            // of that `0x8000_0000_0000_0000` wild pointer
                            // segfaults. Deopt out so the interpreter routes
                            // the stashed exception through the exception
                            // table instead.
                            self.emit_post_invoke_exception_check(ret_type);
                        }
                        if let Some(keep) = merged_keep {
                            self.patch_rel32_to_here(keep);
                        }
                        // VALUE_OF_INLINE_BOX hit edge: the fresh box is in RAX,
                        // exactly where the helper leaves its answer, so the
                        // push below serves both. It made no call, so it owes
                        // neither the safepoint map / reload nor the sentinel
                        // check above, and skips them (the ArrayList prefix's
                        // contract).
                        if let Some(hit) = value_of_box_hit {
                            self.patch_rel32_to_here(hit);
                        }

                        // Reclaim the spill cursor to the popped-args depth
                        // before the result is pushed, exactly as the
                        // dispatch-helper arm below does with its own
                        // `post_pop_spill`.
                        //
                        // `reserve_direct_call_service_slots` parks the cold
                        // deopt-service copy of the arguments ABOVE the argument
                        // slots (it has to: the slots `pop_stack` handed back are
                        // still live sources for `emit_stack_arg_setup`), which
                        // leaves `next_spill_offset` n slots past the pre-pop top.
                        // Pushing the return value from there parks it above its
                        // semantic operand-stack depth, and every later push in
                        // this basic block inherits the shift. The linear walk
                        // stays self-consistent, so nothing looks wrong -- until
                        // the first branch target after the call, whose depth is
                        // re-established from the bytecode. Writer and reader then
                        // address different slots and the method computes with a
                        // stale one. Measured on ECJ's
                        // `OperandStack.pop(OperandCategory)`, whose `if_icmpeq`
                        // (a tableswitch merge point) compared `TypeBinding.id`
                        // against the expected category instead of
                        // `TypeIds.getCategory(id)`: every JSP compiled after that
                        // method tiered up threw `AssertionError: Unexpected
                        // operand at stack top` (tomcat/ecj-operandstack-*.md).
                        //
                        // Safe to hand the reserved range back: its only consumer
                        // is `emit_inline_callee_deopt_check`, emitted just above.
                        self.next_spill_offset = post_pop_spill;

                        if ret_type != b'V' {
                            if matches!(ret_type, b'D' | b'F') {
                                self.push_from_rax_as_xmm0();
                            } else {
                                self.push_from_rax();
                            }
                            if matches!(ret_type, b'L' | b'[') {
                                self.mark_top_as_oop();
                            }
                        }
                        // The normal edge's release, with the result already on
                        // the simulated stack: the exit's safepoint maps it (a
                        // reference) and its spill saves it (the `ldc` site and
                        // the monitor sequence flush scratch/XMM entries to the
                        // frame first). The instance twin's receiver comes back
                        // from a word the result's push cannot have reached (its
                        // range starts above `post_pop_spill`).
                        if sync_direct_site
                            && !self.emit_sync_direct_release(pc, sync_receiver_home)
                        {
                            self.fail("singlepass-codegen/sync-direct-exit");
                            return WalkStep::Return(false);
                        }
                    }
                } else if let Some(info) = info_ptr {
                    // Non-self invokestatic without a direct target — use dispatch helper
                    // SAFETY: info comes from self.invoke_info, which holds pointers to
                    // JitInvokeInfo structs kept alive by the caller for the duration of compilation.
                    let info_ref = unsafe { &*info };
                    let n = info_ref.num_jit_args;

                    // JIT round 20 (lane strings, page
                    // `r20-identity-hash-is-a-native-call-in-compiled-code`):
                    // `System.identityHashCode(o)` is an INVOKESTATIC, and the
                    // static site reaches THIS arm, not the instance walk's
                    // dispatch arm where the nio21 w1 L5 twin was asked -- so
                    // the twin never emitted (`identity-hash-inline
                    // (sp-dispatch)` count 0 on m7, `idhash` 59 ns). Same
                    // prefix, same contract: emitted before anything is popped,
                    // declines land on the unchanged dispatch below, the hit
                    // joins the result push with the hash in RAX.
                    let identity_hash_hit = self
                        .try_emit_sp_identity_hash_dispatch_prefix(pc, info_ref)
                        .or_else(|| self.try_emit_sp_byte_array_view_dispatch_prefix(pc, info_ref));

                    // value-stack-usize-underflow-nio-worker-panic fix:
                    // snapshot the pre-pop operand stack (see the matching
                    // invokevirtual/interface fix below) so a post-invoke
                    // exception/deopt guard's `Reinterpret` resume at this
                    // bci has the args this invokestatic needs, instead of
                    // underflowing on an empty stack.
                    if crate::deopt_real_enabled() {
                        self.snapshot_pre_intrinsic_call(
                            pc,
                            crate::deopt::DeoptReason::ReceiverTypeChanged,
                        );
                    }

                    // Capture spill offset BEFORE popping to prevent
                    // the args buffer from overlapping source Frame slots.
                    let pre_pop_spill = self.next_spill_offset;
                    let (arg_slots, arg_oops) = self.pop_invoke_args(n);
                    // Cursor at the popped-args depth — the reclaim after
                    // the call restores THIS level (not `pre_pop_spill`).
                    // Restoring to pre_pop left the return value parked
                    // n slots above its semantic depth, permanently
                    // inflating the cursor by n per non-void dispatch
                    // site; across a long method the creep walked the
                    // args buffer past `sub rsp, frame_size` into the
                    // callee's stack (Bug 4: testAdHocData's FFT receiver
                    // zeroed by the next helper call's frame).
                    let post_pop_spill = self.next_spill_offset;

                    let args_base_offset = pre_pop_spill;
                    if n > 0 {
                        let Some(args_end) = self.checked_spill_range_end(args_base_offset, n)
                        else {
                            return WalkStep::Return(false);
                        };
                        self.next_spill_offset = args_end;
                        // Store args in reverse offset order so they form
                        // a contiguous ascending-address buffer:
                        //   arg[0] at [rbp - highest_offset] (lowest addr)
                        //   arg[n-1] at [rbp - args_base_offset] (highest addr)
                        // This is necessary because modrm_rbp_disp negates
                        // the offset, so higher offsets map to lower addresses.
                        for (i, slot) in arg_slots.iter().enumerate() {
                            let buf_offset = args_base_offset + ((n - 1 - i) as i32) * 8; // Cast: x86-64 immediate encoding
                            self.load_slot_to_reg(RAX, *slot);
                            self.emit_store_local(buf_offset, RAX);
                            // This argument leaves the simulated operand
                            // stack here; if it is a reference, the
                            // safepoint map below is the only thing that
                            // can still name it.
                            if arg_oops[i] {
                                self.pending_staged_arg_oops.push(buf_offset);
                            }
                        }
                        // gci i1/sp (default ON since i2): the popped sources are dead
                        // copies now (g5jm's holder shape).
                        self.emit_zero_dead_arg_sources(
                            &arg_slots,
                            &arg_oops,
                            post_pop_spill,
                            pre_pop_spill,
                        );
                    }
                    self.emit_load_vm_context(ARG_REGS[0]);
                    self.emit_mov_imm64(ARG_REGS[1], info as *const _ as i64); // Cast: function pointer for JIT call target
                    if n > 0 {
                        // LEA to the highest offset = lowest address = start of buffer
                        let buf_start = args_base_offset + ((n as i32) - 1) * 8; // Cast: x86-64 immediate encoding
                        self.emit_lea_frame_slot(ARG_REGS[2], buf_start);
                    } else {
                        self.emit_xor_reg_self(ARG_REGS[2]);
                    }
                    self.emit_mov_imm32_sx(ARG_REGS[3], n as i32); // Cast: x86-64 immediate encoding
                                                                   // Round-8 wave-3: defensive callee-saved spill before any
                                                                   // GC-triggering CALL -- unless the caller frame is
                                                                   // provably oop-clean here. Every argument of this site,
                                                                   // reference or not, was just stored into the helper's
                                                                   // args buffer at `args_base_offset`, and each oop among
                                                                   // them was pushed to `pending_staged_arg_oops` so the map
                                                                   // below NAMES it: `args_frame_resident` is
                                                                   // unconditionally true here, which is a stronger
                                                                   // guarantee than the direct sites' service slots.
                    if self.can_elide_direct_call_register_spill(&arg_oops, true, 2) {
                        self.emit_safepoint_metadata_only();
                    } else {
                        // Every argument is in the helper's args buffer and
                        // the oops among them are named in the map below;
                        // ARG_REGS carry the helper ABI (heap/info/buf/count),
                        // never a Java oop. RAX is published only when the
                        // staging loop -- which writes through RAX -- ran.
                        self.emit_pre_safepoint_spill_args_published(true, n > 0);
                    }
                    // (vm_ptr, info_ptr, args_ptr, num_args) -> result | sentinel.
                    cratonvm_jit_api::assert_helper_call_shape!(
                        "invoke_dispatch",
                        int_args = 4,
                        returns_value = true
                    );
                    self.emit_call_absolute(self.helpers.invoke_dispatch);
                    // T1.1.2 — invoke dispatch is a full safepoint:
                    // the callee may allocate, trigger GC, or throw.
                    // Record an oop map for the operand stack state
                    // that survives the call (args are already popped,
                    // return value not yet pushed).
                    self.emit_oop_map_for_safepoint();

                    // After the dispatch returns, check whether the
                    // static callee threw a Java exception. `jit_invoke_
                    // dispatch` returns `i64::MIN` (and stashes the
                    // exception in `JIT_PENDING_EXCEPTION`) when the
                    // callee throws. Without this guard — which the
                    // invokevirtual/invokespecial paths already have —
                    // the JIT pushes the `i64::MIN` sentinel as the
                    // return value; for an L/[ static method it is then
                    // tagged as an oop (`mark_top_as_oop` below) and the
                    // next deref of that wild `0x8000_0000_0000_0000`
                    // pointer segfaults (the Tomcat boot regression).
                    // Deopt out so the interpreter routes the stashed
                    // exception through the method's exception table.
                    self.emit_post_invoke_exception_check(info_ref.return_type);

                    // The identity-hash prefix's hit edge: the int hash is in
                    // RAX where the dispatch leaves its result; it made no
                    // call, so it skips the map and the exception check (the
                    // instance dispatch arm's contract).
                    if let Some(hit) = identity_hash_hit {
                        self.patch_rel32_to_here(hit);
                    }

                    // Reclaim spill slots used for invoke args AND the
                    // popped arg values — see `post_pop_spill` above.
                    self.next_spill_offset = post_pop_spill;

                    if info_ref.return_type != b'V' {
                        if matches!(info_ref.return_type, b'D' | b'F') {
                            self.push_from_rax_as_xmm0();
                        } else {
                            self.push_from_rax();
                        }
                        // T1.1.2 — the return value is an object
                        // reference iff the descriptor ends in `L`
                        // or `[`. Tag it so the next safepoint
                        // records it as a live oop.
                        if matches!(info_ref.return_type, b'L' | b'[') {
                            self.mark_top_as_oop();
                        }
                    }
                } else {
                    // Self-recursive call (no invoke_info, no direct_call)
                    //
                    // t1 wave 3, lane X: positive, not inferred. A door that
                    // marks its static self-calls
                    // (`BackendRequest::static_self_calls_registered`) has put
                    // a `JIT_SELF_CALL_GUARD` row at every site meant for this
                    // arm (the instance twin's row is the same marker). A site
                    // without one is a table-builder or emitter-filter gap,
                    // and lowering it here would `CALL` this body's own entry
                    // with another callee's operands (the SBR4 `binarySort`
                    // shape): refuse instead.
                    if self.static_self_calls_registered && !self.has_self_call_marker(pc) {
                        self.fail("singlepass-codegen/static-invoke-without-registration");
                        return WalkStep::Return(false);
                    }
                    let n = self.num_params;

                    // Check for tail call: invokestatic self at PC, xreturn at PC+3.
                    // Never inside a try region — the tail form tears this
                    // frame down, so a throw from the self-recursive callee
                    // would bypass the handler covering this pc
                    // (see `pc_is_protected`).
                    // Never when the `xreturn` is a branch target: the
                    // tail form consumes that PC without emitting it, so
                    // another edge into it has no native offset to be
                    // patched to (see the sibling-tail arm above for the
                    // full argument).
                    let is_tail_call = pc + 3 < code_len
                        // An instance self-call handed over by
                        // `walk_invoke_instance` (round 9 wave 10) keeps the
                        // raw CALL: the tail form is the opt-in static shape.
                        && code[pc] == 0xb8
                        && matches!(code[pc + 3], 0xac..=0xb0) // ireturn..areturn
                        && !branch_targets[pc + 3]
                        && !self.pc_is_protected(pc)
                        // OPT-IN: `CRATONVM_JIT=self-tailcall`
                        // (`CRATONVM_JIT_SELF_TAILCALL`, a `runtime_flag_on`
                        // read -- default OFF since 2026-08-20, see
                        // `licm.rs::self_tailcall_enabled`). Off keeps the raw
                        // self-recursive CALL below, one native frame per
                        // activation, which is the HotSpot-faithful answer.
                        // (This comment used to say "on is the default".)
                        && self_tailcall_enabled();

                    // jit-invokedynamic-groovy-regression fix: a method
                    // containing an invokedynamic site THAT LOWERS TO THE
                    // unconditional reason-8 trap must NEVER machine-CALL
                    // its own entry: a trap in the INNER recursive
                    // invocation stashes a frame whose method identity
                    // equals this method's, so the dispatch-helper resume
                    // above this frame could not distinguish the inner
                    // invocation's frame from the outer's and would resume
                    // the wrong one (dropping the outer continuation). The
                    // tail-JMP form below is exempt (it reuses the SAME
                    // frame, so the stash genuinely describes the one live
                    // invocation). For the non-tail raw CALL, bail the
                    // whole compile — correctness first.
                    //
                    // The test is `has_trapping_indy_site`, not "has any indy
                    // site". Since the indy BRIDGE landed (the 0xba arm below,
                    // `StringConcatFactory` / `LambdaMetafactory` /
                    // `SwitchBootstraps` / `ObjectMethods`), a bridged site
                    // emits a plain fallible helper call and NO trap at its
                    // bci — the same shape as every other call in this method,
                    // whose deopt points this arm has never refused. So there
                    // is nothing for the frame-identity hazard above to be
                    // about, and `!indy_info.is_empty()` refused the whole
                    // compile of every self-recursive method that creates a
                    // lambda or concatenates a string. `driver.rs` already
                    // derives `CompiledMethod::has_indy_trap` from exactly this
                    // predicate for the CALLERS of such a method; this is the
                    // same question asked about its own body.
                    if self.has_trapping_indy_site()
                        && !(is_tail_call && self.body_entry_offset > 0)
                    {
                        return WalkStep::Return(false);
                    }

                    // value-stack-usize-underflow-nio-worker-panic fix:
                    // snapshot the pre-pop operand stack for the non-tail
                    // path below (see the matching invokevirtual/interface
                    // fix elsewhere in this match arm) — the tail-call form
                    // JMPs and never reaches `emit_post_invoke_exception_check`,
                    // so this is a no-op for it beyond the idempotent
                    // `deopt_box_ptr_by_bci` insert.
                    if crate::deopt_real_enabled() {
                        self.snapshot_pre_intrinsic_call(
                            pc,
                            crate::deopt::DeoptReason::ReceiverTypeChanged,
                        );
                    }
                    // `pop_invoke_args` hands the argument SLOTS back to the spill
                    // allocator while they are still live sources for
                    // `emit_stack_arg_setup` far below, so the deopt-service copy
                    // has to be placed ABOVE them. Remember the pre-pop top, and
                    // separately the post-pop top the return value belongs at —
                    // the two direct-call arms' `args_frame_top`/`post_pop_spill`
                    // pair, for the same two reasons.
                    let args_frame_top = self.next_spill_offset;
                    let (arg_slots, arg_oops) = self.pop_invoke_args(n);
                    let post_pop_spill = self.next_spill_offset;
                    // THE SAME CHANNEL THE TWO DIRECT-CALL SITES USE, for the one
                    // invoke arm `CRATONVM_JIT_DIRECT_CALL_ARG_MAPS` did not reach.
                    //
                    // This arm used to raise `pending_staged_args_unmapped` for any
                    // reference argument, on the stated grounds that the value had
                    // been staged "into an area no oop map can name". At the
                    // safepoint that consumes the flag it has not: `pop_invoke_args`
                    // hands back the arguments' FRAME slots, the non-tail form's
                    // stack-guard safepoint and recursive CALL are emitted below
                    // this point, and `emit_stack_arg_setup` — the step that does
                    // move them into the un-nameable outgoing-ABI area — runs later
                    // still. So the slots are live, frame-resident and nameable
                    // exactly where the refusal was being raised.
                    //
                    // Naming them is also what PUBLISHES them:
                    // `collect_live_oop_homes` reads `pending_staged_arg_oops`, so
                    // the shadow stack carries them and the band verifier stops
                    // finding a movable word nothing published. See
                    // `self_call_arg_maps_enabled` for the measurement.
                    //
                    // An argument whose home is NOT a frame slot still fails
                    // closed: a register/scratch/xmm-resident reference is precisely
                    // what a frame-slot map cannot describe.
                    let staged_self_args_mark = self.pending_staged_arg_oops.len();
                    // The tail form below un-stages this site (it moves the
                    // arguments into the parameter homes the entry mask names),
                    // so it needs the fail-closed bit as it was BEFORE this
                    // site raised it, not just the list truncated.
                    let staged_self_args_unmapped_before = self.pending_staged_args_unmapped;
                    if self_call_arg_maps_enabled() {
                        let mut unnameable = false;
                        for (i, slot) in arg_slots.iter().enumerate() {
                            if !arg_oops.get(i).copied().unwrap_or(false) {
                                continue;
                            }
                            match slot {
                                StackSlot::Frame(off) => {
                                    self.pending_staged_arg_oops.push(*off);
                                }
                                _ => unnameable = true,
                            }
                        }
                        if unnameable {
                            self.pending_staged_args_unmapped = true;
                        }
                    } else if arg_oops.iter().any(|&o| o) {
                        self.pending_staged_args_unmapped = true;
                    }

                    if is_tail_call && self.body_entry_offset > 0 {
                        // Tail-call optimization: load args into parameter locals
                        // and JMP back to body entry (skip prologue). The
                        // homes, and the order the moves may be made in, are
                        // the prologue's — see `emit_self_tail_param_moves`.
                        self.emit_self_tail_param_moves(&arg_slots);
                        // Un-stage BEFORE the back-edge poll below, not after
                        // the JMP where the truncation used to sit: the poll is
                        // a safepoint, and its map would otherwise consume the
                        // argument homes this site staged -- homes the moves
                        // above have just vacated -- and, for an argument that
                        // was not frame-resident, the fail-closed bit, marking
                        // the poll's map incomplete for a state the entry mask
                        // describes exactly. See the comment after the JMP for
                        // why the staged list must not outlive this site.
                        self.pending_staged_arg_oops.truncate(staged_self_args_mark);
                        self.pending_staged_args_unmapped = staged_self_args_unmapped_before;
                        // The JMP below is a BACK-EDGE that skips the
                        // prologue, and with it the method-entry poll, so a
                        // self-tail-recursive method whose only loop is the
                        // recursion ran with no poll at all
                        // (`NOTES-invoke.md` proposal 5). See
                        // `emit_self_tail_safepoint_poll`.
                        self.emit_self_tail_safepoint_poll();
                        // JMP rel32 back to body entry
                        self.buf.emit_byte(0xE9); // JMP rel32
                        let jmp_offset = self.buf.pos();
                        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                        // Patch: target = body_entry_offset
                        let rel = self.body_entry_offset as i32 - (jmp_offset as i32 + 4); // Cast: x86-64 rel32 displacement
                        let pos = self.buf.pos();
                        self.buf.try_patch_i32(jmp_offset, rel).ok(); // on Err try_patch_i32 set buf.overflowed; compile bails
                        let _ = pos;

                        // THE TAIL FORM PUBLISHES NO STAGED ARGUMENT. The
                        // arguments were just loaded into this method's own
                        // parameter locals, which the entry mask names from here
                        // on, and `reset_spills` below hands their old slots
                        // straight back to the spill allocator. Anything left
                        // pending would be consumed by a LATER, unrelated
                        // safepoint and would name a slot the cursor has already
                        // given to something else — the mirror of the defect
                        // this staging fixes. The truncation to the mark (and
                        // the fail-closed bit's restore) now happen BEFORE the
                        // JMP, ahead of the back-edge poll, which is the one
                        // safepoint that could otherwise have taken them.

                        // Skip the following xreturn — we already jumped
                        pc += 3; // invokestatic
                        pc += 1; // xreturn
                        self.reset_spills();
                        return WalkStep::Next(pc);
                    }

                    // Preserve the outgoing Java arguments in a contiguous frame
                    // range for the cold callee-sentinel service below. A raw
                    // `CALL` has no dispatch-helper frame to recover them from
                    // when the callee's own exception table has to run, and this
                    // callee IS this method, so the range is what its handler is
                    // re-entered with. The two direct-call arms and the inline
                    // splice all do exactly this; only the self-call arm did not.
                    //
                    // `None` with a staged info (no room, or a range that would
                    // overlap the argument slots it reads) refuses the compile
                    // since JIT round 20 (below): the propagating route it used
                    // to keep handed an inner activation's stashed frame to the
                    // outer activation's sink. `None` with no info (a unit-test
                    // compile) keeps the pre-fix route.
                    //
                    // `needs_heap` is a HARD precondition, not a nicety: the
                    // service is called with this frame's `vm_ptr` slot
                    // (`emit_inline_callee_deopt_check` loads ARG0 from
                    // `heap_local_offset`), and a frame that reserved no such
                    // slot would pass whatever that offset happens to address.
                    // Both planner arms that bind a self-call set it — but the
                    // TAIL-shaped arm does not, and its site still reaches this
                    // code as an ordinary raw `CALL` whenever
                    // `self_tailcall_enabled()` is off (the default), so the
                    // info can be staged for a frame that has no slot.
                    let self_service_args_base = (self.self_call_invoke_info != 0
                        && self.needs_heap)
                        .then(|| self.reserve_direct_call_service_slots(args_frame_top, &arg_slots))
                        .flatten();
                    // JIT round 20, lane stash
                    // (`jr20-a-self-call-without-service-slots-hands-an-inner-activations-frame-to-the-outer-sink`):
                    // a staged info with no service range refuses the compile, as
                    // the other direct arms do (`fail_unserviced_java_direct_call`).
                    // Left unserviced, the INNER activation's stashed frame
                    // propagated past every enclosing activation of this same
                    // method and was resumed by the outer sink as the OUTER
                    // activation's trap (same key, so no identity test can tell
                    // them apart): a wrong answer. Not reachable on the round-20
                    // tree by reading -- `jit_scan` sets `needs_heap` for every
                    // invoke, every self-call arm stages the info, the range is
                    // bumped past the arguments so it cannot overlap them, and an
                    // exhausted spill area already fails the compile -- so this is
                    // a guard against a future change to any of the three.
                    if self.self_call_invoke_info != 0 && self_service_args_base.is_none() {
                        self.fail("direct-call-service-slots");
                    }
                    if let Some(base) = self_service_args_base {
                        for (i, slot) in arg_slots.iter().enumerate() {
                            self.load_slot_to_reg(R11, *slot);
                            // Cast: x86-64 immediate encoding.
                            let off = base + ((arg_slots.len() - 1 - i) as i32) * 8;
                            self.emit_store_local(off, R11);
                            // The direct-call arms' channel verbatim: a contiguous
                            // frame range written before the CALL and read after
                            // it, so each reference in it is named by this call's
                            // safepoint map and published on the shadow stack.
                            if direct_call_arg_maps_enabled()
                                && arg_oops.get(i).copied().unwrap_or(false)
                            {
                                self.pending_staged_arg_oops.push(off);
                            }
                        }
                    }

                    // BUG-1 companion — native-stack headroom guard for the
                    // direct self-recursive CALL below. Historically every
                    // NON-tail self-recursive site was routed through
                    // `jit_invoke_dispatch` purely so the dispatch depth
                    // guard could convert runaway compiled recursion into a
                    // catchable StackOverflowError; that made each recursive
                    // call pay the full dispatch-helper round trip (the
                    // dominant cost of fib/binarytrees-style recursion).
                    // With the dedicated guard helper wired, the site stays
                    // a direct CALL and pays one cheap leaf helper call:
                    //   MOV  ARG0, [RBP - heap_local]   ; vm_ptr
                    //   CALL self_call_stack_guard      ; 0 = ok
                    //   TEST RAX, RAX
                    //   JNZ  merge                      ; RAX = i64::MIN →
                    //                                   ; post-invoke check
                    //                                   ; routes the stashed
                    //                                   ; StackOverflowError
                    // The guard's overflow arm allocates (SOE construction),
                    // so it is bracketed like a call safepoint: defensive
                    // callee-saved spill before, and — under precise/shadow
                    // modes — an oop map (whose shadow RELOAD pairs with the
                    // spill's PUSH) at its return PC. The JNZ target sits
                    // AFTER `emit_stack_arg_cleanup`, so the overflow path
                    // skips arg setup + CALL + cleanup as one balanced unit
                    // (no RSP adjustment happens on that path). `arg_slots`
                    // are call-safe across the guard CALL: the
                    // `flush_scratch_registers()` at the 0xb8 arm entry
                    // moved scratch GPR/XMM stack slots into frame slots.
                    // Requires `needs_heap` (the routing in `try_compile`
                    // sets it for every raw-routed self-call site) — the
                    // vm_ptr frame slot is what the guard is called with.
                    // Unwired helper (tests, historical callers): emits
                    // nothing, byte-identical legacy code.
                    let guard_skip_patch =
                        if self.helpers.self_call_stack_guard != 0 && self.needs_heap {
                            // perf/throughput-20260710 -- INLINE floor fast
                            // path: the prologue cached this thread's
                            // native-stack floor in a frame slot, so the
                            // common case is:
                            //   CMP RSP, [rbp - floor_slot]
                            //   JA  <skip helper guard>   (headroom ok)
                            // OSR-entered frames have the slot initialised
                            // to usize::MAX by the trampoline (`RSP > MAX`
                            // is unsatisfiable), so their first self-call
                            // takes the slow path, which swaps in the real
                            // floor. The skipped block is the guard CALL
                            // plus its safepoint spill and shadow
                            // push/reload bracketing (skipped TOGETHER, so
                            // the shadow stack stays balanced); the
                            // recursive CALL below still emits its own
                            // spill + oop map, so GC coverage of the
                            // actual recursion is unchanged. This constant
                            // was the dominant per-level cost of
                            // fib/binarytrees-style recursion.
                            let fast_skip = if self.stack_floor_slot_off != 0 {
                                self.emit_cmp_r64_rbp_local(RSP, self.stack_floor_slot_off);
                                Some(self.emit_jcc_rel32_patch(0x87)) // JA
                            } else {
                                None
                            };
                            if self.gc_inert_selfrec {
                                self.emit_pre_safepoint_spill_without_shadow();
                            } else {
                                // gcki-ngr: the stack guard is a Rust helper
                                // (as in `emit_cycle_edge_cell_call`).
                                self.emit_pre_safepoint_spill_before_helper_call();
                            }
                            // An OSR-entered frame's placeholder floor is
                            // replaced by the real one on its first self-call
                            // here, so later calls take the inline fast path.
                            self.emit_osr_placeholder_floor_refresh();
                            self.emit_load_vm_context(ARG_REGS[0]);
                            // A rejected <init> must withdraw the new receiver before
                            // allocating its StackOverflowError. Other calls pass zero.
                            // SAFETY: a non-zero `self_call_invoke_info` is the address of
                            // the `JitInvokeInfo` of the method being compiled, which the
                            // caller of this compile keeps alive for the whole compile.
                            if self.self_call_invoke_info != 0
                                // SAFETY: the method's invoke metadata stays live throughout compilation.
                                && unsafe {
                                    (*(self.self_call_invoke_info as *const crate::JitInvokeInfo))
                                        .method_name
                                        == "<init>"
                                }
                            {
                                self.load_slot_to_reg(ARG_REGS[1], arg_slots[0]);
                            } else {
                                self.emit_xor_reg_self(ARG_REGS[1]);
                            }
                            cratonvm_jit_api::assert_helper_call_shape!(
                                "self_call_stack_guard",
                                int_args = 2,
                                returns_value = true
                            );
                            self.emit_call_absolute(self.helpers.self_call_stack_guard);
                            if self.precise_maps || self.shadow_enabled {
                                self.emit_oop_map_for_safepoint();
                            }
                            self.emit_test_r64_r64(RAX);
                            let jne_merge = Some(self.emit_jcc_rel32_patch(0x85)); // JNE merge
                            if let Some(skip) = fast_skip {
                                self.patch_rel32_to_here(skip);
                            }
                            jne_merge
                        } else {
                            None
                        };
                    // Round-8 wave-3 HIGH fix: stack-arg setup for
                    // self-recursive direct calls past ARG_REGS.
                    let total_sub = self.emit_stack_arg_setup(&arg_slots, self.needs_heap);
                    // Round-8 wave-3: defensive callee-saved spill
                    // before the recursive CALL (which transitively
                    // can allocate and reach a GC safepoint).
                    if self.gc_inert_selfrec {
                        // The callee is this same poll-free,
                        // allocation-free method. The direct edge is not a
                        // safepoint, so publishing roots here would be pure
                        // overhead. The overflow helper above retains its
                        // cold safepoint protocol.
                    } else if self.can_elide_self_call_register_spill() {
                        self.emit_safepoint_metadata_only();
                    } else {
                        self.emit_pre_safepoint_spill();
                    }
                    // Normal self-call via CALL (rel32, patched
                    // post-emission).
                    self.buf.emit_byte(0xE8);
                    let call_patch = self.buf.pos();
                    self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                    self.self_call_patches.push(call_patch);
                    // Round 9 wave 2 (call overhead): on the inline-TLS
                    // frame-record shape, republish ONLY the RBP half. The
                    // callee of this CALL is this very buffer's entry
                    // (`patch_self_calls`), whose prologue published THIS
                    // compile's `compile_id` as the identity half; every
                    // later writer inside the callee (its own post-call
                    // republishes, the Rust dispatch brackets'
                    // `top_cm_id_mirror_write` restore) writes that same
                    // id back before control returns here. So the identity
                    // store `emit_post_call_rbp_republish` would add is a
                    // byte-for-byte no-op (see
                    // `docs/feature-designs/jit-ir-optimizations.md`
                    // §3). The helper-call fallback shape is unchanged.
                    if self.precise_maps
                        && self.helpers.frame_record != 0
                        && self.inline_rbp_tls_disp != 0
                    {
                        self.emit_mov_tls_disp32_rbp(self.inline_rbp_tls_disp as u32);
                    } else {
                        self.emit_post_call_rbp_republish();
                    }
                    // Stage A (precise oop maps, B-K fix) — a self-recursive
                    // compiled call IS a GC-capable safepoint (the callee
                    // allocates: this is exactly bintrees18's recursive
                    // `make()`). The neighbour direct/dispatch invoke sites
                    // already pair `emit_pre_safepoint_spill` with an oop map
                    // at the return PC; this site historically spilled but
                    // recorded NO map, so the precise relocation path could
                    // not remap this frame (`remap_one_jit_frame` found no
                    // entry for the active sp-id) — the documented gap #1.
                    // Gated behind `precise_maps`: emits zero bytes and no
                    // metadata on the default path, so it is byte-identical
                    // gate-OFF; gate-ON it records the map AND the paired
                    // post-safepoint register reload (Stage 4 / G5).
                    // Also required under `shadow_enabled`: the shadow-stack
                    // PUSH happened in `emit_pre_safepoint_spill`, so its
                    // paired RELOAD (inside `emit_oop_map_for_safepoint`) must
                    // run here too, else the shadow stack grows unbalanced
                    // through this recursive call → unbounded pinning → OOM.
                    if !self.gc_inert_selfrec && (self.precise_maps || self.shadow_enabled) {
                        self.emit_oop_map_for_safepoint();
                    }
                    self.emit_stack_arg_cleanup(total_sub);
                    // Give the CALLEE's own exception table one last look before
                    // the sentinel below is treated as THIS frame's unwind.
                    //
                    // This is the fix for `R10SelfRecCatch`. The callee here is
                    // this same method one activation down, and it returns
                    // `i64::MIN` both for a trap inside itself and for a rethrow
                    // out of ITS nested calls — exactly what every other raw-CALL
                    // route in this backend services (`emit_inline_direct_call`,
                    // the two `invokestatic`/instance direct arms, the retire
                    // cell). This arm never did, because the planner gives a
                    // self-call site no `invoke_info` row; `self_call_invoke_info`
                    // is the channel that carries one anyway.
                    //
                    // Without it, `emit_post_invoke_exception_check` below is the
                    // only consumer, and at a bci with no precise exceptional
                    // frame it is the SHARED SENTINEL EXIT: it stamps this
                    // frame's own throw bci over the callee's and returns the
                    // sentinel through the epilogue. Correct when the caller is
                    // the interpreter, which then routes through this method's
                    // table — but the caller of a self-recursive `CALL` is
                    // compiled code, which repeats the same overwrite. So the
                    // exception skipped every compiled activation's `catch` and
                    // was first resolved at the topmost compiled frame.
                    //
                    // Deliberately BEFORE the stack-guard merge point: on the
                    // guard's overflow edge the callee never ran, so there is no
                    // callee stash to service and the pending `StackOverflowError`
                    // is genuinely this frame's. That edge keeps its documented
                    // route straight into the sentinel check.
                    if let (info, Some(base)) = (self.self_call_invoke_info, self_service_args_base)
                    {
                        self.emit_inline_callee_deopt_check(
                            info as *const crate::JitInvokeInfo, // Cast: staged address
                            arg_slots.len(),
                            base,
                        );
                    }
                    // Stack-guard merge point: the overflow JNE lands here
                    // with RAX = i64::MIN, flowing straight into the
                    // sentinel check below (exactly as if the callee threw),
                    // and past the service above for the reason given there.
                    if let Some(p) = guard_skip_patch {
                        self.patch_rel32_to_here(p);
                    }
                    // A self-recursive compiled call that throws (or
                    // deopts) returns the `i64::MIN` sentinel — same
                    // hazard as the direct/dispatch invokestatic paths
                    // above. Guard it so the sentinel is never pushed
                    // (and never tagged as an oop) as a return value.
                    //
                    // The callee here IS this method, so its return type is
                    // the method's own, and `method_key`
                    // ("Class.name:descriptor") carries it. This used to pass
                    // `b'I'` on the claim that single-pass does not have the
                    // descriptor — while the void-push fix eleven lines below
                    // was already reading it out of `method_key`. `b'I'` is
                    // the plain `CMP; JE` arm, so a self-recursive
                    // `long`/`double` method that legitimately returned
                    // `Long.MIN_VALUE` — or `-0.0`, whose bits ARE `i64::MIN`
                    // — was taken for a throw. The `J`/`D`/`F` arm
                    // disambiguates with the out-of-band `dispatch_threw`
                    // peek on the (cold) sentinel-equal edge only. An empty
                    // key (the legacy test wrapper) keeps `b'I'`.
                    let self_ret_ty = self.own_return_tag();
                    let sentinel_check_ty = match self_ret_ty {
                        Some(t @ (b'J' | b'D' | b'F')) => t,
                        _ => b'I',
                    };
                    self.emit_post_invoke_exception_check(sentinel_check_ty);
                    // Hand the service range back BEFORE the result is pushed.
                    //
                    // `reserve_direct_call_service_slots` parks its copy ABOVE
                    // the argument slots (it must: those are still live sources
                    // for `emit_stack_arg_setup`), which leaves the cursor n
                    // slots past the pre-pop top. Pushing the return value from
                    // there parks it above its semantic operand-stack depth and
                    // every later push in this block inherits the shift — self-
                    // consistent until the first branch target, whose depth is
                    // re-established from the bytecode, after which writer and
                    // reader address different slots. That is the ECJ
                    // `OperandStack.pop` miscompile the invokestatic direct arm
                    // documents at length; this arm now reserves too, so it
                    // needs the same reclaim. Safe: the range's only consumer is
                    // the `emit_inline_callee_deopt_check` emitted just above.
                    //
                    // Unconditional — with no reservation the cursor is already
                    // exactly `post_pop_spill`, so this is a no-op on that path.
                    self.next_spill_offset = post_pop_spill;
                    // VOID self-recursive fix (ES SortingDigestTests -Jit
                    // on): this push was unconditional, so a `void`
                    // self-recursive callee (DualPivotQuicksort.sort) left
                    // a PHANTOM entry on the simulated operand stack after
                    // every non-tail self-call. The extra entry shifts the
                    // canonical spill-slot layout for everything downstream
                    // of the next merge point, so later loads read
                    // neighbouring slots (an array ref read as a double →
                    // heap addresses stored into double[] elements,
                    // deterministic mis-sorts and garbage AIOOBE indices —
                    // no GC involved). The method's own return type IS
                    // available via `method_key` ("Class.name:descriptor"),
                    // so only push a return value when there is one. When
                    // `method_key` is empty (unit-test compiles) keep the
                    // historical push — those callers never compile void
                    // self-recursive methods. (`self_ret_ty` is read out of
                    // `method_key` just above, for the sentinel check.)
                    if self_ret_ty != Some(b'V') {
                        self.push_from_rax();
                        // The callee IS this method, so its return type is
                        // the method's own. A reference result MUST be
                        // tagged: `collect_live_oop_homes` publishes only
                        // marked operand entries, so an untagged reference
                        // left on the operand stack across a later
                        // GC-capable call is invisible to the shadow stack
                        // — while `moving_young_safepoint_coverage_complete`
                        // still certifies the frame, because it only checks
                        // that MARKED entries have frame/register homes.
                        //
                        // That combination is the measured heap corruption
                        // in `moving-young-gen-drops-jit-held-oops-FIXED.md`:
                        // `BinTreesClassic.bottomUpTree` keeps the result of
                        // its first recursive call — an entire subtree — on
                        // the operand stack across its second, and a moving
                        // young cycle neither marked nor rewrote it.
                        match self_ret_ty {
                            Some(b'L') | Some(b'[') => self.mark_top_as_oop(),
                            // No descriptor (the legacy `compile` test
                            // wrapper passes an empty `method_key`): we
                            // cannot tell whether this is a reference, so
                            // the mark vector is no longer exact and this
                            // frame must not certify moving-young coverage.
                            // Fail-closed costs a non-moving cycle; guessing
                            // costs the heap.
                            None => self.stack_oop_marks_exact = false,
                            _ => {}
                        }
                    }
                }
                pc += 3;
            }
            _ => {
                self.fail("singlepass-codegen/walk-family-misdispatch");
                return WalkStep::Return(false);
            }
        }
        WalkStep::Next(pc)
    }

    /// The caller-held synchronized INSTANCE direct CALL planned at `pc`
    /// (round 13 wave 1, lane `sync`; `crate::JIT_SYNC_DIRECT_INSTANCE_TAG`), as
    /// `(jit_args, exact_class_id)` -- receiver included, and `0` for a
    /// statically bound site -- when this frame can lower it with the monitor
    /// held. `None` leaves the site on the dispatch its `JitInvokeInfo`
    /// describes (`walk_invoke_instance` drops the row).
    ///
    /// The lowering keeps the receiver across the CALL in argument 0's word of
    /// the direct-call service range, which the CALL's map names only under
    /// `direct_call_arg_maps_enabled` and only for an operand the model marks a
    /// reference: without both, a moving collection inside the callee would
    /// hand the release a stale receiver. That range, the enter's receiver copy
    /// and the release's copy above the result fit in `jit_args + 1` words past
    /// the cursor. A pc a handler of this method protects is refused for the
    /// static arm's reason (see `sync_direct_site`).
    /// The `JitInvokeInfo` registered for the `Arrays.sort` / `Arrays.fill`
    /// intrinsic at `pc` (`jit/src/lib.rs` registers it next to the sentinel,
    /// as for `System.arraycopy`), or `None` -- then the arm keeps its shared
    /// NPE stub. The names are checked so a table that disagrees with the
    /// emitter can never send the null edge to another method.
    pub(in crate::x64::op_invoke) fn arrays_intrinsic_null_dispatch_info(
        &self,
        pc: usize,
        method: &str,
    ) -> Option<*const crate::JitInvokeInfo> {
        let &i = self.invoke_info_idx.get(&pc)?;
        let ptr = self.invoke_info.get(i)?.1;
        if ptr.is_null() {
            return None;
        }
        // SAFETY: `invoke_info` owns every pointer it hands out for the life
        // of this compile.
        let info = unsafe { &*ptr };
        (info.class_name == "java/util/Arrays" && info.method_name == method).then_some(ptr)
    }

    /// The null-array edge of the `Arrays.sort(prim[])` / `Arrays.fill(prim[],
    /// v)` intrinsics (JIT round 20, lane huntIntr, page
    /// `jr20-compiled-arrays-sort-fill-null-npe-loses-the-jdk-message`): instead
    /// of the shared stub's unmessaged NPE at the CALLER's bci, the null edge
    /// calls the real `java.util.Arrays` method through `jit_invoke_dispatch`
    /// -- the `System.arraycopy` arm's decline shape -- so the JDK body fails at
    /// its own `a.length` and the NPE carries HotSpot's `Cannot read the array
    /// length because "a" is null` and the `Arrays` frames. Every argument is
    /// passed as zero: the array IS null on this edge, and the body throws
    /// before it reads any other argument.
    ///
    /// Emitted after the arm's body: `JMP skip; null_patch: <dispatch>; skip:`.
    /// Returns `false` when the args buffer does not fit (the caller fails the
    /// compile, as the arraycopy arm does).
    pub(in crate::x64::op_invoke) fn emit_arrays_intrinsic_null_dispatch(
        &mut self,
        null_patch: usize,
        info: *const crate::JitInvokeInfo,
        num_args: usize,
    ) -> bool {
        let Some(args_base) = self.reserve_spill_slots(num_args, SpillReason::HelperArgs) else {
            return false;
        };
        let skip = self.emit_jmp_rel32_patch();
        self.patch_rel32_to_here(null_patch);
        // XOR EAX, EAX  (31 C0)
        self.buf.emit(&[0x31, 0xC0]);
        for k in 0..num_args {
            // Cast: arg index * 8, a frame displacement of a few words.
            self.emit_store_local(args_base + (k as i32) * 8, RAX);
        }
        self.emit_load_vm_context(ARG_REGS[0]);
        // Cast: the info pointer, as the helper's immediate argument.
        self.emit_mov_imm64(ARG_REGS[1], info as i64);
        // arg[0] lives at the HIGHEST offset of the buffer, as at every
        // dispatch site (all words are zero here, so only the start matters).
        // Cast: arg count, a handful of words.
        let buf_start = args_base + ((num_args as i32) - 1) * 8;
        self.emit_lea_frame_slot(ARG_REGS[2], buf_start);
        // Cast: arg count, 1 or 2.
        self.emit_mov_imm32_sx(ARG_REGS[3], num_args as i32);
        self.emit_pre_safepoint_spill_before_helper_call();
        cratonvm_jit_api::assert_helper_call_shape!(
            "invoke_dispatch",
            int_args = 4,
            returns_value = true
        );
        self.emit_call_absolute(self.helpers.invoke_dispatch);
        self.emit_oop_map_for_safepoint();
        self.emit_post_invoke_exception_check(b'V');
        self.next_spill_offset = args_base;
        self.patch_rel32_to_here(skip);
        true
    }

    /// t1 wave 3, lane X: does `pc` carry a `crate::JIT_SELF_CALL_GUARD`
    /// marker row (a static self-call a door marked, or the instance twin)?
    pub(in crate::x64) fn has_self_call_marker(&self, pc: usize) -> bool {
        self.direct_calls_idx.get(&pc).is_some_and(|&i| {
            self.direct_calls
                .get(i)
                .is_some_and(|(_, dc)| dc.guard_class_id == crate::JIT_SELF_CALL_GUARD)
        })
    }

    /// t1 wave 3, lane X: does `pc` carry a `direct_calls` row that ROUTES the
    /// site (anything but a self-call marker)? The raw-self-call recognisers
    /// (`op_object.rs` allocation sinking) ask this where they used to ask
    /// "any row", so a marked static self-call reads as the unmarked one did.
    pub(in crate::x64) fn has_routing_direct_row(&self, pc: usize) -> bool {
        self.direct_calls_idx.contains_key(&pc) && !self.has_self_call_marker(pc)
    }

    pub(in crate::x64::op_invoke) fn sync_direct_instance_site(&self, pc: usize) -> Option<(usize, u32)> {
        let &i = self.direct_calls_idx.get(&pc)?;
        let dc = &self.direct_calls.get(i)?.1;
        let exact_class_id = crate::sync_direct_instance_guard_class(dc.guard_class_id)?;
        let n = dc.num_params.checked_add(1)?;
        let recv_idx = self.stack.len().checked_sub(n)?;
        let admitted = dc.entry != 0
            && !crate::JitIntrinsic::is_sentinel_entry(dc.entry)
            && self.needs_heap
            && self.direct_helpers.monitor_enter != 0
            && self.direct_helpers.monitor_exit != 0
            && self.invoke_info_idx.contains_key(&pc)
            && !self.inline_sites.contains_key(&pc)
            && self.stack_oop_marks.len() == self.stack.len()
            && self.stack_oop_marks.get(recv_idx).copied().unwrap_or(false)
            && direct_call_arg_maps_enabled()
            && !self.pc_is_protected(pc)
            && self.spill_range_fits(self.next_spill_offset, n + 1);
        admitted.then_some((n, exact_class_id))
    }

    /// Push the monitor a caller-held synchronized direct CALL at `pc` enters,
    /// for `emit_monitor_on_stack_top`: a copy of the receiver `receiver_depth`
    /// entries down (the instance twin, round 13 wave 1), else the class mirror
    /// (`emit_ldc_class`). Taken while the arguments are still on the simulated
    /// stack, so a contended enter's safepoint maps every one of them.
    pub(in crate::x64::op_invoke) fn push_sync_direct_enter_monitor(&mut self, pc: usize, receiver_depth: Option<usize>) -> bool {
        let Some(depth) = receiver_depth else {
            return self.emit_ldc_class(pc);
        };
        let Some(idx) = self.stack.len().checked_sub(depth) else {
            return false;
        };
        let receiver = self.stack[idx];
        self.load_slot_to_reg(RAX, receiver);
        self.push_from_rax();
        self.mark_top_as_oop();
        true
    }

    /// The cold side's `dispatch_threw` peek of a caller-held synchronized
    /// direct CALL with a wide result: entered with `RAX == i64::MIN`, it
    /// asks whether a throwable is pending and leaves the sentinel bits back in
    /// RAX; the returned `JE` patch is taken when none is (the bits are the
    /// genuine `Long.MIN_VALUE` / `-0.0` result, which keeps the normal edge).
    pub(in crate::x64::op_invoke) fn emit_sync_direct_dispatch_threw_peek(&mut self) -> usize {
        // Cast: helper address
        self.emit_mov_imm64(RAX, self.helpers.dispatch_threw as i64);
        self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
        self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
        self.emit_mov_imm64(RAX, i64::MIN); // MOV keeps ZF
        self.emit_jcc_rel32_patch(0x84) // JE .keep
    }

    /// Release the monitor of the caller-held synchronized direct CALL at `pc`,
    /// on either edge: the receiver read back from `receiver_home` (the
    /// instance twin's service-range word, round 13 wave 1), else the class
    /// mirror. The instance arm flushes first, so a floating-point result takes
    /// its canonical word before the receiver copy is pushed above it, and
    /// reads the home before it pushes, so the push may reuse any word.
    pub(in crate::x64::op_invoke) fn emit_sync_direct_release(&mut self, pc: usize, receiver_home: Option<i32>) -> bool {
        let staged = match receiver_home {
            Some(off) => {
                self.flush_scratch_registers();
                self.emit_load_local(RAX, off);
                self.push_from_rax();
                self.mark_top_as_oop();
                true
            }
            None => self.emit_ldc_class(pc),
        };
        staged && self.emit_monitor_on_stack_top(false, pc)
    }

}
