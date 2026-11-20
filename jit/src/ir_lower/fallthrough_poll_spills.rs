#[allow(unused_imports)]
use super::*;

impl<'a> Lowerer<'a> {
    /// JIT round 25 item 1a (item-2 review): the inspection spills of a
    /// FALL-THROUGH back-edge poll -- a `goto` block with no terminator whose
    /// successor is a loop header (`lower_block`). Its poll's safepoint map
    /// records `cur_bci`, the last bci lowered, which the plan never made an
    /// inspection bci; a debugger local read or a stack walk of a thread
    /// parked there would take the table point at that bci, which names a
    /// site-named value's register (no register file there).
    ///
    /// So every site-named value a frame state at `cur_bci` names is stored
    /// into its home before the poll, and `cur_bci` joins the inspection bcis
    /// (its table point then names the home). A value that cannot be stored
    /// -- not live at the block's end, not resident, or wave 3 off -- refuses
    /// the attempt, and the retry keeps its home. The mode exit of this poll
    /// resumes at the HEADER, whose bci the plan hard-blocks, so it names
    /// none of these.
    pub(in crate::ir_lower) fn emit_fallthrough_poll_spills(&mut self, block_idx: usize) {
        // W6-B: the poll right behind this resolves its mode exit with no
        // node or terminator being lowered; tell `site_location_of` where it
        // is (cleared by `emit_safepoint_poll`).
        if self.site_named_loop_on {
            self.fallthrough_poll_block = Some(block_idx);
        }
        if self.site_named_count == 0 {
            return;
        }
        let graph = self.graph;
        let bci = self.cur_bci;
        let resume = self.resume_bci(bci);
        let mut owed: Vec<NodeId> = Vec::new();
        for sp in &graph.safepoints {
            if sp.bci != bci && sp.bci != resume {
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
                if v != NO_NODE
                    && self.site_named.get(v as usize).copied().unwrap_or(false)
                    && !owed.contains(&v)
                {
                    owed.push(v);
                }
            }
        }
        if owed.is_empty() {
            return;
        }
        if self.spill_at_inspection_on {
            self.site_spill_bcis.insert(bci);
            self.site_spill_bcis.insert(resume);
        }
        let edge = self
            .site_live
            .as_ref()
            .and_then(|sl| sl.span.get(block_idx).map(|&(_, edge)| edge));
        for v in owed {
            if self.spill_at_inspection_on && self.spilled_on_every_path(v, block_idx) {
                self.inspection_spills_deduped += 1;
                continue;
            }
            let reg = self.gp_reg_of.get(v as usize).copied().flatten();
            let home = self.home_offset_of(v);
            let live = match (self.site_live.as_ref(), edge) {
                (Some(sl), Some(p)) => site_value_live_at(sl, &self.schedule.blocks, v, p),
                _ => false,
            } || self.reg_alloc_coalesced_live_at_edge(v, block_idx);
            match (reg, home) {
                (Some(reg), Some(off))
                    if self.spill_at_inspection_on && live && self.resident_gpr(v) == Some(reg) =>
                {
                    if self.site_named_loop_on {
                        // W6-B: into the poll's slow path (see
                        // `emit_inspection_spills`).
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
                            "n{v} is named at the fall-through back-edge poll's bci {bci} and cannot be stored there"
                        ),
                    ));
                }
            }
        }
    }

    /// Has an inspection spill of `v` happened on every path to the block
    /// being lowered: one emitted earlier in this block, or in a block that
    /// dominates it?
    pub(in crate::ir_lower) fn spilled_on_every_path(&self, v: NodeId, block_idx: usize) -> bool {
        let Some(blocks) = self.site_spills.get(&v) else {
            return false;
        };
        blocks.iter().any(|&b| {
            b == block_idx
                || (self.schedule.dom.is_reachable(block_idx)
                    && self.schedule.dom.dominates(b, block_idx))
        })
    }

    /// Where the site-named value `v` is at the site being lowered, for a
    /// frame that may be read there: its HOME, when an inspection spill
    /// reached here on every path (wave 3), else its REGISTER when the
    /// allocator's liveness proves it (wave 2), else `None`. The reusable
    /// half of [`Self::site_named_frame_value`]; a GC map that wants to name
    /// a reference parked in a callee-saved register would ask the same
    /// question, and use the same spill to answer it with a frame word.
    pub(in crate::ir_lower) fn site_location_of(&self, v: NodeId) -> Option<FrameValue> {
        let node = self.graph.nodes.get(v as usize)?;
        let reg = self.gp_reg_of.get(v as usize).copied().flatten()?;
        let (at, term) = match (self.cur_def, self.cur_term) {
            (Some(d), _) => (d, false),
            (None, Some(t)) => (t, true),
            // W6-B: the fall-through back-edge poll of the block being
            // lowered (its mode exit): asked at the block's outgoing edge,
            // which is all `site_live_at` reads for a terminator.
            (None, None) if self.fallthrough_poll_block == Some(self.cur_lower_block) => {
                (NO_NODE, true)
            }
            (None, None) => return None,
        };
        if self.spill_at_inspection_on && self.spilled_on_every_path(v, self.cur_lower_block) {
            let off = self.home_offset_of(v)?;
            return Some(Self::typed_stack_slot(-off, node.ty));
        }
        if self.resident_gpr(v) == Some(reg) && self.site_live_at(v, at, term) {
            return Some(match node.ty {
                IrType::Long => FrameValue::RegisterLong(reg),
                IrType::Ref => FrameValue::RegisterRef(reg),
                _ => FrameValue::Register(reg),
            });
        }
        None
    }

    /// Which values [`Self::plan_site_named_homes`] must leave alone, and
    /// (wave 3) the bcis at which a value it takes must be SPILLED to its home
    /// before the frame can be inspected there.
    ///
    /// * **Hard blocks**, never site-named: the method entry, every loop
    ///   header (the OSR entry seeds homes there), and the loop-carried and
    ///   promoted-pending recipes.
    /// * **Inspection bcis**: where a node traps through something other than a
    ///   pad -- a call, an allocation, a monitor, a helper that can reach a
    ///   safepoint -- and every back-edge branch (its poll). A parked frame
    ///   there is read without a register file, through its homes. Without
    ///   `spill` a value named there is blocked, as in wave 2; with it the
    ///   value is taken, and `emit_inspection_spills` stores its register
    ///   into its home at the inspecting node's first byte (or before the
    ///   poll), once per dominating path.
    /// * **Liveness**: at every node that can transfer at a bci naming the
    ///   value (and every back-edge branch, under `spill`), the allocator's
    ///   model must have it live ([`site_value_live_at`]); a value named
    ///   where its register may already belong to another is blocked here
    ///   rather than refused at emission.
    /// * **W6-B, `header_live`** ([`ir_site_named_loop_values_enabled`]): a
    ///   loop header is no hard block. A value its frame state names passes
    ///   when it is a phi of that header or the model has it live into the
    ///   header block; anything else named there is blocked as before.
    ///
    /// `why`, when given (the `ir-home-why` census), receives per value the
    /// first reason it was blocked (`HK_*`; `HK_NONE` for none).
    pub(in crate::ir_lower) fn site_named_blocked(
        &self,
        spill: bool,
        header_live: bool,
        why: Option<&mut Vec<u8>>,
    ) -> (Vec<bool>, std::collections::HashSet<usize>) {
        let mut why = why;
        let graph = self.graph;
        let mut out = vec![false; graph.nodes.len()];
        // W6-B: `(header bci, header block)` of every back edge's target, when
        // headers are checked by liveness instead of hard-blocked.
        let mut headers: Vec<(usize, usize)> = Vec::new();
        let mut hard: std::collections::HashSet<usize> = std::collections::HashSet::new();
        let mut inspected: std::collections::HashSet<usize> = std::collections::HashSet::new();
        // The method entry and its poll.
        hard.insert(0);
        // bci -> the snapshots at it.
        let mut at_bci: HashMap<usize, Vec<usize>> = HashMap::new();
        for (si, sp) in graph.safepoints.iter().enumerate() {
            at_bci.entry(sp.bci).or_default().push(si);
        }
        let named_at = |bci: usize, out: &mut Vec<NodeId>| {
            for &si in at_bci.get(&bci).map(|v| v.as_slice()).unwrap_or(&[]) {
                if let Some(sp) = graph.safepoints.get(si) {
                    for v in sp
                        .locals
                        .iter()
                        .chain(sp.stack.iter())
                        .chain(sp.monitors.iter())
                        .copied()
                        .chain(sp.sb_virtual_nodes())
                    {
                        if v != NO_NODE {
                            out.push(v);
                        }
                    }
                }
            }
        };
        // Every node a transfer can happen at, with the position the site
        // liveness is asked at, and (W7-A) the node itself when the snapshot
        // is the one at its own bci (`NO_NODE` for resume and back-edge sites).
        let mut sites: Vec<(usize, usize, NodeId)> = Vec::new();
        let sl = self.site_live.as_ref();
        for (nid, node) in graph.nodes.iter().enumerate() {
            let Some(pc) = node.bytecode_pc else {
                continue;
            };
            // A direct self-call transfers nothing (`self_call_reads_no_frame_state`)
            // but still parks the frame for as long as the callee runs, so it
            // is an inspection point like any call.
            if self.node_cannot_deopt_here(node) {
                continue;
            }
            let resume = self.resume_bci(pc);
            if !op_traps_only_through_pads(&node.op) {
                inspected.insert(pc);
                inspected.insert(resume);
            }
            if let Some(p) = sl.and_then(|s| s.pos_of.get(nid).copied().flatten()) {
                sites.push((pc, p, NodeId::try_from(nid).unwrap_or(NO_NODE)));
                if resume != pc {
                    sites.push((resume, p, NO_NODE));
                }
            }
        }
        for (b, block) in self.schedule.blocks.iter().enumerate() {
            if !block.successors.iter().any(|&s| s <= b) {
                continue;
            }
            if let Some(pc) = block
                .terminator
                .and_then(|t| graph.nodes.get(t as usize))
                .and_then(|t| t.bytecode_pc)
            {
                inspected.insert(pc);
                if let Some(&(_, edge)) = sl.and_then(|s| s.span.get(b)) {
                    sites.push((pc, edge, NO_NODE));
                }
            }
            for &succ in &block.successors {
                if succ > b {
                    continue;
                }
                if let Some(pc) = self
                    .schedule
                    .blocks
                    .get(succ)
                    .and_then(|h| graph.nodes.get(h.ctrl as usize))
                    .and_then(|h| h.bytecode_pc)
                {
                    if header_live {
                        headers.push((pc, succ));
                    } else {
                        hard.insert(pc);
                    }
                }
            }
        }
        let mut named: Vec<NodeId> = Vec::new();
        for &bci in &hard {
            named.clear();
            named_at(bci, &mut named);
            let code = if bci == 0 { HK_ENTRY } else { HK_HEADER };
            for &v in &named {
                mark_site_blocked(&mut out, &mut why, v, code);
            }
        }
        // W6-B: a header's namings, by liveness. The OSR entry seeds a dropped
        // home's REGISTER (`emit_osr_seed`), which holds the value from the
        // header on when it is live into it (or is the header's own phi, which
        // every incoming edge copy writes); nothing parks a frame AT a header
        // bci unless a node there inspects it, and those nodes are `sites`.
        for &(bci, h) in &headers {
            if hard.contains(&bci) {
                continue;
            }
            let ctrl = self.schedule.blocks.get(h).map(|blk| blk.ctrl);
            named.clear();
            named_at(bci, &mut named);
            for &v in &named {
                let own_phi = graph.nodes.get(v as usize).is_some_and(|node| {
                    matches!(node.op, Op::Phi)
                        && ctrl.is_some()
                        && node.inputs.first().copied() == ctrl
                });
                let live_in = sl
                    .and_then(|s| s.facts.get(&v))
                    .and_then(|(_, live_in)| live_in.get(h).copied())
                    .unwrap_or(false);
                if !(own_phi || live_in) {
                    mark_site_blocked(&mut out, &mut why, v, HK_HEADER);
                }
            }
        }
        if !spill {
            for &bci in &inspected {
                named.clear();
                named_at(bci, &mut named);
                for &v in &named {
                    mark_site_blocked(&mut out, &mut why, v, HK_INSPECT);
                }
            }
        }
        if let Some(sl) = sl {
            for &(bci, p, at) in &sites {
                named.clear();
                named_at(bci, &mut named);
                for &v in &named {
                    // W7-A: a value read by the pad site itself is still in its
                    // register there (`Self::reg_alloc_read_at_site`).
                    if sl.facts.contains_key(&v)
                        && !site_value_live_at(sl, &self.schedule.blocks, v, p)
                        && !self.reg_alloc_read_at_site(v, at)
                    {
                        mark_site_blocked(&mut out, &mut why, v, HK_NOT_LIVE);
                    }
                }
            }
        }
        for v in loop_carried_recipe_values(self.sr_map) {
            mark_site_blocked(&mut out, &mut why, v, HK_RECIPE);
        }
        for v in promoted_pending_deopt_named(graph, self.schedule) {
            mark_site_blocked(&mut out, &mut why, v, HK_RECIPE);
        }
        if !spill {
            inspected.clear();
        }
        (out, inspected)
    }

    /// Is the site-named value `v` live, in the allocator's model, at the
    /// site being lowered -- inside node `at` (`term`: the block terminator
    /// `at`, taken at its block's outgoing-edge position)? Live means defined
    /// before the position (in its block, or live into the block) and read
    /// STRICTLY after it in the block, or live out of the block. Strict,
    /// because at a value's last read the node being lowered may take its
    /// register for its own result.
    pub(in crate::ir_lower) fn site_live_at(&self, v: NodeId, at: NodeId, term: bool) -> bool {
        let Some(sl) = self.site_live.as_ref() else {
            return false;
        };
        // W7-A (`CRATONVM_JIT_IR_REG_ALLOC`): a coalesced back-edge source is
        // in its register at its own block's outgoing edge, where the phi
        // takes it over without a copy.
        if term && self.reg_alloc_coalesced_live_at_edge(v, self.cur_lower_block) {
            return true;
        }
        // W7-A: and a value read by the pad site being lowered is in its
        // register until that node publishes its own result elsewhere.
        if !term && self.reg_alloc_read_at_site(v, at) {
            return true;
        }
        let p = if term {
            // A terminator is placed in no block of `node_to_block`; it is the
            // end of the block being lowered.
            let _ = at;
            match sl.span.get(self.cur_lower_block) {
                Some(&(_, edge)) => edge,
                None => return false,
            }
        } else {
            match sl.pos_of.get(at as usize).copied().flatten() {
                Some(p) => p,
                None => return false,
            }
        };
        site_value_live_at(sl, &self.schedule.blocks, v, p)
    }

    /// The frame value of a site-named value ([`Self::plan_site_named_homes`]):
    /// its register at a site that passes [`Self::site_live_at`] and where
    /// `resident_gpr` agrees; its planned register in a TABLE point (whose
    /// values no transfer reads -- a guard bakes its own box -- and every
    /// site at that bci checked the register already); otherwise a refusal of
    /// the attempt naming the value.
    pub(in crate::ir_lower) fn site_named_frame_value(&self, id: NodeId, ty: IrType) -> FrameValue {
        let wrap = |r: u8| match ty {
            IrType::Int => FrameValue::Register(r),
            IrType::Long => FrameValue::RegisterLong(r),
            IrType::Ref => FrameValue::RegisterRef(r),
            _ => FrameValue::Unsupported,
        };
        let planned = self.gp_reg_of.get(id as usize).copied().flatten();
        if let Some(reg) = planned {
            if self.building_table_points.get() {
                // Wave 3: at an inspection bci the readers read the HOME the
                // inspection spill wrote there.
                let at_inspection = self
                    .table_point_bci
                    .get()
                    .is_some_and(|bci| self.site_spill_bcis.contains(&bci));
                if at_inspection {
                    if let Some(off) = self
                        .home_offset_of(id)
                        .or_else(|| self.planned_slot_off(id).ok())
                    {
                        return Self::typed_stack_slot(-off, ty);
                    }
                } else {
                    return wrap(reg);
                }
            } else if let Some(v) = self.site_location_of(id) {
                if matches!(v, FrameValue::Register(_) | FrameValue::RegisterLong(_)) {
                    self.deopt_reg_named.set(self.deopt_reg_named.get() + 1);
                }
                return v;
            }
        }
        if !self.quiet_dropped_home.get() {
            self.latch_bailout(Bailout::with_context(
                BailoutReason::UnallocatedValue { node: id },
                format!("n{id}'s home was dropped (site-named) but its register is not provably its at this site"),
            ));
        }
        FrameValue::Unsupported
    }

    /// Let a CARRIED single-use value stop writing its home word.
    ///
    /// # The store this removes
    ///
    /// `plan_register_residency` deliberately does not promote a single-use
    /// intermediate — its live range is one instruction long, and what it wants
    /// is not a register for the whole method but *not to be written to memory
    /// at all*. The carry gives it exactly that on the READ side: the producer
    /// leaves the value in a register and its one consumer reads it there, no
    /// load.
    ///
    /// The STORE stayed. `store_rax` drops it only when `home_dropped` is set,
    /// and `home_dropped` came from `value_home_droppable`, which requires a
    /// register assignment the value does not have. So the emitter wrote a
    /// frame word for every intermediate and then never read it — visible in
    /// the disassembly as literal dead stores:
    ///
    /// ```text
    ///     imul eax,41C64E6Dh
    ///     mov  [rbp-0A8h],rax   ; never read
    ///     add  eax,3039h
    ///     mov  [rbp-0C8h],rax   ; never read
    /// ```
    ///
    /// Measured on the counted-loop probe: of 52 graph nodes, **28 are
    /// single-use** and skipped by the residency planner for this reason, and
    /// the loop body carried 17 frame stores for 16 instructions of real work.
    ///
    /// # Why it is safe
    ///
    /// The carry planner's own contract is that the value has exactly one use
    /// and that use is the next node in the same block. So the only reader of
    /// the home word would be a frame state — and this runs only for values no
    /// REACHABLE deopt names (see `compute_deopt_named_reachable`). Any other
    /// site that asks for the slot meets `slot_of`'s refusal on a dropped home
    /// and the compile is abandoned to the single-pass backend, which is the
    /// same net every other home-drop rests on.
    ///
    /// `Ref` is excluded here as everywhere else in this file: a reference in a
    /// register is invisible to a root walk, and the oop map cannot name one.
    /// This changes nothing about that.
    pub(in crate::ir_lower) fn extend_home_drops_to_carried_values(&mut self) {
        if !ir_register_authoritative_enabled() {
            return;
        }
        // The same switch conjunction `value_home_droppable` opens with: the
        // carry's read side and the publish discipline have to be in force, or
        // dropping the write leaves the value nowhere.
        if !(ir_publish_at_def_enabled()
            && ir_deopt_regs_enabled()
            && ir_skip_live_republish_enabled()
            && self.phi_copy_regs_on)
        {
            return;
        }
        for id in 0..self.home_dropped.len().min(self.carry_of.len()) {
            if self.home_dropped[id] {
                continue;
            }
            if self.carry_of[id].is_none() {
                continue;
            }
            let Some(node) = self.graph.nodes.get(id) else {
                continue;
            };
            if !matches!(node.ty, IrType::Int | IrType::Long) {
                continue;
            }
            if !op_home_is_one_store_rax(&node.op) {
                continue;
            }
            if self.deopt_named_reachable.get(id).copied().unwrap_or(true) {
                continue;
            }
            self.home_dropped[id] = true;
            self.carried_homes_dropped += 1;
        }
    }

    /// `MOV reg, imm` in the shortest encoding that reproduces `val` in all 64
    /// bits: `mov r32, imm32` (zero-extends) for 0..=u32::MAX, `mov r64,
    /// simm32` for the rest of the i32 range, `mov r64, imm64` otherwise.
    pub(in crate::ir_lower) fn emit_mov_reg_imm_smart(&mut self, reg: u8, val: i64) {
        if reg == RCX {
            self.note_rcx_written();
        }
        if (0..=i64::from(u32::MAX)).contains(&val) {
            if reg >= 8 {
                self.buf.emit_byte(0x41); // REX.B
            }
            self.buf.emit_byte(0xB8 + (reg & 7));
            self.buf.emit(&(val as u32).to_le_bytes());
        } else if i32::try_from(val).is_ok() {
            let rex = 0x48 | if reg >= 8 { 0x01 } else { 0 };
            self.buf.emit_byte(rex);
            self.buf.emit_byte(0xC7); // MOV r/m64, simm32
            self.buf.emit_byte(0xC0 | (reg & 7));
            self.buf.emit(&(val as i32).to_le_bytes());
        } else {
            self.emit_mov_reg_imm64(reg, val as u64);
        }
    }

    /// Whether `base` was already null-tested and mapped-proved in the block
    /// being lowered (`ir_receiver_guard_cse_enabled`).
    pub(in crate::ir_lower) fn receiver_already_guarded(&self, base: NodeId) -> bool {
        self.receiver_cse_on && self.guarded_receivers.contains(&base)
    }

    /// Is `base` known non-null at this use? Besides block-local guards,
    /// reuse the allocation/phi proof already used for field stores: failed
    /// allocations leave the compiled path before their result can be used.
    pub(in crate::ir_lower) fn receiver_already_non_null(&self, base: NodeId) -> bool {
        self.receiver_already_guarded(base)
            || self.null_proven_receivers.contains(&base)
            || store_receiver_cannot_be_null(self.graph, base)
            || matches!(
                self.graph.nodes.get(base as usize).map(|n| &n.op),
                Some(Op::IndyRootRef { slot_addr })
                    if self.graph.const_ref_meta.get(slot_addr).is_some_and(|m| m.non_null)
            )
    }

    /// Record that a null test has just proven `base` on its fall-through.
    pub(in crate::ir_lower) fn note_receiver_non_null(&mut self, base: NodeId) {
        if !self.null_proven_receivers.contains(&base) {
            self.null_proven_receivers.push(base);
        }
    }

    /// Seed this block's null proofs with `this`.
    ///
    /// # Why a seed, and not just the per-block CSE
    ///
    /// The CSE above proves a receiver once per block. That is worth nothing to
    /// a loop whose body reads one field: the body is one block, its single
    /// `getfield` is the first dereference in it, and so the test is emitted
    /// once and executed on every iteration — forever, for a value the JVM
    /// guarantees at the call site.
    ///
    /// This is the same hole the single-pass backend had, in the same shape
    /// and for the same reason, and it was closed there by
    /// `CRATONVM_JIT_THIS_NONNULL` seeding the null-check dataflow's entry
    /// state. That fix was single-pass only: both arms it touches are in
    /// `x64/bytecode_walk.rs`, so this tier kept emitting `TEST RAX, RAX; JZ`
    /// at every `getfield` — and a 2026-09-03 measurement put the optimizing
    /// tier at ~1.65x the baseline's time on exactly that loop.
    ///
    /// The seed is one fact: an instance method's parameter 0 is non-null,
    /// because the JVM enters one only through a call site that has already
    /// null-checked the receiver. `<init>` included — its receiver is
    /// uninitialized, never null.
    ///
    /// **It is only ever the parameter NODE.** A local reassigned from
    /// parameter 0 is a different node and gets nothing; there is no
    /// bytecode-local pattern match here to mis-attribute, which is the class
    /// of bug `preceding_aload_nonnull_local` carries a soundness fix for.
    pub(in crate::ir_lower) fn seed_block_null_proofs(&mut self) {
        if !ir_this_nonnull_enabled() {
            return;
        }
        let Some(recv) = self.graph.receiver_param else {
            return;
        };
        for (id, node) in self.graph.nodes.iter().enumerate() {
            if node.op == Op::Param(recv) {
                // Cast: node ids index `graph.nodes`, which the builder bounds.
                self.null_proven_receivers.push(id as NodeId);
                crate::metrics::note_ir_receiver_seed();
                break;
            }
        }
    }

    /// Record that the block being lowered has just proved `base`.
    pub(in crate::ir_lower) fn note_receiver_guarded(&mut self, base: NodeId) {
        if self.receiver_cse_on && !self.guarded_receivers.contains(&base) {
            self.guarded_receivers.push(base);
        }
    }

    /// Lower one data node, with `cur_def` set across it so `store_rax` can
    /// recognise the node's own home write.
    ///
    /// The verification at the bottom is the reason `op_home_is_one_store_rax`
    /// may be an audited list rather than a proof. If a value whose home was
    /// dropped finishes its own lowering without its register having been
    /// published, then nothing wrote the value anywhere and the artifact is
    /// wrong -- so the compile is refused here, deterministically, at the node
    /// that did it, rather than later at whichever reader happened to notice.
    /// A phi is exempt: `lower_data_node` deliberately emits nothing for one
    /// (`emit_phi_copies` owns it), which is not the same event.
    pub(in crate::ir_lower) fn lower_data_node_tracked(&mut self, id: NodeId) {
        if self.value_flow.is_absorbed(id) {
            return;
        }
        let prev = self.cur_def;
        let prev_published = self.cur_def_published;
        self.cur_def = Some(id);
        self.cur_def_published = false;
        // Interpreter round i2 wave 64 (lane L2): the token-read guard's set
        // for this node (`token_read_check`), restored on the way out like
        // `cur_def`. Empty, and nothing computed, while the guard is off.
        // (JIT round 19 wave 10: the same computation, moved into
        // `token_scope_enter` so the `term` and `mir` scopes share it.)
        let prev_token_only = self.token_scope_enter(&[id], TokenScope::Data);
        let rcx_writes_before = self.rcx_writes;
        // Capture the state at this scheduled use without invoking readers
        // that update token-verification state. Off unless both debug flags
        // are enabled, so normal lowering performs no census allocation.
        let location_trace = crate::x64::dbg_jitc_enabled()
            && false;
        let trace_start = self.buf.pos();
        let trace_inputs = location_trace.then(|| {
            self.graph.nodes[id as usize]
                .inputs
                .iter()
                .map(|&input| {
                    let i = input as usize;
                    (
                        input,
                        self.node_slot.get(i).copied().flatten().map(|v| v.get()),
                        self.gp_reg_of.get(i).copied().flatten(),
                        self.gp_reg_live.get(i).copied().unwrap_or(false),
                        self.home_dropped.get(i).copied().unwrap_or(false),
                    )
                })
                .collect::<Vec<_>>()
        });
        // JIT round 19 wave 5 (lane osr5, OSR2-3): inside a promoted loop with
        // kept stores, every deopt pad this node emits writes the cells back.
        let pending = self.begin_promoted_pending(id);
        self.lower_data_node(id);
        if let Some(inputs) = trace_inputs {
            let node = &self.graph.nodes[id as usize];
            let rule = self.mir.as_ref().and_then(|plan| plan.tile(id)).map(|tile| tile.rule);
            eprintln!(
                "[ir-location] base={:p} method={} block={:?} node={} bci={:?} op={:?} rule={:?} bytes={}..{} inputs={:?} result_slot={:?} result_reg={:?} result_live={} home_dropped={}",
                self.buf.as_ptr(),
                self.graph.scope_method_key,
                self.schedule.node_to_block.get(id as usize),
                id,
                node.bytecode_pc,
                node.op,
                rule,
                trace_start,
                self.buf.pos(),
                inputs,
                self.node_slot.get(id as usize).copied().flatten().map(|v| v.get()),
                self.gp_reg_of.get(id as usize).copied().flatten(),
                self.gp_reg_live.get(id as usize).copied().unwrap_or(false),
                self.home_dropped.get(id as usize).copied().unwrap_or(false),
            );
        }
        self.end_promoted_pending(id, pending);
        // The guard audits the node's OWN arm; what follows (a prefix's join,
        // the post-call exit site) is not an operand read of this node.
        self.token_scope_exit(prev_token_only);
        // An inline call prefix's hit edge joins HERE, after every route the
        // node's arm took (round 9 wave 9, irl9; see
        // `try_emit_ir_arraylist_access_prefix`). The hit path has already
        // stored the node's result.
        // W8-4: a prefix's hit route joins after the arm (checked below).
        let w8_prefixed = self.call_prefix_join.is_some();
        if let Some(p) = self.call_prefix_join.take() {
            // JIT round 25 item 1a: that hit edge is a second route to this
            // position, and its RAX is not the elided store's -- unless the
            // store's elision proved it is (JIT round 26, `prefix_rax_proofs`).
            let proven = self.prefix_rax_proofs.iter().any(|&(_, op, prod)| op == p && prod == id);
            if matches!(self.tail_carry, Some((prod, ..)) if prod == id) && !proven {
                self.refuse_tail_carry("an inline call prefix joins after it");
            }
            self.patch_rel32_to_here(p);
        }
        // Interpreter round i1 wave 28, lane L6: after a real call of an
        // OSR-door compile, where every route of the node's arm (and a prefix's
        // hit edge, joined just above) has stored the result, the patchable
        // exit a class redefinition that withdraws this body forces. A `NOP`
        // (no register, no flag, no RCX write) until then.
        let site_from = self.buf.pos();
        self.maybe_emit_post_call_exit_site(id);
        // JIT round 25 item 1a: a tail carry started by this call survives the
        // site. The site is `NOP`s on the fall-through path; forced, it CALLs
        // the verdict tail, which preserves every GPR, and either leaves for
        // good (the deopt stub's image holds RAX, which is how the site's frame
        // state names the result) or jumps back to the site's end with RAX
        // intact -- the one branch `verify_tail_carry_points` lets reach a
        // tail carry's read (`post_call_exit_backs`).
        if let Some((prod, cons, pos)) = self.tail_carry {
            if prod == id && pos == site_from && self.buf.pos() != site_from {
                let to = self.buf.pos();
                self.tail_carry = Some((prod, cons, to));
                self.tail_carry_site_ends.push(to);
            }
        }
        // 2026-10-07: likewise after a static access whose helper may have run
        // `<clinit>` (item 1 of the i24-L6 page).
        let static_from = self.buf.pos();
        self.maybe_emit_post_static_helper_exit_site(id);
        // JIT round 26 (item 1a, the `LoadStatic` residual): a tail carry of a
        // `getstatic` survives that site exactly as one of a call survives the
        // call's: the site's frame state names the carried result as RAX (the
        // site sits at the elided store, `post_site_frame_state`), and its
        // stub's way back is one of `post_call_exit_backs`.
        if let Some((prod, cons, pos)) = self.tail_carry {
            if prod == id && pos == static_from && self.buf.pos() != static_from {
                let to = self.buf.pos();
                self.tail_carry = Some((prod, cons, to));
                self.tail_carry_site_ends.push(to);
            }
        }
        // W8-4 (`CRATONVM_JIT_IR_REG_ALLOC`): a site-named call or static
        // read's routes, recorded for the body-end branch scan.
        self.reg_alloc_check_routed_def(id, w8_prefixed);
        // A CARRIED value satisfies its dropped home the other way: it is left
        // in RAX or RCX for its one consumer rather than published into a
        // register of its own, so `cur_def_published` is false for it and
        // always will be. `live_carry` naming this node as the producer is the
        // carry having just started — the stale-carry case below is what
        // catches one that never gets read.
        //
        // Found by flipping these switches on together: the probe this was
        // built on has a frame state naming every intermediate, so no carried
        // value's home was ever dropped there and the two mechanisms never met.
        let started_carry = matches!(self.live_carry, Some((prod, _, _, _)) if prod == id)
            || matches!(self.deferred_rcx, Some((prod, _, _)) if prod == id)
            || matches!(self.tail_carry, Some((prod, _, _)) if prod == id);
        if self.home_dropped.get(id as usize).copied().unwrap_or(false)
            && !self.cur_def_published
            && !started_carry
            && !matches!(
                self.graph.nodes.get(id as usize).map(|n| &n.op),
                Some(Op::Phi)
            )
            // CG16P-4: a dropped constant is an immediate to every reader.
            && !self.is_dropped_const(id)
            // JIT rx27 wave 8c: an `And` its branch's or guard's `TEST`
            // answers is never computed, so it publishes nothing by design.
            && !self.fused_test_and.get(id as usize).copied().unwrap_or(false)
            && !self.fused_i2l.get(id as usize).copied().unwrap_or(false)
        {
            self.latch_bailout(Bailout::with_context(
                BailoutReason::UnallocatedValue { node: id },
                format!("n{id}'s home was dropped but its lowering published no register"),
            ));
        }
        // A carry that outlives the node that was supposed to read it is a
        // value with nowhere to be. `prod == id` is the carry this node just
        // STARTED, waiting for the next one.
        if let Some((prod, _, _, _)) = self.live_carry {
            if prod != id {
                self.refuse_carry("its consumer finished without reading it");
            }
        }
        // JIT round 25 item 1a: the same rule for a tail carry. It is read by
        // the very next node or the terminator, so any other node finishing
        // with one still in flight means the read never happened.
        if let Some((prod, _, _)) = self.tail_carry {
            if prod != id {
                self.refuse_tail_carry("its consumer finished without reading it");
            }
        }
        // The DEFERRED carry gets exactly one arm of grace, and that arm has to
        // be one that leaves RCX alone. Both halves are checked here rather
        // than trusted from the planner, because the planner reasons about the
        // SCHEDULE and this reasons about what was actually lowered — and it is
        // the second that the register image depends on.
        //
        // Three cases, in the order they occur: the producer that just started
        // it, the one arm in between, and the consumer that was supposed to
        // have read it.
        if let Some((prod, cons, allowance)) = self.deferred_rcx {
            if prod == id {
                // Just started. Nothing to spend yet.
            } else if id == cons {
                // The consumer finished without taking it out of RCX.
                self.refuse_deferred("its consumer finished without reading it");
            } else if self.rcx_writes != rcx_writes_before {
                // OBSERVED, not predicted. `note_rcx_written` has normally
                // refused already; this is the same fact asked of the node as a
                // whole, and it is what lets `node_preserves_rcx` widen past
                // the two ops whose arms mention no RCX at all.
                self.refuse_deferred("the arm in between wrote RCX");
            } else if !self.node_preserves_rcx(id) {
                self.refuse_deferred("an arm that can write RCX was lowered in between");
            } else if allowance == 0 {
                self.refuse_deferred("more than one arm came between it and its consumer");
            } else {
                self.deferred_rcx = Some((prod, cons, allowance - 1));
            }
        }
        self.cur_def = prev;
        self.cur_def_published = prev_published;
    }

    /// A deopt site of kind `why` read the flat snapshot at `bci`. `reads_table`: the site
    /// resumes through the by-bci TABLE point anchored there (an exception route, an OSR
    /// re-check), not through a frame of its own; only those make an undominated anchor
    /// matter (`undominated_anchor`). A guard's pad bakes its own boxed frame, whose
    /// definedness is `latch_if_site_frame_undefined`'s question.
    pub(in crate::ir_lower) fn note_flat_transfer(&mut self, bci: usize, why: &'static str, reads_table: bool) {
        self.by_bci_transfer_bcis.insert(bci);
        if reads_table {
            self.table_transfer_bcis.insert(bci);
        }
        if crate::ir_stage_reporting() && !self.by_bci_transfer_why.contains_key(&bci) {
            let op = self
                .cur_def
                .and_then(|d| self.graph.nodes.get(d as usize))
                .map(|n| format!("{:?}", n.op))
                .unwrap_or_default();
            self.by_bci_transfer_why.insert(bci, format!("{why}@{op}"));
        }
    }

    /// Record `here` as the `bci_native` anchor of `pc` if it is the earliest.
    ///
    /// Round 11 wave 6: a node that names its own snapshot AT ITS OWN bci
    /// anchors that snapshot through `snapshot_native` and does not also
    /// anchor the bci. LICM's pre-header frame is a node-named COPY at the
    /// hoisted node's bci; when the hoisted node also anchored the bci, the
    /// unnamed original landed on the same offset, sorted first and survived
    /// `build_deopt_points`' dedup in place of the corrected copy. An unnamed
    /// snapshot is now anchored only at an unnamed node of its bci, and a bci
    /// whose every node names its own snapshot keeps just those points. A
    /// node naming a snapshot at ANOTHER bci still anchors its own pc.
    pub(in crate::ir_lower) fn note_bci_anchor(&mut self, id: NodeId, pc: usize) {
        let graph = self.graph;
        let names_own_bci = graph
            .nodes
            .get(id as usize)
            .and_then(|node| node.frame_snapshot)
            .and_then(|si| graph.safepoints.get(si as usize))
            .is_some_and(|sp| sp.bci == pc);
        if names_own_bci {
            return;
        }
        // JIT round 23 wave 9 (lane W9-L7, `CRATONVM_JIT_IR_ANCHOR_TRAP_FREE_SKIP`):
        // a node that cannot deopt anchors nothing -- the scheduler's anchor
        // model (`ir_schedule`) made the same call when it placed the values
        // these frames name. A direct `getstatic` the schedule counted as
        // trap-free but this lowering does not read directly (it would take
        // the helper, which can exit) is a disagreement: refuse the compile.
        if ir_anchor_trap_free_skip_enabled() {
            if let Some(node) = graph.nodes.get(id as usize) {
                if !can_deopt(graph, node) {
                    if let Op::LoadStatic {
                        class_id,
                        field_index,
                        type_tag,
                        ..
                    } = node.op
                    {
                        if !node_cannot_deopt(graph, node)
                            && !self.load_static_reads_directly(class_id, field_index, type_tag)
                        {
                            self.latch_bailout(Bailout::with_context(
                                BailoutReason::UnsupportedShape(
                                    "ir_lower: a getstatic the schedule took as trap-free takes the helper",
                                ),
                                format!("n{id} at pc {pc}"),
                            ));
                        }
                    }
                    return;
                }
            }
        }
        let here = self.buf.pos();
        // JIT round 17 wave 3 (lane irlower; item 1 of
        // `r17-monitor2-by-bci-readers-that-stay-by-bci-FIXED-20261005.md`): path-
        // aware anchoring. With two copies of a bci on different paths (the
        // window pass's unnamed fast-arm clones, any clone a pass leaves
        // unnamed) the EARLIEST owner may sit on the path where the values the
        // by-bci snapshot names were never computed. An owner in a block that
        // every defining block of those values dominates wins over one that is
        // not; between two owners of the same standing, the earliest, as
        // before. When every owner is dominated -- every single-copy graph
        // whose anchor was honest -- this is the old rule exactly.
        if let Some(value_blocks) = self.bci_anchor_value_blocks.as_ref() {
            let schedule = self.schedule;
            let dominated = match (
                value_blocks.get(&pc),
                schedule.node_to_block.get(id as usize).copied(),
            ) {
                (None, _) => true,
                (Some(defs), Some(block)) if block < schedule.blocks.len() => {
                    defs.iter().all(|&d| schedule.dom.dominates(d, block))
                }
                // An owner with no block of its own: not provably honest.
                (Some(defs), _) => defs.is_empty(),
            };
            match (
                self.bci_native.get(&pc).copied(),
                self.bci_native_dominated.get(&pc).copied(),
            ) {
                (Some(e), Some(was)) => {
                    if (dominated && !was) || (dominated == was && here < e) {
                        self.bci_native.insert(pc, here);
                        self.bci_native_dominated.insert(pc, dominated);
                    }
                }
                _ => {
                    self.bci_native.insert(pc, here);
                    self.bci_native_dominated.insert(pc, dominated);
                }
            }
            return;
        }
        self.bci_native
            .entry(pc)
            .and_modify(|e| {
                if here < *e {
                    *e = here;
                }
            })
            .or_insert(here);
    }

}
