#[allow(unused_imports)]
use super::*;

impl<'a> Lowerer<'a> {
    /// JIT round 25 wave 9 (lane W9-5): is the `Op::Guard` node `id` at `bci`
    /// a loop CLASS predicate -- `class_predicate`'s, `seq_charat_predicate`'s
    /// String guard or W7-D's field-class guard: a guard at a loop header's
    /// bci (not in a spliced body) naming a frame at that bci, whose
    /// condition is `ExactClassIs` of one reference? Re-executing it in an
    /// OSR stub (`osr_check_compile`'s `ExactClassIs` arm) needs the
    /// reference to be a local the entry's snapshot names; anything else
    /// fails the compile of the check and refuses the entry. `ExactClassIs`
    /// answers `0` for null, so a pass also re-establishes the non-null fact
    /// the predicate's folded null guards relied on.
    pub(in crate::ir_lower) fn op_guard_is_class_predicate(&self, id: NodeId, bci: usize) -> bool {
        if self.pc_is_in_a_spliced_body(bci) || !self.is_loop_header_bci(bci) {
            return false;
        }
        let nodes = &self.graph.nodes;
        let Some(g) = nodes.get(id as usize) else {
            return false;
        };
        if g.bytecode_pc != Some(bci)
            || g.inputs.len() != 2
            || !g
                .frame_snapshot
                .and_then(|si| self.graph.safepoints.get(si as usize))
                .is_some_and(|sp| sp.bci == bci && !sp.is_splice_state())
        {
            return false;
        }
        g.inputs
            .get(1)
            .and_then(|&c| nodes.get(c as usize))
            .is_some_and(|cn| {
                matches!(cn.op, Op::ExactClassIs { class_id } if class_id != 0)
                    && cn.inputs.len() == 1
                    && nodes
                        .get(cn.inputs[0] as usize)
                        .is_some_and(|r| r.ty == IrType::Ref)
            })
    }

    /// W9-5 fix: does the phi `phi` only ever carry `v` -- every value input
    /// is `v` or another phi that only carries `v` (a cycle of such phis
    /// included)? Then `phi == v` on every path. `false` for anything that is
    /// not a phi, and past a small search bound.
    pub(in crate::ir_lower) fn osr_phi_only_carries(&self, phi: NodeId, v: NodeId) -> bool {
        let nodes = &self.graph.nodes;
        let mut seen: Vec<NodeId> = Vec::new();
        let mut work = vec![phi];
        while let Some(p) = work.pop() {
            if p == v || seen.contains(&p) {
                continue;
            }
            if seen.len() >= 64 {
                return false;
            }
            seen.push(p);
            let Some(n) = nodes.get(p as usize) else {
                return false;
            };
            if n.op != Op::Phi || n.inputs.len() < 2 {
                return false;
            }
            for &x in &n.inputs[1..] {
                if x == NO_NODE {
                    return false;
                }
                work.push(x);
            }
        }
        true
    }

    /// Is `bci` the bci of a loop header (a `Merge` / `Region` block control)?
    pub(in crate::ir_lower) fn is_loop_header_bci(&self, bci: usize) -> bool {
        self.schedule.blocks.iter().any(|b| {
            self.graph.nodes.get(b.ctrl as usize).is_some_and(|n| {
                matches!(n.op, Op::Merge | Op::Region) && n.bytecode_pc == Some(bci)
            })
        })
    }

    /// Round 16 (lane matrix, MX16-1): the nodes an OSR entry must re-execute
    /// when it jumps past their block, with that block: every node the two
    /// pre-header transforms plant AT A LOOP HEADER'S BCI under a named frame
    /// -- the array-load hoist's copies (`ArrayLoad`) and the range
    /// predicates (`ArrayLength` and the `Guard`). No other node carries a
    /// header's bci: a header's first bytecode loads an operand and builds no
    /// node of these kinds.
    ///
    /// JIT round 25 wave 9 (lane W9-5): with `class` (the
    /// `CRATONVM_JIT_IR_OSR_RECHECK_CLASS_PREDICATES` switch, default ON since j27), the loop class
    /// predicates too ([`Self::op_guard_is_class_predicate`]).
    pub(in crate::ir_lower) fn osr_recheck_nodes(&self, class: bool) -> Vec<(NodeId, usize)> {
        let nblocks = self.schedule.blocks.len();
        let mut out = Vec::new();
        for (id, n) in self.graph.nodes.iter().enumerate() {
            // Cast: an index into the node arena is a `NodeId`.
            let id = id as NodeId;
            let Some(pc) = n.bytecode_pc else {
                continue;
            };
            let kind_ok = match n.op {
                Op::ArrayLoad(_) | Op::ArrayLength => n.frame_snapshot.is_some(),
                Op::Guard { .. } => {
                    self.op_guard_is_range_predicate(id, pc)
                        || (class && self.op_guard_is_class_predicate(id, pc))
                }
                _ => false,
            };
            if !kind_ok || !self.is_loop_header_bci(pc) {
                continue;
            }
            let Some(b) = self.schedule.node_to_block.get(id as usize).copied() else {
                continue;
            };
            if b < nblocks {
                out.push((id, b));
            }
        }
        out
    }

