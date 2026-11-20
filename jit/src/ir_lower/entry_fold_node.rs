#[allow(unused_imports)]
use super::*;

impl<'a> Lowerer<'a> {
    /// One data node of the evaluated path: `Ok(Some(v))` for an evaluable
    /// `int` node whose operands are known, `Ok(None)` for any other pure
    /// node (its value stays unknown; reading it later gives up), `Err` for a
    /// node that is not pure.
    pub(in crate::ir_lower) fn entry_fold_node(
        &self,
        node: &Node,
        param_idx: u16,
        arg: i32,
        env: &HashMap<NodeId, i32>,
    ) -> Result<Option<i32>, String> {
        match node.op {
            Op::Add
            | Op::Sub
            | Op::Mul
            | Op::And
            | Op::Or
            | Op::Xor
            | Op::Shl
            | Op::Shr
            | Op::UShr
            | Op::Neg
            | Op::Cmp(_) => {
                if node.ty != IrType::Int {
                    return Ok(None);
                }
                let unary = node.op == Op::Neg;
                let arity = if unary { 1 } else { 2 };
                if node.inputs.len() != arity {
                    return Ok(None);
                }
                // `entry_fold_operand` knows only `int` values, so a `long`,
                // floating or reference operand leaves the node unknown.
                let known = |i: usize| {
                    node.inputs
                        .get(i)
                        .and_then(|&id| self.entry_fold_operand(id, param_idx, arg, env))
                };
                let Some(a) = known(0) else {
                    return Ok(None);
                };
                let b = if unary {
                    None
                } else {
                    match known(1) {
                        Some(b) => Some(b),
                        None => return Ok(None),
                    }
                };
                Ok(entry_fold_int_op(&node.op, a, b))
            }
            Op::Const(_) | Op::Param(_) => Ok(None),
            ref op if entry_fast_return_may_skip(op) => Ok(None),
            ref op => Err(format!("{op:?} is not pure")),
        }
    }

    /// The phi assignment of the edge `pred -> succ`, in parallel: every
    /// source is read before any phi is written. An `int` phi whose source is
    /// known gets its value; any other phi stays unknown. An edge that feeds a
    /// phi through more than one merge input gives up.
    pub(in crate::ir_lower) fn entry_fold_phi_edge(
        &self,
        pred: usize,
        succ: usize,
        param_idx: u16,
        arg: i32,
        env: &mut HashMap<NodeId, i32>,
    ) -> Result<(), String> {
        let merge_ctrl = self
            .schedule
            .blocks
            .get(succ)
            .ok_or_else(|| format!("block {succ} does not exist"))?
            .ctrl;
        let Some(merge) = self.graph.nodes.get(merge_ctrl as usize) else {
            return Err(format!("n{merge_ctrl} does not exist"));
        };
        if !matches!(merge.op, Op::Merge | Op::Region) {
            return Ok(());
        }
        let Some(phis) = self.phis_by_merge.get(&merge_ctrl) else {
            return Ok(());
        };
        if phis.is_empty() {
            return Ok(());
        }
        let from_pred: Vec<usize> = merge
            .inputs
            .iter()
            .enumerate()
            .filter(|&(_, &ctrl_in)| self.block_of_ctrl(ctrl_in) == Some(pred))
            .map(|(k, _)| k)
            .collect();
        let [k] = from_pred.as_slice() else {
            return Err(format!(
                "the edge {pred} -> {succ} feeds {} merge inputs",
                from_pred.len()
            ));
        };
        let mut incoming: Vec<(NodeId, i32)> = Vec::new();
        for &phi in phis {
            let Some(node) = self.graph.nodes.get(phi as usize) else {
                continue;
            };
            if node.ty != IrType::Int {
                continue;
            }
            let Some(&src) = node.inputs.get(k + 1) else {
                continue;
            };
            if src == NO_NODE {
                continue;
            }
            if let Some(v) = self.entry_fold_operand(src, param_idx, arg, env) {
                incoming.push((phi, v));
            }
        }
        for (phi, v) in incoming {
            env.insert(phi, v);
        }
        Ok(())
    }

    /// The outlined half of [`Self::emit_entry_fold`], after the body where
    /// the entry's `JLE` lands: `MOV RAX, r(hi)`, then for every other folded
    /// argument `v` a `MOV R11, r(v); CMP p32, v; CMOVE RAX, R11`, then `RET`.
    /// Every immediate is a sign-extended `imm32`, as `Op::Const` materialises
    /// an `int`.
    pub(in crate::ir_lower) fn emit_entry_fold_stub(&mut self) {
        let Some((patch, fold)) = self.entry_fold.take() else {
            return;
        };
        let Some((&last, rest)) = fold.values.split_last() else {
            return;
        };
        // Every compare the stub makes, computed before anything is emitted:
        // leaving the `JLE` unpatched sends it into the prologue, the full path.
        let mut selects: Vec<(i32, i32)> = Vec::new();
        for (i, &r) in rest.iter().enumerate() {
            let Some(v) = i32::try_from(i).ok().and_then(|i| fold.lo.checked_add(i)) else {
                return;
            };
            if r != last {
                selects.push((v, r));
            }
        }
        // Cast: code offsets of one method, far below `i32::MAX`.
        let rel = self.buf.pos() as i32 - (patch as i32 + 4);
        if !Self::patch_or_bail(&mut self.buf, patch, rel) {
            return;
        }
        self.buf.emit(&[0x48, 0xC7, 0xC0]); // MOV RAX, simm32
        self.buf.emit(&last.to_le_bytes());
        for (v, r) in selects {
            self.buf.emit(&[0x49, 0xC7, 0xC3]); // MOV R11, simm32
            self.buf.emit(&r.to_le_bytes());
            self.emit_cmp_reg_imm(fold.cmp_reg, v, false);
            self.buf.emit(&[0x49, 0x0F, 0x44, 0xC3]); // CMOVE RAX, R11
        }
        self.buf.emit_byte(0xC3); // RET
    }

    /// The outlined half of [`Self::emit_entry_fast_return`]: `MOV RAX, v;
    /// RET`, after the body, where the entry's `Jcc` lands.
    pub(in crate::ir_lower) fn emit_entry_fast_return_stub(&mut self) {
        self.emit_entry_fold_stub();
        let Some((patch, value)) = self.entry_fast_return.take() else {
            return;
        };
        let rel = self.buf.pos() as i32 - (patch as i32 + 4);
        if !Self::patch_or_bail(&mut self.buf, patch, rel) {
            return;
        }
        match value {
            EntryFastValue::Reg(reg) => self.emit_mov_reg_reg64_raw(RAX, reg),
            // Cast: the constant's own 64-bit pattern, as `Op::Const` holds it.
            EntryFastValue::Imm(k) => self.emit_mov_reg_imm64(RAX, k as u64),
        }
        self.buf.emit_byte(0xC3); // RET
    }

    /// fib44-fix follow-up: emit a self-recursive call as a DIRECT `CALL` to this
    /// method's own entry (code offset 0), instead of the generic
    /// `jit_invoke_dispatch` helper. Only reached for an `Op::Call` whose
    /// `JitInvokeInfo.invoke_kind == 4` (the eligibility loop sets that when
    /// `CRATONVM_JIT_IR_SELFREC_DIRECT` is on for a supported self-recursive
    /// static call). `inputs` is the call node's `inputs` (`[ctrl, mem, args…]`), `slot`
    /// its result slot, `num_args` its Java arg count.
    ///
    /// SAFETY of a direct call vs. the C-ABI dispatch helper: the IR method is
    /// itself `extern "C"` (called from Rust via `try_call_with_context`), so it
    /// preserves the platform callee-saved registers — a self-call is just a call
    /// to that same ABI-compliant function.
    pub(in crate::ir_lower) fn emit_self_recursive_call(
        &mut self,
        inputs: &[NodeId],
        slot: i32,
        num_args: usize,
        info_ptr: usize,
        map: &mut Option<SafepointMapToken>,
    ) {
        // Preserve Java's catchable StackOverflowError semantics without a
        // helper call in every recursive frame. Sample once whenever the next
        // frame can cross a 64 KiB native-stack boundary. The runtime guard's
        // floor reserves 1 MiB, so even a check delayed by one full stride still
        // leaves at least 960 KiB for exception construction and unwinding.
        //
        //   low = RSP & 0xffff
        //   if low > frame_size + call/prologue bytes: skip helper
        //
        // Round 11 wave 12 (lane fib): `MOVZX EAX, SP` is `low` in one
        // instruction; it was `MOV RAX, RSP; AND EAX, 0xFFFF` (two, 8 bytes).
        // Same value in RAX (zero-extended to 64 bits), same flags consumer
        // (the `CMP` below sets them afresh), once per recursive call.
        // Round 11 wave 15 (lane fib): the id the `Op::Call` arm's map recorded
        // but did not store (a reference-free body; `deferred_self_call_sp_id`).
        // Stored below on each path on which this frame is the innermost
        // compiled frame while the call is in progress -- before the guard
        // helper, and at the head of the sentinel path -- and nowhere on the
        // hot path. `None`: the map already stored it, or none was recorded.
        let deferred_sp_id = self.deferred_self_call_sp_id.take();
        // Round 12 wave 8 (lane callcost): a site another self-call's sample
        // dominates emits none (`plan_self_call_sample_elision`).
        let skip_sample = std::mem::take(&mut self.self_call_skip_sample);
        self.self_call_sites_lowered += 1;
        if skip_sample {
            self.self_call_samples_elided += 1;
        } else {
            let ctor_receiver = (info_ptr != 0
                // SAFETY: the resolver's invoke metadata stays live throughout compilation.
                && unsafe { (*(info_ptr as *const JitInvokeInfo)).method_name == "<init>" })
            .then(|| inputs[2]);
            self.emit_self_call_stack_sample(deferred_sp_id, ctor_receiver, map.clone());
        }
        // Round 14 wave 6 (lane callcost2, CE-1): an argument the callee's
        // entry would answer without a frame is answered here, and the answer
        // joins the call's `.keep`. After the sample, never before it: a site
        // whose sample dominates others (`plan_self_call_sample_elision`)
        // still samples on its answer path.
        let answer_join = self.emit_self_call_answer(inputs, num_args);

        // Marshal args into this method's OWN entry ABI — IDENTICAL to the
        // register list `emit_prologue` reads incoming args from: abi[0] = the
        // hidden VM context pointer, abi[1 + i] = Java arg i. Each source is a
        // frame slot (memory), so loading straight into the abi registers cannot
        // inter-clobber.
        self.emit_self_call_marshal_and_call(
            inputs,
            slot,
            num_args,
            info_ptr,
            deferred_sp_id,
            answer_join,
            map,
        );
    }

    /// Round 14 wave 6 (lane callcost2, proposal CE-1 of
    /// `jit-r14-callcost-proposals-RETIRED-20260930.md`): answer a direct self-call in the
    /// CALLER when its argument is one the callee's entry answers without a
    /// frame (the entry fast return, [`Self::emit_entry_fast_return`], and the
    /// entry fold, [`Self::emit_entry_fold`]). Emitted after the site's stack
    /// sample and before the marshal; returns the answer path's `JMP rel32`
    /// operand, which the call's `.keep` patches (`None`: nothing emitted).
    ///
    /// ```text
    ///   CMP  c32, hi ; JG .call              ; with a fold (hi = its last argument)
    ///   MOV  RAX, r(hi)
    ///   MOV  R11, r(v) ; CMP c32, v ; CMOVE RAX, R11     ; each other folded v
    ///   MOV  R11, <fast value> ; CMP c32, lo ; CMOVL RAX, R11
    ///   JMP  .keep
    /// .call:                                 ; the marshal, CALL, sentinel as before
    ///   ...
    /// .keep:
    ///   <store the result>
    /// ```
    ///
    /// Without a fold: `CMP c32, imm; J<not ret_cc> .call; MOV RAX, <fast
    /// value>; JMP .keep`. `c` is the compared argument's register, or R10
    /// loaded from its home.
    ///
    /// Why this is the call's own result: the callee is this body (a direct
    /// self-call CALLs offset 0), and for these arguments its first
    /// instructions are the same signed 32-bit compares of the same register
    /// the marshal would load (`gp_load_value`'s source, read here by the same
    /// rule), followed by the same `MOV`s and `CMOV`s into RAX and a `RET`
    /// (`emit_entry_fast_return_stub`, `emit_entry_fold_stub`). Nothing else
    /// that path does is observable: it has no safepoint, helper, trap or
    /// store, so no stack walk, collection or deopt can see the missing frame,
    /// and it leaves the caller's callee-saved file, RSP and the frame record
    /// (which already names this frame) as they were.
    ///
    /// Why the join is sound: at `.keep` the call path has clobbered every
    /// caller-saved register, so the lowering state there claims none of
    /// them; the answer path writes only RAX, R10, R11 and the flags. The
    /// lowering-state updates the call path makes before `.keep`
    /// (`invalidate_ref_residency`, `rcx_twin`, `note_rcx_written`, the
    /// monitor fact) only forget or widen, so they hold on the answer path
    /// too. Before the `JG`, R10/R11 are read by nothing the call path emits
    /// (its marshal reads constants, the callee-saved file and homes).
    ///
    /// Admission ([`Self::plan_self_call_answer_site`]): a reference-free
    /// body (no shadow publication spans the call), no carried argument, the
    /// compared argument in a register or a home word, and the fast return's
    /// value an argument or a constant. A not-entrant patch at offset 0 is
    /// not consulted by the answer path, exactly as it is not by the spliced
    /// copies of the callee this body already contains
    /// (`IR_RECURSIVE_INLINE_MAX_COPIES`): the answer is the same bytecode's.
    ///
    /// Kill switch: `CRATONVM_JIT_IR_SELF_CALL_ANSWER=0`, read per compile.
    /// Census: `CRATONVM_DBG_IR_SELF_CALL_ANSWER=1`, one
    /// `[ir-self-call-answer]` line per direct self-call site.
    pub(in crate::ir_lower) fn emit_self_call_answer(&mut self, inputs: &[NodeId], num_args: usize) -> Option<usize> {
        let census = false;
        let (cmp, value) = match self.plan_self_call_answer_site(inputs, num_args) {
            Ok(site) => site,
            Err(why) => {
                if census {
                    eprintln!(
                        "[ir-self-call-answer] bci {}: declined: {why}",
                        self.cur_bci
                    );
                }
                return None;
            }
        };
        let plan = self.self_call_answer.clone()?;
        // Every compare the answer makes, computed before anything is emitted.
        // `(lo, hi, r(hi), [(v, r(v)) for the other v whose r(v) != r(hi)])`.
        let fold = match &plan.fold {
            Some((lo, values)) => {
                let (&last, rest) = values.split_last()?;
                let hi = i32::try_from(values.len())
                    .ok()
                    .and_then(|len| lo.checked_add(len - 1))?;
                let mut selects: Vec<(i32, i32)> = Vec::new();
                for (i, &r) in rest.iter().enumerate() {
                    let v = i32::try_from(i).ok().and_then(|i| lo.checked_add(i))?;
                    if r != last {
                        selects.push((v, r));
                    }
                }
                Some((*lo, hi, last, selects))
            }
            None => None,
        };
        let c = match cmp {
            SelfCallArgSource::Reg(reg) => reg,
            SelfCallArgSource::Home(off) => {
                self.load_reg_from_frame(R10, off);
                R10
            }
            // Refused by the admission.
            SelfCallArgSource::Imm(_) => return None,
        };
        let value = if value == cmp {
            SelfCallArgSource::Reg(c)
        } else {
            value
        };
        let to_call = match &fold {
            Some((_, hi, _, _)) => {
                self.emit_cmp_reg_imm(c, *hi, false);
                self.emit_jcc_rel32(CmpOp::Gt.x64_cc()) // JG .call
            }
            None => {
                self.emit_cmp_reg_imm(c, plan.imm, false);
                self.emit_jcc_rel32(plan.ret_cc.negate().x64_cc()) // J<not ret_cc> .call
            }
        };
        match fold {
            Some((lo, hi, last, selects)) => {
                self.buf.emit(&[0x48, 0xC7, 0xC0]); // MOV RAX, simm32
                self.buf.emit(&last.to_le_bytes());
                for (v, r) in selects {
                    self.buf.emit(&[0x49, 0xC7, 0xC3]); // MOV R11, simm32
                    self.buf.emit(&r.to_le_bytes());
                    self.emit_cmp_reg_imm(c, v, false);
                    self.buf.emit(&[0x49, 0x0F, 0x44, 0xC3]); // CMOVE RAX, R11
                }
                // Every argument below `lo` is the fast return's (its `<=`/`<`
                // is what `plan_entry_fold` admits).
                self.emit_load_reg_from_arg_source(R11, value);
                self.emit_cmp_reg_imm(c, lo, false);
                self.buf.emit(&[0x49, 0x0F, 0x4C, 0xC3]); // CMOVL RAX, R11
                if census {
                    eprintln!(
                        "[ir-self-call-answer] bci {}: answered n <= {hi} (fast return below {lo}, fold to {hi})",
                        self.cur_bci
                    );
                }
            }
            None => {
                self.emit_load_reg_from_arg_source(RAX, value);
                if census {
                    eprintln!(
                        "[ir-self-call-answer] bci {}: answered n {:?} {} (fast return only)",
                        self.cur_bci, plan.ret_cc, plan.imm
                    );
                }
            }
        }
        let join = self.emit_jmp_rel32(); // JMP .keep
        self.patch_rel32_to_here(to_call);
        Some(join)
    }

    /// The admission of [`Self::emit_self_call_answer`] at the current direct
    /// self-call: the source of the compared argument (a register or a home
    /// word) and of the fast return's value, each read by `gp_load_value`'s
    /// rule, or why the site keeps its plain `CALL`.
    pub(in crate::ir_lower) fn plan_self_call_answer_site(
        &self,
        inputs: &[NodeId],
        num_args: usize,
    ) -> Result<(SelfCallArgSource, SelfCallArgSource), String> {
        let Some(plan) = self.self_call_answer.as_ref() else {
            return Err("no entry fast return (or CRATONVM_JIT_IR_SELF_CALL_ANSWER=0)".to_string());
        };
        if self.mir_mode != MirMode::Off {
            return Err("a MIR mode".to_string());
        }
        if !self.needs_context {
            return Err("the body takes no context".to_string());
        }
        // No shadow publication spans the call, so its reload after `.keep`
        // has nothing to restore on either path.
        if !self.body_publishes_no_roots() {
            return Err("the body publishes roots".to_string());
        }
        if num_args != self.num_params {
            return Err(format!(
                "{num_args} arguments for {} parameters",
                self.num_params
            ));
        }
        let args = inputs
            .get(2..2 + num_args)
            .ok_or("the call has fewer inputs than arguments")?;
        // A carried argument is readable only by the marshal's own
        // `gp_load_value`, and an RAX carry must meet it with nothing emitted
        // in between.
        if args.iter().any(|&a| self.carry_names(a)) {
            return Err("an argument is carried".to_string());
        }
        let cmp_idx = match self.graph.nodes.get(plan.param as usize).map(|n| &n.op) {
            Some(Op::Param(idx)) => usize::from(*idx),
            _ => return Err("the compared value is not a parameter".to_string()),
        };
        let cmp_arg = *args
            .get(cmp_idx)
            .ok_or("the compared parameter has no argument")?;
        let cmp = self.self_call_answer_source(cmp_arg)?;
        if matches!(cmp, SelfCallArgSource::Imm(_)) {
            return Err("the compared argument is a constant".to_string());
        }
        let value = match plan.value {
            EntryFastValue::Imm(k) => SelfCallArgSource::Imm(k),
            EntryFastValue::Reg(reg) => {
                // `entry_fast_int_param_reg`'s mapping, inverted.
                let base = usize::from(self.needs_context);
                let j = ENTRY_ABI_REGS
                    .iter()
                    .position(|&r| r == reg)
                    .and_then(|p| p.checked_sub(base))
                    .ok_or("the fast return's register is not a parameter's")?;
                if j == cmp_idx {
                    cmp
                } else {
                    let arg = *args
                        .get(j)
                        .ok_or("the returned parameter has no argument")?;
                    self.self_call_answer_source(arg)?
                }
            }
        };
        Ok((cmp, value))
    }

    /// Where `gp_load_value` would read the self-call argument `arg` from
    /// (past its carry slots, which the admission refuses): a constant's
    /// immediate, its resident register, or its home word.
    pub(in crate::ir_lower) fn self_call_answer_source(&self, arg: NodeId) -> Result<SelfCallArgSource, String> {
        if self.const_imm_on {
            if let Some(Op::Const(val)) = self.graph.nodes.get(arg as usize).map(|n| &n.op) {
                return Ok(SelfCallArgSource::Imm(*val));
            }
        }
        if let Some(reg) = self.resident_gpr(arg) {
            // The answer path writes these; the GP file never holds them.
            if matches!(reg, RAX | R10 | R11) {
                return Err(format!("n{arg} is resident in a scratch register"));
            }
            return Ok(SelfCallArgSource::Reg(reg));
        }
        if self
            .home_dropped
            .get(arg as usize)
            .copied()
            .unwrap_or(false)
        {
            return Err(format!("n{arg}'s home is dropped"));
        }
        self.home_offset_of(arg)
            .map(SelfCallArgSource::Home)
            .ok_or_else(|| format!("n{arg} has no home"))
    }

    /// `dst` from a captured argument source (round 14 wave 6, lane callcost2):
    /// the instruction `gp_load_value(dst, arg)` would emit for it.
    pub(in crate::ir_lower) fn emit_load_reg_from_arg_source(&mut self, dst: u8, source: SelfCallArgSource) {
        match source {
            SelfCallArgSource::Imm(val) => self.emit_mov_reg_imm_smart(dst, val),
            SelfCallArgSource::Reg(reg) => self.emit_mov_reg_reg64_raw(dst, reg),
            SelfCallArgSource::Home(off) => self.load_reg_from_frame(dst, off),
        }
    }

    /// The sampled stack guard of [`Self::emit_self_recursive_call`]: `MOVZX
    /// EAX, SP; CMP EAX, span; JA .ok`, and on the slow side the deferred id,
    /// the guard helper and its bail. Split out unchanged (round 12 wave 8,
    /// lane callcost) so a dominated site can omit it whole.
    pub(in crate::ir_lower) fn emit_self_call_stack_sample(
        &mut self,
        deferred_sp_id: Option<u32>,
        ctor_receiver: Option<NodeId>,
        mut map: Option<SafepointMapToken>,
    ) {
        self.buf.emit(&[0x0F, 0xB7, 0xC4]); // MOVZX EAX, SP
        self.buf.emit_byte(0x3D); // CMP EAX, imm32
        let next_frame_span = (self.frame_size as u32).saturating_add(16).min(0xffff);
        self.buf.emit(&next_frame_span.to_le_bytes());
        // Round 13 wave 8 (lane callcost3, CC2-1): the slow side after the
        // body, entered by the inverted branch, so the hot path falls through.
        if self.self_call_stack_guard != 0
            && ctor_receiver.is_none()
            && self.self_call_cold_tail_admitted()
        {
            self.buf.emit(&[0x0F, 0x86]); // JBE .guard (deferred)
            let entry_patch = self.buf.pos();
            self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
            let bci = self.note_deferred_self_call_exit();
            let copy_back = self.pending_shadow_copy_back();
            self.invalidate_ref_residency(None);
            let resume = self.buf.pos();
            self.deferred_self_call_tails
                .push(DeferredSelfCallTail::StackGuard {
                    entry_patch,
                    resume,
                    sp_id: deferred_sp_id,
                    map,
                    copy_back,
                    bci,
                });
            return;
        }
        self.buf.emit(&[0x0F, 0x87]); // JA .guard_ok
        let fast_skip_patch = self.buf.pos();
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
        if self.self_call_stack_guard != 0 {
            // The guard helper is not a compiled frame and can throw
            // `StackOverflowError`, whose trace reads this frame's slot.
            self.emit_deferred_safepoint_id_store(deferred_sp_id, &mut map);
            self.load_context(CALL_ARG_REGS[0]);
            if let Some(receiver) = ctor_receiver {
                self.gp_load_value(CALL_ARG_REGS[1], receiver);
            } else {
                self.emit_mov_reg_imm_smart(CALL_ARG_REGS[1], 0);
            }
            self.emit_mov_reg_imm64(RAX, self.self_call_stack_guard as u64);
            self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
            self.anchor_safepoint_return(map.as_mut(), self.buf.pos());
            self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
            self.buf.emit(&[0x0F, 0x85]); // JNE shared bail stub
            let patch = self.buf.pos();
            self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
            self.push_self_call_exc_patch(patch);
            // Round 11 wave 4 (lane lower,
            // r11-irlower-self-recursive-call-withdraws-moving-coverage): the
            // site's map and shadow push were emitted BEFORE this helper call,
            // and the argument marshal below reads the HOMES. Refresh them from
            // the published words (without retracting `top`) so a collection
            // inside the guard could never hand the recursive call pre-move
            // addresses. The guard reaches no safepoint on this path today;
            // this makes the route independent of that, at a cost paid only
            // once per 64 KiB of stack. The tripping path leaves through the
            // call-exception stub, which reads no home.
            self.emit_pending_shadow_copy_back();
        }
        let rel = self.buf.pos() as i32 - (fast_skip_patch as i32 + 4);
        // ir_lower self-call stack-sample -- tolerated on an overflowed buffer; see
        // `Self::patch_or_bail` / `patch_rel32_to_here`.
        Self::patch_or_bail(&mut self.buf, fast_skip_patch, rel);
    }

