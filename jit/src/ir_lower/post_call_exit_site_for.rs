#[allow(unused_imports)]
use super::*;

impl<'a> Lowerer<'a> {
    /// Lay down one post-call exit site whose point is `frame_state`: the
    /// patchable `NOP` the redefinition's force pass rewrites, and its point,
    /// queued for [`Self::emit_post_call_exit_stubs`].
    pub(in crate::ir_lower) fn emit_post_call_exit_site_for(&mut self, frame_state: FrameState) {
        if frame_state.caller.is_some() {
            // A call inside a splice (wave 47, lane L3).
            self.post_call_chain_sites += 1;
        }
        let reason = DeoptReason::OsrExit;
        let bci = frame_state.bci;
        let point = Box::new(DeoptimizationPoint {
            // Cast: a buffer offset, as every other point records it.
            native_offset: self.buf.pos() as u32,
            bci,
            reason,
            action: DeoptAction::Reinterpret,
            speculation_id: crate::deopt::speculation_id(bci, reason),
            frame_state,
            semantics: ResumeSemantics::for_reason(reason),
        });
        let point_ptr = point.as_ref() as *const DeoptimizationPoint as u64;
        // Kept apart from the guard facts, like the poll exits' points, and
        // appended to the artifact's boxes with them.
        self.poll_exit_boxes.push(point);
        // Five bytes inside one 8-aligned word, so the force is one aligned
        // store (`not_entrant::post_call_exit_jump` checks it again). The
        // buffer is the body's final mapping, so this is the site's address.
        // Cast: the site's absolute address, for its alignment only.
        let here = self.buf.as_ptr() as usize + self.buf.pos();
        let misalign = here % 8;
        if misalign > 8 - crate::ENTRY_PATCH_PAD.len() {
            // A single `NOP` of `8 - misalign` (1 to 4) bytes.
            match 8 - misalign {
                1 => self.buf.emit(&[0x90]),
                2 => self.buf.emit(&[0x66, 0x90]),
                3 => self.buf.emit(&[0x0F, 0x1F, 0x00]),
                _ => self.buf.emit(&[0x0F, 0x1F, 0x40, 0x00]),
            }
        }
        let site = self.buf.pos();
        self.buf.emit(&crate::ENTRY_PATCH_PAD);
        self.post_call_exits_pending.push((site, point_ptr));
        // The census's denominator ([`Self::post_call_exit_refused`]).
        if crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] ir-post-call-exit site at bci={bci} of {}",
                self.graph.scope_method_key
            );
        }
    }

    /// The interpreter frame the call node `call` returns into, resumed at
    /// the invoke's successor (`REEXECUTE` there, the call's result on the
    /// stack: the same interpreter state as `RESUME` after the invoke), or
    /// `None` to emit no site. Fail closed, as
    /// [`Self::back_edge_mode_exit_state`] is:
    ///
    /// * the call is an `invoke*` of the method's own code (not inside a
    ///   splice), and its successor is inside the method;
    /// * the snapshot is the node's own one at the invoke's bci, or the only
    ///   one there (after an unroll the first-by-bci snapshot can name another
    ///   copy's values);
    /// * a lock the frame holds is admitted as the OSR door's back-edge poll
    ///   exits admit one (wave 29, lane L3): its object current at the site
    ///   and in a reference frame word, an elided level only under
    ///   `osr_exit::OSR_POLL_EXITS_WITH_AN_ELIDED_LOCK_ENABLED` (it was
    ///   refused outright before);
    /// * the argument count agrees with the invoke info (the snapshot's top
    ///   `pops` entries are exactly what the call consumed);
    /// * every named value is current right after the call: a constant, a
    ///   parameter, or a value emitted in (or a φ of) a block that dominates
    ///   the call's -- a value the scheduler sank past the call is not, and
    ///   refuses unless its local is dead at the successor;
    /// * every value has a location ([`Self::frame_value_for`], quieted), a
    ///   register one only with the deopt stub's register image, and the frame
    ///   passes `osr_exit::guard_exit_point_resumable`, which the planless
    ///   transfer asks again (`poll_exit_point_resumable`).
    pub(in crate::ir_lower) fn post_call_exit_state(
        &self,
        call: NodeId,
        info_ptr: usize,
    ) -> Option<FrameState> {
        // Gated on the switch of the sink the exit reaches, as
        // `back_edge_mode_exit_state` is: the OSR door's planless transfer, or
        // (a method-entry compile, wave 38) the stash sinks' precise resume,
        // which the door that chose `MethodEntry` already asked.
        let sought = match self.poll_mode_exits {
            IrPollModeExits::OsrDoor => ir_osr_guard_exit_entries_enabled(),
            IrPollModeExits::MethodEntry => true,
            IrPollModeExits::MethodEntryNamedOnly | IrPollModeExits::Off => false,
        };
        if !sought || !crate::deopt_real_enabled() {
            return None;
        }
        let graph = self.graph;
        let node = graph.nodes.get(call as usize)?;
        let pc = node.bytecode_pc?;
        if self.pc_is_in_a_spliced_body(pc) {
            // Interpreter round i1 wave 47, lane L3: a call inside a splice
            // leaves through a caller chain, when the build recorded one.
            return self.post_call_chain_exit_state(call, info_ptr, pc);
        }
        // A caller-held synchronized direct call (the builder's
        // `MonitorEnter` before the node and `MonitorExit` after it, the
        // `Op::Call` arm's first route): right after the call the frame holds
        // a lock no frame state describes, which a resume would leak.
        if self
            .direct_calls
            .contains_key(&crate::compile_request::sync_direct_row_key(pc))
        {
            return None;
        }
        let bytecode = self.poll_exit_bytecode.as_ref()?;
        if pc >= bytecode.code_len {
            return None;
        }
        let invoke_len = match bytecode.code.get(pc).copied()? {
            // invokevirtual, invokespecial, invokestatic.
            0xb6..=0xb8 => 3,
            // invokeinterface, invokedynamic.
            0xb9 | 0xba => 5,
            _ => return None,
        };
        let next = pc + invoke_len;
        if next >= bytecode.code_len {
            return None;
        }
        let bci = u32::try_from(next).ok()?;
        // SAFETY: `info_ptr` names a live `JitInvokeInfo` owned by this
        // compile's `ir_call_infos`, exactly as the `Op::Call` arm reads it.
        let info = unsafe { &*(info_ptr as *const JitInvokeInfo) };
        // JIT round 16 wave 17 (lane mh7): an `invokevirtual` built as the
        // static call of a constant handle's member (`IrBuilder::
        // mh_constant_opcode`) also consumed the handle, which the invoke's
        // snapshot holds below the row's arguments: popping the row's count
        // would leave it on the successor's stack. No site.
        if bytecode.code.get(pc).copied() == Some(0xb6) && info.invoke_kind == 3 {
            return None;
        }
        // JIT round 17 wave 3 (lane mh4; MHCONST17-2): so, too, a virtual
        // handle's member call (an instance row at the same `invokevirtual`).
        if graph.mh_constant_call_pcs.contains(&pc) {
            return None;
        }
        // JIT round 21 wave 17 (lane records, IN16-6): a record fold's
        // `Objects` calls are not the `invokedynamic`'s result, so no frame
        // after the instruction describes their return. No site.
        if graph.record_fold_call_pcs.contains(&pc) {
            return None;
        }
        let pops = node.inputs.len().checked_sub(2)?;
        if info.num_jit_args != pops {
            return None;
        }
        let returns_value = info.return_type != b'V';
        let own = node
            .frame_snapshot
            .and_then(|si| graph.safepoints.get(si as usize))
            .filter(|sp| sp.bci == pc);
        let sp = match own {
            Some(sp) => sp,
            None => {
                if graph.safepoints.iter().filter(|sp| sp.bci == pc).count() != 1 {
                    return None;
                }
                self.safepoint_first_by_bci
                    .get(&pc)
                    .and_then(|&si| graph.safepoints.get(si))?
            }
        };
        if sp.bci != pc {
            return None;
        }
        let keep = sp.stack.len().checked_sub(pops)?;
        self.post_site_frame_state(call, pc, next, bci, sp, keep, returns_value.then_some(call))
    }

    /// The frame a post-call exit site after `anchor` (at `pc`) leaves with,
    /// resumed at `next` (`REEXECUTE`, bci `bci`): the snapshot `sp`'s locals,
    /// its first `keep` stack entries, then `pushed` (the call's result) when
    /// there is one. Every check of [`Self::post_call_exit_state`] past the
    /// call's own shape, shared with
    /// [`Self::post_static_helper_exit_state`] (2026-10-07), whose `sp` is
    /// already the successor's snapshot (`keep` its whole stack, nothing
    /// pushed).
    #[allow(clippy::too_many_arguments)]
    pub(in crate::ir_lower) fn post_site_frame_state(
        &self,
        anchor: NodeId,
        pc: usize,
        next: usize,
        bci: u32,
        sp: &crate::ir::SafepointSnapshot,
        keep: usize,
        pushed: Option<NodeId>,
    ) -> Option<FrameState> {
        let graph = self.graph;
        // A frame holding a lock at the call (interpreter round i1 wave 29,
        // lane L3; `i24-L6-...-keeps-its-old-splice`, "optimizing frames
        // holding a lock"): admitted under the switches the OSR door's
        // back-edge poll exits use (`back_edge_mode_exit_state`), because the
        // exit is the same kind of point taken by the same planless transfer
        // (`deopt_resume::transfer_osr_guard_exit_into_live_frame`, which asks
        // `osr_exit::poll_exit_point_resumable` again): each hold stays with the
        // live frame, an elided level is re-taken. An invoke takes and releases
        // no lock, so the invoke's own snapshot names the locks the successor
        // holds. A method-entry compile's site (wave 38) takes the lock rules
        // of its own back-edge exits (`IR_ENTRY_EXITS_*`): the stash sinks
        // rebuild the frame, leaving each hold the compiled code took held and
        // re-taking an elided level.
        let holds_a_lock = !sp.monitors.is_empty();
        let lock_admitted = match self.poll_mode_exits {
            IrPollModeExits::MethodEntry => IR_ENTRY_EXITS_HOLDING_A_LOCK_ENABLED,
            IrPollModeExits::OsrDoor => crate::osr_exit::OSR_POLL_EXITS_HOLDING_A_LOCK_ENABLED,
            IrPollModeExits::MethodEntryNamedOnly | IrPollModeExits::Off => false,
        };
        if holds_a_lock && !lock_admitted {
            return None;
        }
        let elided_admitted = match self.poll_mode_exits {
            IrPollModeExits::MethodEntry => IR_ENTRY_EXITS_WITH_AN_ELIDED_LOCK_ENABLED,
            IrPollModeExits::OsrDoor => crate::osr_exit::OSR_POLL_EXITS_WITH_AN_ELIDED_LOCK_ENABLED,
            IrPollModeExits::MethodEntryNamedOnly | IrPollModeExits::Off => false,
        };
        let dom = &self.schedule.dom;
        let block_of = |v: NodeId| -> Option<usize> {
            self.schedule
                .node_to_block
                .get(v as usize)
                .copied()
                .filter(|&b| b != usize::MAX)
        };
        let call_block = block_of(anchor)?;
        if !dom.is_reachable(call_block) {
            return None;
        }
        let emitted = |v: NodeId| self.defined_nodes.get(v as usize).copied().unwrap_or(false);
        // Item 3 of the i24-L6 page (2026-10-07): an object escape analysis
        // scalar-replaced is no machine word, but the frame can name it as a
        // `VirtualObject` the transfer re-allocates, as a guard's frame does
        // (`Self::resolve_frame_values`, `frame_value_for_object`): javac's
        // for-each iterators live across the loop body's calls.
        let virtual_object = |v: NodeId| {
            v != NO_NODE
                && crate::deopt::osr_guard_exit_virtuals_enabled()
                && self.sr_map.is_some_and(|sr| sr.objects.contains_key(&v))
        };
        let current = |v: NodeId| -> bool {
            if v == NO_NODE || virtual_object(v) {
                return true;
            }
            let Some(n) = graph.nodes.get(v as usize) else {
                return false;
            };
            if matches!(n.op, Op::Const(_) | Op::ConstF(_) | Op::Param(_)) {
                return true;
            }
            let Some(b) = block_of(v) else {
                return false;
            };
            (n.op == Op::Phi || emitted(v)) && dom.dominates(b, call_block)
        };
        // An invoke reads no local, so a local dead at the successor was dead
        // at the invoke too: `Undefined`, whatever node names it.
        let dead = |slot: usize| self.poll_exit_local_dead(next, slot);
        let mut local_nodes: Vec<NodeId> = Vec::with_capacity(sp.locals.len());
        for (slot, &v) in sp.locals.iter().enumerate() {
            if current(v) {
                local_nodes.push(v);
            } else if dead(slot) {
                local_nodes.push(NO_NODE);
            } else {
                return self
                    .post_call_exit_refused(pc, &format!("local {slot} not current (n{v})"));
            }
        }
        let mut stack_nodes: Vec<NodeId> = Vec::with_capacity(keep + 1);
        for (i, &v) in sp.stack.get(..keep)?.iter().enumerate() {
            if !current(v) {
                return self.post_call_exit_refused(pc, &format!("stack {i} not current (n{v})"));
            }
            stack_nodes.push(v);
        }
        if let Some(result) = pushed {
            if !emitted(result) {
                return self.post_call_exit_refused(pc, "the call's result not emitted");
            }
            stack_nodes.push(result);
        }
        // Each locked object must be current right after the call, as a local
        // is.
        if sp.monitors.iter().any(|&obj| !current(obj)) {
            return self.post_call_exit_refused(pc, "a locked object not current");
        }
        let was_quiet = self.quiet_dropped_home.replace(true);
        let kept_result_mask = pushed
            .and_then(|res| self.lazy_resident(res))
            .map_or(0u16, |r| 1u16 << r);
        let prev_collect_mask = self.post_collect_ref_mask.replace(Some(kept_result_mask));
        // A scalar-replaced object is described at the call, its site: what
        // ran before it in its block ran, what follows did not
        // (`frame_value_for_object`'s rules for a guard's site).
        let mut virtual_defined: std::collections::HashSet<NodeId> =
            std::collections::HashSet::new();
        let mut describe = |n: NodeId| -> FrameValue {
            match self.sr_map {
                Some(sr) if virtual_object(n) => self.frame_value_for_object(
                    n,
                    Some(call_block),
                    Some((anchor, anchor)),
                    sr,
                    &mut virtual_defined,
                ),
                _ => match self.frame_value_for(n) {
                    // Item 3 (2026-10-07): a primitive with no home word
                    // (a dropped one) is described by the register it is
                    // RESIDENT in right here, exclusive owner or not -- the
                    // back-edge exits' rule (`back_edge_mode_exit_state`'s
                    // `site_value`): the point is read at this one native
                    // site, and the stub preserves every register into the
                    // deopt image but `DEOPT_ARG0`, which it loads with the
                    // point (`emit_post_call_exit_stubs`).
                    FrameValue::Unsupported => graph
                        .nodes
                        .get(n as usize)
                        .filter(|node| matches!(node.ty, IrType::Int | IrType::Long))
                        .filter(|node| !matches!(node.op, Op::Const(_)))
                        .and_then(|node| {
                            self.resident_gpr(n)
                                .filter(|&r| r != DEOPT_ARG0 && self.deopt_image_holds_gpr(r))
                                .map(|r| {
                                    if node.ty == IrType::Long {
                                        FrameValue::RegisterLong(r)
                                    } else {
                                        FrameValue::Register(r)
                                    }
                                })
                        })
                        .unwrap_or(FrameValue::Unsupported),
                    other => other,
                },
            }
        };
        let mut locals: Vec<FrameValue> = local_nodes.iter().map(|&n| describe(n)).collect();
        let mut stack: Vec<FrameValue> = stack_nodes.iter().map(|&n| describe(n)).collect();
        // Item 3 (2026-10-07): the call's own primitive result, still CARRIED
        // in the register it returned in for its one consumer (no home word
        // yet), is that register: the site's stub preserves every register up
        // to the deopt stub's image, except `DEOPT_ARG0`, which it loads with
        // the point (`emit_post_call_exit_stubs`).
        if let (Some(result), Some(last)) = (pushed, stack.last_mut()) {
            if matches!(last, FrameValue::Unsupported) {
                // JIT round 25 item 1a: or a TAIL carry, whose home store was
                // never emitted and which is in RAX right here (the site is
                // emitted at the elided store's position; see
                // `lower_data_node_tracked`, which moves the carry past it).
                let carried = match (self.live_carry, self.tail_carry) {
                    (Some((producer, _, reg, _)), _) if producer == result => Some(reg),
                    (_, Some((producer, _, pos)))
                        if producer == result && pos == self.buf.pos() =>
                    {
                        Some(RAX)
                    }
                    _ => None,
                };
                if let Some(reg) = carried {
                    let ty = graph.nodes.get(result as usize).map(|n| n.ty);
                    if reg != DEOPT_ARG0 && self.deopt_image_holds_gpr(reg) {
                        match ty {
                            Some(IrType::Int) => *last = FrameValue::Register(reg),
                            Some(IrType::Long) => *last = FrameValue::RegisterLong(reg),
                            _ => {}
                        }
                    }
                }
            }
        }
        // The transfer re-allocates scalar-replaced objects in a monitor-free
        // frame only (`deopt::osr_in_place_virtuals_refusal`).
        let names_a_virtual = locals.iter().chain(stack.iter()).any(|v| {
            matches!(
                v,
                FrameValue::VirtualObject(_)
                    | FrameValue::VirtualObjectRef(_)
                    | FrameValue::ConditionalVirtualObject(_)
            )
        });
        if names_a_virtual && holds_a_lock {
            self.post_collect_ref_mask.set(prev_collect_mask);
            self.quiet_dropped_home.set(was_quiet);
            return self
                .post_call_exit_refused(pc, "a scalar-replaced object in a frame holding a lock");
        }
        let monitors = if holds_a_lock {
            self.resolve_monitors(sp, |obj| self.monitor_object_value(obj))
        } else {
            Vec::new()
        };
        self.post_collect_ref_mask.set(prev_collect_mask);
        self.quiet_dropped_home.set(was_quiet);
        // Every lock's object in a reference frame word, and an elided level
        // only where the transfer re-takes one: what `poll_exit_point_resumable`
        // accepts (fail closed otherwise).
        if monitors.iter().any(|m| {
            (m.relock && !elided_admitted)
                || m.lock_depth == 0
                || !matches!(m.object, FrameValue::StackSlotRef(_))
        }) {
            return self.post_call_exit_refused(pc, "a monitor the transfer cannot take");
        }
        for (slot, value) in locals.iter_mut().enumerate() {
            if matches!(value, FrameValue::Unsupported) && dead(slot) {
                *value = FrameValue::Undefined;
            }
        }
        let names_a_register = locals.iter().chain(stack.iter()).any(|v| {
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
            return self
                .post_call_exit_refused(pc, "a register home without the deopt register region");
        }
        if let Some((i, (v, n))) = locals
            .iter()
            .zip(local_nodes.iter())
            .chain(stack.iter().zip(stack_nodes.iter()))
            .enumerate()
            .find(|(_, (v, _))| matches!(v, FrameValue::Unsupported))
        {
            let op = self
                .graph
                .nodes
                .get(*n as usize)
                .map(|node| format!("{:?}", node.op));
            return self.post_call_exit_refused(
                pc,
                &format!(
                    "value {i} has no location ({v:?}, n{n} {}; carry {:?}, register {:?}, nameable {:?})",
                    op.unwrap_or_default(),
                    self.live_carry,
                    self.gp_reg_of.get(*n as usize).copied().flatten(),
                    self.deopt_nameable.get(*n as usize).copied(),
                ),
            );
        }
        let reason = DeoptReason::OsrExit;
        let probe = DeoptimizationPoint {
            native_offset: 0,
            bci,
            reason,
            action: DeoptAction::Reinterpret,
            speculation_id: crate::deopt::speculation_id(bci, reason),
            frame_state: FrameState {
                class_id: 0,
                method_key: String::new(),
                bci,
                locals,
                stack,
                monitors: Vec::new(),
                caller: None,
            },
            semantics: ResumeSemantics::for_reason(reason),
        };
        // Asked without the monitors, which that predicate refuses for the
        // guard exits' sake; they were checked above and join the frame after,
        // as `back_edge_mode_exit_state` does.
        if !crate::osr_exit::guard_exit_point_resumable(&probe) {
            return self.post_call_exit_refused(pc, "a frame the transfer does not resume");
        }
        let mut frame_state = probe.frame_state;
        frame_state.monitors = monitors;
        Some(frame_state)
    }

    /// The census line of item 3 of
    /// `docs/feature-designs/deopt-osr.md`
    /// ("an optimizing call whose snapshot names a value with no location"):
    /// under `CRATONVM_DBG_JITC`, one `ir-post-call-exit REFUSED <why>` line
    /// per call [`Self::post_call_exit_state`] gives no site once its shape
    /// passed, naming what it could not describe. Rank with
    /// `grep 'ir-post-call-exit REFUSED' | sed 's/ at pc=.*//' | sort | uniq -c`.
    /// Always `None`.
    pub(in crate::ir_lower) fn post_call_exit_refused(
        &self,
        pc: usize,
        why: &str,
    ) -> Option<FrameState> {
        if crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] ir-post-call-exit REFUSED {why} at pc={pc} of {}",
                self.graph.scope_method_key
            );
        }
        None
    }

    /// Interpreter round i1 wave 47, lane L3 -- the first stage of
    /// `docs/feature-designs/deopt-osr.md`:
    /// the frames the spliced call node `call` (at combined pc `pc`) returns
    /// into, as an inlined caller CHAIN -- the spliced callee at the call's
    /// successor with its result pushed (`REEXECUTE` there), every caller
    /// parked in its `invoke` -- or `None` to emit no site, as before wave 47.
    ///
    /// The chain is the post-call snapshot the builder recorded at the
    /// successor ([`ir::SafepointSnapshot::post_call_of`], under
    /// `CRATONVM_JIT_IR_POST_CALL_SPLICE_EXITS`), published by
    /// [`Self::splice_chain_frame_state`] exactly as a guard's chain is. Fail
    /// closed, as [`Self::post_call_exit_state`] is:
    ///
    /// * an OSR-door compile: the chain leaves through the VM's planless
    ///   chain transfer (`deopt_resume::transfer_osr_guard_exit_chain_into_live_frame`,
    ///   round 13 wave 8); or, since interpreter round i2 wave 54 (lane L2,
    ///   under [`crate::ir::ir_entry_post_call_splice_exits_enabled`]), a
    ///   METHOD-ENTRY compile whose artifact keeps the bytecode of every
    ///   spliced scope of the chain ([`chain_scopes_have_kept_sources`],
    ///   `ir::Graph::entry_chain_source_keys`): its stash sinks
    ///   (`deopt_resume::real_frame_deopt_resume_or_throw_and_despeculate`,
    ///   `build_stale_chain_from_own_sources`) push the chain and rebuild a
    ///   redefined spliced scope from that bytecode, and the VM's post-call
    ///   verdict refuses the exit while one would not translate
    ///   (`vm::jit::helpers::post_call_chain_exits_resumable`), since a sink
    ///   that refused after the call committed would re-run the method;
    /// * not a caller-held synchronized direct call (a lock no frame names);
    /// * the snapshot's innermost stack ends with the call's result, when it
    ///   returns one;
    /// * every value the chain names is current right after the call (a
    ///   constant, a parameter, or emitted in -- or a φ of -- a block that
    ///   dominates the call's): a value defined after the call, or a φ of a
    ///   join at the successor, is not, and refuses;
    /// * no monitor in any scope, no register home without the deopt stub's
    ///   register image, and a frame `osr_exit::guard_exit_point_resumable`
    ///   admits (for a chain: `chain_resumable_in_place`, which the transfer's
    ///   provenance check `guard_exit_resume_bci` asks again at run time).
    pub(in crate::ir_lower) fn post_call_chain_exit_state(
        &self,
        call: NodeId,
        info_ptr: usize,
        pc: usize,
    ) -> Option<FrameState> {
        // Interpreter round i2 wave 54, lane L2 (stage 2 of the proposal): a
        // METHOD-ENTRY compile too, under its own switch, when the artifact
        // keeps the bytecode of every spliced scope the chain names (checked
        // below, once the chain is built).
        let entry = match self.poll_mode_exits {
            IrPollModeExits::OsrDoor => false,
            IrPollModeExits::MethodEntry => true,
            IrPollModeExits::MethodEntryNamedOnly | IrPollModeExits::Off => return None,
        };
        let si = *self.post_call_splice_state_by_call.get(&call)?;
        if entry
            && (self.graph.entry_chain_source_keys.is_empty()
                || !crate::ir::ir_entry_post_call_splice_exits_enabled())
        {
            return None;
        }
        if self
            .direct_calls
            .contains_key(&crate::compile_request::sync_direct_row_key(pc))
        {
            return None;
        }
        let graph = self.graph;
        let sp = graph.safepoints.get(si)?;
        let chain = sp.splice_scope.as_deref()?;
        if !sp.monitors.is_empty() {
            return None;
        }
        // SAFETY: `info_ptr` names a live `JitInvokeInfo` owned by this
        // compile's `ir_call_infos`, exactly as the `Op::Call` arm reads it.
        let info = unsafe { &*(info_ptr as *const JitInvokeInfo) };
        let innermost_stack = usize::try_from(chain.scopes.first()?.stack_len).ok()?;
        if info.return_type != b'V' {
            // Scope 0's operand stack is the first `stack_len` entries.
            let top = innermost_stack.checked_sub(1)?;
            if sp.stack.get(top).copied() != Some(call) {
                return None;
            }
        }
        let dom = &self.schedule.dom;
        let block_of = |v: NodeId| -> Option<usize> {
            self.schedule
                .node_to_block
                .get(v as usize)
                .copied()
                .filter(|&b| b != usize::MAX)
        };
        let call_block = block_of(call)?;
        if !dom.is_reachable(call_block) {
            return None;
        }
        let emitted = |v: NodeId| self.defined_nodes.get(v as usize).copied().unwrap_or(false);
        let current = |v: NodeId| -> bool {
            if v == NO_NODE {
                return true;
            }
            let Some(n) = graph.nodes.get(v as usize) else {
                return false;
            };
            if matches!(n.op, Op::Const(_) | Op::ConstF(_) | Op::Param(_)) {
                return true;
            }
            let Some(b) = block_of(v) else {
                return false;
            };
            (n.op == Op::Phi || emitted(v)) && dom.dominates(b, call_block)
        };
        if !sp.locals.iter().chain(sp.stack.iter()).all(|&v| current(v)) {
            return None;
        }
        let kept_result_mask = (info.return_type != b'V')
            .then(|| self.lazy_resident(call))
            .flatten()
            .map_or(0u16, |r| 1u16 << r);
        let prev_collect_mask = self.post_collect_ref_mask.replace(Some(kept_result_mask));
        let fs = self.splice_chain_frame_state(si);
        self.post_collect_ref_mask.set(prev_collect_mask);
        let fs = fs.ok()?;
        if entry && !chain_scopes_have_kept_sources(&fs, &self.graph.entry_chain_source_keys) {
            return None;
        }
        let mut names_a_register = false;
        let mut scope = Some(&fs);
        while let Some(f) = scope {
            names_a_register |= f.locals.iter().chain(f.stack.iter()).any(|v| {
                matches!(
                    v,
                    FrameValue::Register(_)
                        | FrameValue::RegisterLong(_)
                        | FrameValue::RegisterRef(_)
                        | FrameValue::XmmFloat(_)
                        | FrameValue::XmmDouble(_)
                )
            });
            scope = f.caller.as_deref();
        }
        if names_a_register && self.deopt_regs_base <= 0 {
            return None;
        }
        let reason = DeoptReason::OsrExit;
        let probe = DeoptimizationPoint {
            native_offset: 0,
            bci: fs.bci,
            reason,
            action: DeoptAction::Reinterpret,
            speculation_id: crate::deopt::speculation_id(fs.bci, reason),
            frame_state: fs,
            semantics: ResumeSemantics::for_reason(reason),
        };
        if !crate::osr_exit::guard_exit_point_resumable(&probe) {
            return None;
        }
        Some(probe.frame_state)
    }

    /// Lay down the stubs of every post-call exit site the walk emitted, and
    /// record `(site, stub)` for `CompiledMethod::post_call_exit_sites`. After
    /// the poll exits' pads (so after every OSR fact was read: these exits,
    /// like those, resume at a state the graph itself reaches) and before
    /// `emit_deopt_stub`, which takes the `deopt_stub_patches` rows these add.
    /// Nothing when no site was emitted.
    ///
    /// Per site:
    ///
    /// ```text
    /// stub:  CALL tail                ; E8 rel32 -- the verdict, in the flags
    ///        JZ   <site + 5>          ; stay: back past the NOP
    ///        MOV  DEOPT_ARG0, <point> ; leave: the successor's OsrExit point
    ///        JMP  <shared deopt stub>
    /// ```
    ///
    /// and once per compile the tail ([`Self::emit_post_call_exit_verdict_tail`]).
    pub(in crate::ir_lower) fn emit_post_call_exit_stubs(&mut self) {
        let pending = std::mem::take(&mut self.post_call_exits_pending);
        if pending.is_empty() {
            return;
        }
        let Some(bit) = self.poll_mode_exits.verdict_bit() else {
            return;
        };
        let tail = self.buf.pos();
        self.emit_post_call_exit_verdict_tail(bit);
        for (site, point_ptr) in pending {
            let stub = self.buf.pos();
            self.buf.emit_byte(0xE8); // CALL rel32 -> the tail
            let call_patch = self.buf.pos();
            self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
            // Cast: x86-64 rel32 displacements between offsets of one buffer.
            let rel = tail as i32 - (call_patch as i32 + 4);
            if !Self::patch_or_bail(&mut self.buf, call_patch, rel) {
                return;
            }
            self.buf.emit(&[0x0F, 0x84]); // JZ rel32 -> back past the NOP
            let back = self.buf.pos();
            self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
            let resume = site + crate::ENTRY_PATCH_PAD.len();
            // Cast: as above.
            let rel = resume as i32 - (back as i32 + 4);
            if !Self::patch_or_bail(&mut self.buf, back, rel) {
                return;
            }
            // JIT round 25 item 1a: the one branch allowed to reach a tail
            // carry's read (`verify_tail_carry_points`).
            self.post_call_exit_backs.push(back);
            self.emit_mov_reg_imm64(DEOPT_ARG0, point_ptr);
            self.buf.emit_byte(0xE9); // JMP rel32 -> the shared deopt stub
            let jmp_patch = self.buf.pos();
            self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
            self.deopt_stub_patches.push(jmp_patch);
            if let (Ok(site), Ok(stub)) = (u32::try_from(site), u32::try_from(stub)) {
                self.post_call_exit_sites.push((site, stub));
            }
        }
    }

    /// The shared tail every post-call exit stub `CALL`s: ask the safepoint
    /// slow path for this body's verdict WITHOUT parking
    /// (`crate::POST_CALL_EXIT_VERDICT_ONLY`), with every GPR and XMM register
    /// preserved, and return with the flags of `TEST AL, bit` (`bit`: the one
    /// this compile's polls test, `IrPollModeExits::verdict_bit`). The same
    /// code as the single-pass tier's (`x64/op_invoke.rs::
    /// emit_post_call_exit_verdict_tail`), and the same argument for needing no
    /// spill and no oop map: the helper does not park in this mode.
    ///
    /// ```text
    /// tail:  PUSH RAX RCX RDX RBX RSI RDI R8 R9 R10 R11
    ///        MOV  RBX, RSP ; AND RSP, -16 ; SUB RSP, 0x120
    ///        MOVDQU [RSP+0x20+16n], XMMn     (n = 0..15)
    ///        MOV  ARG0, <safepoint flag> ; MOV ARG1, <id> | VERDICT_ONLY
    ///        MOV  RAX, <safepoint slow path> ; CALL RAX
    ///        MOVDQU XMMn, [RSP+0x20+16n] ; MOV RSP, RBX
    ///        TEST AL, bit
    ///        POP  R11 .. RAX ; RET            ; POP and RET keep the flags
    /// ```
    pub(in crate::ir_lower) fn emit_post_call_exit_verdict_tail(&mut self, bit: u8) {
        // PUSH RAX, RCX, RDX, RBX, RSI, RDI, then R8-R11.
        self.buf.emit(&[0x50, 0x51, 0x52, 0x53, 0x56, 0x57]);
        self.buf
            .emit(&[0x41, 0x50, 0x41, 0x51, 0x41, 0x52, 0x41, 0x53]);
        self.buf.emit(&[0x48, 0x89, 0xE3]); // MOV RBX, RSP
        self.buf.emit(&[0x48, 0x83, 0xE4, 0xF0]); // AND RSP, -16
                                                  // SUB RSP, 0x120: the Win64 home area (harmless on SysV) and sixteen
                                                  // 16-byte XMM slots; a multiple of 16, so the CALL stays aligned.
        self.buf.emit(&[0x48, 0x81, 0xEC]);
        self.buf.emit(&(0x20i32 + 16 * 16).to_le_bytes());
        for xmm in 0u8..16 {
            self.emit_post_call_exit_xmm(0x7F, xmm); // MOVDQU [RSP+..], XMMn
        }
        // Cast: the flag byte's address and the helper's, baked as immediates.
        self.emit_mov_reg_imm64(CALL_ARG_REGS[0], self.safepoint_flag_addr as u64);
        // Cast: the verdict-only bit is bit 62, a non-negative `i64`.
        let body = u64::from(self.compile_id.id()) | crate::POST_CALL_EXIT_VERDICT_ONLY as u64;
        self.emit_mov_reg_imm64(CALL_ARG_REGS[1], body);
        self.emit_mov_reg_imm64(RAX, self.safepoint_slow_path as u64);
        self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
        for xmm in 0u8..16 {
            self.emit_post_call_exit_xmm(0x6F, xmm); // MOVDQU XMMn, [RSP+..]
        }
        self.buf.emit(&[0x48, 0x89, 0xDC]); // MOV RSP, RBX
        self.buf.emit(&[0xF6, 0xC0, bit]); // TEST AL, imm8
                                           // POP R11-R8, then RDI, RSI, RBX, RDX, RCX, RAX.
        self.buf
            .emit(&[0x41, 0x5B, 0x41, 0x5A, 0x41, 0x59, 0x41, 0x58]);
        self.buf.emit(&[0x5F, 0x5E, 0x5B, 0x5A, 0x59, 0x58]);
        self.buf.emit_byte(0xC3); // RET
    }

    /// `MOVDQU` between XMM register `xmm` and its save slot
    /// `[RSP + 0x20 + 16 * xmm]` of the verdict tail: `opcode` `0x7F` stores,
    /// `0x6F` loads (`F3 [REX.R] 0F op ModRM(mod=10, reg, rm=SIB) 24 disp32`).
    pub(in crate::ir_lower) fn emit_post_call_exit_xmm(&mut self, opcode: u8, xmm: u8) {
        self.buf.emit_byte(0xF3);
        if xmm >= 8 {
            self.buf.emit_byte(0x44); // REX.R
        }
        self.buf
            .emit(&[0x0F, opcode, 0x84 | ((xmm & 7) << 3), 0x24]);
        let disp: i32 = 0x20 + 16 * i32::from(xmm);
        self.buf.emit(&disp.to_le_bytes());
    }

    /// Publish at the current spill watermark when this path has a map slot.
    pub(in crate::ir_lower) fn emit_safepoint_map_if_enabled(
        &mut self,
    ) -> Option<SafepointMapToken> {
        if self.sp_id_slot_off <= 0 || !self.reloc_emit_on {
            return None;
        }
        self.emit_safepoint_map(self.spill_high_water)
    }

    /// `CRATONVM_JIT_IR_GC_POINT_MAPS=0` -- restore the two IR-backend
    /// GC-capable sites that recorded no safepoint at all: the cooperative
    /// safepoint POLL and `Op::New`.
    ///
    /// Default ON, and one switch rather than two because it is one statement:
    /// every point in an IR body that can reach a collection has to record an
    /// id and a map, or the collector reads the sp-id slot and finds either the
    /// prologue sentinel (no map -> the cycle refuses) or the id of an EARLIER
    /// safepoint (a map for a different program point -> a silent wrong
    /// answer). The other six GC-capable ops have always paid it.
    ///
    /// With it off, `frame_coverage_reason::NO_MAP_FOR_STORED_ID` climbs again
    /// and `xt_cov` refusals rise -- the census the fix is measured on.
    pub(in crate::ir_lower) fn ir_gc_point_maps_enabled() -> bool {
        use std::sync::OnceLock;
        static ON: OnceLock<bool> = OnceLock::new();
        *ON.get_or_init(|| {
            !matches!(
                Err::<String, std::env::VarError>(std::env::VarError::NotPresent).as_deref(),
                Ok("0") | Ok("false") | Ok("FALSE")
            )
        })
    }

    /// MOV [RBP - offset], reg  (REX.W [+ REX.R for an extended reg]).
    /// Prefers the disp8 ModRM form when `-offset` fits in a signed byte.
    pub(in crate::ir_lower) fn store_abi_reg(&mut self, reg: u8, offset: i32) {
        let start = self.buf.pos();
        self.store_abi_reg_raw(reg, offset);
        if self.home_reload_forward_on {
            self.rax_home_mirror = Some((reg, offset, start, self.buf.pos()));
        }
    }

    /// [`Self::store_abi_reg`]'s encoding alone.
    pub(in crate::ir_lower) fn store_abi_reg_raw(&mut self, reg: u8, offset: i32) {
        let neg = -offset;
        let mut prefix = 0x48u8; // REX.W
        if reg >= 8 {
            prefix |= 0x04; // REX.R
        }
        self.buf.emit_byte(prefix);
        self.buf.emit_byte(0x89);
        if (i8::MIN as i32..=i8::MAX as i32).contains(&neg) {
            self.buf.emit_byte(0x45 | ((reg & 7) << 3));
            self.buf.emit_byte(neg as u8);
        } else {
            self.buf.emit_byte(0x85 | ((reg & 7) << 3));
            self.buf.emit(&neg.to_le_bytes());
        }
    }

    /// Emit the ModRM byte and displacement of a `[RBP - offset]` operand whose
    /// ModRM `reg` field is `reg`, choosing the **smallest legal form**.
    ///
    /// RBP has no `mod=00` encoding — that bit pattern is RIP-relative — so the
    /// two forms are `mod=01` + disp8 and `mod=10` + disp32, and a zero
    /// displacement still needs an explicit disp8 of `0`. Same rule
    /// `x64::disp` states for the single-pass backend, applied here because
    /// this is the backend that touches the frame on *every* value read.
    ///
    /// One helper rather than the rule repeated per emitter: the per-emitter
    /// shape is exactly how `store_abi_reg` came to pick the short form while
    /// `load_reg_from_frame` directly below it kept emitting disp32.
    pub(in crate::ir_lower) fn emit_rbp_modrm_disp(&mut self, reg: u8, offset: i32) {
        let neg = -offset;
        if (i32::from(i8::MIN)..=i32::from(i8::MAX)).contains(&neg) {
            // mod=01, r/m=RBP(101), disp8
            self.buf.emit_byte(0x45 | ((reg & 7) << 3));
            // Cast: guarded by the range check above.
            self.buf.emit_byte(neg as u8);
        } else {
            // mod=10, r/m=RBP(101), disp32
            self.buf.emit_byte(0x85 | ((reg & 7) << 3));
            self.buf.emit(&neg.to_le_bytes());
        }
    }

    /// gch h2/fj (`gcd-d1a-proposal-context-slot-loads-cannot-read-an-unreserved-slot`,
    /// item 1, IR half): the ONE load of the saved VM-context pointer,
    /// `MOV reg, [RBP - context_slot_off]`, byte for byte the load every site
    /// emitted itself before. A frame that reserved no slot reports offset 0,
    /// and `[rbp - 0]` is the saved caller RBP, which a helper would then take
    /// for a `SharedVm` (gcd d1/mon: the crash lands far from the cause). Such
    /// a load now refuses the compile with a named bail instead
    /// (`scan_frame_needs` missed an op); the bytes emitted after the latch are
    /// never published.
    pub(in crate::ir_lower) fn load_context(&mut self, reg: u8) {
        if self.context_slot_off == 0 {
            self.latch_bailout(Bailout::new(BailoutReason::Internal(
                "ir_lower: vm-context load from an unreserved slot (scan_frame_needs)",
            )));
        }
        self.load_reg_from_frame(reg, self.context_slot_off);
    }

    /// MOV reg, [RBP - offset]  (REX.W [+ REX.R]; smallest displacement form).
    /// General form of `load_to_rax`/`load_to_rcx` for an arbitrary (possibly
    /// extended) destination.
    pub(in crate::ir_lower) fn load_reg_from_frame(&mut self, reg: u8, offset: i32) {
        let start = self.buf.pos();
        let mut prefix = 0x48u8;
        if reg >= 8 {
            prefix |= 0x04;
        }
        self.buf.emit_byte(prefix);
        self.buf.emit_byte(0x8B);
        self.emit_rbp_modrm_disp(reg, offset);
        self.note_home_reload(offset, start, reg);
    }

    /// LEA reg, [RBP - offset]  (REX.W [+ REX.R]; smallest displacement form).
    /// Used to compute the `args_ptr` the dispatch helper reads the marshalled
    /// Java args from.
    pub(in crate::ir_lower) fn lea_reg_from_frame(&mut self, reg: u8, offset: i32) {
        let mut prefix = 0x48u8;
        if reg >= 8 {
            prefix |= 0x04;
        }
        self.buf.emit_byte(prefix);
        self.buf.emit_byte(0x8D);
        self.emit_rbp_modrm_disp(reg, offset);
    }

    /// `MOV qword [rbp - off], 0` (mod=10 disp32, /0).
    /// `CRATONVM_JIT_ZERO_SPID` — default ON. Off restores the pre-fix
    /// behaviour (the safepoint-id slot reads uninitialised stack until the
    /// first safepoint stores an id), so the repair can be A/B'd on one binary.
    pub(in crate::ir_lower) fn zero_sp_id_slot_enabled() -> bool {
        // Shared with the single-pass backend, which establishes its own
        // sentinel in `x64::frames::emit_prologue` under the same key.
        crate::sp_id_slot_init_enabled()
    }

    pub(in crate::ir_lower) fn emit_zero_frame_slot(&mut self, off: i32) {
        // Round 16 wave 6 (lane codegen, CG16-10): inside the prologue's
        // zeroing stretch of a frame `emit_zero_frame_sse` already cleared,
        // every word this would zero is still zero -- nothing between the
        // whole-frame clear and here writes a local past the parameters, a
        // reserved bookkeeping word, the sp-id word, a phi home or a
        // reference colour -- so the store would write zero over zero.
        // Round 16 wave 7 (lane frames): unless the clear LEFT THIS WORD OUT
        // (`fresh_clear_skip`); then the store is the word's only zero.
        if self.frame_prezeroed && self.prologue_zero_stretch && !self.fresh_clear_skipped(off) {
            return;
        }
        // Round 16 wave 4 (lane codegen, CG16-8): in the method prologue, a
        // run of zero stores is `XOR EAX, EAX` once and then `MOV [rbp - off],
        // RAX` (4 or 7 bytes) per word instead of `MOV qword [rbp - off], 0`
        // (11 bytes). RAX is free there -- no incoming argument arrives in it
        // on either ABI, and nothing the prologue emits reads it before
        // writing it -- and it is known to be zero only right after the
        // previous store of the run (`zero_rax_end`): any byte emitted in
        // between, a label bound in between included, starts a new run.
        if self.prologue_zero_rax {
            if self.zero_rax_end != Some(self.buf.pos()) {
                self.buf.emit(&[0x31, 0xC0]); // XOR EAX, EAX
            }
            let mut bytes = FrameAccess::new();
            enc_frame_store(RAX, off, &mut bytes);
            self.buf.emit(bytes.as_slice());
            self.zero_rax_end = Some(self.buf.pos());
            return;
        }
        self.buf.emit(&[0x48, 0xC7, 0x85]);
        self.buf.emit(&(-off).to_le_bytes());
        self.buf.emit(&0i32.to_le_bytes());
    }

    /// Zero every word of the frame just carved, `[rsp, rbp)`, walking DOWN
    /// from `rbp` (gen r5w4/jit8; emitted by [`Self::emit_prologue`] only under
    /// default-on `CRATONVM_JIT_IR_ZERO_FRESH_FRAMES`).
    ///
    /// # Why
    ///
    /// A compiled frame is carved out of stack the previous frame at the same
    /// depth used, and the collector's band scan reads every word of it that
    /// no liveness claim excludes: `Prim` colours, spill words below the
    /// watermark whose definitions this activation has not run, the staging
    /// region, the bookkeeping tail. Until this activation writes such a word
    /// it holds the previous frame's value -- after a program dropped a large
    /// structure, often that structure's address, left by the callee that
    /// built it at exactly this depth (`GenR5W3OsrHolderProbe`'s
    /// `warm-build-warm-check`: `check`'s fresh frame sits where `build`'s did;
    /// `GenR4W4NativeStringOomProbe`: the `println` chain sits where the OSR'd
    /// `main` frames did). The dead-colour clears handle words this activation
    /// wrote; this handles the ones it never wrote. It is item 1 of
    /// `docs/feature-designs/jit-ir-optimizations.md`.
    ///
    /// # Soundness
    ///
    /// Emitted right after `sub rsp, frame_size`, before the callee-saved
    /// saves, the parameter stores and every other prologue write: at that
    /// point no word of `[rsp, rbp)` holds anything this activation, its
    /// caller or the JVM can read (the caller's state is at and above `rbp`,
    /// stack arguments included), so zero is as good as any value -- the
    /// other prologue zeroings already rely on that. Scratch: R10 and R11
    /// only, which are caller-saved on both ABIs and carry no argument at
    /// entry (the IC sequence that calls through R11 and indexes through R10
    /// is the CALLER's), and the flags.
    ///
    /// Downward and contiguous, so on a target that grows its stack through a
    /// guard page each new page is touched in order. `frame_size` is a positive
    /// multiple of 8 (the caller checks), so the loop stops exactly at `rsp`.
    /// Round 16 wave 6 (lane codegen, CG16-10;
    /// `CRATONVM_JIT_IR_ZERO_FRESH_FRAMES_SSE`): [`Self::emit_zero_whole_frame`]'s
    /// effect -- every word of `[rbp - frame_size, rbp)` zero, nothing else
    /// written but XMM5, R10, R11 and the flags -- with 16-byte stores.
    ///
    /// `PXOR XMM5, XMM5`, then, from the top of the frame DOWN:
    ///
    /// * up to [`ZERO_FRAME_SSE_STRAIGHT_BYTES`]: one `MOVDQU [rbp - 16k], XMM5`
    ///   per 16 bytes, straight-line, no branch;
    /// * above it, a loop of four `MOVDQU` per 64 bytes (`R10` walks down from
    ///   `RBP`; the four stores go high to low), then the remainder below it
    ///   straight-line;
    /// * an odd 8-byte word at the bottom is a `MOVQ [rbp - frame_size], XMM5`.
    ///
    /// Every store lies inside the frame, and the stores run in descending
    /// address order, as the 8-byte loop's did: a stack that grows through a
    /// guard page meets each new page from above. XMM5 is volatile on both
    /// ABIs and no argument arrives in it here (`ENTRY_ABI_REGS` are
    /// general-purpose); the callee-saved XMM save area is written after this,
    /// from XMM6 up. The 8-byte loop retires about `frame_size / 2`
    /// instructions; this, about `frame_size / 16` stores.
    pub(in crate::ir_lower) fn emit_zero_frame_sse(buf: &mut ExecutableBuffer, frame_size: i32) {
        if frame_size <= 0 || frame_size % 8 != 0 {
            return;
        }
        buf.emit(&[0x66, 0x0F, 0xEF, 0xED]); // PXOR XMM5, XMM5

        // `MOVDQU [rbp + disp], XMM5` (wide) / `MOVQ [rbp + disp], XMM5`.
        let store_rbp = |buf: &mut ExecutableBuffer, disp: i32, wide: bool| {
            if wide {
                buf.emit(&[0xF3, 0x0F, 0x7F]);
            } else {
                buf.emit(&[0x66, 0x0F, 0xD6]);
            }
            if let Ok(d8) = i8::try_from(disp) {
                buf.emit(&[0x6D]); // mod=01 reg=XMM5 rm=RBP
                buf.emit(&d8.to_le_bytes());
            } else {
                buf.emit(&[0xAD]); // mod=10 reg=XMM5 rm=RBP
                buf.emit(&disp.to_le_bytes());
            }
        };
        // 16-byte chunks below RBP, and how many of the top ones the loop
        // clears (a multiple of four).
        let chunks = frame_size / 16;
        let looped = if frame_size > ZERO_FRAME_SSE_STRAIGHT_BYTES {
            (chunks / 4) * 4
        } else {
            0
        };
        if looped > 0 {
            // R10 = RBP; R11 = RBP - 16 * looped; .loop: SUB R10, 64; four
            // MOVDQU high to low; CMP R10, R11; JA .loop.
            buf.emit(&[0x49, 0x89, 0xEA]); // MOV R10, RBP
            buf.emit(&[0x4C, 0x8D, 0x9D]); // LEA R11, [RBP + disp32]
            buf.emit(&(-16 * looped).to_le_bytes());
            let top = buf.pos();
            buf.emit(&[0x49, 0x83, 0xEA, 0x40]); // SUB R10, 64
            buf.emit(&[0xF3, 0x41, 0x0F, 0x7F, 0x6A, 0x30]); // MOVDQU [R10 + 48], XMM5
            buf.emit(&[0xF3, 0x41, 0x0F, 0x7F, 0x6A, 0x20]); // MOVDQU [R10 + 32], XMM5
            buf.emit(&[0xF3, 0x41, 0x0F, 0x7F, 0x6A, 0x10]); // MOVDQU [R10 + 16], XMM5
            buf.emit(&[0xF3, 0x41, 0x0F, 0x7F, 0x2A]); // MOVDQU [R10], XMM5
            buf.emit(&[0x4D, 0x39, 0xDA]); // CMP R10, R11

            // Cast: the loop body is 30 bytes, so its JA's rel8 is -32.
            let rel = top as i64 - (buf.pos() as i64 + 2);
            buf.emit(&[0x77, rel as i8 as u8]); // JA .loop
        }
        for k in (looped + 1)..=chunks {
            store_rbp(buf, -16 * k, true);
        }
        if frame_size % 16 == 8 {
            store_rbp(buf, -frame_size, false);
        }
    }

    /// Round 16 wave 7 (lane frames): the SSE-form fresh-frame clear with the
    /// lane's switches applied -- the words [`Self::fresh_frame_clear_skip`]
    /// names are left out, and under `CRATONVM_JIT_IR_ZERO_FRESH_FRAMES_AVX`
    /// the wide stores are 32 bytes. With neither, this IS
    /// [`Self::emit_zero_frame_sse`], byte for byte. Returns the skip mask
    /// (empty when nothing was left out). `entry_writes_params`: the entry
    /// this clear opens stores every incoming parameter word (the prologue;
    /// not an OSR entry stub).
    pub(in crate::ir_lower) fn emit_fresh_frame_clear_sse(
        &mut self,
        entry_writes_params: bool,
    ) -> Vec<bool> {
        let mut skip = self.fresh_frame_clear_skip(entry_writes_params);
        // JIT round 25 item 2, wave 3: the prologue (never an OSR entry, which
        // enters past the writes) leaves the lazy words out too.
        if entry_writes_params {
            merge_block_lazy_clear_skip(&mut skip, &self.block_lazy_clear, self.frame_size);
        }
        // JIT round 25 (item 3): what the claim took out of the prologue's
        // clear, for `CRATONVM_DBG_JITC` (printed, named, at publication).
        // Wave 2: priced for every body, claim or none, for the
        // `ir-frame-anatomy` line.
        if entry_writes_params && crate::x64::dbg_jitc_enabled() {
            let before = self.fresh_frame_clear_skip_with(entry_writes_params, false);
            let priced = (
                fresh_clear_written_bytes(self.frame_size, &before),
                fresh_clear_written_bytes(self.frame_size, &skip),
            );
            if self.prim_colour_claim {
                self.prim_colour_clear_bytes = priced;
            }
            self.frame_clear_bytes = priced;
        }
        let avx = ir_zero_fresh_frames_avx_enabled();
        // JIT round 25 wave 6 (W6-A, `CRATONVM_JIT_IR_FRESH_CLEAR_COVER`): the
        // same words, as a minimum set of straight-line stores (section above
        // `ir_fresh_clear_cover_enabled`); the census line prints either way.
        let cover_on = true;
        if entry_writes_params && crate::x64::dbg_jitc_enabled() {
            let census = self.fresh_clear_census(&skip, avx, cover_on);
            eprintln!(
                "{}",
                census.line(&self.graph.scope_method_key, self.frame_size)
            );
        }
        if cover_on {
            if let Some(stores) = fresh_clear_cover_stores(self.frame_size, &skip, avx) {
                Self::emit_zero_frame_cover(&mut self.buf, &stores, avx);
                return skip;
            }
        }
        if skip.is_empty() && !avx {
            Self::emit_zero_frame_sse(&mut self.buf, self.frame_size);
        } else {
            Self::emit_zero_frame_words(&mut self.buf, self.frame_size, &skip, avx);
        }
        skip
    }

    /// Did the prologue's fresh-frame clear leave the word `[rbp - off]` out?
    pub(in crate::ir_lower) fn fresh_clear_skipped(&self, off: i32) -> bool {
        off > 0
            && off % 8 == 0
            // Cast: a positive frame offset over 8, far below `usize::MAX`.
            && self.fresh_clear_skip.get((off / 8 - 1) as usize).copied().unwrap_or(false)
    }

    /// Round 16 wave 7 (lane frames): which words of this frame the fresh-frame
    /// clear may leave out, as a mask indexed by word `w` = `[rbp - 8(w + 1)]`,
    /// or empty for "none" (both switches off, or nothing to skip). See the
    /// section above `ir_zero_fresh_frames_skip_unrooted_enabled` for why each
    /// class is safe; every word NOT in the mask is zeroed exactly as before.
    ///
    /// A lone written word between two cleared runs is cleared after all:
    /// zeroing it is always correct (it is the baseline), and splitting a run
    /// for one word costs a store rather than saving one.
    pub(in crate::ir_lower) fn fresh_frame_clear_skip(
        &self,
        entry_writes_params: bool,
    ) -> Vec<bool> {
        self.fresh_frame_clear_skip_with(entry_writes_params, self.prim_colour_claim)
    }

    /// The per-body claim (`CompiledMethod::ir_prim_colour_claim`), ascending
    /// and distinct, when `prim` (empty otherwise): item 3's `Prim` colours
    /// ([`Self::prim_slot_offsets`]), and -- JIT round 25 wave 2, each only
    /// when its switch was on in [`Lowerer::new`] -- the claimable `Pinned`
    /// colours ([`Self::pinned_prim_slot_offsets`]) and the deopt GPR image's
    /// words ([`Self::deopt_image_claim`]). ONE function for the clear's skip
    /// ([`Self::fresh_frame_clear_skip_with`]) and the publication, so the
    /// clear never leaves out a word the collector is not told about. With the
    /// wave-2 switches off this is exactly `prim_slot_offsets` (already
    /// ascending and distinct: one offset per colour).
    ///
    /// Every word lies inside the frame: a colour is below `spill_cap_off`
    /// and the image above the staging region. The collector's half reads the
    /// list only as "never root an object start here" (`ir_prim_colour_claim`
    /// in `vm/src/jit/conservative_roots.rs`), which needs nothing of a word but
    /// that no reference of this activation is ever stored there while the
    /// collector can look -- the soundness argument of each class.
    /// JIT round 25 wave 3 (`CRATONVM_JIT_IR_SNAPSHOT_RANGE_COLOURS`): may a
    /// frame state describe value `id` by its home word here? Always for a
    /// value the plan did not range. For a ranged one (its colour is shared
    /// by range, [`apply_snapshot_range_colours`]):
    ///
    /// * at a point no deopt can arrive at (`quiet_dropped_home`) and in a
    ///   table point (`building_table_points`: a guard bakes its own box, and
    ///   every site at that bci was checked when it was lowered), yes;
    /// * inside an OSR entry stub, only at a bci whose block start the plan
    ///   made read the value (`SnapshotRanged::entry_bcis`);
    /// * while a data node or a terminator is lowered, only at a position the
    ///   plan recorded as reading the value (the node's, or for a terminator
    ///   its own or its block's outgoing edge);
    /// * anywhere else (a pad or exit resolved after the body) no.
    ///
    /// A `false` refuses the attempt (`frame_value_for`); the ladder then
    /// lowers again without the ranging.
    pub(in crate::ir_lower) fn snapshot_ranged_home_ok(&self, id: NodeId) -> bool {
        let Some(sr) = self.slot_plan.snapshot_ranged.as_deref() else {
            return true;
        };
        let Some(reads) = sr.reads.get(&id) else {
            return true;
        };
        if self.quiet_dropped_home.get() || self.building_table_points.get() {
            return true;
        }
        if let Some(bci) = self.snapshot_range_osr_bci.get() {
            return sr
                .entry_bcis
                .get(&id)
                .is_some_and(|bcis| bcis.binary_search(&bci).is_ok());
        }
        let pos = |n: NodeId| self.slot_plan.pos_of.get(n as usize).copied().flatten();
        let read_at = |p: usize| reads.binary_search(&p).is_ok();
        match (self.cur_def, self.cur_term) {
            (Some(d), _) => pos(d).is_some_and(read_at),
            (None, Some(t)) => {
                let edge = self
                    .schedule
                    .node_to_block
                    .get(t as usize)
                    .and_then(|&b| sr.span.get(b))
                    .map(|&(_, e)| e);
                pos(t).is_some_and(read_at) || edge.is_some_and(read_at)
            }
            (None, None) => false,
        }
    }

    pub(in crate::ir_lower) fn body_claim_offsets_with(&self, prim: bool) -> Vec<i16> {
        if !prim {
            return Vec::new();
        }
        let mut offs: Vec<i16> = self.prim_slot_offsets.clone();
        offs.extend_from_slice(&self.pinned_prim_slot_offsets);
        if self.deopt_image_claim {
            let (lo, hi) = self.deopt_gpr_image();
            if lo > 0 && hi > lo && hi - 8 <= self.frame_size {
                for off in (lo..hi).step_by(8) {
                    if let Ok(o) = i16::try_from(off) {
                        offs.push(o);
                    }
                }
            }
        }
        // JIT round 25 wave 3: the helper-argument area, `(frame_size - 48,
        // frame_size]` -- the words just above `RSP` at a call, below the
        // staging region's `arg[0]` (`args_stage_top_off == frame_size - 48`).
        if self.helper_area_claim && self.frame_size >= IR_HELPER_AREA_BYTES {
            let lo = self.frame_size - IR_HELPER_AREA_BYTES + 8;
            for off in (lo..=self.frame_size).step_by(8) {
                if let Ok(o) = i16::try_from(off) {
                    offs.push(o);
                }
            }
        }
        offs.sort_unstable();
        offs.dedup();
        offs
    }

    /// JIT round 25 wave 2, `CRATONVM_DBG_JITC` only: this body's frame by
    /// region and its clear, the tail of the `ir-frame-anatomy` line
    /// (`lib.rs` prints it with the method's name). `frame_before` is the frame
    /// the plan would have had with every Java local reserved and no sizing
    /// step (`estimate_frame_bytes` over `max_locals`); the clear's two numbers
    /// are [`Self::frame_clear_bytes`] (no round-25 claim, then as emitted, on
    /// this frame). Colours are counted once per colour, by class; a `Pinned`
    /// colour is split by its one occupant's type.
    pub(in crate::ir_lower) fn frame_anatomy(&self, frame_before: i32) -> String {
        let plan = self.slot_plan;
        // 1 prim, 2 pinned non-ref, 3 ref, 4 pinned ref.
        let mut kind = vec![0u8; plan.slots];
        let mut pinned_phis = 0usize;
        for (id, class) in plan.class.iter().enumerate() {
            let Some(class) = *class else {
                continue;
            };
            let Some(Some(colour)) = plan.node_color.get(id).copied() else {
                continue;
            };
            let node = self.graph.nodes.get(id);
            let is_ref = node.is_some_and(|n| n.ty == IrType::Ref);
            let k = match class {
                SlotClass::Prim => 1u8,
                SlotClass::Ref => 3,
                SlotClass::Pinned if is_ref => 4,
                SlotClass::Pinned => 2,
            };
            if class == SlotClass::Pinned && node.is_some_and(|n| n.op == Op::Phi) {
                pinned_phis += 1;
            }
            if let Some(slot) = kind.get_mut(colour as usize) {
                *slot = k;
            }
        }
        let bytes = |k: u8| kind.iter().filter(|&&x| x == k).count() * 8;
        let ctx = if self.needs_context { 8 } else { 0 };
        format!(
            "frame {frame_before} -> {} bytes, clear {} -> {} bytes; locals {} (params {}), \
             ctx+bookkeeping {}, colours {} -> {} = prim {} + pinned-prim {} + ref {} + \
             pinned-ref {} (peak live {} words, pinned phis {}, snapshot-ranged {} values), \
             saves {}, deopt image {}, staging {}, reserve+shadow 48; claim {} word(s) \
             ({} pinned-prim{})",
            self.frame_size,
            self.frame_clear_bytes.0,
            self.frame_clear_bytes.1,
            self.locals_size,
            self.num_params * 8,
            ctx + 40,
            plan.slots_before_ranging() * 8,
            plan.slots * 8,
            bytes(1),
            bytes(2),
            bytes(3),
            bytes(4),
            plan.peak_live,
            pinned_phis,
            plan.snapshot_ranged_count(),
            self.saved_xmm_bytes + self.saved_gpr_bytes,
            self.deopt_regs_bytes,
            self.args_stage_words * 8,
            self.body_claim_offsets_with(self.prim_colour_claim).len(),
            self.pinned_prim_slot_offsets.len(),
            match (
                self.deopt_image_claim && self.deopt_regs_base > 0,
                self.helper_area_claim,
            ) {
                (true, true) => ", deopt image, helper area",
                (true, false) => ", deopt image",
                (false, true) => ", helper area",
                (false, false) => "",
            },
        )
    }

    /// [`Self::fresh_frame_clear_skip`] with the round-25 primitive-colour
    /// class (`prim`) stated rather than read from the lowerer, so the debug
    /// line can price the clear without it.
    ///
    /// JIT round 25 (item 3, `CRATONVM_JIT_IR_PRIMITIVE_COLOUR_CLAIM`): with
    /// `prim`, every word of [`Self::prim_slot_offsets`] is left out too. Such
    /// a word is a `Prim` colour: `assign_colors` keeps the `Ref` and `Prim`
    /// free lists disjoint and `verify_slot_colouring` refuses a colour shared
    /// across classes, so no value of this activation ever stores a reference
    /// there. The body publishes the same list as its per-body claim
    /// (`CompiledMethod::ir_prim_colour_claim`), and the collector's band scan
    /// drops an object-start word at a claimed offset at EVERY poll -- the
    /// entry poll and any other point with no current safepoint id included,
    /// where the per-map claim (`OopMapEntry::non_oop_stack_slots`) abstains.
    /// So a stale value the previous occupant left in such a word is never a
    /// root, and zeroing it buys nothing. Like a `_SKIP_WRITTEN` word, a lone
    /// claimed word between two cleared ones is cleared after all.
    ///
    /// JIT round 25 wave 2: `prim` leaves out the whole per-body claim
    /// ([`Self::body_claim_offsets_with`]) -- the `Prim` colours, plus the
    /// claimable `Pinned` colours and the deopt GPR image when their switches
    /// were on in [`Lowerer::new`]. The publication names the same list.
    pub(in crate::ir_lower) fn fresh_frame_clear_skip_with(
        &self,
        entry_writes_params: bool,
        prim: bool,
    ) -> Vec<bool> {
        let unrooted = ir_zero_fresh_frames_skip_unrooted_enabled();
        let written = ir_zero_fresh_frames_skip_written_enabled();
        let claimed = self.body_claim_offsets_with(prim);
        let prim = !claimed.is_empty();
        if (!unrooted && !written && !prim) || self.frame_size <= 0 || self.frame_size % 8 != 0 {
            return Vec::new();
        }
        // Cast: a positive frame size over 8 (at most 32 KiB / 8).
        let words = (self.frame_size / 8) as usize;
        let mut skip = vec![false; words];
        // `unrooted` words may never be re-included; `written` ones may.
        let mut fixed = vec![false; words];
        let index = |off: i32| -> Option<usize> {
            // Cast: a positive frame offset over 8.
            (off > 0 && off % 8 == 0 && off <= self.frame_size).then_some((off / 8 - 1) as usize)
        };
        if unrooted {
            if let Some((lo, hi)) = self.deopt_xmm_image() {
                for off in (lo..hi).step_by(8) {
                    if let Some(w) = index(off) {
                        skip[w] = true;
                        fixed[w] = true;
                    }
                }
            }
        }
        if written {
            let mut offs: Vec<i32> = Vec::new();
            // `MOVUPS [rbp - off]` writes the 16 bytes `[rbp - off, rbp - off + 16)`:
            // the words at `off` and `off - 8`.
            for (_, off) in self.saved_xmm_regs() {
                offs.push(off);
                offs.push(off - 8);
            }
            for (_, off) in self.saved_gpr_regs() {
                offs.push(off);
            }
            if self.needs_context && self.context_slot_off > 0 {
                offs.push(self.context_slot_off);
            }
            if entry_writes_params {
                for i in 0..self.num_params {
                    // Cast: a JVM local index times 8 -- `max_locals` is u16.
                    offs.push(((i as i32) + 1) * 8);
                }
            }
            for off in offs {
                if let Some(w) = index(off) {
                    skip[w] = true;
                }
            }
        }
        if prim {
            for &off in &claimed {
                if let Some(w) = index(i32::from(off)) {
                    skip[w] = true;
                }
            }
        }
        if written || prim {
            // Re-include a lone written (or claimed) word between two cleared
            // words.
            for w in 1..words.saturating_sub(1) {
                if skip[w] && !fixed[w] && !skip[w - 1] && !skip[w + 1] {
                    skip[w] = false;
                }
            }
        }
        if skip.iter().any(|&s| s) {
            skip
        } else {
            Vec::new()
        }
    }

    /// Zero every word of `[rbp - frame_size, rbp)` whose index `w` (`[rbp -
    /// 8(w + 1)]`) `skip` does not mark, and nothing else: each maximal run of
    /// such words, from the top of the frame down, gets [`Self::emit_zero_run`].
    /// `avx`: 32-byte VEX stores, `VPXOR` (VEX.128, which also clears YMM5's
    /// upper half) before and `VZEROUPPER` after, so no legacy-SSE instruction
    /// ever runs with a dirty upper state. Writes XMM5 (YMM5), R10, R11 and
    /// the flags, as [`Self::emit_zero_frame_sse`] does; the descending order
    /// across runs keeps every page met from above (a skipped run is far
    /// shorter than a page).
    pub(in crate::ir_lower) fn emit_zero_frame_words(
        buf: &mut ExecutableBuffer,
        frame_size: i32,
        skip: &[bool],
        avx: bool,
    ) {
        if frame_size <= 0 || frame_size % 8 != 0 {
            return;
        }
        // Cast: a positive frame size over 8.
        let words = (frame_size / 8) as usize;
        let skipped = |w: usize| skip.get(w).copied().unwrap_or(false);
        let mut runs: Vec<(i32, i32)> = Vec::new();
        let mut w = 0usize;
        while w < words {
            if skipped(w) {
                w += 1;
                continue;
            }
            let start = w;
            while w < words && !skipped(w) {
                w += 1;
            }
            // Cast: word indices below 4096 (a frame is at most 32 KiB).
            runs.push((start as i32 * 8, w as i32 * 8));
        }
        if runs.is_empty() {
            return;
        }
        if avx {
            buf.emit(&[0xC5, 0xD1, 0xEF, 0xED]); // VPXOR XMM5, XMM5, XMM5
        } else {
            buf.emit(&[0x66, 0x0F, 0xEF, 0xED]); // PXOR XMM5, XMM5
        }
        for (top, bottom) in runs {
            Self::emit_zero_run(buf, top, bottom, avx);
        }
        if avx {
            buf.emit(&[0xC5, 0xF8, 0x77]); // VZEROUPPER
        }
    }

    /// Zero `[rbp - bottom, rbp - top)` from XMM5 / YMM5 (already zero), from
    /// the top down: above [`ZERO_FRAME_SSE_STRAIGHT_BYTES`] a loop of four
    /// wide stores per iteration (`R10` walks down from `rbp - top` to `R11`),
    /// then the rest straight-line, a 16-byte store (AVX only) and an 8-byte
    /// one for what a wide store would overshoot. With `top == 0`,
    /// `bottom == frame_size` and `avx` off, these are exactly the bytes of
    /// [`Self::emit_zero_frame_sse`] after its `PXOR`.
    pub(in crate::ir_lower) fn emit_zero_run(
        buf: &mut ExecutableBuffer,
        top: i32,
        bottom: i32,
        avx: bool,
    ) {
        let len = bottom - top;
        if len <= 0 || len % 8 != 0 || top < 0 {
            return;
        }
        let wide: i32 = if avx { 32 } else { 16 };
        let step = 4 * wide;
        let looped = if len > ZERO_FRAME_SSE_STRAIGHT_BYTES {
            (len / step) * step
        } else {
            0
        };
        if looped > 0 {
            if top == 0 {
                buf.emit(&[0x49, 0x89, 0xEA]); // MOV R10, RBP
            } else {
                buf.emit(&[0x4C, 0x8D, 0x95]); // LEA R10, [RBP + disp32]
                buf.emit(&(-top).to_le_bytes());
            }
            buf.emit(&[0x4C, 0x8D, 0x9D]); // LEA R11, [RBP + disp32]
            buf.emit(&(-(top + looped)).to_le_bytes());
            let head = buf.pos();
            if step == 64 {
                buf.emit(&[0x49, 0x83, 0xEA, 0x40]); // SUB R10, 64
            } else {
                buf.emit(&[0x49, 0x83, 0xC2, 0x80]); // ADD R10, -128
            }
            for k in (0..4).rev() {
                Self::emit_clear_store(buf, true, k * wide, wide, avx);
            }
            buf.emit(&[0x4D, 0x39, 0xDA]); // CMP R10, R11

            // Cast: the loop body is at most 30 bytes, so the rel8 fits.
            let rel = head as i64 - (buf.pos() as i64 + 2);
            buf.emit(&[0x77, rel as i8 as u8]); // JA .loop
        }
        let mut at = top + looped;
        while bottom - at >= wide {
            at += wide;
            Self::emit_clear_store(buf, false, -at, wide, avx);
        }
        if bottom - at >= 16 {
            at += 16;
            Self::emit_clear_store(buf, false, -at, 16, avx);
        }
        if bottom - at == 8 {
            Self::emit_clear_store(buf, false, -bottom, 8, avx);
        }
    }

    /// One store of XMM5 / YMM5's low `width` bytes (8, 16 or 32; 32 only
    /// with `vex`) to `[RBP + disp]` or `[R10 + disp]`: `MOVQ` / `MOVDQU`
    /// (legacy SSE) or `VMOVQ` / `VMOVDQU` (VEX.128 / VEX.256).
    pub(in crate::ir_lower) fn emit_clear_store(
        buf: &mut ExecutableBuffer,
        on_r10: bool,
        disp: i32,
        width: i32,
        vex: bool,
    ) {
        // `F3 0F 7F` is MOVDQU m, xmm; `66 0F D6` is MOVQ m64, xmm.
        let (pp, opcode) = if width == 8 {
            (1u8, 0xD6u8)
        } else {
            (2u8, 0x7Fu8)
        };
        if vex {
            let l = u8::from(width == 32);
            if on_r10 {
                // 3-byte VEX: R/X set (inverted), B clear (R10), map 0F;
                // W0, vvvv = 1111 (unused), L, pp.
                buf.emit(&[0xC4, 0xC1, 0x78 | (l << 2) | pp]);
            } else {
                // 2-byte VEX: R set (inverted), vvvv = 1111, L, pp.
                buf.emit(&[0xC5, 0xF8 | (l << 2) | pp]);
            }
            buf.emit_byte(opcode);
        } else {
            debug_assert!(width != 32, "a 32-byte store needs VEX");
            buf.emit_byte(if pp == 1 { 0x66 } else { 0xF3 });
            if on_r10 {
                buf.emit_byte(0x41); // REX.B
            }
            buf.emit(&[0x0F, opcode]);
        }
        // ModRM: reg = XMM5, rm = RBP (101) or R10 (010 with REX/VEX .B).
        let rm: u8 = if on_r10 { 2 } else { 5 };
        if on_r10 && disp == 0 {
            buf.emit_byte((5 << 3) | rm);
        } else if let Ok(d8) = i8::try_from(disp) {
            buf.emit_byte(0x40 | (5 << 3) | rm);
            buf.emit(&d8.to_le_bytes());
        } else {
            buf.emit_byte(0x80 | (5 << 3) | rm);
            buf.emit(&disp.to_le_bytes());
        }
    }

    pub(in crate::ir_lower) fn emit_zero_whole_frame(&mut self) {
        self.buf.emit(&[0x45, 0x31, 0xDB]); // XOR R11D, R11D
        self.buf.emit(&[0x49, 0x89, 0xEA]); // MOV R10, RBP
                                            // .loop:
        self.buf.emit(&[0x49, 0x83, 0xEA, 0x08]); // SUB R10, 8
        self.buf.emit(&[0x4D, 0x89, 0x1A]); // MOV [R10], R11
        self.buf.emit(&[0x49, 0x39, 0xE2]); // CMP R10, RSP
        self.buf.emit(&[0x77, 0xF4]); // JA .loop  (rel8 -12: the four instructions above)
    }

    /// Restore the shadow `top` captured at method entry, unwinding any push
    /// this activation did not pop. Mirrors the single-pass backend's epilogue.
    /// R10/R11 are caller-saved and dead at every exit; RAX (the return value,
    /// or the `i64::MIN` sentinel on the bail path) is untouched.
    /// Can this body NEVER publish a shadow root? Round 11 wave 13 (lane fib).
    ///
    /// Every publication comes from [`Self::emit_safepoint_map`], whose slot
    /// list is `ref_param_homes` plus the defined members of `ref_node_ids`
    /// (and a frame block is planned from the same two sets), so with both
    /// empty `shadow_pushed_any` can never become true and
    /// [`Self::finish_lazy_thread_fetch`] would erase the thread fetch and
    /// every shadow-`top` restore. Not emitting them leaves the same runtime
    /// state -- the prologue still zeroes the thread and watermark words, so
    /// every null-guarded reader of the thread slot sees the `0` it saw after
    /// the erase -- without the taken `JMP` over each span: one on entry and
    /// one per return, on every activation of a reference-free method
    /// (`CratonBench fib`). The debug assertion in `finish_lazy_thread_fetch`
    /// checks the prediction.
    pub(in crate::ir_lower) fn body_publishes_no_roots(&self) -> bool {
        // `frame_block` is built from those two lists, so it is empty whenever
        // they are; asked explicitly so the deopt stub's retraction can never
        // be skipped for a block this predicate did not see.
        self.ref_node_ids.is_empty()
            && self.ref_param_homes.is_empty()
            && self.frame_block.is_empty()
    }

    pub(in crate::ir_lower) fn emit_shadow_savetop_restore(&mut self) {
        if self.get_current_thread == 0
            || self.shadow_thread_slot_off <= 0
            || self.shadow_savetop_slot_off <= 0
            || self.body_publishes_no_roots()
            // W6-D: a leaf frame's prologue neither fetched the thread nor
            // captured the watermark, and it pushes nothing to unwind.
            || self.leaf_entry.plan.leaf_frame
        {
            return;
        }
        let ss_top = self.shadow_off_in_thread;
        let start = self.buf.pos();
        // MOV R10, [rbp - thread_slot] ; TEST R10,R10 ; JE skip
        self.buf.emit(&[0x4C, 0x8B, 0x95]);
        self.buf.emit(&(-self.shadow_thread_slot_off).to_le_bytes());
        self.buf.emit(&[0x4D, 0x85, 0xD2]);
        self.buf.emit(&[0x0F, 0x84]);
        let skip = self.buf.pos();
        self.buf.emit(&[0, 0, 0, 0]);
        // MOV R11, [rbp - savetop] ; MOV [R10 + ss_top], R11
        self.buf.emit(&[0x4C, 0x8B, 0x9D]);
        self.buf
            .emit(&(-self.shadow_savetop_slot_off).to_le_bytes());
        self.buf.emit(&[0x4D, 0x89, 0x9A]);
        self.buf.emit(&ss_top.to_le_bytes());
        self.patch_rel32_to_here(skip);
        // The JE above lands exactly here, so a jump over the whole span is the
        // path a zero thread slot already takes.
        self.savetop_restore_spans.push((start, self.buf.pos()));
    }

    pub(in crate::ir_lower) fn emit_epilogue(&mut self) {
        self.emit_shadow_savetop_restore();
        self.emit_callee_saved_restore();
        // add rsp, frame_size
        self.buf.emit(&[0x48, 0x81, 0xC4]);
        self.buf.emit(&self.frame_size.to_le_bytes());
        // pop rbp
        self.buf.emit_byte(0x5D);
        // ret
        self.buf.emit_byte(0xC3);
    }

    /// MOV RAX, [RBP - offset]
    ///
    /// Emits the shorter disp8 form (mod=01, 4 bytes) when `neg`
    /// fits in a signed 8-bit value, falling back to disp32 (mod=10,
    /// 7 bytes) otherwise. Most spill slots for typical methods sit
    /// within ±128 bytes of RBP, so the disp8 form is the common case
    /// and saves 3 bytes per frame access.
    pub(in crate::ir_lower) fn load_to_rax(&mut self, offset: i32) {
        let start = self.buf.pos();
        let mut bytes = FrameAccess::new();
        enc_frame_load(RAX, offset, &mut bytes);
        self.buf.emit(bytes.as_slice());
        self.note_home_reload(offset, start, RAX);
    }

    /// MOV RCX, [RBP - offset]
    pub(in crate::ir_lower) fn load_to_rcx(&mut self, offset: i32) {
        self.note_rcx_written();
        let start = self.buf.pos();
        let mut bytes = FrameAccess::new();
        enc_frame_load(RCX, offset, &mut bytes);
        self.buf.emit(bytes.as_slice());
        self.note_home_reload(offset, start, RCX);
    }

    /// A 64-bit frame load of `[rbp - offset]` into `dst` was just emitted at
    /// `start`. When the instruction right before it was [`Self::store_rax`]'s
    /// store of that same word, record it for
    /// [`Self::apply_home_reload_forwards`].
    pub(in crate::ir_lower) fn note_home_reload(&mut self, offset: i32, start: usize, dst: u8) {
        if !self.home_reload_forward_on {
            return;
        }
        let Some((src, off, store_start, store_end)) = self.rax_home_mirror.take() else {
            return;
        };
        let end = self.buf.pos();
        if off != offset || store_end != start || end <= start || end - start > 8 {
            return;
        }
        if self.buf.overflowed() {
            return;
        }
        let Some(bytes) = self.buf.as_slice().get(store_start..end) else {
            return;
        };
        let bytes = bytes.to_vec();
        self.home_reload_forwards.push(HomeReloadForward {
            store_start,
            start,
            end,
            src,
            dst,
            bytes,
        });
    }

    /// `CRATONVM_JIT_IR_HOME_RELOAD_FORWARD`: rewrite every load
    /// [`Self::note_home_reload`] recorded -- `MOV r, [rbp - off]` right after
    /// `MOV [rbp - off], RAX` -- into `MOV r, RAX` (a `NOP` when `r` is RAX),
    /// padded to its length so no offset moves, unless some instruction of the
    /// finished body may branch to it ([`home_reload_targets`]): on the
    /// fall-through path the word holds RAX, on a branch into the load it may
    /// not. Runs after every branch is patched; a site whose bytes changed
    /// since (a rewind, an erased range) is left alone.
    pub(in crate::ir_lower) fn apply_home_reload_forwards(&mut self) {
        let forwards = std::mem::take(&mut self.home_reload_forwards);
        if forwards.is_empty() || self.buf.overflowed() {
            return;
        }
        let len = self.buf.pos();
        let base = self.buf.as_ptr() as u64;
        let wanted: std::collections::HashSet<usize> = forwards.iter().map(|f| f.start).collect();
        let targeted = match self.buf.as_slice().get(..len) {
            Some(code) => home_reload_targets(code, base, &wanted),
            None => return,
        };
        let (mut rewritten, mut kept) = (0usize, 0usize);
        for f in forwards {
            if targeted.contains(&f.start) {
                kept += 1;
                continue;
            }
            let unchanged = self
                .buf
                .as_slice()
                .get(f.store_start..f.end)
                .is_some_and(|now| now == f.bytes.as_slice());
            if !unchanged || f.end > len {
                kept += 1;
                continue;
            }
            let n = f.end - f.start;
            let mut repl: Vec<u8> = Vec::with_capacity(n);
            if f.dst != f.src {
                // MOV dst, src (`REX.W 89 /r`, reg = src, r/m = dst).
                repl.extend_from_slice(&[
                    0x48 | (u8::from(f.src >= 8) << 2) | u8::from(f.dst >= 8),
                    0x89,
                    0xC0 | ((f.src & 7) << 3) | (f.dst & 7),
                ]);
            }
            let Some(pad) = n
                .checked_sub(repl.len())
                .and_then(|k| HOME_RELOAD_NOPS.get(k))
            else {
                kept += 1;
                continue;
            };
            repl.extend_from_slice(pad);
            for (k, b) in repl.into_iter().enumerate() {
                if self.buf.try_patch_byte(f.start + k, b).is_err() {
                    return;
                }
            }
            rewritten += 1;
        }
        if false {
            eprintln!("[ir-ls] home reload forwards: rewritten={rewritten} kept={kept}");
        }
    }

    /// MOV [RBP - offset], RAX
    pub(in crate::ir_lower) fn store_rax(&mut self, offset: i32) {
        // Whatever the arm offered, it is this store's to use or drop: a twin
        // never outlives the store that follows it (round 9 wave 11).
        let twin = self.rcx_twin.take();
        // The int poisoning stress mode, for the home write of an `Int`
        // definition only. Emitted before the store, the register publish and
        // the carry, so all three hand on the poisoned word; a twin offer then
        // no longer ends at `buf.pos()` and declines, which costs one `MOV`.
        if let Some(id) = self.cur_def {
            if self.poisons_int_def(id) && self.home_offset_of(id) == Some(offset) {
                self.emit_poison_int_high(RAX);
            }
        }
        // JIT round 25 item 1a (`CRATONVM_JIT_IR_ELIDE_UNNAMED_HOMES`): the
        // home write of a planned TAIL carry is not emitted at all. RAX is the
        // value here (that is what this store would have written), and its one
        // consumer reads it at exactly this position or the attempt refuses
        // (`gp_load_value`, `lower_data_node_tracked`, `lower_terminator`,
        // `verify_tail_carry_points`). A second home write of the same value
        // means the producer has another route to its home, whose register
        // nothing proves reaches the consumer: refuse that too.
        // An inline call prefix was emitted for this node: its hit edge joins
        // after the arm with the home written and is the second route to the
        // consumer, so the store stays and the tail carry simply does not
        // happen (the consumer reads the home, which both routes write).
        // JIT round 26: unless the prefix's hit edge provably arrives with the
        // result in RAX too (`prefix_hit_proves_rax`): then both routes reach
        // the consumer with RAX = the value, and the store goes.
        let mut prefixed = self.call_prefix_join.is_some();
        if prefixed && self.tail_carry_at_store(offset).is_some() {
            match self.prefix_hit_proves_rax(offset) {
                Some((jmp, operand)) => {
                    if let Some((id, _)) = self.tail_carry_at_store(offset) {
                        self.prefix_rax_proofs.push((jmp, operand, id));
                    }
                    prefixed = false;
                }
                None => self.tail_prefix_kept += 1,
            }
        }
        if let Some((id, cons)) = self.tail_carry_at_store(offset).filter(|_| !prefixed) {
            if self
                .tail_carry_seen
                .get(id as usize)
                .copied()
                .unwrap_or(true)
            {
                self.tail_carries_refused += 1;
                self.latch_bailout(Bailout::with_context(
                    BailoutReason::UnallocatedValue { node: id },
                    format!("n{id}'s elided home store (consumer n{cons}) has a second home write"),
                ));
            } else {
                if let Some(cell) = self.tail_carry_seen.get_mut(id as usize) {
                    *cell = true;
                }
                // From here on the home word is never written: every other
                // reader must refuse (`slot_of`, `frame_value_for`).
                if let Some(cell) = self.home_dropped.get_mut(id as usize) {
                    *cell = true;
                }
                self.tail_carry = Some((id, cons, self.buf.pos()));
                self.tail_stores_elided += 1;
                // Nothing was stored, so no later load reads a word RAX mirrors.
                self.rax_home_mirror = None;
                return;
            }
        }
        // Two independent decisions, deliberately kept apart. Whether this
        // definition is CARRIED to its consumer decides where the consumer
        // reads it; whether its home was DROPPED decides if the word is
        // written at all. A carried value normally still writes its home,
        // because `graph.safepoints` names it.
        let carry = self.carry_at_store(offset);
        let drop_store = carry
            .is_some_and(|(id, _, _)| self.home_dropped.get(id as usize).copied().unwrap_or(false));
        if !drop_store && !self.publish_def_at_store(offset) {
            let start = self.buf.pos();
            let mut bytes = FrameAccess::new();
            enc_frame_store(RAX, offset, &mut bytes);
            self.buf.emit(bytes.as_slice());
            // The `ir-home-stores` census: a definition's own home write.
            if self
                .cur_def
                .is_some_and(|d| self.home_offset_of(d) == Some(offset))
            {
                self.def_home_stores += 1;
                // JIT round 25 wave 6 (lane W6-B): the `ir-home-why` census.
                if let Some(d) = self.cur_def {
                    self.note_home_keep(d);
                }
                // JIT round 25 item 2, wave 3: the lazy clear's write record
                // (empty, so a no-op, without a plan).
                if let Some(cell) = self
                    .cur_def
                    .and_then(|d| self.own_home_written.get_mut(d as usize))
                {
                    *cell = offset;
                }
                // Wave 4: the `[ir-lazy-ref]` census of reference definitions.
                if self
                    .cur_def
                    .and_then(|d| self.graph.nodes.get(d as usize))
                    .is_some_and(|n| n.ty == IrType::Ref)
                {
                    self.ref_def_home_stores += 1;
                }
                if crate::x64::dbg_jitc_enabled() {
                    if let Some(d) = self.cur_def {
                        let class = self.home_keep_class(d) as usize;
                        self.home_keep_census[class] += 1;
                        let f = self
                            .schedule
                            .freq
                            .freq
                            .get(self.cur_lower_block)
                            .copied()
                            .unwrap_or(0.0);
                        self.home_keep_freq[class] += f;
                        let key = format!("{}:{}", HOME_KEEP_NAMES[class], self.home_keep_shape(d));
                        let cell = self.home_keep_shapes.entry(key).or_insert((0, 0.0, d));
                        cell.0 += 1;
                        cell.1 += f;
                    }
                }
            }
            // `CRATONVM_JIT_IR_HOME_RELOAD_FORWARD`: RAX is this word's value
            // at `buf.pos()`, the next instruction (`note_home_reload`).
            if self.home_reload_forward_on {
                self.rax_home_mirror = Some((RAX, offset, start, self.buf.pos()));
            }
            // JIT rx27 wave 9: a home store writes memory only, so the
            // publish right before it still mirrors RAX after it.
            if let Some((id, reg, end)) = self.rax_reg_mirror {
                if end == start && self.cur_def == Some(id) {
                    self.rax_reg_mirror = Some((id, reg, self.buf.pos()));
                }
            }
        }
        if let Some((id, reg, cons)) = carry {
            // RAX already holds it — that is what the store was about to
            // write. RCX costs one register move and still removes a load.
            // `_raw`, because this write to RCX is the carry being INSTALLED:
            // counting it would make the producer's own node look as if it had
            // clobbered the value it just placed.
            //
            // Round 9 wave 11 (irl11): or, when the arm offered an RCX twin of
            // the run it just emitted, that twin instead -- same RCX result,
            // one instruction fewer. Uncounted for the same reason as the
            // `MOV`: it IS the carry being installed. See `RcxTwin`.
            if reg != RAX && !self.apply_rcx_twin(twin, id, reg, drop_store) {
                self.emit_mov_reg_reg64_raw(reg, RAX);
            }
            if self
                .carry_deferred
                .get(id as usize)
                .copied()
                .unwrap_or(false)
            {
                // The DEFERRED slot. `reg` is RCX by construction (the planner
                // only defers a consumer's second operand), and the value has
                // to survive exactly one arm before its consumer reads it.
                debug_assert_eq!(reg, RCX, "a deferred carry is RCX-only");
                self.deferred_rcx = Some((id, cons, 1));
                self.carry_deferred_planned += 1;
            } else {
                self.live_carry = Some((id, cons, reg, self.buf.pos()));
            }
            self.carries_taken += 1;
            if drop_store {
                self.carry_stores_dropped += 1;
            }
        }
    }

    /// Why `id`'s definition home store is emitted, for the `ir-home-keep`
    /// census line. A diagnosis only: nothing is decided from it.
    pub(in crate::ir_lower) fn home_keep_class(&self, id: NodeId) -> HomeKeep {
        let i = id as usize;
        let Some(node) = self.graph.nodes.get(i) else {
            return HomeKeep::Other;
        };
        match node.ty {
            IrType::Ref => return HomeKeep::Ref,
            IrType::Float | IrType::Double => return HomeKeep::Fp,
            _ => {}
        }
        if matches!(node.op, Op::Phi) || self.phi_coalesced_src.get(i).copied().unwrap_or(false) {
            return HomeKeep::Phi;
        }
        let reach = self.elide_reach.get(i).copied().unwrap_or(true);
        let held = self.elide_held.get(i).copied().unwrap_or(true);
        if self.assigned_gpr(id).is_some() {
            return if held {
                HomeKeep::ResidentHeld
            } else if reach {
                HomeKeep::ResidentReach
            } else {
                HomeKeep::ResidentOther
            };
        }
        let uses = self.use_count.get(i).copied().unwrap_or(0);
        if uses > 1 {
            return HomeKeep::SpilledMultiUse;
        }
        if held {
            return HomeKeep::SingleHeld;
        }
        if reach {
            return HomeKeep::SingleReach;
        }
        // Single use, named nowhere a deopt can read: is its consumer the next
        // thing lowered?
        let adjacent = self
            .schedule
            .node_to_block
            .get(i)
            .and_then(|&b| self.schedule.blocks.get(b))
            .is_some_and(|blk| {
                let pos = blk.nodes.iter().position(|&n| n == id);
                match pos {
                    Some(p) if p + 1 < blk.nodes.len() => self
                        .graph
                        .nodes
                        .get(blk.nodes[p + 1] as usize)
                        .is_some_and(|c| c.inputs.contains(&id)),
                    Some(_) => blk.terminator.is_some_and(|t| {
                        self.graph
                            .nodes
                            .get(t as usize)
                            .is_some_and(|c| c.inputs.contains(&id))
                    }),
                    None => false,
                }
            });
        if adjacent {
            HomeKeep::SingleAdjacent
        } else {
            HomeKeep::SingleDistant
        }
    }

    /// JIT round 26 (item 1a, multi-route producers): does the pending inline
    /// call prefix's hit edge arrive at the join with the call's result in
    /// RAX? Proven from the bytes, not from the prefix's code: the hit `JMP
    /// rel32` (opcode at `call_prefix_join - 1`) must be immediately preceded
    /// by `MOV [RBP - offset], RAX`, the result's own home store -- so RAX is
    /// the value the home was just given, and the `JMP` changes no register.
    /// A route that jumps to that `JMP` instead of falling into it is not
    /// covered: `verify_tail_carry_points` refuses the value if any branch
    /// targets the `JMP`. `Some((jmp, operand))` when proven.
    pub(in crate::ir_lower) fn prefix_hit_proves_rax(&self, offset: i32) -> Option<(usize, usize)> {
        let operand = self.call_prefix_join?;
        let jmp = operand.checked_sub(1)?;
        let code = self.buf.as_slice();
        if code.get(jmp).copied() != Some(0xE9) {
            return None;
        }
        let mut store = FrameAccess::new();
        enc_frame_store(RAX, offset, &mut store);
        let want = store.as_slice();
        if want.is_empty() || jmp < want.len() {
            return None;
        }
        (code.get(jmp - want.len()..jmp)? == want).then_some((jmp, operand))
    }

    /// RB25-1 step 2 (`CRATONVM_JIT_IR_OSR_FLAG_IF_FOLD`): may the `XOR; store`
    /// of the jump-entered flag `id` (`OsrEntryFlag`, `CatchEntryFlag`) be left
    /// out? Only when nothing reads the word: no frame state names the flag,
    /// and every user is an `If` on it that ends a block and that
    /// [`Self::constant_if_live_block`] folds -- such an `If` emits a jump and
    /// reads nothing. The entry stub and a handler probe jump past the `If`
    /// into its arm, and neither reads the flag either.
    pub(in crate::ir_lower) fn jump_entered_flag_store_unread(&self, id: NodeId) -> bool {
        if self.mir_mode != MirMode::Off {
            return false;
        }
        let named = self.graph.safepoints.iter().any(|sp| {
            sp.locals
                .iter()
                .chain(sp.stack.iter())
                .chain(sp.monitors.iter())
                .any(|&v| v == id)
        });
        if named {
            return false;
        }
        let mut readers = 0usize;
        for (u, n) in self.graph.nodes.iter().enumerate() {
            if n.op == Op::Dead || !n.inputs.contains(&id) {
                continue;
            }
            if n.op != Op::If || n.inputs.get(1) != Some(&id) || n.inputs.first() == Some(&id) {
                return false;
            }
            let u = u as NodeId;
            let Some(b) = self
                .schedule
                .blocks
                .iter()
                .position(|blk| blk.terminator == Some(u))
            else {
                return false;
            };
            if self.constant_if_live_block(u, id, b).is_none() {
                return false;
            }
            readers += 1;
        }
        readers > 0
    }

    /// `producer->consumer#operand` of `id`'s first user, for the
    /// `ir-home-keep` census.
    pub(in crate::ir_lower) fn home_keep_shape(&self, id: NodeId) -> String {
        fn op_name(op: &Op) -> String {
            let d = format!("{op:?}");
            d.split(|c: char| c == '(' || c == '{' || c == ' ')
                .next()
                .unwrap_or("")
                .to_string()
        }
        let Some(node) = self.graph.nodes.get(id as usize) else {
            return "?".into();
        };
        let user = self
            .graph
            .nodes
            .iter()
            .find(|n| n.op != Op::Dead && n.inputs.contains(&id));
        match user {
            Some(u) => {
                let k = u.inputs.iter().position(|&x| x == id).unwrap_or(0);
                format!("{}->{}#{k}", op_name(&node.op), op_name(&u.op))
            }
            None => format!("{}->none", op_name(&node.op)),
        }
    }

    /// `id`'s planned home offset (`node_slot`), dropped or not.
    pub(in crate::ir_lower) fn home_offset_of(&self, id: NodeId) -> Option<i32> {
        self.node_slot
            .get(id as usize)
            .copied()
            .flatten()
            .map(|off| off.get() as i32)
    }

    /// Does the int poisoning stress mode poison `id`'s definition? Only an
    /// `IrType::Int` value, and never under a MIR mode, whose byte-equality
    /// lane would then compare two different programs.
    pub(in crate::ir_lower) fn poisons_int_def(&self, id: NodeId) -> bool {
        self.poison_int_high_on
            && self.mir_mode == MirMode::Off
            && self
                .graph
                .nodes
                .get(id as usize)
                .is_some_and(|n| n.ty == IrType::Int)
    }

    /// `CRATONVM_DBG_IR_POISON_INT_HIGH=1` (round 12 wave 2, lane irback,
    /// proposal R12-1): add garbage into bits 32..63 of the `Int` value in
    /// `reg`, leaving bits 0..31, every other register and the flags as they
    /// were.
    ///
    /// This tier's int convention is that a consumer reads the LOW 32 bits
    /// only: a 32-bit ALU result is zero-extended, `MOVSXD` producers
    /// (`iaload`, `F2I`, `LCmp`, a negative `Const`) sign-extend, and nothing
    /// promises either. A 64-bit read of an `int` is therefore a wrong answer
    /// for the one input whose extension differs, found so far only by bug
    /// reports. Under this mode the high half is neither extension, so every
    /// such consumer fails on every input, in the debug suite and the probe
    /// battery, including consumers in other tiers that receive the word as an
    /// argument or a return value.
    ///
    /// `PUSH R11; MOV R11, 0xA5A5A5A5_00000000; LEA reg, [reg + R11]; POP R11`:
    /// `LEA` is the flag-free add, the zero low half of the addend keeps
    /// bits 0..31 exact, and the push/pop keeps R11 (the tier's scratch, which
    /// a caller may still hold) intact. Never the default: it grows every
    /// `Int` definition by 18 bytes.
    pub(in crate::ir_lower) fn emit_poison_int_high(&mut self, reg: u8) {
        if let Some((bytes, len)) = poison_int_high_bytes(reg) {
            self.buf.emit(&bytes[..len]);
        }
    }

    /// Swap in the twin `store_rax` was handed, if it describes exactly the run
    /// that ends here, for exactly this definition, and the carry it would
    /// install is an RCX carry whose home store was dropped. Answers whether it
    /// did; `false` leaves the caller's `MOV RCX, RAX` to run as before.
    ///
    /// Every condition is re-checked here rather than trusted from the offer:
    /// the offer knew what the arm emitted, only this knows what the store
    /// decided.
    pub(in crate::ir_lower) fn apply_rcx_twin(
        &mut self,
        twin: Option<RcxTwin>,
        id: NodeId,
        reg: u8,
        drop_store: bool,
    ) -> bool {
        let Some(t) = twin else {
            return false;
        };
        if !self.rcx_twin_on
            || reg != RCX
            || !drop_store
            || t.id != id
            || self.cur_def != Some(id)
            || t.end != self.buf.pos()
            || t.start >= t.end
            || t.end - t.start != t.len
            || t.len > RCX_TWIN_MAX
        {
            return false;
        }
        // Patched IN PLACE, byte by byte, rather than rewound and re-emitted:
        // same length, so every offset at or past `t.start` keeps its meaning,
        // nothing inside the run is a patch site (both kinds of run are plain
        // register instructions), and `ExecutableBuffer::wanted` -- which this
        // backend's retry sizing reads as exact because it never rewinds --
        // stays exact. A failed patch marks the buffer overflowed, and the
        // compile is then discarded whatever this answers.
        let mut i = 0usize;
        while i < t.len {
            let byte = t.bytes[i];
            let _ = self.buf.try_patch_byte(t.start + i, byte);
            i += 1;
        }
        self.rcx_twins += 1;
        true
    }

    /// Record `twin` as the RCX twin of the `orig` bytes that END at the
    /// current buffer position, for the current definition. Records nothing
    /// (and clears any stale offer) unless the buffer really ends with `orig`.
    pub(in crate::ir_lower) fn record_rcx_twin(&mut self, orig: &[u8], twin: &[u8]) {
        self.rcx_twin = None;
        if !self.rcx_twin_on
            || orig.len() != twin.len()
            || orig.is_empty()
            || orig.len() > RCX_TWIN_MAX
        {
            return;
        }
        let Some(id) = self.cur_def else {
            return;
        };
        let end = self.buf.pos();
        let Some(start) = end.checked_sub(orig.len()) else {
            return;
        };
        if self.buf.as_slice().get(start..end) != Some(orig) {
            return;
        }
        let mut bytes = [0u8; RCX_TWIN_MAX];
        bytes[..twin.len()].copy_from_slice(twin);
        self.rcx_twin = Some(RcxTwin {
            id,
            start,
            end,
            bytes,
            len: twin.len(),
        });
    }

    /// Offer the RCX twin of a commutative register-form ALU instruction
    /// (`Op::Add`/`And`/`Or`/`Xor`: `op r/m, r` with ModRM `C8`, i.e. `op
    /// RAX, RCX`; `Op::Mul`: `IMUL RAX, RCX`, `0F AF C1`) that the current
    /// arm has just emitted. The twin is the same instruction with the two
    /// registers exchanged -- `op RCX, RAX` -- which is the same value because
    /// the op commutes. Called only from inside the arms' register-form
    /// branches, right after the instruction.
    pub(in crate::ir_lower) fn offer_alu_rcx_twin(&mut self) {
        let Some(id) = self.cur_def else {
            return;
        };
        let Some(node) = self.graph.nodes.get(id as usize) else {
            return;
        };
        let wide = match node.ty {
            IrType::Int => false,
            IrType::Long => true,
            _ => return,
        };
        // (opcode bytes, how many of them, ModRM as emitted, ModRM of the twin)
        let (op0, op1, op_len, orig_modrm, twin_modrm): (u8, u8, usize, u8, u8) = match node.op {
            Op::Add => (0x01, 0, 1, 0xC8, 0xC1),
            Op::Or => (0x09, 0, 1, 0xC8, 0xC1),
            Op::And => (0x21, 0, 1, 0xC8, 0xC1),
            Op::Xor => (0x31, 0, 1, 0xC8, 0xC1),
            Op::Mul => (0x0F, 0xAF, 2, 0xC1, 0xC8),
            _ => return,
        };
        let mut orig = [0u8; RCX_TWIN_MAX];
        let mut twin = [0u8; RCX_TWIN_MAX];
        let mut n = 0usize;
        if wide {
            orig[n] = 0x48;
            twin[n] = 0x48;
            n += 1;
        }
        orig[n] = op0;
        twin[n] = op0;
        n += 1;
        if op_len == 2 {
            orig[n] = op1;
            twin[n] = op1;
            n += 1;
        }
        orig[n] = orig_modrm;
        twin[n] = twin_modrm;
        n += 1;
        self.record_rcx_twin(&orig[..n], &twin[..n]);
    }

    /// Offer the RCX twin of a folded shift of a RESIDENT value: `MOV RAX, r;
    /// <shift> EAX/RAX, imm8` becomes `MOV RCX, r; <shift> ECX/RCX, imm8`.
    /// `modrm` is the value the arm handed `emit_shift_imm` (`0xE0` SHL,
    /// `0xF8` SAR, `0xE8` SHR, all `rm = RAX`). Offers nothing when the count
    /// did not fold, when `x` has no register, or when the buffer does not end
    /// with exactly that pair (an RAX carry into the shift, a constant `x`) --
    /// the byte check in [`Self::record_rcx_twin`] decides, not this.
    pub(in crate::ir_lower) fn offer_shift_rcx_twin(
        &mut self,
        x: NodeId,
        count: NodeId,
        modrm: u8,
        wide: bool,
    ) {
        self.rcx_twin = None;
        if !self.rcx_twin_on || !matches!(modrm, 0xE0 | 0xE8 | 0xF8) {
            return;
        }
        let Some(imm) = self.alu_imm32(count) else {
            return;
        };
        let Some(src) = self.resident_gpr(x) else {
            return;
        };
        if src == RAX || src == RCX || src > 15 {
            return;
        }
        // The same mask `emit_shift_imm` applied.
        let c = ((imm as u32) & if wide { 63 } else { 31 }) as u8;
        // `MOV r/m64, r64` (89 /r), exactly as `emit_mov_reg_reg64_raw`
        // encodes it: REX.W, REX.R for the source, `rm` = the destination.
        let rex = 0x48u8 | (((src >= 8) as u8) << 2);
        let mov_modrm = 0xC0 | ((src & 7) << 3);
        let mut orig = [0u8; RCX_TWIN_MAX];
        let mut twin = [0u8; RCX_TWIN_MAX];
        orig[..3].copy_from_slice(&[rex, 0x89, mov_modrm]);
        twin[..3].copy_from_slice(&[rex, 0x89, mov_modrm | 1]);
        let mut n = 3usize;
        if wide {
            orig[n] = 0x48;
            twin[n] = 0x48;
            n += 1;
        }
        orig[n..n + 3].copy_from_slice(&[0xC1, modrm, c]);
        twin[n..n + 3].copy_from_slice(&[0xC1, modrm | 1, c]);
        n += 3;
        self.record_rcx_twin(&orig[..n], &twin[..n]);
    }

    /// Is this store the home write of a value planned to be carried, and if
    /// so, `(the value, the register, its consumer)`?
    pub(in crate::ir_lower) fn carry_at_store(&self, offset: i32) -> Option<(NodeId, u8, NodeId)> {
        let id = self.cur_def?;
        let (reg, cons) = self.carry_of.get(id as usize).copied().flatten()?;
        let home = self
            .node_slot
            .get(id as usize)
            .copied()
            .flatten()
            .map(|off| off.get() as i32)?;
        (home == offset).then_some((id, reg, cons))
    }

    /// JIT round 25 item 1a: is this store the home write of a value
    /// [`Self::plan_tail_carries`] planned, and if so `(the value, its
    /// consumer)`? Keyed on `cur_def` exactly as [`Self::carry_at_store`]:
    /// only the definition's OWN home write, never a stage or shadow word.
    pub(in crate::ir_lower) fn tail_carry_at_store(&self, offset: i32) -> Option<(NodeId, NodeId)> {
        let id = self.cur_def?;
        let cons = self.tail_carry_of.get(id as usize).copied().flatten()?;
        let home = self.home_offset_of(id)?;
        (home == offset).then_some((id, cons))
    }

    /// Refuse THIS attempt because a tail carry did not reach its consumer the
    /// way it was planned to. Never a wrong answer: the value's home is
    /// `home_dropped`, so every other read refuses as well. Never the method
    /// either: the attempt planned a tail carry, so it reported a dropped home
    /// (`image_dropped`) and `lower_inner_with_mode_exit_polls` lowers once
    /// more with every home kept.
    pub(in crate::ir_lower) fn refuse_tail_carry(&mut self, why: &'static str) {
        if let Some((prod, cons, _)) = self.tail_carry.take() {
            self.tail_carries_refused += 1;
            self.latch_bailout(Bailout::with_context(
                BailoutReason::UnallocatedValue { node: prod },
                format!("n{prod}'s elided home store (consumer n{cons}) did not hold: {why}"),
            ));
        }
    }

    /// Refuse the compile because a carry did not reach its consumer the way it
    /// was planned to.
    ///
    /// Never a wrong answer, always a coverage loss, and `carries_refused`
    /// names it. The fallback is safe on its own terms too: a carried value's
    /// home is `home_dropped`, so any read that gets past here refuses in
    /// `slot_of_checked`.
    /// Refuse the compile because the DEFERRED carry did not reach its
    /// consumer the way it was planned to.
    ///
    /// The sibling of [`Self::refuse_carry`] and, like it, a coverage loss and
    /// never a wrong answer: a deferred value's home is `home_dropped`, so any
    /// read that gets past here refuses in `slot_of_checked`.
    pub(in crate::ir_lower) fn refuse_deferred(&mut self, why: &'static str) {
        if let Some((prod, cons, _)) = self.deferred_rcx.take() {
            self.carries_refused += 1;
            self.latch_bailout(Bailout::with_context(
                BailoutReason::UnallocatedValue { node: prod },
                format!("n{prod}'s deferred carry to n{cons} did not hold: {why}"),
            ));
        }
    }

    pub(in crate::ir_lower) fn refuse_carry(&mut self, why: &'static str) {
        if let Some((prod, cons, _, _)) = self.live_carry.take() {
            self.carries_refused += 1;
            self.latch_bailout(Bailout::with_context(
                BailoutReason::UnallocatedValue { node: prod },
                format!("n{prod}'s carry to n{cons} did not hold: {why}"),
            ));
        }
    }

    /// Count a write to RCX, and kill a deferred carry it has just destroyed.
    ///
    /// The OBSERVATION half of the deferred carry's proof. `op_preserves_rcx`
    /// and [`Self::node_preserves_rcx`] are audits of SOURCE, so they can be
    /// wrong in the one direction that matters — claiming an arm leaves RCX
    /// alone when it does not. Every route by which this file reaches RCX
    /// (`gp_load_value`, `load_to_rcx`, `emit_mov_reg_reg64` and the two
    /// immediate forms) calls this first, so a wrong audit costs the METHOD, at
    /// the node that did it, and never a wrong answer at run time.
    ///
    /// The counter is what `lower_data_node_tracked` compares across a node:
    /// "did this arm write RCX" is then a fact about the emission rather than a
    /// prediction about the op, which is what lets the audit widen past the two
    /// ops whose arms mention no RCX at all.
    pub(in crate::ir_lower) fn note_rcx_written(&mut self) {
        self.rcx_writes += 1;
        if self.deferred_rcx.is_some() {
            self.refuse_deferred("RCX was written before its consumer read it");
        }
    }

    /// Recognise the store that writes the CURRENT definition's own home word,
    /// publish RAX into its register there, and answer whether the store itself
    /// may now be skipped.
    ///
    /// This is what makes the other forty-nine `store_rax` sites reachable
    /// without editing forty-nine arms. The two facts the register image needed
    /// -- that RAX holds a particular value, and WHICH value -- are both present
    /// here and nowhere else in this file: `cur_def` supplies the identity, and
    /// "the offset is that value's home" supplies the rest, because the only
    /// thing a home word is ever written with is its own value.
    ///
    /// Writing the register cannot disturb an operand still to be read. Every
    /// input of `id` is live at its definition, and the allocator gives one
    /// register to one value across its live range, so no input of `id` is in
    /// `id`'s register. (`deopt_nameable`, which a dropped home requires, is
    /// stronger still: no value ANYWHERE holds it.)
    pub(in crate::ir_lower) fn publish_def_at_store(&mut self, offset: i32) -> bool {
        if !self.publish_at_def_on {
            return false;
        }
        let Some(id) = self.cur_def else {
            return false;
        };
        let home = self
            .node_slot
            .get(id as usize)
            .copied()
            .flatten()
            .map(|off| off.get() as i32);
        if home != Some(offset) {
            return false;
        }
        let Some(reg) = self.assigned_gpr(id) else {
            // Not resident: nothing to publish, and nothing may be dropped.
            return false;
        };
        self.emit_mov_reg_reg64(reg, RAX);
        self.mark_gp_reg_live(id);
        self.reg_publishes_at_def += 1;
        self.cur_def_published = true;
        // JIT rx27 wave 9: RAX and `reg` hold the same word here.
        if self.publish_reload_elide_on
            && !self
                .publish_reload_excluded
                .get(id as usize)
                .copied()
                .unwrap_or(true)
        {
            self.rax_reg_mirror = Some((id, reg, self.buf.pos()));
        }
        if self.home_dropped.get(id as usize).copied().unwrap_or(false) {
            self.homes_dropped_at_def += 1;
            self.home_stores_dropped += 1;
            // W8-4: where this route's register write ends.
            if self.reg_alloc_on {
                self.reg_alloc_publish_at = Some((id, self.buf.pos()));
            }
            return true;
        }
        false
    }

    /// `MOV qword [RBP - offset], imm32` (sign-extended), or `false` when the
    /// value needs all 64 bits and the caller must go through a register.
    ///
    /// A constant's home word is still written — a deopt frame may name it,
    /// and a handful of sites read home slots directly — but since every
    /// READER materialises a constant as an immediate
    /// (`ir_const_imm_enabled`), nothing needs it in RAX on the way there.
    /// One instruction instead of two, at every `Op::Const` in every method.
    pub(in crate::ir_lower) fn emit_store_frame_imm32(&mut self, offset: i32, val: i64) -> bool {
        let Ok(imm) = i32::try_from(val) else {
            return false;
        };
        self.buf.emit_byte(0x48); // REX.W
        self.buf.emit_byte(0xC7); // MOV r/m64, imm32 (/0)
        self.emit_rbp_modrm_disp(0, offset);
        self.buf.emit(&imm.to_le_bytes());
        true
    }

    /// MOV RAX, imm64
    pub(in crate::ir_lower) fn emit_mov_rax_imm64(&mut self, val: i64) {
        if val >= i32::MIN as i64 && val <= i32::MAX as i64 {
            // MOV EAX, imm32 (sign-extended to 64-bit)
            if val >= 0 && val <= u32::MAX as i64 {
                self.buf.emit_byte(0xB8);
                self.buf.emit(&(val as u32).to_le_bytes());
            } else {
                // MOV RAX, imm32 sign-extended
                self.buf.emit(&[0x48, 0xC7, 0xC0]);
                self.buf.emit(&(val as i32).to_le_bytes());
            }
        } else {
            // MOV RAX, imm64
            self.buf.emit(&[0x48, 0xB8]);
            self.buf.emit(&val.to_le_bytes());
        }
    }

    /// The safepoint slow path's second argument (`HelperFnSafepointSlowPath`'s
    /// `body_id`), on the slow path only.
    ///
    /// A method-entry compile whose polls may leave
    /// ([`IrPollModeExits::MethodEntry`]; interpreter round i1 wave 18, lane
    /// L2) names its body: `MOV r64, compile_id`, the id the prologue and every
    /// post-call republish store into the compile-id mirror
    /// ([`Self::emit_frame_record_identity`]), so the VM, which trusts the id
    /// only when it equals that mirror (`helpers::polling_body_id`), answers
    /// `SAFEPOINT_VERDICT_POLLING_BODY` about this body's method, as it does
    /// for a single-pass body (`x64::safepoint::emit_safepoint_body_id_arg`).
    /// Every other compile, and one with no id reserved, passes
    /// `XOR r32, r32` (wave 17, lane L1): a defined 0 rather than a register
    /// the VM would have to rule out against the mirror. The OSR door's
    /// compile leaves on the innermost-frame bit, but since interpreter round
    /// i1 wave 23 (lane L6) it names its body too, so the VM can tell a body a
    /// class redefinition withdrew (`helpers::jit_safepoint_loop_exit_verdict`
    /// then sets that bit for it); its OSR entry stub publishes the same id
    /// (`emit_frame_record`), so the mirror confirms it.
    pub(in crate::ir_lower) fn emit_poll_body_id_arg(&mut self) {
        let reg = CALL_ARG_REGS[1];
        let id = self.compile_id.id();
        if matches!(
            self.poll_mode_exits,
            IrPollModeExits::MethodEntry
                | IrPollModeExits::MethodEntryNamedOnly
                | IrPollModeExits::OsrDoor
        ) && id != 0
        {
            self.emit_mov_reg_imm64(reg, u64::from(id));
            return;
        }
        if reg == RCX {
            self.note_rcx_written();
        }
        if reg >= 8 {
            self.buf.emit_byte(0x45); // REX.R + REX.B
        }
        self.buf.emit(&[0x31, 0xC0 | ((reg & 7) << 3) | (reg & 7)]); // XOR r32, r32
    }

    /// JIT round 18 wave 11 (lane osrfollow, OSRPOLY18-1): the
    /// `Op::ExactClassIs` arm's test of the reference in RAX, answered in the
    /// flags for a fused `If`: **ZF=1 iff** the reference is non-null, a plain
    /// object (kind bits clear) and of exactly `class_id`. RAX is clobbered.
    /// Fixed-length, so every jump is rel8:
    ///
    /// ```text
    ///   +0  TEST RAX, RAX                 48 85 C0
    ///   +3  JZ null (+15)                 74 0F
    ///   +5  TEST BYTE [RAX+kind], mask    F6 40 kk mm   ; non-plain: ZF=0
    ///   +9  JNE done (+12)                75 0C
    ///  +11  CMP DWORD [RAX+0], class_id   81 78 00 id32 ; ZF = match
    ///  +18  JMP done (+3)                 EB 03
    ///  +20  null: OR EAX, 1               83 C8 01      ; ZF=0
    ///  +23  done:
    /// ```
    pub(in crate::ir_lower) fn emit_exact_class_test_flags(&mut self, class_id: u32) {
        self.buf.emit(&exact_class_test_flags_bytes(class_id));
    }

    pub(in crate::ir_lower) fn emit_exact_class_test_flags_for(
        &mut self,
        obj: NodeId,
        class_id: u32,
    ) {
        if self.receiver_already_non_null(obj) {
            self.buf
                .emit(&exact_class_test_flags_non_null_bytes(class_id));
        } else {
            self.emit_exact_class_test_flags(class_id);
        }
    }

    /// Emit 1..=3 scalar array element stores (`base[j + k] = val`) using `R9`
    /// as the value scratch, expecting `RAX = base`, `RDX = j`, `R11 = n - j`
    /// with `1 <= R11 <= 3`. Clobbers `R9`.
    pub(in crate::ir_lower) fn emit_small_scalar_fill(&mut self, inv: NodeId, elem_bytes: u8) {
        self.gp_load_value(R9, inv);
        let disp0 = cratonvm_types::ARRAY_DATA_OFFSET as i32;
        let emit_store_at = |buf: &mut ExecutableBuffer, disp: i32| {
            let d8 = disp as u8;
            match elem_bytes {
                1 => buf.emit(&[0x44, 0x88, 0x4C, 0x10, d8]), // MOV BYTE [RAX+RDX+d8], R9B
                2 => buf.emit(&[0x66, 0x44, 0x89, 0x4C, 0x50, d8]), // MOV WORD [RAX+RDX*2+d8], R9W
                4 => buf.emit(&[0x44, 0x89, 0x4C, 0x90, d8]), // MOV DWORD [RAX+RDX*4+d8], R9D
                8 => buf.emit(&[0x4C, 0x89, 0x4C, 0xD0, d8]), // MOV QWORD [RAX+RDX*8+d8], R9
                _ => {}
            }
        };
        emit_store_at(&mut self.buf, disp0);
        self.buf.emit(&[0x41, 0x83, 0xFB, 0x01]); // CMP R11D, 1
        let d1 = self.emit_jcc_rel32(0x84); // JE done
        emit_store_at(&mut self.buf, disp0 + i32::from(elem_bytes));
        self.buf.emit(&[0x41, 0x83, 0xFB, 0x02]); // CMP R11D, 2
        let d2 = self.emit_jcc_rel32(0x84); // JE done
        emit_store_at(&mut self.buf, disp0 + 2 * i32::from(elem_bytes));
        self.patch_rel32_to_here(d1);
        self.patch_rel32_to_here(d2);
    }

    /// Return `Some((exit_block, exits_on_ge))` when `header_block` is a pure
    /// loop-bound header whose exit edge carries no phi copies and can be jumped
    /// to directly once `RDX == bound` (or `RDX >= bound` when `exits_on_ge`).
    pub(in crate::ir_lower) fn trivial_vec_header_exit(
        &self,
        header_block: usize,
        plan: &crate::ir_vector_loop::VecLoopPlan,
    ) -> Option<(usize, bool)> {
        if plan.reduce.is_some() || plan.scan.is_some() {
            return None;
        }
        let hb = self.schedule.blocks.get(header_block)?;
        if hb.successors.len() != 2 {
            return None;
        }
        let term = hb.terminator?;
        let term_node = self.graph.nodes.get(term as usize)?;
        if term_node.op != Op::If || term_node.inputs.len() < 2 {
            return None;
        }
        let cond_id = term_node.inputs[1];
        let cond = self.graph.nodes.get(cond_id as usize)?;
        let Op::Cmp(cmp_op) = cond.op else {
            return None;
        };
        if cond.inputs.len() != 2 {
            return None;
        }
        let (true_edge_exits, exits_on_ge) = match (cmp_op, cond.inputs[0], cond.inputs[1]) {
            (crate::ir::CmpOp::Ge, a, b) | (crate::ir::CmpOp::Le, b, a)
                if a == plan.iv && b == plan.bound =>
            {
                (true, true)
            }
            (crate::ir::CmpOp::Lt, a, b) | (crate::ir::CmpOp::Gt, b, a)
                if a == plan.iv && b == plan.bound =>
            {
                (false, true)
            }
            (crate::ir::CmpOp::Eq, a, b)
                if (a == plan.iv && b == plan.bound) || (b == plan.iv && a == plan.bound) =>
            {
                (true, false)
            }
            (crate::ir::CmpOp::Ne, a, b)
                if (a == plan.iv && b == plan.bound) || (b == plan.iv && a == plan.bound) =>
            {
                (false, false)
            }
            _ => return None,
        };
        let want_proj = if true_edge_exits { 0u16 } else { 1u16 };
        let exit_block = hb.successors.iter().copied().find(|&sb| {
            let Some(sb_blk) = self.schedule.blocks.get(sb) else {
                return false;
            };
            sb_blk.predecessors.len() == 1
                && self.graph.nodes.get(sb_blk.ctrl as usize).is_some_and(|n| {
                    matches!(n.op, Op::Proj(w) if w == want_proj)
                        && n.inputs.first().copied() == Some(term)
                })
        })?;
        if exit_block == header_block || self.edge_has_phi_copies(header_block, exit_block) {
            return None;
        }
        for &nid in &hb.nodes {
            let n = self.graph.nodes.get(nid as usize)?;
            match &n.op {
                Op::Phi => {
                    if nid != plan.iv && n.ty != IrType::Void {
                        return None;
                    }
                }
                Op::Cmp(_) if nid == cond_id => {}
                Op::ArrayLength if plan.bound_length_of.is_some() && nid == plan.bound => {}
                Op::Const(_) | Op::ConstF(_) => {}
                _ => return None,
            }
            if n.op != Op::Phi {
                let used_outside = self.graph.nodes.iter().enumerate().any(|(uid, u)| {
                    self.schedule.node_to_block.get(uid).copied() != Some(header_block)
                        && u.inputs.contains(&nid)
                });
                if used_outside {
                    return None;
                }
                let in_safepoint = self.graph.safepoints.iter().any(|sp| {
                    sp.locals.contains(&nid)
                        || sp.stack.contains(&nid)
                        || sp.monitors.contains(&nid)
                });
                if in_safepoint {
                    return None;
                }
            }
        }
        Some((exit_block, exits_on_ge))
    }

    /// Return `Some(class_id)` if a dominating `Guard(ExactClassIs(v, cid))` or
    /// `If(ExactClassIs(v, cid))` true-arm proves `value` is a non-null instance
    /// of exact class `cid` at the entry/position in `block`.
    pub(in crate::ir_lower) fn dominating_exact_class(
        &self,
        value: NodeId,
        block: usize,
    ) -> Option<u32> {
        let dom = &self.schedule.dom;
        if !dom.is_reachable(block) {
            return None;
        }
        let target_canon = self.field_proof_canon(value);
        // 1. Check same-block dominating `Op::Guard` before `self.cur_def`.
        if let Some(blk) = self.schedule.blocks.get(block) {
            for &nid in &blk.nodes {
                if Some(nid) == self.cur_def {
                    break;
                }
                let Some(n) = self.graph.nodes.get(nid as usize) else {
                    continue;
                };
                if !matches!(n.op, Op::Guard { .. }) || n.inputs.len() < 2 {
                    continue;
                }
                if let Some(cond) = self.graph.nodes.get(n.inputs[1] as usize) {
                    if let Some((tested, class_id)) = self.exact_class_from_guard_cond(cond) {
                        if tested == value || self.field_proof_canon(tested) == target_canon {
                            return Some(class_id);
                        }
                    }
                }
            }
        }
        // 2. Check dominating `Op::Proj(0)` / `Op::Proj(1)` arms of `Op::If(...)`.
        let entries = self.field_proof_entry_blocks();
        let no_throw_entries =
            self.call_throw_entry_blocks.is_empty() && self.graph.catch_arm_sites.is_empty();
        for (tb, blk) in self.schedule.blocks.iter().enumerate() {
            if !dom.is_reachable(tb) || !dom.dominates(tb, block) {
                continue;
            }
            if tb != block && !no_throw_entries {
                continue;
            }
            if tb != block
                && entries.iter().any(|&e| {
                    e != tb
                        && Self::osr_entry_can_reach(self.schedule, e, Some(tb), block, true)
                })
            {
                continue;
            }
            if blk.predecessors.len() != 1 {
                continue;
            }
            let Some(ctrl_node) = self.graph.nodes.get(blk.ctrl as usize) else {
                continue;
            };
            let proj = match ctrl_node.op {
                Op::Proj(p @ (0 | 1)) => p,
                _ => continue,
            };
            let Some(&if_id) = ctrl_node.inputs.first() else {
                continue;
            };
            let Some(if_node) = self.graph.nodes.get(if_id as usize) else {
                continue;
            };
            if if_node.op != Op::If || if_node.inputs.len() < 2 {
                continue;
            }
            if let Some(cond) = self.graph.nodes.get(if_node.inputs[1] as usize) {
                if let Some((tested, class_id)) =
                    self.exact_class_from_branch_cond(cond, u32::from(proj))
                {
                    if tested == value || self.field_proof_canon(tested) == target_canon {
                        return Some(class_id);
                    }
                }
            }
        }
        None
    }

    /// Load `String.coder` from `RAX` into `RDX` without touching `value`.
    pub(in crate::ir_lower) fn emit_str_coder_load(
        &mut self,
        layout: &crate::StringFieldLayout,
        compact_proven: bool,
    ) {
        let emit_compact = |this: &mut Self| {
            let off = layout.coder_compact_offset;
            if layout.coder_compact_is_byte {
                if let Ok(d8) = i8::try_from(off) {
                    this.buf.emit(&[0x0F, 0xB6, 0x50, d8 as u8]); // MOVZX EDX, BYTE [RAX + d8]
                } else {
                    this.buf.emit(&[0x0F, 0xB6, 0x90]); // MOVZX EDX, BYTE [RAX + d32]
                    this.buf.emit(&off.to_le_bytes());
                }
            } else {
                this.emit_load_base_disp32(RDX, RAX, off, false);
            }
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
        let done = self.emit_jmp_rel32();
        self.patch_rel32_to_here(legacy);
        self.emit_load_base_disp32(RDX, RAX, layout.coder_legacy_offset, false);
        self.patch_rel32_to_here(done);
    }

    /// MOV reg, imm64 (REX.W [+ REX.B for r8–r15]).
    /// Interpreter round i5 wave 102, lane L2: a throw-site stamp (a bci,
    /// possibly with the defer bit) with this method's stamp token in the high
    /// half (`ir::Graph::throw_stamp_token`), for `set_throw_bci`,
    /// `throw_exception` and `throw_aioobe`
    /// (`crate::implicit_null::throw_stamp_with_owner`,
    /// `CRATONVM_JIT_THROW_STAMP_OWNER`). A function of the method alone, so
    /// codegen stays deterministic; unchanged without a token. The single-pass
    /// twin is `x64::Compiler::emit_throw_stamp_arg`.
    pub(in crate::ir_lower) fn throw_stamp_word(&self, stamp: u64) -> u64 {
        // Casts: bit reinterpretation both ways; a stamp is below 2^31.
        crate::implicit_null::throw_stamp_with_owner(stamp as i64, self.graph.throw_stamp_token)
            as u64
    }

    pub(in crate::ir_lower) fn emit_mov_reg_imm64(&mut self, reg: u8, val: u64) {
        if reg == RCX {
            self.note_rcx_written();
        }
        let rex = 0x48 | if reg >= 8 { 0x01 } else { 0 }; // REX.W (+REX.B)
        self.buf.emit_byte(rex);
        self.buf.emit_byte(0xB8 + (reg & 7));
        self.buf.emit(&val.to_le_bytes());
    }

    /// MOV reg, RBP (REX.W [+ REX.B]).
    pub(in crate::ir_lower) fn emit_mov_reg_rbp(&mut self, reg: u8) {
        let rex = 0x48 | if reg >= 8 { 0x01 } else { 0 }; // REX.W (+REX.B for r/m)
        self.buf.emit_byte(rex);
        self.buf.emit_byte(0x89);
        // ModRM: mod=11, reg=RBP(5), r/m=reg → 0xC0 | (5<<3) | (reg&7)
        self.buf.emit_byte(0xE8 | (reg & 7));
    }

    // ── FP value tier (inc 30) — XMM scratch helpers ─────────────────────
    //
    // The naive spill-everything model extends cleanly to FP: every value
    // (int/long/ref AND float/double) lives in a frame slot as raw bits. FP
    // arithmetic and conversions load operand bits from slots into XMM0/XMM1
    // (scratch — caller-saved on both ABIs, and no value lives across nodes),
    // compute, and store the result back. Constants and FP negation never touch
    // XMM: a float/double constant is just its bit pattern written via a GPR
    // immediate, and negation is a sign-bit XOR on the integer bit pattern.
    // XMM0/XMM1 are the FP analogues of RAX/RCX. The `[rbp - offset]` operand
    // goes through `emit_rbp_modrm_disp`, which picks disp8 or disp32 — the
    // ModRM `reg` field is an XMM number here rather than a GPR number, which
    // changes nothing about the encoding of the memory half.
    //
    // ── REX, and where it goes (R14) ─────────────────────────────────
    //
    // The ModRM `reg` field is three bits, so XMM8–XMM15 need REX.R and
    // without it XMM8 encodes as XMM0 — a silent truncation, and the worst
    // shape of wrong code this file can emit. Every helper below that takes an
    // XMM NUMBER therefore encodes REX rather than assuming a low register:
    // the residency file (`IR_LOWER_LS_XMMS`) is low-numbered *today*, but
    // "the encoder cannot reach XMM8" and "the pool stops at XMM7" are
    // different facts and only the second one should be a policy decision.
    //
    // **Prefix order is not negotiable.** For the `MOVSS`/`MOVSD`/arithmetic
    // forms the mandatory SSE prefix (`F2`/`F3`) comes FIRST, then REX, then
    // the `0F` escape. A REX emitted before the `F2` is not the prefix
    // immediately preceding the opcode, so the CPU drops the extension and
    // XMM8 becomes XMM0 again — the same defect with a different spelling, and
    // one no smoke test catches. `MOVAPS` (`fp_reg_move`) and `MOVUPS`
    // (`emit_xmm_frame_move`) have no mandatory prefix, so REX is the first
    // byte there instead.

    /// MOVSS/MOVSD xmm, [rbp - offset] — load a 32/64-bit FP value from a slot.
    ///
    /// The memory operand's base is RBP (encoding 5, `< 8`), so REX.B is never
    /// needed and REX.R is the only extension this form can want.
    pub(in crate::ir_lower) fn fp_load(&mut self, xmm: u8, offset: i32, is_double: bool) {
        self.buf.emit_byte(if is_double { 0xF2 } else { 0xF3 });
        if xmm >= 8 {
            self.buf.emit_byte(0x44); // REX.R — after the SSE prefix, before 0F
        }
        self.buf.emit(&[0x0F, 0x10]);
        self.emit_rbp_modrm_disp(xmm, offset);
    }

    /// MOVSS/MOVSD [rbp - offset], xmm — store an FP value to a slot. A `MOVSS`
    /// writes only the low 4 bytes; the slot's high 4 are left stale, which is
    /// harmless because every float consumer reads it back with `MOVSS` (4 bytes).
    ///
    /// REX.R as in [`Self::fp_load`], and for the same reason.
    pub(in crate::ir_lower) fn fp_store(&mut self, offset: i32, xmm: u8, is_double: bool) {
        self.buf.emit_byte(if is_double { 0xF2 } else { 0xF3 });
        if xmm >= 8 {
            self.buf.emit_byte(0x44); // REX.R — after the SSE prefix, before 0F
        }
        self.buf.emit(&[0x0F, 0x11]);
        self.emit_rbp_modrm_disp(xmm, offset);
    }

    /// `MOVSD [rbp - offset], xmm` for **any** xmm, including `XMM8`-`XMM15`.
    ///
    /// Byte-for-byte what [`Self::fp_store`] emits for `is_double`; kept as a
    /// separate name because its caller (`emit_deopt_stub`) sweeps the WHOLE
    /// register file rather than a residency plan, and the `XMM8`-`XMM15` half
    /// of that sweep is the point rather than a possibility. A spill that
    /// silently wrote `xmm[0]` eight times would leave the upper half of the
    /// register image holding stale stack, and a `FrameValue::XmmDouble(9)`
    /// resolving against it would reconstruct a plausible wrong number. Until
    /// 2026-09-17 `fp_store` really could not reach those registers, which is
    /// what this helper existed for; `every_xmm_encoder_reaches_xmm8_through_rex` is
    /// now what keeps the two from drifting apart again.
    pub(in crate::ir_lower) fn emit_movsd_frame_from_xmm(&mut self, offset: i32, xmm: u8) {
        self.buf.emit_byte(0xF2);
        if xmm >= 8 {
            self.buf.emit_byte(0x44); // REX.R
        }
        self.buf.emit(&[0x0F, 0x11]);
        self.emit_rbp_modrm_disp(xmm, offset);
    }

    /// Scalar FP binary op (`<prefix> [REX] 0F <op>`), reg-reg form
    /// `dst op= src`. `op` is the second opcode byte: ADD=0x58, SUB=0x5C,
    /// MUL=0x59, DIV=0x5E.
    ///
    /// Two REGISTER operands, so unlike [`Self::fp_load`] this needs the full
    /// two-operand REX: REX.R for the ModRM `reg` field (`dst`) and REX.B for
    /// `r/m` (`src`). Same shape as [`Self::fp_reg_move`], one prefix byte
    /// later.
    ///
    /// Every call site passes `XMM0`/`XMM1` today — the value tier computes in
    /// the scratch pair and `fp_store_value` copies the result into whatever
    /// register residency assigned — so the extension is unreachable from this
    /// file as it stands. It is encoded anyway because the day an arm computes
    /// straight into a resident register is not the day to discover that this
    /// one truncates.
    pub(in crate::ir_lower) fn fp_binop(&mut self, op: u8, dst: u8, src: u8, is_double: bool) {
        self.buf.emit_byte(if is_double { 0xF2 } else { 0xF3 });
        // REX.R for dst >= 8, REX.B for src >= 8; `0x40` means neither.
        let rex = 0x40u8 | (u8::from(dst >= 8) << 2) | u8::from(src >= 8);
        if rex != 0x40 {
            self.buf.emit_byte(rex);
        }
        self.buf.emit(&[0x0F, op]);
        self.buf.emit_byte(0xC0 | ((dst & 7) << 3) | (src & 7));
    }

    /// IEEE-754 NaN/overflow fixup after a `CVTTSS2SI`/`CVTTSD2SI` whose source
    /// is still in XMM0 and whose (sentinel-or-real) result is in EAX/RAX.
    ///
    /// x86 `CVTT*` yields the "integer indefinite" (0x8000_0000 / 0x8000…0) for
    /// NaN AND any out-of-range/∞ input, but the JVM requires NaN→0,
    /// +overflow→MAX, −overflow→MIN. This mirrors the single-pass backend's
    /// `x64::arith::emit_fp_to_int_nan_fixup` as round 9 rewrote it, so the two
    /// backends agree bit-for-bit:
    ///
    /// * the indefinite pattern is detected with `CMP r, 1; JNO .done` (`r - 1`
    ///   overflows iff `r == MIN`) — 4/5 bytes and no scratch register, where
    ///   the old 64-bit form loaded `MOV RCX, imm64` (clobbering RCX) and the
    ///   32-bit form spent a 5-byte `CMP EAX, imm32`;
    /// * positive overflow is `NOT` of the MIN pattern (0x80..0 → 0x7F..F)
    ///   rather than a 5/10-byte immediate load. The 32-bit `NOT EAX`
    ///   zero-extends into RAX exactly as `MOV EAX, imm32` did.
    ///
    /// Uses XMM1 as scratch (PXOR to materialize +0.0 for the sign test).
    pub(in crate::ir_lower) fn emit_fp_to_int_fixup(&mut self, is_double: bool, is_long: bool) {
        // CMP EAX/RAX, 1 — OF=1 iff the value is i32::MIN / i64::MIN.
        if is_long {
            self.buf.emit(&[0x48, 0x83, 0xF8, 0x01]);
        } else {
            self.buf.emit(&[0x83, 0xF8, 0x01]);
        }
        // JNO .done — not the indefinite pattern: already correct.
        self.buf.emit_byte(0x71);
        let jno_patch = self.buf.pos();
        self.buf.emit_byte(0x00);
        // UCOMISD/UCOMISS XMM0, XMM0 — PF=1 if NaN
        if is_double {
            self.buf.emit(&[0x66, 0x0F, 0x2E, 0xC0]);
        } else {
            self.buf.emit(&[0x0F, 0x2E, 0xC0]);
        }
        // JP .nan
        self.buf.emit_byte(0x7A);
        let jp_patch = self.buf.pos();
        self.buf.emit_byte(0x00);
        // Not NaN — overflow. PXOR XMM1,XMM1 then compare sign.
        self.buf.emit(&[0x66, 0x0F, 0xEF, 0xC9]);
        if is_double {
            self.buf.emit(&[0x66, 0x0F, 0x2E, 0xC1]);
        } else {
            self.buf.emit(&[0x0F, 0x2E, 0xC1]);
        }
        // JBE .done (negative overflow — the MIN pattern is already correct)
        self.buf.emit_byte(0x76);
        let jbe_patch = self.buf.pos();
        self.buf.emit_byte(0x00);
        // Positive overflow: NOT turns MIN into MAX ; JMP .done
        if is_long {
            self.buf.emit(&[0x48, 0xF7, 0xD0]); // NOT RAX
        } else {
            self.buf.emit(&[0xF7, 0xD0]); // NOT EAX
        }
        self.buf.emit_byte(0xEB);
        let jmp_patch = self.buf.pos();
        self.buf.emit_byte(0x00);
        // .nan: XOR EAX,EAX (zeroes all of RAX for both widths)
        let nan_off = self.buf.pos();
        // Range-checked, not truncated: see
        // `ExecutableBuffer::patch_rel8_or_bail`. This sequence is
        // fixed-size and comfortably inside rel8, so the helper can only fire
        // on a genuine codegen bug.
        let rel = |target: usize, patch: usize| (target as i64) - (patch as i64) - 1;
        self.buf
            .patch_rel8_or_bail(jp_patch, rel(nan_off, jp_patch));
        self.buf.emit(&[0x31, 0xC0]);
        // .done:
        let done_off = self.buf.pos();
        self.buf
            .patch_rel8_or_bail(jno_patch, rel(done_off, jno_patch));
        self.buf
            .patch_rel8_or_bail(jbe_patch, rel(done_off, jbe_patch));
        self.buf
            .patch_rel8_or_bail(jmp_patch, rel(done_off, jmp_patch));
    }

    /// `Math.round(double)J` (`dbl`) / `Math.round(float)I` -- JIT round 22
    /// wave 1, lane intrinsics, INTRINSICS21-7
    /// (`CRATONVM_JIT_IR_MATH_ROUND_SCALAR`, opt-in). In: XMM0 = the argument,
    /// RAX (EAX) = its raw bits. Out: RAX = the result (the `int` form
    /// sign-extended). Writes RCX, RDX and, on the fallback, XMM1.
    ///
    /// The JDK 25 body, instruction for instruction:
    ///
    /// ```text
    /// shift = (SIGNIFICAND_WIDTH - 2 + EXP_BIAS) - biasedExp   // 1074 / 149
    /// if ((shift & -64) == 0) {                                // -32 for float
    ///     r = (bits & SIGNIF_BIT_MASK) | (SIGNIF_BIT_MASK + 1);
    ///     if (bits < 0) r = -r;
    ///     return ((r >> shift) + 1) >> 1;
    /// }
    /// return (long) a;                                         // (int) a
    /// ```
    ///
    /// `(shift & -64) == 0` is the UNSIGNED `shift <= 63` (a negative shift
    /// reads as huge), the shifts are arithmetic, and the fallback is the
    /// `d2l` / `f2i` lowering with its NaN -> 0 / overflow -> MAX|MIN fix-up
    /// ([`Self::emit_fp_to_int_fixup`]): `|a| >= 2^52` (already an integer,
    /// infinities, NaN) and `|a|` below a half ulp of one (zero, subnormals).
    pub(in crate::ir_lower) fn emit_math_round_scalar(&mut self, dbl: bool) {
        let rel = |target: usize, patch: usize| (target as i64) - (patch as i64) - 1;
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
            self.buf.emit(&[0x48, 0xC1, 0xE9, 0x0C]); // SHR RCX, 12 (the significand)
            self.buf.emit(&[0x48, 0x0F, 0xBA, 0xE9, 0x34]); // BTS RCX, 52 (the hidden bit)
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
        // .slow: `(long) a` / `(int) a`.
        let slow = self.buf.pos();
        self.buf.patch_rel8_or_bail(ja_slow, rel(slow, ja_slow));
        if dbl {
            self.buf.emit(&[0xF2, 0x48, 0x0F, 0x2C, 0xC0]); // CVTTSD2SI RAX, XMM0
        } else {
            self.buf.emit(&[0xF3, 0x0F, 0x2C, 0xC0]); // CVTTSS2SI EAX, XMM0
        }
        self.emit_fp_to_int_fixup(dbl, dbl);
        // .done
        let done = self.buf.pos();
        self.buf.patch_rel8_or_bail(jmp_done, rel(done, jmp_done));
        if !dbl {
            self.buf.emit(&[0x48, 0x63, 0xC0]); // MOVSXD RAX, EAX
        }
    }

    // ── Node lowering ────────────────────────────────────────────────

    /// JIT round 21 wave 22 (lane cf): is the volatile `Op::Store` `node_id`
    /// one whose trailing StoreLoad fence `lower_block` skips? The set is
    /// computed once per compile, at the first volatile store, from the
    /// finished schedule (`CRATONVM_JIT_IR_FRESH_VOLATILE_STORE_UNFENCED`,
    /// read there; off answers `false` for every store).
    pub(in crate::ir_lower) fn volatile_store_fence_elided(&mut self, node_id: NodeId) -> bool {
        let elided = self.volatile_store_owes_no_fence(node_id);
        if elided && crate::x64::dbg_jitc_enabled() {
            let pc = self
                .graph
                .nodes
                .get(node_id as usize)
                .and_then(|n| n.bytecode_pc)
                .map_or(-1, |pc| pc as i64);
            eprintln!(
                "[cratonvm-jitc] ir-fresh-volatile-store-unfenced {} pc={pc} n{node_id}",
                self.graph.scope_method_key,
            );
        }
        elided
    }

    /// [`Self::volatile_store_fence_elided`] without its debug line, for the
    /// fence-sink plan, which asks about every store before any is lowered.
    pub(in crate::ir_lower) fn volatile_store_owes_no_fence(&mut self, node_id: NodeId) -> bool {
        if self.unfenced_fresh_stores.is_none() {
            // Wave 24 (lane flipprep): not while a debugger or a JVMTI agent
            // may read this compile's locals (`CompileLocalsObservedScope`).
            let set = if ir_fresh_volatile_store_unfenced_enabled()
                && !CompileLocalsObservedScope::current()
            {
                let (graph, schedule) = (self.graph, self.schedule);
                let views: Vec<FenceBlockView<'_>> = schedule
                    .blocks
                    .iter()
                    .map(|b| FenceBlockView {
                        nodes: &b.nodes,
                        terminator: b.terminator,
                        successors: &b.successors,
                    })
                    .collect();
                fresh_allocation_unfenced_volatile_stores(graph, &views, &schedule.node_to_block)
            } else {
                std::collections::HashSet::new()
            };
            self.unfenced_fresh_stores = Some(set);
        }
        // JIT round 23 wave 9 (lane W9-L2, CF-W9-3): a release / opaque store
        // (`Graph::release_stores`) is ordered like a volatile one by every
        // pass but owes no StoreLoad fence: on x86 TSO the plain store IS the
        // release store.
        self.graph.release_stores.contains(&node_id)
            || self
                .unfenced_fresh_stores
                .as_ref()
                .is_some_and(|s| s.contains(&node_id))
    }

    /// Does node `id` owe a volatile store's StoreLoad fence once it has run?
    /// A volatile `Op::Store` whose fence no rule elides, or a volatile
    /// `Op::StoreStatic` (which no rule elides).
    pub(in crate::ir_lower) fn node_owes_store_fence(&mut self, id: NodeId) -> bool {
        let Some(node) = self.graph.nodes.get(id as usize) else {
            return false;
        };
        match node.op {
            Op::StoreStatic { is_volatile, .. } => is_volatile,
            _ => node.is_volatile_store() && !self.volatile_store_owes_no_fence(id),
        }
    }

    /// JIT round 23 close (`CRATONVM_JIT_IR_VOLATILE_FENCE_SINK`): build the
    /// per-block "fence may be owed on entry" table, once per compile.
    ///
    /// The JMM owes a volatile store a StoreLoad edge to a LATER volatile load
    /// (or lock, or anything this frame cannot see: a call, an exit). On x86
    /// TSO nothing else can pass the store, so the fence need not stand right
    /// after it: it can stand before the next node that could read under a
    /// volatile load's ordering, and a run of volatile stores with only
    /// arithmetic, plain statics and branches between them pays ONE fence --
    /// for a store-only loop, the one on its exit path (HotSpot's C2 merges the
    /// adjacent trailing `MemBarVolatile`s of its unrolled stores the same way;
    /// this also crosses the back edge).
    ///
    /// The table is a forward may-analysis over the schedule's predecessor
    /// edges: a block's entry is owed a fence when any predecessor's exit is.
    /// Inside a block, an owing store sets the state, a node that is not
    /// [`fence_sink_transparent`] clears it (the lowering fences before that
    /// node), and a terminator other than a branch clears it (fence before a
    /// return or a throw). An entry with no predecessor -- method entry, an OSR
    /// door -- is owed nothing: the interpreter or the caller already fenced.
    /// A `true` that is not really owed costs one extra fence, never a missing
    /// one, so the iteration cap falls back to "owed everywhere".
    pub(in crate::ir_lower) fn ensure_fence_sink_plan(&mut self) {
        if self.fence_sink_in.is_some() {
            return;
        }
        if !ir_volatile_fence_sink_enabled() {
            self.fence_sink_in = Some(Vec::new());
            return;
        }
        let schedule = self.schedule;
        let n = schedule.blocks.len();
        // Which nodes owe a fence, asked once (the elision sets are lazy).
        let mut owes: std::collections::HashSet<NodeId> = std::collections::HashSet::new();
        for block in &schedule.blocks {
            for &id in &block.nodes {
                if self.node_owes_store_fence(id) {
                    owes.insert(id);
                }
            }
        }
        if owes.is_empty() {
            self.fence_sink_in = Some(Vec::new());
            return;
        }
        let graph = self.graph;
        let transfer = |b: usize, mut owed: bool| -> bool {
            let block = &schedule.blocks[b];
            for &id in &block.nodes {
                if owes.contains(&id) {
                    owed = true;
                } else if !graph
                    .nodes
                    .get(id as usize)
                    .is_some_and(|node| fence_sink_transparent(&node.op))
                {
                    owed = false;
                }
            }
            if let Some(t) = block.terminator {
                if !graph
                    .nodes
                    .get(t as usize)
                    .is_some_and(|node| matches!(node.op, Op::If | Op::Switch { .. }))
                {
                    owed = false;
                }
            }
            owed
        };
        let mut entry = vec![false; n];
        let mut exit = vec![false; n];
        let mut converged = false;
        for _ in 0..(4 * n + 8) {
            let mut changed = false;
            for b in 0..n {
                let preds = &schedule.blocks[b].predecessors;
                let owed_in = !preds.is_empty()
                    && preds.iter().all(|&p| exit.get(p).copied().unwrap_or(false));
                let owed_out = transfer(b, owed_in);
                let owed_out = transfer(b, owed_in);
                if owed_in != entry[b] || owed_out != exit[b] {
                    entry[b] = owed_in;
                    exit[b] = owed_out;
                    changed = true;
                }
            }
            if !changed {
                converged = true;
                break;
            }
        }
        if !converged {
            entry = vec![false; n];
        }
        if crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] ir-volatile-fence-sink {}: {} store(s) defer, {} of {} block(s) entered owing",
                self.graph.scope_method_key,
                owes.len(),
                entry.iter().filter(|&&o| o).count(),
                n,
            );
        }
        self.fence_sink_deferred_any = true;
        self.fence_sink_in = Some(entry);
    }

    /// Is the fence sink planned for this compile (and so are stores deferred)?
    pub(in crate::ir_lower) fn fence_sink_on(&mut self) -> bool {
        self.ensure_fence_sink_plan();
        self.fence_sink_deferred_any
    }

    fn block_accepts_owed_fence(&self, b: usize) -> bool {
        self.fence_sink_in
            .as_ref()
            .and_then(|v| v.get(b).copied())
            .unwrap_or(false)
    }

    fn all_successors_accept_owed_fence(&self, block_idx: usize) -> bool {
        let succs = &self.schedule.blocks[block_idx].successors;
        !succs.is_empty() && succs.iter().all(|&s| self.block_accepts_owed_fence(s))
    }

    /// The volatile store just lowered owes its StoreLoad fence: emit it here,
    /// or, under the sink, leave it owed for the next node that needs it.
    pub(in crate::ir_lower) fn emit_or_defer_store_fence(&mut self) {
        if self.fence_sink_on() {
            self.fence_pending = true;
        } else {
            self.buf.emit(&[0xF0, 0x83, 0x04, 0x24, 0x00]); // LOCK ADD DWORD [RSP], 0
        }
    }

    /// Before a CALL inside a node the sink treats as transparent (the helper
    /// routes of `getstatic` / `putstatic`): pay an owed fence on that route
    /// only. The state is left alone -- the inline route still owes it.
    pub(in crate::ir_lower) fn emit_owed_fence_before_helper_call(&mut self) {
        if self.fence_pending {
            self.buf.emit(&[0xF0, 0x83, 0x04, 0x24, 0x00]); // LOCK ADD DWORD [RSP], 0
        }
    }

    /// Pay an owed fence now (before a node or terminator the sink does not
    /// see through).
    pub(in crate::ir_lower) fn settle_owed_fence(&mut self) {
        if self.fence_pending {
            self.buf.emit(&[0xF0, 0x83, 0x04, 0x24, 0x00]); // LOCK ADD DWORD [RSP], 0
            self.fence_pending = false;
        }
    }

    pub(in crate::ir_lower) fn lower_block(&mut self, block_idx: usize) {
        let location_trace = crate::x64::dbg_jitc_enabled() && false;
        let block_trace_start = self.buf.pos();
        self.gp_zx32_mask = 0;
        // A carry never spans a block — `carry_of` is planned between adjacent
        // nodes of one block's list — so one left here is a planning bug, and
        // the block boundary is a branch target besides.
        self.refuse_carry("a block began while it was still in flight");
        // Round 25 (item 1b): a block start is a branch target, and no
        // caller-saved value is live across one (`plan_caller_saved_residency`).
        self.invalidate_caller_saved_residency();
        self.block_offsets[block_idx] = self.buf.pos();
        // A block start is a branch target: RAX is nothing here.
        self.rax_home_mirror = None;
        self.rax_reg_mirror = None;
        // The fence sink's state on entry is the plan's, whatever the block
        // emitted before this one left (`ensure_fence_sink_plan`).
        self.ensure_fence_sink_plan();
        self.fence_pending = self
            .fence_sink_in
            .as_ref()
            .and_then(|v| v.get(block_idx).copied())
            .unwrap_or(false);
        // A receiver proof is block-local: control can enter this block from a
        // predecessor that never proved it.
        self.guarded_receivers.clear();
        self.null_proven_receivers.clear();
        self.unbox_class_proven.clear();
        self.checkcast_null_or_exact_proven.clear();
        self.block_cse
            .set(crate::ir_lower::block_cse::BlockCseState::default());
        self.scoped_screened_bases.clear();
        self.block_inline_field_sites = 0;
        self.seed_block_null_proofs();
        // j27 wave 4 (lane MONROWS, MON17W2-2): a coarsened field monitor's
        // base, below the copy-0 read that proved it (empty unless
        // `CRATONVM_JIT_IR_LOOP_LOCK_COARSEN_DOMINATED_FIELD_BASE` recorded one).
        for base in crate::ir_optimize::monitor_rows::dominated_monitor_bases(
            self.graph,
            block_idx,
            |id| self.schedule.node_to_block.get(id as usize).copied(),
            |a, b| self.schedule.dom.dominates(a, b),
        ) {
            self.note_receiver_non_null(base);
        }
        // JIT round 25 item 1a (wave 3): the block an inspection spill, and a
        // site's question about one, is in.
        self.cur_lower_block = block_idx;

        let block = &self.schedule.blocks[block_idx];

        self.emit_preheader_layout_epoch_guard(block_idx);

        // Emit data nodes.
        //
        // The level-2 detour is a no-op unless a machine-level flag is on; see
        // `lower_data_node_through_mir`, which falls through to the per-opcode
        // arm for every node no tile covers.
        // Hoisted out of the loop deliberately. `ir_ref_residency_enabled` is an
        // uncached `runtime_var` read, and asking it once per NODE is how
        // `CRATONVM_DBG_COMPACT_INLINE` came to be 99.1% of all flag reads on a
        // BigDecimal run -- a cost paid in the DEFAULT configuration, where the
        // answer is always `false`, for a feature that is off.
        let ref_residency = self.ref_residency_on;
        // Round 9: "is this reference still readable from its register" is
        // tracked in EMISSION order, but control can enter this block from a
        // predecessor other than the one emitted just before it — a loop
        // header's back edge, whose poll or call ran AFTER the header's reads
        // were emitted, or a merge whose other arm called out. The register
        // then holds a pre-move address while the collector updated only the
        // home word. Unless the sole runtime predecessor is the block emitted
        // immediately before (whose end state IS this model's state), forget
        // every cached reference; the next read reloads the home word.
        if ref_residency {
            let preds = &self.schedule.blocks[block_idx].predecessors;
            // nio21 wave 3 L2: a block a call-site exception pad jumps into is
            // entered from inside its call, never straight-line.
            let straight_line = block_idx > 0
                && preds.len() == 1
                && preds[0] == block_idx - 1
                && !self.call_throw_entry_blocks.contains(&block_idx);
            if !straight_line && !self.is_call_free_gc_free_loop_header(block_idx) {
                self.invalidate_ref_residency(None);
            }
        }
        // The linear position this block starts at, when a split plan is
        // installed. `None` in every default build, and the `if let` below is
        // then the whole cost of the feature on the emission path.
        //
        // Reproduced from the schedule rather than carried from the allocator:
        // `plan_register_residency` has already refused the method unless
        // `build_live_model`'s total position count matches the one this same
        // formula produces, so the two numberings are checked to agree before
        // either is used.
        let ls_start = self.ls_block_start.get(block_idx).copied();
        for (offset, &node_id) in block.nodes.iter().enumerate() {
            // An owed volatile-store fence is paid before the first node the
            // sink cannot see through (`fence_sink_transparent`), ahead of
            // every byte that node emits. A fused compare emits nothing at its
            // own position, so this never lands between a `CMP` and its `Jcc`.
            if self.fence_pending
                && !self
                    .graph
                    .nodes
                    .get(node_id as usize)
                    .is_some_and(|n| fence_sink_transparent(&n.op))
            {
                self.settle_owed_fence();
            }
            // BEFORE the node, because a `Load` / `Remat` / `Move` event's
            // position is the position at which the value must ALREADY be in
            // its new register — and because being inside this loop is what
            // puts every new publish under the invalidation audit below.
            if let Some(start) = ls_start {
                self.ls_apply_transitions(start + offset);
            }
            // gen r5w2/oomjit6: armed for exactly this node's lowering, and
            // the stale argument staging cleared before its first byte.
            self.dead_ref_clear_site = self.dead_ref_clear_site_for(node_id);
            // gen r5w3/live7: an upper bound on the staging words this call
            // writes, for the store-free claim (`precise_liveness_claim`).
            // gci i4/ir: a non-call, non-allocation op armed under
            // `CRATONVM_JIT_IR_OTHER_GC_POINT_CLEARS` claims no staging word
            // dead (`usize::MAX`): its slow path's staging is not modelled.
            self.dead_ref_clear_site_args =
                self.graph
                    .nodes
                    .get(node_id as usize)
                    .map_or(usize::MAX, |n| {
                        if matches!(n.op, Op::Call { .. } | Op::New { .. } | Op::NewArray { .. }) {
                            n.inputs.len().saturating_sub(2)
                        } else {
                            usize::MAX
                        }
                    });
            // The stores only under their own opt-in; the store-free claim
            // arms the same site without writing anything here.
            //
            // hga: held, like the planned clears below, by a call that may
            // take an inline prefix: a prefix hit stages nothing and reaches no
            // GC point, and the next call's own dense clear zeroes whatever an
            // earlier call left before that call's GC point.
            //
            // JIT round 20 wave 17, lane gcDead (I-9): an armed allocation
            // holds them the same way, for its slow path
            // (`Op::New` / `Op::NewArray` arms; `disarm_alloc_site`).
            let prefixed =
                self.call_site_may_take_inline_prefix(node_id) || self.alloc_site_armed(node_id);
            if self.dead_ref_clear_site.is_some() && self.dead_ref_clear_stores {
                if prefixed {
                    self.pending_arg_stage_clears = true;
                } else {
                    self.emit_dead_arg_stage_clears();
                }
            }
            // gen r5w5/oomjit9 (default on): the planned dead-home and
            // stale-staging clears, at the node's FIRST byte -- ahead of an
            // inline prefix whose fast path skips the call's own lowering
            // (and with it every store `emit_safepoint_map` would make). Plain
            // `MOV qword [rbp - off], 0`s: no register, no flag, so a carry
            // into this node is undisturbed. Nothing between here and the
            // call's GC point writes a dead home or reads a staging word it
            // did not write first (`plan_dead_home_clears`).
            //
            // hga: a call that may take an inline prefix holds them until its
            // prefixes are emitted (`emit_pending_call_clears`), so a prefix
            // hit -- the cached `Integer.valueOf` of a `HashMap.get` loop --
            // does not pay a dozen stores per iteration for the call it skips.
            if let Some(offs) = self.dead_home_clears.get(&node_id).cloned() {
                if prefixed {
                    self.pending_call_clears = Some(offs);
                } else {
                    self.emit_dead_home_clear_batch(&offs);
                }
            }
            // nio21 wave 3 L2: an armed call carries its plan through its own
            // lowering only (`push_call_exc_patch` reads it).
            if !self.call_throw_plans.is_empty() {
                self.cur_call_throw = self.call_throw_plans.get(&node_id).cloned();
            }
            self.call_throw_publication = None;
            // JIT round 19 wave 5 (lane handler5, SPLICE4-1): armed for exactly
            // this call's lowering (its exceptional exits ask the probe). The
            // builder never gives one call both (`plant_call_handler_entry`).
            self.cur_catch_arm = self.catch_arm_wire(block_idx, offset, node_id);
            // Round 25 (item 1b): a node that may write a caller-saved
            // register (a call, a poll, a helper or guard sequence) ends every
            // caller-saved residency BEFORE it is lowered; see
            // `invalidate_caller_saved_residency`. No caller-saved value is
            // read by such a node (`plan_caller_saved_residency`).
            if self.ls_caller_saved_on
                && !self
                    .graph
                    .nodes
                    .get(node_id as usize)
                    .is_some_and(ir_op_caller_saved_transparent)
            {
                self.invalidate_caller_saved_residency();
            }
            // JIT round 25 item 1a (wave 3): the inspection spills this node
            // owes, ahead of its own first byte (plain register-to-frame
            // stores: no flag, no RAX, no RCX).
            self.emit_inspection_spills(node_id, false, block_idx);
            // JIT round 25 item 2, wave 4: the lazy references this node's
            // helper calls spill and reload (resident now, or refused).
            let lazy_keep = self.lazy_ref_keep_before(node_id);
            let lazy_start = self.buf.pos();
            self.lower_data_node_through_mir(block_idx, node_id);
            // Round 25 wave 2: the planner's reloads after this node (its own
            // opaque definition, or a split run starting after it).
            if self.ls_caller_saved_on {
                self.republish_caller_saved_after(node_id);
            }
            self.cur_catch_arm = None;
            self.cur_call_throw = None;
            self.call_throw_publication = None;
            // Every `Op::Call` reaches its arm's prefix point; one that did
            // not would have run its call with the planned words unzeroed.
            if self.pending_call_clears.take().is_some()
                || std::mem::take(&mut self.pending_arg_stage_clears)
            {
                self.latch_bailout(Bailout::new(BailoutReason::Internal(
                    "ir_lower: a call's held dead-home clears were never emitted",
                )));
            }
            self.dead_ref_clear_site = None;
            // Round 9 wave 6 (irsnap6, perf-ir-refuses-volatile-instance-field-methods):
            // a VOLATILE `putfield` owes the JMM's StoreLoad fence after the
            // store. Every route of the `Op::Store` arm (inline int, compact
            // helper, wide helper, gated and helper reference) and the MIR tile
            // path return here. The single-pass twin is `x64/op_field.rs`
            // `walk_field`.
            //
            // Round 11 wave 2 (lane lower, `r11-x64gen-volatile-store-uses-
            // mfence`): `LOCK ADD DWORD [RSP], 0`, not `MFENCE`. On x86-TSO the
            // only barrier a volatile store owes is StoreLoad, which any
            // `LOCK`-prefixed read-modify-write provides; `MFENCE` also orders
            // weakly-ordered (non-temporal / WC) accesses this code never makes
            // and is markedly slower on Intel cores. `[RSP]` is this frame's own
            // (always mapped, thread-private) bottom word, and adding 0 leaves
            // it unchanged. It writes no register, so no carry or deferred-RCX
            // state is disturbed; it DOES write the flags, which is sound here
            // because no node's lowering leaves flags live for the next one
            // (every consumer of a compare emits its own `CMP`/`TEST`).
            //
            // JIT round 21 wave 22 (lane cf): a store into an allocation no
            // other thread can reach yet owes no StoreLoad
            // (`CRATONVM_JIT_IR_FRESH_VOLATILE_STORE_UNFENCED`, default ON; see
            // `fresh_allocation_unfenced_volatile_stores`).
            //
            // JIT round 23 close (`CRATONVM_JIT_IR_VOLATILE_FENCE_SINK`): under
            // the sink the fence is owed instead, and paid before the next node
            // that needs it (`emit_or_defer_store_fence`).
            if self.graph.nodes[node_id as usize].is_volatile_store()
                && !self.volatile_store_fence_elided(node_id)
            {
                self.emit_or_defer_store_fence();
            }
            // Did lowering that node let a collector run? `op_cannot_deopt` is
            // the allowlist that answers it — every op on it is pure
            // arithmetic, control, a constant or a parameter, and none of them
            // emits a call. An op NOT on it invalidates, which is the safe
            // direction and the reason the question is asked through an
            // allowlist rather than through `ir_op_is_safepoint` (which is a
            // model of oop-map publication sites, not of calls).
            if ref_residency && !self.ref_node_never_runs_gc(node_id) {
                // tcm21 (TCMISS21-2, first step of its gate): how many cached
                // reference copies does a TYPE CHECK's node-end invalidation
                // take? Debug census only (`CRATONVM_DBG_JITC=1` with
                // `CRATONVM_JIT_IR_REF_RESIDENCY=1`), counted before the drop.
                if crate::x64::dbg_jitc_enabled()
                    && matches!(
                        self.graph.nodes[node_id as usize].op,
                        Op::InstanceOf { .. } | Op::CheckCast { .. }
                    )
                {
                    eprintln!(
                        "[cratonvm-jitc] ref residency: type check n{node_id} drops {} cached reference copies",
                        self.live_ref_copies(Some(node_id))
                    );
                }
                self.invalidate_ref_residency(Some(node_id));
            }
            // Wave 4: a lazy reference survives the node in its register --
            // every helper call the node emitted spilled it and reloaded it --
            // or the attempt is refused naming it.
            if !lazy_keep.is_empty() {
                self.lazy_ref_after_node(node_id, lazy_start, &lazy_keep);
            }
        }

        // Round 25 (item 1b): only an `If` with no back-edge poll reads a
        // caller-saved value; every other terminator (and a fall-through edge,
        // which may poll) is a point past which none survives.
        if self.ls_caller_saved_on && !self.caller_saved_survive_terminator(block_idx) {
            self.invalidate_caller_saved_residency();
        }

        // `CRATONVM_JIT_IR_LOOP_ENTRY_DEAD_HOME_CLEARS`: the homes this loop
        // entry block zeroes for the loop it enters, after its last node and
        // before its edge, preserving GPRs and flags. XMM0 is scratch here,
        // before any terminator operands are staged.
        if let Some(offs) = self.loop_entry_dead_home_clears.get(&block_idx).cloned() {
            self.emit_dead_home_clear_batch(&offs);
        }

        // Emit terminator and edge copies; this range excludes data nodes.
        let edge_trace_start = self.buf.pos();
        if let Some(term) = block.terminator {
            // A return, a throw or any other exit settles an owed fence; a
            // branch carries it only when every successor accepts it.
            if self.fence_pending
                && (!self
                    .graph
                    .nodes
                    .get(term as usize)
                    .is_some_and(|n| matches!(n.op, Op::If | Op::Switch { .. }))
                    || !self.all_successors_accept_owed_fence(block_idx))
            {
                self.settle_owed_fence();
            }
            self.lower_terminator(term, block_idx);
        } else {
            // No explicit terminator: this is a goto / fall-through edge into
            // a Merge/Region. BUG FIX [jit-irlower #2]: emit the edge's phi
            // copies before transferring control, then transfer to the
            // successor — as a real fall-through when the layout already put it
            // physically next, and otherwise as an explicit `JMP`.
            let succ = self.schedule.blocks[block_idx].successors.first().copied();
            if let Some(succ_block) = succ {
                if self.fence_pending && !self.block_accepts_owed_fence(succ_block) {
                    self.settle_owed_fence();
                }
                if succ_block <= block_idx {
                    // Unconditional (a `goto`, or a branch's taken edge in its
                    // own `Proj` block): the header is next on every path
                    // through this poll, so it may leave there (wave 15, L2).
                    //
                    // JIT round 25 item 1a (item-2 review): this is the one
                    // back-edge poll `lower_terminator` does not emit. A frame
                    // parked here is read at `cur_bci` (the poll's map), which
                    // is no terminator's pc and so no inspection bci of the
                    // plan: spill what is named there, or refuse it.
                    self.emit_fallthrough_poll_spills(block_idx);
                    self.emit_safepoint_poll(Some(block_idx), Some(succ_block));
                }
                self.emit_phi_copies(block_idx, succ_block);
                // MX16-5: a loop's entry edge may carry a vector pre-loop.
                self.emit_vector_preloop(block_idx, succ_block);
                self.emit_jmp_to_block_or_fall_through(succ_block, block_idx);
            } else if block_idx == 0 || !block.predecessors.is_empty() {
                // Round 12 wave 2 (lane irback, proposal R12-4): a block with
                // no terminator is a goto and must name where it goes. One
                // that names nowhere emitted no transfer at all and ran on
                // into whichever block the layout put next (or off the end of
                // the body). That is the shape a lost back edge takes when the
                // latch is a `goto` -- the header `Merge` keeps one input,
                // its φs agree with it, and every verifier lane passes -- and
                // it is what the self-loop edge `build_block_cfg` once
                // dropped produced. Refused, not guessed at: a coverage loss,
                // never a wrong answer. A block nothing enters is left alone.
                self.latch_bailout(Bailout::with_context(
                    BailoutReason::Internal(
                        "ir_lower: a reachable block with no terminator and no successor",
                    ),
                    format!("block {block_idx} (head n{})", block.ctrl),
                ));
            }
        }
        if location_trace {
            let op = block
                .terminator
                .and_then(|id| self.graph.nodes.get(id as usize))
                .map(|node| format!("{:?}", node.op))
                .unwrap_or_else(|| "fallthrough".to_string());
            eprintln!(
                "[ir-location-edge] base={:p} method={} block={} term={:?} op={} bytes={}..{}",
                self.buf.as_ptr(),
                self.graph.scope_method_key,
                block_idx,
                block.terminator,
                op,
                edge_trace_start,
                self.buf.pos(),
            );
            eprintln!(
                "[ir-location-block] base={:p} method={} block={} bytes={}..{}",
                self.buf.as_ptr(),
                self.graph.scope_method_key,
                block_idx,
                block_trace_start,
                self.buf.pos(),
            );
        }
        // Round 25 (item 1b): the edge's phi copies and the code emitted after
        // the last block (stubs, pads) may write any caller-saved register.
        self.invalidate_caller_saved_residency();

        // JIT round 21, lane tcfuse: a fused `instanceof`'s inline answers are
        // resolved by the `If` ending this block, or the compile is refused (a
        // coverage loss, never a jump to nowhere).
        if self.typecheck_branch_pending.take().is_some() {
            self.latch_bailout(Bailout::new(BailoutReason::Internal(
                "ir_lower: a fused instanceof's answers reached no If edge",
            )));
        }
    }

    /// Round 12 wave 8 (lane callcost,
    /// `r12w8-callcost-dominated-self-call-samples-FIXED-20260927.md`): mark every
    /// direct self-call whose stack sample another direct self-call already
    /// answered on every path to it, so [`Self::emit_self_recursive_call`]
    /// emits no sample there.
    ///
    /// The sample is `MOVZX EAX, SP; CMP EAX, span; JA` plus the guard helper
    /// on its slow side, and it is a pure function of RSP: `span` is this
    /// body's constant, and the helper answers `sp > floor` against a floor
    /// fixed per thread (`jit_self_call_stack_guard`). RSP is the same at every
    /// self-call of one activation -- the prologue's `SUB RSP` is the only
    /// adjustment that outlives a node, and a self-call reserves no stack
    /// arguments. So when site `S1` dominates site `S2`, `S2`'s sample repeats
    /// the answer `S1` got in the same activation: pass (or a helper that
    /// returned 0) at `S1` is pass at `S2`, and a trip at `S1` left the frame
    /// through the call-exception stub, so `S2` is never reached. Within one
    /// block the earlier node dominates; across blocks the schedule's
    /// dominator relation answers (a site in an unreachable block is never
    /// executed, so eliding it is moot). The IR builds no handler blocks, so
    /// nothing re-enters the body between the two sites except the callee
    /// deopt service's resume at `S1`'s own `.keep`.
    ///
    /// Planned only for a loop-free block CFG, where no OSR entry can land
    /// past `S1`; `lower_inner_attempt` also refuses the compile if an OSR
    /// entry was emitted anyway, or if a planned site was not lowered through
    /// the self-call route (its sample would then be missing too). Kill
    /// switch: `CRATONVM_JIT_IR_SELF_CALL_SAMPLE_DOMINATED=0`, read per
    /// compile.
    pub(in crate::ir_lower) fn plan_self_call_sample_elision(&mut self) {
        if self.mir_mode != MirMode::Off || schedule_block_cfg_has_cycle(self.schedule) {
            return;
        }
        let mut sites: Vec<(usize, usize, NodeId)> = Vec::new();
        for (b, block) in self.schedule.blocks.iter().enumerate() {
            for (i, &n) in block.nodes.iter().enumerate() {
                if self
                    .graph
                    .nodes
                    .get(n as usize)
                    .is_some_and(Self::is_direct_self_call)
                {
                    sites.push((b, i, n));
                }
            }
        }
        if sites.len() < 2 {
            return;
        }
        let schedule: &Schedule = self.schedule;
        let mut dominated = vec![false; self.graph.nodes.len()];
        let mut any = false;
        for &(b2, i2, n2) in &sites {
            let covered = sites.iter().any(|&(b1, i1, n1)| {
                if n1 == n2 {
                    return false;
                }
                if b1 == b2 {
                    i1 < i2
                } else {
                    schedule.dom.is_reachable(b1) && schedule.dom.dominates(b1, b2)
                }
            });
            if covered {
                if let Some(cell) = dominated.get_mut(n2 as usize) {
                    *cell = true;
                    any = true;
                }
            }
        }
        if any {
            self.self_call_sample_dominated = dominated;
            self.self_call_sites_planned = sites.len();
        }
    }

    /// The incoming ABI register of `id` when it is an `int` parameter the
    /// prologue receives in a register (the same mapping
    /// [`Self::emit_prologue`] stores from: `ENTRY_ABI_REGS[base + idx]`).
    pub(in crate::ir_lower) fn entry_fast_int_param_reg(&self, id: NodeId) -> Option<u8> {
        let node = self.graph.nodes.get(id as usize)?;
        let Op::Param(idx) = node.op else {
            return None;
        };
        if node.ty != IrType::Int {
            return None;
        }
        let idx = usize::from(idx);
        let base = usize::from(self.needs_context);
        let reg_count = self
            .num_params
            .min(ENTRY_ABI_REGS.len().saturating_sub(base));
        if idx >= reg_count {
            return None;
        }
        ENTRY_ABI_REGS.get(base + idx).copied()
    }

    /// `id`'s value when it is an `int` constant that fits an `imm32`.
    pub(in crate::ir_lower) fn entry_fast_int_const(&self, id: NodeId) -> Option<i64> {
        let node = self.graph.nodes.get(id as usize)?;
        let Op::Const(k) = node.op else {
            return None;
        };
        if node.ty != IrType::Int || i32::try_from(k).is_err() {
            return None;
        }
        Some(k)
    }

    /// What block `block_idx` returns, when it is exactly "`return` an `int`
    /// parameter or constant" entered only from block 0: its one predecessor
    /// is the entry block, it has no successor, every data node in it is
    /// skippable, and its terminator is an `Op::Return` of such a value.
    ///
    /// The scheduler splits the entry `If`'s edges, so the return block is
    /// usually reached through an empty pass-through block (no nodes, no
    /// terminator, one successor); up to four such blocks are followed, each
    /// entered only from the block before it.
    ///
    /// Round 16 (lane codegen, CG16-1): the return block may also be a JOIN
    /// that ends the method -- javac's `return n < 2 ? n : f(n - 1) + f(n - 2);`
    /// meets its two arms at one `ireturn` of a phi. The path to it from block
    /// 0 is still pass-throughs only (at least one), and what it returns is
    /// read on THIS edge ([`Self::entry_fast_join_value`]). Every other node of
    /// the join is skipped under the same purity rule. Kill switch:
    /// `CRATONVM_JIT_IR_ENTRY_FAST_RETURN_PHI=0`.
    pub(in crate::ir_lower) fn entry_fast_return_value(
        &self,
        block_idx: usize,
    ) -> Option<EntryFastValue> {
        let mut pred = 0usize;
        let mut cur = block_idx;
        let mut hops = 0;
        let mut join_pred: Option<usize> = None;
        let block = loop {
            let block = self.schedule.blocks.get(cur)?;
            if block.predecessors.len() != 1 || block.predecessors.first() != Some(&pred) {
                // A join that returns, entered from `pred` (among others)
                // through at least one pass-through of this edge alone.
                if hops == 0
                    || !block.successors.is_empty()
                    || !block.predecessors.contains(&pred)
                    || !ir_entry_fast_return_phi_enabled()
                {
                    return None;
                }
                join_pred = Some(pred);
                break block;
            }
            if block.successors.is_empty() {
                break block;
            }
            if hops >= 4
                || block.terminator.is_some()
                || !block.nodes.is_empty()
                || block.successors.len() != 1
            {
                return None;
            }
            hops += 1;
            pred = cur;
            cur = *block.successors.first()?;
        };
        let term = self.graph.nodes.get(block.terminator? as usize)?;
        if term.op != Op::Return || term.inputs.len() != 2 {
            return None;
        }
        let mut val = *term.inputs.get(1)?;
        if val == NO_NODE {
            return None;
        }
        let skippable = block.nodes.iter().all(|&n| {
            self.graph
                .nodes
                .get(n as usize)
                .is_some_and(|x| entry_fast_return_may_skip(&x.op))
        });
        if !skippable {
            return None;
        }
        if let Some(p) = join_pred {
            val = self.entry_fast_join_value(cur, p, val)?;
        }
        if let Some(reg) = self.entry_fast_int_param_reg(val) {
            return Some(EntryFastValue::Reg(reg));
        }
        self.entry_fast_int_const(val).map(EntryFastValue::Imm)
    }

    /// The value the join block `join` returns when it is entered from
    /// `pred` (round 16, lane codegen, CG16-1): `val` itself when it is not a
    /// phi of the join's merge (a value defined above the join is the same on
    /// every edge), else the phi's input on the merge input `pred` feeds --
    /// the same edge-to-input mapping [`Self::gather_phi_copies`] copies from
    /// and [`Self::entry_fold_phi_edge`] evaluates. `None` when that edge
    /// feeds no merge input or more than one, or `val` is a phi of another
    /// merge.
    pub(in crate::ir_lower) fn entry_fast_join_value(
        &self,
        join: usize,
        pred: usize,
        val: NodeId,
    ) -> Option<NodeId> {
        let merge_ctrl = self.schedule.blocks.get(join)?.ctrl;
        let merge = self.graph.nodes.get(merge_ctrl as usize)?;
        if !matches!(merge.op, Op::Merge | Op::Region) {
            return None;
        }
        let node = self.graph.nodes.get(val as usize)?;
        if node.op != Op::Phi {
            return Some(val);
        }
        if node.inputs.first() != Some(&merge_ctrl) {
            return None;
        }
        let mut from_pred = merge
            .inputs
            .iter()
            .enumerate()
            .filter(|&(_, &ctrl_in)| self.block_of_ctrl(ctrl_in) == Some(pred))
            .map(|(k, _)| k);
        let k = from_pred.next()?;
        if from_pred.next().is_some() {
            return None;
        }
        let src = *node.inputs.get(k + 1)?;
        (src != NO_NODE).then_some(src)
    }

    /// Round 12 wave 8 (lane callcost,
    /// `r12w8-callcost-entry-fast-return-FIXED-20260927.md`): the shape
    /// [`Self::emit_entry_fast_return`] recognises, or `None`.
    ///
    /// Block 0 holds only skippable data nodes and ends in an `If` on
    /// `Cmp(cc, p, k)` (either operand order), `p` an `int` parameter that
    /// arrives in a register and `k` an `int` constant; one of the two edges
    /// is a block that only returns an `int` parameter (in a register) or an
    /// `int` constant. Every node the fast path skips is on the pure allowlist,
    /// so skipping it is unobservable; the compare it keeps is the same signed
    /// 32-bit `CMP` the `If` arm emits for two `int`s
    /// (`emit_fused_int_compare`), taken on the same edge (`successors[0]` is
    /// the true edge there and here).
    pub(in crate::ir_lower) fn plan_entry_fast_return(&self) -> Option<EntryFastReturn> {
        if self.poison_int_high_on || self.mir_mode != MirMode::Off {
            return None;
        }
        let entry = self.schedule.blocks.first()?;
        let term = self.graph.nodes.get(entry.terminator? as usize)?;
        if term.op != Op::If || entry.successors.len() != 2 {
            return None;
        }
        let cond = self.graph.nodes.get(*term.inputs.get(1)? as usize)?;
        let Op::Cmp(cc) = cond.op else {
            return None;
        };
        let a = *cond.inputs.first()?;
        let b = *cond.inputs.get(1)?;
        let (cmp_reg, k, cc, param) = match (
            self.entry_fast_int_param_reg(a),
            self.entry_fast_int_const(b),
        ) {
            (Some(reg), Some(k)) => (reg, k, cc, a),
            _ => match (
                self.entry_fast_int_const(a),
                self.entry_fast_int_param_reg(b),
            ) {
                (Some(k), Some(reg)) => (reg, k, cc.swapped(), b),
                _ => return None,
            },
        };
        let imm = i32::try_from(k).ok()?;
        let skippable = entry.nodes.iter().all(|&n| {
            self.graph
                .nodes
                .get(n as usize)
                .is_some_and(|x| entry_fast_return_may_skip(&x.op))
        });
        if !skippable {
            return None;
        }
        let true_block = *entry.successors.first()?;
        let false_block = *entry.successors.get(1)?;
        if true_block == false_block {
            return None;
        }
        let (value, ret_cc) = match self.entry_fast_return_value(true_block) {
            Some(v) => (v, cc),
            None => (self.entry_fast_return_value(false_block)?, cc.negate()),
        };
        Some(EntryFastReturn {
            cmp_reg,
            imm,
            ret_cc,
            value,
            param,
        })
    }

    /// Round 12 wave 8 (lane callcost): the entry fast return. Emitted right
    /// after the not-entrant pad, before the prologue:
    ///
    /// ```text
    ///   NOP5                      ; ENTRY_PATCH_PAD (offset 0, unchanged)
    ///   CMP  <p>32, imm           ; the entry block's compare, on the ABI register
    ///   Jcc  .fast_return         ; outlined after the body
    ///   PUSH RBP ; ...            ; the ordinary prologue and body
    ///   ...
    /// .fast_return:
    ///   MOV  RAX, <p> | imm       ; what the return block returns
    ///   RET
    /// ```
    ///
    /// For a base case (`fib`'s `if (n <= 1) return n;`, entered on about half
    /// of all its compiled activations) the call no longer builds a frame,
    /// saves the callee-saved file, stores the context and the parameter,
    /// zeroes the thread and safepoint-id words, publishes the frame record,
    /// polls, and restores it all: four instructions instead of about thirty.
    ///
    /// Why nothing observable is skipped:
    ///
    /// * the skipped nodes are pure ([`entry_fast_return_may_skip`]), and the
    ///   value returned is the full path's own: `Op::Return` loads the
    ///   parameter's prologue word, which is the whole incoming register;
    /// * no instruction on the fast path can reach a safepoint, a helper, a
    ///   trap or a deopt, so no stack walk, collection or frame-state reader
    ///   ever sees an activation without its frame; the frame record keeps
    ///   naming the caller, which is the frame that is really innermost;
    /// * only RAX and the flags are written: the callee-saved file and RSP are
    ///   the caller's, untouched;
    /// * the entry poll is skipped only on a path that makes no call and has
    ///   no loop, so every stop a thread can make is still preceded by a poll
    ///   within a bounded number of instructions;
    /// * offset 0 is still the pad, so a not-entrant patch still intercepts
    ///   every entry, a direct self-call included.
    ///
    /// Kill switch: `CRATONVM_JIT_IR_ENTRY_FAST_RETURN=0`, read per compile.
    /// JIT round 22 wave 12, lane runtime (`CRATONVM_JIT_IR_SWEEP_ENTRY_COUNTER`,
    /// behaviour, default ON, `=0` the old uncounted optimizing body; page
    /// `r22-w7-code-cache-sweep-phases-bistable-FIXED-20261020.md`): the single-pass
    /// tier's two-instruction entry counter (`x64::frames::emit_entry_invocation_counter`,
    /// the same `MOV R10, imm64; ADD qword [R10+0], 1`), in an optimizing body,
    /// right after the not-entrant pad (so every entry counts, a fast return
    /// included), and ONLY while the cold-body sweeper is armed
    /// (`CRATONVM_JIT_CODE_CACHE_SWEEP`, opt-in).
    ///
    /// Why: the sweeper's only usage signal is the entry count
    /// ([`crate::CompiledMethod::entry_count`]), and an optimizing body had no
    /// counter (`crate::entry_counter`'s "C2 bodies" note), so it read `0` at
    /// every sweep and was withdrawn as cold on the second sweep after it
    /// published -- the HOTTEST bodies, while they were being called (w12
    /// measurement: phase-3 bodies published, then demoted to the interpreter,
    /// `compiled bodies per phase now: [0,0,0,0]`). With the sweeper off
    /// nothing reads an optimizing body's count, so nothing is emitted and the
    /// body is byte-identical to before (the layout tests run unarmed).
    ///
    /// R10 at entry is dead under both ABIs (never an argument register) and
    /// is already the single-pass prologue's scratch for this counter.
    pub(in crate::ir_lower) fn emit_sweep_entry_counter(
        &mut self,
    ) -> Option<Box<crate::entry_counter::JitEntryCounter>> {
        if !crate::code_cache_sweep_enabled()
            || !crate::entry_counter::entry_counter_enabled_for_this_compile()
            || !true
        {
            return None;
        }
        let counter = Box::new(crate::entry_counter::JitEntryCounter::new());
        // Cast: a heap address in the imm64 field, bit pattern reinterpreted.
        let addr = counter.count_addr() as u64;
        // MOV R10, imm64 (REX.W + REX.B, B8+r).
        self.buf.emit(&[0x49, 0xBA]);
        self.buf.emit(&addr.to_le_bytes());
        // ADD qword [R10 + disp8], imm8 (REX.W + REX.B, 83 /0, mod=01 rm=010).
        let disp = crate::entry_counter::JitEntryCounter::COUNT_OFFSET as u8; // Cast: the offset is 0 (asserted where it is defined)
        self.buf.emit(&[0x49, 0x83, 0x42, disp, 0x01]);
        Some(counter)
    }

    pub(in crate::ir_lower) fn emit_entry_fast_return(&mut self) {
        // Round 15 wave 1 (lane sync, SS8-2): a self-locking body never
        // returns without taking its monitor (the plan's own purity screen
        // refuses its entry block's `MonitorEnter` too; this says so).
        if self.self_lock.is_some() {
            return;
        }
        let Some(plan) = self.plan_entry_fast_return() else {
            return;
        };
        self.emit_cmp_reg_imm(plan.cmp_reg, plan.imm, false);
        self.buf.emit(&[0x0F, plan.ret_cc.x64_cc()]); // Jcc rel32 .fast_return
        let patch = self.buf.pos();
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
        self.entry_fast_return = Some((patch, plan.value));
        self.emit_entry_fold(&plan);
        // Round 14 wave 6 (lane callcost2, CE-1): the same answers, for the
        // direct self-calls of this body (`emit_self_call_answer`).
        if ir_self_call_answer_enabled() {
            self.self_call_answer = Some(SelfCallAnswer {
                param: plan.param,
                imm: plan.imm,
                ret_cc: plan.ret_cc,
                value: plan.value,
                fold: self
                    .entry_fold
                    .as_ref()
                    .map(|(_, fold)| (fold.lo, fold.values.clone())),
            });
        }
    }

    /// Round 14 wave 5 (lane callcost, proposal CC5-5 of
    /// `jit-r13-callcost5-proposals-RETIRED-20260929.md`): the call-free activations just past
    /// the entry fast return, folded away.
    ///
    /// CC5-5 asked to shrink-wrap the callee-saved saves of `fib`'s call-free
    /// activations (`n` in `{2, 3}` once two levels are spliced). Read against
    /// the spliced body that cannot pay: every call-free path joins a
    /// call-bearing one at a splice's result phi before the method's one
    /// return, so the shared epilogue owes the full save set on both, and a
    /// save sunk past the entry has to be re-inserted on the call-free edge
    /// into that phi -- which is the path it was meant to spare
    /// (`r12w8-callcost-fib-call-anatomy-FIXED-20261001.md`, "Round 14 wave 5").
    /// Such an activation is a pure function of its argument instead, so this
    /// evaluates the body at compile time for the next few arguments and
    /// answers them without a frame:
    ///
    /// ```text
    ///   CMP  <p>32, k ; J<cc> .fast_return   ; the entry fast return
    ///   CMP  <p>32, hi                       ; every argument < lo left above
    ///   JLE  .fold                           ; outlined after the body
    ///   PUSH RBP ; ...
    /// .fold:                                 ; lo <= p <= hi
    ///   MOV  RAX, r(hi)                      ; sign-extended, as `Op::Const`
    ///   MOV  R11, r(v) ; CMP <p>32, v ; CMOVE RAX, R11   ; each v < hi
    ///   RET
    /// ```
    ///
    /// Why nothing observable is skipped (the entry fast return's argument,
    /// extended from one block to a path):
    ///
    /// * the evaluation ([`Self::entry_fold_eval`]) walks the blocks the body
    ///   would run for that argument, from block 0 through each `If` and
    ///   phi edge to an `Op::Return`, and gives up at the first node that is
    ///   not pure ([`entry_fast_return_may_skip`]: no call, trap, store,
    ///   allocation or safepoint) or not evaluable, so a folded argument's
    ///   activation is exactly `int` arithmetic on it and constants, with
    ///   Java's wrapping and shift masking ([`entry_fold_int_op`]);
    /// * the body is loop-free (no back-edge poll is skipped, no OSR entry
    ///   exists), and it makes a direct self-call somewhere, so the frames
    ///   this removes are a recursion's;
    /// * the stub writes RAX, R11 (caller-saved on both ABIs, and never an
    ///   incoming argument register) and the flags; the callee-saved file and
    ///   RSP stay the caller's;
    /// * offset 0 is still the not-entrant pad;
    /// * an `int` result's high half is not read by any consumer (the
    ///   poison mode proves it, and the plan is off under it), so the
    ///   sign-extended immediate is the full path's value.
    ///
    /// The `JLE` is emitted with a zero `rel32` and patched when the stub is
    /// written; unpatched, it falls into the prologue, which is the full path.
    /// Kill switch: `CRATONVM_JIT_IR_ENTRY_FOLD=0`, read per compile. Census:
    /// `CRATONVM_DBG_IR_ENTRY_FOLD=1` prints one `[ir-entry-fold]` line per
    /// compile that plans an entry fast return: the folded range, or why none.
    pub(in crate::ir_lower) fn emit_entry_fold(&mut self, fast: &EntryFastReturn) {
        let census = false;
        let fold = match self.plan_entry_fold(fast) {
            Ok(fold) => fold,
            Err(why) => {
                if census {
                    eprintln!("[ir-entry-fold] declined: {why}");
                }
                return;
            }
        };
        let Some(hi) = i32::try_from(fold.values.len())
            .ok()
            .and_then(|len| fold.lo.checked_add(len - 1))
        else {
            return;
        };
        self.emit_cmp_reg_imm(fold.cmp_reg, hi, false);
        self.buf.emit(&[0x0F, CmpOp::Le.x64_cc()]); // JLE rel32 .fold
        let patch = self.buf.pos();
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
        if census {
            eprintln!(
                "[ir-entry-fold] folded n in [{}, {hi}] -> {:?} (stopped: {}; self-calls answered: {})",
                fold.lo, fold.values, fold.stop, fold.self_calls
            );
        }
        self.entry_fold = Some((patch, fold));
    }

    /// What [`Self::emit_entry_fold`] folds for the entry fast return `fast`,
    /// or why it folds nothing.
    pub(in crate::ir_lower) fn plan_entry_fold(
        &self,
        fast: &EntryFastReturn,
    ) -> Result<EntryFold, String> {
        if fast.cmp_reg == RAX || fast.cmp_reg == R11 {
            return Err("the argument arrives in a register the stub writes".to_string());
        }
        if !self.graph.nodes.iter().any(Self::is_direct_self_call) {
            return Err("no direct self-call".to_string());
        }
        if schedule_block_cfg_has_cycle(self.schedule) {
            return Err("the body has a loop".to_string());
        }
        // The arguments the fast return does NOT take, from the first one up:
        // `p <= imm` returns below `imm + 1`, `p < imm` below `imm`. Any other
        // condition leaves no contiguous range starting right past it.
        let lo = match fast.ret_cc {
            CmpOp::Le => fast.imm.checked_add(1),
            CmpOp::Lt => Some(fast.imm),
            _ => None,
        }
        .ok_or_else(|| format!("the fast return is taken on {:?}", fast.ret_cc))?;
        let param_idx = match self.graph.nodes.get(fast.param as usize).map(|n| &n.op) {
            Some(Op::Param(idx)) => *idx,
            _ => return Err("the compared value is not a parameter".to_string()),
        };
        let mut values: Vec<i32> = Vec::new();
        let mut stop = format!("the cap of {ENTRY_FOLD_MAX_VALUES} values");
        let mut calls = EntryFoldCalls {
            enabled: ir_entry_fold_self_calls_enabled(),
            memo: HashMap::new(),
            active: Vec::new(),
            activations: 0,
        };
        for i in 0..ENTRY_FOLD_MAX_VALUES {
            let Some(arg) = i32::try_from(i).ok().and_then(|i| lo.checked_add(i)) else {
                stop = "the int range".to_string();
                break;
            };
            // The folded argument itself is "in progress": a body that calls
            // itself with its own argument recurses forever and folds nothing.
            calls.active.clear();
            calls.active.push(arg);
            match self.entry_fold_eval(param_idx, arg, &mut calls) {
                Ok(r) => {
                    calls.memo.insert(arg, r);
                    values.push(r);
                }
                Err(why) => {
                    stop = format!("n={arg}: {why}");
                    break;
                }
            }
        }
        if values.is_empty() {
            return Err(stop);
        }
        let self_calls = calls.activations;
        Ok(EntryFold {
            cmp_reg: fast.cmp_reg,
            lo,
            values,
            stop,
            self_calls,
        })
    }

    /// The body's `int` result for argument `arg` of parameter `param_idx`,
    /// by walking the blocks it would run: block 0 first, each block's data
    /// nodes in schedule order, an `If` on its condition's value
    /// (`successors[0]` is the true edge), a phi edge as the parallel
    /// assignment [`Self::gather_phi_copies`] describes, and an `Op::Return`
    /// of a known `int` as the answer. `Err` names the first thing that is
    /// not a pure, evaluable step. Pure: reads the graph and the schedule.
    ///
    /// Round 14 wave 7 (lane callcost3): a direct self-call on the path is a
    /// step too, when its argument is known -- its value is this same
    /// evaluation for that argument ([`Self::entry_fold_self_call`]).
    pub(in crate::ir_lower) fn entry_fold_eval(
        &self,
        param_idx: u16,
        arg: i32,
        calls: &mut EntryFoldCalls,
    ) -> Result<i32, String> {
        let mut env: HashMap<NodeId, i32> = HashMap::new();
        let mut pred: Option<usize> = None;
        let mut cur = 0usize;
        for _ in 0..ENTRY_FOLD_MAX_STEPS {
            let block = self
                .schedule
                .blocks
                .get(cur)
                .ok_or_else(|| format!("block {cur} does not exist"))?;
            if let Some(p) = pred {
                self.entry_fold_phi_edge(p, cur, param_idx, arg, &mut env)?;
            }
            for &n in &block.nodes {
                let node = self
                    .graph
                    .nodes
                    .get(n as usize)
                    .ok_or_else(|| format!("n{n} does not exist"))?;
                if Self::is_direct_self_call(node) {
                    let v = self.entry_fold_self_call(node, param_idx, arg, &env, calls)?;
                    env.insert(n, v);
                    continue;
                }
                if let Some(v) = self.entry_fold_node(node, param_idx, arg, &env)? {
                    env.insert(n, v);
                }
            }
            let Some(term_id) = block.terminator else {
                match block.successors.as_slice() {
                    [next] => {
                        pred = Some(cur);
                        cur = *next;
                        continue;
                    }
                    _ => {
                        return Err(format!(
                            "block {cur} has no terminator and no single successor"
                        ))
                    }
                }
            };
            let term = self
                .graph
                .nodes
                .get(term_id as usize)
                .ok_or_else(|| format!("n{term_id} does not exist"))?;
            match term.op {
                Op::If => {
                    let cond = *term.inputs.get(1).ok_or("an If without a condition")?;
                    let taken = self
                        .entry_fold_condition(cond, param_idx, arg, &env)
                        .ok_or_else(|| format!("block {cur}'s condition n{cond} is not known"))?;
                    let next = match (block.successors.as_slice(), taken) {
                        ([t, _], true) => *t,
                        ([_, f], false) => *f,
                        _ => return Err(format!("block {cur}'s If has no two successors")),
                    };
                    pred = Some(cur);
                    cur = next;
                }
                Op::Return => {
                    if term.inputs.len() != 2 {
                        return Err("a return without exactly one value".to_string());
                    }
                    let val = *term.inputs.get(1).ok_or("a return without a value")?;
                    return self
                        .entry_fold_operand(val, param_idx, arg, &env)
                        .ok_or_else(|| format!("the returned n{val} is not a known int"));
                }
                ref other => return Err(format!("block {cur} ends in {other:?}")),
            }
        }
        Err(format!("more than {ENTRY_FOLD_MAX_STEPS} blocks"))
    }

    /// Round 14 wave 7 (lane callcost3): the value of the direct self-call
    /// `node` on the evaluated path, by evaluating this body for its argument
    /// (memoized in `calls`), or why it is not known.
    ///
    /// Why that is the call's result: a direct self-call enters this same
    /// body at offset 0 with the call's arguments, and the evaluation walks
    /// exactly the path the callee would run for them, under the same purity
    /// rule. So a folded activation whose path makes such calls is still a
    /// pure function of its argument: the frames it no longer builds are
    /// frameless in effect already (every node on every callee path is pure),
    /// and all it drops besides is what the entry fold's own argument drops
    /// (the entry poll of a loop-free body, the sampled stack check). The
    /// admission keeps the callee's inputs exactly known: the compared
    /// argument is a known `int`, and every other argument is the caller's
    /// own parameter in the same position, so the callee's unknowns are the
    /// caller's (and a path that reads one gives up, as it does here). An
    /// argument already being evaluated is a recursion that never returns at
    /// run time and gives up; so do a depth past [`ENTRY_FOLD_MAX_DEPTH`] and
    /// a compile past [`ENTRY_FOLD_MAX_ACTIVATIONS`] distinct arguments.
    ///
    /// Kill switch: `CRATONVM_JIT_IR_ENTRY_FOLD_SELF_CALLS=0`, read per
    /// compile (`plan_entry_fold`).
    pub(in crate::ir_lower) fn entry_fold_self_call(
        &self,
        node: &Node,
        param_idx: u16,
        arg: i32,
        env: &HashMap<NodeId, i32>,
        calls: &mut EntryFoldCalls,
    ) -> Result<i32, String> {
        if !calls.enabled {
            return Err("a direct self-call (CRATONVM_JIT_IR_ENTRY_FOLD_SELF_CALLS=0)".to_string());
        }
        if node.ty != IrType::Int {
            return Err("a direct self-call without an int result".to_string());
        }
        let args = node.inputs.get(2..).unwrap_or(&[]);
        if args.len() != self.num_params {
            return Err(format!(
                "a self-call with {} arguments for {} parameters",
                args.len(),
                self.num_params
            ));
        }
        let mut next: Option<i32> = None;
        for (j, &a) in args.iter().enumerate() {
            if j == usize::from(param_idx) {
                next = self.entry_fold_operand(a, param_idx, arg, env);
                continue;
            }
            let same_param = matches!(
                self.graph.nodes.get(a as usize).map(|x| &x.op),
                Some(Op::Param(pj)) if usize::from(*pj) == j
            );
            if !same_param {
                return Err(format!(
                    "self-call argument {j} is not the caller's own parameter {j}"
                ));
            }
        }
        let next = next.ok_or("the self-call's compared argument is not a known int")?;
        if let Some(&v) = calls.memo.get(&next) {
            return Ok(v);
        }
        if calls.active.contains(&next) {
            return Err(format!(
                "f({next}) calls itself with an argument in progress"
            ));
        }
        if calls.active.len() >= ENTRY_FOLD_MAX_DEPTH {
            return Err(format!("self-calls nested past {ENTRY_FOLD_MAX_DEPTH}"));
        }
        if calls.activations >= ENTRY_FOLD_MAX_ACTIVATIONS {
            return Err(format!(
                "more than {ENTRY_FOLD_MAX_ACTIVATIONS} self-call arguments"
            ));
        }
        calls.activations += 1;
        calls.active.push(next);
        let result = self.entry_fold_eval(param_idx, next, calls);
        calls.active.pop();
        let v = result.map_err(|why| format!("self-call f({next}): {why}"))?;
        calls.memo.insert(next, v);
        Ok(v)
    }

    /// `id`'s value on the evaluated path, when it is known: an evaluated
    /// node, or an `int` constant that fits 32 bits, or the folded parameter.
    pub(in crate::ir_lower) fn entry_fold_operand(
        &self,
        id: NodeId,
        param_idx: u16,
        arg: i32,
        env: &HashMap<NodeId, i32>,
    ) -> Option<i32> {
        if let Some(&v) = env.get(&id) {
            return Some(v);
        }
        let node = self.graph.nodes.get(id as usize)?;
        if node.ty != IrType::Int {
            return None;
        }
        match node.op {
            Op::Const(k) => i32::try_from(k).ok(),
            Op::Param(idx) if idx == param_idx => Some(arg),
            _ => None,
        }
    }

    /// An `If` condition's truth: an evaluated value (the `TEST EAX, EAX`
    /// reading, non-zero), or an `Op::Cmp` of two known `int`s.
    pub(in crate::ir_lower) fn entry_fold_condition(
        &self,
        cond: NodeId,
        param_idx: u16,
        arg: i32,
        env: &HashMap<NodeId, i32>,
    ) -> Option<bool> {
        if let Some(v) = self.entry_fold_operand(cond, param_idx, arg, env) {
            return Some(v != 0);
        }
        let node = self.graph.nodes.get(cond as usize)?;
        self.entry_fold_node(node, param_idx, arg, env)
            .ok()
            .flatten()
            .map(|v| v != 0)
    }
}