    /// Round 16 (lane matrix, MX16-1): the program an ordinary OSR entry at
    /// block `entry_block` (bci `bci`) runs after its seeding, so that what the
    /// pre-headers it jumps past would have done is done: every re-checkable
    /// node ([`Self::osr_recheck_nodes`]) in a block strictly dominating the
    /// entry is re-executed from the interpreter's locals -- a hoisted read
    /// with its null and bounds checks (published when `roots` says the body
    /// still reads it), a predicate's length read (null fails) and compare --
    /// and any failure leaves through an `OsrExit` point at `bci` whose frame is
    /// the header snapshot the seeding wrote.
    ///
    /// `Ok(None)`: nothing to re-execute. `Err`: the entry must be refused
    /// (a node whose operands the stub cannot rebuild, a root it cannot
    /// write, no deopt stub, or a frame that cannot be resumed in place).
    ///
    /// # Why it is sound
    ///
    /// Each re-checked node's operands are loop-invariant values defined before
    /// its loop, read from the locals that name them now, so the stub computes
    /// what the pre-header computes, against memory the loop does not write
    /// (`ir_optimize::hoist_invariant_array_loads` proved it). A check that
    /// passes establishes exactly the fact the body assumes; one that fails
    /// enters nothing, and the interpreter resumes the frame it handed over.
    #[allow(clippy::type_complexity)]
    pub(in crate::ir_lower) fn osr_entry_recheck_program(
        &self,
        entry_block: usize,
        bci: usize,
        locals: &[NodeId],
        nodes: &[(NodeId, usize)],
        roots: &[NodeId],
    ) -> Result<Option<(Vec<OsrCheckInsn>, Vec<usize>, FrameState)>, &'static str> {
        let dom = &self.schedule.dom;
        let skipped: Vec<NodeId> = nodes
            .iter()
            .filter(|&&(_, b)| b != entry_block && dom.dominates(b, entry_block))
            .map(|&(id, _)| id)
            .collect();
        if roots.iter().any(|r| !skipped.contains(r)) {
            return Err("a hoisted array read live here is not one the entry re-executes");
        }
        if skipped.is_empty() {
            return Ok(None);
        }
        if self.deopt_stub_off.is_none() {
            return Err("no deopt stub for the entry's re-checks to leave through");
        }
        for &r in roots {
            let has_home = self.node_slot.get(r as usize).copied().flatten().is_some()
                && !self.home_dropped.get(r as usize).copied().unwrap_or(false);
            if (!has_home && self.assigned_gpr(r).is_none())
                || self.ls_managed.get(r as usize).copied().unwrap_or(false)
            {
                return Err("a hoisted array read live here has nowhere to be written");
            }
        }
        let policy = OsrRematPolicy {
            compares: ir_osr_remat_compares_enabled(),
            array_length: ir_osr_remat_array_length(),
            static_read: ir_osr_remat_static_read_enabled(),
            // JIT round 19 wave 4 (lane osr4, IROPS2-4): on top of `static_read`.
            static_read_wide: ir_osr_remat_static_read_enabled()
                && ir_osr_remat_static_read_wide_enabled(),
        };
        let mut prog: Vec<OsrCheckInsn> = Vec::new();
        let mut leaves: Vec<usize> = Vec::new();
        let mut budget = OSR_REMAT_MAX_NODES;
        for &id in &skipped {
            let Some(node) = self.graph.nodes.get(id as usize) else {
                return Err("a re-checked node is not in the graph");
            };
            let ok = match node.op {
                Op::Guard { .. } => match node.inputs.get(1).copied() {
                    Some(cond) => {
                        let ok = self.osr_check_compile(
                            cond,
                            locals,
                            policy,
                            (&mut prog, &mut leaves),
                            &mut budget,
                            0,
                            false,
                        );
                        prog.push(OsrCheckInsn::FailIfZero);
                        ok
                    }
                    None => false,
                },
                _ => {
                    let ok = self.osr_check_compile(
                        id,
                        locals,
                        policy,
                        (&mut prog, &mut leaves),
                        &mut budget,
                        0,
                        true,
                    );
                    if roots.contains(&id) {
                        prog.push(OsrCheckInsn::Publish(id));
                    }
                    ok
                }
            };
            if !ok {
                return Err("a skipped hoisted read or range predicate cannot be re-executed here");
            }
        }
        let frame = self.osr_entry_recheck_fail_frame(bci)?;
        Ok(Some((prog, leaves, frame)))
    }

    /// The frame an OSR entry's re-check exit resumes at `bci` (the header
    /// snapshot the seeding wrote), held to what that exit can publish: a
    /// register image when it names a register, and a frame the guard exit
    /// can resume in place. Shared by MX16-1's re-check program and item 4
    /// residual 2's field-proof checks.
    pub(in crate::ir_lower) fn osr_entry_recheck_fail_frame(&self, bci: usize) -> Result<FrameState, &'static str> {
        if self.deopt_stub_off.is_none() {
            return Err("no deopt stub for the entry's re-checks to leave through");
        }
        let frame = self.resolve_frame_state_for_bci(bci);
        let names_a_register = frame.locals.iter().chain(frame.stack.iter()).any(|v| {
            matches!(
                v,
                FrameValue::Register(_)
                    | FrameValue::RegisterLong(_)
                    | FrameValue::RegisterRef(_)
                    | FrameValue::XmmFloat(_)
                    | FrameValue::XmmDouble(_)
            )
        });
        if names_a_register && self.deopt_regs_base <= 0 {
            return Err("the entry's re-check exit has no register image");
        }
        let Ok(bci32) = u32::try_from(bci) else {
            return Err("the entry's bci does not fit u32");
        };
        let reason = DeoptReason::OsrExit;
        let probe = DeoptimizationPoint {
            native_offset: 0,
            bci: bci32,
            reason,
            action: DeoptAction::Reinterpret,
            speculation_id: crate::deopt::speculation_id(bci32, reason),
            frame_state: frame,
            semantics: ResumeSemantics::for_reason(reason),
        };
        if !crate::osr_exit::guard_exit_point_resumable(&probe) {
            return Err("the entry's re-check exit cannot resume the frame in place");
        }
        Ok(probe.frame_state)
    }

    /// Append to the re-check program the instructions that leave value `v` in
    /// RAX ([`Self::osr_entry_recheck_program`]); `false` for a shape it does
    /// not rebuild. `top` forces a re-checked node itself to be RE-EXECUTED
    /// even when a local happens to name it: the point is its checks.
    #[allow(clippy::too_many_arguments)]
    pub(in crate::ir_lower) fn osr_check_compile(
        &self,
        v: NodeId,
        locals: &[NodeId],
        policy: OsrRematPolicy,
        out: (&mut Vec<OsrCheckInsn>, &mut Vec<usize>),
        budget: &mut u32,
        depth: u32,
        top: bool,
    ) -> bool {
        let (prog, leaves) = out;
        if *budget == 0 || depth > OSR_REMAT_MAX_DEPTH {
            return false;
        }
        *budget -= 1;
        let Some(node) = self.graph.nodes.get(v as usize) else {
            return false;
        };
        if !top {
            if let Some(i) = locals.iter().position(|&l| l == v) {
                prog.push(OsrCheckInsn::Remat(OsrRematInsn::Local(i)));
                leaves.push(i);
                return true;
            }
        }
        let ty_of = |id: NodeId| self.graph.nodes.get(id as usize).map(|n| n.ty);
        match node.op {
            // zt t19/zb2: a reference element read in an OSR entry check would
            // be the raw inline load (`emit_gpr_array_elem_load`); under an
            // armed ZGC load barrier the entry is refused instead.
            Op::ArrayLoad(MemKind::Ref) if crate::x64::zgc_read_barrier_blocks_inline_fields() => {
                false
            }
            Op::ArrayLoad(kind)
                if node.inputs.len() == 4 && !matches!(kind, MemKind::Float | MemKind::Double) =>
            {
                let (base, idx) = (node.inputs[2], node.inputs[3]);
                if ty_of(base) != Some(IrType::Ref) || ty_of(idx) != Some(IrType::Int) {
                    return false;
                }
                if !self.osr_check_compile(
                    idx,
                    locals,
                    policy,
                    (&mut *prog, &mut *leaves),
                    budget,
                    depth + 1,
                    false,
                ) {
                    return false;
                }
                prog.push(OsrCheckInsn::Remat(OsrRematInsn::Push));
                if !self.osr_check_compile(
                    base,
                    locals,
                    policy,
                    (&mut *prog, &mut *leaves),
                    budget,
                    depth + 1,
                    false,
                ) {
                    return false;
                }
                prog.push(OsrCheckInsn::Remat(OsrRematInsn::PopRcx));
                prog.push(OsrCheckInsn::ArrayLoad(kind));
                true
            }
            Op::ArrayLength if node.inputs.len() == 3 => {
                let arr = node.inputs[2];
                if ty_of(arr) != Some(IrType::Ref)
                    || !self.osr_check_compile(
                        arr,
                        locals,
                        policy,
                        (&mut *prog, &mut *leaves),
                        budget,
                        depth + 1,
                        false,
                    )
                {
                    return false;
                }
                prog.push(OsrCheckInsn::ArrayLength);
                true
            }
            // JIT round 25 wave 9 (lane W9-5): a loop class predicate's test
            // of a seeded reference (`op_guard_is_class_predicate`); the
            // reference is a leaf (a named local or the null constant), and
            // the instruction is the remat's `ExactClassIs` (null and arrays
            // answer 0).
            //
            // W9-5 fix: the entry's snapshot may name the receiver through a
            // loop-carried phi that only ever carries it -- the builder makes
            // a phi for every live local at every loop header, and
            // `phi(region, r, self)` for an invariant one survives until GVN.
            // An INNER header's local is then such a phi over the outer
            // header's, never `r` itself, and the plain leaf lookup refused
            // the entry. Such a phi equals `r` on every path
            // ([`Self::osr_phi_only_carries`]), so the interpreter's local is
            // `r`'s value.
            Op::ExactClassIs { class_id } if class_id != 0 && node.inputs.len() == 1 => {
                let r = node.inputs[0];
                if ty_of(r) != Some(IrType::Ref) {
                    return false;
                }
                let carried = locals
                    .iter()
                    .position(|&l| l != NO_NODE && l != r && self.osr_phi_only_carries(l, r));
                match (locals.iter().position(|&l| l == r), carried) {
                    (None, Some(i)) => {
                        prog.push(OsrCheckInsn::Remat(OsrRematInsn::Local(i)));
                        leaves.push(i);
                    }
                    _ => {
                        if !self.osr_check_compile(
                            r,
                            locals,
                            policy,
                            (&mut *prog, &mut *leaves),
                            budget,
                            depth + 1,
                            false,
                        ) {
                            return false;
                        }
                    }
                }
                prog.push(OsrCheckInsn::Remat(OsrRematInsn::ExactClassIs { class_id }));
                true
            }
            Op::Cmp(cc) if node.ty == IrType::Int && node.inputs.len() == 2 => {
                let (a, b) = (node.inputs[0], node.inputs[1]);
                if ty_of(a) != Some(IrType::Int) || ty_of(b) != Some(IrType::Int) {
                    return false;
                }
                if !self.osr_check_compile(
                    b,
                    locals,
                    policy,
                    (&mut *prog, &mut *leaves),
                    budget,
                    depth + 1,
                    false,
                ) {
                    return false;
                }
                prog.push(OsrCheckInsn::Remat(OsrRematInsn::Push));
                if !self.osr_check_compile(
                    a,
                    locals,
                    policy,
                    (&mut *prog, &mut *leaves),
                    budget,
                    depth + 1,
                    false,
                ) {
                    return false;
                }
                prog.push(OsrCheckInsn::Remat(OsrRematInsn::PopRcx));
                prog.push(OsrCheckInsn::Remat(OsrRematInsn::Cmp {
                    setcc: cc.x64_cc() + 0x10,
                    wide: false,
                }));
                true
            }
            _ if node.ty == IrType::Ref => {
                if crate::ir::is_null_const(node) {
                    prog.push(OsrCheckInsn::Remat(OsrRematInsn::Imm(0)));
                    return true;
                }
                false
            }
            _ if matches!(node.ty, IrType::Int | IrType::Long) => {
                // Total integer arithmetic: the existing recomputation.
                let mut tmp = OsrRemat::default();
                if !self.osr_remat_compile(v, locals, policy, &mut tmp, budget, depth) {
                    return false;
                }
                prog.extend(tmp.prog.into_iter().map(OsrCheckInsn::Remat));
                leaves.extend(tmp.leaf_locals);
                true
            }
            _ => false,
        }
    }

    /// Emit an OSR entry stub's re-check program (round 16, lane matrix,
    /// MX16-1; [`Self::osr_entry_recheck_program`]) and its failure exit:
    /// every failing test jumps to one exit that restores RSP (a failure may
    /// leave `PUSH`ed words behind), loads an `OsrExit` point at the entry's
    /// bci naming `remat.fail_frame`, and jumps to the shared deopt stub. The
    /// passing path falls through to the rest of the stub. `false` only when a
    /// patch overflowed the buffer (the compile is discarded).
    pub(in crate::ir_lower) fn emit_osr_entry_rechecks(&mut self, bci: usize, remat: &OsrRemat) -> bool {
        let (Some(stub_off), Some(frame)) = (self.deopt_stub_off, remat.fail_frame.clone()) else {
            return false;
        };
        // The OSR stubs' one array-length displacement (`OSR_ARRAY_LENGTH_DISP8`).
        const LEN_DISP: i8 = OSR_ARRAY_LENGTH_DISP8;
        let mut fails: Vec<usize> = Vec::new();
        for &insn in &remat.checked {
            match insn {
                OsrCheckInsn::Remat(r) => self.emit_osr_remat_insn(r),
                OsrCheckInsn::ArrayLoad(kind) => {
                    // RAX = array, RCX = index. TEST RAX, RAX ; JZ fail
                    self.buf.emit(&[0x48, 0x85, 0xC0, 0x0F, 0x84]);
                    fails.push(self.buf.pos());
                    self.buf.emit(&[0, 0, 0, 0]);
                    // MOV ECX, ECX ; CMP ECX, [RAX + len] ; JAE fail
                    // Cast: the disp8 byte of a range-checked constant.
                    self.buf
                        .emit(&[0x89, 0xC9, 0x3B, 0x48, LEN_DISP as u8, 0x0F, 0x83]);
                    fails.push(self.buf.pos());
                    self.buf.emit(&[0, 0, 0, 0]);
                    self.emit_gpr_array_elem_load(kind);
                    // A narrow reference decode uses R11, which holds the
                    // interpreter's locals pointer for the reads that follow.
                    self.load_reg_from_frame(R11, self.phi_copy_scratch_slot_off);
                }
                OsrCheckInsn::ArrayLength => {
                    // TEST RAX, RAX ; JZ fail ; MOV EAX, [RAX + len]
                    self.buf.emit(&[0x48, 0x85, 0xC0, 0x0F, 0x84]);
                    fails.push(self.buf.pos());
                    self.buf.emit(&[0, 0, 0, 0]);
                    // Cast: the disp8 byte of a range-checked constant.
                    self.buf.emit(&[0x8B, 0x40, LEN_DISP as u8]);
                }
                OsrCheckInsn::FailIfZero => {
                    // TEST RAX, RAX ; JZ fail
                    self.buf.emit(&[0x48, 0x85, 0xC0, 0x0F, 0x84]);
                    fails.push(self.buf.pos());
                    self.buf.emit(&[0, 0, 0, 0]);
                }
                OsrCheckInsn::CompactReceiver => {
                    // TEST RAX, RAX ; JZ fail
                    self.buf.emit(&[0x48, 0x85, 0xC0, 0x0F, 0x84]);
                    fails.push(self.buf.pos());
                    self.buf.emit(&[0, 0, 0, 0]);
                    // TEST byte [RAX + flags], GC_FLAG_COMPACT ; JZ fail
                    self.buf.emit(&[0xF6, 0x80]);
                    self.buf
                        .emit(&(cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32).to_le_bytes());
                    self.buf.emit_byte(cratonvm_types::GC_FLAG_COMPACT);
                    self.buf.emit(&[0x0F, 0x84]);
                    fails.push(self.buf.pos());
                    self.buf.emit(&[0, 0, 0, 0]);
                }
                OsrCheckInsn::LayoutEpoch => {
                    // CMP epoch, baked ; JNE fail (no cell: nothing was proven).
                    if self.emit_layout_epoch_cmp() {
                        self.buf.emit(&[0x0F, 0x85]);
                        fails.push(self.buf.pos());
                        self.buf.emit(&[0, 0, 0, 0]);
                    }
                    // The non-RIP compare form used R11, which holds the
                    // interpreter's locals pointer for the reads that follow.
                    self.load_reg_from_frame(R11, self.phi_copy_scratch_slot_off);
                }
                OsrCheckInsn::Publish(v) => self.emit_osr_remat_insn(OsrRematInsn::Publish(v)),
            }
        }
        // JMP over the failure exit.
        self.buf.emit_byte(0xE9);
        let over = self.buf.pos();
        self.buf.emit(&[0, 0, 0, 0]);
        for p in fails {
            self.patch_rel32_to_here(p);
        }
        let fail_at = self.buf.pos();
        // LEA RSP, [RBP - frame_size]: the stack the prologue left.
        self.buf.emit(&[0x48, 0x8D, 0xA5]);
        self.buf.emit(&(-self.frame_size).to_le_bytes());
        // Cast: a bci that `osr_entry_recheck_program` already fitted to u32,
        // and an offset inside a bounded executable buffer.
        let bci32 = bci as u32;
        let reason = DeoptReason::OsrExit;
        let point = Box::new(DeoptimizationPoint {
            native_offset: fail_at as u32,
            bci: bci32,
            reason,
            action: DeoptAction::Reinterpret,
            speculation_id: crate::deopt::speculation_id(bci32, reason),
            frame_state: frame,
            semantics: ResumeSemantics::for_reason(reason),
        });
        let point_ptr = point.as_ref() as *const DeoptimizationPoint as u64;
        self.deopt_boxes.push(point);
        self.transfer_bcis.insert(bci);
        self.note_flat_transfer(bci, "osr_entry_rechecks", true);
        // MOV DEOPT_ARG0, imm64. Raw: no body register bookkeeping applies here.
        self.buf.emit(&[0x48, 0xB8 + DEOPT_ARG0]);
        self.buf.emit(&point_ptr.to_le_bytes());
        // JMP the shared deopt stub.
        self.buf.emit_byte(0xE9);
        let to_stub = self.buf.pos();
        self.buf.emit(&[0, 0, 0, 0]);
        // Cast: offsets inside a bounded executable buffer.
        let rel = stub_off as i32 - (to_stub as i32 + 4);
        if !Self::patch_or_bail(&mut self.buf, to_stub, rel) {
            return false;
        }
        self.patch_rel32_to_here(over);
        true
    }

    /// `b` into RCX and `a` into RAX: `b` first, parked with `PUSH`, so the
    /// evaluation of `a` may use RCX freely.
    #[allow(clippy::too_many_arguments)]
    pub(in crate::ir_lower) fn osr_remat_compile_pair(
        &self,
        a: NodeId,
        b: NodeId,
        locals: &[NodeId],
        policy: OsrRematPolicy,
        out: &mut OsrRemat,
        budget: &mut u32,
        depth: u32,
    ) -> bool {
        if !self.osr_remat_compile(b, locals, policy, out, budget, depth + 1) {
            return false;
        }
        out.prog.push(OsrRematInsn::Push);
        if !self.osr_remat_compile(a, locals, policy, out, budget, depth + 1) {
            return false;
        }
        out.prog.push(OsrRematInsn::PopRcx);
        true
    }

    /// Emit one instruction of an OSR entry stub's recomputation
    /// ([`OsrRematInsn`]).
    pub(in crate::ir_lower) fn emit_osr_remat_insn(&mut self, insn: OsrRematInsn) {
        match insn {
            OsrRematInsn::Local(i) => {
                // Cast: a JVM local index times 8; `max_locals` is u16.
                self.emit_mov_rax_from_r11_disp((i as i32) * 8);
            }
            OsrRematInsn::Imm(c) => self.emit_mov_rax_imm64(c),
            // PUSH RAX
            OsrRematInsn::Push => self.buf.emit_byte(0x50),
            // POP RCX
            OsrRematInsn::PopRcx => self.buf.emit_byte(0x59),
            OsrRematInsn::Alu { op, wide } => {
                let bytes: &[u8] = match op {
                    // ADD EAX, ECX
                    OsrRematAlu::Add => &[0x01, 0xC8],
                    // SUB EAX, ECX
                    OsrRematAlu::Sub => &[0x29, 0xC8],
                    // IMUL EAX, ECX
                    OsrRematAlu::Mul => &[0x0F, 0xAF, 0xC1],
                    // AND EAX, ECX
                    OsrRematAlu::And => &[0x21, 0xC8],
                    // OR EAX, ECX
                    OsrRematAlu::Or => &[0x09, 0xC8],
                    // XOR EAX, ECX
                    OsrRematAlu::Xor => &[0x31, 0xC8],
                };
                self.emit_w(wide, bytes);
            }
            OsrRematInsn::Shift { op, wide } => {
                // SHL / SAR / SHR EAX, CL
                let modrm = match op {
                    OsrRematShift::Shl => 0xE0,
                    OsrRematShift::Shr => 0xF8,
                    OsrRematShift::UShr => 0xE8,
                };
                self.emit_w(wide, &[0xD3, modrm]);
            }
            // NEG EAX
            OsrRematInsn::Neg { wide } => self.emit_w(wide, &[0xF7, 0xD8]),
            // MOVSXD RAX, EAX
            OsrRematInsn::I2L => self.buf.emit(&[0x48, 0x63, 0xC0]),
            // MOV EAX, EAX
            OsrRematInsn::L2I => self.buf.emit(&[0x89, 0xC0]),
            OsrRematInsn::DivImm { d, rem, wide } => {
                // MOV RCX, imm64. Raw, not `emit_mov_reg_imm64`: this is no
                // body block, and RCX bookkeeping (`note_rcx_written`) is the
                // body's carry accounting.
                self.buf.emit(&[0x48, 0xB9]);
                self.buf.emit(&d.to_le_bytes());
                // CDQ / CQO
                self.emit_w(wide, &[0x99]);
                // IDIV ECX / RCX
                self.emit_w(wide, &[0xF7, 0xF9]);
                if rem {
                    // MOV EAX, EDX
                    self.emit_w(wide, &[0x89, 0xD0]);
                }
            }
            // ── Round 15 wave 6 (lane irosr2, IRO-4) ──────────────────────
            OsrRematInsn::Cmp { setcc, wide } => {
                // CMP EAX, ECX (CMP RAX, RCX)
                self.emit_w(wide, &[0x39, 0xC8]);
                // SETcc AL ; MOVZX EAX, AL
                self.buf.emit(&[0x0F, setcc, 0xC0]);
                self.buf.emit(&[0x0F, 0xB6, 0xC0]);
            }
            OsrRematInsn::LCmp => {
                // Byte for byte the `Op::LCmp` arm.
                self.buf.emit(&[0x48, 0x39, 0xC8]); // CMP RAX, RCX
                self.buf.emit(&[0x0F, 0x9F, 0xC0]); // SETG AL
                self.buf.emit(&[0x0F, 0x9C, 0xC2]); // SETL DL
                self.buf.emit(&[0x0F, 0xB6, 0xC0]); // MOVZX EAX, AL
                self.buf.emit(&[0x0F, 0xB6, 0xD2]); // MOVZX EDX, DL
                self.buf.emit(&[0x29, 0xD0]); // SUB EAX, EDX
                self.buf.emit(&[0x48, 0x63, 0xC0]); // MOVSXD RAX, EAX
            }
            OsrRematInsn::ExactClassIs { class_id } => {
                // The `Op::ExactClassIs` arm's test with rel8 jumps (the
                // sequence is fixed-length, so the displacements are too):
                //
                //   +0  TEST RAX, RAX                 48 85 C0
                //   +3  JZ done (+24): RAX is the 0   74 18
                //   +5  TEST BYTE [RAX+kind], mask    F6 40 kk mm
                //   +9  JNE miss (+16)                75 10
                //  +11  CMP DWORD [RAX+0], class_id   81 78 00 id32
                //  +18  JNE miss (+7)                 75 07
                //  +20  MOV EAX, 1                    B8 01 00 00 00
                //  +25  JMP done (+2)                 EB 02
                //  +27  miss: XOR EAX, EAX            31 C0
                //  +29  done:
                const _: () = assert!(cratonvm_types::KIND_TAGS_BYTE_OFFSET <= 127);
                self.buf.emit(&[0x48, 0x85, 0xC0, 0x74, 0x18]);
                self.buf.emit(&[
                    0xF6,
                    0x40,
                    cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8,
                    cratonvm_types::KIND_TAG_BYTE_MASK,
                    0x75,
                    0x10,
                ]);
                self.buf.emit(&[0x81, 0x78, 0x00]);
                self.buf.emit(&class_id.to_le_bytes());
                self.buf.emit(&[0x75, 0x07]);
                self.buf.emit(&[0xB8, 0x01, 0x00, 0x00, 0x00, 0xEB, 0x02]);
                self.buf.emit(&[0x31, 0xC0]);
            }
            // JIT round 19 (lane osr19): byte for byte the `Op::ClassWordOf` arm.
            OsrRematInsn::ClassWordOf => self.buf.emit(&class_word_of_bytes()),
            // JIT round 19 wave 2 (lane irops2, IROPS19-7): the
            // `emit_inline_getstatic` read, RAX only (R11 holds the locals).
            OsrRematInsn::StaticLoad {
                base_cell,
                disp,
                wide,
            } => {
                // Cast: the baked cell address, as an imm64 bit pattern.
                self.emit_mov_rax_imm64(base_cell as i64);
                self.buf.emit(&[0x48, 0x8B, 0x80]); // MOV RAX, [RAX + 0]
                self.buf.emit(&0i32.to_le_bytes());
                if wide {
                    self.buf.emit(&[0x48, 0x8B, 0x80]); // MOV RAX, [RAX + disp32]
                } else {
                    self.buf.emit(&[0x48, 0x63, 0x80]); // MOVSXD RAX, [RAX + disp32]
                }
                self.buf.emit(&disp.to_le_bytes());
            }
            // JIT round 19 wave 4 (lane osr4, IROPS2-4): the `F` width of
            // `emit_inline_getstatic`.
            OsrRematInsn::StaticLoadF32 { base_cell, disp } => {
                // Cast: the baked cell address, as an imm64 bit pattern.
                self.emit_mov_rax_imm64(base_cell as i64);
                self.buf.emit(&[0x48, 0x8B, 0x80]); // MOV RAX, [RAX + 0]
                self.buf.emit(&0i32.to_le_bytes());
                self.buf.emit(&[0x8B, 0x80]); // MOV EAX, [RAX + disp32] (zero-extends)
                self.buf.emit(&disp.to_le_bytes());
            }
            OsrRematInsn::ArrayLength => {
                const LEN_DISP: i8 = OSR_ARRAY_LENGTH_DISP8;
                // TEST RAX, RAX ; JZ +3 (over the load: a null answers 0)
                self.buf.emit(&[0x48, 0x85, 0xC0, 0x74, 0x03]);
                // MOV EAX, [RAX + ARRAY_LENGTH_OFFSET]
                // Cast: the disp8 byte of a range-checked constant.
                self.buf.emit(&[0x8B, 0x40, LEN_DISP as u8]);
            }
            OsrRematInsn::Publish(v) => {
                if !self.home_dropped.get(v as usize).copied().unwrap_or(false) {
                    if let Some(off) = self.node_slot.get(v as usize).copied().flatten() {
                        // Cast: a frame offset inside this method's own frame.
                        self.store_abi_reg(RAX, off.get() as i32);
                        // JIT round 19 wave 4 (lane osr4, IROPS2-4): an FP
                        // root (a re-read `float` / `double` static) reaches
                        // its XMM register from the word just written, as
                        // `emit_inline_getstatic` publishes it. No-op for
                        // every other type and for an FP value with no
                        // register.
                        let fp = self.graph.nodes.get(v as usize).map(|n| n.ty);
                        if matches!(fp, Some(IrType::Float | IrType::Double)) {
                            // Cast: as above.
                            self.publish_fp_from_slot(
                                v,
                                off.get() as i32,
                                fp == Some(IrType::Double),
                            );
                        }
                    }
                }
                if let Some(dst) = self.assigned_gpr(v) {
                    self.emit_mov_reg_reg64(dst, RAX);
                    self.mark_gp_reg_live(v);
                }
            }
        }
    }

    /// Emit the single shared deopt stub (if any guard jumps to it) and patch
    /// every guard's `JMP` to it. The stub expects the failing guard's
    /// `DeoptimizationPoint` pointer already in `DEOPT_ARG0`; it loads `rbp`
    /// into `DEOPT_ARG1`, calls `ir_deopt_entry`, and returns its result (the
    /// `i64::MIN` deopt sentinel) via the normal epilogue.
    pub(in crate::ir_lower) fn emit_deopt_stub(&mut self) {
        if self.deopt_stub_patches.is_empty() {
            return;
        }
        let stub_off = self.buf.pos();
        self.deopt_stub_off = Some(stub_off);
        // ── Spill the register file, BEFORE anything here clobbers one ──
        //
        // Every guard reaches this stub by a `JMP`, so on entry the registers
        // still hold their trapping-instant values — with ONE exception, which
        // is why naming is restricted to `IR_LOWER_LS_GPRS`: the guard site
        // already loaded the `DeoptimizationPoint` pointer into `DEOPT_ARG0`
        // (RCX on Windows, RDI on System V) before jumping, so THAT register's
        // spilled word is the pointer and not the trapping value. Neither
        // register is in the residency file, and `deopt_nameable` is built from
        // that file, so no frame value can name one.
        //
        // `gpr[r]` at `[rbp - (base - 8r)]` and `xmm[n]` at
        // `[rbp - (base - 128 - 8n)]` — the `#[repr(C)]` field order of
        // `SavedRegisters`, and the same arithmetic the single-pass backend's
        // stub uses.
        // Round 16 wave 11 (lane frames2): exactly the words the frame
        // reserved -- all 32 for the full image, `gpr[0..words)` and no XMM
        // half for a sized one (`size_deopt_image`). Every register a frame
        // state of this body can name is among them (`deopt_image_holds_gpr`),
        // and `ir_deopt_entry` reads only the words a frame state names.
        if self.deopt_regs_base > 0 {
            let gpr_words = (self.deopt_regs_bytes / 8).clamp(0, 16);
            for r in 0u8..16 {
                if i32::from(r) >= gpr_words {
                    break;
                }
                // Cast: a 0..16 register index into the frame displacement.
                self.store_abi_reg(r, self.deopt_regs_base - (r as i32) * 8);
            }
            if self.deopt_regs_bytes >= DEOPT_IMAGE_FULL_BYTES {
                for n in 0u8..16 {
                    // Cast: as above. MOVSD writes the low 8 bytes, which is
                    // exactly the `u64` the `xmm` half of the struct holds.
                    self.emit_movsd_frame_from_xmm(self.deopt_regs_base - 128 - (n as i32) * 8, n);
                }
            }
        }
        // mov arg1, rbp
        self.emit_mov_reg_rbp(DEOPT_ARG1);
        // arg2 = &SavedRegisters, or NULL when no region was reserved — which
        // is what `ir_deopt_entry` reads as "this frame has no register image",
        // giving it back the default-zeros behaviour it had before the region
        // existed.
        if self.deopt_regs_base > 0 {
            self.lea_reg_from_frame(DEOPT_ARG2, self.deopt_regs_base);
        } else {
            self.emit_mov_reg_imm64(DEOPT_ARG2, 0);
        }
        // mov rax, ir_deopt_entry ; call rax
        //
        // Interpreter round i2 wave 60, lane L2: under
        // `CRATONVM_DEOPT_STASH_NAMES_TRAPPING_BODY`, a compile that reserved
        // an id bakes it as a 4th argument and calls the naming entry, which
        // stashes the trapping body beside the frame (one more immediate per
        // stub tail, not per site). Off: the bytes above and below, unchanged.
        let id = self.compile_id.id();
        let fn_addr = if id != 0 && crate::deopt::stash_names_trapping_body_enabled() {
            self.emit_mov_reg_imm64(DEOPT_ARG3, u64::from(id));
            crate::deopt::ir_deopt_entry_named as *const () as u64
        } else {
            ir_deopt_entry as *const () as u64
        };
        self.emit_mov_reg_imm64(RAX, fn_addr);
        self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
                                      // Epilogue (RAX holds the sentinel returned by ir_deopt_entry).
                                      //
                                      // This stub inlines the teardown rather
                                      // than calling `emit_epilogue` (whose
                                      // shadow `top` restore it takes only
                                      // under a frame block, below;
                                      // `ir_deopt_entry` itself does not touch
                                      // the shadow stack -- gcd d2/f review,
                                      // this comment used to say it had
                                      // already unwound it), so the
                                      // callee-saved restore has to be repeated
                                      // here. Omitting it returns to the caller
                                      // with XMM6/XMM7 holding this frame's
                                      // values, on the one exit that is hardest
                                      // to notice.
                                      //
                                      // Except under a frame block, which it
                                      // must retract: nothing on this path
                                      // unwinds `top` (only the VM boundary's
                                      // `restore_jit_thread` heals it), and a
                                      // leaked block is a set of tagged
                                      // addresses into THIS frame, which is
                                      // dead once it returns. A leaked value
                                      // push is a stale oop; a leaked block
                                      // names whatever the caller puts in this
                                      // stack memory next. R10/R11 only; RAX
                                      // (the sentinel) survives.
        if !self.frame_block.is_empty() {
            self.emit_shadow_savetop_restore();
        }
        self.emit_callee_saved_restore();
        self.buf.emit(&[0x48, 0x81, 0xC4]); // add rsp, frame_size
        self.buf.emit(&self.frame_size.to_le_bytes());
        self.buf.emit_byte(0x5D); // pop rbp
        self.buf.emit_byte(0xC3); // ret

        let patches = std::mem::take(&mut self.deopt_stub_patches);
        for p in patches {
            let rel = stub_off as i32 - (p as i32 + 4);
            if !Self::patch_or_bail(&mut self.buf, p, rel) {
                break;
            }
        }
    }

    /// Patch one stub-relative `rel32`, tolerating an overflowed buffer.
    ///
    /// Returns `false` once the patch could not be applied.
    /// [`ExecutableBuffer::try_patch_i32`] reports that ONLY by marking the
    /// buffer overflowed, and an overflowed buffer makes `lower_inner`
    /// discard the whole `CompiledMethod` and fall back to the single-pass
    /// backend -- so the error is genuinely ignorable here, exactly as
    /// `try_patch_i32`'s own contract says ("the caller may ignore the `Err`
    /// and rely on that bail") and as `patch_rel32_to_here` already does.
    ///
    /// It must NOT be an `expect`. `emit_deopt_stub` / `emit_call_exc_stub`
    /// run BEFORE that bail-out, on a VM thread with no unwinding catch, so a
    /// body that outgrew its estimated buffer aborted the whole process
    /// instead of falling back. Real repro (2026-07-27): parsing Groovy
    /// source through `GroovyClassLoader.parseClass` panicked with
    /// `call-exc JE patch in-bounds: PatchFailed { kind: "i32", offset: 4125 }`
    /// -- offset == the emitted length, i.e. the branch's own rel32
    /// placeholder had already been dropped by the sticky-overflow `emit` --
    /// and took the VM down with `fatal runtime error: failed to initiate
    /// panic`. The identical run under `--nojit` is clean.
    pub(in crate::ir_lower) fn patch_or_bail(buf: &mut ExecutableBuffer, offset: usize, rel: i32) -> bool {
        if buf.try_patch_i32(offset, rel).is_err() {
            debug_assert!(
                buf.overflowed(),
                "try_patch_i32 must mark the buffer overflowed when it fails, \
                 so that lower_inner discards this compile"
            );
            return false;
        }
        true
    }

    /// Emit the slow path of every poll [`Self::emit_safepoint_poll`] outlined,
    /// after the body, each ending in a `JMP` back to its own poll site.
    ///
    /// Byte-for-byte the same sequence the inline arm emits — optional oop map,
    /// `MOV RAX, <slow path>` / `CALL RAX`, optional shadow reload — plus the
    /// five-byte return jump. It is deferrable because the per-site inputs —
    /// `spill_high_water`, `defined_nodes` and `cur_bci` — were recorded at the
    /// site and are reinstated around the map here (round 9: only the first
    /// used to be, see `outlined_polls`), and the map is keyed by the id the
    /// slow path stores, which is correct wherever that call ends up.
    ///
    /// A no-op when nothing was outlined, which is every compile with
    /// `CRATONVM_JIT_IR_ENTRY_POLL_OUTLINE=0` (only the entry poll outlines).
    pub(in crate::ir_lower) fn emit_outlined_polls(&mut self) {
        for (
            slow_patch,
            resume,
            live_hi,
            defined_at_poll,
            bci_at_poll,
            mode_exit,
            pc_at_poll,
            back_edge_block,
            slow_clears,
            ref_reloads,
            owners_at_poll,
        ) in std::mem::take(&mut self.outlined_polls)
        {
            // The byte poll's `JNZ` lands here; a page-form poll's fault
            // recovers here (j27 wave 5, `ir_page_poll`).
            self.bind_outlined_poll_slow(slow_patch);
            if let Some(stw_patch) = self.outlined_poll_stw_patches.remove(&slow_patch) {
                let rel = self.buf.pos() as i32 - (stw_patch as i32 + 4);
                Self::patch_or_bail(&mut self.buf, stw_patch, rel);
            }
            // As the inline arm: a deferred volatile-store fence is paid on
            // the way into the safepoint (`ensure_fence_sink_plan`).
            if self.fence_sink_on() {
                self.buf.emit(&[0xF0, 0x83, 0x04, 0x24, 0x00]); // LOCK ADD DWORD [RSP], 0
            }
            // FUZZ21-3: as the inline arm (back-edge dead homes poisoned,
            // lazy words zeroed).
            for off in slow_clears {
                if back_edge_block.is_some() {
                    self.emit_dead_home_clear(off);
                } else {
                    self.emit_zero_frame_slot(off);
                }
            }
            // JIT round 25 wave 6 (lane W6-B): the site-named values the
            // poll's frame state names, as the inline arm stores them. The
            // registers are what they were at the poll: its `JNZ` lands here.
            if let Some(spills) = self.outlined_poll_spills.remove(&slow_patch) {
                for (reg, off) in spills {
                    self.store_abi_reg_raw(reg, off);
                    self.poll_slow_spills_emitted += 1;
                }
            }
            // The site's high-water mark, defined set and bci, not the
            // method's final ones: the map has to describe the frame as it
            // stood AT THE POLL (see the `outlined_polls` field).
            let saved = self.spill_high_water;
            self.spill_high_water = live_hi;
            let saved_defined = std::mem::replace(&mut self.defined_nodes, defined_at_poll);
            let saved_bci = std::mem::replace(&mut self.cur_bci, bci_at_poll);
            // Round 16 (lane codegen): and the site's node pc, which keys the
            // map's inline-frame row (`ir_outlined_poll_node_pc_enabled`).
            let saved_pc = true
                .then(|| std::mem::replace(&mut self.cur_node_pc, pc_at_poll));
            // Publish from the restored poll state and keep that exact map
            // when returning to the emitter's end-of-body state.
            let saved_owners = std::mem::replace(&mut self.gp_reg_owner, owners_at_poll);
            let mut map = self.emit_poll_safepoint_map(back_edge_block);
            self.gp_reg_owner = saved_owners;
            self.spill_high_water = saved;
            self.defined_nodes = saved_defined;
            self.cur_bci = saved_bci;
            if let Some(pc) = saved_pc {
                self.cur_node_pc = pc;
            }
            self.emit_poll_body_id_arg();
            self.emit_mov_reg_imm64(CALL_ARG_REGS[0], self.safepoint_flag_addr as u64);
            self.emit_mov_reg_imm64(RAX, self.safepoint_slow_path as u64);
            self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
            if map.is_some() {
                self.anchor_safepoint_return(map.as_mut(), self.buf.pos());
                self.emit_shadow_reload();
            }
            // A back-edge poll's FP reloads (CG16P-1), then its mode exit: the
            // inline arm's sequence. The reloads write XMMs only, so RAX (the
            // verdict the mode exit tests) survives them.
            if let Some(block) = back_edge_block {
                self.emit_poll_fp_reloads(block);
            }
            self.emit_poll_ref_phi_reloads(&ref_reloads, map.as_ref());
            if let Some(exit) = mode_exit {
                self.emit_poll_mode_exit(exit);
            }
            self.buf.emit_byte(0xE9); // JMP back to the instruction after the poll
            let back_patch = self.buf.pos();
            self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
            let back_rel = resume as i32 - (back_patch as i32 + 4);
            Self::patch_or_bail(&mut self.buf, back_patch, back_rel);
        }
    }

    /// Gap B: emit the shared call-exception bail stub (if any `Op::Call`
    /// emitted a sentinel check) and patch every dispatch site's `JO` (round 9
    /// wave 2: `CMP RAX, 1; JO`, formerly `CMP RAX, R10; JE`) to it. On
    /// entry `RAX` already holds the `i64::MIN` sentinel the helper returned when
    /// the callee threw; the stub stamps this method's own throw-site bci, then
    /// runs the epilogue, returning the sentinel so the VM's post-JIT path takes
    /// the pending exception (the same protocol the single-pass backend uses).
    ///
    /// # One stub per DISTINCT throw-site bci, not one shared stub
    ///
    /// This is the IR half of RBC.6, and it was the gap cov-07's closeout doc
    /// flagged and did not own (`offsetdatetimetest-zoneddatetimetest-athrow-ir-sneaky-throw-swallowed-20260804-FIXED.md`). `JitSignals::athrow_bci` is consumed by `execute_jit_call`
    /// as *this* method's throw site and range-tested against `[start_pc,
    /// end_pc)` of every entry in this method's own exception table. Until this
    /// stub stamped it, that field still held whatever the CALLEE's compiled
    /// `athrow` lowering left there — a pc in a different method, which lands
    /// inside this method's protected region only by coincidence.
    ///
    /// A typed handler survives that coincidence often enough to look healthy
    /// (it is also matched on exception class), but a catch-all (`catch_type ==
    /// 0`, i.e. a javac `finally`) has nothing else to match on: a foreign bci
    /// outside the region silently drops it and the `finally` never runs.
    /// Witness: `FinallyBalanceProbe.java` — `try { n++; thrower(); } finally {
    /// n--; }` leaked one count per throw under the single-pass JIT until RBC.6
    /// fixed it there (`x64/deopt_stubs.rs`, `emit_exception_check_stub`), and
    /// leaked again once cov-07 let the same method shape reach THIS tier.
    ///
    /// Grouping by bci keeps the cost at one small pad per distinct fallible
    /// bytecode rather than one per branch site.
    ///
    /// `Op::Throw`'s own exit already passes its bci to `jit_throw_exception`,
    /// which stamps it; re-stamping the same value here is a no-op for it and
    /// keeps the stub's contract uniform — every exit through it leaves an
    /// `athrow_bci` belonging to THIS method.
    pub(in crate::ir_lower) fn emit_call_exc_stub(&mut self) {
        // Round 15 wave 1 (lane sync, SS8-2): a self-locking body's own
        // monitor edges, which release nothing.
        self.emit_self_lock_unheld_exc_stubs();
        if self.call_exc_patches.is_empty() {
            return;
        }
        let patches = std::mem::take(&mut self.call_exc_patches);
        let mut stub_by_bci: HashMap<usize, usize> = HashMap::new();
        for (patch, bci) in patches {
            let stub_off = match stub_by_bci.get(&bci) {
                Some(&off) => off,
                None => {
                    let off = self.buf.pos();
                    stub_by_bci.insert(bci, off);
                    // Stamp this method's own throw-site bci over whatever the
                    // callee left behind. The argument registers are dead here —
                    // the method is about to return — and RAX is reloaded with
                    // the sentinel afterwards because the helper call clobbers
                    // it.
                    if self.set_throw_bci != 0 {
                        self.emit_mov_reg_imm64(CALL_ARG_REGS[0], self.throw_stamp_word(bci as u64));
                        self.emit_mov_reg_imm64(RAX, self.set_throw_bci as u64);
                        self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
                    }
                    // Round 15 wave 1 (lane sync, SS8-2): the exception (or a
                    // callee's bare trap) leaves a self-locking body here, so
                    // the method monitor is released first -- JVMS 2.11.10,
                    // and the single-pass twin's `emit_self_lock_release_at_exit`.
                    if self.self_lock.is_some() {
                        self.emit_self_lock_release_at_exit();
                    }
                    self.emit_mov_reg_imm64(RAX, i64::MIN as u64);
                    // Shares the method epilogue so the shadow `top` watermark
                    // is restored here too — this is the path a callee's
                    // `i64::MIN` exception/deopt sentinel takes, skipping the
                    // call site's matching shadow reload.
                    self.emit_epilogue();
                    off
                }
            };
            let rel = stub_off as i32 - (patch as i32 + 4);
            if !Self::patch_or_bail(&mut self.buf, patch, rel) {
                break;
            }
        }
    }

    /// Round 15 wave 1 (lane sync, SS8-2): one stub per edge of
    /// [`Self::self_lock_unheld_exc_patches`] -- [`Self::emit_call_exc_stub`]'s
    /// sequence without the method-monitor release: stamp the bci, return
    /// the sentinel. Empty for every other body.
    pub(in crate::ir_lower) fn emit_self_lock_unheld_exc_stubs(&mut self) {
        for (patch, bci) in std::mem::take(&mut self.self_lock_unheld_exc_patches) {
            let off = self.buf.pos();
            if self.set_throw_bci != 0 {
                self.emit_mov_reg_imm64(CALL_ARG_REGS[0], self.throw_stamp_word(bci as u64));
                self.emit_mov_reg_imm64(RAX, self.set_throw_bci as u64);
                self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
            }
            self.emit_mov_reg_imm64(RAX, i64::MIN as u64);
            self.emit_epilogue();
            // Cast: code offsets of one method, far below `i32::MAX`.
            let rel = off as i32 - (patch as i32 + 4);
            if !Self::patch_or_bail(&mut self.buf, patch, rel) {
                break;
            }
        }
    }

    /// Round 15 wave 1 (lane sync, SS8-2): the frame word a self-locking body
    /// releases its method monitor from on an exceptional exit -- the
    /// receiver's prologue home `[rbp - 8]` (`Op::Param(0)`'s, as
    /// [`Self::monitor_object_home`] computes it). The prologue writes it,
    /// nothing else does (an admitted method never stores local 0, and the
    /// dead-home clears touch only the spill band), and every safepoint map
    /// republishes it ([`Self::ref_param_homes`]), so a moving collection
    /// keeps it current. `None` when the graph's monitor is not that
    /// reference parameter.
    pub(in crate::ir_lower) fn self_lock_obj_home(&self) -> Option<i32> {
        let lock = self.self_lock.as_ref()?;
        let node = self.graph.nodes.get(lock.monitor as usize)?;
        if !matches!(node.op, Op::Param(0)) || node.ty != IrType::Ref {
            return None;
        }
        const HOME: i16 = 8;
        self.ref_param_homes
            .contains(&HOME)
            .then_some(i32::from(HOME))
    }

    /// JIT round 19 proposal wave 8, lane gc8 (GC719-1): the frame words --
    /// positive RBP offsets -- of the lock objects of the `synchronized`
    /// blocks the frame holds at the safepoint being recorded (the current
    /// node's), outermost first, for `CompiledMethod::ir_monitor_homes`.
    /// `slots` is the map that safepoint records.
    ///
    /// The frame state is the one a deopt there would describe: the node's
    /// own snapshot when it names one at the site's method bci, else EVERY
    /// method-frame snapshot at that bci, which must agree on the monitor
    /// stack (a disagreement answers `None`). Inside a spliced callee the
    /// site's bci is the enclosing invoke's ([`Self::resume_bci`]), whose
    /// snapshot names exactly the blocks THIS method holds -- a splice takes
    /// no bytecode monitor (`ir.rs`), so an inlined level holds none to omit.
    ///
    /// Skipped: an object no live monitor op names (escape analysis elided
    /// every lock on it; the thread does not hold it), an entry nested-lock
    /// elision deleted (an outer entry on the same object is the hold), and
    /// the method monitor of a self-locking body (`self_lock_hold` names it).
    /// Every other lock must have its reference home ([`Self::monitor_object_home`])
    /// among `slots`, or the whole point answers `None`: the reader reads a
    /// home only where the active map names it, and all or nothing.
    ///
    /// Compile-time bookkeeping only; the VM compares what it reads there with
    /// the thread's lock stack and never makes an object of it.
    pub(in crate::ir_lower) fn safepoint_monitor_homes(&self, slots: &[i16]) -> Option<Box<[i32]>> {
        if self.live_monitor_operands.is_empty() || slots.is_empty() {
            return None;
        }
        let bci = self.resume_bci(self.cur_bci);
        let graph = self.graph;
        let own = self
            .cur_node_frame
            .and_then(|si| graph.safepoints.get(si as usize))
            .filter(|sp| sp.bci == bci && !sp.is_splice_state());
        let sp = match own {
            Some(sp) => sp,
            None => {
                let first = self
                    .safepoint_first_by_bci
                    .get(&bci)
                    .and_then(|&si| graph.safepoints.get(si))?;
                let agree = graph
                    .safepoints
                    .iter()
                    .filter(|sp| sp.bci == bci && !sp.is_splice_state())
                    .all(|sp| {
                        sp.monitors == first.monitors
                            && sp.sb_virtual_builders == first.sb_virtual_builders
                            && sp.monitor_elided_mask == first.monitor_elided_mask
                    });
                if !agree {
                    return None;
                }
                first
            }
        };
        if sp.monitors.is_empty() {
            return None;
        }
        let method_monitor = self.self_lock.as_ref().map(|lock| lock.monitor);
        let mut homes: Vec<i32> = Vec::with_capacity(sp.monitors.len());
        let mut seen: Vec<NodeId> = Vec::with_capacity(sp.monitors.len());
        for (k, &obj) in sp.monitors.iter().enumerate() {
            if sp.monitor_elided(k)
                || Some(obj) == method_monitor
                || !self.live_monitor_operands.contains(&obj)
                || seen.contains(&obj)
            {
                continue;
            }
            seen.push(obj);
            let Some(FrameValue::StackSlotRef(neg)) = self.monitor_object_home(obj) else {
                return None;
            };
            let off = neg.checked_neg().filter(|&o| o > 0 && o % 8 == 0)?;
            let named = i16::try_from(off).is_ok_and(|o| slots.contains(&o));
            if !named {
                return None;
            }
            homes.push(off);
        }
        (!homes.is_empty()).then(|| homes.into_boxed_slice())
    }

    /// Round 15 wave 3 (lane sync2, proposal S15-3): the VM's GC-maintained
    /// mirror slot a `static synchronized` self-locking body locks
    /// (`ir::IrBuilder::set_method_self_lock_mirror`), or `None` -- an
    /// instance body, or no self-lock. Recognised by shape: the monitor is the
    /// builder's `ConstClass { cp_idx: ir::SELF_LOCK_MIRROR_CP_IDX, .. }` with
    /// a slot, in a graph that claims no receiver. The word holds the mirror
    /// at every read (the VM fills it from a live mirror before any compile
    /// sees it, never clears it, and the collector rewrites it in place), so
    /// the body's `ConstClass` arm reads it and every exceptional exit re-reads
    /// it to release ([`Self::emit_self_lock_release_at_exit`]).
    pub(in crate::ir_lower) fn self_lock_mirror_slot(&self) -> Option<usize> {
        let lock = self.self_lock.as_ref()?;
        if self.graph.receiver_param.is_some() {
            return None;
        }
        match self.graph.nodes.get(lock.monitor as usize).map(|n| &n.op) {
            Some(&Op::ConstClass {
                holder_class_id,
                cp_idx,
                slot_addr,
            }) if holder_class_id != 0
                && cp_idx == crate::ir::SELF_LOCK_MIRROR_CP_IDX
                && slot_addr != 0 =>
            {
                Some(slot_addr)
            }
            _ => None,
        }
    }

    /// Round 15 wave 3 (lane sync2, S15-3): [`Self::emit_self_lock_release_at_exit`]
    /// for a `static synchronized` body: the exit helper on the class mirror
    /// re-read from its slot ([`Self::self_lock_mirror_slot`]) straight into
    /// the helper's object argument -- the body's own `ConstClass` home is not
    /// trusted at an exit (it may be dead, dropped or recoloured there). The
    /// sequence is `runtime_lowering::emit_monitor_stub`'s with the object
    /// load swapped; no oop map, for the instance twin's reason.
    pub(in crate::ir_lower) fn emit_self_lock_mirror_release_at_exit(&mut self, slot_addr: usize) {
        if self.context_slot_off == 0 || self.monitor_exit == 0 {
            self.latch_bailout(Bailout::new(BailoutReason::Internal(
                "ir_lower: a static self-locking body's exceptional exit cannot release its monitor",
            )));
            return;
        }
        self.load_context(CALL_ARG_REGS[0]); // vm_ptr
                                             // Cast: a VM slot address as the imm64 operand.
        self.emit_mov_reg_imm64(RAX, slot_addr as u64);
        self.buf.emit(&[0x48, 0x8B, 0x00]); // MOV RAX, [RAX]
        self.emit_mov_reg_reg64_raw(CALL_ARG_REGS[1], RAX); // obj_ptr
        self.emit_mov_reg_imm64(RAX, self.monitor_exit as u64);
        self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
        crate::runtime_lowering::emit_post_call_frame_republish(&mut self.buf, self.frame_record);
        self.emit_identity_after_shared_stub();
    }

    /// Round 15 wave 1 (lane sync, SS8-2): release a self-locking body's
    /// method monitor on an exceptional exit, inside
    /// [`Self::emit_call_exc_stub`]'s per-bci stub: the exit helper on the
    /// receiver's home word ([`Self::self_lock_obj_home`]). No oop map, as on
    /// the single-pass twin (`x64/frames.rs` `emit_self_lock_release_at_exit`):
    /// the exit helper cannot reach a safepoint on a monitor this thread owns,
    /// and the frame is torn down right after. Its result is ignored: it
    /// fails only for a monitor this thread does not hold, and it does not
    /// clear the pending exception. RAX is clobbered; the caller re-loads the
    /// sentinel. An unwired piece refuses the compile.
    ///
    /// Round 15 wave 3 (lane sync2): a `static synchronized` body releases its
    /// class mirror instead ([`Self::emit_self_lock_mirror_release_at_exit`],
    /// proposal S15-3); an instance body tries the inline thin/inflated unlock
    /// on the home word first and calls the helper only when it declines
    /// (proposal S15-5, `CRATONVM_JIT_IR_SELF_LOCK_INLINE_EXIT_RELEASE`): the
    /// `Op::MonitorExit` arm's and `emit_sync_exit_then_throw`'s sequence. It
    /// clobbers only scratch registers, and nothing is live in this stub.
    pub(in crate::ir_lower) fn emit_self_lock_release_at_exit(&mut self) {
        if let Some(slot_addr) = self.self_lock_mirror_slot() {
            self.emit_self_lock_mirror_release_at_exit(slot_addr);
            return;
        }
        let home = self.self_lock_obj_home();
        let Some(home) = home.filter(|_| self.context_slot_off != 0 && self.monitor_exit != 0)
        else {
            self.latch_bailout(Bailout::new(BailoutReason::Internal(
                "ir_lower: a self-locking body's exceptional exit cannot release its monitor",
            )));
            return;
        };
        let inline_done = {
            self.emit_monitor_fast_path(false, home)
        };
        crate::runtime_lowering::emit_monitor_stub(
            &mut self.buf,
            self.context_slot_off,
            home,
            self.monitor_exit,
            self.frame_record,
        );
        self.emit_identity_after_shared_stub();
        if let Some(done) = inline_done {
            self.patch_rel32_to_here(done);
        }
    }

    /// Round 15 wave 1 (lane sync, SS8-2; step 4 of the design in the round 14
    /// wave 4 section of
    /// `r13w4-sync2-self-locking-bodies-with-deopt-exits-FIXED-20260930.md`): why a
    /// SELF-LOCKING body must not be published, or `None` for any other body
    /// and for one whose every exit is closed. The optimizing twin of the
    /// single-pass `x64/driver.rs` `self_lock_exits_are_closed`, and it FAILS
    /// CLOSED: a missed exit would leak the monitor (a hang in another
    /// thread), so everything below is required, not assumed.
    ///
    /// * The monitor is the receiver's published prologue home, and the
    ///   compile has no protected range (a handler frame would not hand over).
    /// * The enter survived the optimizer, sits in the entry block -- which
    ///   no edge re-enters -- and nothing that can transfer is scheduled ahead
    ///   of it: every other block runs after the entry block completes.
    /// * Every surviving exit is a live `MonitorExit` of the monitor in a block
    ///   whose terminator is an `Op::Return`, with nothing that can transfer
    ///   scheduled after it; every live `Op::Return` has exactly one.
    /// * Every exceptional exit releases ([`Self::emit_call_exc_stub`]) and
    ///   every deopt exit keeps the hold, so every frame a deopt can publish
    ///   -- the boxes the stubs bake and the by-bci points -- must hand it
    ///   over: flat (no inlined caller), exactly one monitor, one taken level
    ///   (`relock == false`) located as a reference, and local 0 located as a
    ///   reference (`deopt::handed_method_monitor_index` compares the two), and
    ///   only while `CRATONVM_JIT_SELF_LOCK_DEOPT_HANDOVER` is on.
    ///
    /// Round 15 wave 4 (lane sync3, SY2-6): the refusal comes with its cause
    /// ([`SelfLockRefusal`]), counted on its own census row.
    pub(in crate::ir_lower) fn self_lock_refusal(
        &self,
        boxes: &[Box<DeoptimizationPoint>],
        points: &[DeoptimizationPoint],
    ) -> Option<(SelfLockRefusal, String)> {
        let lock = self.self_lock.as_ref()?;
        // Round 15 wave 3 (lane sync2, S15-3): or, for a `static
        // synchronized` body, its class mirror read from the VM's slot.
        let mirror_slot = self.self_lock_mirror_slot();
        if self.self_lock_obj_home().is_none() && mirror_slot.is_none() {
            return Some((
                SelfLockRefusal::MonitorHome,
                "the monitor is neither the receiver's published prologue home nor a class \
                 mirror slot"
                    .to_string(),
            ));
        }
        if !self.protected_ranges.is_empty() {
            return Some((
                SelfLockRefusal::ProtectedRange,
                "the compile has a protected range".to_string(),
            ));
        }
        let graph = self.graph;
        let schedule = self.schedule;
        let is_monitor_op = |id: NodeId, enter: bool| {
            graph.nodes.get(id as usize).is_some_and(|n| {
                (if enter {
                    n.op == Op::MonitorEnter
                } else {
                    n.op == Op::MonitorExit
                }) && n.inputs.get(2) == Some(&lock.monitor)
            })
        };
        let block_of = |id: NodeId| {
            schedule
                .node_to_block
                .get(id as usize)
                .copied()
                .filter(|&b| b != usize::MAX && b < schedule.blocks.len())
        };
        let can_transfer = |id: NodeId| {
            graph
                .nodes
                .get(id as usize)
                .is_none_or(|n| !node_cannot_deopt(graph, n))
        };
        // The enter.
        let enter = |why: String| Some((SelfLockRefusal::Enter, why));
        if !is_monitor_op(lock.enter, true) {
            return enter(format!("the enter n{} did not survive", lock.enter));
        }
        let Some(entry) = schedule.blocks.first() else {
            return enter("no entry block".to_string());
        };
        if block_of(lock.enter) != Some(0) || !entry.predecessors.is_empty() {
            return enter("the enter is not in an entry block nothing re-enters".to_string());
        }
        let Some(at) = entry.nodes.iter().position(|&n| n == lock.enter) else {
            return enter("the enter is not scheduled".to_string());
        };
        // A static body's mirror read is lowered as a plain load of its slot
        // (the `Op::ConstClass` arm's self-lock case: no helper, no exit), so
        // it is the one `ConstClass` that cannot transfer; it must itself be
        // in the entry block ahead of the enter, which consumes it.
        if mirror_slot.is_some() {
            let Some(m_at) = entry.nodes.iter().position(|&n| n == lock.monitor) else {
                return enter("the class mirror is not scheduled in the entry block".to_string());
            };
            if m_at >= at {
                return enter("the class mirror is not read before the enter".to_string());
            }
        }
        if let Some(&n) = entry.nodes[..at]
            .iter()
            .find(|&&n| !(mirror_slot.is_some() && n == lock.monitor) && can_transfer(n))
        {
            return Some((
                SelfLockRefusal::TransferBeforeEnter,
                format!("n{n} can transfer before the enter"),
            ));
        }
        // The exits, one per returning block.
        let exit_shape = |why: String| Some((SelfLockRefusal::ExitShape, why));
        let mut exit_blocks: HashMap<usize, NodeId> = HashMap::new();
        for &x in &lock.exits {
            if graph.nodes.get(x as usize).is_none_or(|n| n.op == Op::Dead) {
                // Its return was pruned with it; the Return sweep below
                // checks every surviving one.
                continue;
            }
            if !is_monitor_op(x, false) {
                return exit_shape(format!("the exit n{x} is not a MonitorExit of the monitor"));
            }
            let Some(b) = block_of(x) else {
                return exit_shape(format!("the exit n{x} is not scheduled"));
            };
            if exit_blocks.insert(b, x).is_some() {
                return exit_shape(format!("two exits in block {b}"));
            }
            let block = &schedule.blocks[b];
            let returns = block
                .terminator
                .and_then(|t| graph.nodes.get(t as usize))
                .is_some_and(|t| t.op == Op::Return);
            if !returns {
                return exit_shape(format!(
                    "the exit n{x}'s block {b} does not end in a return"
                ));
            }
            let Some(at) = block.nodes.iter().position(|&n| n == x) else {
                return exit_shape(format!("the exit n{x} is not in its block's order"));
            };
            if let Some(&n) = block.nodes[at + 1..].iter().find(|&&n| can_transfer(n)) {
                return Some((
                    SelfLockRefusal::TransferAfterExit,
                    format!("n{n} can transfer after the exit n{x}"),
                ));
            }
        }
        for (b, block) in schedule.blocks.iter().enumerate() {
            let returns = block
                .terminator
                .and_then(|t| graph.nodes.get(t as usize))
                .is_some_and(|t| t.op == Op::Return);
            if returns && !exit_blocks.contains_key(&b) {
                return Some((
                    SelfLockRefusal::ReturnNotReleased,
                    format!("the return of block {b} does not release"),
                ));
            }
        }
        // The deopt frames.
        if (!boxes.is_empty() || !points.is_empty())
            && !crate::deopt::self_lock_deopt_handover_enabled()
        {
            return Some((
                SelfLockRefusal::HandoverOff,
                "a deopt exit, and CRATONVM_JIT_SELF_LOCK_DEOPT_HANDOVER=0".to_string(),
            ));
        }
        // Round 15 wave 3 (lane sync2, S15-3): a static body's frames hand the
        // class mirror over under the single-pass static twin's own switch too
        // (the one `x64/deopt_stubs.rs` `self_lock_hands_over_at` asks).
        if mirror_slot.is_some()
            && (!boxes.is_empty() || !points.is_empty())
            && !crate::deopt::self_lock_static_deopt_handover_enabled()
        {
            return Some((
                SelfLockRefusal::HandoverOff,
                "a deopt exit, and CRATONVM_JIT_SELF_LOCK_STATIC_DEOPT_HANDOVER=0".to_string(),
            ));
        }
        let receiver = mirror_slot.is_none();
        for p in boxes.iter().map(|b| &**b).chain(points.iter()) {
            if let Some(why) = self_lock_frame_refusal_for(&p.frame_state, receiver) {
                // An inlined caller chain is its own row: the chain planners
                // (`CRATONVM_JIT_IR_SYNC_METHOD_CHAINS`) are meant to step aside.
                let cause = if p.frame_state.caller.is_some() {
                    SelfLockRefusal::FrameChain
                } else {
                    SelfLockRefusal::FrameShape
                };
                return Some((
                    cause,
                    format!("the deopt point at bci {} ({:?}): {why}", p.bci, p.reason),
                ));
            }
        }
        None
    }

    /// Build the interpreter `FrameState` for one safepoint snapshot.
    ///
    /// `method_key` is left to the VM caller to fill (the lowerer does not
    /// know it); deopt resume keys on the running `CompiledMethod`, not this
    /// string. It is recorded empty here.
    ///
    /// **`caller` is `None`, unconditionally, and that is the honest state of
    /// this function rather than a placeholder.** (Round 13 wave 3: a guard
    /// inside a spliced body may publish a caller chain instead, built from a
    /// chain snapshot by [`Self::splice_chain_frame_state`]; this function
    /// never sees one -- every by-bci index skips them.) It used to be filled from
    /// `ir::InlineScopeTable` through a `caller_chain_for` helper; round 10
    /// wave 9 retired both, because the table had no production populator and
    /// so the chain was `None` on every compile anyway — while a reader
    /// grepping for `InlineScopeTable` found production hits and concluded
    /// otherwise. See the retirement note where that helper used to be.
    ///
    /// `si` is `sp`'s index in `graph.safepoints`: a scalar-replaced object is
    /// described at the program point of the deopt sites that resolve to THIS
    /// snapshot, not of every site at its bci (see
    /// [`Self::site_resolves_to_snapshot`]).
    pub(in crate::ir_lower) fn resolve_frame_state(&self, si: usize, sp: &SafepointSnapshot) -> FrameState {
        let (locals, stack, monitors) = self.resolve_frame_values(si, sp);
        FrameState {
            class_id: 0,
            method_key: String::new(),
            bci: sp.bci as u32,
            locals,
            stack,
            monitors,
            caller: None,
        }
    }

    /// Describe one held monitor's locked reference as a frame value.
    ///
    /// [`Self::frame_value_for`] with the reference kind forced. The operand of
    /// a `monitorenter` is a reference by JVMS, but a node's `IrType` is only as
    /// good as the typing its producer did — a hand-built graph's `Op::Param` is
    /// `Int` — and an int-typed descriptor here would make the install verifier
    /// reject the whole compile (`deopt::monitor_object_defect`). A frame word
    /// holds the full 64-bit value whatever the node's type, so an int stack
    /// slot is re-spelled as a reference slot. A REGISTER copy of an int-typed
    /// node is not trusted to hold all 64 bits; the node's home word is used
    /// instead when it has one, and the answer is `Unsupported` (refuse, never
    /// guess) when it does not.
    pub(in crate::ir_lower) fn monitor_object_value(&self, obj: NodeId) -> FrameValue {
        match self.frame_value_for(obj) {
            FrameValue::StackSlot(off) | FrameValue::StackSlotLong(off) => {
                FrameValue::StackSlotRef(off)
            }
            FrameValue::Register(_) | FrameValue::RegisterLong(_) => self
                .monitor_object_home(obj)
                .unwrap_or(FrameValue::Unsupported),
            FrameValue::RegisterRef(r) => self
                .monitor_object_home(obj)
                .unwrap_or(FrameValue::RegisterRef(r)),
            other => other,
        }
    }

    /// The home frame word of `obj`, as a reference slot, when it has one that
    /// was written.
    pub(in crate::ir_lower) fn monitor_object_home(&self, obj: NodeId) -> Option<FrameValue> {
        let node = self.graph.nodes.get(obj as usize)?;
        if self
            .home_dropped
            .get(obj as usize)
            .copied()
            .unwrap_or(false)
        {
            return None;
        }
        let off = match (
            self.node_slot.get(obj as usize).copied().flatten(),
            &node.op,
        ) {
            (Some(slot), _) => slot.get() as i32,
            (None, Op::Param(idx)) => ((*idx as i32) + 1) * 8,
            (None, _) => return None,
        };
        Some(FrameValue::StackSlotRef(-off))
    }

    /// The held monitors of one snapshot, outermost first, in the shape
    /// `deopt::FrameState::monitors` wants.
    ///
    /// * **Coalesced per object.** The snapshot has one entry per executed
    ///   `monitorenter`; a re-entrant lock is one `MonitorInfo` whose
    ///   `lock_depth` is the count (the install verifier refuses two entries on
    ///   one object, except the held-then-re-take split below). Order is first
    ///   occurrence, which is acquisition order.
    /// * **`relock`** is `true` exactly when no live monitor op names the
    ///   object — see [`Lowerer::live_monitor_operands`]. The compiled code took
    ///   every other lock itself, and the thread still holds it at the deopt.
    /// * `describe` turns the object into a frame value. It is the same
    ///   describer the locals use, so a scalar-replaced object that a local
    ///   already defined comes back as a `VirtualObjectRef` to that definition.
    /// * **Nested-lock elision** (round 11 wave 14, lane fib): an entry whose
    ///   bit is set in [`SafepointSnapshot::monitor_elided_mask`] is a
    ///   re-entrant region `ir_optimize::elide_nested_monitors` deleted, so a
    ///   live object is then TWO entries -- the levels the compiled code holds
    ///   (`relock == false`), then the elided ones (`relock == true`). The
    ///   resume and the reason-9 materialiser honour `relock` per entry, so the
    ///   thread ends up holding the object as often as the interpreter frame
    ///   will release it. The install verifier admits exactly that split, held
    ///   part first (round 11 wave 15, lane `ir`: `deopt::DeoptVerifier`'s
    ///   monitor lane); the pass stays opt-in until a soak reads it clean.
    ///   An object no live op names keeps the single all-relock entry: the
    ///   compiled code holds none of its levels, elided or not.
    pub(in crate::ir_lower) fn resolve_monitors(
        &self,
        sp: &SafepointSnapshot,
        mut describe: impl FnMut(NodeId) -> FrameValue,
    ) -> Vec<crate::deopt::MonitorInfo> {
        if sp.monitors.is_empty() {
            return Vec::new();
        }
        // (object, levels the compiled code took, levels it elided).
        let mut order: Vec<(NodeId, u32, u32)> = Vec::with_capacity(sp.monitors.len());
        for (k, &obj) in sp.monitors.iter().enumerate() {
            let elided = sp.monitor_elided(k);
            match order.iter_mut().find(|(o, _, _)| *o == obj) {
                Some((_, held, nested)) => {
                    if elided {
                        *nested = nested.saturating_add(1);
                    } else {
                        *held = held.saturating_add(1);
                    }
                }
                None => order.push((obj, u32::from(!elided), u32::from(elided))),
            }
        }
        let mut out: Vec<crate::deopt::MonitorInfo> = Vec::with_capacity(order.len());
        for (obj, held, nested) in order {
            let object = describe(obj);
            if !self.live_monitor_operands.contains(&obj) {
                out.push(crate::deopt::MonitorInfo {
                    object,
                    lock_depth: held.saturating_add(nested),
                    relock: true,
                });
                continue;
            }
            match (held, nested) {
                (_, 0) => out.push(crate::deopt::MonitorInfo {
                    object,
                    lock_depth: held,
                    relock: false,
                }),
                (0, _) => out.push(crate::deopt::MonitorInfo {
                    object,
                    lock_depth: nested,
                    relock: true,
                }),
                _ => {
                    out.push(crate::deopt::MonitorInfo {
                        object: object.clone(),
                        lock_depth: held,
                        relock: false,
                    });
                    out.push(crate::deopt::MonitorInfo {
                        object,
                        lock_depth: nested,
                        relock: true,
                    });
                }
            }
        }
        out
    }

    /// The `(locals, stack, monitors)` of one snapshot's frame — everything
    /// [`Self::resolve_frame_state`] builds except the identity and the caller
    /// chain.
    ///
    /// Split out so the retired `caller_chain_for` could resolve a *caller's*
    /// snapshot without re-entering `resolve_frame_state` — chaining those two
    /// would have been mutually recursive, and a table that (wrongly) named a
    /// scope's own snapshot as its `caller_snapshot` would have recursed until
    /// the stack ran out, inside a compile, for a metadata defect. The split
    /// outlived the caller chain because `resolve_frame_values` is also what
    /// `resolve_frame_state` is built on, and keeping the two apart is what
    /// makes the re-entrancy hazard un-reintroducible by a future producer.
    pub(in crate::ir_lower) fn resolve_frame_values(
        &self,
        si: usize,
        sp: &SafepointSnapshot,
    ) -> (
        Vec<FrameValue>,
        Vec<FrameValue>,
        Vec<crate::deopt::MonitorInfo>,
    ) {
        // Guard-surviving scalar replacement (producer): when `sr_map` is set, a
        // snapshot slot holding a scalar-replaced (now-`Op::Dead`) `Op::New`
        // lowers to a `FrameValue::VirtualObject` (first occurrence) /
        // `VirtualObjectRef` (later occurrences), so a precise resume can
        // re-materialize the elided object instead of falling back to a
        // whole-method re-run. `emitted` tracks which objects already have their
        // defining `VirtualObject` in this frame. With `sr_map == None` this is
        // exactly the historical `frame_value_for` mapping (byte-identical).
        if let Some(sr) = self.sr_map {
            let deopt_block = self.deopt_block_for_snapshot(si, sp.bci);
            let site_span = deopt_block.and_then(|b| self.deopt_site_span_in_block(si, sp.bci, b));
            if false {
                let matches: Vec<NodeId> = sp
                    .locals
                    .iter()
                    .chain(sp.stack.iter())
                    .copied()
                    .filter(|n| *n != NO_NODE && sr.objects.contains_key(n))
                    .collect();
                eprintln!(
                    "[DBG_SCALAR_DEOPT] resolve bci {} deopt_block={:?} sr_objects={} matching_slots={:?}",
                    sp.bci,
                    deopt_block,
                    sr.objects.len(),
                    matches
                );
            }
            // A loop-carried object is live at its loop's header, whose snapshot
            // no site publishes (a table point for a loop exit at the header).
            // It is described there at the header block's start, where its
            // value φs are defined. Other objects keep the site-only answer.
            let header = if deopt_block.is_none() {
                self.loop_header_block_at(sp.bci)
            } else {
                None
            };
            // The for-each residual round (2026-10-06): sites in more than one
            // block (the flat frame of the `invoke` around a spliced body) give
            // no single deopt block; a loop-carried object whose fields agree
            // at every one of them is described at the first
            // ([`Self::loop_carried_agreeing_site`]).
            let point = |n: NodeId| match (header, sr.objects.get(&n)) {
                (Some((hb, region)), Some(info)) if info.loop_phis.is_some() => {
                    (Some(hb), Some((region, region)))
                }
                (None, Some(info))
                    if deopt_block.is_none()
                        && info.loop_phis.is_some()
                        && crate::deopt::osr_guard_exit_virtuals_enabled() =>
                {
                    match self.loop_carried_agreeing_site(n, info, si, sp.bci) {
                        Some((b, span)) => (Some(b), Some(span)),
                        None => (deopt_block, site_span),
                    }
                }
                (None, Some(info)) if deopt_block.is_none() && info.loop_phis.is_none() => {
                    match self.object_agreeing_site(n, si, sp.bci, sr) {
                        Some((b, span)) => (Some(b), Some(span)),
                        None => (deopt_block, site_span),
                    }
                }
                _ => (deopt_block, site_span),
            };
            let mut emitted: std::collections::HashSet<NodeId> = std::collections::HashSet::new();
            let mut locals = Vec::with_capacity(sp.locals.len());
            for &n in &sp.locals {
                locals.push(if n != NO_NODE && sr.objects.contains_key(&n) {
                    let (db, span) = point(n);
                    self.frame_value_for_object(n, db, span, sr, &mut emitted)
                } else {
                    self.frame_value_for(n)
                });
            }
            let mut stack = Vec::with_capacity(sp.stack.len());
            for &n in &sp.stack {
                stack.push(if n != NO_NODE && sr.objects.contains_key(&n) {
                    let (db, span) = point(n);
                    self.frame_value_for_object(n, db, span, sr, &mut emitted)
                } else {
                    self.frame_value_for(n)
                });
            }
            // After locals and stack, sharing `emitted`: a locked object a local
            // already defined is named by reference, not defined a second time.
            let monitors = self.resolve_monitors(sp, |n| {
                if n != NO_NODE && sr.objects.contains_key(&n) {
                    let (db, span) = point(n);
                    self.frame_value_for_object(n, db, span, sr, &mut emitted)
                } else {
                    self.monitor_object_value(n)
                }
            });
            if !sp.sb_virtual_builders.is_empty() {
                self.apply_sb_virtual_builders(sp, &mut locals, &mut stack);
            }
            return (locals, stack, monitors);
        }
        let mut locals: Vec<FrameValue> =
            sp.locals.iter().map(|&n| self.frame_value_for(n)).collect();
        let mut stack: Vec<FrameValue> =
            sp.stack.iter().map(|&n| self.frame_value_for(n)).collect();
        let monitors = self.resolve_monitors(sp, |n| self.monitor_object_value(n));
        if !sp.sb_virtual_builders.is_empty() {
            self.apply_sb_virtual_builders(sp, &mut locals, &mut stack);
        }
        (locals, stack, monitors)
    }

    fn apply_sb_virtual_builders(
        &self,
        sp: &SafepointSnapshot,
        locals: &mut [FrameValue],
        stack: &mut [FrameValue],
    ) {
        let mut next_id = locals
            .iter()
            .chain(stack.iter())
            .filter_map(|fv| match fv {
                FrameValue::VirtualObject(v) => Some(v.id + 1),
                FrameValue::VirtualObjectRef(id) => Some(*id + 1),
                _ => None,
            })
            .max()
            .unwrap_or(0);
        for vb in &sp.sb_virtual_builders {
            let vid = next_id;
            next_id += 1;
            let mut field_values = Vec::with_capacity(vb.parts.len() * 2);
            for part in &vb.parts {
                match part {
                    crate::ir::SbVirtualPart::Literal { text } => {
                        for &ch in text {
                            field_values.push(FrameValue::Long(crate::deopt::SB_VIRTUAL_TAG_CHAR));
                            field_values.push(FrameValue::Int(i64::from(ch)));
                        }
                    }
                    crate::ir::SbVirtualPart::Value { kind, val } => {
                        field_values.push(FrameValue::Long(kind.virtual_tag()));
                        field_values.push(self.frame_value_for(*val));
                    }
                }
            }
            let state = crate::deopt::VirtualObjectState {
                id: vid,
                class_id: 0,
                num_fields: field_values.len(),
                field_values,
                array_element_type: Some(crate::deopt::VIRTUAL_STRING_BUILDER_ATYPE),
            };
            let mut placed = false;
            if let Some(s) = vb.stack_slot {
                if s < stack.len() {
                    stack[s] = FrameValue::VirtualObject(state.clone());
                    placed = true;
                }
            }
            if let Some(l) = vb.local_slot {
                if l < locals.len() {
                    locals[l] = if placed {
                        FrameValue::VirtualObjectRef(vid)
                    } else {
                        FrameValue::VirtualObject(state)
                    };
                }
            }
        }
    }

    // `Lowerer::caller_chain_for` was DELETED in round 10 wave 9, together with
    // `ir::InlineScopeTable`. It built `FrameState::caller` from that table, and
    // the table had no production populator: `push_scope`, `bind_snapshot` and
    // `bind_snapshot_range` had no caller outside `#[cfg(test)]` anywhere in the
    // workspace, so `is_empty()` was true on every compile and this returned
    // `None` every time. Its READ side being wired is exactly what made it worth
    // deleting rather than leaving: a `rg` for `InlineScopeTable` found
    // production hits and told a reader the tier described inlined scopes, which
    // it never has.
    //
    // The honest state is the one `resolve_frame_state` now states outright:
    // this tier publishes flat frames, one scope each. What a future producer of
    // caller chains needs is written up in
    // `docs/jit/deopt-inline-scopes.md` (the metadata shape, including the
    // fail-closed `Unsupported` caller frame and the cross-scope `emitted`-set
    // gap this code had) and `docs/feature-designs/deopt-osr.md`
    // (why the staged half was retired rather than finished). The VM consumer
    // half exists and is tested: `deopt_resume::materialise_inlined_chain` walks
    // a chain, and `scope_resume_pc` parks each scope by its own
    // `ResumeSemantics`.

    /// Build [`Self::deopt_sites_by_bci`]: every node that can fire a deopt,
    /// under the bci that deopt RESUMES at.
    ///
    /// `Lowerer::resume_bci`'s mapping is restated here because it runs from
    /// `Lowerer::new`, before `self` exists. All three kinds of site use it:
    /// the `Op::Guard` arm and `emit_deopt_unless` (which serves the division
    /// zero guard) both resolve their point at `resume_bci(bci)`, so keying a
    /// guard or a division by its raw pc — as this index did until round 9
    /// wave 2 — hid every such site inside a spliced body from
    /// `deopt_block_for_snapshot(_, invoke)`, and a scalar-replaced object live there
    /// either lost its precise `VirtualObject` or had its block chosen from the
    /// OTHER sites at the invoke.
    pub(in crate::ir_lower) fn index_deopt_sites(
        graph: &Graph,
        spliced_ranges: &[(usize, usize, usize)],
    ) -> HashMap<usize, Vec<NodeId>> {
        let resume = |pc: usize| {
            spliced_ranges
                .iter()
                .find(|&&(start, end, _)| pc >= start && pc < end)
                .map_or(pc, |&(_, _, invoke)| invoke)
        };
        let mut sites: HashMap<usize, Vec<NodeId>> = HashMap::new();
        for (id, n) in graph.nodes.iter().enumerate() {
            let key = match n.op {
                Op::Guard { bci } => Some(resume(bci)),
                Op::Div | Op::Rem => n.bytecode_pc.map(resume),
                // Graph-aware since round 9 wave 4: a store into a fresh
                // allocation emits no deopt (`store_cannot_deopt`), so it is
                // not a site -- indexing it anyway only made the scalar-replace
                // producer's block choice more conservative than the emission.
                _ if !node_cannot_deopt(graph, n) => n.bytecode_pc.map(resume),
                _ => None,
            };
            if let Some(bci) = key {
                sites.entry(bci).or_default().push(id as NodeId);
            }
        }
        sites
    }

    /// Does `n` (a node indexed under the RESUME bci `bci` in
    /// [`Self::deopt_sites_by_bci`]) fire the deopt at `bci`?
    ///
    /// * An explicit `Op::Guard { bci: g }` does when `resume_bci(g) == bci`.
    /// * An integer division does only when no guard is anchored at its OWN
    ///   raw pc (the anchored guard owns the trap, and the floating division
    ///   beside it traps nowhere — `emit_div_zero_guard`), and only when its
    ///   divisor is not a non-zero constant (it then emits no test at all —
    ///   [`int_division_cannot_trap`]). An FP division never traps.
    /// * Every other indexed node is indexed precisely because it can transfer
    ///   from that program point.
    ///
    /// Round 9 wave 2: the anchoring question is asked with the division's raw
    /// pc and the "is it this bci" question with the resume bci — they were
    /// both asked with one `bci`, which cannot be right for both inside a
    /// splice.
    pub(in crate::ir_lower) fn fires_deopt_at(&self, n: &Node, bci: usize) -> bool {
        match n.op {
            Op::Div | Op::Rem => {
                let Some(pc) = n.bytecode_pc else {
                    return false;
                };
                matches!(n.ty, IrType::Int | IrType::Long)
                    && self.resume_bci(pc) == bci
                    && !self.guard_bcis.contains(&pc)
                    && !int_division_cannot_trap(self.graph, n)
            }
            Op::Guard { bci: gb } => self.resume_bci(gb) == bci,
            _ => !self.node_cannot_deopt_here(n),
        }
    }

    /// The lowest node id among the deopt sites at `bci` that sit in `block`.
    ///
    /// The builder creates a block's control-pinned nodes in bytecode walk
    /// order, so a node with a smaller id in the same block executes before
    /// every one of these sites. See [`Self::eliminated_node_precedes_deopt`].
    ///
    /// Answered together with the HIGHEST such id: `(first, last)`. A node with
    /// a larger id than `last` in the same block executes after every one of
    /// these sites (see [`Self::eliminated_store_follows_fresh_deopt`]).
    ///
    /// Only the sites that resolve to snapshot `si` count (round 11 wave 8,
    /// see [`Self::site_resolves_to_snapshot`]).
    pub(in crate::ir_lower) fn deopt_site_span_in_block(
        &self,
        si: usize,
        bci: usize,
        block: usize,
    ) -> Option<(NodeId, NodeId)> {
        let sites: &[NodeId] = self
            .deopt_sites_by_bci
            .get(&bci)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let mut span: Option<(NodeId, NodeId)> = None;
        for site in sites.iter().copied().filter(|&site| {
            self.graph.nodes.get(site as usize).is_some_and(|n| {
                self.fires_deopt_at(n, bci)
                    && self.site_resolves_to_snapshot(n, si, bci)
                    && self.schedule.node_to_block.get(site as usize).copied() == Some(block)
            })
        }) {
            span = Some(match span {
                None => (site, site),
                Some((lo, hi)) => (lo.min(site), hi.max(site)),
            });
        }
        span
    }

    /// The object was allocated in the deopt block `db` itself, before the
    /// first deopt site there. Has the eliminated store `victim`, whose
    /// control input is `ctrl`, certainly NOT executed for THIS object when
    /// any site at the bci fires?
    ///
    /// * `ctrl`'s block is not `db` -- yes. Between the allocation and the
    ///   site only `db`'s own straight-line code runs, and a store to an object
    ///   cannot run before its allocation, so a store in any other block runs
    ///   after the site (or never). A store in a loop block from an earlier
    ///   iteration wrote an earlier iteration's object: the allocation in `db`
    ///   makes a fresh one each time `db` runs, and the snapshot names that one.
    /// * `ctrl`'s block IS `db` -- yes exactly when `victim` comes after the
    ///   LAST site at the bci (`last_site`), by the same in-block id order
    ///   [`Self::eliminated_node_precedes_deopt`] relies on.
    /// * unscheduled control, or no ids to compare -- no (refuse).
    pub(in crate::ir_lower) fn eliminated_store_follows_fresh_deopt(
        &self,
        ctrl: NodeId,
        victim: Option<NodeId>,
        db: usize,
        last_site: Option<NodeId>,
    ) -> bool {
        match self.schedule.node_to_block.get(ctrl as usize).copied() {
            None | Some(usize::MAX) => false,
            Some(b) if b != db => true,
            Some(_) => matches!((victim, last_site), (Some(v), Some(site)) if v > site),
        }
    }

    /// Had the eliminated node `victim` (an `Op::New` or one of its field
    /// stores), whose control input is `ctrl`, executed by the time the deopt
    /// in block `db` fires?
    ///
    /// * `ctrl`'s block strictly dominates `db` — yes, on every path.
    /// * `ctrl`'s block IS `db` — yes exactly when `victim` precedes the first
    ///   deopt site there (`first_site`). Both are control-pinned nodes of one
    ///   block, which the builder creates in bytecode walk order and nothing
    ///   later reorders: a store is a hard barrier to hoisting, an unroll clones
    ///   whole iterations in execution order, and a load-CSE survivor is the
    ///   earlier node. A floating division is excluded as a site when a guard
    ///   is anchored at its bci (always, for built graphs).
    /// * otherwise — no. Refusing costs a precise resume, never correctness.
    ///
    /// The same-block case used to refuse outright, which made the recipe fail
    /// for the commonest shape there is: `o = new; o.f = x; ...; guard` in one
    /// straight-line block.
    pub(in crate::ir_lower) fn eliminated_node_precedes_deopt(
        &self,
        ctrl: NodeId,
        victim: Option<NodeId>,
        db: usize,
        first_site: Option<NodeId>,
    ) -> bool {
        if self.schedule.node_strictly_dominates_block(ctrl, db) {
            return true;
        }
        let same_block = self
            .schedule
            .node_to_block
            .get(ctrl as usize)
            .is_some_and(|&b| b != usize::MAX && b == db);
        same_block && matches!((victim, first_site), (Some(v), Some(site)) if v < site)
    }

    /// Does the deopt site `n`, indexed under the resume bci `bci`, publish
    /// snapshot `si`'s frame when it fires?
    ///
    /// The rule [`Self::resolve_frame_state_for_site`] applies: a node that
    /// names a snapshot AT `bci` publishes that one; any other node publishes
    /// the first snapshot at `bci` ([`Self::safepoint_first_by_bci`]).
    ///
    /// Round 11 wave 8 (`r11w7-mid-sr-deopt-block-is-chosen-per-bci-not-per-snapshot`):
    /// a scalar-replaced object is described at the program point of the
    /// sites that publish ITS snapshot. Choosing among every site at the bci
    /// described an unnamed original at the pre-header point of a node-named
    /// copy (LICM's pre-header frame, the range-check predicate) whenever the
    /// named nodes were the only sites there — and a store in the loop body
    /// looks "not yet run" from the pre-header while it has run at the
    /// original's point in the loop.
    pub(in crate::ir_lower) fn site_resolves_to_snapshot(&self, n: &Node, si: usize, bci: usize) -> bool {
        self.snapshot_published_by_site(n, bci) == Some(si)
    }

    /// The snapshot index the deopt site `n`, indexed under the resume bci
    /// `bci`, publishes when it fires: its own named snapshot when that one is
    /// AT `bci`, else the first snapshot at `bci`. `None` only when `bci` has
    /// no snapshot at all. The one statement of the rule
    /// [`Self::site_resolves_to_snapshot`] and
    /// [`Self::snapshots_no_site_publishes_beside_one_that_does`] both apply.
    pub(in crate::ir_lower) fn snapshot_published_by_site(&self, n: &Node, bci: usize) -> Option<usize> {
        let named = n.frame_snapshot.filter(|&sj| {
            self.graph
                .safepoints
                .get(sj as usize)
                .is_some_and(|s| s.bci == bci)
        });
        match named {
            Some(sj) => Some(sj as usize),
            None => self.safepoint_first_by_bci.get(&bci).copied(),
        }
    }

    /// Snapshots no deopt site publishes, at a bci where some site publishes
    /// ANOTHER snapshot (irlower proposal W8-1, round 11 wave 9).
    ///
    /// Since wave 8 a site publishes the snapshot it names when it names one
    /// at its bci, so after LICM or the range-check predicate gives the only
    /// sites at a bci a pre-header snapshot, the first snapshot there — the
    /// original's — is published by nothing. Its table point was still built,
    /// and it cost twice: (1) `build_deopt_points` dedups by native offset
    /// keeping the FIRST index, so the by-bci readers in `deopt_resume` read
    /// the unpublished original's frame at that offset while every trap
    /// publishes the copy's; (2) a scalar-replaced object is
    /// `MaterializationRequired` there (`deopt_block_for_snapshot` answers
    /// `None` for a snapshot no site publishes), and
    /// `ir_artifact_names_an_undescribable_elided_object` walks table points
    /// too, so a side-effecting body could be discarded over a point nothing
    /// traps at.
    ///
    /// Only snapshots SHADOWED this way are returned: the bci keeps a table
    /// point (the published sibling's), so every by-bci provenance question
    /// that found a point before still finds one, with the same reason. A
    /// snapshot at a bci no site publishes anything at is left alone — that
    /// is `ir_deopt_points_at_traps_enabled`'s broader, default-off question.
    pub(in crate::ir_lower) fn snapshots_no_site_publishes_beside_one_that_does(&self) -> std::collections::HashSet<usize> {
        let published = self.snapshots_published_by_a_site();
        let published_bcis: std::collections::HashSet<usize> = published
            .iter()
            .filter_map(|&si| self.graph.safepoints.get(si).map(|s| s.bci))
            .collect();
        self.graph
            .safepoints
            .iter()
            .enumerate()
            .filter(|&(si, sp)| published_bcis.contains(&sp.bci) && !published.contains(&si))
            .map(|(si, _)| si)
            .collect()
    }

    /// The flat snapshots some deopt site of this body publishes
    /// ([`Self::snapshot_published_by_site`]); W8-1's half, and the for-each
    /// residual round's test for a table point no stub hands out.
    pub(in crate::ir_lower) fn snapshots_published_by_a_site(&self) -> std::collections::HashSet<usize> {
        let mut published: std::collections::HashSet<usize> = std::collections::HashSet::new();
        for (&bci, sites) in &self.deopt_sites_by_bci {
            for &site in sites {
                let Some(n) = self.graph.nodes.get(site as usize) else {
                    continue;
                };
                if !self.fires_deopt_at(n, bci) {
                    continue;
                }
                if let Some(si) = self.snapshot_published_by_site(n, bci) {
                    published.insert(si);
                }
            }
        }
        published
    }

    /// Block where the deopt publishing snapshot `si` (at `bci`) fires — the
    /// program point all of a scalar-replaced object's field stores must
    /// precede for its `VirtualObject` emission to be temporally correct: the
    /// block of the deopt site(s) [`Self::deopt_sites_by_bci`] indexes under
    /// this bci that publish `si` ([`Self::site_resolves_to_snapshot`]).
    /// Returns `None` (⇒ the producer bails to `MaterializationRequired`) when
    /// the block can't be uniquely identified, and when no site publishes
    /// `si` at all: such a snapshot is a table point nothing traps at, and
    /// "not describable here" is the honest answer for it.
    /// The loop header whose join is at `bci`: its block and its region
    /// node, when that block is entered by a back edge (a predecessor it
    /// dominates). `None` for any other bci.
    pub(in crate::ir_lower) fn loop_header_block_at(&self, bci: usize) -> Option<(usize, NodeId)> {
        self.schedule
            .blocks
            .iter()
            .enumerate()
            .find_map(|(b, block)| {
                let ctrl = self.graph.nodes.get(block.ctrl as usize)?;
                if !matches!(ctrl.op, Op::Merge | Op::Region) || ctrl.bytecode_pc != Some(bci) {
                    return None;
                }
                block
                    .predecessors
                    .iter()
                    .any(|&p| self.schedule.dom.dominates(b, p))
                    .then_some((b, block.ctrl))
            })
    }

    /// The for-each residual round (2026-10-06): the block and site span to
    /// describe the LOOP-CARRIED object `new_id` at for snapshot `si`, whose
    /// deopt sites at `bci` sit in more than one block
    /// ([`Self::deopt_block_for_snapshot`] answers `None` there: the flat frame
    /// of the `invoke` around a spliced body, every guard and call of which
    /// resumes at that `invoke`). The object is describable when the
    /// allocation precedes every site and each field has ONE reaching
    /// definition there, the same at every site block
    /// ([`Self::loop_carried_field_values`]): its value at the snapshot is then
    /// that, whichever site fires, and the first site block describes it. Every
    /// such value is pinned body-wide (`loop_carried_recipe_values`). `None`
    /// for fewer than two site blocks, an unscheduled site, or any
    /// disagreement.
    /// The distinct blocks of the deopt sites that publish snapshot `si` at
    /// `bci`, in site order; `None` when one of them is unscheduled.
    fn snapshot_site_blocks(&self, si: usize, bci: usize) -> Option<Vec<usize>> {
        let sites: &[NodeId] = self
            .deopt_sites_by_bci
            .get(&bci)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let mut blocks: Vec<usize> = Vec::new();
        for &site in sites {
            let Some(n) = self.graph.nodes.get(site as usize) else {
                continue;
            };
            if !(self.fires_deopt_at(n, bci) && self.site_resolves_to_snapshot(n, si, bci)) {
                continue;
            }
            let b = *self.schedule.node_to_block.get(site as usize)?;
            if b == usize::MAX {
                return None;
            }
            if !blocks.contains(&b) {
                blocks.push(b);
            }
        }
        Some(blocks)
    }

    /// [`Self::loop_carried_agreeing_site`] for an object that is NOT
    /// loop-carried (a varargs `Object[]` on the operand stack of the
    /// `invoke` around a spliced `Method.invoke`): describable when every
    /// site block describes it as the same virtual object, and the first
    /// site block then serves. `None` for fewer than two site blocks, an
    /// unscheduled site, an undescribable block, or any disagreement.
    pub(in crate::ir_lower) fn object_agreeing_site(
        &self,
        new_id: NodeId,
        si: usize,
        bci: usize,
        sr: &ScalarReplacementMap,
    ) -> Option<(usize, (NodeId, NodeId))> {
        let blocks = self.snapshot_site_blocks(si, bci)?;
        if blocks.len() < 2 {
            return None;
        }
        let mut first: Option<((usize, (NodeId, NodeId)), FrameValue)> = None;
        for b in blocks {
            let span = self.deopt_site_span_in_block(si, bci, b)?;
            let v = self.frame_value_for_object(
                new_id,
                Some(b),
                Some(span),
                sr,
                &mut std::collections::HashSet::new(),
            );
            if !matches!(v, FrameValue::VirtualObject(_)) {
                return None;
            }
            match &first {
                None => first = Some(((b, span), v)),
                Some((_, agreed)) if *agreed == v => {}
                Some(_) => return None,
            }
        }
        first.map(|(at, _)| at)
    }

    pub(in crate::ir_lower) fn loop_carried_agreeing_site(
        &self,
        new_id: NodeId,
        info: &VirtualObjectInfo,
        si: usize,
        bci: usize,
    ) -> Option<(usize, (NodeId, NodeId))> {
        let phis = info.loop_phis.as_ref()?;
        let blocks = self.snapshot_site_blocks(si, bci)?;
        if blocks.len() < 2 {
            return None;
        }
        let mut first: Option<((usize, (NodeId, NodeId)), Vec<Option<NodeId>>)> = None;
        for b in blocks {
            let span = self.deopt_site_span_in_block(si, bci, b)?;
            if !self.eliminated_node_precedes_deopt(info.new_ctrl, Some(new_id), b, Some(span.0)) {
                return None;
            }
            let values = self.loop_carried_field_values(new_id, info, phis, b, Some(span))?;
            match &first {
                None => first = Some(((b, span), values)),
                Some((_, agreed)) if *agreed == values => {}
                Some(_) => return None,
            }
        }
        first.map(|(at, _)| at)
    }

    pub(in crate::ir_lower) fn deopt_block_for_snapshot(&self, si: usize, bci: usize) -> Option<usize> {
        // An `Op::Guard` at this bci OWNS the deopt: since
        // `ir::IrBuilder::add_div_zero_guard`, a division's zero-divisor trap
        // is the control-anchored guard's, and the floating `Op::Div` beside it
        // no longer traps at all. The two sit in different blocks in exactly
        // the case that anchoring exists to fix — the scheduler hoisted the
        // division — so consulting both would report "ambiguous" and give up
        // on a program point that is in fact unambiguous. Anchoring is judged
        // per division, at its own raw pc — see `fires_deopt_at`.
        let mut found: Option<usize> = None;
        // Only the nodes `Lowerer::new` indexed under this bci can answer, so
        // walk those rather than the whole graph once per snapshot. They are in
        // node-id order, and the answer does not depend on the order anyway:
        // every exit is either a single agreed block or `None`.
        let sites: &[NodeId] = self
            .deopt_sites_by_bci
            .get(&bci)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        for &site in sites {
            let id = site as usize;
            let Some(n) = self.graph.nodes.get(id) else {
                continue;
            };
            // The deopt at `bci` fires from an explicit `Op::Guard { bci }`,
            // from the div/rem zero/overflow guard the lowerer emits at a
            // division no guard is anchored beside, or from any other node that
            // can transfer from this program point — and publishes `si`.
            let is_deopt_here =
                self.fires_deopt_at(n, bci) && self.site_resolves_to_snapshot(n, si, bci);
            if is_deopt_here {
                let b = *self.schedule.node_to_block.get(id)?;
                if b == usize::MAX {
                    return None;
                }
                match found {
                    Some(prev) if prev != b => return None, // ambiguous
                    _ => found = Some(b),
                }
            }
        }
        found
    }

    /// The field values of a LOOP-CARRIED scalar-replaced object
    /// ([`VirtualObjectInfo::loop_phis`]) at the deopt sites `site_span` of
    /// block `db`: per field, the definition that reaches them, or `None`
    /// (refuse the object) when any field has no single one.
    ///
    /// A field's definitions are its eliminated stores (in their blocks, in
    /// id order) and the value φs escape analysis created at joins. Walking
    /// the scheduled CFG backwards from the sites:
    ///
    /// * in `db`, the latest store before the first site wins; a store between
    ///   the first and last site is ambiguous and refuses;
    /// * at the END of any other block, its latest store, else (the
    ///   allocation's block) the zero default, else the value at its start;
    /// * at the START of a block, its value φ when it has one; else the value
    ///   at the end of its predecessors, which must all agree. A predecessor
    ///   already being computed (a loop's back edge with no store on the way)
    ///   adds nothing, which is the trivial-φ rule; a predecessor that brings
    ///   a different value refuses. Reaching the entry block without passing
    ///   the allocation refuses.
    ///
    /// The allocation dominates every site that names the object, so every
    /// block this walk visits lies on a path from the allocation to the site.
    /// The answer is therefore exactly the field's value there, or a refusal;
    /// a φ escape analysis did not offer shows up as a disagreement, never as
    /// a stale value.
    pub(in crate::ir_lower) fn loop_carried_field_values(
        &self,
        new_id: NodeId,
        info: &VirtualObjectInfo,
        phis: &[(usize, NodeId)],
        db: usize,
        site_span: Option<(NodeId, NodeId)>,
    ) -> Option<Vec<Option<NodeId>>> {
        if phis.is_empty() {
            return None;
        }
        let (first_site, last_site) = site_span?;
        let block_of = |n: NodeId| -> Option<usize> {
            self.schedule
                .node_to_block
                .get(n as usize)
                .copied()
                .filter(|&b| b != usize::MAX)
        };
        let alloc_block = block_of(info.new_ctrl)?;
        if info.store_nodes.len() != info.store_ctrls.len()
            || info.store_fields.len() != info.store_ctrls.len()
        {
            return None;
        }
        let dbg = false;
        let mut out = Vec::with_capacity(info.num_fields);
        for field in 0..info.num_fields {
            let value = self.loop_carried_one_field(
                new_id,
                info,
                phis,
                db,
                first_site,
                last_site,
                alloc_block,
                field,
                &block_of,
            );
            if value.is_none() && dbg {
                eprintln!(
                    "[DBG_SCALAR_DEOPT] new {new_id}: loop-carried field {field} refused at block {db} (sites {first_site}..{last_site}, alloc block {alloc_block}, phis {phis:?})"
                );
            }
            out.push(value?);
        }
        Some(out)
    }

    /// One field of [`Self::loop_carried_field_values`].
    #[allow(clippy::too_many_arguments)]
    pub(in crate::ir_lower) fn loop_carried_one_field(
        &self,
        new_id: NodeId,
        info: &VirtualObjectInfo,
        phis: &[(usize, NodeId)],
        db: usize,
        first_site: NodeId,
        last_site: NodeId,
        alloc_block: usize,
        field: usize,
        block_of: &dyn Fn(NodeId) -> Option<usize>,
    ) -> Option<Option<NodeId>> {
        {
            // Per block: this field's stores, as (store id, value).
            let mut stores: HashMap<usize, Vec<(NodeId, Option<NodeId>)>> = HashMap::new();
            for ((&ctrl, &store), &entry) in info
                .store_ctrls
                .iter()
                .zip(info.store_nodes.iter())
                .zip(info.store_fields.iter())
            {
                let (f, value) = entry?;
                if f == field {
                    stores
                        .entry(block_of(ctrl)?)
                        .or_default()
                        .push((store, value));
                }
            }
            let mut phi_at: HashMap<usize, NodeId> = HashMap::new();
            for &(f, phi) in phis {
                if f == field {
                    let b = block_of(phi)?;
                    if phi_at.insert(b, phi).is_some_and(|prev| prev != phi) {
                        return None;
                    }
                }
            }
            // The value at the sites in `db`.
            let in_db = stores.get(&db).map(Vec::as_slice).unwrap_or(&[]);
            if in_db
                .iter()
                .any(|&(st, _)| st >= first_site && st <= last_site)
            {
                return None;
            }
            let before = in_db
                .iter()
                .filter(|&&(st, _)| st < first_site)
                .max_by_key(|&&(st, _)| st);
            let value = if let Some(&(_, v)) = before {
                v
            } else if db == alloc_block {
                if new_id >= first_site {
                    return None;
                }
                None
            } else {
                let mut memo: HashMap<usize, Option<Option<NodeId>>> = HashMap::new();
                match self.loop_carried_value_at_start(
                    db,
                    alloc_block,
                    &stores,
                    &phi_at,
                    &mut memo,
                    0,
                )? {
                    Some(v) => v,
                    None => return None,
                }
            };
            Some(value)
        }
    }

    /// [`Self::loop_carried_field_values`]' value at the START of block `b`.
    /// `Some(Some(v))`: the field holds `v` (`None` inside: the zero default);
    /// `Some(None)`: `b` is already being computed (a cycle: no contribution);
    /// `None`: refuse.
    #[allow(clippy::type_complexity)]
    pub(in crate::ir_lower) fn loop_carried_value_at_start(
        &self,
        b: usize,
        alloc_block: usize,
        stores: &HashMap<usize, Vec<(NodeId, Option<NodeId>)>>,
        phi_at: &HashMap<usize, NodeId>,
        memo: &mut HashMap<usize, Option<Option<NodeId>>>,
        depth: usize,
    ) -> Option<Option<Option<NodeId>>> {
        if let Some(&phi) = phi_at.get(&b) {
            return Some(Some(Some(phi)));
        }
        match memo.get(&b) {
            Some(Some(v)) => return Some(Some(*v)),
            Some(None) => return Some(None),
            None => {}
        }
        if depth > 4096 {
            return None;
        }
        let preds = &self.schedule.blocks.get(b)?.predecessors;
        if preds.is_empty() {
            return None; // the entry block, never past the allocation
        }
        memo.insert(b, None);
        let mut agreed: Option<Option<NodeId>> = None;
        for &p in preds {
            // The value at the END of `p`.
            let at_end = if let Some(v) = stores
                .get(&p)
                .and_then(|list| list.iter().max_by_key(|&&(st, _)| st))
            {
                Some(v.1)
            } else if p == alloc_block {
                Some(None)
            } else {
                self.loop_carried_value_at_start(p, alloc_block, stores, phi_at, memo, depth + 1)?
            };
            let Some(v) = at_end else {
                continue; // a cycle back into a block being computed
            };
            match agreed {
                None => agreed = Some(v),
                Some(prev) if prev == v => {}
                Some(_) => return None,
            }
        }
        match agreed {
            Some(v) => {
                memo.insert(b, Some(v));
                Some(Some(v))
            }
            // Every predecessor leads back into a block still being computed
            // (a loop body entered from its own header): `b` holds whatever
            // that block holds, so it adds nothing. Not memoised -- the answer
            // is that block's, which is not known yet.
            None => {
                memo.remove(&b);
                Some(None)
            }
        }
    }

    /// The field values of an object some of whose stores ran before the site
    /// and some did not: per field, the value of the last RUN store to it, or
    /// `None` (the zero default) when none ran. Round 11 wave 6, the
    /// follow-up of `r11w5-lower-virtual-object-store-fields-patch`.
    ///
    /// For a FRESH object (`fresh_here`: allocated in the deopt block `db`
    /// before its first site) every run store must be in `db` itself, so that
    /// node-id order is execution order ([`Self::eliminated_node_precedes_deopt`]'s
    /// premise); on a fresh object that is the only place a run store can be.
    /// For an object allocated in a block strictly dominating `db` (round 11
    /// wave 8, `r11w7-mid-sr-store-after-deopt-outside-the-allocation-block`)
    /// a run store may also sit in a block strictly dominating `db`. Those
    /// blocks form one chain of the dominator tree, and on every path the last
    /// execution of a dominator of `db` precedes the last execution of each of
    /// its dominated blocks on that chain, so "deeper block, then higher id in
    /// one block" is execution order.
    ///
    /// Anything else is a map this function does not understand, and so is an
    /// incomplete `store_fields` (a hand-built map, or a store whose field or
    /// value did not map back to the IR) or a field index past `num_fields`.
    /// Each of those answers `None`, and the caller refuses the object.
    pub(in crate::ir_lower) fn field_values_after_run_stores(
        &self,
        info: &VirtualObjectInfo,
        db: usize,
        first_site: Option<NodeId>,
        fresh_here: bool,
    ) -> Option<Vec<Option<NodeId>>> {
        let n = info.store_ctrls.len();
        if info.store_nodes.len() != n || info.store_fields.len() != n {
            return None;
        }
        // Per field: (id of the last run store, its block, the value it wrote).
        let mut last: Vec<Option<(NodeId, usize, Option<NodeId>)>> = vec![None; info.num_fields];
        for ((&ctrl, &store), &entry) in info
            .store_ctrls
            .iter()
            .zip(info.store_nodes.iter())
            .zip(info.store_fields.iter())
        {
            let (field, value) = entry?;
            if !self.eliminated_node_precedes_deopt(ctrl, Some(store), db, first_site) {
                // Not run at the site: the caller has classified it pending.
                continue;
            }
            let block = match self.schedule.node_to_block.get(ctrl as usize).copied() {
                Some(b) if b != usize::MAX => b,
                _ => return None,
            };
            if block != db && (fresh_here || !self.schedule.dom.dominates(block, db)) {
                return None;
            }
            let slot = last.get_mut(field)?;
            let later = match *slot {
                None => true,
                Some((prev, prev_block, _)) if prev_block == block => store > prev,
                // Both blocks dominate `db`, so one dominates the other.
                Some((_, prev_block, _)) => self.schedule.dom.dominates(prev_block, block),
            };
            if later {
                *slot = Some((store, block, value));
            }
        }
        Some(
            last.into_iter()
                .map(|e| e.and_then(|(_, _, v)| v))
                .collect(),
        )
    }

    /// Round 11 wave 8 (`r11w7-mid-sr-store-after-deopt-outside-the-allocation-block`):
    /// the object was allocated in block `alloc_block`, which strictly
    /// dominates the deopt block `db`. Has the eliminated store `victim`,
    /// whose control input is `ctrl`, certainly NOT executed on THIS object
    /// when a site at the bci fires?
    ///
    /// Yes when the store sits after the LAST site of `db` (same in-block id
    /// order as [`Self::eliminated_node_precedes_deopt`]) or in a block `db`
    /// strictly dominates, AND no path leads from it back to `db` without
    /// passing `alloc_block`. The second half is the loop case the fresh
    /// rule never met: with the allocation outside a loop around `db`, a body
    /// store after the site ran on this very object in the previous
    /// iteration. A path through `alloc_block` allocates a new object, so a
    /// store reaching `db` only through it wrote an older one. A store in any
    /// other block (another arm) may or may not have run: `false`.
    pub(in crate::ir_lower) fn eliminated_store_pending_after_alloc(
        &self,
        ctrl: NodeId,
        victim: Option<NodeId>,
        db: usize,
        last_site: Option<NodeId>,
        alloc_block: usize,
    ) -> bool {
        let sb = match self.schedule.node_to_block.get(ctrl as usize).copied() {
            Some(b) if b != usize::MAX => b,
            _ => return false,
        };
        let starts: Vec<usize> = if sb == db {
            if !matches!((victim, last_site), (Some(v), Some(site)) if v > site) {
                return false;
            }
            match self.schedule.blocks.get(db) {
                Some(block) => block.successors.clone(),
                None => return false,
            }
        } else if self.schedule.dom.dominates(db, sb) {
            vec![sb]
        } else {
            return false;
        };
        !self.block_reaches_avoiding(&starts, db, alloc_block)
    }

    /// Can control flow from any of `starts` (inclusive) to `target` without
    /// entering `avoid`? A depth-first walk over the scheduled CFG, linear in
    /// its size. Out-of-range successor indices are skipped.
    pub(in crate::ir_lower) fn block_reaches_avoiding(&self, starts: &[usize], target: usize, avoid: usize) -> bool {
        let blocks = &self.schedule.blocks;
        let mut seen = vec![false; blocks.len()];
        let mut work: Vec<usize> = Vec::new();
        for &b in starts {
            if b != avoid {
                if let Some(s) = seen.get_mut(b) {
                    if !*s {
                        *s = true;
                        work.push(b);
                    }
                }
            }
        }
        while let Some(b) = work.pop() {
            if b == target {
                return true;
            }
            let Some(block) = blocks.get(b) else {
                continue;
            };
            for &s in &block.successors {
                if s == avoid {
                    continue;
                }
                if let Some(v) = seen.get_mut(s) {
                    if !*v {
                        *v = true;
                        work.push(s);
                    }
                }
            }
        }
        false
    }

    /// Has the home word of `v`, a value a scalar-replacement recipe names
    /// (a field value or a run store's value), been written on every path to
    /// the deopt whose first site in block `db` is `first_site`?
    ///
    /// Round 11 wave 6. A recipe value is named by no frame state and, once
    /// its store is eliminated, often read by nothing at all, so the round-9
    /// rule that keeps a frame-NAMED value ahead of the next deopt point
    /// (`ir_schedule::priority_sort_block`) does not hold it: list scheduling
    /// gives a node with no users the lowest priority and can place it after
    /// the site. The deopt would then rebuild the field from a home word this
    /// iteration has not written. So: a constant, a value in a block strictly
    /// dominating `db`, a phi of `db` (written on entry), or a node of `db`
    /// ordered before the site. Anything else (unscheduled, another block,
    /// after the site, no site to compare with) answers `false`, and the
    /// caller refuses the object.
    pub(in crate::ir_lower) fn sr_value_written_before_site(
        &self,
        v: NodeId,
        db: usize,
        first_site: Option<NodeId>,
    ) -> bool {
        let Some(node) = self.graph.nodes.get(v as usize) else {
            return false;
        };
        if matches!(node.op, Op::Const(_) | Op::ConstF(_)) {
            return true;
        }
        if self.schedule.node_strictly_dominates_block(v, db) {
            return true;
        }
        if self.schedule.node_to_block.get(v as usize).copied() != Some(db) {
            return false;
        }
        if matches!(node.op, Op::Phi) {
            return true;
        }
        let (Some(site), Some(block)) = (first_site, self.schedule.blocks.get(db)) else {
            return false;
        };
        let Some(pv) = block.nodes.iter().position(|&n| n == v) else {
            return false;
        };
        if block.terminator == Some(site) {
            return true;
        }
        block
            .nodes
            .iter()
            .position(|&n| n == site)
            .is_some_and(|ps| pv < ps)
    }

    /// Lower a scalar-replaced object (`new_id`, an eliminated `Op::New`) that is
    /// live in a deopt snapshot slot into a `FrameValue::VirtualObject` (its
    /// first occurrence in this frame) or `VirtualObjectRef` (a later, shared
    /// occurrence). Bails to
    /// [`FrameValue::MaterializationRequired`] (⇒ the deopt is refused and the
    /// method takes the safe whole-method re-run) unless every soundness
    /// condition holds:
    ///   * a deopt block is known, the `Op::New` has run by the site, and every
    ///     eliminated field store has either certainly run (strict dominance,
    ///     or earlier in the deopt block) or certainly NOT run on this object
    ///     (`eliminated_store_follows_fresh_deopt`,
    ///     `eliminated_store_pending_after_alloc`) — so each field holds what
    ///     is described at the deopt bci (a deopt *before* a store would
    ///     otherwise materialize a post-store value);
    ///   * a field value that is itself a scalar-replaced object passes these
    ///     same gates, recursively (a nested `VirtualObject`, or a
    ///     `VirtualObjectRef` on a repeat); its refusal refuses this object;
    ///   * every field value resolves to a real machine/const `FrameValue`
    ///     (never `Undefined`/`Unsupported`/`MaterializationRequired`), which
    ///     `resolve_value` makes concrete from machine state at deopt time.
    ///
    /// Every bail below used to be spelled `FrameValue::Undefined`, and that was
    /// the last silent-null producer in this pipeline: the slot describes an
    /// object escape analysis DELETED, the interpreter *will* read it, and every
    /// resume sink maps `Undefined` to `Value::Int(0)` — i.e. `null` for a
    /// reference local, with no error and no refusal. `MaterializationRequired`
    /// says the same thing honestly and is unresumable by construction (see the
    /// `deopt` module's "Eliminated vs. undefined" section), so the wrong value
    /// becomes a refused deopt instead.
    pub(in crate::ir_lower) fn frame_value_for_object(
        &self,
        new_id: NodeId,
        deopt_block: Option<usize>,
        site_span: Option<(NodeId, NodeId)>,
        sr: &ScalarReplacementMap,
        emitted: &mut std::collections::HashSet<NodeId>,
    ) -> FrameValue {
        let first_site = site_span.map(|(first, _)| first);
        let last_site = site_span.map(|(_, last)| last);
        let dbg = false;
        let info = match sr.objects.get(&new_id) {
            Some(i) => i,
            // Unreachable through `resolve_frame_state` (which only calls here
            // for a key it just found), so the class id is not recoverable —
            // record the producer alone rather than invent a `class_id`.
            None => {
                return FrameValue::MaterializationRequired(EliminatedValue::new(
                    new_id,
                    EliminationCause::ScalarReplacedObject,
                ))
            }
        };
        let db = match deopt_block {
            Some(b) => b,
            None => {
                if dbg {
                    eprintln!("[DBG_SCALAR_DEOPT] bail new {new_id}: no deopt block for bci");
                }
                return Self::eliminated_object(
                    new_id,
                    info,
                    EliminationCause::ScalarReplacedObject,
                );
            }
        };
        // The allocation and every field store must have executed before the
        // deopt: strict block dominance, or the same block and an earlier node
        // id (`eliminated_node_precedes_deopt`). We test the *control* node of
        // each for the block — the New/store nodes themselves are now
        // `Op::Dead` (unscheduled), but their captured control inputs are live
        // and carry the same block — and the dead node's own id for the order.
        if !self.eliminated_node_precedes_deopt(info.new_ctrl, Some(new_id), db, first_site) {
            if dbg {
                eprintln!(
                    "[DBG_SCALAR_DEOPT] bail new {new_id}: new_ctrl {} (block {:?}) !strict-dom deopt block {db}",
                    info.new_ctrl,
                    self.schedule.node_to_block.get(info.new_ctrl as usize)
                );
            }
            return Self::eliminated_object(new_id, info, EliminationCause::ScalarReplacedObject);
        }
        // A loop-carried object: each field's reaching definition, or refuse.
        let loop_values = match &info.loop_phis {
            None => None,
            Some(phis) => match self.loop_carried_field_values(new_id, info, phis, db, site_span) {
                Some(values) => Some(values),
                None => {
                    if dbg {
                        eprintln!(
                            "[DBG_SCALAR_DEOPT] bail new {new_id}: loop-carried fields have no single reaching definition at deopt block {db}"
                        );
                    }
                    return Self::eliminated_object(
                        new_id,
                        info,
                        EliminationCause::ScalarReplacedObject,
                    );
                }
            },
        };
        // Round 11 wave 5 (lane lower; fix 1 of
        // `r11-iropt-ea-scalar-replacement-discards-compiles-and-leaks-recipes`,
        // the part this file can decide alone): a store may also be one that
        // certainly has NOT run yet. That is decidable when the object was
        // allocated in the deopt block itself, before its first site
        // (`fresh_here`); see `eliminated_store_follows_fresh_deopt`. If EVERY
        // store is such a store, the object at the deopt is the freshly
        // allocated one, with every field at its zero default (the same
        // premise as a `None` field below). `o = new Foo(); v = a[i]; o.x = v`
        // at the `a[i]` bounds deopt is that shape, and it used to refuse the
        // object, which discards the whole artifact once the method commits a
        // side effect. A MIX of run and not-yet-run stores is rebuilt field by
        // field from `store_fields` since wave 6, and still refuses when that
        // list is incomplete (`field_values_after_run_stores`).
        let fresh_here = self
            .schedule
            .node_to_block
            .get(info.new_ctrl as usize)
            .is_some_and(|&b| b != usize::MAX && b == db)
            && matches!(first_site, Some(site) if new_id < site);
        // Round 11 wave 8: an object allocated in a block STRICTLY dominating
        // `db` may have pending stores too, under a stricter rule than the
        // fresh one (a loop around `db` can carry a later store back to it);
        // see `eliminated_store_pending_after_alloc`. `None` when the
        // allocation is in `db` (then `fresh_here` decides) or unscheduled.
        let alloc_block_above = match self.schedule.node_to_block.get(info.new_ctrl as usize) {
            Some(&b) if b != usize::MAX && b != db && self.schedule.dom.dominates(b, db) => Some(b),
            _ => None,
        };
        let (mut stores_run, mut stores_pending) = (0usize, 0usize);
        // A loop-carried object's stores were already accounted for above.
        let store_ctrls: &[NodeId] = if loop_values.is_some() {
            &[]
        } else {
            &info.store_ctrls
        };
        for (k, &store_ctrl) in store_ctrls.iter().enumerate() {
            let store_node = info.store_nodes.get(k).copied();
            if self.eliminated_node_precedes_deopt(store_ctrl, store_node, db, first_site) {
                stores_run += 1;
                continue;
            }
            if fresh_here
                && self.eliminated_store_follows_fresh_deopt(store_ctrl, store_node, db, last_site)
            {
                stores_pending += 1;
                continue;
            }
            if alloc_block_above.is_some_and(|ab| {
                self.eliminated_store_pending_after_alloc(store_ctrl, store_node, db, last_site, ab)
            }) {
                stores_pending += 1;
                continue;
            }
            if dbg {
                eprintln!(
                    "[DBG_SCALAR_DEOPT] bail new {new_id}: store_ctrl {} (block {:?}) !strict-dom deopt block {db}",
                    store_ctrl,
                    self.schedule.node_to_block.get(store_ctrl as usize)
                );
            }
            return Self::eliminated_object(new_id, info, EliminationCause::ScalarReplacedObject);
        }
        // A MIX of run and not-yet-run stores (round 11 wave 6): each field
        // holds what its last RUN store wrote, or its zero default. Answerable
        // only from a complete `store_fields`; see `field_values_after_run_stores`.
        let mut mixed_field_values: Option<Vec<Option<NodeId>>> = loop_values;
        if stores_run > 0 && stores_pending > 0 {
            match self.field_values_after_run_stores(info, db, first_site, fresh_here) {
                Some(values) => mixed_field_values = Some(values),
                None => {
                    if dbg {
                        eprintln!(
                            "[DBG_SCALAR_DEOPT] bail new {new_id}: {stores_run} store(s) ran and \
                             {stores_pending} did not; no complete per-field store list"
                        );
                    }
                    return Self::eliminated_object(
                        new_id,
                        info,
                        EliminationCause::ScalarReplacedObject,
                    );
                }
            }
        }
        // A later occurrence of an already-defined object is a back/shared edge.
        if emitted.contains(&new_id) {
            return FrameValue::VirtualObjectRef(new_id as usize);
        }
        if stores_pending > 0 && mixed_field_values.is_none() {
            // Nothing stored yet: the zero-initialised allocation.
            emitted.insert(new_id);
            return FrameValue::VirtualObject(VirtualObjectState {
                array_element_type: info.array_element_type,
                id: new_id as usize,
                class_id: info.class_id,
                num_fields: info.num_fields,
                field_values: (0..info.num_fields).map(|_| FrameValue::Int(0)).collect(),
            });
        }
        // Build per-field values. `None` ⇒ zero default (admitted allocations set
        // no non-zero primitive field in <init>). A field whose value is itself a
        // scalar-replaced New (nested virtual) or an unresolvable slot bails the
        // whole object.
        let mut field_values: Vec<FrameValue> = Vec::with_capacity(info.num_fields);
        for i in 0..info.num_fields {
            let value = match &mixed_field_values {
                Some(values) => values.get(i).copied().flatten(),
                None => info.field_values.get(i).copied().flatten(),
            };
            let fv = match value {
                None => FrameValue::Int(0),
                Some(vnode) => {
                    // NESTED VIRTUAL OBJECT. A field whose value is itself a
                    // scalar-replaced allocation is described in place, by the
                    // same function, recursively. `emitted` is what makes that
                    // terminate and what makes sharing work: the second and
                    // later occurrences of an id come back as
                    // `FrameValue::VirtualObjectRef`, which the materializer
                    // resolves against the shell it already allocated. A cycle
                    // therefore ends at its first repeat rather than recursing.
                    //
                    // This used to bail the WHOLE enclosing object ("v1 emits
                    // no nested graphs"), which is what stopped a wrapper and
                    // its storage array from both being deleted: the wrapper's
                    // recipe names the array, so the moment the array became
                    // replaceable the wrapper's own recipe was refused.
                    //
                    // The recursive call re-runs every gate on the nested
                    // object -- its allocation and stores must strictly
                    // dominate the same deopt block -- so a nested object that
                    // cannot be described refuses itself, and the check below
                    // turns that refusal into a refusal of the enclosing
                    // object. Fail-closed, one level at a time.
                    if sr.objects.contains_key(&vnode) {
                        let nested =
                            self.frame_value_for_object(vnode, deopt_block, site_span, sr, emitted);
                        if matches!(
                            nested,
                            FrameValue::Undefined
                                | FrameValue::Unsupported
                                | FrameValue::MaterializationRequired(_)
                        ) {
                            if dbg {
                                eprintln!("[DBG_SCALAR_DEOPT] bail new {new_id}: field {i} nested virtual (node {vnode}) -> {nested:?}");
                            }
                            return Self::eliminated_object(
                                new_id,
                                info,
                                EliminationCause::NestedVirtualObject,
                            );
                        }
                        field_values.push(nested);
                        continue;
                    }
                    // Round 11 wave 6: the value's home must already hold it
                    // when the deopt fires; see `sr_value_written_before_site`.
                    if !self.sr_value_written_before_site(vnode, db, first_site) {
                        if dbg {
                            eprintln!("[DBG_SCALAR_DEOPT] bail new {new_id}: field {i} node {vnode} is not computed before the deopt site");
                        }
                        return Self::eliminated_object(
                            new_id,
                            info,
                            EliminationCause::ScalarReplacedObject,
                        );
                    }
                    // Round 9: a field value whose home was dropped and that no
                    // register describes is not describable HERE, and that is
                    // a per-object answer, not a compiler bug. `plan_slots`
                    // pins scalar-replaced field values, but `deopt_named` /
                    // `compute_deopt_named_reachable` (and regalloc's pinned
                    // set) do not, so an `Int`/`Long` field value can be
                    // promoted and lose its home. Asking `frame_value_for`
                    // anyway LATCHED `UnallocatedValue` and refused the whole
                    // method; falling back to the object-level
                    // `eliminated_object` answer (the same fallback every
                    // other undescribable field takes below) keeps the compile.
                    if let Some(vn) = self.graph.nodes.get(vnode as usize) {
                        if !matches!(vn.op, Op::Const(_) | Op::ConstF(_))
                            && self
                                .home_dropped
                                .get(vnode as usize)
                                .copied()
                                .unwrap_or(false)
                            && self.register_frame_value(vnode, vn.ty).is_none()
                        {
                            if dbg {
                                eprintln!("[DBG_SCALAR_DEOPT] bail new {new_id}: field {i} node {vnode} home dropped, no register");
                            }
                            return Self::eliminated_object(
                                new_id,
                                info,
                                EliminationCause::ScalarReplacedObject,
                            );
                        }
                    }
                    // Quieted for the same reason as the dropped-home check
                    // above: a field value with no location is a per-object
                    // "not describable here" (answered by `eliminated_object`
                    // below), not a compiler bug that should refuse the method.
                    let was_quiet = self.quiet_dropped_home.replace(true);
                    let fv = self.frame_value_for(vnode);
                    self.quiet_dropped_home.set(was_quiet);
                    // `MaterializationRequired` joins the refusal set: a field
                    // whose own producer was deleted must not be stored into a
                    // materialized object (`frame_state_is_resumable` documents
                    // that the producers, i.e. here, refuse such a field rather
                    // than let it through inside a `VirtualObject`).
                    if matches!(
                        fv,
                        FrameValue::Undefined
                            | FrameValue::Unsupported
                            | FrameValue::MaterializationRequired(_)
                    ) {
                        if dbg {
                            eprintln!("[DBG_SCALAR_DEOPT] bail new {new_id}: field {i} node {vnode} -> {fv:?}");
                        }
                        return Self::eliminated_object(
                            new_id,
                            info,
                            EliminationCause::ScalarReplacedObject,
                        );
                    }
                    fv
                }
            };
            field_values.push(fv);
        }
        emitted.insert(new_id);
        if false {
            eprintln!(
                "[DBG_SCALAR_DEOPT] emit VirtualObject (new {new_id}, class_id {}, {} field(s)) at deopt block {db}",
                info.class_id, info.num_fields
            );
        }
        let state = VirtualObjectState {
            array_element_type: info.array_element_type,
            id: new_id as usize,
            class_id: info.class_id,
            num_fields: info.num_fields,
            field_values,
        };
        if let Some(cond_node) = info.non_null_cond {
            let cond_fv = self.frame_value_for(cond_node);
            if matches!(
                cond_fv,
                FrameValue::Undefined
                    | FrameValue::Unsupported
                    | FrameValue::MaterializationRequired(_)
            ) {
                return Self::eliminated_object(
                    new_id,
                    info,
                    EliminationCause::ScalarReplacedObject,
                );
            }
            return FrameValue::ConditionalVirtualObject(Box::new((cond_fv, state)));
        }
        FrameValue::VirtualObject(state)
    }

    /// Resolve every recorded safepoint snapshot into a `DeoptimizationPoint`,
    /// keyed by the native offset of its bci. Returns them sorted+deduped by
    /// `native_offset` — the order `deopt::DeoptMetadataError::DeoptPointsUnsorted`
    /// checks on the install path. (It used to say "so
    /// `CompiledMethod::find_deopt_point` can binary search"; that search had no
    /// production caller and was deleted in round 10. Nothing resolves a point
    /// by offset: the guards bake `deopt_boxes`' addresses.)
    /// Snapshots whose bci emitted no machine code are skipped.
    /// `emission_trap_free` is `lower_inner`'s `osr_sentinel_free`: the body
    /// emitted no deopt stub and no call-exception stub, so nothing inside it
    /// can transfer to the interpreter.
    ///
    /// **This is the verification, not the prediction.** `graph_cannot_deopt`
    /// decided which homes to drop before emission; this is the emission
    /// answering. The two must agree for a point to be skipped, and when they
    /// do not, every point is built and `frame_value_for` latches
    /// `UnallocatedValue` on the first dropped home it cannot describe at a bci
    /// the body CAN transfer from — which `lower_inner` reads right after this
    /// returns and refuses the compile on.
    ///
    /// Round 9 wave 2: that last clause was not true. `lower_inner` read the
    /// latch once, BEFORE this ran, so every latch raised here was dropped and
    /// the value silently became `Unsupported`. A point at a bci with no
    /// transfer (`transfer_bcis`) is now resolved with the latch quieted —
    /// nothing can read it, and values named only there drop their homes
    /// routinely under `CRATONVM_JIT_IR_REG_AUTHORITATIVE` — and every other
    /// latch is enforced by the second `take_latched_bailout` in
    /// `lower_inner_with_array_list`.
    pub(in crate::ir_lower) fn build_deopt_points(&self, emission_trap_free: bool) -> Vec<DeoptimizationPoint> {
        if ir_drop_unreachable_homes_enabled() && emission_trap_free && self.graph_cannot_deopt() {
            // Every frame state in this body describes a program point no
            // deopt can arrive at. Nothing resolves a point from a trapping pc
            // at all — the guards bake their box addresses — and there are no
            // trapping pcs here in any case; the
            // by-bci readers in `deopt_resume` sit on the resume path, which
            // is reached only after a deopt that cannot happen, and both
            // return an error rather than a value when they find nothing.
            self.unreachable_points_skipped
                .set(self.graph.safepoints.len());
            return Vec::new();
        }
        let at_traps_only = ir_deopt_points_at_traps_enabled();
        // W8-1: a snapshot no site publishes, beside one that is published at
        // the same bci, gets no table point. See the function's doc.
        let shadowed = self.snapshots_no_site_publishes_beside_one_that_does();
        // The for-each residual round: asked only of a point that names an
        // undescribable scalar-replaced object (below), and computed once.
        let published_by_a_site: std::cell::OnceCell<std::collections::HashSet<usize>> =
            std::cell::OnceCell::new();
        let mut points: Vec<DeoptimizationPoint> = Vec::with_capacity(self.graph.safepoints.len());
        // JIT round 25 item 1a (wave 2): the table points are not sites
        // (`site_named_frame_value`).
        self.building_table_points.set(true);
        for (index, sp) in self.graph.safepoints.iter().enumerate() {
            if shadowed.contains(&index) {
                self.unreachable_points_skipped
                    .set(self.unreachable_points_skipped.get() + 1);
                continue;
            }
            // A chain snapshot (round 13 wave 3) is not a frame of this method:
            // its bci is a combined-buffer pc, and every by-bci reader of this
            // table (the OSR entry contract, the trap-reason lookups) asks about
            // this method's own bytecode. Its only publisher is a guard's box.
            if sp.is_splice_state() {
                continue;
            }
            // A snapshot a copy's nodes claim anchors inside THAT copy;
            // `bci_native` holds one offset per bci (the earliest, i.e. copy
            // 0's) and would give every copy of an unrolled body the same
            // point. `snapshot_native` is empty unless something cloned a
            // region, so this is `bci_native` verbatim on every other compile.
            let native_offset = match self
                .snapshot_native
                .get(&(index as u32))
                .or_else(|| self.bci_native.get(&sp.bci))
            {
                Some(&off) => off as u32,
                // bci produced no node / no machine code — nothing to anchor.
                None => continue,
            };
            // A bci this body cannot transfer from describes a program point no
            // resume can ask about — see `transfer_bcis`. Skipping it costs a
            // fully-resolved frame state per bytecode index, which is where the
            // 8x metadata-to-code ratio comes from.
            //
            // OFF by default. The consumers are the resume path's by-bci
            // provenance checks, and those FAIL CLOSED — a missing point turns
            // a precise resume into a whole-method re-run, and an
            // exception-exit transfer into a propagate. Both are safe
            // directions, but the second is observable, so this wants a soak on
            // the exception-heavy suites before it becomes a default rather
            // than an argument made here.
            if at_traps_only && !self.transfer_bcis.contains(&sp.bci) {
                self.unreachable_points_skipped
                    .set(self.unreachable_points_skipped.get() + 1);
                continue;
            }
            // No speculation yet — these are plain resume points (step 2,
            // emit-and-discard). A real guard (step 3) sets its own reason.
            let reason = DeoptReason::TransferToInterpreter;
            // Quiet the dropped-home latch at a bci nothing transfers from;
            // enforce it everywhere else. See `quiet_dropped_home`.
            self.quiet_dropped_home
                .set(!self.transfer_bcis.contains(&sp.bci));
            self.table_point_bci.set(Some(sp.bci));
            let frame_state = self.resolve_frame_state(index, sp);
            self.table_point_bci.set(None);
            self.quiet_dropped_home.set(false);
            // JIT round 19 proposal wave 12, lane lv12 (LV11-4): a point a
            // resume can arrive at must not read a shareable home whose
            // colouring range ended before the point.
            if self.deopt_point_range_check && self.transfer_bcis.contains(&sp.bci) {
                self.check_deopt_point_ranges(index, sp);
            }
            // The for-each residual round (2026-10-06): a table point naming a
            // scalar-replaced object the lowerer could not describe there is
            // dropped when no stub hands it out -- at a bci nothing transfers
            // from (`at_traps_only`'s argument for one point: nothing asks
            // about it), or for a snapshot no deopt site publishes (the flat
            // frame of the `invoke` around a spliced body whose guards publish
            // chains). Such a point has no deopt site to anchor a loop-carried
            // object's reaching definitions (the join after a pruned branch, a
            // spliced call in the loop), and the readers of this table only
            // ask whether a bci has a point and of what reason, failing closed
            // if it has none; every stub's own box is still checked.
            // Published, it made the `MaterializationRequired` backstop
            // (`ea_ir_bridge::ir_artifact_names_an_undescribable_elided_object`)
            // discard a body whose every real exit is described.
            if crate::deopt::osr_guard_exit_virtuals_enabled()
                && crate::ea_ir_bridge::frame_state_names_an_undescribable_elided_object(
                    &frame_state,
                )
                && (!self.transfer_bcis.contains(&sp.bci)
                    || !published_by_a_site
                        .get_or_init(|| self.snapshots_published_by_a_site())
                        .contains(&index))
            {
                self.unreachable_points_skipped
                    .set(self.unreachable_points_skipped.get() + 1);
                continue;
            }
            points.push(DeoptimizationPoint {
                native_offset,
                bci: sp.bci as u32,
                reason,
                action: DeoptAction::Reinterpret,
                // Derived like the two guard producers, even though these are
                // plain resume points rather than speculations: the id is a
                // function of the `(bci, reason)` the point already publishes,
                // so stamping it costs nothing and keeps the whole IR tier on
                // one rule. `TransferToInterpreter` is its own reason, so the
                // id is distinct from the `UncommonTrap` guard's at the same
                // bci — which is the distinction M4 exists to make.
                //
                // NOTE: a de-spec recorded against this id withdraws nothing,
                // because no emitter gate asks about `TransferToInterpreter`;
                // it is the guard reasons that are consulted before a
                // speculation is put down.
                speculation_id: crate::deopt::speculation_id(sp.bci as u32, reason),
                frame_state,
                semantics: ResumeSemantics::for_reason(reason),
            });
        }
        self.building_table_points.set(false);
        points.sort_by_key(|p| p.native_offset);
        points.dedup_by_key(|p| p.native_offset);
        points
    }
}
// ── Resource bounds (checked BEFORE anything is reserved) ────────────
//
// The lowerer used to size its frame straight from `graph.nodes.len()` — one
// 8-byte spill slot per node, unconditionally — and then `assert!` its way out
// if a slot fell past the cap. Two problems: `nodes * 8` on a pathological
// graph overflows the `i32` frame arithmetic before any check runs, and the
// assertion is a panic on the compiler thread for what is only a compiler
// resource limit. Both are now decided up front, from the same numbers the
// frame is actually built from — and the spill term itself is now the
// liveness-coloured slot count ([`plan_slots`]) rather than the node count.