    /// The rest of [`Self::emit_self_recursive_call`] after its stack sample:
    /// the marshal, the `CALL`, and the post-call protocol. Split out unchanged
    /// (round 12 wave 8, lane callcost).
    pub(in crate::ir_lower) fn emit_self_call_marshal_and_call(
        &mut self,
        inputs: &[NodeId],
        slot: i32,
        num_args: usize,
        info_ptr: usize,
        deferred_sp_id: Option<u32>,
        answer_join: Option<usize>,
        map: &mut Option<SafepointMapToken>,
    ) {
        // `1 + num_args <= abi.len()` is required and is enforced at ELIGIBILITY
        // (`jit::lib`'s `is_self_recursive_direct`, via
        // `incoming_abi_reg_capacity`), not here. It used to be implied by
        // `lower()`'s whole-method bail on a graph with more incoming slots than
        // registers; when `emit_prologue` learned to read the overflow off the
        // caller's stack (Gap B) that bail went away and this comment kept
        // asserting a guarantee nobody was making any more.
        //
        // MEASURED, 2026-08-29, Windows: a `static long f(int,int,int,int)`
        // calling itself is 4 Java args plus the context = 5 against a
        // four-register file, so `abi[4]` panicked the compiler thread with
        // `index out of bounds: the len is 4 but the index is 4` — and the
        // thread does not come back, so the FIRST such method silently disables
        // the JIT for the rest of the process. SysV has six registers and needs
        // six arguments to reach it, which is why it showed up on Windows first.
        // `probes/SelfRecArgs.java` is the arity sweep.
        #[cfg(target_os = "windows")]
        let abi: &[u8] = &[1, 2, 8, 9]; // RCX, RDX, R8, R9
        #[cfg(not(target_os = "windows"))]
        let abi: &[u8] = &[7, 6, 2, 1, 8, 9]; // RDI, RSI, RDX, RCX, R8, R9
        self.load_context(abi[0]); // vm_ptr
        for i in 0..num_args {
            let arg = inputs[2 + i];
            self.gp_load_value(abi[1 + i], arg); // Java arg i
        }
        // Direct CALL rel32 to entry (code offset 0), patched at finalize.
        self.buf.emit(&[0xE8]);
        let patch = self.buf.pos();
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
        self.self_call_patches.push(patch);
        // This route bypasses `emit_call_return_check`, so it records its own
        // inline-frame row. Same obligation, same reason as the republish
        // below: what that helper does for the shared routes has to happen
        // here too, or a frame suspended in this call reports no inlined
        // callees while the identical call on another route reports them.
        self.anchor_recursive_caller_return(map.as_mut(), self.buf.pos());
        self.note_inline_frame_return_site();
        // The recursive callee published ITS frame's RBP into the mirror on
        // entry. This route bypasses `emit_call_return_check`, so it has to
        // republish here or the mirror keeps naming the returned frame — which
        // for a deeply recursive method is every frame but the right one.
        //
        // Round 9 wave 3 (invoke2 X1): the RBP half only. The identity half
        // would re-store the id the callee -- this same buffer's entry --
        // already published and restored on every normal return.
        let identity_elided = self.emit_post_self_call_frame_record();
        // Exception/deopt sentinel — identical to the dispatch path's return
        // check. Integer/reference results cannot equal the full-width sentinel;
        // wide returns can legitimately carry those bits (for example
        // Long.MIN_VALUE), so on `RAX == i64::MIN` peek the out-of-band signal
        // via `dispatch_threw` and bail only when one is pending.
        self.emit_cmp_rax_sentinel();
        // Round 13 wave 8 (lane callcost3, CC2-1): the whole sentinel path
        // below, emitted after the body; the hot path falls through to `.keep`.
        if let Some(args) = self.deferred_self_call_service_args(inputs, num_args) {
            self.buf.emit(&[0x0F, 0x80]); // JO .sentinel (deferred)
            let entry_patch = self.buf.pos();
            self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
            let bci = self.note_deferred_self_call_exit();
            let copy_back = self.pending_shadow_copy_back();
            let resume = self.buf.pos();
            self.deferred_self_call_tails
                .push(DeferredSelfCallTail::Sentinel {
                    entry_patch,
                    resume,
                    sp_id: deferred_sp_id,
                    map: map.clone(),
                    copy_back,
                    identity_elided,
                    info_ptr,
                    args,
                    bci,
                    lean: self.self_call_sentinel_lean_site(),
                });
            // .keep: (and the caller-side answer's join, CE-1)
            if let Some(join) = answer_join {
                self.patch_rel32_to_here(join);
            }
            self.store_rax(slot);
            return;
        }
        self.buf.emit(&[0x0F, 0x81]); // JNO .keep
        let keep_patch = self.buf.pos();
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
        let mut service_map = map.clone();
        // The sentinel path: every call below it (the callee-deopt service,
        // `dispatch_threw`, the exception stubs) leaves compiled code with
        // this frame innermost, so its slot must name this call. A plain
        // store to the frame: no register, no flag is touched.
        self.emit_deferred_safepoint_id_store(deferred_sp_id, &mut service_map);
        if identity_elided {
            // The sentinel path is the one return a deopt or a throw inside the
            // callee can take without passing its own republish, so it gets
            // the identity half back. Off the hot path; touches no register.
            // This MUST run before anything below that reads the mirror
            // (`emit_inline_callee_deopt_service`'s `jit_service_callee_deopt`
            // resolves its stash against whatever `(rbp, compile_id)` the
            // mirror names right now) — a stale compile_id here would look up
            // the wrong `CompiledMethod`'s deopt point.
            self.emit_frame_record_identity();
        }
        // r10 irlow: this raw `CALL` is exactly the shape
        // `emit_inline_callee_deopt_service` exists for — a direct jump to a
        // known compiled entry, bypassing `jit_invoke_dispatch` — and the
        // recursive callee (this same method's entry) can return the
        // sentinel for either an implicit exception/guard failure inside
        // ITSELF or a rethrow from one of ITS OWN nested calls, exactly as
        // any other direct or inline-cached callee can. Every other raw-CALL
        // route (`emit_direct_cross_call`, `emit_cycle_edge_cell_call`,
        // `emit_inline_cache_call`) threads `info_ptr` through to this
        // service so the callee's own exception table gets one last look
        // before the sentinel is treated as unwinding THIS frame too; this
        // route had `info_ptr` sitting unused in its caller (`Op::Call`,
        // `invoke_kind == 4`) and never made the call. Without it, a
        // recursive method whose own body catches what its recursive call
        // throws never got the chance: the catch was skipped and every
        // enclosing recursive activation unwound as if the exception were
        // its own, collapsing an N-deep recursion's stack trace into the
        // outermost frame's throw site. `after_reload = false`: this route
        // never calls `emit_shadow_reload` (unlike the merged direct-call
        // shape), so the service must still do its own post-call frame
        // republish and pending-shadow copy-back on its cold side.
        // JIT round 19 wave 4, lane throws4 (THROWS3-3): the lean shape hands
        // the service its nothing-stashed early out and, below, leaves on the
        // service's sentinel without the `dispatch_threw` peek.
        let lean = self.self_call_sentinel_lean_site();
        self.service_lean_bit_pending = lean;
        self.emit_inline_callee_deopt_service(
            info_ptr, num_args, inputs, /* after_reload */ false,
            &mut service_map,
        );
        self.service_lean_bit_pending = false;
        // The service may have replaced the sentinel with the recursive
        // callee's genuine resumed result (its own catch handled what its own
        // call threw). A fresh, independent check — the `dispatch_threw`
        // sequence below unconditionally overwrites RAX, so without this gate
        // a successfully serviced value would be discarded and replaced by
        // whatever the stale pending-signal flag from some earlier, unrelated
        // sentinel happens to say.
        self.emit_cmp_rax_sentinel();
        self.buf.emit(&[0x0F, 0x81]); // JNO .keep
        let serviced_patch = self.buf.pos();
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
        if lean {
            // A narrow result is never `i64::MIN`: the sentinel is a throw or
            // a deopt, which is what `dispatch_threw` would have answered
            // (OF is still set from the compare, so this `JO` is taken).
            self.buf.emit(&[0x0F, 0x80]); // JO bail_stub
        } else {
            self.emit_mov_reg_imm64(RAX, self.dispatch_threw as u64);
            self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
            self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
            self.emit_mov_reg_imm64(RAX, i64::MIN as u64); // restore (MOV preserves ZF)
            self.buf.emit(&[0x0F, 0x85]); // JNE bail_stub
        }
        let exc_patch = self.buf.pos();
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
        // Round 11 wave 13 (lane fib): recorded as a transfer site only when
        // a reason-9 pad makes it one -- the match for
        // `self_call_reads_no_frame_state` (both exits of this route).
        self.push_self_call_exc_patch(exc_patch);
        // .keep:
        let keep_off = self.buf.pos();
        let rel = keep_off as i32 - (keep_patch as i32 + 4);
        // ir_lower self-call sentinel keep -- tolerated on an overflowed buffer; see
        // `Self::patch_or_bail` / `patch_rel32_to_here`.
        Self::patch_or_bail(&mut self.buf, keep_patch, rel);
        let serviced_rel = keep_off as i32 - (serviced_patch as i32 + 4);
        Self::patch_or_bail(&mut self.buf, serviced_patch, serviced_rel);
        // Round 14 wave 6 (lane callcost2, CE-1): the caller-side answer's join.
        if let Some(join) = answer_join {
            self.patch_rel32_to_here(join);
        }
        // Spill the return value.
        self.store_rax(slot);
    }

    /// May the current direct self-call's cold halves be emitted after the
    /// body instead of inline? Round 13 wave 8 (lane callcost3, proposal
    /// CC2-1; `r12w8-callcost-fib-call-anatomy-FIXED-20261001.md` F8-3 and the
    /// sampled guard's taken `JA`). Kill switch:
    /// `CRATONVM_JIT_IR_SELF_CALL_COLD_TAILS=0`, read per site.
    ///
    /// Wave 3's LC-3 admission: the cold halves read lowering state that is
    /// valid only AT the site, and this is the set of sites where every such
    /// read is a per-compile constant or is captured there:
    ///
    /// * a body that publishes no roots, and no pending shadow publication at
    ///   the site, so the inline `emit_pending_shadow_copy_back` would emit no
    ///   code (its residency bookkeeping is replayed at the site);
    /// * an unprotected bci with `ir_self_call_states_unread_enabled`, so the
    ///   exit is a plain call-exception row ([`Self::push_self_call_exc_patch`]'s
    ///   second branch): no reason-9 pad, whose frame state resolves from the
    ///   node being lowered, and no `transfer_bcis` entry;
    /// * no level-2 machine list.
    pub(in crate::ir_lower) fn self_call_cold_tail_admitted(&self) -> bool {
        self.mir_mode == MirMode::Off
            && !self.direct_call_args_owned
            && ir_self_call_states_unread_enabled()
            && !self.bci_is_protected(self.resume_bci(self.cur_bci))
            && true
    }

    /// JIT round 19 wave 4, lane throws4 (THROWS3-3 of
    /// `docs/feature-designs/jit-ir-optimizations.md`,
    /// `CRATONVM_JIT_SELF_CALL_SENTINEL_LEAN`, opt-in): does the current direct
    /// self-call take the lean sentinel path?
    ///
    /// Every frame a throw unwinds through a direct self-call ran two helper
    /// calls: the callee-deopt service, which for a plain propagating
    /// throwable with nothing stashed takes the whole signal record and puts
    /// it back, and then the `dispatch_threw` peek. Lean: the service is
    /// handed [`crate::SERVICE_ARGS_LEAN_PROPAGATE_BIT`], under which it
    /// answers "propagate" before touching the record when no deopt or
    /// exceptional frame is stashed, no not-entrant escape is recorded, a
    /// throwable is pending and the callee has no exception table; and the
    /// peek is dropped, because a narrow result (void, `int`, reference) is
    /// never the full-width `i64::MIN`, so a sentinel the service hands back
    /// is a throw or a deopt -- the rule the deferred cross-call tail already
    /// applies (`emit_call_return_sentinel_tail`, non-wide). Wide results
    /// keep the peek (a genuine `Long.MIN_VALUE`).
    pub(in crate::ir_lower) fn self_call_sentinel_lean_site(&self) -> bool {
        self.self_call_sentinel_lean_on
            && self.self_call_result_narrow
            && self.service_callee_deopt != 0
    }

    /// The half of [`Self::push_self_call_exc_patch`] that reads the site: its
    /// method bci and the monitor fact, taken while the call is the node being
    /// lowered. [`Self::emit_deferred_self_call_tails`] pushes the row itself
    /// when it emits the exit, before anything reads `call_exc_patches`. Only
    /// for an admitted site, where `push_self_call_exc_patch` takes exactly
    /// this branch.
    pub(in crate::ir_lower) fn note_deferred_self_call_exit(&mut self) -> usize {
        let bci = self.resume_bci(self.cur_bci);
        if !self.propagating_exit_holds_monitor && self.site_snapshot_may_hold_monitor(bci) {
            self.propagating_exit_holds_monitor = true;
        }
        bci
    }

    /// The sentinel half's admission ([`Self::self_call_cold_tail_admitted`])
    /// and the sources its callee-deopt service stages the arguments from:
    /// what the inline service's `gp_load_value(RAX, arg)` reads at this
    /// point, after its copy-back's residency bookkeeping (replayed here).
    /// `None` keeps the inline shape, as does an argument a carry still names
    /// (only `gp_load_value` may read one). No service wired: no staging, an
    /// empty list.
    pub(in crate::ir_lower) fn deferred_self_call_service_args(
        &mut self,
        inputs: &[NodeId],
        num_args: usize,
    ) -> Option<Vec<(SelfCallArgSource, bool)>> {
        if !self.self_call_cold_tail_admitted() {
            return None;
        }
        if self.service_callee_deopt == 0 {
            return Some(Vec::new());
        }
        let args = inputs.get(2..2 + num_args)?;
        if args.iter().any(|&a| self.carry_names(a)) {
            return None;
        }
        self.invalidate_ref_residency(None);
        if num_args > 0 {
            // The inline staging's `store_rax` takes any twin offer; the
            // `.keep` store must not see one either.
            self.rcx_twin = None;
        }
        let mut sources = Vec::with_capacity(num_args);
        for &arg in args {
            let node_opt = self.graph.nodes.get(arg as usize);
            let sign_extend = node_opt
                .is_some_and(|n| n.ty == IrType::Int && !matches!(n.op, Op::Const(_)));
            let konst = match node_opt.map(|n| &n.op) {
                Some(Op::Const(val)) if self.const_imm_on => Some(*val),
                _ => None,
            };
            let source = match (konst, self.resident_gpr(arg)) {
                (Some(val), _) => SelfCallArgSource::Imm(val),
                (None, Some(reg)) => SelfCallArgSource::Reg(reg),
                (None, None) => SelfCallArgSource::Home(self.slot_of(arg)),
            };
            sources.push((source, sign_extend));
        }
        Some(sources)
    }

    /// May the current merged direct cross call's cold side go to a deferred
    /// tail? Round 14 wave 3 (lane calls, CC3-1). Narrower than
    /// [`Self::self_call_cold_tail_admitted`] needs to be, because the merged
    /// route has already republished and reloaded on the hot path (its service
    /// runs with `after_reload`, so the cold side copies nothing back and the
    /// body may publish roots):
    ///
    /// * an unprotected bci, so the exit is the plain call-exception row
    ///   [`Self::push_call_exc_patch`] records (no reason-9 pad, whose frame
    ///   state resolves from the node being lowered);
    /// * no level-2 machine list;
    /// * the switch.
    pub(in crate::ir_lower) fn cross_call_cold_tail_admitted(&self) -> bool {
        self.mir_mode == MirMode::Off
            && !self.bci_is_protected(self.resume_bci(self.cur_bci))
            // nio21 wave 3 L2: the deferred tail pushes its exit row itself,
            // past `push_call_exc_patch`, so an armed call keeps the inline
            // tail, whose exit reaches the call-site exception pad.
            && self.cur_call_throw.is_none()
            && ir_cross_call_cold_tails_enabled()
    }

    /// The admission ([`Self::cross_call_cold_tail_admitted`]) and the sources
    /// the deferred service stages from: what the inline service's
    /// `gp_load_value(RAX, arg)` reads at this point (after the reload). `None`
    /// keeps the inline shape, as does an argument a carry names. No service
    /// wired: an empty list.
    pub(in crate::ir_lower) fn deferred_cross_call_service_args(
        &mut self,
        inputs: &[NodeId],
        num_args: usize,
    ) -> Option<Vec<SelfCallArgSource>> {
        if !self.cross_call_cold_tail_admitted() {
            return None;
        }
        // JIT round 19 wave 5 (lane handler5, SPLICE4-1): an armed call keeps
        // the inline exception tail, whose exit goes through
        // `push_call_exc_patch` and so through the compiled handler probe; the
        // deferred tail pushes its exit row directly.
        if self.cur_catch_arm.is_some() {
            return None;
        }
        // JIT round 21, lane argwin (OOM21-1): an owned site stages nothing;
        // the tail hands the service the owned word instead.
        if self.service_callee_deopt == 0 || self.direct_call_args_owned {
            return Some(Vec::new());
        }
        let args = inputs.get(2..2 + num_args)?;
        if args.iter().any(|&a| self.carry_names(a)) {
            return None;
        }
        if num_args > 0 {
            // The inline staging's `store_rax` takes any twin offer; the
            // `.keep` store must not see one either.
            self.rcx_twin = None;
        }
        let mut sources = Vec::with_capacity(num_args);
        for &arg in args {
            let konst = match self.graph.nodes.get(arg as usize).map(|n| &n.op) {
                Some(Op::Const(val)) if self.const_imm_on => Some(*val),
                _ => None,
            };
            let source = match (konst, self.resident_gpr(arg)) {
                (Some(val), _) => SelfCallArgSource::Imm(val),
                (None, Some(reg)) => SelfCallArgSource::Reg(reg),
                (None, None) => SelfCallArgSource::Home(self.slot_of(arg)),
            };
            sources.push(source);
        }
        Some(sources)
    }

    /// zr s1/ca (`CRATONVM_JIT_ZGC_INLINE_ARM_FAST_JOIN`): may the inline ZGC
    /// start-bit arm of the allocation being lowered (`Op::New` /
    /// `Op::NewArray`) land at the site's `allocated` label, past the
    /// identity store and the zero test only the stub owes? Only for a ZGC
    /// plan (`announce != 0`) and only while no shadow publication is pending:
    /// the stub's `emit_shadow_reload` that the arm jumps over would then emit
    /// nothing, so no push is left unretracted on it. zt t9/ca: one place for
    /// the gate both arms used to spell out, so the lowering-level test pins it.
    pub(in crate::ir_lower) fn zgc_inline_arm_fast_join_admitted(&self, announce: usize) -> bool {
        announce != 0
            && self.pending_shadow.is_empty()
            && crate::x64::zgc_inline_arm_fast_join_enabled()
    }

    /// JIT r23 wave 11, lane L3
    /// (`CRATONVM_JIT_IR_NEWARRAY_FAST_JOIN_ANY_GC`): may the inline
    /// `newarray` / `anewarray` bump of a plan with NO announce (Generational,
    /// G1) land at the site's `allocated` label? The same gate as the ZGC arm:
    /// no shadow publication pending, so the `emit_shadow_reload` the fast path
    /// jumps over emits nothing.
    pub(in crate::ir_lower) fn gen_newarray_fast_join_admitted(&self, announce: usize) -> bool {
        announce == 0
            && self.pending_shadow.is_empty()
            && crate::runtime_lowering::ir_newarray_fast_join_any_gc_enabled()
    }

    /// zs s9/ca4: may the inline ZGC allocation being lowered split, its
    /// cold half going after the body? The switch, no MIR mode, no pending
    /// shadow publication (the fast-join condition: the cold half's reload
    /// would emit nothing), and the cross-call cold tails' exit conditions --
    /// an unprotected bci with no armed call-throw plan or catch arm, so the
    /// exit is the plain call-exception row the tail pushes itself.
    ///
    /// zt t2/ca: also only while `CRATONVM_JIT_ZGC_INLINE_ARM_SKIPS_REPUBLISH`
    /// stands. The split inline arm skips the frame republish (it falls
    /// through past it, as the fast join does), so under that switch's `=0`
    /// kill -- which puts the republish back on the inline arm and with it
    /// drops the fast join -- the site keeps the unsplit bytes instead of
    /// silently ignoring the kill.
    pub(in crate::ir_lower) fn zgc_alloc_tail_admitted(&self) -> bool {
        crate::runtime_lowering::zgc_inline_arm_fall_through_enabled()
            && crate::x64::zgc_inline_arm_skips_republish_enabled()
            && self.mir_mode == MirMode::Off
            && self.pending_shadow.is_empty()
            && !self.bci_is_protected(self.resume_bci(self.cur_bci))
            && self.cur_call_throw.is_none()
            && self.cur_catch_arm.is_none()
    }

    /// Record a split allocation's cold half: its continuation is HERE (the
    /// inline arm just fell through to it).
    pub(in crate::ir_lower) fn defer_zgc_alloc_tail(
        &mut self,
        kind: DeferredZgcAllocKind,
        split: crate::runtime_lowering::ZgcAllocSplit,
        map: Option<SafepointMapToken>,
    ) {
        let exit_bci = self.note_deferred_cross_call_exit();
        self.deferred_zgc_alloc_tails.push(DeferredZgcAllocTail {
            map,
            kind,
            split,
            resume: self.buf.pos(),
            exit_bci,
            node_pc: self.cur_node_pc,
            site_bci: self.cur_bci,
        });
    }

    /// Emit every deferred ZGC allocation cold half (zs s9/ca4), after the
    /// body and before anything reads `call_exc_patches`: the helper arm and
    /// the stub (`runtime_lowering::emit_inline_tlab_newarray_ir_cold_half` /
    /// `emit_inline_tlab_new_ir_cold_half`), the stub's return-site rows
    /// under the site's own pc and bci, then what the unsplit site emitted
    /// after its stub: the identity republish, `TEST RAX, RAX; JNZ resume`,
    /// and the `i64::MIN` exit to the shared exception epilogue.
    pub(in crate::ir_lower) fn emit_deferred_zgc_alloc_tails(&mut self) {
        for mut tail in std::mem::take(&mut self.deferred_zgc_alloc_tails) {
            let return_address = match tail.kind {
                DeferredZgcAllocKind::Array { plan, stub_target } => {
                    crate::runtime_lowering::emit_inline_tlab_newarray_ir_cold_half(
                        &mut self.buf,
                        &plan,
                        stub_target,
                        self.frame_record,
                        tail.split,
                    )
                }
                DeferredZgcAllocKind::New {
                    plan,
                    stub_target,
                    slow_map_bytes,
                    ensure_frame_block,
                } => {
                    let savebase = -self.shadow_savebase_slot_off;
                    let shared = self.frame_block_shared && !self.frame_block.is_empty();
                    let stub_at = self.frame_block_stub_at;
                    let patches = &mut self.frame_block_call_patches;
                    let mut out_of_range = false;
                    let ret = crate::runtime_lowering::emit_inline_tlab_new_ir_cold_half(
                        &mut self.buf,
                        &plan,
                        stub_target,
                        self.frame_record,
                        tail.split,
                        |buf| {
                            if !slow_map_bytes.is_empty() {
                                buf.emit(&slow_map_bytes);
                            }
                            if ensure_frame_block && shared {
                                // CMP qword [rbp - savebase], 0
                                buf.emit(&[0x48, 0x83, 0xBD]);
                                buf.emit(&savebase.to_le_bytes());
                                buf.emit_byte(0);
                                // JNE +5 (over the CALL) ; CALL publish
                                buf.emit(&[0x75, 0x05]);
                                buf.emit_byte(0xE8);
                                let rel_at = buf.pos();
                                match stub_at {
                                    Some(stub) => {
                                        let rel = i32::try_from(stub as i64 - (rel_at as i64 + 4));
                                        if rel.is_err() {
                                            out_of_range = true;
                                        }
                                        buf.emit(&rel.unwrap_or(0).to_le_bytes());
                                    }
                                    None => {
                                        patches.push(rel_at);
                                        buf.emit(&[0, 0, 0, 0]);
                                    }
                                }
                            }
                        },
                    );
                    if out_of_range {
                        self.latch_bailout(Bailout::new(BailoutReason::Internal(
                            "ir_lower: a frame-block publication call is out of rel32 range",
                        )));
                    }
                    ret
                }
            };
            self.anchor_safepoint_return(tail.map.as_mut(), return_address);
            let saved = (self.cur_node_pc, self.cur_bci);
            self.cur_node_pc = tail.node_pc;
            self.cur_bci = tail.site_bci;
            self.note_inline_frame_return_site_at(return_address);
            self.cur_node_pc = saved.0;
            self.cur_bci = saved.1;
            self.emit_identity_after_shared_stub();
            self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
            let ok = self.emit_jcc_rel32(0x85); // JNZ resume
                                                // Cast: code offsets of one method, far below `i32::MAX`.
            let rel = tail.resume as i32 - (ok as i32 + 4);
            if !Self::patch_or_bail(&mut self.buf, ok, rel) {
                return;
            }
            self.emit_mov_reg_imm64(RAX, i64::MIN as u64);
            self.buf.emit_byte(0xE9); // JMP shared exception epilogue
            let exc = self.buf.pos();
            self.buf.emit(&[0; 4]);
            self.call_exc_patches.push((exc, tail.exit_bci));
        }
    }

    /// JIT round 23 wave 9 (lane W9-L7): may this field read's cold half go to
    /// a deferred tail? Only where everything the exception exit asks of the
    /// lowering state is the plain answer, so emitting it past the body
    /// changes nothing: no machine-level emission mode, no call-site exception
    /// pad or compiled catch arm wired for this node (`cur_call_throw`,
    /// `cur_catch_arm`: the tail runs with both cleared), no pending shadow
    /// publication, a bci outside every protected range (whose exits take a
    /// rethrow pad), and a receiver the tail can reload by itself (not a
    /// constant).
    pub(in crate::ir_lower) fn getfield_cold_tail_admitted(&self, base: NodeId) -> bool {
        self.mir_mode == MirMode::Off
            && self.cur_call_throw.is_none()
            && self.cur_catch_arm.is_none()
            && self.pending_shadow.is_empty()
            && !self.bci_is_protected(self.resume_bci(self.cur_bci))
            && !matches!(
                self.graph.nodes.get(base as usize).map(|n| &n.op),
                Some(Op::Const(_)) | None
            )
    }

    /// Emit every deferred field-read cold half
    /// ([`DeferredGetfieldTail`]), after the body and before every reader of
    /// `call_exc_patches`. Each is the inline arm's legacy read and checked
    /// helper, byte for byte, except that the site's branches enter it and a
    /// `JMP done` returns to the site's join.
    pub(in crate::ir_lower) fn gen_alloc_tail_admitted(&self) -> bool {
        self.mir_mode == MirMode::Off
            && self.pending_shadow.is_empty()
            && !self.frame_block.is_empty()
            && self.frame_block_shared
            && !self.bci_is_protected(self.resume_bci(self.cur_bci))
            && self.cur_call_throw.is_none()
            && self.cur_catch_arm.is_none()
    }

    pub(in crate::ir_lower) fn emit_deferred_putfield_tails(&mut self) {
        let jmp_done = |lo: &mut Self, done: usize| -> bool {
            lo.buf.emit_byte(0xE9); // JMP rel32 -> done
            let p = lo.buf.pos();
            lo.buf.emit(&[0; 4]);
            let rel = done as i32 - (p as i32 + 4);
            Self::patch_or_bail(&mut lo.buf, p, rel)
        };
        for t in std::mem::take(&mut self.deferred_putfield_tails) {
            match t {
                DeferredPutfieldTail::Helper {
                    jumps,
                    base_slot,
                    field_index,
                    value_slot,
                    done,
                } => {
                    for p in jumps {
                        self.patch_rel32_to_here(p);
                    }
                    self.load_context(CALL_ARG_REGS[0]);
                    self.load_reg_from_frame(CALL_ARG_REGS[1], base_slot);
                    self.emit_mov_reg_imm64(CALL_ARG_REGS[2], field_index as u64);
                    self.load_reg_from_frame(CALL_ARG_REGS[3], value_slot);
                    self.emit_mov_reg_imm64(RAX, self.putfield_object as u64);
                    self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
                    if !jmp_done(self, done) {
                        return;
                    }
                }
                DeferredPutfieldTail::G1 {
                    cross_region,
                    to_helper,
                    cell_off,
                    base_slot,
                    field_index,
                    value_slot,
                    done,
                } => {
                    if let Some(cr) = cross_region {
                        self.patch_rel32_to_here(cr);
                        self.buf.emit(&[0x48, 0x89, 0x90]); // MOV [RAX + disp32], RDX
                        self.buf.emit(&cell_off.to_le_bytes());
                        if let Some((bytes, skips)) =
                            crate::x64::g1_young_holder_skip_bytes(RCX, Some(R10), R11)
                        {
                            let at = self.buf.pos();
                            self.buf.emit(&bytes);
                            if !skips.is_empty() {
                                let not_young = self.emit_jmp_rel32();
                                for s in skips {
                                    self.patch_rel32_to_here(at + s);
                                }
                                if !jmp_done(self, done) {
                                    return;
                                }
                                self.patch_rel32_to_here(not_young);
                            }
                        }
                        if let Some(g) =
                            crate::ir_g1_edge_filter::g1_edge_filter_geom(&self.direct_helpers)
                        {
                            let (bytes, misses) =
                                crate::ir_g1_edge_filter::g1_edge_filter_hit_bytes(&g);
                            let at = self.buf.pos();
                            self.buf.emit(&bytes);
                            if !jmp_done(self, done) {
                                return;
                            }
                            for m in misses {
                                self.patch_rel32_to_here(at + m);
                            }
                        }
                        self.load_context(CALL_ARG_REGS[0]);
                        self.load_reg_from_frame(CALL_ARG_REGS[1], base_slot);
                        self.load_reg_from_frame(CALL_ARG_REGS[2], value_slot);
                        cratonvm_jit_api::assert_helper_call_shape!(
                            "g1_post_write_barrier",
                            int_args = 3,
                            returns_value = false
                        );
                        self.emit_mov_reg_imm64(RAX, self.g1_post_write_barrier as u64);
                        self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
                        if !jmp_done(self, done) {
                            return;
                        }
                    }
                    if !to_helper.is_empty() {
                        for p in to_helper {
                            self.patch_rel32_to_here(p);
                        }
                        self.load_context(CALL_ARG_REGS[0]);
                        self.load_reg_from_frame(CALL_ARG_REGS[1], base_slot);
                        self.emit_mov_reg_imm64(CALL_ARG_REGS[2], field_index as u64);
                        self.load_reg_from_frame(CALL_ARG_REGS[3], value_slot);
                        self.emit_mov_reg_imm64(RAX, self.putfield_object as u64);
                        self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
                        if !jmp_done(self, done) {
                            return;
                        }
                    }
                }
            }
        }
    }

    pub(in crate::ir_lower) fn emit_deferred_gen_alloc_tails(&mut self) {
        for mut tail in std::mem::take(&mut self.deferred_gen_alloc_tails) {
            for p in tail.slow {
                self.patch_rel32_to_here(p);
            }
            if !tail.slow_map_bytes.is_empty() {
                self.buf.emit(&tail.slow_map_bytes);
            }
            if tail.ensure_frame_block && self.frame_block_shared && !self.frame_block.is_empty() {
                self.emit_frame_block_ensure();
            }
            let return_address = match tail.kind {
                DeferredGenAllocKind::New {
                    class_id,
                    num_fields,
                    stub_target,
                } => crate::runtime_lowering::emit_new_object_stub(
                    &mut self.buf,
                    self.context_slot_off,
                    stub_target,
                    class_id,
                    num_fields,
                    self.frame_record,
                ),
                DeferredGenAllocKind::Array {
                    target,
                    immediate,
                    length_slot,
                } => crate::runtime_lowering::emit_new_array_stub(
                    &mut self.buf,
                    self.context_slot_off,
                    target,
                    immediate,
                    length_slot,
                    self.frame_record,
                ),
            };
            self.anchor_safepoint_return(tail.map.as_mut(), return_address);
            let saved = (self.cur_node_pc, self.cur_bci);
            self.cur_node_pc = tail.node_pc;
            self.cur_bci = tail.site_bci;
            self.note_inline_frame_return_site_at(return_address);
            self.cur_node_pc = saved.0;
            self.cur_bci = saved.1;
            self.emit_identity_after_shared_stub();
            self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
            let ok = self.emit_jcc_rel32(0x85); // JNZ resume
            let rel = tail.resume as i32 - (ok as i32 + 4);
            if !Self::patch_or_bail(&mut self.buf, ok, rel) {
                return;
            }
            self.emit_mov_reg_imm64(RAX, i64::MIN as u64);
            self.buf.emit_byte(0xE9); // JMP shared exception epilogue
            let exc = self.buf.pos();
            self.buf.emit(&[0; 4]);
            self.call_exc_patches.push((exc, tail.exit_bci));
        }
    }

    pub(in crate::ir_lower) fn emit_deferred_getfield_tails(&mut self) {
        for t in std::mem::take(&mut self.deferred_getfield_tails) {
            let mut slow = t.slow;
            let jmp_done = |lo: &mut Self| -> bool {
                lo.buf.emit_byte(0xE9); // JMP rel32 → done
                let p = lo.buf.pos();
                lo.buf.emit(&[0; 4]);
                // Cast: code offsets of one method, far below `i32::MAX`.
                let rel = t.done as i32 - (p as i32 + 4);
                Self::patch_or_bail(&mut lo.buf, p, rel)
            };
            if let Some(legacy_patch) = t.legacy_patch {
                self.patch_rel32_to_here(legacy_patch);
                self.emit_getfield_legacy_read(
                    t.ref_node,
                    t.type_tag,
                    t.legacy_cell_off,
                    &mut slow,
                    t.result_gpr.unwrap_or(RAX),
                    t.receiver_gpr,
                );
                if !jmp_done(self) {
                    return;
                }
            }
            // The checked helper.
            for p in slow {
                self.patch_rel32_to_here(p);
            }
            // JIT round 25 item 2, wave 4: the site's lazy references, spilled
            // for the helper's collection (reloaded after the sentinel check).
            self.emit_lazy_ref_spill_stores(&t.lazy_spills);
            self.load_context(CALL_ARG_REGS[0]);
            match t.base_src {
                SelfCallArgSource::Reg(r) => self.emit_mov_reg_reg64(CALL_ARG_REGS[1], r),
                SelfCallArgSource::Home(off) => self.load_reg_from_frame(CALL_ARG_REGS[1], off),
                SelfCallArgSource::Imm(v) => self.emit_mov_reg_imm64(CALL_ARG_REGS[1], v as u64),
            }
            self.emit_mov_reg_imm64(CALL_ARG_REGS[2], t.arg2);
            self.emit_mov_reg_imm64(RAX, self.getfield as u64);
            self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
            // The exit names the site's bci, with no pad or catch arm (the
            // admission's premise).
            let saved = (
                self.cur_bci,
                self.cur_node_pc,
                self.cur_call_throw.take(),
                self.cur_catch_arm.take(),
            );
            self.cur_bci = t.site_bci;
            self.cur_node_pc = t.node_pc;
            self.emit_helper_sentinel_check(t.node_ty);
            self.emit_lazy_ref_reloads(&t.lazy_spills);
            self.cur_bci = saved.0;
            self.cur_node_pc = saved.1;
            self.cur_call_throw = saved.2;
            self.cur_catch_arm = saved.3;
            if let Some(dst) = t.result_gpr {
                self.emit_mov_reg_reg64(dst, RAX);
            }
            if !jmp_done(self) {
                return;
            }
        }
    }

    /// The site half of [`Self::push_call_exc_patch`] for an admitted deferred
    /// cross call (unprotected bci, so its plain branch): the bci is a transfer
    /// bci and the monitor fact is taken now; the deferred tail pushes the row.
    /// (zt t2/ca: this doc had drifted onto `zgc_alloc_tail_admitted` when
    /// zs s9/ca4 inserted that function above this one.)
    pub(in crate::ir_lower) fn note_deferred_cross_call_exit(&mut self) -> usize {
        let bci = self.note_deferred_self_call_exit();
        self.transfer_bcis.insert(bci);
        self.note_flat_transfer(bci, "deferred_cross_call_exit", true);
        bci
    }

    /// Emit every deferred direct self-call cold half (round 13 wave 8, lane
    /// callcost3, CC2-1), right after the body and before anything reads
    /// `call_exc_patches` (the OSR facts, `emit_call_exc_stub`).
    ///
    /// Each is the inline sequence it replaces, byte for byte except that the
    /// site's inverted branch enters it and a `JMP` returns to the site:
    ///
    /// * **StackGuard**: the deferred id, `CALL guard(ctx)`, `TEST; JNE` the
    ///   call-exception exit, `JMP .ok`;
    /// * **Sentinel** (entered with `RAX == i64::MIN`, so the service's own
    ///   `CMP; JNO` is omitted): the deferred id, the identity half, the
    ///   callee-deopt service staging from the captured sources, the re-check
    ///   `CMP; JNO .keep`, the `dispatch_threw` peek, `JNE` the exit,
    ///   `JMP .keep`.
    ///
    /// Nothing between the site's `Jcc` and the tail touches a register or a
    /// frame word, so the tail runs in exactly the state the inline code ran
    /// in; every value it emits is a per-compile constant or was captured at
    /// the site.
    pub(in crate::ir_lower) fn emit_deferred_self_call_tails(&mut self) {
        for tail in std::mem::take(&mut self.deferred_self_call_tails) {
            match tail {
                DeferredSelfCallTail::StackGuard {
                    entry_patch,
                    resume,
                    sp_id,
                    mut map,
                    copy_back,
                    bci,
                } => {
                    self.patch_rel32_to_here(entry_patch);
                    self.emit_deferred_safepoint_id_store(sp_id, &mut map);
                    self.load_context(CALL_ARG_REGS[0]);
                    self.emit_mov_reg_imm_smart(CALL_ARG_REGS[1], 0);
                    self.emit_mov_reg_imm64(RAX, self.self_call_stack_guard as u64);
                    self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
                    self.anchor_safepoint_return(map.as_mut(), self.buf.pos());
                    self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
                    let exc = self.emit_jcc_rel32(0x85); // JNE call-exception exit
                    self.call_exc_patches.push((exc, bci));
                    if let Some((thread_slot_off, savebase_slot_off, offsets)) = copy_back {
                        crate::runtime_lowering::emit_shadow_copy_back(
                            &mut self.buf,
                            &crate::runtime_lowering::ShadowCopyBack {
                                thread_slot_off,
                                savebase_slot_off,
                                offsets: &offsets,
                            },
                        );
                    }
                    self.emit_jmp_back_to(resume);
                }
                DeferredSelfCallTail::Sentinel {
                    entry_patch,
                    resume,
                    sp_id,
                    mut map,
                    copy_back,
                    identity_elided,
                    info_ptr,
                    args,
                    bci,
                    lean,
                } => {
                    self.patch_rel32_to_here(entry_patch);
                    self.emit_deferred_safepoint_id_store(sp_id, &mut map);
                    if identity_elided {
                        self.emit_frame_record_identity();
                    }
                    // THROWS3-3 (lane throws4): the service's nothing-stashed
                    // early out; a narrow result leaves on its sentinel below.
                    let lean_bit = if lean {
                        crate::SERVICE_ARGS_LEAN_PROPAGATE_BIT
                    } else {
                        0
                    };
                    if self.service_callee_deopt != 0 {
                        // `emit_callee_deopt_service_cold`, `after_reload =
                        // false`: republish, copy back any pending shadow roots,
                        // then stage arguments from their refreshed sources.
                        self.emit_post_call_frame_record();
                        if let Some((thread_slot_off, savebase_slot_off, offsets)) = copy_back {
                            crate::runtime_lowering::emit_shadow_copy_back(
                                &mut self.buf,
                                &crate::runtime_lowering::ShadowCopyBack {
                                    thread_slot_off,
                                    savebase_slot_off,
                                    offsets: &offsets,
                                },
                            );
                        }
                        for (i, &(source, sign_extend)) in args.iter().enumerate() {
                            self.emit_load_rax_from_arg_source(source);
                            if sign_extend {
                                self.buf.emit(&[0x48, 0x63, 0xC0]); // MOVSXD RAX, EAX
                            }
                            // Cast: an argument index is bounded by the callee's
                            // parameter count, so `i * 8` cannot overflow an x86-64
                            // displacement.
                            self.store_abi_reg(RAX, self.args_stage_top_off - (i as i32) * 8);
                        }
                        self.load_context(CALL_ARG_REGS[0]);
                        self.emit_mov_reg_imm64(CALL_ARG_REGS[1], info_ptr as u64);
                        self.lea_reg_from_frame(CALL_ARG_REGS[2], self.args_stage_top_off);
                        self.emit_mov_reg_imm64(CALL_ARG_REGS[3], args.len() as u64 | lean_bit);
                        self.emit_mov_reg_imm64(RAX, self.service_callee_deopt as u64);
                        self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
                        self.anchor_safepoint_return(map.as_mut(), self.buf.pos());
                        self.emit_post_call_frame_record();
                    }
                    self.emit_cmp_rax_sentinel();
                    let serviced = self.emit_jcc_rel32(0x81); // JNO .keep
                    let exc = if lean {
                        // A narrow result is never `i64::MIN` (OF still set:
                        // taken), as `emit_call_return_sentinel_tail`'s
                        // non-wide exit.
                        self.emit_jcc_rel32(0x80) // JO call-exception exit
                    } else {
                        self.emit_mov_reg_imm64(RAX, self.dispatch_threw as u64);
                        self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
                        self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
                        self.emit_mov_reg_imm64(RAX, i64::MIN as u64); // restore (MOV preserves ZF)
                        self.emit_jcc_rel32(0x85) // JNE call-exception exit
                    };
                    self.call_exc_patches.push((exc, bci));
                    self.emit_jmp_back_to(resume);
                    let rel = resume as i32 - (serviced as i32 + 4);
                    Self::patch_or_bail(&mut self.buf, serviced, rel);
                }
                DeferredSelfCallTail::CrossCallSentinel {
                    mut map,
                    entry_patch,
                    resume,
                    info_ptr,
                    args,
                    wide,
                    bci,
                    lean,
                    republish,

                    owned,
                } => {
                    // THROWS4-1 (lane throws5): the service's nothing-stashed
                    // early out for a statically bound site.
                    let lean_bit = if lean {
                        crate::SERVICE_ARGS_LEAN_PROPAGATE_BIT
                    } else {
                        0
                    };
                    self.patch_rel32_to_here(entry_patch);
                    // W7-C: before the service and the exit, which read it.
                    if republish {
                        self.emit_post_call_frame_record();
                    }
                    if self.service_callee_deopt != 0 {
                        // `emit_callee_deopt_service_cold` with `after_reload`
                        // (the site republished and reloaded before its `JO`),
                        // minus its entry `CMP; JNO`: RAX is the sentinel here.
                        for (i, &source) in args.iter().enumerate() {
                            self.emit_load_rax_from_arg_source(source);
                            // Cast: an argument index is bounded by the callee's
                            // parameter count, so `i * 8` cannot overflow an x86-64
                            // displacement.
                            self.store_abi_reg(RAX, self.args_stage_top_off - (i as i32) * 8);
                        }
                        self.load_context(CALL_ARG_REGS[0]);
                        self.emit_mov_reg_imm64(CALL_ARG_REGS[1], info_ptr as u64);
                        if owned {
                            // OOM21-1 (lane argwin): no buffer, the owned word
                            // (with the lean bit's own encoding when lean).
                            self.emit_mov_reg_imm64(CALL_ARG_REGS[2], 0);
                            // Cast: the service word's bits.
                            self.emit_mov_reg_imm64(
                                CALL_ARG_REGS[3],
                                crate::service_args_word(None, lean) as u64,
                            );
                        } else {
                            self.lea_reg_from_frame(CALL_ARG_REGS[2], self.args_stage_top_off);
                            self.emit_mov_reg_imm64(CALL_ARG_REGS[3], args.len() as u64 | lean_bit);
                        }
                        self.emit_mov_reg_imm64(RAX, self.service_callee_deopt as u64);
                        self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
                        self.anchor_safepoint_return(map.as_mut(), self.buf.pos());
                    }
                    // `emit_call_return_sentinel_tail`, its exit row pushed here.
                    self.emit_cmp_rax_sentinel();
                    if wide {
                        let keep = self.emit_jcc_rel32(0x81); // JNO .keep
                        self.emit_mov_reg_imm64(RAX, self.dispatch_threw as u64);
                        self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
                        self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
                        self.emit_mov_reg_imm64(RAX, i64::MIN as u64); // restore (MOV preserves ZF)
                        let exc = self.emit_jcc_rel32(0x85); // JNE call-exception exit
                        self.call_exc_patches.push((exc, bci));
                        self.emit_jmp_back_to(resume);
                        let rel = resume as i32 - (keep as i32 + 4);
                        Self::patch_or_bail(&mut self.buf, keep, rel);
                    } else {
                        let exc = self.emit_jcc_rel32(0x80); // JO call-exception exit
                        self.call_exc_patches.push((exc, bci));
                        self.emit_jmp_back_to(resume);
                    }
                }
            }
        }
    }

    /// `RAX` from a captured argument source: the instruction
    /// `gp_load_value(RAX, arg)` emitted for it at the site.
    pub(in crate::ir_lower) fn emit_load_rax_from_arg_source(&mut self, source: SelfCallArgSource) {
        match source {
            SelfCallArgSource::Imm(val) => self.emit_mov_reg_imm_smart(RAX, val),
            SelfCallArgSource::Reg(reg) => self.emit_mov_reg_reg64_raw(RAX, reg),
            SelfCallArgSource::Home(off) => self.load_reg_from_frame(RAX, off),
        }
    }

    /// `JMP rel32` to the earlier offset `target` (a deferred tail's return).
    pub(in crate::ir_lower) fn emit_jmp_back_to(&mut self, target: usize) {
        let patch = self.emit_jmp_rel32();
        let rel = target as i32 - (patch as i32 + 4);
        Self::patch_or_bail(&mut self.buf, patch, rel);
    }

    /// JIT round 21, lane argwin (OOM21-1 step 1,
    /// `CRATONVM_JIT_IR_CALLEE_OWNED_ARGS`): may the plain direct cross call
    /// being lowered -- to `entry`, through `info_ptr`, at node pc `pc` --
    /// leave its arguments to the callee? The site half is
    /// [`ir_direct_call_site_admits_owned_args`]; the callee half,
    /// `crate::direct_callee_owns_its_arguments`, is asked last (it takes the
    /// owner registry's lock). See the section above
    /// [`ir_callee_owned_args_enabled`] for why each term is needed. `false`
    /// without the method's bytecode (`owned_args_bytecode`), which is every
    /// compile without the switch and every debugger-observed one.
    pub(in crate::ir_lower) fn ir_direct_call_args_owned_by_callee(
        &self,
        entry: usize,
        info_ptr: usize,
        pc: usize,
    ) -> bool {
        let Some(bytecode) = self.owned_args_bytecode else {
            return false;
        };
        if entry == 0 || info_ptr == 0 {
            return false;
        }
        // SAFETY: as every other read of an `Op::Call`'s `info_ptr` in this
        // file -- a live `JitInvokeInfo` owned by `ir_call_infos` for the
        // lifetime of this compile.
        let info: &JitInvokeInfo = unsafe { &*(info_ptr as *const JitInvokeInfo) };
        let code = bytecode.code;
        let in_splice =
            self.pc_is_in_a_spliced_body(pc) || resolve_spliced_pc(self.spliced_ranges, pc) != pc;
        // Exits that read this frame after the call: a call-site exception pad,
        // a compiled-handler arm, a reason-9 rethrow pad (precise route).
        let armed_exits = self.cur_call_throw.is_some()
            || self.cur_catch_arm.is_some()
            || self.bci_is_protected(pc);
        // j27 ARGFIX2: a spliced call is admitted only through the two-level
        // replay rule, which also names the call's own opcode.
        let splice_verdict = (in_splice && ir_callee_owned_args_splice_replay_enabled())
            .then(|| self.owned_args_splice_verdict(pc, bytecode));
        let splice_opcode = splice_verdict.and_then(Result::ok);
        let admitted = ir_direct_call_site_admits_owned_args(
            splice_opcode.or_else(|| code.get(pc).copied()),
            info.invoke_kind,
            pc,
            bytecode.handlers,
            self.graph.osr_compile,
            self.self_lock.is_some(),
            in_splice && splice_opcode.is_none(),
            armed_exits,
            || {
                if in_splice {
                    // Every level was already replayed by the verdict.
                    return splice_opcode.is_some();
                }
                // The single-pass rule's own function (ARGWIN21-4 included;
                // j27 OOM21: plus constant `putstatic`s into plain statics).
                u32::try_from(pc).is_ok_and(|bci| {
                    crate::deopt::owned_args_caller_replay_is_exact_with_statics(
                        code,
                        code.len(),
                        bci,
                        bytecode.handlers,
                        &|at: usize| self.own_putstatic_is_plain(at),
                    )
                })
            },
        );
        let owned = admitted && crate::direct_callee_owns_its_arguments(entry);
        if false {
            eprintln!(
                "[ir-owned-args] site bci={pc} {}.{}{} kind={} admitted={admitted} owned={owned}{}{}",
                info.class_name,
                info.method_name,
                info.descriptor,
                info.invoke_kind,
                splice_verdict.map_or(String::new(), |v| format!(" splice-replay={v:?}")),
                // j27 wave 6 (lane T1SPLICE): census of the level replay
                // (`owned_args_splice_replay::level_replay_plan`, NOT wired),
                // for the sites the physical re-run cannot serve.
                if in_splice || self.graph.osr_compile {
                    format!(" level-replay={:?}", self.owned_args_level_replay(pc, bytecode))
                } else {
                    String::new()
                },
            );
        }
        owned
    }

    /// j27 wave 6 (lane T1SPLICE): [`Self::owned_args_splice_verdict`]'s
    /// inputs asked for the level replay plan
    /// (`owned_args_splice_replay::spliced_call_level_replay`). Diagnostic
    /// only: `Ok(k)` names the outermost level a decline could resume from.
    pub(in crate::ir_lower) fn owned_args_level_replay(
        &self,
        pc: usize,
        bytecode: DeadRefClearBytecode<'_>,
    ) -> Result<usize, crate::owned_args_splice_replay::ChainRefusal> {
        crate::owned_args_splice_replay::spliced_call_level_replay(
            pc,
            &self.inline_frame_sites.bodies_enclosing(pc),
            bytecode.code,
            bytecode.handlers,
            bytecode.spliced_bodies,
            self.graph.osr_compile,
            self.self_lock.is_some() || !self.graph.sync_splice_windows.is_empty(),
        )
    }

    /// j27 OOM21: does the `putstatic` at the compiling method's own pc `at`
    /// write a non-volatile static (`owned_args_splice_replay::plain_static_store`
    /// over the graph nodes carrying that pc)? A spliced pc answers `false`.
    pub(in crate::ir_lower) fn own_putstatic_is_plain(&self, at: usize) -> bool {
        !self.pc_is_in_a_spliced_body(at)
            && crate::owned_args_splice_replay::plain_static_store(
                self.graph
                    .nodes
                    .iter()
                    .filter(|n| n.bytecode_pc == Some(at))
                    .map(|n| &n.op),
            )
    }