/// What a graph demands of the frame beyond its own spills. Shared by
/// [`Lowerer::new`] (which builds the frame) and [`estimate_frame_bytes`]
/// (which bounds it), so the checked size and the built size cannot drift.
pub(in crate::ir_lower) struct FrameNeeds {
    /// The method takes the VM context pointer as a hidden first argument and
    /// reserves a frame slot for it.
    pub(in crate::ir_lower) needs_context: bool,
    /// Widest outgoing Java-argument list staged for a call.
    pub(in crate::ir_lower) max_call_args: usize,
    /// VEC21-7: the frame saves [`IR_LOWER_SAVED_XMMS_VECTOR`] instead of
    /// [`IR_LOWER_SAVED_XMMS`] ([`vector_xmm_saves_wanted`]). Set by the two
    /// callers that hold the schedule; `scan_frame_needs` answers `false`.
    pub(in crate::ir_lower) vector_xmm_saves: bool,
}

/// VEC21-7 (the r23 L8 close, L8-2): does this body save XMM8-XMM11 for its
/// vector pre-loops? Only where they are non-volatile (Windows) and only when
/// the planner admits a loop, so every other method's frame is unchanged.
/// `CRATONVM_JIT_IR_VECTOR_SAVE_XMM=0` kills it (default ON, perf). Read per
/// compile; the frame estimate and `Lowerer::new` call it with the same
/// graph and schedule, so they agree.
pub(in crate::ir_lower) fn vector_xmm_saves_wanted(graph: &Graph, schedule: &Schedule) -> bool {
    !IR_LOWER_SAVED_XMMS_VECTOR.is_empty()
        && IR_LOWER_SAVED_XMMS_VECTOR != IR_LOWER_SAVED_XMMS
        && crate::ir_vector_loop::ir_vector_loop_enabled()
        && crate::x64::has_avx2()
        && !crate::ir_vector_loop::plan_vector_loops(graph, schedule, &|c| {
            ctrl_block_of(graph, schedule, c)
        })
        .0
        .is_empty()
}