    /// j27 ARGFIX2 (`ir_callee_owned_args_splice_replay_enabled`): the
    /// two-level replay rule for the call at spliced pc `pc`, over the chain
    /// the frame-site table names and the unmasked body codes `lib.rs`
    /// `spliced_body_codes` handed in. `Ok(the call's opcode)` when admitted.
    pub(in crate::ir_lower) fn owned_args_splice_verdict(
        &self,
        pc: usize,
        bytecode: DeadRefClearBytecode<'_>,
    ) -> Result<u8, crate::owned_args_splice_replay::ChainRefusal> {
        crate::owned_args_splice_replay::spliced_call_replay(
            pc,
            &self.inline_frame_sites.bodies_enclosing(pc),
            bytecode.code,
            bytecode.handlers,
            bytecode.spliced_bodies,
            self.graph.osr_compile,
            self.self_lock.is_some() || !self.graph.sync_splice_windows.is_empty(),
        )
    }

    /// gkr (OOM21-2): the site terms of [`ir_direct_call_site_admits_owned_args`]
    /// without the caller-replay one, for a direct call the hand-off refused:
    /// statically bound, no OSR compile, no self lock, not in a splice, no
    /// armed exit and no handler covering it. Its sentinel then leaves through
    /// the frameless shared exit, so after the CALL nothing but the
    /// callee-deopt service reads the argument-only homes.
    pub(in crate::ir_lower) fn ir_direct_call_site_admits_service_copy_window(&self, info_ptr: usize, pc: usize) -> bool {
        let Some(bytecode) = self.owned_args_bytecode else {
            if false {
                eprintln!("[ir-owned-args] service-copy-site bci={pc}: refused, no bytecode");
            }
            return false;
        };
        if info_ptr == 0 {
            if false {
                eprintln!("[ir-owned-args] service-copy-site bci={pc}: refused, no invoke info");
            }
            return false;
        }
        // SAFETY: as `ir_direct_call_args_owned_by_callee`'s read of the same
        // `Op::Call` info pointer.
        let info: &JitInvokeInfo = unsafe { &*(info_ptr as *const JitInvokeInfo) };
        let in_splice =
            self.pc_is_in_a_spliced_body(pc) || resolve_spliced_pc(self.spliced_ranges, pc) != pc;
        let armed_exits = self.cur_call_throw.is_some()
            || self.cur_catch_arm.is_some()
            || self.bci_is_protected(pc);
        let refusal = ir_direct_call_site_owned_args_refusal(
            bytecode.code.get(pc).copied(),
            info.invoke_kind,
            pc,
            bytecode.handlers,
            // j27 wave 9 (W8OSR wiring): the window hands nothing off, so an
            // OSR body may record copies under the opt-in switch; the owned
            // hand-off keeps the plain `osr_compile`.
            crate::osr_service_copy_window::service_copy_window_osr_term(self.graph.osr_compile),
            self.self_lock.is_some(),
            in_splice,
            armed_exits,
            || true,
        );
        if let Some(why) = refusal {
            if false {
                eprintln!("[ir-owned-args] service-copy-site bci={pc}: refused, {why}");
            }
            false
        } else {
            true
        }
    }

    /// gkr (OOM21-2): record the refused direct call returning to `ret`
    /// together with its argument-only homes (`direct_call_service_copies`),
    /// leaving out a home this site's shadow push copied by VALUE (outside the
    /// frame block): clearing the home alone would leave the copy rooting it.
    pub(in crate::ir_lower) fn note_ir_service_arg_copies(&mut self, ret: usize) {
        let homes = std::mem::take(&mut self.direct_call_service_copies);
        let block = !self.frame_block.is_empty();
        let slots: Vec<i16> = homes
            .iter()
            .filter_map(|&off| i16::try_from(off).ok())
            .filter(|off| block || !self.pending_shadow.contains(off))
            .collect();
        if false {
            eprintln!(
                "[ir-owned-args] service-copies ret={ret} homes {homes:?} recorded {slots:?} block={block}"
            );
        }
        if let (false, Ok(ret)) = (slots.is_empty(), u32::try_from(ret)) {
            self.service_arg_copies.push((ret, slots.into_boxed_slice()));
        }
    }

    /// OOM21-1 step 2 (`CRATONVM_JIT_IR_CALLEE_ARG_WINDOW`): the frame homes of
    /// the argument-only reference arguments of the owned direct call `call`
    /// (node pc `pc`, value inputs `inputs[2..2 + num_args]`), ascending and
    /// distinct. Empty -- zero nothing -- off the switch, when the call is not
    /// numbered or its block lies on a cycle, or when no argument qualifies.
    /// The rule and the reasons are in the section above
    /// [`ir_callee_owned_args_enabled`]; every unknown answers "keep".
    pub(in crate::ir_lower) fn owned_call_arg_window(
        &self,
        call: NodeId,
        inputs: &[NodeId],
        num_args: usize,
        pc: usize,
    ) -> Vec<i32> {
        self.argument_only_homes(call, inputs, num_args, pc)
    }

    /// The rule of [`Self::owned_call_arg_window`] without its switch: the
    /// frame homes of `call`'s argument-only reference arguments. gkr
    /// (OOM21-2) also asks it at a direct call the hand-off refuses, whose
    /// copies only the callee-deopt service reads after the CALL
    /// ([`Self::note_ir_service_arg_copies`]).
    pub(in crate::ir_lower) fn argument_only_homes(
        &self,
        call: NodeId,
        inputs: &[NodeId],
        num_args: usize,
        pc: usize,
    ) -> Vec<i32> {
        let mut out: Vec<i32> = Vec::new();
        let Some(bytecode) = self.owned_args_bytecode else {
            return out;
        };
        let Some(call_pos) = self.slot_plan.pos_of.get(call as usize).copied().flatten() else {
            return out;
        };
        let Ok(verified) = cratonvm_reader::verified_code(bytecode.code) else {
            return out;
        };
        let Some((readable, call_cycle, reachable_bcis)) =
            self.snapshots_readable_after_call(call, pc, bytecode, &verified)
        else {
            return out;
        };
        // Slot-plan intervals include frame snapshots. Their post-call reads
        // are checked separately by the readable-state analysis; use value-only
        // ranges so an entry-replayable splice is not kept alive by its outer
        // invoke snapshot.
        let value_ranges = value_use_ranges(self.graph, self.schedule, self.slot_plan);
        // The keep set's own narrowing (gen r5w6/oomjit10, under the precise
        // keep set only): a LOCAL a snapshot at one of this method's own
        // instructions names is not read there when bytecode liveness calls it
        // dead (exception edges included). The operand stack and the monitors
        // are never narrowed.
        let local_live = true.then(|| {
            crate::regalloc::live_locals_per_pc_all(
                bytecode.code,
                bytecode.code.len(),
                bytecode.num_params,
                &[],
                bytecode.handlers,
                bytecode.num_locals,
            )
        });
        // j27 ARGFIX2: at a spliced owned call, a chain state's scope locals
        // are narrowed by each scope's own liveness, as the keep set does
        // ([`splice_snapshot_dead_slots`], same conditions). Empty otherwise.
        let splice_live: Vec<(&str, (Vec<u64>, Vec<bool>, usize), usize)> =
            if self.owned_window_per_level(pc)
                && local_live.is_some()
                && ir_keep_set_splice_local_liveness_enabled()
            {
                bytecode
                    .spliced_bodies
                    .iter()
                    .map(|b| (b.method_key.as_str(), spliced_body_liveness(b), b.num_locals))
                    .collect()
            } else {
                Vec::new()
            };
        let splice_bodies: HashMap<&str, (&(Vec<u64>, Vec<bool>, usize), usize)> =
            splice_live.iter().map(|(key, live, n)| (*key, (live, *n))).collect();
        let splice_root = local_live.as_ref().map(|live| (live, bytecode.num_locals));
        let Some(args) = inputs.get(2..2 + num_args) else {
            return out;
        };
        for &a in args {
            // In a loop, the invoke's state can be reached again after a fresh
            // allocation. Only snapshots for which every bytecode path from
            // this invoke crosses the allocation can treat the static IR node
            // as the next iteration's value. Earlier states remain readers.
            let fresh_cycle_array = call_cycle && self.owned_window_fresh_cycle_array(call, a);
            let fresh_cycle_osr_array = fresh_cycle_array
                && self.graph.osr_compile
                && !self.pc_is_in_a_spliced_body(pc);
            let fresh_array_pc = fresh_cycle_osr_array
                .then(|| self.graph.nodes.get(a as usize))
                .flatten()
                .filter(|node| matches!(node.op, Op::NewArray { .. }))
                .and_then(|node| node.bytecode_pc);
            let named = |sp: &crate::ir::SafepointSnapshot, value: NodeId| {
                if fresh_cycle_array && sp.bci == pc {
                    return false;
                }
                if fresh_cycle_osr_array
                    && !self.pc_is_in_a_spliced_body(sp.bci)
                    && reachable_bcis.contains(&sp.bci)
                    && fresh_array_pc.is_some_and(|array_pc| {
                        !ir_owned_cycle_bytecode_path_without_array(
                            pc,
                            sp.bci,
                            array_pc,
                            |p| verified.successors(p),
                            bytecode.handlers,
                        )
                    })
                {
                    return false;
                }
                let own_instruction =
                    verified.is_instruction_start(sp.bci) && !self.pc_is_in_a_spliced_body(sp.bci);
                let dead = (!splice_bodies.is_empty())
                    .then(|| splice_snapshot_dead_slots(sp, &splice_bodies, splice_root))
                    .flatten()
                    .unwrap_or_default();
                crate::owned_args_splice_replay::frame_names_value(
                    &sp.locals,
                    &sp.stack,
                    &sp.monitors,
                    value,
                    (dead.0.as_slice(), dead.1.as_slice()),
                    |k| {
                        own_instruction
                            && local_live.as_ref().is_some_and(|live| {
                                local_dead_at(live, bytecode.num_locals, sp.bci, k)
                            })
                    },
                )
            };
            match self.owned_call_window_home(
                a,
                args,
                inputs,
                call_pos,
                value_ranges.as_deref(),
                &readable,
                named,
            ) {
                Ok(off) => out.push(off),
                Err(why) => {
                    if false {
                        eprintln!("[ir-owned-args] window n{call} bci={pc} arg n{a}: kept, {why}");
                    }
                }
            }
        }
        out.sort_unstable();
        out.dedup();
        if false {
            eprintln!("[ir-owned-args] window n{call} bci={pc} pos={call_pos}: homes {out:?}");
        }
        out
    }

    /// OOM21-1 step 2: every frame state a deopt, an exception transfer or a
    /// mode exit can read once the owned call `call` (node pc `pc`) has
    /// transferred, indexed like `graph.safepoints`, together with whether a
    /// control-flow path returns to the call block -- or `None` (window
    /// nothing) when a table or decode is missing.
    ///
    /// * the schedule's answer: every node after the call in its block, and
    ///   every node and terminator of every block reachable from it, reads its
    ///   own snapshot (`Node::frame_snapshot`) and every snapshot at its pc or
    ///   at the pc it resolves to through the splices;
    /// * the bytecode's answer: every snapshot at a bci reachable from the
    ///   invoke's successors over normal and exception edges (a splice's
    ///   states resolve to their enclosing invoke's bci);
    /// * a snapshot at no instruction of the method and in no splice (a
    ///   per-copy state) is readable everywhere.
    ///
    pub(in crate::ir_lower) fn snapshots_readable_after_call(
        &self,
        call: NodeId,
        pc: usize,
        bytecode: DeadRefClearBytecode<'_>,
        verified: &cratonvm_reader::VerifiedCode,
    ) -> Option<(Vec<bool>, bool, std::collections::HashSet<usize>)> {
        let graph = self.graph;
        let schedule = self.schedule;
        let spliced = self.spliced_ranges;
        let nb = schedule.blocks.len();
        let call_block = schedule
            .node_to_block
            .get(call as usize)
            .copied()
            .filter(|&b| b < nb)?;
        let own_pc = resolve_spliced_pc(spliced, pc);
        // Schedule: the blocks after the call's.
        let mut seen = vec![false; nb];
        let mut work: Vec<usize> = schedule.blocks[call_block].successors.clone();
        let mut call_cycle = false;
        while let Some(b) = work.pop() {
            if b >= nb {
                return None;
            }
            if b == call_block {
                call_cycle = true;
                continue;
            }
            if std::mem::replace(&mut seen[b], true) {
                continue;
            }
            work.extend(schedule.blocks[b].successors.iter().copied());
        }
        // Bytecode: the bcis reachable from the invoke's successors.
        let (mut reach, bytecode_cycle) =
            crate::owned_args_splice_replay::reach_after_invoke_with_cycle(
                own_pc,
                |p| verified.successors(p),
                bytecode.handlers,
            )?;
        call_cycle |= bytecode_cycle;
        // j27 ARGFIX2 (`CRATONVM_JIT_IR_CALLEE_OWNED_ARGS_SPLICE_REPLAY`): at a
        // spliced owned call on an acyclic tail, judged per level of the chain
        // -- each spliced body's tail after its own `invoke` joins the reach,
        // and the splice's states no longer resolve to the outer `invoke`
        // wholesale (below). Off the rule, the single-level rule unchanged.
        let per_level = self.owned_window_per_level(pc)
            && crate::owned_args_splice_replay::seen_blocks_acyclic(&seen, |b| {
                schedule.blocks[b].successors.as_slice()
            });
        if per_level {
            reach.extend(self.spliced_levels_reach(pc, bytecode)?);
        }
        let mut read_pcs: std::collections::HashSet<usize> = reach.clone();
        let mut readable = vec![false; graph.safepoints.len()];
        let mut note_node = |id: NodeId, read_pcs: &mut std::collections::HashSet<usize>| {
            let Some(node) = graph.nodes.get(id as usize) else {
                return;
            };
            if let Some(si) = node.frame_snapshot {
                if let Some(flag) = readable.get_mut(si as usize) {
                    *flag = true;
                }
            }
            if let Some(p) = node.bytecode_pc {
                read_pcs.insert(p);
                let resolved = resolve_spliced_pc(spliced, p);
                // j27 ARGFIX2: a node of the call's own outermost splice that
                // cannot transfer ([`can_deopt`]) resumes nothing, so it does
                // not read the outer `invoke`'s flat state; one that can still
                // does (a flat resume re-executes that `invoke`).
                let reads_nothing_flat =
                    per_level && resolved == own_pc && resolved != p && !can_deopt(graph, node);
                if !reads_nothing_flat {
                    read_pcs.insert(resolved);
                }
            }
        };
        let after = schedule.blocks[call_block]
            .nodes
            .iter()
            .position(|&n| n == call)?;
        let call_blk = &schedule.blocks[call_block];
        for &n in call_blk
            .nodes
            .iter()
            .skip(after + 1)
            .chain(call_blk.terminator.iter())
        {
            note_node(n, &mut read_pcs);
        }
        for (b, &was_seen) in seen.iter().enumerate() {
            if !was_seen {
                continue;
            }
            let blk = &schedule.blocks[b];
            for &n in blk.nodes.iter().chain(blk.terminator.iter()) {
                note_node(n, &mut read_pcs);
            }
        }
        for (si, sp) in graph.safepoints.iter().enumerate() {
            let resolved = resolve_spliced_pc(spliced, sp.bci);
            let placed = verified.is_instruction_start(sp.bci) || resolved != sp.bci;
            // j27 ARGFIX2: per level, a CHAIN state is read where its own pc
            // is, or where a level reaches it (its pc or an enclosing
            // `invoke` in `reach`) -- not because the splice resolves to the
            // outer `invoke` the call itself is parked under.
            let through_splice = if per_level && sp.is_splice_state() {
                let ancestors: Vec<usize> = self
                    .inline_frame_sites
                    .bodies_enclosing(sp.bci)
                    .iter()
                    .map(|&(_, _, caller_pc, _)| caller_pc)
                    .collect();
                crate::owned_args_splice_replay::chain_state_reached(sp.bci, &ancestors, &reach)
            } else {
                read_pcs.contains(&resolved)
            };
            if !placed || read_pcs.contains(&sp.bci) || through_splice {
                readable[si] = true;
            }
        }
        Some((readable, call_cycle, reach))
    }

    /// A loop's invoke state can name the same static value node on every
    /// iteration. Exempt only a reference array allocated earlier in this
    /// exact basic block; all other cyclic arguments keep every reachable
    /// snapshot live.
    pub(in crate::ir_lower) fn owned_window_fresh_cycle_array(&self, call: NodeId, arg: NodeId) -> bool {
        let graph = self.graph;
        let schedule = self.schedule;
        let Some(call_block) = schedule
            .node_to_block
            .get(call as usize)
            .copied()
            .filter(|&b| b < schedule.blocks.len())
        else {
            return false;
        };
        let Some(arg_block) = schedule
            .node_to_block
            .get(arg as usize)
            .copied()
            .filter(|&b| b < schedule.blocks.len())
        else {
            return false;
        };
        let Some(node) = graph.nodes.get(arg as usize) else {
            return false;
        };
        let Some(arg_pos) = schedule.blocks[arg_block]
            .nodes
            .iter()
            .position(|&id| id == arg)
        else {
            return false;
        };
        let Some(call_pos) = schedule.blocks[call_block]
            .nodes
            .iter()
            .position(|&id| id == call)
        else {
            return false;
        };
        let successors: Vec<Vec<usize>> = schedule
            .blocks
            .iter()
            .map(|block| block.successors.clone())
            .collect();
        let reachable = schedule.dom.is_reachable(arg_block)
            && schedule.dom.is_reachable(call_block);
        let reexecutes_each_cycle = if arg_block == call_block {
            arg_pos < call_pos
        } else {
            let Some(call_pc) = graph
                .nodes
                .get(call as usize)
                .and_then(|node| node.bytecode_pc)
            else {
                return false;
            };
            if !graph.osr_compile || self.pc_is_in_a_spliced_body(call_pc) {
                return false;
            }
            reachable
                && schedule.dom.dominates(arg_block, call_block)
                && ir_owned_cycle_reentry_crosses_definition(
                    arg_block,
                    call_block,
                    &successors,
                )
        };
        ir_owned_cycle_arg_is_fresh_array(node.ty, &node.op, reexecutes_each_cycle)
    }

    /// j27 ARGFIX2: is the owned call at `pc` judged per level of its splice
    /// chain? Only under `CRATONVM_JIT_IR_CALLEE_OWNED_ARGS_SPLICE_REPLAY`
    /// (which alone admits a spliced owned site) and only inside a splice.
    pub(in crate::ir_lower) fn owned_window_per_level(&self, pc: usize) -> bool {
        ir_callee_owned_args_splice_replay_enabled() && self.pc_is_in_a_spliced_body(pc)
    }

    /// j27 ARGFIX2: the combined-buffer bcis every spliced body of the chain
    /// at `pc` reaches after its own parked `invoke` (over its unmasked code
    /// and handler rows), or `None` -- window nothing -- when the chain, a
    /// body's code, or its decode is missing, or a body's call is on a cycle.
    pub(in crate::ir_lower) fn spliced_levels_reach(
        &self,
        pc: usize,
        bytecode: DeadRefClearBytecode<'_>,
    ) -> Option<std::collections::HashSet<usize>> {
        let enclosing = self.inline_frame_sites.bodies_enclosing(pc);
        let levels = crate::owned_args_splice_replay::replay_levels(
            pc,
            &enclosing,
            bytecode.code,
            bytecode.handlers,
            bytecode.spliced_bodies,
        )?;
        let mut out = std::collections::HashSet::new();
        for level in levels.iter().skip(1) {
            let verified = cratonvm_reader::verified_code(level.code?).ok()?;
            let reach = crate::owned_args_splice_replay::reach_after_invoke(
                level.parked_bci,
                |p| verified.successors(p),
                level.handlers,
            )?;
            out.extend(reach.into_iter().map(|p| level.base + p));
        }
        Some(out)
    }

    /// OOM21-1 step 2: the home of argument `a` of the owned call at linear
    /// position `call_pos`, when it is argument-only (`None` keeps it): a `Ref`
    /// value with a written home in the spill band, used by the call only as
    /// arguments (ARGWIN21-2: once or several times -- the marshal has read
    /// every argument word from the one home before the window zeroes it), no
    /// phi's input, no
    /// scalar-replaced field value, no carry, whose value and colouring ranges
    /// end at the call, whose colour no other value holds across the call, and
    /// that no `readable` frame state names (`names`: does this snapshot name
    /// `a` where a resume reads it).
    pub(in crate::ir_lower) fn owned_call_window_home(
        &self,
        a: NodeId,
        args: &[NodeId],
        inputs: &[NodeId],
        call_pos: usize,
        value_ranges: Option<&[Option<LiveRange>]>,
        readable: &[bool],
        names: impl Fn(&crate::ir::SafepointSnapshot, NodeId) -> bool,
    ) -> Result<i32, &'static str> {
        let graph = self.graph;
        let plan = self.slot_plan;
        let node = graph.nodes.get(a as usize).ok_or("no node")?;
        if node.ty != IrType::Ref {
            return Err("not a non-phi ref");
        }
        // A reference phi has a real spill home just like any other value.
        // Its incoming values can still be named by a readable post-call
        // state, so include those alternatives in the state check below.
        let named_values = Self::owned_window_named_values(
            a,
            matches!(node.op, Op::Phi),
            &node.inputs,
        );
        let as_arg = args.iter().filter(|&&x| x == a).count();
        if as_arg == 0 || inputs.iter().filter(|&&x| x == a).count() != as_arg {
            return Err("not argument-only in the call");
        }
        if self.home_dropped.get(a as usize).copied().unwrap_or(false)
            || self.is_dropped_const(a)
            || self.carry_names(a)
            || plan.sr_field_values.contains(&a)
        {
            return Err("dropped, carried or scalar-replaced");
        }
        // Cast: a frame offset, far below `i32::MAX`.
        let off = self.node_slot.get(a as usize).copied().flatten().ok_or("no home")?.get() as i32;
        if off < self.first_spill || off > self.spill_cap_off {
            return Err("home outside the spill band");
        }
        let colour = plan.node_color.get(a as usize).copied().flatten().ok_or("no colour")?;
        let range = value_ranges
            .and_then(|ranges| ranges.get(a as usize).copied().flatten())
            .or_else(|| plan.range.get(a as usize).copied().flatten())
            .ok_or("no range")?;
        if range.hi > call_pos {
            return Err("live past the call");
        }
        for (o, c) in plan.node_color.iter().enumerate() {
            if o == a as usize || *c != Some(colour) {
                continue;
            }
            match plan.colouring_ranges().get(o).copied().flatten() {
                Some(r) if r.hi < call_pos || r.lo > call_pos => {}
                _ => return Err("colour shared across the call"),
            }
        }
        let phi_input = graph
            .nodes
            .iter()
            .any(|n| matches!(n.op, Op::Phi) && n.inputs.iter().skip(1).any(|&x| x == a));
        if phi_input {
            return Err("a phi input");
        }
        let named = graph
            .safepoints
            .iter()
            .enumerate()
            .any(|(si, sp)| {
                readable.get(si).copied().unwrap_or(true)
                    && named_values.iter().any(|&value| names(sp, value))
            });
        if named {
            return Err("named by a state readable after the call");
        }
        Ok(off)
    }

    /// Values whose readable snapshots keep the home of an owned reference
    /// argument live. A phi output is also retained when a readable state names
    /// one of its incoming reference values.
    pub(in crate::ir_lower) fn owned_window_named_values(a: NodeId, is_phi: bool, inputs: &[NodeId]) -> Vec<NodeId> {
        let mut values = vec![a];
        if is_phi {
            values.extend(inputs.iter().skip(1).copied());
        }
        values
    }

    /// OOM21-1 step 2: zero the planned window homes
    /// ([`Self::direct_call_arg_window`]) and their words of the site's shadow
    /// push, right before the owned call's `CALL` -- after the marshal, so the
    /// callee receives every argument. The words are written with
    /// [`Self::emit_dead_home_clear`]'s value (the poison under
    /// `CRATONVM_DBG_DEAD_HOME_POISON`). The shadow words are reached through
    /// the push's saved base under the push's own two guards (no thread: no
    /// push; a tagged base: the push overflowed and stored nothing). Clobbers
    /// R11 and the flags only, neither an argument register nor read by the
    /// `MOV RAX, entry; CALL RAX` that follows. Consumes the plan.
    pub(in crate::ir_lower) fn emit_owned_call_arg_window_clears(&mut self) {
        let offs = std::mem::take(&mut self.direct_call_arg_window);
        if offs.is_empty() {
            return;
        }
        // Each home's index in this site's push (`pending_shadow`, the push
        // order); none in the frame-block form, which publishes the homes.
        let shadow_idx: Vec<usize> = offs
            .iter()
            .filter_map(|&off| i16::try_from(off).ok())
            .flat_map(|off| {
                self.pending_shadow
                    .iter()
                    .enumerate()
                    .filter(move |&(_, &s)| s == off)
                    .map(|(i, _)| i)
            })
            .collect();
        for &off in &offs {
            self.emit_dead_home_clear(off);
        }
        if !shadow_idx.is_empty()
            && self.shadow_thread_slot_off > 0
            && self.shadow_savebase_slot_off > 0
        {
            // MOV R11, [rbp - thread_slot] ; TEST R11, R11 ; JE skip
            self.buf.emit(&[0x4C, 0x8B, 0x9D]);
            self.buf.emit(&(-self.shadow_thread_slot_off).to_le_bytes());
            self.buf.emit(&[0x4D, 0x85, 0xDB]);
            let no_thread = self.emit_jcc_rel32(0x84);
            // MOV R11, [rbp - savebase] ; TEST R11, 1 ; JNE skip
            self.buf.emit(&[0x4C, 0x8B, 0x9D]);
            self.buf
                .emit(&(-self.shadow_savebase_slot_off).to_le_bytes());
            self.buf.emit(&[0x49, 0xF7, 0xC3]);
            self.buf.emit(&1i32.to_le_bytes());
            let tagged = self.emit_jcc_rel32(0x85);
            for idx in shadow_idx {
                let Some(disp) = i32::try_from(idx).ok().and_then(|i| i.checked_mul(8)) else {
                    continue;
                };
                if self.dead_home_poison {
                    let poison = cratonvm_types::jit_activation::DEAD_HOME_POISON;
                    // Cast: the two 32-bit halves of the 64-bit poison word.
                    let (lo, hi) = (poison as u32, (poison >> 32) as u32);
                    // MOV dword [R11 + disp32], imm32 (twice)
                    self.buf.emit(&[0x41, 0xC7, 0x83]);
                    self.buf.emit(&disp.to_le_bytes());
                    self.buf.emit(&lo.to_le_bytes());
                    self.buf.emit(&[0x41, 0xC7, 0x83]);
                    self.buf.emit(&(disp + 4).to_le_bytes());
                    self.buf.emit(&hi.to_le_bytes());
                } else {
                    // MOV qword [R11 + disp32], 0
                    self.buf.emit(&[0x49, 0xC7, 0x83]);
                    self.buf.emit(&disp.to_le_bytes());
                    self.buf.emit(&0i32.to_le_bytes());
                }
            }
            self.patch_rel32_to_here(tagged);
            self.patch_rel32_to_here(no_thread);
        }
    }

    /// IR direct-call lowering — emit a statically-bound `Op::Call` as a raw
    /// `CALL` into the callee's already-compiled entry, instead of the generic
    /// `jit_invoke_dispatch` round trip.
    ///
    /// This is the IR analogue of the single-pass backend's direct-call path
    /// (`x64.rs`, `direct_calls` / `emit_call_absolute`): the eligibility loop in
    /// `lib.rs` eagerly compiles a resolved `invokestatic` / non-`<init>`
    /// `invokespecial` callee via `callee_compiler` and records
    /// `(pc → (entry, callee_needs_context))`. Both call kinds are STATICALLY
    /// bound, so no receiver TYPE check is needed — exactly why single-pass may
    /// bind them directly too.
    ///
    /// A null check is a different question, and this sentence used to answer
    /// it by omission. `invokestatic` has no receiver to test; `invokespecial`
    /// does, and JVMS 6.5 raises NPE at the INVOKE rather than inside the
    /// callee. Jumping straight to a compiled entry skips the dispatch door
    /// that used to raise it, so the only thing left to fault was the callee
    /// body — and a body that never dereferences `this` does not. See
    /// `has_receiver` below.
    ///
    /// # Why this matters
    ///
    /// `jit_invoke_dispatch` costs a full helper round trip per call
    /// (`note_jit_boundary`, an SATB flush, a `lookup_jit_code_range` scan, a
    /// re-entrant `JitEntryGuard` push/pop, and re-marshalling the staged args
    /// into `Value`s). That per-call tax is the whole reason the IR pipeline
    /// capped `invoke_ops` at 5 — an "optimizing" recompile of a call-heavy
    /// method was a NET REGRESSION versus the single-pass body, which has had
    /// direct calls all along. Lowering the statically-bound cases directly
    /// removes the tax, so the cap can be raised.
    ///
    /// # ABI
    ///
    /// The callee is an `extern "C"` compiled artifact whose prologue reads its
    /// incoming arguments from [`ENTRY_ABI_REGS`]: `abi[0]` = the hidden VM
    /// context pointer when the callee `needs_context`, then one register per
    /// Java argument. Every argument is marshalled as a raw 64-bit slot value
    /// (the VM's compact all-GPR JIT ABI — an FP argument travels as its
    /// `to_bits()` pattern in an INTEGER register, and an FP result comes back in
    /// RAX), which is exactly how each argument is already stored in its frame
    /// slot. Every source is memory, so loading straight into the ABI registers
    /// cannot inter-clobber; `RAX` (the indirect-call target scratch) is not an
    /// argument register on either ABI.
    ///
    /// # Arguments past the register file
    ///
    /// Arguments beyond `ENTRY_ABI_REGS` travel on the stack, exactly as the
    /// single-pass backend's `x64::frames::emit_stack_arg_setup` marshals them:
    /// reserve the block (Win64 shadow space included, rounded to 16 so the
    /// `CALL` stays aligned), materialise the stack args through `RAX` FIRST,
    /// then load the register args, then `CALL`, then release the block. Every
    /// source is `[rbp - off]`, which `SUB RSP` does not disturb, so the two
    /// passes cannot interfere.
    ///
    /// This is what the `HttpContentDecompressorTest` page was blocked on: a
    /// `ByteBuffer` accessor's remaining native rungs are
    /// `ScopedMemoryAccess.put*Unaligned`, which need **seven** incoming slots
    /// (receiver + five arguments + the context pointer) and so could not be
    /// bound to a thin direct helper at this door while the lowering was
    /// register-only.
    ///
    /// The callee side agrees: `extern "C"` thin VM helpers read their stack
    /// arguments the way the platform C ABI says, and a JIT-compiled callee's
    /// prologue loads them too — this tier's `emit_prologue` since Gap B (a
    /// graph wider than the register file is no longer refused), and the
    /// single-pass prologue through `emit_load_caller_arg`. (This paragraph
    /// used to say neither JIT prologue could; round 9 wave 2 corrected it.)
    ///
    /// SAFETY: `entry` is a code address produced by this JIT for the resolved
    /// callee; `lib.rs` records it in `CompiledMethod::_direct_callee_entries`,
    /// which both keeps the callee's buffer alive (`_direct_callee_roots`) and
    /// puts this method into the callee's invalidation closure, so the baked
    /// address can never outlive the code it points at.
    ///
    /// `sync_exit`: the home of the class monitor THIS frame holds around a
    /// caller-held synchronized direct CALL (round 11 wave 10, lane
    /// `synccall`; see [`Self::sync_direct_call_monitor`]), released on the
    /// exceptional edge by [`Self::emit_sync_exit_then_throw`]. Its callee
    /// body can neither trap nor call, so a sentinel return is a pending
    /// exception -- or, under `crate::caller_held_callee_resume_enabled`
    /// (shape 2 (c) of the i41 page), a poll exit the VM granted the callee,
    /// which the callee-deopt service asked on the cold side BEFORE the
    /// release resumes holding nothing of this frame's monitor. `None`
    /// is the historical lowering. The second half is the call node itself:
    /// its own result word is hidden from the exceptional edge's map (round 11
    /// wave 12, lane `calls`), and a `long`/`float`/`double` result whose bits
    /// are `i64::MIN` is told from a throw by the `dispatch_threw` peek before
    /// anything is released.
    #[allow(clippy::too_many_arguments)]
    pub(in crate::ir_lower) fn emit_direct_cross_call(
        &mut self,
        inputs: &[NodeId],
        slot: i32,
        num_args: usize,
        entry: usize,
        callee_needs_ctx: bool,
        native_context: Option<usize>,
        info_ptr: usize,
        ty: IrType,
        has_receiver: bool,
        bci: usize,
        sync_exit: Option<(i32, NodeId)>,
        map: &mut Option<SafepointMapToken>,
    ) {
        // JVMS 6.5 on argument 0 of a receiver-bearing call. Deopt rather than
        // raise inline: the interpreter re-executes this invoke and owns the
        // canonical NPE, its message and its stack trace, exactly as it does
        // for the field-access null checks elsewhere in this lowerer.
        //
        // This is all that survives of the loop that used to run here. The
        // rest of it copied every argument into the staging region, whose only
        // reader is `emit_inline_callee_deopt_service` — reached when the
        // callee returns the deopt sentinel, and otherwise never. Paying N
        // loads and N stores on the hot path so a cold path could read a
        // contiguous array made every call cost 3N memory operations where N
        // does: the register marshal below reads the same frame slots again.
        // The staging now happens inside that cold block, out of those same
        // slots, which still hold the same values there because nothing
        // between the marshal and the sentinel test writes them.
        if has_receiver && num_args > 0 {
            self.gp_load_value(RAX, inputs[2]);
            self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
            self.emit_deopt_if_zero(bci, DeoptReason::NullCheck);
        }
        // A constructor reached through a raw direct-call entry can be
        // rebound from an old C1 artifact to a newer C2 body. That edge is
        // not necessarily classified as a self-call, even when it closes a
        // recursion cycle, so it must use the exact stack-floor guard rather
        // than the sampled self-call guard. Pass the pending receiver so a
        // StackOverflowError withdraws its finalizer registration before the
        // exception is allocated.
        if has_receiver
            && num_args > 0
            && info_ptr != 0
            // SAFETY: the resolver's invoke metadata stays live throughout compilation.
            && unsafe { (*(info_ptr as *const JitInvokeInfo)).method_name == "<init>" }
            && self.self_call_stack_guard != 0
        {
            self.emit_exact_stack_floor_guard(Some(inputs[2]), map);
        }
        let base = usize::from(callee_needs_ctx);
        // Java arguments that fit the register file, and the remainder that
        // must be pushed. `saturating_sub` rather than a subtraction: a callee
        // taking the context pointer plus fewer arguments than the file holds
        // leaves `reg_capacity > num_args`, which is the ordinary case.
        let reg_capacity = ENTRY_ABI_REGS.len() - base;
        let stack_args = num_args.saturating_sub(reg_capacity);
        let (total_sub, base_disp) = stack_arg_block_size(stack_args);
        if total_sub > 0 {
            self.emit_sub_rsp_imm32(total_sub);
        }
        // Materialise the stack arguments before the register pass: this uses
        // RAX as the transfer scratch, and the register pass must be the last
        // thing before the `CALL` so nothing can clobber an ABI register after
        // it is loaded.
        for k in 0..stack_args {
            let arg = inputs[2 + reg_capacity + k];
            self.gp_load_abi_value(RAX, arg);
            // Cast: a stack-arg index is bounded by the callee's parameter
            // count, so `k * 8` cannot overflow an x86-64 displacement.
            self.emit_mov_rsp_disp_from_rax(base_disp + (k as i32) * 8);
        }
        if callee_needs_ctx {
            if let Some(context) = native_context {
                self.emit_mov_reg_imm64(ENTRY_ABI_REGS[0], context as u64);
            } else {
                self.load_context(ENTRY_ABI_REGS[0]);
            }
        }
        for i in 0..num_args.min(reg_capacity) {
            let arg = inputs[2 + i];
            self.gp_load_abi_value(ENTRY_ABI_REGS[base + i], arg);
        }
        // JIT round 21, lane argwin (OOM21-1 step 2,
        // `CRATONVM_JIT_IR_CALLEE_ARG_WINDOW`): the callee window. Every
        // argument is in its ABI register or the outgoing block now, so the
        // homes of the argument-only ones (planned by the `Op::Call` arm, owned
        // sites only) and their shadow words are zeroed here, before the
        // transfer. Writes R11, flags and frame / shadow words only.
        if self.direct_call_args_owned && sync_exit.is_none() {
            self.emit_owned_call_arg_window_clears();
        }
        // MOV RAX, entry ; CALL RAX.
        // JIT r23 wave 10, lane L6 (`CRATONVM_JIT_IR_DIRECT_CALL_IMM_ALIGN`):
        // NOP-pad so the imm64 is one 8-aligned word, which the stale-call
        // rebind can rewrite atomically (`JitCache::rebind_stale_call_site`).
        if ir_direct_call_imm_align_enabled() {
            // Cast: an address, for its alignment only.
            let at = self.buf.as_ptr() as usize + self.buf.pos();
            emit_multibyte_nop(&mut self.buf, direct_call_imm_align_pad(at));
        }
        self.emit_mov_reg_imm64(RAX, entry as u64);
        self.buf.emit(&[0xFF, 0xD0]);
        // gkr (OOM21-2): keyed by this return address, as the collector reads
        // it out of the callee frame's `[rbp + 8]`.
        if sync_exit.is_none() {
            let ret = self.buf.pos();
            self.note_ir_service_arg_copies(ret);
        }
        // The return address for THIS call is HERE -- the very next byte after
        // the `CALL` opcode -- so the inline-frame row (if this call is inside
        // a spliced body) must be recorded before ANYTHING else advances
        // `buf`, on both branches below. It used to be recorded after the
        // stack-argument release (whenever `total_sub > 0`, i.e. this call has
        // more Java arguments than fit `ENTRY_ABI_REGS`) and, on the
        // non-merged branch, after `emit_inline_callee_deopt_service`'s
        // CMP/JNO guard too -- both put the recorded offset past the real
        // return address, so `x64::InlineFrameMap::chain_for_native_offset`'s
        // exact-match lookup
        // (`vm/src/jit/conservative_roots.rs::compiled_frame_inline_chain`,
        // key 1) never found the row and silently dropped the inlined chain
        // from a stack trace built across a splice. See
        // `docs/feature-designs/profile-guided-inlining.md`.
        self.anchor_safepoint_return(map.as_mut(), self.buf.pos());
        self.note_inline_frame_return_site();
        // Release the block BEFORE anything else RUNS (as opposed to merely
        // being recorded above). The deopt service below calls back into the
        // VM and relies on the frame's own reserved shadow space at
        // `[rsp, rsp+32)`; leaving RSP lowered would point that at the (now
        // dead) stack-argument block instead.
        if total_sub > 0 {
            self.emit_add_rsp_imm32(total_sub);
        }
        if crate::x64::merged_call_sentinel_enabled() {
            // 2026-09-02: one sentinel compare on the hot path. The frame
            // republish and shadow reload come FIRST so the cold side (which
            // can run the interpreter and collect) sees this frame's own
            // identity rather than the callee's dead one. The inline-frame row
            // is already recorded above, before either.
            // W7-C: a sunk call republishes on its cold side instead (the
            // deferred tail's entry, or right past the fast skip below).
            let sunk = self.tail_sink.cur && sync_exit.is_none();
            if sunk {
                self.tail_sink.sunk_sites += 1;
            } else {
                self.emit_post_call_frame_record();
            }
            self.emit_shadow_reload();
            // Round 14 wave 3 (lane calls, CC3-1): the cold side after the
            // body, entered by the inverted `JO`, so the hot path falls through
            // to `.keep` instead of taking the `JNO` past the service.
            if sync_exit.is_none() {
                if let Some(args) = self.deferred_cross_call_service_args(inputs, num_args) {
                    self.emit_cmp_rax_sentinel();
                    let entry_patch = self.emit_jcc_rel32(0x80); // JO .cold (deferred)
                    let bci = self.note_deferred_cross_call_exit();
                    let resume = self.buf.pos();
                    self.deferred_self_call_tails
                        .push(DeferredSelfCallTail::CrossCallSentinel {
                            entry_patch,
                            resume,
                            map: map.clone(),
                            info_ptr,
                            args,
                            wide: matches!(ty, IrType::Long | IrType::Double | IrType::Float),
                            bci,
                            lean: self.direct_call_lean_site,
                            republish: sunk,

                            owned: self.direct_call_args_owned,
                        });
                    self.cross_call_cold_tails.0 += 1;
                    // .keep:
                    self.store_rax(slot);
                    return;
                }
                self.cross_call_cold_tails.1 += 1;
            }
            let keep = self.emit_call_sentinel_fast_skip();
            // W7-C: the cold side, ahead of every reader there.
            if sunk {
                self.emit_post_call_frame_record();
            }
            // Round 11 wave 12 (lane `calls`): a wide result's legitimate
            // `i64::MIN` bits, told apart from a throw on the cold side.
            let mut wide_keep: Vec<usize> = Vec::new();
            if let Some((monitor_slot, call)) = sync_exit {
                // Cold: `RAX == i64::MIN`. For an int-category, reference or
                // void result that is always the callee's exception sentinel.
                // A `long`/`float`/`double` result can carry those bits for
                // real (`Long.MIN_VALUE`, `-0.0`), so peek the out-of-band
                // signal first, exactly as `emit_call_return_sentinel_tail`
                // does, and take the normal edge (whose `Op::MonitorExit`
                // releases) when nothing is pending. The callee body can
                // neither deopt nor call (the VM lookup's promise), so a
                // pending signal here is its own exception. The peek reads two
                // flags and reaches no safepoint.
                let wide = matches!(ty, IrType::Long | IrType::Double | IrType::Float);
                if wide {
                    wide_keep.push(self.emit_sync_exit_dispatch_threw_peek());
                }
                // Shape 2 (c) of the i41 page
                // (`crate::caller_held_callee_resume_enabled`): the callee may
                // have LEFT -- a poll exit the VM granted this caller-held
                // activation. Ask the callee-deopt service before anything is
                // released: it resumes the callee holding nothing of this
                // frame's monitor, and a value takes the normal edge, whose
                // `Op::MonitorExit` is the one release. A sentinel answer (the
                // callee, or its resumed frame, threw) falls through to the
                // exceptional release.
                // No service wired (a unit test's zeroed helpers): the
                // historical lowering, as the other cold tails do.
                if crate::caller_held_callee_resume_enabled() && self.service_callee_deopt != 0 {
                    self.emit_callee_deopt_service_cold(
                        info_ptr, num_args, inputs, /* after_reload */ true,
                        map,
                    );
                    self.emit_cmp_rax_sentinel();
                    wide_keep.push(self.emit_jcc_rel32(0x81)); // JNO .keep: a value
                    if wide {
                        wide_keep.push(self.emit_sync_exit_dispatch_threw_peek());
                    }
                }
                // The callee threw. Release the monitor this frame took for
                // it, then take the exception route. The normal edge is
                // released by the `Op::MonitorExit` after this node.
                self.emit_sync_exit_then_throw(monitor_slot, call);
            } else {
                self.service_lean_bit_pending = self.direct_call_lean_site;
                self.emit_inline_callee_deopt_service(
                    info_ptr, num_args, inputs, /* after_reload */ true,
                    map,
                );
                self.service_lean_bit_pending = false;
                self.emit_call_return_sentinel_tail(ty);
            }
            for p in wide_keep {
                self.patch_rel32_to_here(p);
            }
            self.patch_rel32_to_here(keep);
            self.store_rax(slot);
        } else if sync_exit.is_some() {
            // `lib.rs` plans a caller-held synchronized CALL only on the merged
            // route; the split route has no place for the exceptional release.
            self.latch_bailout(Bailout::new(BailoutReason::Internal(
                "ir_lower: a caller-held synchronized CALL needs the merged sentinel route",
            )));
        } else {
            self.service_lean_bit_pending = self.direct_call_lean_site;
            self.emit_inline_callee_deopt_service(
                info_ptr, num_args, inputs, /* after_reload */ false,
                map,
            );
            self.service_lean_bit_pending = false;
            // Not `emit_call_return_check`: that method's own
            // `note_inline_frame_return_site()` call would record a SECOND,
            // wrong-offset row here (after the deopt-service guard above) --
            // the row for this call was already recorded above, immediately
            // after the `CALL`.
            self.emit_call_return_check_after_note(slot, ty);
        }
    }

    /// The `dispatch_threw` peek of a caller-held synchronized CALL's cold side
    /// for a `long` / `float` / `double` result (round 11 wave 12, lane
    /// `calls`): entered with `RAX == i64::MIN`, it asks whether a throwable
    /// is pending, puts the sentinel bits back in RAX (`MOV` leaves ZF alone)
    /// and returns the `JE .keep` patch taken when none is -- the bits are the
    /// genuine result. The peek reads two flags and reaches no safepoint.
    pub(in crate::ir_lower) fn emit_sync_exit_dispatch_threw_peek(&mut self) -> usize {
        self.emit_mov_reg_imm64(RAX, self.dispatch_threw as u64);
        self.buf.emit(&[0xFF, 0xD0]); // CALL RAX (RAX = 0/1)
        self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
        self.emit_mov_reg_imm64(RAX, i64::MIN as u64);
        self.emit_jcc_rel32(0x84) // JE .keep
    }

    /// The exceptional edge of a caller-held synchronized direct CALL (round
    /// 11 wave 10, lane `synccall`): entered with `RAX == i64::MIN` after the
    /// post-call republish and shadow reload, so `monitor_slot` already holds
    /// the post-collection address of the monitor. Releases it exactly as the
    /// `Op::MonitorExit` arm does (inline thin unlock, else the helper with its
    /// own map), then takes the exception route with the sentinel in RAX.
    ///
    /// The helper's own result is ignored: `jit_monitor_exit` fails only for a
    /// monitor this thread does not hold, which the enter before the CALL
    /// rules out, and it does not clear the callee's pending exception.
    ///
    /// `call` is the call node. Its result slot is already allocated (and so
    /// marked defined) but was never written on this edge: for a REFERENCE
    /// result the helper's map would otherwise publish whatever an earlier
    /// execution left in that word as a live root. It is hidden for the map,
    /// the way the `Op::CheckCast` slow path hides its own (round 11 wave 12,
    /// lane `calls`; that is what kept reference results off this route).
    pub(in crate::ir_lower) fn emit_sync_exit_then_throw(&mut self, monitor_slot: i32, call: NodeId) {
        let thin_unlock_done = self.emit_monitor_fast_path(false, monitor_slot);
        let sp_live_hi = self.spill_high_water;
        let hidden = self
            .defined_nodes
            .get_mut(call as usize)
            .map(|defined| std::mem::replace(defined, false));
        let mut map = self.emit_safepoint_map(sp_live_hi);
        if let (Some(was), Some(defined)) = (hidden, self.defined_nodes.get_mut(call as usize)) {
            *defined = was;
        }
        let return_address = crate::runtime_lowering::emit_monitor_stub(
            &mut self.buf,
            self.context_slot_off,
            monitor_slot,
            self.monitor_exit,
            self.frame_record,
        );
        self.anchor_safepoint_return(map.as_mut(), return_address);
        self.note_inline_frame_return_site_at(return_address);
        self.emit_identity_after_shared_stub();
        self.emit_shadow_reload();
        // A thin unlock bypasses the release helper's publication. Give
        // the two IDs different lookup returns when this call has a local pad.
        let release_map = map.filter(|_| {
            self.cur_call_throw.is_some() && self.call_throw_publication.is_some()
        });
        let mut incoming = release_map.and_then(|map| {
            self.call_throw_publication
                .replace(CallThrowPublication { map: Some(map) })
        });
        if let Some(done) = thin_unlock_done {
            if let Some(publication) = incoming.take() {
                self.emit_sync_throw_edge();
                self.call_throw_publication = Some(publication);
            }
            self.patch_rel32_to_here(done);
        }
        self.emit_sync_throw_edge();
        if let Some(publication) = incoming {
            self.call_throw_publication = Some(publication);
        }
    }

    pub(in crate::ir_lower) fn emit_sync_throw_edge(&mut self) {
        self.emit_mov_reg_imm64(RAX, i64::MIN as u64);
        let exception_patch = self.emit_jmp_rel32();
        self.push_call_exc_patch(exception_patch);
    }

    /// The class monitor a caller-held synchronized direct CALL at `call`
    /// holds, or why the graph no longer has the shape that makes the CALL
    /// sound (round 11 wave 10, lane `synccall`; page
    /// `r11w9-sync-caller-held-monitor-direct-call-patch`).
    ///
    /// The builder emits `m = ConstClass; MonitorEnter m; Call; MonitorExit m`
    /// and never names `m` in a frame state. That is sound only while nothing
    /// between the enter and the exit can transfer to the interpreter: a deopt
    /// there would rebuild a frame that does not know it holds `m`, and the
    /// lock would leak. The callee half is the VM lookup's promise (a body with
    /// no trap and no call); this checks the caller half on the FINAL
    /// schedule, which is the emission order: the three nodes in one block,
    /// enter before the call before the exit, and every node emitted between
    /// them one that `op_cannot_deopt` admits. The exit must also name the
    /// call as its memory input and the same object, so the normal edge
    /// releases exactly what the enter took.
    ///
    /// Round 11 wave 11 (lane `calls`): an INSTANCE callee (`invoke_kind == 1`)
    /// holds its RECEIVER, argument 0, instead of a `ConstClass`
    /// (`IrBuilder::enter_receiver_monitor`). Two instance pairs on one
    /// receiver node with nothing between them (`o.a(); o.b();`) are merged by
    /// lock coarsening (`ir_optimize::coarsen_adjacent_monitors`) into
    /// `MonitorEnter o; Call a; Call b; MonitorExit o`, so the enter is found by
    /// walking back over earlier caller-held calls on the same receiver, and the
    /// exit by walking forward over later ones. That merged window is as sound
    /// as one pair: each call releases `o` on its own exceptional edge and the
    /// hold count is one throughout; coarsening admitted nothing between them,
    /// and every node emitted in the window is still checked below.
    pub(in crate::ir_lower) fn sync_direct_call_monitor(&self, call: NodeId) -> Result<NodeId, &'static str> {
        // Bound on the coarsened chain walk; a longer run keeps the refusal.
        const MAX_CHAINED_CALLS: usize = 64;
        let graph = self.graph;
        let call_node = graph.node_opt(call).ok_or("the call node is missing")?;
        // Round 11 wave 12 (lane `calls`): every result kind. A reference
        // result is kept out of the exceptional edge's map by
        // `emit_sync_exit_then_throw`; a wide one needs the `dispatch_threw`
        // peek `emit_direct_cross_call` makes before it releases anything.
        match call_node.ty {
            IrType::Int | IrType::Void | IrType::Ref => {}
            IrType::Long | IrType::Double | IrType::Float => {
                if self.dispatch_threw == 0 {
                    return Err("a wide result needs the dispatch_threw peek, which is unwired");
                }
            }
            _ => return Err("the call's result kind has no caller-held route"),
        }
        let Op::Call { info_ptr } = &call_node.op else {
            return Err("the node is not a call");
        };
        // SAFETY: as the `invoke_kind == 4` read in the `Op::Call` arm -- the
        // pointer names a live `JitInvokeInfo` owned by `ir_call_infos` for the
        // lifetime of this compile.
        let instance = match unsafe { (*(*info_ptr as *const JitInvokeInfo)).invoke_kind } {
            3 => false,
            1 => true,
            _ => return Err("the call is neither static nor statically bound"),
        };
        let mut enter = call_node
            .input_opt(1)
            .ok_or("the call has no memory input")?;
        let mut hops = 0usize;
        loop {
            let n = graph.node_opt(enter).ok_or("the enter node is missing")?;
            if n.op == Op::MonitorEnter {
                break;
            }
            if instance && hops < MAX_CHAINED_CALLS && self.is_sync_direct_call(enter) {
                enter = n.input_opt(1).ok_or("a chained call has no memory input")?;
                hops += 1;
                continue;
            }
            return Err("the call's memory input is not a monitor enter");
        }
        let enter_node = graph.node_opt(enter).ok_or("the enter node is missing")?;
        if enter_node.inputs.len() != 3 {
            return Err("the monitor enter has the wrong shape");
        }
        let monitor = enter_node
            .input_opt(2)
            .ok_or("the monitor enter names no object")?;
        if instance {
            if call_node.input_opt(2) != Some(monitor) {
                return Err("the monitor is not the call's receiver");
            }
            if !graph.node_opt(monitor).is_some_and(|m| m.ty == IrType::Ref) {
                return Err("the receiver monitor is not a reference");
            }
        } else if !graph
            .node_opt(monitor)
            .is_some_and(|m| matches!(m.op, Op::ConstClass { .. }))
        {
            return Err("the monitor is not a class mirror");
        }
        let block_of = |id: NodeId| self.schedule.node_to_block.get(id as usize).copied();
        let block = block_of(call).ok_or("the call is not scheduled")?;
        if block_of(enter) != Some(block) {
            return Err("the monitor enter is not in the call's block");
        }
        let nodes = &self
            .schedule
            .blocks
            .get(block)
            .ok_or("the call's block is missing")?
            .nodes;
        let enter_at = nodes
            .iter()
            .position(|&n| n == enter)
            .ok_or("the monitor enter is not emitted")?;
        let call_at = nodes
            .iter()
            .position(|&n| n == call)
            .ok_or("the call is not emitted")?;
        if enter_at >= call_at {
            return Err("the monitor enter is not emitted before the call");
        }
        // The memory chain from the enter: this call, and for an instance
        // monitor any caller-held call on the same receiver a coarsening joined
        // to it, each consuming the previous one's token, then the exit.
        let mut tail = enter;
        let mut seen_call = false;
        for &n in nodes.iter().skip(enter_at + 1) {
            let Some(node) = graph.node_opt(n) else {
                return Err("a scheduled node is missing");
            };
            if matches!(node.op, Op::Call { .. })
                && node.input_opt(1) == Some(tail)
                && (n == call
                    || (instance
                        && self.is_sync_direct_call(n)
                        && node.input_opt(2) == Some(monitor)))
            {
                tail = n;
                seen_call |= n == call;
                continue;
            }
            if n == call {
                return Err("the call does not follow the monitor enter on the memory chain");
            }
            if node.op == Op::MonitorExit
                && node.input_opt(1) == Some(tail)
                && node.input_opt(2) == Some(monitor)
            {
                return if seen_call {
                    Ok(monitor)
                } else {
                    Err("the monitor exit is emitted before the call")
                };
            }
            if !op_cannot_deopt(&node.op) {
                return Err("a node that can deopt is emitted while the monitor is held");
            }
        }
        Err("no monitor exit after the call in its block")
    }

    /// Is `n` an `Op::Call` served by a caller-held synchronized direct CALL
    /// row (`compile_request::sync_direct_row_key`)? Round 11 wave 11.
    pub(in crate::ir_lower) fn is_sync_direct_call(&self, n: NodeId) -> bool {
        self.graph.node_opt(n).is_some_and(|node| {
            matches!(node.op, Op::Call { .. })
                && node.bytecode_pc.is_some_and(|pc| {
                    self.direct_calls
                        .contains_key(&crate::compile_request::sync_direct_row_key(pc))
                })
        })
    }

    /// Round 14 wave 2 (lane syncsplice, SR-1): may monitor op `id` be emitted?
    /// `Ok` for every op that is not half of a synchronized splice
    /// (`Graph::sync_splice_windows`); for one that is, `Ok` only when every
    /// live enter pairs with a live exit and the FINAL schedule puts nothing
    /// that can trap between them.
    ///
    /// No frame state names a synchronized splice's monitor, so a deopt inside
    /// the window would resume a frame that does not know it holds the lock
    /// (and re-execute the `invoke`, which locks again), and an exception
    /// would leave with it held. The builder checked the nodes the body built;
    /// this checks what the optimizer and the scheduler made of them, the way
    /// [`Self::sync_direct_call_monitor`] checks the caller-held CALL's window:
    ///
    /// * both halves live and naming one object (after a lock coarsening, the
    ///   enter's partner is the exit of the window whose enter it deleted);
    /// * the enter's block dominates the exit's, and every path from the enter
    ///   reaches the exit without leaving the window (no return, throw or back
    ///   edge inside it, and no block of it entered from outside);
    /// * every node scheduled in the window, and every block terminator in it,
    ///   passes [`crate::ir::sync_splice_window_node_admitted`].
    pub(in crate::ir_lower) fn sync_splice_window_refusal(&self, id: NodeId) -> Result<(), &'static str> {
        let graph = self.graph;
        if !graph
            .sync_splice_windows
            .iter()
            .any(|w| w.enter == id || w.exit == id)
        {
            return Ok(());
        }
        let live = |n: NodeId| graph.node_opt(n).is_some_and(|x| x.op != Op::Dead);
        let object = |n: NodeId| graph.node_opt(n).and_then(|x| x.input_opt(2));
        // Pair every live enter with a live exit. Its own, normally; lock
        // coarsening (`ir_optimize::coarsen_adjacent_monitors`) deletes an
        // exit together with the NEXT enter on the same object
        // (`o.a(); o.b();`), and the merged window then ends at that enter's
        // window's exit -- a candidate only while its own enter is dead, and
        // only once. Each pairing is judged by the region check below; an
        // enter with no pairing, or an exit no enter claimed, is refused.
        let windows = &graph.sync_splice_windows;
        let mut claimed: Vec<NodeId> = Vec::with_capacity(windows.len());
        let mut verdict: Result<(), &'static str> =
            Err("a synchronized splice's monitor op has no partner");
        for w in windows.iter().filter(|w| live(w.enter)) {
            let monitor = object(w.enter).ok_or("the monitor enter names no object")?;
            let own = std::iter::once(w.exit).filter(|&x| live(x));
            let merged = windows
                .iter()
                .filter(|o| !live(o.enter) && live(o.exit))
                .map(|o| o.exit);
            let mut paired = None;
            let mut why = "one half of the monitor pair was removed";
            for exit in own.chain(merged) {
                if claimed.contains(&exit) || object(exit) != Some(monitor) {
                    continue;
                }
                let window = crate::ir::SyncSpliceWindow {
                    enter: w.enter,
                    exit,
                    monitor,
                    static_class_id: w.static_class_id,
                };
                match self.sync_splice_region_refusal(&window) {
                    Ok(()) => {
                        paired = Some(exit);
                        break;
                    }
                    Err(e) => why = e,
                }
            }
            let Some(exit) = paired else {
                return Err(why);
            };
            claimed.push(exit);
            if w.enter == id || exit == id {
                verdict = Ok(());
            }
        }
        verdict
    }

    /// The region half of [`Self::sync_splice_window_refusal`] for one
    /// enter/exit pairing (`window.monitor` is the object both name now: a
    /// GVN may have renamed the one the builder recorded).
    pub(in crate::ir_lower) fn sync_splice_region_refusal(
        &self,
        window: &crate::ir::SyncSpliceWindow,
    ) -> Result<(), &'static str> {
        let graph = self.graph;
        let schedule = self.schedule;
        let window = *window;
        let admitted = |n: NodeId| {
            graph
                .node_opt(n)
                .is_some_and(|x| crate::ir::sync_splice_window_node_admitted(graph, x, &window))
        };
        let blocks = &schedule.blocks;
        let block_of = |n: NodeId| schedule.node_to_block.get(n as usize).copied();
        let be = block_of(window.enter).ok_or("the monitor enter is not scheduled")?;
        let bx = block_of(window.exit).ok_or("the monitor exit is not scheduled")?;
        let be_nodes = blocks
            .get(be)
            .map(|b| b.nodes.as_slice())
            .ok_or("the enter's block is missing")?;
        let bx_nodes = blocks
            .get(bx)
            .map(|b| b.nodes.as_slice())
            .ok_or("the exit's block is missing")?;
        let enter_at = be_nodes
            .iter()
            .position(|&n| n == window.enter)
            .ok_or("the monitor enter is not emitted")?;
        let exit_at = bx_nodes
            .iter()
            .position(|&n| n == window.exit)
            .ok_or("the monitor exit is not emitted")?;
        const TRAPS: &str = "a node that can trap is scheduled inside the monitor window";
        if be == bx {
            if exit_at <= enter_at {
                return Err("the monitor exit is emitted before the enter");
            }
            let between = be_nodes.get(enter_at + 1..exit_at).unwrap_or(&[]);
            return if between.iter().all(|&n| admitted(n)) {
                Ok(())
            } else {
                Err(TRAPS)
            };
        }
        if !schedule.dom.dominates(be, bx) {
            return Err("the monitor enter does not dominate its exit");
        }
        let after_enter = be_nodes.get(enter_at + 1..).unwrap_or(&[]);
        let before_exit = bx_nodes.get(..exit_at).unwrap_or(&[]);
        if !after_enter.iter().all(|&n| admitted(n)) || !before_exit.iter().all(|&n| admitted(n)) {
            return Err(TRAPS);
        }
        // A terminator that is not a plain branch or join (a return, a throw)
        // would leave the window with the monitor held.
        let terminator_ok = |b: usize| {
            blocks
                .get(b)
                .and_then(|blk| blk.terminator)
                .is_none_or(|t| graph.node_opt(t).is_some_and(|x| op_cannot_deopt(&x.op)))
        };
        if !terminator_ok(be) {
            return Err(TRAPS);
        }
        // The blocks strictly inside the window: reachable from the enter's
        // block without passing its exit's.
        let mut inside = vec![false; blocks.len()];
        let mut work: Vec<usize> = blocks
            .get(be)
            .map(|b| b.successors.clone())
            .unwrap_or_default();
        while let Some(b) = work.pop() {
            if b == bx {
                continue;
            }
            if b == be {
                return Err("the monitor window has a back edge to its enter");
            }
            let seen = inside.get_mut(b).ok_or("a window block is missing")?;
            if *seen {
                continue;
            }
            *seen = true;
            let blk = blocks.get(b).ok_or("a window block is missing")?;
            if blk.successors.is_empty() {
                return Err("a path leaves the monitor window without its exit");
            }
            if !blk.nodes.iter().all(|&n| admitted(n)) || !terminator_ok(b) {
                return Err(TRAPS);
            }
            work.extend(blk.successors.iter().copied());
        }
        // Single entry, no cycle: every block of the window (and the exit's)
        // is entered only from the enter's block or from inside, and no edge
        // inside it goes back to a block that dominates its source.
        for (b, &is_inside) in inside.iter().enumerate() {
            if !is_inside && b != bx {
                continue;
            }
            let blk = blocks.get(b).ok_or("a window block is missing")?;
            if !blk
                .predecessors
                .iter()
                .all(|&p| p == be || inside.get(p).copied().unwrap_or(false))
            {
                return Err("a monitor window block is entered from outside the window");
            }
            let back_edge =
                |s: usize| inside.get(s).copied().unwrap_or(false) && schedule.dom.dominates(s, b);
            if is_inside && blk.successors.iter().any(|&s| back_edge(s)) {
                return Err("the monitor window contains a loop");
            }
        }
        Ok(())
    }

    /// Round 15 wave 1 (lane chain, hand-back;
    /// `r15w1-orch-chain-arm-eager-chain-deopt-in-sync-splice-wrong-answer-FIXED-20260930.md`):
    /// is the node being lowered ([`Self::cur_def`]) inside a synchronized
    /// splice's monitor window, so that a deopt planted for it must publish
    /// the flat frame and not a caller chain?
    ///
    /// No frame state names a window's monitor (see `SyncSpliceWindow`), so a
    /// chain resumed from inside one rebuilds the synchronized callee's frame
    /// without the hold: the interpreter runs the rest of the body with no
    /// `monitor_on_exit`, the compiled `MonitorExit` never runs, and the
    /// thread keeps a hold nothing releases. The flat frame is what every such
    /// site had before the chain arm (and has in the default arm), and it is
    /// sound for the same reason SR-1 is: every node in a window passes
    /// [`crate::ir::sync_splice_window_node_admitted`] (checked again, on the
    /// final schedule, by [`Self::sync_splice_window_refusal`]), so the only
    /// deopts planted there are the null-receiver exits of reads and writes
    /// through the object the window just locked, which cannot fire. The
    /// chain arm then has no chain point there for `CRATONVM_DEOPT_EAGER_CHAINS`
    /// to force, and a chain-relaxed fence (`Graph::splice_chains_required`)
    /// has nothing to require of an exit that never runs.
    ///
    /// Windows are found on the schedule (after lock coarsening merged any), so
    /// SS-3 in-region splices, SS8-1 a's nested same-receiver bodies (judged by
    /// the enclosing window) and coarsened `o.a(); o.b();` windows are all
    /// covered. Kill switch `CRATONVM_JIT_IR_SYNC_WINDOW_NO_CHAIN_POINTS=0`.
    pub(in crate::ir_lower) fn sync_window_site_publishes_no_chain(&self) -> bool {
        let Some(id) = self.cur_def else {
            return false;
        };
        if self.graph.sync_splice_windows.is_empty() || !ir_sync_window_no_chain_points_enabled() {
            return false;
        }
        self.sync_window_nodes
            .get_or_init(|| self.sync_window_region_nodes())
            .contains(&id)
    }

    /// Every node scheduled strictly inside a synchronized splice's monitor
    /// window, for the pairings [`Self::sync_splice_window_refusal`] makes
    /// (its own exit, or after a coarsening the exit of the window whose enter
    /// it deleted) and whose region check passes. A window that fails the
    /// check contributes nothing (the compile is refused at its enter anyway).
    pub(in crate::ir_lower) fn sync_window_region_nodes(&self) -> std::collections::HashSet<NodeId> {
        let graph = self.graph;
        let mut out = std::collections::HashSet::new();
        let live = |n: NodeId| graph.node_opt(n).is_some_and(|x| x.op != Op::Dead);
        let object = |n: NodeId| graph.node_opt(n).and_then(|x| x.input_opt(2));
        let windows = &graph.sync_splice_windows;
        let mut claimed: Vec<NodeId> = Vec::with_capacity(windows.len());
        for w in windows.iter().filter(|w| live(w.enter)) {
            let Some(monitor) = object(w.enter) else {
                continue;
            };
            let own = std::iter::once(w.exit).filter(|&x| live(x));
            let merged = windows
                .iter()
                .filter(|o| !live(o.enter) && live(o.exit))
                .map(|o| o.exit);
            for exit in own.chain(merged) {
                if claimed.contains(&exit) || object(exit) != Some(monitor) {
                    continue;
                }
                let window = crate::ir::SyncSpliceWindow {
                    enter: w.enter,
                    exit,
                    monitor,
                    static_class_id: w.static_class_id,
                };
                if self.sync_splice_region_refusal(&window).is_ok() {
                    if let Some(nodes) = self.sync_splice_region_nodes(&window) {
                        out.extend(nodes);
                    }
                    claimed.push(exit);
                    break;
                }
            }
        }
        out
    }

    /// The nodes (and block terminators) scheduled between `window.enter` and
    /// `window.exit`: after the enter in its block, before the exit in its
    /// block, and every block reachable from the enter's without passing the
    /// exit's -- the region [`Self::sync_splice_region_refusal`] checks.
    pub(in crate::ir_lower) fn sync_splice_region_nodes(
        &self,
        window: &crate::ir::SyncSpliceWindow,
    ) -> Option<Vec<NodeId>> {
        let schedule = self.schedule;
        let blocks = &schedule.blocks;
        let block_of = |n: NodeId| schedule.node_to_block.get(n as usize).copied();
        let be = block_of(window.enter)?;
        let bx = block_of(window.exit)?;
        let be_nodes = blocks.get(be)?.nodes.as_slice();
        let bx_nodes = blocks.get(bx)?.nodes.as_slice();
        let enter_at = be_nodes.iter().position(|&n| n == window.enter)?;
        let exit_at = bx_nodes.iter().position(|&n| n == window.exit)?;
        let mut out: Vec<NodeId> = Vec::new();
        if be == bx {
            out.extend_from_slice(be_nodes.get(enter_at + 1..exit_at)?);
            return Some(out);
        }
        out.extend_from_slice(be_nodes.get(enter_at + 1..)?);
        out.extend(blocks.get(be)?.terminator);
        out.extend_from_slice(bx_nodes.get(..exit_at)?);
        let mut seen = vec![false; blocks.len()];
        let mut work: Vec<usize> = blocks.get(be)?.successors.clone();
        while let Some(b) = work.pop() {
            if b == bx || b == be {
                continue;
            }
            let s = seen.get_mut(b)?;
            if *s {
                continue;
            }
            *s = true;
            let blk = blocks.get(b)?;
            out.extend_from_slice(&blk.nodes);
            out.extend(blk.terminator);
            work.extend(blk.successors.iter().copied());
        }
        Some(out)
    }

    /// `SUB RSP, imm32` — reserve a call's outgoing stack-argument block.
    pub(in crate::ir_lower) fn emit_sub_rsp_imm32(&mut self, bytes: i32) {
        self.buf.emit(&[0x48, 0x81, 0xEC]);
        self.buf.emit(&bytes.to_le_bytes());
    }

    /// `ADD RSP, imm32` — release the block reserved by [`Self::emit_sub_rsp_imm32`].
    pub(in crate::ir_lower) fn emit_add_rsp_imm32(&mut self, bytes: i32) {
        self.buf.emit(&[0x48, 0x81, 0xC4]);
        self.buf.emit(&bytes.to_le_bytes());
    }

    /// `MOV qword [RSP + disp32], RAX` — place one outgoing stack argument.
    ///
    /// RSP-relative addressing needs a SIB byte (`ModRM.rm = 100`); `0x24` is
    /// `base = RSP, index = none`.
    pub(in crate::ir_lower) fn emit_mov_rsp_disp_from_rax(&mut self, disp: i32) {
        self.buf.emit(&[0x48, 0x89, 0x84, 0x24]);
        self.buf.emit(&disp.to_le_bytes());
    }

    /// Service an exceptional return from an inline cached compiled callee.
    ///
    /// The service reads the outgoing Java arguments as a contiguous array, so
    /// they have to be materialised into the staging region — but **here**, on
    /// the sentinel-taken side of the branch, not at the call site.
    ///
    /// They used to be staged unconditionally before every call, which cost N
    /// loads and N stores on a path that then loaded the same N frame slots
    /// again to marshal them into ABI registers: 3N memory operations per call
    /// for a cold path's convenience. Staging here is sound because the
    /// arguments live in the lowerer's own frame slots and nothing between the
    /// marshal and this test writes them — the callee cannot, it owns a
    /// different frame, and the marshal only reads.
    ///
    /// `inputs` is the call node's input list; arguments start at index 2, the
    /// same convention every caller in this file uses.
    ///
    /// # Reload before staging (round 9 wave 2)
    ///
    /// The service rebuilds the callee's interpreter frame from the staged
    /// words, so they must be POST-call values: a reference argument a moving
    /// collection relocated while the callee ran is only correct in its home
    /// word once the site's shadow publication has been copied back. The
    /// inline-cache arms and the non-merged direct route reach here BEFORE the
    /// site's reload (which runs at the shared `.done`, via
    /// `emit_call_return_check`), and used to stage the pre-GC homes. So the
    /// cold side now, when `after_reload` is false:
    ///
    /// 1. republishes this frame's identity (`emit_post_call_frame_record`) —
    ///    the service runs Java and can collect, and the non-merged direct
    ///    route has not republished yet;
    /// 2. copies the pending shadow words back into their homes WITHOUT
    ///    retracting `top` (`runtime_lowering::emit_shadow_copy_back`), so they
    ///    stay roots through the service call and the site's real reload at
    ///    `.done` copies the newest values once more and retracts;
    /// 3. stages every argument from those homes — ALWAYS, including under the
    ///    eager `CRATONVM_JIT_IR_COLD_ARG_STAGE=0` shape, whose pre-call staging
    ///    is exactly the pre-GC copy this fixes.
    ///
    /// `after_reload` is true on the merged direct route, which has already
    /// republished and reloaded before its fast sentinel skip.
    pub(in crate::ir_lower) fn emit_inline_callee_deopt_service(
        &mut self,
        info_ptr: usize,
        num_args: usize,
        inputs: &[NodeId],
        after_reload: bool,
        map: &mut Option<SafepointMapToken>,
    ) {
        if self.service_callee_deopt == 0 {
            return;
        }
        self.emit_cmp_rax_sentinel();
        self.buf.emit(&[0x0F, 0x81]); // JNO .done
        let skip = self.buf.pos();
        self.buf.emit(&[0, 0, 0, 0]);
        self.emit_callee_deopt_service_cold(info_ptr, num_args, inputs, after_reload, map);
        let rel = self.buf.pos() as i32 - (skip as i32 + 4);
        Self::patch_or_bail(&mut self.buf, skip, rel);
    }

    /// The cold side of [`Self::emit_inline_callee_deopt_service`]: entered with
    /// the sentinel in RAX, leaves with the service's answer in RAX (a value, or
    /// the sentinel again). Emits nothing when no service is wired.
    pub(in crate::ir_lower) fn emit_callee_deopt_service_cold(
        &mut self,
        info_ptr: usize,
        num_args: usize,
        inputs: &[NodeId],
        after_reload: bool,
        map: &mut Option<SafepointMapToken>,
    ) {
        if self.service_callee_deopt == 0 {
            return;
        }
        // ── cold from here ──────────────────────────────────────────────
        // RAX holds the sentinel, so it is free as the transfer scratch.
        if !after_reload {
            self.emit_post_call_frame_record();
            self.emit_pending_shadow_copy_back();
        }
        // Staged here under the eager shape too: after a copy-back the
        // pre-call staging block may hold pre-relocation references.
        //
        // Round 11 (irlower): UNCONDITIONALLY, as the doc above says. This was
        // `true || !after_reload`, which on the merged
        // direct route (`after_reload`) under the eager kill switch
        // (`CRATONVM_JIT_IR_COLD_ARG_STAGE=0`) skipped the re-stage and handed
        // the service the block staged BEFORE the call -- pre-relocation
        // references, when the homes it would have re-read had already been
        // reloaded. Cold path only; the eager pre-call staging is unchanged.
        //
        // JIT round 21, lane argwin (OOM21-1): not at an owned direct call,
        // whose arguments belong to the callee -- the service is handed the
        // owned word and no buffer, and reads no argument (a window may have
        // zeroed their homes).
        let owned = self.direct_call_args_owned;
        if !owned {
            for i in 0..num_args {
                let arg = inputs[2 + i];
                self.gp_load_abi_value(RAX, arg);
                // Cast: an argument index is bounded by the callee's parameter
                // count, so `i * 8` cannot overflow an x86-64 displacement.
                self.store_rax(self.args_stage_top_off - (i as i32) * 8);
            }
        }
        self.load_context(CALL_ARG_REGS[0]);
        self.emit_mov_reg_imm64(CALL_ARG_REGS[1], info_ptr as u64);
        let lean = std::mem::take(&mut self.service_lean_bit_pending);
        if owned {
            self.emit_mov_reg_imm64(CALL_ARG_REGS[2], 0);
            // Cast: the service word's bits.
            self.emit_mov_reg_imm64(
                CALL_ARG_REGS[3],
                crate::service_args_word(None, lean) as u64,
            );
        } else {
            self.lea_reg_from_frame(CALL_ARG_REGS[2], self.args_stage_top_off);
            let lean_bit = if lean {
                crate::SERVICE_ARGS_LEAN_PROPAGATE_BIT
            } else {
                0
            };
            self.emit_mov_reg_imm64(CALL_ARG_REGS[3], num_args as u64 | lean_bit);
        }
        self.emit_mov_reg_imm64(RAX, self.service_callee_deopt as u64);
        self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
        self.anchor_safepoint_return(map.as_mut(), self.buf.pos());
        if !after_reload {
            // The service ran the interpreter, which may have entered compiled
            // code that published ITS identity. Round 11 wave 2 (lane lower):
            // an inline-cache hit now joins its site's `.done` AFTER the
            // republish there (see `emit_inline_cache_call`), so this cold
            // side restores the identity itself. RAX (the service's answer) is
            // preserved by both forms of the republish.
            self.emit_post_call_frame_record();
        }
    }

    /// The exit of one inline-cache hit (the MIC arm or a PIC rung), right
    /// after [`Self::emit_call_loaded_ic_entry`].
    ///
    /// Round 11 wave 3 (lane lower,
    /// `r11-irlower-ic-hit-republishes-and-tests-the-sentinel-twice`): the hot
    /// hit used to test the sentinel twice -- once here to skip the callee-deopt
    /// service, again in the shared tail. Now the one compare's not-sentinel edge
    /// goes to `.hit_join` (the shadow reload and the result store), which comes
    /// AFTER the site's sentinel tail. Only the cold side, whose service can
    /// answer the sentinel again, goes to `.tail`. With no service wired, the
    /// hit goes to `.tail`, the only compare it then has.
    ///
    /// Under the republish kill switch the hit has not republished, so every
    /// edge that would skip `.done`'s republish goes to `.done` instead. The
    /// cold side republishes for itself, so it may still take `.tail`.
    #[allow(clippy::too_many_arguments)]
    pub(in crate::ir_lower) fn emit_ic_hit_exit(
        &mut self,
        info_ptr: usize,
        num_args: usize,
        inputs: &[NodeId],
        hits_republished: bool,
        done_patches: &mut Vec<usize>,
        tail_patches: &mut Vec<usize>,
        hit_patches: &mut Vec<usize>,
        map: &mut Option<SafepointMapToken>,
    ) {
        if self.service_callee_deopt == 0 {
            let to_tail = self.emit_jmp_rel32();
            // W7-C: a sunk hit did not republish, and with no service there is
            // no cold side to do it, so it takes `.done`'s.
            if hits_republished && !self.tail_sink.cur {
                tail_patches.push(to_tail);
            } else {
                done_patches.push(to_tail);
            }
            return;
        }
        // Round 14 wave 3 (lane calls, CC3-1): the census the IC half is
        // gated on -- would the CC2-1 admission take this exit's cold side?
        self.ic_hit_exit_census.0 += 1;
        if self.mir_mode == MirMode::Off
            && self.body_publishes_no_roots()
            && self.pending_shadow_copy_back().is_none()
            && !self.bci_is_protected(self.resume_bci(self.cur_bci))
        {
            self.ic_hit_exit_census.1 += 1;
        }
        self.emit_cmp_rax_sentinel();
        let not_sentinel = self.emit_jcc_rel32(0x81); // JNO
        if hits_republished {
            hit_patches.push(not_sentinel);
        } else {
            done_patches.push(not_sentinel);
        }
        self.emit_callee_deopt_service_cold(info_ptr, num_args, inputs, false, map);
        tail_patches.push(self.emit_jmp_rel32());
    }

    // ── Inline caches (jit-inlining-and-ir-calls) ────────────────────

    /// Record one exceptional-exit branch for [`Self::emit_call_exc_stub`],
    /// tagged with the bytecode pc currently being lowered.
    ///
    /// Every site that jumps to the shared bail stub goes through here so no
    /// exit can reach the stub without a throw-site bci — the defect the stub's
    /// own doc comment describes.
    /// [`Self::push_call_exc_patch`] for a native-stack guard's trip in front
    /// of a call (the cycle-edge cell and the inline-cache stack check): never
    /// a call-site exception pad (round nio21 wave 3, lane L2). The trip has
    /// just raised `StackOverflowError` at the stack floor, and the pad's
    /// handler lookup can resolve a catch type -- class loading, deep native
    /// frames -- exactly where there is no stack left for it. Today's exit.
    pub(in crate::ir_lower) fn push_stack_guard_exc_patch(&mut self, patch: usize) {
        let publication = self.call_throw_publication.take();
        self.call_throw_stack_guard_exit = true;
        self.push_call_exc_patch(patch);
        self.call_throw_stack_guard_exit = false;
        self.call_throw_publication = publication;
    }

    pub(in crate::ir_lower) fn push_call_exc_patch(&mut self, patch: usize) {
        // Round nio21 wave 3 (lane L2): an armed call's exceptional edge goes
        // to its call-site exception pad, which asks this method's own
        // handlers first and falls back to exactly the exit decided below.
        if self.call_throw_publication.is_some() {
            if let Some(plan) = self.cur_call_throw.clone() {
                if self.push_call_throw_pad(patch, plan) {
                    return;
                }
            }
        }
        // nio21 wave 4 (lane L2): a call inside a spliced callee's `try` has
        // no correct "today's exit" for a throwable the callee catches (it
        // leaves through the caller's `invoke`, past the callee's `catch`).
        // An exit of such a call that does not reach its pad -- one taken
        // before the call's map is published (an inline prefix's helper), or
        // a `Ref` result with no home -- refuses the compile and names the
        // splice; a stack guard's `StackOverflowError` is the one exception
        // (no admitted catch type takes it).
        if !self.call_throw_stack_guard_exit
            && self
                .cur_call_throw
                .as_ref()
                .is_some_and(|plan| plan.spliced)
        {
            note_sync_window_refused_pc(self.cur_bci);
            self.latch_bailout(Bailout::with_context(
                BailoutReason::UnsupportedShape(
                    "ir_lower: a spliced call-site edge has an exit that misses its pad",
                ),
                format!("combined pc {}", self.cur_bci),
            ));
        }
        // JIT round 19 wave 5 (lane handler5, SPLICE4-1): an armed call's
        // exceptional exit asks the compiled handler first. Not while the
        // site's shadow publication is still pending (an inline-cache `.tail`
        // exit leaves before its reload): the arm would read homes a moving
        // collection left stale.
        if let Some(wire) = self.cur_catch_arm.clone() {
            match self.pending_shadow_copy_back() {
                None => {
                    self.push_catch_probe(patch, wire, None);
                    return;
                }
                // JIT round 19 wave 7 (lane handler7, HANDLER6-3): the probe's
                // hit edge reloads the publication itself, so the arm reads
                // refreshed homes and `top` is retracted exactly once.
                Some((_, _, offsets)) if crate::ir::ir_catch_arm_shadow_exits_enabled() => {
                    self.push_catch_probe(patch, wire, Some(offsets));
                    return;
                }
                Some(_) => self.note_catch_wire_refused(CatchWireRefusal::PendingShadow),
            }
        }
        self.push_call_exc_patch_route(patch);
    }

    /// HANDLER6-2 (lane handler7): count and name one exit of the armed call
    /// being lowered that keeps its old route (`cur_bci` is that call's pc).
    pub(in crate::ir_lower) fn note_catch_wire_refused(&self, clause: CatchWireRefusal) {
        crate::metrics::note_local_handler(clause.metric());
        if crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] ir-catch-wire refused {} call pc={}: {}",
                self.graph.scope_method_key,
                self.cur_bci,
                clause.as_str(),
            );
        }
    }

    /// [`Self::push_call_exc_patch`]'s route as it was before SPLICE4-1: a
    /// reason-9 rethrow pad inside a protected range, the shared exit outside.
    pub(in crate::ir_lower) fn push_call_exc_patch_route(&mut self, patch: usize) {
        // `cur_bci` is the node's own pc, which inside a spliced region is a
        // combined-buffer pc — see `resume_bci`.
        let bci = self.resume_bci(self.cur_bci);
        // This bci CAN transfer. See `transfer_bcis`.
        self.transfer_bcis.insert(bci);
        // Only a protected bci resumes through the table point (the rethrow pad below
        // builds the method's exceptional frame); an unprotected one is the shared
        // propagating exit, which stamps the throw bci and returns the sentinel and
        // reads no frame value (`emit_call_exc_stub`).
        let protected = self.bci_is_protected(bci);
        self.note_flat_transfer(bci, "call_exc_patch_route", protected);
        // Round 11 wave 10 (lane irexc): inside a protected range the exit
        // publishes the method's precise exceptional frame instead. Empty
        // ranges (every compile off the precise-frame route) never take it.
        if protected {
            self.push_rethrow_pad(patch, bci);
            return;
        }
        if !self.propagating_exit_holds_monitor && self.site_snapshot_may_hold_monitor(bci) {
            self.propagating_exit_holds_monitor = true;
        }
        self.call_exc_patches.push((patch, bci));
    }

    /// JIT round 19 wave 5 (lane handler5, SPLICE4-1): is the call `node_id`,
    /// at `offset` in `block_idx`, one whose exceptional exit may jump into
    /// the compiled handler arm the builder hung off it? `None` keeps the exit
    /// it always had, and the arm stays code no path enters.
    ///
    /// The probe jumps from the call's exceptional exit straight to the arm
    /// block, so the arm must see exactly the machine state the `If` that
    /// ends the call's block would hand it. Each clause is one way that could
    /// fail:
    ///
    /// * the block's terminator is the arm's `If(CatchEntryFlag)` for THIS
    ///   call (same `invoke` pc; an unrolled copy pairs with its own copy),
    ///   and nothing but the flag is scheduled after the call in the block:
    ///   a value computed there would be skipped by the jump;
    /// * the arm block's only predecessor is that block, and every node
    ///   scheduled in it before the caught throwable cannot call, trap or
    ///   reach a safepoint (`op_cannot_deopt`): the throwable sits in the
    ///   phi-copy scratch word, which no map names, until its read; a
    ///   throwable read scheduled in another block refuses;
    /// * every value keeps one location for its whole range: no split
    ///   residency or edge-resolution copy (`CRATONVM_JIT_IR_LS_SPLITS`), no
    ///   machine-level emission mode, no reference residency cache
    ///   (`CRATONVM_JIT_IR_REF_RESIDENCY`), whose block-entry model assumes the
    ///   emission order;
    /// * no self-locking body (its exits release a monitor this entry would
    ///   have to account for), and the helper, the VM pointer's frame word
    ///   and the scratch word are all wired.
    pub(in crate::ir_lower) fn catch_arm_wire(
        &self,
        block_idx: usize,
        offset: usize,
        node_id: NodeId,
    ) -> Option<CatchArmWire> {
        match self.catch_arm_wire_or_refusal(block_idx, offset, node_id) {
            Ok(wire) => Some(wire),
            Err(None) => None,
            Err(Some(clause)) => {
                // JIT round 19 wave 7 (lane handler7, HANDLER6-2): an armed
                // call whose probe is NOT wired names the refusing clause, so
                // a non-engagement is one counter line, not a reading.
                crate::metrics::note_local_handler(clause.metric());
                if crate::x64::dbg_jitc_enabled() {
                    let pc = self
                        .graph
                        .nodes
                        .get(node_id as usize)
                        .and_then(|n| n.bytecode_pc)
                        .map_or(-1, |pc| pc as i64);
                    eprintln!(
                        "[cratonvm-jitc] ir-catch-wire refused {} call pc={pc}: {}",
                        self.graph.scope_method_key,
                        clause.as_str(),
                    );
                }
                None
            }
        }
    }

    /// [`Self::catch_arm_wire`]'s decision with its reason (JIT round 19 wave
    /// 7, lane handler7, HANDLER6-2): `Err(None)` is a call with no arm (or
    /// the switch off), `Err(Some(clause))` an armed call whose probe a
    /// clause refused.
    pub(in crate::ir_lower) fn catch_arm_wire_or_refusal(
        &self,
        block_idx: usize,
        offset: usize,
        node_id: NodeId,
    ) -> Result<CatchArmWire, Option<CatchWireRefusal>> {
        if !self.catch_entry_on || self.graph.catch_arm_sites.is_empty() {
            return Err(None);
        }
        let node = self.graph.nodes.get(node_id as usize).ok_or(None)?;
        if !matches!(node.op, Op::Call { .. }) {
            return Err(None);
        }
        let pc = node.bytecode_pc.ok_or(None)?;
        // An armed call (some arm names its pc) that is not paired with its
        // flag below is a refusal, not an unarmed call: the scheduler left
        // the call outside the block its `If(CatchEntryFlag)` ends. A spliced
        // copy of a callee never has an arm (`note_catch_arm`), and its
        // combined pcs lie past the method's own, so no arm can name one.
        let armed = self.graph.catch_arm_sites.iter().any(|s| s.call_bci == pc);
        let unpaired = if armed {
            Err(Some(CatchWireRefusal::CallNotPaired))
        } else {
            Err(None)
        };
        let Some(block) = self.schedule.blocks.get(block_idx) else {
            return unpaired;
        };
        let Some(term) = block.terminator else {
            return unpaired;
        };
        let Some(term_node) = self.graph.nodes.get(term as usize) else {
            return unpaired;
        };
        if term_node.op != Op::If {
            return unpaired;
        }
        let Some(&cond) = term_node.inputs.get(1) else {
            return unpaired;
        };
        let Some(Op::CatchEntryFlag { call_bci, site }) =
            self.graph.nodes.get(cond as usize).map(|n| n.op.clone())
        else {
            return unpaired;
        };
        if call_bci as usize != pc || self.graph.catch_arm_sites.get(site as usize).is_none() {
            return unpaired;
        }
        if !block
            .nodes
            .get(offset + 1..)
            .is_some_and(|tail| tail.iter().all(|&n| n == cond))
        {
            return Err(Some(CatchWireRefusal::TailAfterCall));
        }
        if self.local_handler_lookup == 0
            || self.context_slot_off <= 0
            || self.phi_copy_scratch_slot_off <= 0
        {
            return Err(Some(CatchWireRefusal::Unwired));
        }
        if self.mir_mode != MirMode::Off
            || self.ref_residency_on
            || self.ls_managed.iter().any(|&m| m)
            || !self.ls_edge_copies.is_empty()
        {
            return Err(Some(CatchWireRefusal::Residency));
        }
        if self.self_lock.is_some() {
            return Err(Some(CatchWireRefusal::SelfLock));
        }
        let (arm_block, caught) = self
            .catch_arm_leg(term, block_idx)
            .ok_or(Some(CatchWireRefusal::ArmLeg))?;
        // HANDLER19-3: a ladder chains its further `If(CatchEntryFlag)`s on
        // the normal projections. A chained block holding only its flag emits
        // nothing the arm could miss, so the probe may jump past it straight
        // to that leg's arm. The first block that is not such a link ends the
        // ladder; `catch_site_box` then asks only the rows whose arms made it.
        let mut more: Vec<(u16, usize, bool)> = Vec::new();
        let (mut prev_block, mut prev_term) = (block_idx, term);
        loop {
            let Some(normal) = self.schedule.blocks.iter().position(|b| {
                self.graph
                    .nodes
                    .get(b.ctrl as usize)
                    .is_some_and(|n| n.op == Op::Proj(1) && n.inputs.first() == Some(&prev_term))
            }) else {
                break;
            };
            let link = &self.schedule.blocks[normal];
            if link.predecessors.as_slice() != [prev_block] {
                break;
            }
            let Some(t) = link.terminator else {
                break;
            };
            let Some(t_node) = self.graph.nodes.get(t as usize) else {
                break;
            };
            if t_node.op != Op::If {
                break;
            }
            let Some(&c) = t_node.inputs.get(1) else {
                break;
            };
            let Some(Op::CatchEntryFlag {
                call_bci: next_bci,
                site: next_site,
            }) = self.graph.nodes.get(c as usize).map(|n| n.op.clone())
            else {
                break;
            };
            if next_bci != call_bci
                || self.graph.catch_arm_sites.get(next_site as usize).is_none()
                || !link.nodes.iter().all(|&n| n == c)
            {
                break;
            }
            let Some((leg_block, leg_caught)) = self.catch_arm_leg(t, normal) else {
                break;
            };
            more.push((next_site, leg_block, leg_caught));
            prev_block = normal;
            prev_term = t;
        }
        Ok(CatchArmWire {
            site,
            arm_block,
            caught,
            more,
        })
    }

    /// SPLICE4-1 / HANDLER19-3: the arm block of the `If(CatchEntryFlag)`
    /// `term` ending block `pred`, when the probe may jump into it: its only
    /// predecessor is `pred`, and nothing scheduled before its caught
    /// throwable can call, trap or reach a safepoint (the throwable sits in
    /// the phi-copy scratch word, which no map names, until its read). Returns
    /// the block and whether it reads the throwable at all.
    pub(in crate::ir_lower) fn catch_arm_leg(&self, term: NodeId, pred: usize) -> Option<(usize, bool)> {
        let arm_block = self.schedule.blocks.iter().position(|b| {
            self.graph
                .nodes
                .get(b.ctrl as usize)
                .is_some_and(|n| n.op == Op::Proj(0) && n.inputs.first() == Some(&term))
        })?;
        let arm = &self.schedule.blocks[arm_block];
        if arm.predecessors.as_slice() != [pred] {
            return None;
        }
        let arm_ctrl = arm.ctrl;
        let caught_at = arm.nodes.iter().position(|&n| {
            self.graph
                .nodes
                .get(n as usize)
                .is_some_and(|x| x.op == Op::CaughtException)
        });
        match caught_at {
            Some(k) => {
                let prefix_quiet = arm.nodes[..k].iter().all(|&n| {
                    self.graph.nodes.get(n as usize).is_some_and(|x| {
                        op_cannot_deopt(&x.op)
                            && !matches!(x.op, Op::OsrLocal(_) | Op::CaughtException)
                    })
                });
                if !prefix_quiet {
                    return None;
                }
            }
            None => {
                let elsewhere =
                    self.graph.nodes.iter().any(|x| {
                        x.op == Op::CaughtException && x.inputs.first() == Some(&arm_ctrl)
                    });
                if elsewhere {
                    return None;
                }
            }
        }
        Some((arm_block, caught_at.is_some()))
    }

    /// SPLICE4-1: route the exceptional exit `patch` of the armed call being
    /// lowered through a local-handler probe. The exit the call would have
    /// taken is recorded exactly as before (a rethrow pad on a protected bci,
    /// the shared exit otherwise), then held back as the probe's miss edge.
    pub(in crate::ir_lower) fn push_catch_probe(&mut self, patch: usize, wire: CatchArmWire, shadow: Option<Vec<i16>>) {
        let pads_before = self.rethrow_pads.len();
        let exits_before = self.call_exc_patches.len();
        self.push_call_exc_patch_route(patch);
        let fallback = if self.rethrow_pads.len() > pads_before {
            match self.rethrow_pads.pop() {
                Some(pad) => CatchFallback::Rethrow(pad),
                None => return,
            }
        } else if self.call_exc_patches.len() > exits_before {
            match self.call_exc_patches.pop() {
                Some((_, bci)) => CatchFallback::Exit(bci),
                None => return,
            }
        } else {
            // The route recorded its exit somewhere else (it cannot today);
            // leave that exit as it is.
            return;
        };
        let Some(site_box) = self.catch_site_box(&wire) else {
            self.note_catch_wire_refused(CatchWireRefusal::SiteBox);
            // Put the exit back where the route left it.
            match fallback {
                CatchFallback::Rethrow(pad) => self.rethrow_pads.push(pad),
                CatchFallback::Exit(bci) => self.call_exc_patches.push((patch, bci)),
            }
            return;
        };
        self.catch_probes.push(CatchProbe {
            patch,
            wire,
            site_box,
            fallback,
            shadow,
        });
    }

    /// SPLICE4-1: the `JitLocalHandlerSite` of the wire's call, created on
    /// first use, and `no_collect` (the probe runs where no safepoint map
    /// describes this frame, so the lookup may neither load a class nor build
    /// a throwable; it declines instead). Wave 5's one-arm call asks ONE
    /// candidate, the row the arm enters.
    ///
    /// HANDLER19-3: a ladder asks the call's row prefix in table order, cut
    /// at the first row whose handler has no proved leg in THIS wire (the
    /// lookup takes the throwable on a match, so a matched row must have an
    /// arm). Keyed by the first site and the cut, so an unrolled copy whose
    /// ladder was cut shorter gets its own site.
    pub(in crate::ir_lower) fn catch_site_box(&mut self, wire: &CatchArmWire) -> Option<usize> {
        let row = self.graph.catch_arm_sites.get(wire.site as usize)?;
        let mut legs: Vec<usize> = Vec::with_capacity(1 + wire.more.len());
        legs.push(row.handler_pc);
        for &(s, _, _) in &wire.more {
            legs.push(self.graph.catch_arm_sites.get(s as usize)?.handler_pc);
        }
        let mut candidates: Vec<(&'static str, u32)> = Vec::with_capacity(row.candidates.len());
        for &(name, handler) in &row.candidates {
            if !legs.contains(&handler) || candidates.len() >= CATCH_LADDER_MAX {
                break;
            }
            candidates.push((name, u32::try_from(handler).ok()?));
        }
        if candidates.is_empty() {
            return None;
        }
        let key = (wire.site, candidates.len());
        if let Some(&idx) = self.catch_site_index.get(&key) {
            return Some(idx);
        }
        let throw_bci = u32::try_from(row.call_bci).ok()?;
        let declaring_class_id = row.declaring_class_id;
        let idx = self.catch_site_boxes.len();
        self.catch_site_boxes
            .push(Box::new(crate::JitLocalHandlerSite {
                candidates,
                declaring_class_id,
                throw_bci,
                cache: std::sync::atomic::AtomicU64::new(crate::JitLocalHandlerSite::CACHE_EMPTY),
                implicit_cache: crate::JitLocalHandlerSite::empty_implicit_cache(),
                no_collect: true,
            }));
        self.catch_site_index.insert(key, idx);
        Some(idx)
    }

    /// SPLICE4-1: emit every probe [`Self::push_catch_probe`] deferred, right
    /// after the body (every block offset is known) and before anything reads
    /// `call_exc_patches` or the rethrow pads. Entered with the sentinel in
    /// RAX, after the call's republish and shadow reload, the callee-deopt
    /// service and the `dispatch_threw` peek:
    ///
    /// ```text
    ///   MOV  arg0, [rbp - context]          ; SharedVm
    ///   MOV  arg1, imm64 &JitLocalHandlerSite
    ///   LEA  arg2, [rbp - phi_copy_scratch] ; where the throwable is parked
    ///   CALL jit_local_handler_lookup       ; no_collect: never a GC point
    ///   TEST EAX, EAX
    ///   JS   .miss
    ///   (MOV [rbp - phi_copy_scratch], 0)   ; when nothing reads it
    ///   JMP  arm block
    /// .miss:
    ///   MOV  RAX, i64::MIN
    ///   JMP  the exit the call always took
    /// ```
    ///
    /// The lookup clobbers only caller-saved registers; every value the arm
    /// reads lives in a home word or a callee-saved register
    /// ([`Self::catch_arm_wire`]), so the arm finds it where the `If` would
    /// have left it. On a hit the lookup has taken the pending throwable and
    /// cleared the throw bci; on a miss it changed nothing.
    pub(in crate::ir_lower) fn emit_catch_probes(&mut self) {
        for probe in std::mem::take(&mut self.catch_probes) {
            self.patch_rel32_to_here(probe.patch);
            let Some(site) = self.catch_site_boxes.get(probe.site_box) else {
                self.latch_bailout(Bailout::new(BailoutReason::Internal(
                    "ir_lower: a compiled handler probe lost its site",
                )));
                return;
            };
            // Owned by `catch_site_boxes`, which moves onto the published
            // artifact's `_jit_local_handler_sites`: the address outlives the
            // machine code that bakes it. `Vec<Box<..>>`, so a later push
            // cannot move the pointee.
            let site_ptr = std::ptr::addr_of!(**site) as u64;
            let (handler_pc, call_bci) = (site.candidates.first().map(|c| c.1), site.throw_bci);
            let ladder = site.candidates.len();
            // HANDLER19-3: the arm of each candidate the lookup can answer,
            // by its handler pc. A one-arm call is wave 5's single jump.
            let legs: Vec<(usize, usize, bool)> =
                std::iter::once((probe.wire.site, probe.wire.arm_block, probe.wire.caught))
                    .chain(probe.wire.more.iter().copied())
                    .filter_map(|(s, block, caught)| {
                        self.graph
                            .catch_arm_sites
                            .get(s as usize)
                            .map(|row| (row.handler_pc, block, caught))
                    })
                    .collect();
            let targets: Vec<Option<(usize, bool)>> = site
                .candidates
                .iter()
                .map(|&(_, h)| {
                    legs.iter()
                        .find(|&&(lh, _, _)| lh == h as usize)
                        .map(|&(_, block, caught)| (block, caught))
                })
                .collect();
            self.load_context(CALL_ARG_REGS[0]);
            self.emit_mov_reg_imm64(CALL_ARG_REGS[1], site_ptr);
            self.lea_reg_from_frame(CALL_ARG_REGS[2], self.phi_copy_scratch_slot_off);
            self.emit_mov_reg_imm64(RAX, self.local_handler_lookup as u64);
            self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
            self.buf.emit(&[0x85, 0xC0]); // TEST EAX, EAX
            let miss = self.emit_jcc_rel32(0x88); // JS .miss
                                                  // HANDLER6-3: an exit that left before its site's shadow reload
                                                  // reloads it here, on the hit edge only (EAX, the candidate index,
                                                  // survives: the sequence clobbers RCX/R10/R11). The miss edge keeps
                                                  // the publication pending for the exit it always took, which
                                                  // handles it as before.
            if let Some(offsets) = probe.shadow.as_deref() {
                self.emit_shadow_reload_words(offsets);
            }
            let last = targets.len().saturating_sub(1);
            for (idx, target) in targets.iter().enumerate() {
                let Some((arm_block, caught)) = *target else {
                    self.latch_bailout(Bailout::new(BailoutReason::Internal(
                        "ir_lower: a compiled handler candidate has no arm",
                    )));
                    return;
                };
                // CMP EAX, idx ; JNE .next  (every index but the last; the
                // lookup answers only indices of `candidates`).
                let next = if idx < last {
                    self.buf.emit(&[0x83, 0xF8, idx as u8]);
                    Some(self.emit_jcc_rel32(0x85))
                } else {
                    None
                };
                if !caught {
                    self.emit_zero_frame_slot(self.phi_copy_scratch_slot_off);
                }
                let Some(arm_at) = self.block_offsets.get(arm_block).copied() else {
                    self.latch_bailout(Bailout::new(BailoutReason::Internal(
                        "ir_lower: a compiled handler probe names a block that was not emitted",
                    )));
                    return;
                };
                self.emit_jmp_back_to(arm_at);
                if let Some(next) = next {
                    self.patch_rel32_to_here(next);
                }
            }
            self.patch_rel32_to_here(miss);
            self.emit_mov_reg_imm64(RAX, i64::MIN as u64);
            let exit = self.emit_jmp_rel32();
            match probe.fallback {
                CatchFallback::Rethrow(mut pad) => {
                    pad.patch = exit;
                    self.rethrow_pads.push(pad);
                }
                CatchFallback::Exit(bci) => self.call_exc_patches.push((exit, bci)),
            }
            crate::metrics::note_local_handler(crate::metrics::LOCAL_HANDLER_SITE_EMITTED);
            if crate::x64::dbg_jitc_enabled() {
                eprintln!(
                    "[cratonvm-jitc] ir-catch-probe {} call pc={call_bci} -> handler pc={} \
                     (arm block {}, reads the throwable: {}){}",
                    self.graph.scope_method_key,
                    handler_pc.map_or(-1, i64::from),
                    probe.wire.arm_block,
                    probe.wire.caught,
                    if ladder > 1 {
                        format!(" ladder={ladder}")
                    } else {
                        String::new()
                    },
                );
            }
        }
    }

    /// Round 15 wave 1 (lane sync, SS8-2): [`Self::push_call_exc_patch`] for
    /// the exceptional edge of a self-locking body's OWN method-monitor enter
    /// or exit ([`Self::self_lock_unheld_exc_patches`]): the same bci and
    /// monitor facts, but a row of the stub that releases nothing. An admitted
    /// method has no protected range, so there is no reason-9 arm to take
    /// ([`Self::self_lock_refusal`] refuses one).
    pub(in crate::ir_lower) fn push_self_lock_unheld_exc_patch(&mut self, patch: usize) {
        let bci = self.resume_bci(self.cur_bci);
        self.transfer_bcis.insert(bci);
        self.note_flat_transfer(bci, "self_lock_unheld_exc_patch", true);
        self.propagating_exit_holds_monitor = true;
        self.self_lock_unheld_exc_patches.push((patch, bci));
    }

    /// Does `bci` (a METHOD bci, after [`Self::resume_bci`]) lie in one of the
    /// protected ranges this compile was handed? Always `false` off the
    /// precise-frame route, where the list is empty.
    pub(in crate::ir_lower) fn bci_is_protected(&self, bci: usize) -> bool {
        self.protected_ranges
            .iter()
            .any(|&(start, end)| (start as usize) <= bci && bci < (end as usize))
    }

    /// Might the frame hold a monitor at a throw site resolving to `bci`?
    ///
    /// The snapshot the site's frame state would come from (the node's own
    /// one, else the first at `bci` — the rule of
    /// [`Self::resolve_frame_state_for_site`]) answers it. No snapshot at all
    /// answers `true`: this feeds an OSR admission fact, and not knowing is a
    /// refusal there.
    pub(in crate::ir_lower) fn site_snapshot_may_hold_monitor(&self, bci: usize) -> bool {
        let own = self
            .cur_node_frame
            .and_then(|si| self.graph.safepoints.get(si as usize))
            .filter(|sp| sp.bci == bci);
        let sp = own.or_else(|| {
            self.safepoint_first_by_bci
                .get(&bci)
                .and_then(|&idx| self.graph.safepoints.get(idx))
        });
        sp.is_none_or(|sp| !sp.monitors.is_empty())
    }

    /// Record a reason-9 rethrow pad for the call-exception exit whose rel32
    /// is `patch`, thrown at the protected method bci `bci` (round 11 wave 10,
    /// lane irexc; `r11w9-lower-ir-tier-never-compiles-a-javac-synchronized-block`).
    ///
    /// The IR form of the single-pass backend's reason-9 stub
    /// (`x64/deopt_stubs.rs`, `emit_post_invoke_exception_check`). The frame is
    /// the snapshot at the THROWING bci — the state before that bytecode, whose
    /// locals are the locals at the throw, since no bytecode that throws has
    /// written a local first. It is resolved HERE, while the call site is the
    /// node being lowered, with the same resolver every guard uses, so
    /// `frame_value_for`'s `UnallocatedValue` latch enforces that every local
    /// the snapshot names has a location at this point: the IR form of the
    /// single-pass `first_unsupported_precise_frame_site` proof, checked
    /// instead of assumed. The pad itself ([`Self::emit_rethrow_pads`]) runs
    /// after the call returned, and every location a frame value can name
    /// survives the call: a home word, or a register of `IR_LOWER_LS_GPRS`,
    /// all callee-saved on the target ABI.
    ///
    /// The operand stack is published EMPTY. JVMS §2.10: entering a handler
    /// clears it and pushes the throwable, and every consumer of a reason-9
    /// frame rebuilds exactly that. The snapshot's stack is the PRE-call one
    /// (it still holds the arguments), which is not a state any consumer may
    /// resume on.
    ///
    /// Fails CLOSED. A frame the reason-9 consumers cannot hand a handler as
    /// it stands — an undescribable local, a scalar-replaced object, a lock
    /// that would have to be re-acquired, an inlined caller scope — refuses
    /// the compile (the method keeps its single-pass body). The consumers would
    /// otherwise PROPAGATE the exception past the handler that should catch
    /// it (`route_jit_signal_exception` on an unmappable frame, the OSR
    /// transfer on a virtual or relock one), which is a wrong answer, not a
    /// slow one.
    ///
    /// Monitors are accounted exactly once by construction: a monitor entry
    /// here is one the compiled code took itself (`relock == false`, refused
    /// otherwise), the thread still holds it, and no consumer takes it again —
    /// the handler's own `monitorexit` releases it.
    pub(in crate::ir_lower) fn push_rethrow_pad(&mut self, patch: usize, bci: usize) {
        match self.plan_rethrow_pad(patch, bci) {
            Some(pad) => self.rethrow_pads.push(pad),
            // The compile is doomed (the refusal is latched); keep the walk
            // consistent with a plain exit so nothing below has to know.
            None => self.call_exc_patches.push((patch, bci)),
        }
    }

    /// The pad [`Self::push_rethrow_pad`] records, built but not pushed:
    /// its point (boxed, published to `rethrow_points` / `deopt_boxes`), its
    /// frame and its shadow, resolved NOW, while the site is the node being
    /// lowered. `patch` is the site's own exceptional rel32: the point's
    /// identity (`native_offset`) and the pad's initial `patch`, which a
    /// caller that jumps to the pad from elsewhere (the call-site exception
    /// pad's fallback, nio21 wave 3 L2) replaces with its own `JMP`. `None`
    /// when the frame cannot be published, with the refusal latched.
    pub(in crate::ir_lower) fn plan_rethrow_pad(&mut self, patch: usize, bci: usize) -> Option<RethrowPad> {
        let prev_collect_mask = self.post_collect_ref_mask.replace(Some(0));
        let mut frame_state = self.resolve_frame_state_for_site(bci);
        self.post_collect_ref_mask.set(prev_collect_mask);
        frame_state.stack.clear();
        let osr_virtuals =
            self.graph.osr_compile && crate::deopt::osr_exception_exit_virtuals_enabled();
        if let Some(why) = rethrow_frame_refusal(&frame_state, osr_virtuals) {
            self.latch_bailout(Bailout::with_context(
                BailoutReason::UnsupportedShape(
                    "ir_lower: a protected throw site's exceptional frame cannot be published",
                ),
                format!("throw bci {bci}: {why}"),
            ));
            return None;
        }
        let reason = DeoptReason::PendingException;
        // Cast: a bci fits u32 (`IR_MAX_BYTECODE_SIZE`), and so does a code
        // offset (`ExecutableBuffer` capacities are far below 4 GiB).
        let point = Box::new(DeoptimizationPoint {
            // The site's own exceptional branch: unique per pad, and never an
            // instruction start, so it cannot collide with a by-bci point's
            // anchor in `deopt_points`.
            native_offset: patch as u32,
            bci: bci as u32,
            reason,
            action: DeoptAction::Reinterpret,
            speculation_id: crate::deopt::speculation_id(bci as u32, reason),
            frame_state,
            semantics: ResumeSemantics::for_reason(reason),
        });
        let point_ptr = point.as_ref() as *const DeoptimizationPoint as u64;
        self.rethrow_points.push(DeoptimizationPoint::clone(&point));
        self.deopt_boxes.push(point);
        let shadow = self.pending_shadow_copy_back();
        Some(RethrowPad {
            patch,
            point_ptr,
            bci,
            shadow,
        })
    }

    /// Record the call-site exception pad of an armed `Op::Call`'s exceptional
    /// rel32 `patch` (round nio21 wave 3, lane L2; [`Self::emit_call_throw_pads`]
    /// emits it). Everything [`Self::push_call_exc_patch`] would decide for
    /// this site is decided here, now, while the call is the node being
    /// lowered -- the transfer bci, the monitor fact, a protected bci's
    /// reason-9 point and frame -- and kept as the pad's fallback, so a `-1`
    /// from the helper takes exactly the exit the site had before.
    ///
    /// Returns `false`, having recorded nothing, when a `Ref` call result has
    /// no home word yet (the pad could not null it): the caller then records
    /// today's exit.
    pub(in crate::ir_lower) fn push_call_throw_pad(&mut self, patch: usize, plan: CallThrowPlan) -> bool {
        let result_home = if self
            .graph
            .nodes
            .get(plan.call as usize)
            .is_some_and(|n| n.ty == IrType::Ref)
        {
            match self.node_slot.get(plan.call as usize).copied().flatten() {
                Some(off) => Some(off.get() as i32),
                None => return false,
            }
        } else {
            None
        };
        let bci = self.resume_bci(self.cur_bci);
        self.transfer_bcis.insert(bci);
        self.note_flat_transfer(bci, "call_throw_pad", true);
        let fallback = if self.bci_is_protected(bci) {
            match self.plan_rethrow_pad(patch, bci) {
                Some(pad) => CallThrowFallback::Rethrow(pad),
                // Latched: the compile is refused; any shape will do.
                None => CallThrowFallback::Plain { bci },
            }
        } else {
            if !self.propagating_exit_holds_monitor && self.site_snapshot_may_hold_monitor(bci) {
                self.propagating_exit_holds_monitor = true;
            }
            CallThrowFallback::Plain { bci }
        };
        let shadow = self.pending_shadow_copy_back();
        let map = self.call_throw_publication.as_ref().and_then(|site| site.map.clone());
        self.call_throw_pads.push(CallThrowPad {
            map,
            entry_patch: patch,
            site_ptr: plan.site_ptr,
            targets: plan.targets,
            shadow,
            result_home,
            fallback,
        });
        true
    }

    /// Emit `Jcc rel32` with a placeholder displacement; returns the native
    /// offset of the 4-byte operand. `cc` is the second opcode byte:
    /// `0x84` = JE/JZ, `0x85` = JNE/JNZ.
    pub(in crate::ir_lower) fn emit_jcc_rel32(&mut self, cc: u8) -> usize {
        self.buf.emit(&[0x0F, cc]);
        let patch = self.buf.pos();
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
        patch
    }

    /// Emit `JMP rel32` with a placeholder displacement; returns the operand
    /// offset.
    pub(in crate::ir_lower) fn emit_jmp_rel32(&mut self) -> usize {
        self.buf.emit_byte(0xE9);
        let patch = self.buf.pos();
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
        patch
    }

    /// Resolve a rel32 branch operand so it targets the CURRENT buffer
    /// position.
    ///
    /// Never panics. Once `ExecutableBuffer` has overflowed it drops writes and
    /// stops advancing `len`, so recorded patch offsets no longer address their
    /// placeholder bytes and `try_patch_i32` legitimately fails (setting the
    /// sticky flag itself). `lower_inner` discards the whole artifact in that
    /// case, so swallowing the error is correct — an `expect` here would turn a
    /// recoverable "fall back to single-pass" into a compile-thread panic.
    pub(in crate::ir_lower) fn patch_rel32_to_here(&mut self, patch: usize) {
        let here = self.buf.pos();
        let rel = here as i32 - (patch as i32 + 4);
        let _ = self.buf.try_patch_i32(patch, rel);
    }

    /// Marshal a cache hit into the compiled-callee entry ABI.
    ///
    /// Context-using artifacts receive the hidden VM pointer in `abi[0]`;
    /// context-free artifacts receive Java arg0 there. Inline caches retain the
    /// callee's ABI bit, so both shapes can use the cache instead of forcing
    /// context-free methods back through the resolving helper.
    pub(in crate::ir_lower) fn emit_ic_abi_marshal(&mut self, inputs: &[NodeId], num_args: usize, needs_context: bool) {
        let base = if needs_context {
            self.load_context(ENTRY_ABI_REGS[0]);
            1
        } else {
            0
        };
        for i in 0..num_args {
            let arg = inputs[2 + i];
            self.gp_load_abi_value(ENTRY_ABI_REGS[base + i], arg);
        }
    }

    /// Marshal the Java arguments into the **no-context** register layout once,
    /// ahead of the whole inline-cache cascade.
    ///
    /// # Why this may sit before the guards
    ///
    /// The cascade's guards touch RAX (the receiver and its class id), R10 (the
    /// cache-slot base) and R11 (the call target), and **nothing else** — no
    /// `ENTRY_ABI_REGS` member appears in `emit_cmp_eax_r10_disp`,
    /// `emit_load_ic_entry_word`, `emit_strip_ic_context_tag`, or the
    /// receiver null and kind checks. A layout established here therefore
    /// survives every arm of the cascade to its `CALL`.
    ///
    /// # What it replaces
    ///
    /// `needs_context` is a property of the *cached entry*, not of the site, so
    /// the marshalling was emitted twice per cache entry — ten copies at a site
    /// with one MIC and a four-entry PIC, each of them `num_args` frame loads.
    /// The no-context layout is now built once, and a context-needing arm
    /// converts it with [`Self::emit_ic_shift_for_context`], which is
    /// register-to-register.
    ///
    /// The alternative — one uniform entry ABI — would delete the question
    /// entirely, and is deliberately not taken here: `needs_context` is an
    /// output of optimization, so changing it changes how *every* compiled
    /// method receives its arguments. That is not a change to fold into this
    /// one.
    pub(in crate::ir_lower) fn emit_ic_premarshal_no_context(&mut self, inputs: &[NodeId], num_args: usize) {
        for i in 0..num_args {
            let arg = inputs[2 + i];
            self.gp_load_abi_value(ENTRY_ABI_REGS[i], arg);
        }
    }

    /// Convert the pre-marshalled no-context layout into the context one: shift
    /// every argument up one register and load the context into
    /// `ENTRY_ABI_REGS[0]`.
    ///
    /// **Descending order is load-bearing.** Moving `[0] -> [1]` first would
    /// overwrite argument 1 before it is read; from the top down, every
    /// destination holds a value that has already been moved.
    ///
    /// Legal only for a site that fits the file with the context word
    /// (`num_args + 1 <= ENTRY_ABI_REGS.len()`), so that the top destination
    /// `ENTRY_ABI_REGS[num_args]` is inside it. Since round 11 wave 13
    /// `emit_inline_cache_call` also admits over-wide sites; those never
    /// pre-marshal and never reach here — their hits go through
    /// [`Self::emit_ic_wide_hit_call`] instead.
    pub(in crate::ir_lower) fn emit_ic_shift_for_context(&mut self, num_args: usize) {
        debug_assert!(
            num_args + 1 <= ENTRY_ABI_REGS.len(),
            "the context shift needs {} registers and the file has {}",
            num_args + 1,
            ENTRY_ABI_REGS.len(),
        );
        for i in (0..num_args).rev() {
            self.emit_mov_reg_reg64(ENTRY_ABI_REGS[i + 1], ENTRY_ABI_REGS[i]);
        }
        self.load_context(ENTRY_ABI_REGS[0]);
    }

    /// The outgoing stack block an OVER-WIDE inline-cache hit reserves:
    /// `Some((total_sub, base_disp))` when `num_args` Java arguments plus the
    /// context word overflow `ENTRY_ABI_REGS` by 1..=[`ir_ic_max_stack_words`]
    /// words, `None` when they fit (or overflow further; `ir_ic_admits_arity`
    /// keeps those out).
    ///
    /// Sized for the CONTEXT ABI, which always needs one stack word more than
    /// the context-free one, so one `SUB`/`ADD` pair of this size serves both
    /// paths of a hit; the context-free path leaves the top word unused. The
    /// layout is [`stack_arg_block_size`]'s, the one `emit_direct_cross_call`
    /// writes and every compiled prologue reads (`STACK_ARG_BASE`).
    pub(in crate::ir_lower) fn ic_wide_stack_block(num_args: usize) -> Option<(i32, i32)> {
        let words = (num_args + 1)
            .checked_sub(ENTRY_ABI_REGS.len())
            .filter(|&w| w > 0)?;
        if words > ir_ic_max_stack_words() {
            return None;
        }
        Some(stack_arg_block_size(words))
    }

    /// Whether the inline cache of the site `info_ptr` describes must screen
    /// its receiver's kind before trusting the header class id (round 13 wave
    /// 11, lane mega9, proposal M13-6). `false` only for a real
    /// `invokevirtual` (`invoke_kind == 0`: `lib.rs` re-marks a private or
    /// CHA-bound one `1`, which takes no cache) whose constant-pool owner no
    /// array is assignable to (`inline_cache_pic::ic_static_receiver_admits_arrays`),
    /// and only under `CRATONVM_JIT_IR_IC_KIND_SCREEN_ELIDE` (default on). An
    /// interface site always screens: the verifier treats interface types as
    /// `Object`.
    pub(in crate::ir_lower) fn ic_receiver_may_be_array(info_ptr: usize) -> bool {
        if info_ptr == 0 {
            return true;
        }
        // SAFETY: `info_ptr` is the address `Op::Call` bakes for this site: a
        // live `JitInvokeInfo` owned by this compile's call infos for the
        // artifact's life, which the dispatch arm reads the same way.
        let info = unsafe { &*(info_ptr as *const JitInvokeInfo) };
        info.invoke_kind != 0
            || crate::inline_cache_pic::ic_static_receiver_admits_arrays(info.class_name)
    }

    /// Whether the inline-cache site `info_ptr` describes is an
    /// `invokeinterface` (`invoke_kind == 2`), for the round-17 vtable-column
    /// admission (`runtime_lowering::mega_vtable_column_admitted`). A site
    /// with no info answers `true`, the side that admits less.
    pub(in crate::ir_lower) fn ic_site_is_interface(info_ptr: usize) -> bool {
        if info_ptr == 0 {
            return true;
        }
        // SAFETY: as in `ic_receiver_may_be_array`: the live `JitInvokeInfo`
        // this site bakes, owned by the compile's call infos.
        let info = unsafe { &*(info_ptr as *const JitInvokeInfo) };
        info.invoke_kind != 0
    }

    /// The ONE hit tail of an over-wide inline-cache site (round 11 wave 13,
    /// lane `icwide`; the IR twin of the single-pass MIC's `mic_stack_block`
    /// arm). Entered from the MIC arm by fall-through and from every PIC rung
    /// by a `JMP`, always straight after `BTR R11, 0`: R11 holds the bare
    /// entry, CF its needs-context tag.
    ///
    /// ```text
    ///   JNC .noctx
    ///   SUB RSP, total ; <stack words, context ABI> ; <ctx + registers> ; JMP .call
    /// .noctx:
    ///   SUB RSP, total ; <stack words, no-context ABI> ; <registers>
    /// .call:
    ///   CALL R11 ; <inline-frame row> ; ADD RSP, total ; <frame republish>
    /// ```
    ///
    /// The `SUB` is on each ABI branch, after the tag has chosen it, so one
    /// `ADD` after the shared `CALL` balances both. The release comes right
    /// after the return address is recorded and before the frame republish,
    /// whose helper form calls out with the frame's own RSP (the order
    /// `emit_direct_cross_call` keeps). Everything the caller emits after this
    /// (`emit_ic_hit_exit`, the callee-deopt service) runs at that RSP too.
    ///
    /// Register discipline, as the register-only arms: the marshal writes RAX
    /// (the receiver's class id, dead once a hit is chosen) and
    /// `ENTRY_ABI_REGS` only, never R10/R11, so the target survives to the
    /// `CALL` (the security invariant on [`Self::emit_call_loaded_ic_entry`]).
    pub(in crate::ir_lower) fn emit_ic_wide_hit_call(
        &mut self,
        inputs: &[NodeId],
        num_args: usize,
        total_sub: i32,
        base_disp: i32,
        map: &mut Option<SafepointMapToken>,
    ) {
        let noctx = self.emit_jcc_rel32(0x83); // JNC .noctx
        self.emit_sub_rsp_imm32(total_sub);
        self.emit_ic_wide_marshal(inputs, num_args, true, base_disp);
        let call = self.emit_jmp_rel32();
        self.patch_rel32_to_here(noctx);
        self.emit_sub_rsp_imm32(total_sub);
        self.emit_ic_wide_marshal(inputs, num_args, false, base_disp);
        self.patch_rel32_to_here(call);
        self.emit_call_loaded_ic_entry_releasing(total_sub, map);
    }

    /// Marshal an over-wide hit's arguments for one ABI: the context word
    /// first when `with_ctx`, then Java argument `j` into
    /// `ENTRY_ABI_REGS[j + lead]` while that exists, and the rest to
    /// `[RSP + base_disp + 8k]`, `k` counted from the first slot past the
    /// file. The stack words go FIRST, through RAX, so no register argument is
    /// loaded before a store that needs RAX; the single-pass
    /// `emit_ic_hit_arg_marshal` orders them the same way. RSP has already
    /// been lowered by the block; every source is `[rbp - off]` or a
    /// callee-saved register, neither of which the `SUB` disturbs.
    pub(in crate::ir_lower) fn emit_ic_wide_marshal(
        &mut self,
        inputs: &[NodeId],
        num_args: usize,
        with_ctx: bool,
        base_disp: i32,
    ) {
        let lead = usize::from(with_ctx);
        for j in 0..num_args {
            let Some(k) = (j + lead).checked_sub(ENTRY_ABI_REGS.len()) else {
                continue;
            };
            let Some(&arg) = inputs.get(2 + j) else {
                continue;
            };
            self.gp_load_abi_value(RAX, arg);
            // Cast: `k < ir_ic_max_stack_words()`, so `k * 8` is tiny.
            self.emit_mov_rsp_disp_from_rax(base_disp + (k as i32) * 8);
        }
        if with_ctx {
            self.load_context(ENTRY_ABI_REGS[0]);
        }
        for j in 0..num_args {
            let (Some(&reg), Some(&arg)) = (ENTRY_ABI_REGS.get(j + lead), inputs.get(2 + j)) else {
                continue;
            };
            self.gp_load_abi_value(reg, arg);
        }
    }

    /// Resolve a rel32 branch operand so it targets `target`, an EARLIER
    /// position (a backward branch). Same error policy as
    /// [`Self::patch_rel32_to_here`].
    pub(in crate::ir_lower) fn patch_rel32_to_target(&mut self, patch: usize, target: usize) {
        let rel = target as i64 - (patch as i64 + 4);
        let Ok(rel) = i32::try_from(rel) else {
            self.latch_bailout(Bailout::new(BailoutReason::Internal(
                "ir_lower: a backward rel32 branch does not fit",
            )));
            return;
        };
        let _ = self.buf.try_patch_i32(patch, rel);
    }

    /// `CMP EAX, dword [R10 + disp]` — an inline-cache class-id guard.
    pub(in crate::ir_lower) fn emit_cmp_eax_r10_disp(&mut self, disp: u8) {
        if disp == 0 {
            // REX.B + 3B /r + ModRM(mod=00, reg=RAX, rm=R10)
            self.buf.emit(&[0x41, 0x3B, 0x02]);
        } else {
            // REX.B + 3B /r + ModRM(mod=01, reg=RAX, rm=R10) + disp8
            self.buf.emit(&[0x41, 0x3B, 0x42, disp]);
        }
    }

    /// `MOV R11, qword [R10 + disp] ; TEST R11, R11` — capture an inline-cache
    /// entry word ONCE. The caller follows with `JZ .slow` (a zero word is a
    /// profile seed with no target, or a withdrawn one) and then
    /// [`Self::emit_strip_ic_context_tag`].
    ///
    /// The word carries the target AND its calling convention
    /// (`JIT_IC_NEEDS_CONTEXT_TAG` in bit 0). Loading the two separately let a
    /// reader pair one publication's address with the next one's ABI flag, or
    /// pass a non-zero check and then load a zero target.
    pub(in crate::ir_lower) fn emit_load_ic_entry_word(&mut self, disp: u8) {
        if disp == 0 {
            self.buf.emit(&[0x4D, 0x8B, 0x1A]); // MOV R11, [R10]
        } else {
            self.buf.emit(&[0x4D, 0x8B, 0x5A, disp]); // MOV R11, [R10+disp8]
        }
        self.buf.emit(&[0x4D, 0x85, 0xDB]); // TEST R11, R11
    }

    /// `BTR R11, 0` — move the needs-context tag into CF and leave the bare
    /// entry address in R11. The caller branches `JNC` to the context-free
    /// arm.
    pub(in crate::ir_lower) fn emit_strip_ic_context_tag(&mut self) {
        const _: () = assert!(crate::JIT_IC_NEEDS_CONTEXT_TAG == 1);
        // REX.W+B + 0F BA /6 + ModRM(mod=11, /6, rm=R11) + imm8
        self.buf.emit(&[0x49, 0x0F, 0xBA, 0xF3, 0x00]);
    }

    /// `CALL R11` — the cached-entry indirect call, through the target
    /// [`Self::emit_load_ic_entry_word`] captured.
    ///
    /// SECURITY INVARIANT (inherited from `x64.rs`'s inline caches): the call
    /// target must not be addressed through R10 across other emitted code, so
    /// it lives in R11 (caller-saved, not an argument register on either ABI,
    /// clobbered by the call anyway). Between the capture and this CALL only
    /// the ABI marshalling runs, and it writes `ENTRY_ABI_REGS` only — nothing
    /// that writes R11 may be emitted there.
    pub(in crate::ir_lower) fn emit_call_loaded_ic_entry(&mut self, map: &mut Option<SafepointMapToken>) {
        self.emit_call_loaded_ic_entry_releasing(0, map);
    }

}