/// Scan the graph for the call/allocation shapes that widen the frame.
pub(in crate::ir_lower) fn scan_frame_needs(graph: &Graph, helpers: &JitRuntimeHelpers) -> FrameNeeds {
    // JIT r25 wave 10 (W10-2b): the baseline body's ABI as a floor
    // (`Graph::context_abi_floor`, `CRATONVM_JIT_IR_CONTEXT_ABI_OF_BASELINE`).
    let mut needs_context = graph.context_abi_floor;
    let mut max_call_args = 0usize;
    // An implicit exception edge's flag was seen (see the `CallThrew` arm).
    let mut implicit_throw_pad = false;
    for n in &graph.nodes {
        // A bridged `invokedynamic` (round 16, lane indy) stages its arguments
        // in the same region and hands the bridge the same context word, and
        // so does a `multianewarray` its dimensions (round 16, lane matrix).
        if matches!(
            n.op,
            Op::Call { .. } | Op::IndyBridge { .. } | Op::MultiNewArray { .. }
        ) {
            needs_context = true;
            // inputs = [ctrl, mem, args…]
            let mut words = n.inputs.len().saturating_sub(2);
            // JIT round 22 (lane memory, FFM22-3): an `invokeExact` may be a
            // constant bound downcall whose segment the compiled caller
            // checks, staging `FFM_SEG_STAGE_EXTRA` words after its arguments
            // (`Lowerer::emit_ffm_seg_call`).
            if let Op::Call { info_ptr } = n.op {
                if info_ptr != 0 && ffm_seg_stage_site(info_ptr) {
                    words += FFM_SEG_STAGE_EXTRA;
                }
            }
            max_call_args = max_call_args.max(words);
        }
        if matches!(n.op, Op::LambdaIntToDouble) {
            needs_context = true;
        }
        if helpers.getfield != 0 && matches!(n.op, Op::Load(_)) {
            needs_context = true;
        }
        // COV-03: `jit_putfield_object` is the one putfield helper that takes
        // the VM context pointer (it needs the heap to run the SATB and card
        // barriers). Without this the lowering would load arg0 from an
        // unreserved `context_slot_off` — the single-pass backend's identical
        // `needs_heap` bug, which handed the helper a stack address to
        // dereference as a `SharedVm` (Tomcat's `Catalina.setParentClassLoader`,
        // a bare `aload_0; aload_1; putfield; return` with no other heap op).
        // The wide `putfield_{long,float,double}` helpers take no context.
        if matches!(n.op, Op::Store(MemKind::Ref)) {
            needs_context = true;
        }
        // `jit_aastore` takes the VM context as arg0, exactly like
        // `jit_putfield_object` above -- and for exactly the same reason the
        // line above exists: without the reservation the lowering loads arg0
        // from an unreserved `context_slot_off` and hands the helper a stack
        // address to dereference as a `SharedVm`.
        if matches!(n.op, Op::ArrayStore(MemKind::Ref)) {
            needs_context = true;
        }
        // zt t19/zb2: under an armed ZGC load barrier `aaload` is a call of
        // `jit_aaload`, which takes the VM context as arg0 (the
        // `ArrayLoad(Ref)` arm). Unarmed it is an inline load and reserves
        // nothing, as before.
        // t1 wave 8 (lane LAMBDACAP; `CRATONVM_JIT_IR_ZGC_AALOAD_CONTEXT_EXACT`,
        // default ON): reserve it only where the `ArrayLoad(Ref)` arm really
        // calls the helper -- the predicate it and the single-pass scan
        // (`bytecode_compat`'s `0x32` arm) both use. Under colored stores
        // without the flip the load decodes inline and reads no context, so
        // the broad answer gave the C2 body a context its C1 twin lacked and
        // the supersede forward was refused ("context ABI").
        if helpers.aaload != 0
            && matches!(n.op, Op::ArrayLoad(MemKind::Ref))
            && (if ir_zgc_aaload_context_exact_enabled() {
                crate::x64::zgc_ref_element_load_calls_helper()
            } else {
                crate::x64::zgc_read_barrier_blocks_inline_fields()
            })
        {
            needs_context = true;
        }
        if matches!(n.op, Op::New { .. }) {
            needs_context = true;
        }
        // nio21 w15 L3: `emit_new_object_cp_stub` loads the VM context into
        // ARG0, as `Op::New`'s stub does.
        if matches!(n.op, Op::NewCp { .. }) {
            needs_context = true;
        }
        // cov-06: `emit_new_array_stub` loads the VM context into ARG0, same
        // as `Op::New`'s `emit_new_object_stub`.
        if matches!(n.op, Op::NewArray { .. }) {
            needs_context = true;
        }
        // cov-01: all three take the VM context pointer as their helper's arg0.
        // `Op::LoadStatic` needs it even on the direct route it usually takes,
        // because the route is chosen per SITE at compile time and a single
        // helper-served site in the method is enough — deciding it here, from
        // the graph, keeps the frame layout independent of that choice.
        if matches!(
            n.op,
            Op::ConstString { .. } | Op::ConstClass { .. } | Op::LoadStatic { .. }
        ) {
            needs_context = true;
        }
        // Every `jit_putstatic_*` takes the VM context as arg0 too.
        if matches!(n.op, Op::StoreStatic { .. }) {
            needs_context = true;
        }
        // cov-05: `jit_instanceof`/`jit_checkcast` take the VM context
        // pointer as arg0, same as the three above.
        if matches!(n.op, Op::InstanceOf { .. } | Op::CheckCast { .. }) {
            needs_context = true;
        }
        // `emit_monitor_stub` loads `jit_monitor_enter`/`_exit`'s VM context
        // from `context_slot_off` too. Without this a method whose only
        // context user is a `synchronized` block got `context_slot_off == 0`
        // and handed the helper `[rbp]` -- the saved caller RBP, a stack
        // address -- as its `SharedVm` (Spring `DataBufferUtilsTests`, netty
        // buffer suites: SIGSEGV in `flush_thread_satb` / `LockSlots::lease`).
        if matches!(n.op, Op::MonitorEnter | Op::MonitorExit) {
            needs_context = true;
        }
        // nio21 wave 5 (lane L2): an exception edge's pad hands
        // `jit_local_handler_lookup` the VM context. A call-site edge's call
        // reserves it already; an implicit site's trapping node (an array
        // access, a division) may be the method's only candidate, and
        // `plan_call_throw_edges_armed` arms nothing without the slot.
        if matches!(n.op, Op::CallThrew { .. }) {
            // t1 (lcap, kernel `q`): an IMPLICIT site's flag -- anchored on a
            // trapping node (a division's guard, an array or field access),
            // not a call -- is only remembered here; see
            // `ir_implicit_edge_context_needs_another_use_enabled` below.
            let anchor = n.inputs.get(1).and_then(|&a| graph.nodes.get(a as usize));
            let implicit = anchor.is_some_and(|a| {
                !matches!(
                    a.op,
                    Op::Call { .. } | Op::IndyBridge { .. } | Op::MultiNewArray { .. }
                )
            });
            if implicit {
                implicit_throw_pad = true;
            } else {
                needs_context = true;
            }
        }
    }
    // t1 (lcap, kernel `q`; `CRATONVM_JIT_IR_IMPLICIT_EDGE_CONTEXT_NEEDS_ANOTHER_USE`,
    // default ON, `=0` restores the old reservation): an implicit exception
    // edge's pad is optional -- unarmed, its check deopts exactly as the
    // single-pass body's does -- so it does not make a body take the VM
    // context on its own. With the old answer a heap-free body whose only
    // context reader was that pad (`try { return a / b; } catch (..)`) took
    // the context its C1 twin lacks, the pad was then often refused anyway
    // (`ir-implicit-throw ... NOT armed`), and the entry-ABI mismatch kept
    // every baked caller on C1 (`c2-supersede ... KEPT entrant (context ABI:
    // the C2 body takes the VM context, the C1 body does not)`). A body that
    // takes the context for any other reason still arms its implicit pads.
    if implicit_throw_pad && !ir_implicit_edge_context_needs_another_use_enabled() {
        needs_context = true;
    }
    // JIT round 21 (lane lambdacap): a graph that built a capturing lambda's
    // proxy keeps the context ABI after escape analysis removed it. See
    // [`ir_lambda_proxy_keeps_context_enabled`].
    if !needs_context
        && !graph.lambda_proxy_alloc_classes.is_empty()
        && ir_lambda_proxy_keeps_context_enabled()
    {
        needs_context = true;
    }
    FrameNeeds {
        needs_context,
        max_call_args,
        vector_xmm_saves: false,
    }
}

/// `CRATONVM_JIT_IR_IMPLICIT_EDGE_CONTEXT_NEEDS_ANOTHER_USE` -- entry-ABI
/// consistency, **default ON**, `=0` restores the old reservation (t1, page
/// `t1-lcap-implicit-exception-edges-give-c2-a-context-c1-lacks`): an implicit
/// exception edge's pad alone does not make [`scan_frame_needs`] reserve the VM
/// context (see the note after its node loop). Read per compile (no `static`).
pub(crate) fn ir_implicit_edge_context_needs_another_use_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_ZGC_AALOAD_CONTEXT_EXACT` -- entry-ABI consistency,
/// **default ON**, `=0` restores the broad answer (t1 wave 8, lane LAMBDACAP;
/// page `docs/feature-designs/zgc-architecture.md`).
///
/// [`scan_frame_needs`] reserved the context slot for an `ArrayLoad(Ref)`
/// whenever the ZGC read barrier was armed, while the lowering calls
/// `jit_aaload` (the only context reader of that node) only under
/// `x64::zgc_ref_element_load_calls_helper` -- armed AND (no colored inline
/// decode OR the color flip). The single-pass scan asks the narrow predicate.
/// So under colored stores without the flip, a method whose only context
/// user is a reference-array load had a context-free C1 body and a
/// context-taking C2 body, and `not_entrant::supersede_forward_refusal`
/// kept every baked caller on C1. Read per compile (no `static`).
pub(crate) fn ir_zgc_aaload_context_exact_enabled() -> bool {
    true
}

/// Does the `Op::Call` of `info_ptr` reserve [`FFM_SEG_STAGE_EXTRA`] more
/// stage words (FFM22-3)? An `invokeExact` while the switch is on.
pub(in crate::ir_lower) fn ffm_seg_stage_site(info_ptr: usize) -> bool {
    // SAFETY: as every other read of an `Op::Call`'s `info_ptr` in this file
    // -- a live `JitInvokeInfo` owned by `ir_call_infos`.
    let info: &JitInvokeInfo = unsafe { &*(info_ptr as *const JitInvokeInfo) };
    info.method_name == "invokeExact"
        && info.class_name == "java/lang/invoke/MethodHandle"
        && ir_ffm_segment_inline_enabled()
}

/// `CRATONVM_JIT_IR_LAMBDA_PROXY_KEEPS_CONTEXT` -- perf, **default ON**, `=0`
/// restores the old answer (JIT round 21, lane lambdacap; page
/// `jr21-tlocal-capture-new-kernel-20261014.md`, "Round 21 lane lambdacap").
///
/// A graph that built a capturing lambda's proxy inline
/// ([`Graph::lambda_proxy_alloc_classes`], `IrBuilder::try_indy_capture_new`)
/// takes the VM context as its hidden first argument even when escape
/// analysis removed the proxy and nothing else in the body reads the slot.
/// The single-pass body of the same method always takes it (its
/// `invokedynamic` / `invokeinterface` set `needs_heap`), and every route that
/// moves a caller from the superseded body to this one asks for the SAME entry
/// ABI: hm5's supersede forward and the not-entrant stub's forward
/// (`not_entrant::supersede_forward_admissible`: `new.needs_context ==
/// old.needs_context`), and a late-static / cycle-edge cell
/// (`JitCycleEdgeCell::admits`, staged with the context). With the lambda impl
/// plan (`CRATONVM_JIT_IR_LAMBDA_IMPL_CAPTURING`) the proxy is the only context
/// user of a kernel such as `s += f.getAsInt()` on `() -> j + k`, so its
/// optimizing body was context-free, every caller stayed on (or was
/// re-dispatched by name around) the single-pass body, and the host measured
/// the planned rows 3.3x slower than the interface call. Reserving the slot
/// costs one argument register and one frame store per call. Read per compile
/// (no `static`).
pub(crate) fn ir_lambda_proxy_keeps_context_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_INSTANCEOF_FINAL_MISS` for the optimizing tier's inline
/// `instanceof` (round 9 wave 9, irl9). The same variable, the same default
/// (on) and the same reader as `x64::op_object::instanceof_final_miss_enabled`,
/// which is `pub(crate)` inside `x64` and so not reachable from here; keep the
/// two in step (`NOTES-w9-irl9.md` asks for one shared definition).
pub(in crate::ir_lower) fn ir_instanceof_final_miss_enabled() -> bool {
    true
}

/// The final `java/lang` classes whose `instanceof` miss is definite for a
/// plain object of any other (non-proxy) class id. A copy of
/// `x64::op_object::instanceof_miss_is_final_boot_class` (`pub(crate)`, not
/// reachable from this module) -- read that function's doc for the three
/// reasons each entry must satisfy, and keep the two lists identical; the
/// test `ir_instanceof_final_boot_list_matches_the_single_pass_list` in this
/// file's test module pins the contents.
pub(in crate::ir_lower) fn ir_instanceof_miss_is_final_boot_class(name: &str) -> bool {
    matches!(
        name,
        "java/lang/String"
            | "java/lang/Integer"
            | "java/lang/Long"
            | "java/lang/Short"
            | "java/lang/Byte"
            | "java/lang/Character"
            | "java/lang/Boolean"
            | "java/lang/Float"
            | "java/lang/Double"
    )
}

/// Estimated peak frame requirement, in bytes, for a method the lowerer would
/// build with `num_locals` locals, `spill_slots` distinct spill slots and
/// `needs`.
///
/// Mirrors [`Lowerer::new`]'s layout exactly — locals, optional context slot,
/// the five reserved bookkeeping slots, the spill reservation, the outgoing
/// argument staging area, the 16-byte stack-argument reserve and the 32-byte
/// ABI shadow space, rounded up to the 16-byte stack alignment — but in
/// saturating `usize` arithmetic, so a graph big enough to overflow the
/// constructor's `i32` math is *counted* rather than wrapped. `usize::MAX`
/// therefore reads as "far too large", which is exactly what
/// [`check_frame_size`] concludes.
///
/// `spill_slots` is [`SlotPlan::slots`] — the number of *colours* the liveness
/// colouring needed, i.e. the maximum number of simultaneously live values plus
/// the pinned classes. It used to be `graph.nodes.len()`, which assumed every
/// value in the graph was live at once; a long straight-line method now pays
/// for its working set instead of for its history. The two callers
/// ([`lower_inner`]'s bound and [`Lowerer::new`]'s layout) pass the same
/// `SlotPlan`, so "the frame we checked" and "the frame we build" stay the same
/// number by construction.
pub(in crate::ir_lower) fn estimate_frame_bytes(num_locals: usize, spill_slots: usize, needs: &FrameNeeds) -> usize {
    let locals = num_locals.saturating_mul(8);
    let context = if needs.needs_context { 8 } else { 0 };
    // safepoint id, cached thread pointer, shadow save-base, shadow save-top,
    // phi parallel-copy scratch. Mirrors `bookkeeping_size` in `Lowerer::new`;
    // the `debug_assert_eq!` there is what keeps the two counts equal.
    let bookkeeping = 8usize * 5;
    let spills = spill_slots.saturating_mul(8);
    let args_stage = needs.max_call_args.saturating_mul(8);
    let shadow = 32usize;
    let stack_arg_reserve = 16usize;
    // Cast: `ir_saved_xmm_bytes_with` returns 0, 32 or (VEC21-7) 96.
    let saved_xmms = ir_saved_xmm_bytes_with(needs.vector_xmm_saves) as usize;
    // Cast: `ir_saved_gpr_bytes` returns 0 or 40.
    let saved_gprs = ir_saved_gpr_bytes() as usize;
    // Cast: `ir_deopt_regs_bytes` returns 0 or 256.
    let deopt_regs = ir_deopt_regs_bytes() as usize;
    let total = locals
        .saturating_add(context)
        .saturating_add(bookkeeping)
        .saturating_add(spills)
        .saturating_add(saved_xmms)
        .saturating_add(saved_gprs)
        .saturating_add(deopt_regs)
        .saturating_add(args_stage)
        .saturating_add(shadow)
        .saturating_add(stack_arg_reserve);
    // 16-byte alignment, saturating rather than wrapping at the top end.
    total.saturating_add(15) & !15usize
}

/// Refuse a graph whose node count exceeds the compile-time budget.
///
/// Defence in depth: `lib.rs` already screens on `ir::IR_MAX_GRAPH_NODES`
/// before building a schedule. This restates the bound at the point that
/// *allocates* per node, so a future caller that skips the screen cannot
/// reserve an unbounded frame.
pub(in crate::ir_lower) fn check_graph_size(node_count: usize, limit: usize) -> CompileResult<()> {
    if node_count > limit {
        return Err(Bailout::new(BailoutReason::GraphTooLarge {
            nodes: node_count,
            limit,
        }));
    }
    Ok(())
}

/// Refuse a frame estimate that exceeds the frame budget.
///
/// The bound is a correctness bound, not a stack-consumption policy: this file
/// encodes oop-map slot offsets as `i16`, so a reference living past
/// `i16::MAX` is *unrepresentable* in the map the collector reads. See
/// [`DEFAULT_MAX_FRAME_BYTES`].
pub(in crate::ir_lower) fn check_frame_size(frame_bytes: usize, limit: usize) -> CompileResult<()> {
    if frame_bytes > limit {
        return Err(Bailout::new(BailoutReason::FrameTooLarge {
            bytes: frame_bytes,
            limit,
        }));
    }
    Ok(())
}

// ── Phi parallel copy: the `ValueLoc` view of a frame word ───────────

/// The [`ValueLoc`] this backend uses for the frame word at `[rbp - off]`.
///
/// [`resolve_parallel_copy`] only ever *compares* locations — it never
/// dereferences one, and the scratch is deliberately unnamed in [`CopyOp`] — so
/// the payload has to be an injective token for "this frame word" and nothing
/// more. The rbp-relative byte offset is exactly that: it is what
/// [`Lowerer::slot_of`] hands out, it is positive and bounded by
/// [`DEFAULT_MAX_FRAME_BYTES`], and no arithmetic is done to it on the way in or
/// out.
///
/// Deliberately NOT `off / 8`, even though every offset this file produces is on
/// the 8-byte grid. A division would map two distinct offsets onto one location
/// the moment that ever stopped being true, and `resolve_parallel_copy` DROPS a
/// copy whose destination and source are the same location — so the aliasing
/// would silently delete a real move, which is the same class of wrong-code bug
/// as the sequential emission this replaces.
pub(in crate::ir_lower) fn frame_word_loc(off: i32) -> CompileResult<ValueLoc> {
    u32::try_from(off)
        .ok()
        .filter(|&o| o != 0)
        .map(ValueLoc::Slot)
        .ok_or_else(|| {
            Bailout::with_context(
                BailoutReason::Internal("ir_lower: a phi copy names a non-frame location"),
                format!("offset {off}"),
            )
        })
}

/// One phi's incoming value on one edge: which phi, where its home word is,
/// and **which node** the value comes from as well as where that node's home
/// word is.
///
/// The source NODE is the addition. Without it an edge copy can only be
/// expressed as memory-to-memory — `load rax, [src]; store [dst], rax` — and
/// that is what put a store and a load of the same word two instructions apart
/// at the top of every loop this backend compiles (`JIT_OPTIMIZATION.md`, "the
/// residual 1.36x: latency, not volume"). With the node in hand, a source that
/// is already register-resident is read from its register, and a destination
/// that is register-resident is published from RAX rather than reloaded out of
/// the word just written.
#[derive(Clone, Copy)]
pub(in crate::ir_lower) struct PhiCopy {
    pub(in crate::ir_lower) phi: NodeId,
    pub(in crate::ir_lower) dst: i32,
    pub(in crate::ir_lower) src: NodeId,
    pub(in crate::ir_lower) src_slot: i32,
}

/// Inverse of [`frame_word_loc`].
///
/// A [`ValueLoc::Reg`] is unreachable here: the parallel-copy resolver runs over
/// frame words only, and this backend's register residency is layered on top
/// of the word-level sequence by `emit_phi_copies` rather than expressed in
/// `ValueLoc`. It is representable in the shared type, so it is refused rather
/// than mis-emitted as an offset.
pub(in crate::ir_lower) fn frame_word_off(loc: ValueLoc) -> CompileResult<i32> {
    match loc {
        ValueLoc::Slot(off) => i32::try_from(off).map_err(|_| {
            Bailout::with_context(
                BailoutReason::Internal("ir_lower: a phi copy offset does not fit the frame"),
                format!("{loc:?}"),
            )
        }),
        ValueLoc::Reg(_) => Err(Bailout::with_context(
            BailoutReason::Internal(
                "ir_lower: a phi copy names a register; this backend keeps every value in a \
                 frame word",
            ),
            format!("{loc:?}"),
        )),
    }
}

/// Sequentialise one edge's `(phi slot, source slot)` parallel copy.
///
/// The gathered pairs are simultaneous — every source is read as it was before
/// any destination is written — so they go through
/// [`resolve_parallel_copy`], which returns an order preserving that semantics
/// and breaks any cycle with one [`CopyOp::Save`] / [`CopyOp::Restore`] pair.
/// Self-copies are dropped by the resolver, so the acyclic majority emits
/// exactly one load + one store per surviving pair, as it always did.
pub(in crate::ir_lower) fn phi_copy_sequence(copies: &[(i32, i32)]) -> CompileResult<Vec<CopyOp>> {
    let mut pairs: Vec<(ValueLoc, ValueLoc)> = Vec::with_capacity(copies.len());
    for &(dst, src) in copies {
        pairs.push((frame_word_loc(dst)?, frame_word_loc(src)?));
    }
    resolve_parallel_copy(&pairs)
}

// ── Pre-emission location verification ───────────────────────────────

/// True for the IR types that occupy a frame slot, i.e. the inputs a lowering
/// reads through [`Lowerer::slot_of`]. `Control`, `Memory` and `Void` inputs
/// are edges, not values, and never have a location.
pub(in crate::ir_lower) fn is_value_ty(ty: IrType) -> bool {
    matches!(
        ty,
        IrType::Int | IrType::Long | IrType::Float | IrType::Double | IrType::Ref
    )
}

/// True iff lowering this op assigns its result a frame slot.
///
/// Must stay in step with `lower_data_node`'s match arms: every arm that calls
/// `alloc_slot` is listed here, plus `Op::Phi` (reserved up front by
/// `prealloc_phi_slots`). Ops that reach `lower_data_node`'s catch-all — they
/// refuse the compile — are deliberately absent, because a value read from one
/// of them has no location either.
///
/// "Must stay in step" used to be enforced by this sentence alone. It is now
/// enforced by `every_ir_op_is_lowered_or_declared_unlowerable` and the two
/// tests beside it, which read this function's body, `lower_data_node`'s arms
/// and `ir::Op`'s own declaration out of the source and compare all three. See
/// `docs/feature-designs/jit-machine-level-and-instruction-selection.md`
/// ("The cheap alternative") for why three enumerations of one set is
/// the shape that produced the monitor defect.
pub(in crate::ir_lower) fn op_defines_result_slot(op: &Op) -> bool {
    matches!(
        op,
        Op::Const(_)
            | Op::ConstF(_)
            | Op::Param(_)
            | Op::Phi
            | Op::ScalarIntrinsic(_)
            | Op::Unbox { .. }
            | Op::ExactClassIs { .. }
            // JIT round 19 (lane osr19): a polymorphic chain head's class word.
            | Op::ClassWordOf
            // Round 11 wave 14 (lane osrmerge): the OSR arm's flag and locals.
            | Op::OsrEntryFlag { .. }
            | Op::OsrLocal(_)
            // nio21 wave 3 L2: the call-site exception edge's flag and throwable.
            | Op::CallThrew { .. }
            // JIT round 19 wave 3 (lane eager3): the class-init test.
            | Op::ClassInitTest { .. }
            | Op::CurrentThreadMirror { .. }
            | Op::VthreadBoundaryTest { .. }
            // JIT round 20 wave 11 (lane pIndy): one root word load.
            | Op::IndyRootRef { .. }
            // JIT round 19 wave 5 (lane handler5, SPLICE4-1): the compiled
            // handler entry's flag and caught throwable.
            | Op::CatchEntryFlag { .. }
            | Op::CaughtException
            | Op::Add
            | Op::Sub
            | Op::Mul
            | Op::Div
            | Op::Rem
            | Op::Neg
            | Op::And
            | Op::Or
            | Op::Xor
            | Op::Shl
            | Op::Shr
            | Op::UShr
            | Op::Cmp(_)
            | Op::LCmp
            | Op::FCmp { .. }
            | Op::I2L
            | Op::L2I
            | Op::I2F
            | Op::I2D
            | Op::L2F
            | Op::L2D
            | Op::F2I
            | Op::F2L
            | Op::F2D
            | Op::D2I
            | Op::D2L
            | Op::D2F
            | Op::Load(_)
            | Op::ArrayLoad(_)
            | Op::ArrayStore(_)
            | Op::ByteArrayCopy
            | Op::ArrayLength
            // JIT round 21 (lane charat, STRINTR21-5): the String char read.
            | Op::StrCharDecode
            | Op::New { .. }
            | Op::NewArray { .. }
            | Op::Call { .. }
            // cov-01: each defines a result slot — a `Ref` for the two `ldc`
            // constants, the field's value for `getstatic`.
            | Op::ConstString { .. }
            | Op::ConstClass { .. }
            | Op::LoadStatic { .. }
            | Op::LambdaIntToDouble
            | Op::IndyBridge { .. }
            | Op::MultiNewArray { .. }
            | Op::NewCp { .. }
            // cov-05: `instanceof` defines a result slot — the 0/1 `Int`;
            // `checkcast` defines one too — the `Ref` result.
            | Op::InstanceOf { .. }
            | Op::CheckCast { .. }
    )
}

/// Refuse, *before a single byte is emitted*, any graph in which some node the
/// lowerer will emit reads a value that has no frame location at that point.
///
/// This is the structural fix for the `[rbp - 0]` defect. The observed failure
/// (ES `SortingDigestTests` with the IR tier on) was a `pc17 ArrayLoad(Double)`
/// feeding a GVN-collapsed loop phi: the scheduler placed the phi but not the
/// load, so the load had no slot, and `slot_of` handed back the
/// zero-initialised entry — `[rbp - 0]`, the saved caller frame pointer, read
/// as a `double`. The old detection was a sticky bit consulted *after* the
/// whole body had been emitted.
///
/// Three ways a data input can fail to have a location; all three are the same
/// bailout:
///
///  1. the defining node is in no emitted block (dead, or a scheduler
///     omission) — it never reaches `lower_data_node`, so nothing allocates;
///  2. its op emits nothing (`lower_data_node`'s catch-all), so even though it
///     is scheduled it defines no slot;
///  3. it is emitted, but *after* the use — every location here is created at
///     the point of emission, so a later definition is as absent as no
///     definition.
///
/// Phis are exempt from (3): their slots are reserved by `prealloc_phi_slots`
/// before any block is lowered, precisely so a forward branch can copy into
/// one. Phi *inputs* are exempt too — they are written by predecessor edge
/// copies, not read in the phi's own block — so only (1) and (2) apply to them.
///
/// Rejecting here can only lose graphs that the `unallocated_slot_use` latch
/// already rejected after the fact, so it costs no coverage; what it buys is
/// that the refusal happens before emission, where a mutation that drops a
/// node from the schedule is caught by construction rather than by a bit that
/// someone must remember to check.
///
/// Known residual (why the latch is kept rather than deleted): a phi's value
/// inputs are consumed by `emit_phi_copies` at the *predecessor's* terminator,
/// not in the phi's own block, so their ordering constraint is per-CFG-edge —
/// reconstructing it here would duplicate `block_of_ctrl`'s walk. A def that
/// reached its edge copy too late would still be caught by the latch, one
/// phase later. Every *placement* failure — the class that produced the
/// observed bug — is caught here.
pub(in crate::ir_lower) fn verify_data_locations(graph: &Graph, schedule: &Schedule) -> CompileResult<()> {
    // Emission order: blocks in index order; within a block, its data nodes in
    // scheduled order, then its terminator. This is exactly `lower_inner`'s
    // `for block_idx in 0..blocks.len() { lower_block(block_idx) }`.
    let mut emit_index: Vec<Option<usize>> = vec![None; graph.nodes.len()];
    // Which laid-out block each node lands in. Used only to describe a failure:
    // "def after use" means one thing when both sit in a single block (an
    // intra-block scheduling order) and quite another when they do not (a block
    // LAYOUT that put a definition's block after its user's).
    let mut block_of: Vec<Option<usize>> = vec![None; graph.nodes.len()];
    let mut seq = 0usize;
    for (bpos, block) in schedule.blocks.iter().enumerate() {
        for &node_id in &block.nodes {
            if let Some(cell) = emit_index.get_mut(node_id as usize) {
                *cell = Some(seq);
            }
            if let Some(cell) = block_of.get_mut(node_id as usize) {
                *cell = Some(bpos);
            }
            seq += 1;
        }
        if let Some(term) = block.terminator {
            if let Some(cell) = emit_index.get_mut(term as usize) {
                *cell = Some(seq);
            }
            if let Some(cell) = block_of.get_mut(term as usize) {
                *cell = Some(bpos);
            }
            seq += 1;
        }
    }

    let check_user = |user: NodeId| -> CompileResult<()> {
        let node = match graph.nodes.get(user as usize) {
            Some(n) => n,
            None => {
                return Err(Bailout::new(BailoutReason::Internal(
                    "ir_lower: schedule names a node outside the graph",
                )))
            }
        };
        let user_is_phi = matches!(node.op, Op::Phi);
        for &input in &node.inputs {
            if input == NO_NODE {
                continue;
            }
            let def = match graph.nodes.get(input as usize) {
                Some(d) => d,
                None => {
                    return Err(Bailout::new(BailoutReason::Internal(
                        "ir_lower: node input outside the graph",
                    )))
                }
            };
            // Control / memory edges carry no value and are never `slot_of`'d.
            if !is_value_ty(def.ty) {
                continue;
            }
            if !op_defines_result_slot(&def.op) {
                // (1) dead / (2) no lowering ⇒ no location, ever.
                return Err(Bailout::with_context(
                    BailoutReason::UnallocatedValue { node: input },
                    format!(
                        "n{input} ({:?}, {:?}) is read by n{user} ({:?}) but its \
                         lowering assigns no frame slot",
                        def.op, def.ty, node.op
                    ),
                ));
            }
            let def_seq = match emit_index.get(input as usize).copied().flatten() {
                Some(s) => s,
                None => {
                    // Phis are reserved before emission, so "not in any emitted
                    // block" is still fatal for them: nothing would write the
                    // slot. Report the same way.
                    return Err(Bailout::with_context(
                        BailoutReason::UnallocatedValue { node: input },
                        format!(
                            "n{input} ({:?}) is read by n{user} ({:?}) but is in no \
                             emitted block",
                            def.op, node.op
                        ),
                    ));
                }
            };
            if user_is_phi || matches!(def.op, Op::Phi) {
                // Phi slots exist before the first block is lowered, and a
                // phi's own inputs are consumed by predecessor edge copies
                // rather than read in the phi's block — neither is an
                // ordering constraint.
                continue;
            }
            let use_seq = emit_index
                .get(user as usize)
                .copied()
                .flatten()
                .unwrap_or(usize::MAX);
            if def_seq >= use_seq {
                return Err(Bailout::with_context(
                    BailoutReason::UnallocatedValue { node: input },
                    format!(
                        "n{input} ({:?}) is emitted at position {def_seq}, after its \
                         use by n{user} ({:?}) at position {use_seq} \
                         [def in laid-out block {:?}, use in {:?}]",
                        def.op,
                        node.op,
                        block_of.get(input as usize).copied().flatten(),
                        block_of.get(user as usize).copied().flatten(),
                    ),
                ));
            }
        }
        Ok(())
    };

    for block in &schedule.blocks {
        for &node_id in &block.nodes {
            check_user(node_id)?;
        }
        if let Some(term) = block.terminator {
            check_user(term)?;
        }
    }
    Ok(())
}

// ── Liveness-based frame-slot reuse ──────────────────────────────────
//
// The lowerer used to reserve one 8-byte spill slot per graph node, forever: a
// value dead since block 0 still owned a word of the frame at the return. On a
// 4000-node method that is 32 KiB of stack — the entire `DEFAULT_MAX_FRAME_BYTES`
// budget — spent almost entirely on values nothing can read any more, and it is
// why the optimizing tier *declined* anything much past 4000 nodes.
//
// What replaces it is a textbook two-step: compute each value's live range over
// the lowerer's own emission order, then colour the ranges so values that are
// never live at the same time share a word.
//
// ## Positions
//
// The "program points" are the linear emission positions `lower_inner` walks:
// blocks in index order, within a block its scheduled data nodes then its
// terminator, then ONE more position for the block's outgoing edge — the point
// at which `emit_phi_copies` writes this edge's parallel copies and the block's
// jump is emitted. `verify_data_locations` numbers the first two the same way;
// the third is the subtlety that file already documents: **a phi's value inputs
// are consumed at the predecessor's terminator, not in the phi's own block**, so
// they must stay live to the end of every predecessor that feeds them.
//
// ## Liveness
//
// A backward may-analysis over the block CFG:
//
//     live_out[b] = phi_out[b] ∪ ⋃_{s ∈ succ(b)} live_in[s]
//     live_in[b]  = use[b] ∪ (live_out[b] \ def[b])
//
// with `use[b]` the upward-exposed uses, `def[b]` the values the block defines
// and `phi_out[b]` the values `emit_phi_copies` reads on b's outgoing edges.
// Iterated to a fixed point, so a back edge propagates a loop body's uses
// around the loop and a value used anywhere in a loop is live across the whole
// loop — no special case needed, that is what the fixed point *means*.
//
// Each value's range is then the closed interval
//
//     [ min(def position, start of every block it is live-in to),
//       max(use positions,  end   of every block it is live-out of) ]
//
// which is a contiguous over-approximation in position space: every point at
// which the value is live is inside it. That is all interference needs — two
// values live at the same point have overlapping intervals, so refusing to
// share on overlap can never alias two live values. It can *over*-estimate
// (a value live only in blocks 0 and 9 covers 1..8 too), which costs slots, not
// correctness.
//
// ## Colouring
//
// Linear scan over intervals sorted by start, with an expiry heap and a free
// list of released colours. Endpoints are inclusive on both sides — an interval
// starting exactly where another ends does NOT reuse its slot — because a
// node's lowering allocates its result slot *before* reading its operands at
// some sites and after at others, and one word of frame is not worth auditing
// forty emission arms for.
//
// ## What is NOT coloured, and why
//
//   * **Phis.** `prealloc_phi_slots` reserves them before any block is lowered
//     and `emit_phi_copies` writes them from predecessors that may be lowered
//     much later; the whole phi web has to keep one identity, and
//     `zero_ref_phi_slots` additionally publishes every `Ref` phi's slot as a
//     GC root from the prologue onward. Dedicated slot.
//   * **Anything a deopt frame names.** Every node reachable from a
//     `SafepointSnapshot` (and every scalar-replacement field value, when
//     `sr_map` is set) is read by `frame_value_for` at a native offset chosen at
//     runtime — any guard may trap and hand its baked box to the resume path, so
//     the frame state of *any* recorded bci can be consumed. Its slot must still
//     hold its value there.
//     Reconstructing which guard that is belongs to the deopt producer, not to
//     a register allocator. Dedicated slot.
//   * **`Ref` results of `Op::Call`.** `emit_safepoint_map` publishes the slot
//     of every `Ref` node defined so far, and the self-recursive route
//     (`emit_self_recursive_call`) stores the call's result BEFORE
//     `emit_shadow_reload` copies the published values back. A call result that
//     recycled a published colour would be overwritten by the reload. These may
//     *donate* their colour once they die, but never *receive* a recycled one —
//     the colour they take has been handed to nobody, so it cannot be in the
//     published set (any value that later inherits it is defined strictly after
//     this call, hence not yet in `defined_nodes` here).
//
// ## Reference / primitive separation
//
// A colour is `Ref` or `Prim` from its first assignment and never changes
// class: the two free lists are disjoint. `emit_safepoint_map` publishes a slot
// because *some* `Ref` node was defined into it, and `defined_nodes` is
// monotone, so a slot the map names may hold a stale (but genuine) reference —
// that is the pre-existing behaviour and the collector handles it. What must
// never happen is a slot the map names holding an `int`, which separating the
// pools makes unrepresentable rather than unlikely.

/// A closed interval of linear emission positions over which a value must keep
/// its frame slot. See the module section above for how positions are numbered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::ir_lower) struct LiveRange {
    pub(in crate::ir_lower) lo: usize,
    pub(in crate::ir_lower) hi: usize,
}

impl LiveRange {
    /// Endpoint-**inclusive** overlap: `[4, 7]` and `[7, 9]` overlap.
    ///
    /// Deliberately stricter than "live at a common point". A node's lowering
    /// may allocate its result slot before or after loading its operands
    /// depending on the opcode, so a value whose last use is at position `p`
    /// and a value defined at position `p` are kept apart.
    pub(in crate::ir_lower) fn overlaps(self, other: LiveRange) -> bool {
        self.lo <= other.hi && other.lo <= self.hi
    }
}

/// Which pool a value's frame slot is drawn from. A colour's class is fixed at
/// its first assignment; see the section comment on reference separation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::ir_lower) enum SlotClass {
    /// Reference-typed. Shares only with other references — a slot an oop map
    /// names must never hold a primitive.
    Ref,
    /// Non-reference. Shares only with other non-references.
    Prim,
    /// Never shares with anything: phis, deopt-visible values, and any other
    /// class this file cannot prove safe to alias.
    Pinned,
}

/// The frame-slot assignment for one method.
pub(in crate::ir_lower) struct SlotPlan {
    /// `node_color[id]` = the 0-based 8-byte slot index node `id`'s result
    /// lives in, or `None` when the lowerer never allocates for that node.
    pub(in crate::ir_lower) node_color: Vec<Option<u32>>,
    /// `range[id]` = the live range colouring used, kept so
    /// [`verify_slot_colouring`] can re-derive (rather than trust) the
    /// no-aliasing property.
    pub(in crate::ir_lower) range: Vec<Option<LiveRange>>,
    /// `class[id]` = which pool the colour came from.
    pub(in crate::ir_lower) class: Vec<Option<SlotClass>>,
    /// Distinct 8-byte slots the plan needs — what [`estimate_frame_bytes`]
    /// budgets and [`Lowerer::new`] reserves.
    pub(in crate::ir_lower) slots: usize,
    /// Maximum number of live ranges covering any single position: the floor a
    /// perfect colouring with no pinned classes would reach. Reported for
    /// measurement only; nothing is sized from it.
    pub(in crate::ir_lower) peak_live: usize,
    /// The scalar-replacement field values this plan pinned (the third
    /// `SlotClass::Pinned` population), from [`scalar_replacement_field_values`]
    /// -- kept so `plan_register_residency` hands the allocator's liveness model
    /// the SAME set (`LiveModel::pin_values`) without being given the map.
    pub(in crate::ir_lower) sr_field_values: Vec<NodeId>,
    /// `pos_of[id]` = the linear emission position this plan numbered node
    /// `id` at (a scheduled data node or a block terminator), `None` for a node
    /// no block schedules. The numbering every [`LiveRange`] here is written
    /// in. gen r5w2/oomjit6: kept so [`Lowerer::emit_dead_ref_slot_clears`]
    /// can name the position a GC point is emitted at.
    pub(in crate::ir_lower) pos_of: Vec<Option<usize>>,
    /// gen r5w6/oomjit10: `phi_code_unused[id]` = node `id` is a phi that NO
    /// compiled code reads -- no use in any block, no edge copy naming it as
    /// a source, live in no block -- in a plan whose liveness converged. Its
    /// `range` is the whole method only because the colouring pins it; the
    /// dead-reference rule reads such a phi's home as needed by snapshots
    /// alone ([`plan_dead_ref_slot_clear`]).
    pub(in crate::ir_lower) phi_code_unused: Vec<bool>,
    /// Round 14 wave 1 (lane chain): the ranges the COLOURING used, when they
    /// differ from [`Self::range`] -- a value named only by chain snapshots is
    /// kept to every guard that can publish one
    /// ([`chain_read_colour_ranges`]). Empty otherwise. Each entry contains the
    /// value's `range`, so a colouring without overlaps here has none there.
    pub(in crate::ir_lower) colour_range: Vec<Option<LiveRange>>,
    /// How many values [`Self::colour_range`] widened instead of pinning
    /// (`CRATONVM_DBG_IR_SLOTS` reports it).
    pub(in crate::ir_lower) chain_ranged: usize,
    /// JIT round 25 wave 3 (`CRATONVM_JIT_IR_SNAPSHOT_RANGE_COLOURS`, default ON since j27):
    /// what [`apply_snapshot_range_colours`] did, or `None` when it did
    /// nothing (switch off, the keep-everything retry, no candidate, or a fact
    /// it could not establish). The lowerer checks every frame-state read of a
    /// ranged value against [`SnapshotRanged::reads`]
    /// ([`Lowerer::snapshot_ranged_home_ok`]).
    pub(in crate::ir_lower) snapshot_ranged: Option<Box<SnapshotRanged>>,
}

/// JIT round 25 wave 3: the facts behind a snapshot-ranged colouring
/// ([`apply_snapshot_range_colours`]). A RANGED value is a non-reference
/// value that was `Pinned` only because a safepoint snapshot names it; it is
/// coloured like any other `Prim` value, with a colouring range widened over
/// every position the plan treats as reading it through a snapshot.
pub(in crate::ir_lower) struct SnapshotRanged {
    /// Per ranged value, sorted and distinct: every position at which the plan
    /// made its home readable -- the snapshot reads the widening's dataflow
    /// took as uses, its ordinary uses and its definition. A frame state that
    /// describes the value by its home anywhere else is refused.
    pub(in crate::ir_lower) reads: HashMap<NodeId, Vec<usize>>,
    /// Per ranged value, the bcis whose block starts read it (an OSR entry
    /// stub at that bci seeds or describes it there).
    pub(in crate::ir_lower) entry_bcis: HashMap<NodeId, Vec<usize>>,
    /// The plan's block spans, `(first position, outgoing-edge position)`.
    pub(in crate::ir_lower) span: Vec<(usize, usize)>,
    /// The colour count before the ranging (every ranged value pinned).
    pub(in crate::ir_lower) slots_before: usize,
}

impl SlotPlan {
    /// JIT round 25 wave 3: the colour count before snapshot ranging (this
    /// plan's own count when nothing was ranged).
    pub(in crate::ir_lower) fn slots_before_ranging(&self) -> usize {
        self.snapshot_ranged
            .as_deref()
            .map_or(self.slots, |r| r.slots_before)
    }

    /// JIT round 25 wave 3: how many values the plan ranged.
    pub(in crate::ir_lower) fn snapshot_ranged_count(&self) -> usize {
        self.snapshot_ranged.as_deref().map_or(0, |r| r.reads.len())
    }

    /// JIT round 25 wave 8 (W8-6): how many of the ranged values are
    /// references (only `CRATONVM_JIT_IR_SPLICE_SLOT_SHARE` ranges one).
    pub(in crate::ir_lower) fn snapshot_ranged_ref_count(&self, graph: &Graph) -> usize {
        self.snapshot_ranged.as_deref().map_or(0, |r| {
            r.reads
                .keys()
                .filter(|&&id| graph.nodes.get(id as usize).is_some_and(|n| n.ty == IrType::Ref))
                .count()
        })
    }

    /// The ranges the colouring was computed against: [`Self::colour_range`]
    /// when present, else [`Self::range`].
    pub(in crate::ir_lower) fn colouring_ranges(&self) -> &[Option<LiveRange>] {
        if self.colour_range.is_empty() {
            &self.range[..]
        } else {
            &self.colour_range[..]
        }
    }

    /// The colours that provably never hold a reference.
    ///
    /// `assign_colors` keeps two free lists and never moves a colour between
    /// them: a colour first taken by a non-`Ref` value is returned to
    /// `free_prim` and can only ever be recycled by another non-`Ref` value. So
    /// a colour is single-class for the life of the method, and a `Prim` colour
    /// holds a live primitive or dead bytes left by one -- never an object
    /// pointer.
    ///
    /// This is the IR tier's half of the stale-word oracle. Without it an IR
    /// frame carries no dataflow at all and every stale word in it reports as
    /// unexplained, which is exactly where the last two unattributed words of
    /// the 2026-09-02 `TestRandomMapOps` measurement were. See
    /// `OopMapEntry::non_oop_stack_slots`.
    pub(in crate::ir_lower) fn prim_colors(&self) -> Vec<u32> {
        // Marked by colour, not searched per node: this runs on the compile
        // path of every IR method, and a linear `contains` would make it
        // quadratic in the colour count on exactly the large graphs that can
        // least afford it.
        let mut seen = vec![false; self.slots];
        for (id, class) in self.class.iter().enumerate() {
            if !matches!(class, Some(SlotClass::Prim)) {
                continue;
            }
            if let Some(Some(color)) = self.node_color.get(id).copied() {
                if let Some(flag) = seen.get_mut(color as usize) {
                    *flag = true;
                }
            }
        }
        seen.iter()
            .enumerate()
            .filter(|(_, &f)| f)
            .map(|(c, _)| c as u32)
            .collect()
    }

    /// JIT round 25 (item 3, `CRATONVM_JIT_IR_PRIMITIVE_COLOUR_CLAIM`):
    /// renumber the colours so that every `Prim` colour comes before every
    /// colour that may hold a reference (any colour [`Self::prim_colors`] does
    /// not list), each group in its old relative order. Colour `c` lives at
    /// `[rbp - (first_spill + 8c)]`, so the `Prim` words -- which the
    /// fresh-frame clear leaves out under the claim -- become one run at the
    /// shallow end of the colour band instead of words scattered between the
    /// cleared ones.
    ///
    /// `Prim` first, not last: a safepoint's live cursor
    /// (`OopMapEntry::live_frame_hi`, the emitter's high-water mark) is what
    /// the collector's dead-spill claim drops words above. With the `Prim`
    /// colours shallowest, a `Prim` definition never raises the cursor past a
    /// rootable colour, and the rootable colours keep their relative order, so
    /// a rootable word is above the cursor at least wherever it was before.
    ///
    /// A bijection on colour numbers, applied to [`Self::node_color`] only:
    /// no other field of the plan names a colour (`class`, the ranges, `pos_of`
    /// and the pinned lists are per node), so every node keeps its class and
    /// range, and two nodes share a colour after exactly when they did before
    /// -- `verify_slot_colouring` re-derives that on the result. `slots` is
    /// unchanged, so is the frame. Returns whether any colour moved.
    pub(in crate::ir_lower) fn group_prim_colours_first(&mut self) -> bool {
        let mut is_prim = vec![false; self.slots];
        for c in self.prim_colors() {
            if let Some(flag) = is_prim.get_mut(c as usize) {
                *flag = true;
            }
        }
        self.group_colours_first(is_prim)
    }

    /// JIT round 25 wave 2 (`CRATONVM_JIT_IR_PINNED_PRIM_COLOUR_CLAIM`): the
    /// `Pinned` colours whose one occupant `claimable` marks
    /// ([`claimable_pinned_nodes`]), ascending. A `Pinned` colour has exactly
    /// one occupant (`verify_slot_colouring` refuses a pinned value that
    /// shares), so the colour holds that node's values and nothing else.
    pub(in crate::ir_lower) fn claimable_pinned_colors(&self, claimable: &[bool]) -> Vec<u32> {
        let mut seen = vec![false; self.slots];
        for (id, &c) in claimable.iter().enumerate() {
            if !c || self.class.get(id).copied().flatten() != Some(SlotClass::Pinned) {
                continue;
            }
            if let Some(Some(color)) = self.node_color.get(id).copied() {
                if let Some(flag) = seen.get_mut(color as usize) {
                    *flag = true;
                }
            }
        }
        seen.iter()
            .enumerate()
            .filter(|(_, &f)| f)
            // Cast: a colour index, far below `u32::MAX`.
            .map(|(c, _)| c as u32)
            .collect()
    }

    /// JIT round 25 wave 2: [`Self::group_prim_colours_first`] with the
    /// claimable `Pinned` colours ([`Self::claimable_pinned_colors`]) in the
    /// first group too, so every word the per-body claim names in the colour
    /// band is one run at its shallow end. The same bijection, verified the
    /// same way (`verify_slot_colouring` runs after it).
    pub(in crate::ir_lower) fn group_claimed_colours_first(&mut self, claimable: &[bool]) -> bool {
        let mut first = vec![false; self.slots];
        for c in self
            .prim_colors()
            .into_iter()
            .chain(self.claimable_pinned_colors(claimable))
        {
            if let Some(flag) = first.get_mut(c as usize) {
                *flag = true;
            }
        }
        self.group_colours_first(first)
    }

    /// Renumber the colours so that every colour `first` marks comes before
    /// every other, each group in its old relative order (a bijection on
    /// colour numbers, applied to [`Self::node_color`] only). Returns whether
    /// any colour moved; nothing moves when `first` marks none or all.
    pub(in crate::ir_lower) fn group_colours_first(&mut self, first: Vec<bool>) -> bool {
        let marked = first.iter().filter(|&&f| f).count();
        if marked == 0 || marked >= self.slots || first.len() != self.slots {
            return false;
        }
        let is_prim = first;
        let mut remap = vec![0u32; self.slots];
        let mut next = 0u32;
        for want_prim in [true, false] {
            for (c, &p) in is_prim.iter().enumerate() {
                if p == want_prim {
                    remap[c] = next;
                    next += 1;
                }
            }
        }
        // Cast: a colour index, far below `u32::MAX`.
        let moved = remap.iter().enumerate().any(|(c, &n)| c as u32 != n);
        if moved {
            for colour in self.node_color.iter_mut().flatten() {
                if let Some(&n) = remap.get(*colour as usize) {
                    *colour = n;
                }
            }
        }
        moved
    }
}

/// JIT round 20 wave 20 (lane refspin): the spliced `Reference.referent`
/// reads (`Graph::referent_load_pcs`) -- and the `Ref` value phis of non-header
/// blocks that merge one, such as a guarded splice's join of its inline read
/// with the miss arm's call -- whose home holds nothing any code, deopt or
/// mode exit reads at the back-edge poll of `back_edge_block`, as `(node,
/// colour)`. [`Lowerer::emit_back_edge_dead_home_clears`] zeroes their homes
/// there, so the poll's map, the frame block and the band scan stop rooting a
/// referent the program no longer holds (HotSpot's map at that poll does not
/// name it).
///
/// Such a value `L` (a phi's block is its merge's) qualifies only when every
/// one of these holds, and the answer is empty (zero nothing) whenever a fact
/// cannot be established:
///
/// * **the poll's position is known**: the slot plan's numbering (each
///   scheduled node, the terminator, one outgoing-edge position per block) is
///   re-derived here and checked against `pos_of` for every block up to this
///   one. The poll sits at the terminator's position (`lower_terminator` polls
///   before its branch) or, with no terminator, at the outgoing-edge position;
/// * **this is a back edge of a loop that contains `L`**: every successor at or
///   before the block dominates it (a reducible back edge), and dominates `L`'s
///   block. So every path from the poll re-enters the header and runs `L`
///   again before reaching any point `L` dominates: no node, and no frame
///   state that names `L` (a deopt, a rethrow pad, a later mode exit), can
///   read the zeroed word before `L` rewrites it. The only reads this leaves
///   are the ones AT the latch: a header phi's incoming edge (its copy, a
///   header mode exit), and the terminator, which lowers after the poll
///   together with any compare it fuses -- `L` must feed none of them (it may
///   feed a value phi only when that phi is itself a candidate, i.e. merges
///   inside the loop, where its copy runs before the poll);
/// * **no other live value shares the word**: no other node of `L`'s colour
///   has a colouring range covering the poll's position (a value live at the
///   poll -- one the terminator reads, one live out of the block, an edge
///   copy's source -- has its range cover that position). `L`'s own range is
///   not asked: by default a memory-token use counts as a use, so it runs to
///   the header memory phi's edge although no code reads the home there;
/// * `L` is no pinned promoted / scalar-replaced value
///   (`SlotPlan::sr_field_values`, written back by deopt pads), and no value
///   whose colouring range chain snapshots widened.
///
/// JIT round 20 wave 21 (lane pollroot): the special case of
/// [`back_edge_dead_ref_homes`] with `general == false`, which is where the
/// argument now lives. Review fix carried with it (unconditional): a latch
/// whose branch can also LEAVE the loop (a do-while's conditional back edge,
/// a rotated loop) used to be judged as if every path from the poll re-entered
/// the header, so a referent read live after the loop could be zeroed under
/// its post-loop reader.
pub(in crate::ir_lower) fn referent_loads_dead_at_back_edge(
    graph: &Graph,
    schedule: &Schedule,
    plan: &SlotPlan,
    back_edge_block: usize,
) -> Vec<(usize, u32)> {
    back_edge_dead_ref_homes(graph, schedule, plan, back_edge_block, false)
}

/// JIT round 20 wave 20/21: the linear position of the back-edge poll of
/// `back_edge_block` in the slot plan's numbering (each scheduled node, the
/// terminator, one outgoing-edge position per block), re-derived here and
/// checked against `pos_of` for every block up to this one; `None` when the
/// two disagree. The poll sits at the terminator's position
/// (`lower_terminator` polls before its branch) or, with no terminator, at the
/// outgoing-edge position.
pub(in crate::ir_lower) fn back_edge_poll_pos(
    schedule: &Schedule,
    plan: &SlotPlan,
    back_edge_block: usize,
) -> Option<usize> {
    let block = schedule.blocks.get(back_edge_block)?;
    let pos_of = |id: NodeId| plan.pos_of.get(id as usize).copied().flatten();
    let mut seq = 0usize;
    for blk in schedule.blocks.iter().take(back_edge_block + 1) {
        for (k, &nid) in blk.nodes.iter().enumerate() {
            if pos_of(nid) != Some(seq + k) {
                return None;
            }
        }
        seq += blk.nodes.len();
        if let Some(term) = blk.terminator {
            if pos_of(term) != Some(seq) {
                return None;
            }
            seq += 1;
        }
        // The outgoing-edge position.
        seq += 1;
    }
    let edge = seq - 1;
    Some(if block.terminator.is_some() {
        edge - 1
    } else {
        edge
    })
}

