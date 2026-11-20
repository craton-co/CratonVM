#[allow(unused_imports)]
use super::*;

impl<'a> Lowerer<'a> {
    /// Item 4 residual 2: is entry block `e` of
    /// [`Self::field_proof_entry_blocks`] an ORDINARY OSR entry (a block with
    /// a bci whose stub seeds the locals and may run a re-check program) --
    /// not an opaque OSR arm, not an exception pad target?
    pub(in crate::ir_lower) fn field_proof_entry_is_rechecked_osr_header(&self, e: usize) -> bool {
        !self.call_throw_entry_blocks.contains(&e)
            && !self.block_is_opaque_osr_arm(e)
            && self.osr_entry_block_bci(e).is_some()
    }

    /// Epoch proofs only need `OsrCheckInsn::LayoutEpoch` at OSR entries, which
    /// requires no receiver local and is also supported on opaque OSR arms
    /// (`If(OsrEntryFlag)`).
    pub(in crate::ir_lower) fn epoch_proof_entry_is_rechecked_osr_header(&self, e: usize) -> bool {
        !self.call_throw_entry_blocks.contains(&e)
            && (self.block_is_opaque_osr_arm(e) || self.osr_entry_block_bci(e).is_some())
    }

    /// Item 4 residual 2: the JVM local the header snapshot of entry block
    /// `e` names `base` in, which its OSR stub seeds `base` from. `None`
    /// when no local names it (the stub could not re-read the receiver).
    pub(in crate::ir_lower) fn osr_entry_local_naming(&self, e: usize, base: NodeId) -> Option<usize> {
        let bci = self.osr_entry_block_bci(e)?;
        let sp = self
            .safepoint_first_by_bci
            .get(&bci)
            .and_then(|&idx| self.graph.safepoints.get(idx))?;
        sp.locals
            .iter()
            .position(|&v| v != NO_NODE && self.field_proof_canon(v) == base)
    }

    /// Is block `bi`'s control a projection of an `If` on an
    /// `Op::OsrEntryFlag` (an opaque OSR merge's arm or fall-through)?
    pub(in crate::ir_lower) fn block_is_opaque_osr_arm(&self, bi: usize) -> bool {
        self.schedule
            .blocks
            .get(bi)
            .and_then(|blk| self.graph.nodes.get(blk.ctrl as usize))
            .filter(|proj| matches!(proj.op, Op::Proj(_)))
            .and_then(|proj| proj.inputs.first())
            .and_then(|&iff| self.graph.nodes.get(iff as usize))
            .filter(|iff| iff.op == Op::If)
            .and_then(|iff| iff.inputs.get(1))
            .and_then(|&cond| self.graph.nodes.get(cond as usize))
            .is_some_and(|c| matches!(c.op, Op::OsrEntryFlag { .. }))
    }

    /// Item 4 residual 2: the re-check instructions the OSR stub of entry
    /// block `bi` owes the field proofs it jumps past
    /// ([`Self::field_proof_osr_obligations`]), with the JVM locals they
    /// read. `Ok(None)`: none owed. `Err`: the entry must be refused (a
    /// receiver no local of `locals` names).
    #[allow(clippy::type_complexity)]
    pub(in crate::ir_lower) fn osr_entry_field_proof_checks(
        &self,
        bi: usize,
        locals: &[NodeId],
    ) -> Result<Option<(Vec<OsrCheckInsn>, Vec<usize>)>, &'static str> {
        let obligations = self.field_proof_osr_obligations.borrow();
        let owed: Vec<(NodeId, bool)> = obligations
            .iter()
            .filter(|&&(e, _, _)| e == bi)
            .map(|&(_, base, compact)| (base, compact))
            .collect();
        if owed.is_empty() {
            return Ok(None);
        }
        let mut prog: Vec<OsrCheckInsn> = Vec::new();
        let mut leaves: Vec<usize> = Vec::new();
        let mut epoch = false;
        for (base, compact) in owed {
            if !compact {
                epoch = true;
                continue;
            }
            let Some(i) = locals
                .iter()
                .position(|&v| v != NO_NODE && self.field_proof_canon(v) == base)
            else {
                return Err("a pre-header field proof's receiver is named by no local here");
            };
            if !prog.contains(&OsrCheckInsn::Remat(OsrRematInsn::Local(i))) {
                prog.push(OsrCheckInsn::Remat(OsrRematInsn::Local(i)));
                prog.push(OsrCheckInsn::CompactReceiver);
                leaves.push(i);
            }
        }
        if epoch {
            prog.push(OsrCheckInsn::LayoutEpoch);
        }
        Ok(Some((prog, leaves)))
    }

    /// Wave 2, rule 3 of [`Self::field_proof_reaches`]: is `base` defined at
    /// most once per activation? A `Param` is; any other node is when its
    /// block lies on no CFG cycle and no exception entry can reach that block
    /// (a handler that loops back would re-run it).
    ///
    /// A LOOP-INVARIANT header φ qualifies too (wave 2 fix, 2026-10-24): the
    /// builder carries every local through each loop header as a φ -- `this`
    /// and a reference parameter included, and under an opaque OSR merge as
    /// `φ(param, OsrLocal)` -- so a receiver read inside or after a loop is
    /// a φ in a block ON a cycle, and the rule above refused every one of
    /// them (the build's failing `dominance_covers_a_loop_blocks_later_store`).
    /// Such a φ is assigned only when control crosses an edge whose input is
    /// not the φ itself; if every such edge leaves a block on no cycle, each
    /// is crossed at most once, and two of them cannot both be crossed in one
    /// activation (the second's block would be reachable from the header, so
    /// on a cycle through it). So the φ holds one value per activation when
    /// each non-self input arrives from a block on no cycle and itself runs
    /// once ([`Self::field_proof_value_runs_once`], depth-limited).
    pub(in crate::ir_lower) fn field_proof_base_runs_once(&self, base: NodeId) -> bool {
        self.field_proof_value_runs_once(base, 4)
    }

    /// [`Self::field_proof_base_runs_once`] with a recursion budget for the
    /// φ chain (a pre-header φ under a loop header φ, nested loops).
    pub(in crate::ir_lower) fn field_proof_value_runs_once(&self, v: NodeId, depth: u32) -> bool {
        let Some(n) = self.graph.nodes.get(v as usize) else {
            return false;
        };
        if matches!(n.op, Op::Param(_)) {
            return true;
        }
        let nblocks = self.schedule.blocks.len();
        // A φ lives in its merge's block, whether or not the scheduler
        // placed the φ node itself.
        let merge = if n.op == Op::Phi {
            n.inputs.first().copied()
        } else {
            None
        };
        let block = match merge {
            Some(m) => self.block_of_ctrl(m),
            None => self.schedule.node_to_block.get(v as usize).copied(),
        };
        let Some(d) = block.filter(|&d| d < nblocks) else {
            return false;
        };
        if self.field_proof_cycle_blocks().get(d).copied().unwrap_or(true) {
            let (Some(m), true) = (merge, depth > 0) else {
                return false;
            };
            let Some(merge_node) = self.graph.nodes.get(m as usize) else {
                return false;
            };
            // `phi.inputs = [merge, val_0, val_1, ...]`; `merge.inputs[k]` is
            // the control token for value k (`gather_phi_copies`).
            for (k, &val) in n.inputs.iter().enumerate().skip(1) {
                if val == v {
                    continue;
                }
                if val == NO_NODE {
                    return false;
                }
                let Some(pred) = merge_node
                    .inputs
                    .get(k - 1)
                    .and_then(|&ctrl_in| self.block_of_ctrl(ctrl_in))
                else {
                    return false;
                };
                if self.field_proof_cycle_blocks().get(pred).copied().unwrap_or(true) {
                    return false;
                }
                if !self.field_proof_value_runs_once(val, depth - 1) {
                    return false;
                }
            }
        }
        !self.call_throw_entry_blocks.iter().any(|&e| {
            Self::osr_entry_can_reach(self.schedule, e, None, d, true)
        })
    }

    /// Wave 2: `cycle[x]` -- block `x` can reach itself through its successors.
    pub(in crate::ir_lower) fn field_proof_cycle_blocks(&self) -> &[bool] {
        self.field_proof_cycle_blocks
            .get_or_init(|| blocks_on_a_cycle(self.schedule))
    }

    /// Wave 2, rule 2 of [`Self::field_proof_reaches`]: a superset of the
    /// blocks control can enter other than from the method entry along CFG
    /// edges. With `CRATONVM_JIT_IR_OSR_HEADER_ENTRIES_ONLY` (default ON) an
    /// ordinary OSR entry is planned only at a back-edge target
    /// ([`Self::osr_entry_block_is_back_edge_target`]); with it off, at any
    /// block whose control node carries a bci, so every such block is listed.
    /// Plus every block of an opaque OSR
    /// merge's `If` (`Op::OsrEntryFlag`, both projections, as
    /// `emit_osr_entry_stubs` collects them) and every call-site exception
    /// pad target (`call_throw_entry_blocks`, planned before any block is
    /// lowered).
    pub(in crate::ir_lower) fn field_proof_entry_blocks(&self) -> &[usize] {
        self.field_proof_entries.get_or_init(|| {
            let n = self.schedule.blocks.len();
            let headers_only = true;
            let mut out: Vec<usize> = Vec::new();
            for bi in 0..n {
                let opaque_arm = self.block_is_opaque_osr_arm(bi);
                // An ordinary entry needs a bci on the block's control node
                // (`emit_osr_entry_stubs` refuses one without: "block control
                // carries no bci").
                // A spliced callee's relocated pc is never a door bci
                // (`emit_osr_entry_stubs` refuses it as well).
                let ordinary = self
                    .osr_entry_block_bci(bi)
                    .is_some_and(|bci| !self.pc_is_in_a_spliced_body(bci))
                    && (!headers_only || self.osr_entry_block_is_back_edge_target(bi));
                if ordinary || opaque_arm || self.call_throw_entry_blocks.contains(&bi) {
                    out.push(bi);
                }
            }
            out
        })
    }

    /// Would [`Self::emit_field_layout_guard`] at this site emit a REJOINING
    /// compare (one whose failure takes the site's helper and comes back)?
    /// Asked before anything is emitted; decided by the same predicates the
    /// guard itself reads, so the two cannot disagree.
    pub(in crate::ir_lower) fn field_layout_guard_rejoins(&self, base: NodeId, pc: usize, may_deopt: bool) -> bool {
        let (addr, _) = cratonvm_types::layout_replace_epoch_guard();
        if addr.is_null() {
            // No epoch cell: no compare in any form.
            return false;
        }
        if !self.layout_guard_deopt_on {
            return true;
        }
        let Some(block) = self.field_proof_block() else {
            return true;
        };
        !(self.epoch_proof_holds(block) || self.field_site_may_trap(pc, may_deopt))
    }

    /// JIT round 25 (item 4 of the r24 redesign,
    /// `CRATONVM_JIT_IR_PROVEN_RECEIVER_FIELDS`): how an inline field access
    /// on `base` at `pc` treats the per-object compactness test. `None` keeps
    /// the full sequence (compact test, legacy arm, helper).
    ///
    /// * [`ProvenReceiverMode::Proven`] -- a deopting compactness guard on
    ///   `base` covers this site ([`Self::field_proof_holds`]): no compact
    ///   test, no legacy arm, no null test.
    /// * [`ProvenReceiverMode::Establish`] -- no proof yet, and this site may
    ///   put one down: `TEST GC_FLAG_COMPACT` that DEOPTS on a legacy object
    ///   (`DeoptReason::SpeculationFailed`) instead of reading its 16-byte
    ///   cell. Needs (1) a de-spec registry in scope that has not withdrawn the
    ///   speculation at `pc` (`Graph::proven_compact_withdrawn_pcs`: `None`
    ///   means no registry, so nothing could ever withdraw it, and the guard
    ///   is not planted -- the `ir_optimize::OptimizeContext` rule), (2) the
    ///   site may trap ([`Self::field_site_may_trap`]), and (3) NO rejoining
    ///   branch ahead of the guard -- the layout-epoch guard is deopting,
    ///   elided or absent ([`Self::field_layout_guard_rejoins`]). Only then
    ///   does every path that continues past the site carry the proof: the
    ///   null test's failure is the helper's NPE, which never continues.
    ///
    /// # What invalidates each fact (nothing, after it is established)
    ///
    /// * **Compactness** is fixed at allocation: `init_object_header` and the
    ///   compiled inline `new` stamp `GC_FLAG_COMPACT`, and every later header
    ///   write keeps it -- the G1 old-generation stamp
    ///   (`the_old_generation_stamp_preserves_every_other_header_flag`,
    ///   `gc/src/g1.rs`), the monitor inflation (`vm/src/threading/monitor.rs`,
    ///   "losing `GC_FLAG_COMPACT` turns a compact object ..."), and every
    ///   copying collector, which copies the header whole and never re-lays an
    ///   object. A node's value is one object per activation (SSA; the
    ///   loop-free rule covers the per-iteration case), and a relocation
    ///   rewrites the home word to the SAME object's new address, which
    ///   [`Self::gp_load_value`] reloads. So the proof survives calls,
    ///   safepoints and moves, and no call or safepoint drops it.
    /// * **The layout epoch**: [`Self::emit_field_layout_guard`]'s argument --
    ///   the epoch only grows, and a replacement is published after its bump,
    ///   so a passed deopting guard proves `base`'s layout for good.
    /// * **Containment** is not part of the proof. A primitive read on a
    ///   `Ref`-typed receiver needs none (the trusted-oop argument); a
    ///   reference read keeps [`Self::emit_read_bounds_live_check`], because
    ///   whether reference SLOTS hold plain pointers can change at any
    ///   safepoint (ZGC arming its read barrier clears the table).
    /// Round rx27 wave 5 (lane guards; `CRATONVM_DBG_PROVEN_FIELD_SITES=1`,
    /// diagnostic only): one line per inline field site that keeps the FULL
    /// sequence (per-object compact test, legacy arm, helper; and the
    /// layout-epoch compare unless a proof elides it), naming the receiver
    /// and the FIRST reason [`Self::proven_receiver_mode`] declined, in its
    /// own order. `kind` is `load` or `store`; `shape_ok` says whether the
    /// arm's own load/store preconditions (a guarded `Ref` receiver, not
    /// legacy-only or raw) held, without which the mode is never asked.
    /// Colored loads retain their decode or per-read color-flip barrier.
    /// Changes no emitted byte.
    pub(in crate::ir_lower) fn proven_field_site_census(
        &self,
        kind: &str,
        base: NodeId,
        pc: usize,
        may_deopt: bool,
        shape_ok: bool,
    ) {
        if !false {
            return;
        }
        let block = self.field_proof_block();
        let spliced = self.pc_is_in_a_spliced_body(pc);
        let reason = if !shape_ok {
            "arm-shape"
        } else if block.is_none() {
            "no-block"
        } else if self.graph.proven_compact_withdrawn_pcs.is_none() {
            "no-despec-registry"
        } else if self
            .graph
            .proven_compact_withdrawn_pcs
            .as_ref()
            .is_some_and(|pcs| pcs.contains(&pc))
        {
            "withdrawn"
        } else if !may_deopt {
            "may-not-deopt"
        } else if self.graph.method_self_lock.is_some() {
            "self-locking"
        } else if spliced && !(self.proven_spliced_on && self.field_site_chain_trap_ok(pc)) {
            "spliced-no-chain-trap"
        } else if !spliced && !self.flat_site_frame_located(pc) {
            "frame-not-located"
        } else if !self.field_site_may_trap(pc, may_deopt) {
            "pc-not-lone"
        } else if self.field_layout_guard_rejoins(base, pc, may_deopt) {
            "layout-guard-rejoins"
        } else {
            "other"
        };
        let recv = self.graph.nodes.get(base as usize);
        let op = recv.map_or_else(
            || "?".to_string(),
            |n| {
                let full = format!("{:?}", n.op);
                full.split(|c: char| c == ' ' || c == '(' || c == '{')
                    .next()
                    .unwrap_or("?")
                    .to_string()
            },
        );
        let recv_block = self.schedule.node_to_block.get(base as usize).copied();
        let site_block = block.unwrap_or(usize::MAX);
        let invariant_in_block = recv_block
            .is_some_and(|rb| rb != site_block && self.schedule.dom.dominates(rb, site_block));
        eprintln!(
            "[cratonvm-jitc] proven-field-site FULL {kind} method={} pc={pc} recv=n{base}:{op} \
             recv_block={recv_block:?} site_block={site_block} recv_dominates={invariant_in_block} \
             this={} spliced={spliced} reason={reason}",
            self.graph.scope_method_key,
            self.graph.receiver_param.is_some_and(|r| {
                self.graph
                    .nodes
                    .iter()
                    .position(|n| n.op == Op::Param(r))
                    .is_some_and(|first| first == base as usize)
            }),
        );
    }

    pub(in crate::ir_lower) fn proven_receiver_mode(
        &self,
        base: NodeId,
        pc: usize,
        may_deopt: bool,
    ) -> Option<ProvenReceiverMode> {
        let block = self.field_proof_block()?;
        if self.field_proof_holds(&self.compact_proven, base, block, true) {
            return Some(ProvenReceiverMode::Proven);
        }
        let withdrawn = match self.graph.proven_compact_withdrawn_pcs.as_ref() {
            Some(pcs) => pcs.contains(&pc),
            None => true,
        };
        if withdrawn
            || !self.field_site_may_trap(pc, may_deopt)
            || self.field_layout_guard_rejoins(base, pc, may_deopt)
        {
            return None;
        }
        Some(ProvenReceiverMode::Establish)
    }

    /// Item 4 wave 2 (under `CRATONVM_JIT_IR_PROVEN_RECEIVER_FIELDS`): may an
    /// inline PRIMITIVE `getfield` stay inline while the ZGC read gate is
    /// closed (`zgc_inline_field_loads_gated`), where it used to take the
    /// checked helper with every other shape?
    ///
    /// The gate exists for REFERENCE words: once ZGC colors its slots, a raw
    /// 8-byte reference load hands compiled code `Z_COLORED_TAG | colour |
    /// offset`. A primitive cell is never colored, and the receiver is a
    /// `Ref`-typed node -- an address the compiled code already holds and
    /// dereferences on the colored reference arm too. The trusted-oop arm
    /// never reads the read table the arming cleared, so the inline read is
    /// the helper's answer. Narrow oops stay excluded: the gate's other half,
    /// whose header and row shapes this arm does not claim to know.
    pub(in crate::ir_lower) fn prim_field_read_survives_the_read_gate(&self, node_ty: IrType, base: NodeId) -> bool {
        self.proven_fields_on
            && node_ty != IrType::Ref
            && !cratonvm_types::narrow_oop::narrow_oops_enabled()
            && !crate::x64::inline_getfield_enabled()
            && crate::x64::guarded_inline_getfield_enabled()
            && crate::x64::trusted_oop_receiver_getfield_enabled()
            && self
                .graph
                .nodes
                .get(base as usize)
                .is_some_and(|n| n.ty == IrType::Ref)
    }

    /// Item 4: the DEOPTING compactness guard of an `Establish` site, with the
    /// receiver in RAX (non-null): `TEST byte [RAX + flags], GC_FLAG_COMPACT;
    /// JZ <deopt at pc>`. Records the proof only when `record` (no rejoining
    /// branch preceded it). Touches the flags only.
    pub(in crate::ir_lower) fn emit_compact_receiver_trap(&mut self, base: NodeId, pc: usize, record: bool) {
        self.emit_compact_receiver_trap_at(base, pc, record, RAX);
    }

    pub(in crate::ir_lower) fn emit_compact_receiver_trap_at(&mut self, base: NodeId, pc: usize, record: bool, receiver: u8) {
        self.emit_field_compact_test(receiver);
        // ZF=0 exactly when the object is compact: continue on JNZ.
        self.emit_deopt_unless_as(0x85, pc, DeoptReason::SpeculationFailed, true);
        if self.pc_is_in_a_spliced_body(pc) {
            self.proven_field_census.establish_spliced += 1;
        }
        if !record {
            return;
        }
        if let Some(block) = self.field_proof_block() {
            self.compact_proven.push((base, block));
            if crate::ir_stage_reporting() {
                eprintln!("[ir] compact-receiver deopting n{base} at pc {pc} (block {block})");
            }
        }
    }

    /// JIT round 25 wave 6 (W6-E, `CRATONVM_JIT_IR_PROVEN_RECEIVER_FIELDS_COESTABLISH`):
    /// at an establishing `getfield` on `base` at `pc` whose compactness
    /// proof was just RECORDED, also prove the compiling method's receiver
    /// compact, with a second deopting test at the same bci:
    ///
    /// ```text
    ///   mov rax, <this> ; test rax, rax ; jz <deopt pc>
    ///   test byte [rax+flags], COMPACT ; jz <deopt pc>
    ///   [mov rax, <base>]               ; if the site reads through RAX
    /// ```
    ///
    /// The shape it serves is the regex node's `match(matcher, i, seq)`:
    /// `matcher.to` is read before the loop and `this.predicate` inside it, so
    /// `this`' first proof was put down by the loop body itself, once per
    /// iteration. Proving `this` here lets every `this.f` the block dominates
    /// read with one `MOV` (across a loop header with dev's `_PREHEADER`).
    ///
    /// Only when: the site is in the compiling method's own code (a spliced
    /// site's deopt would resume the callee); its block lies on no cycle (the
    /// test runs once per activation, not once per iteration); the method
    /// HAS a receiver (`Graph::receiver_param`) other than `base`, not
    /// already proven here; and a field site on the receiver lies in a block
    /// this one strictly dominates -- which also makes the receiver live here,
    /// so its location holds it.
    ///
    /// # Why it is sound
    ///
    /// The test sits where the site's own deopting guard sits: after its
    /// epoch guard and null test (whose failure is the helper's NPE, which
    /// never continues), before any load and before any slow edge that
    /// rejoins, at a bci whose deopt re-executes the `getfield` and nothing
    /// else (`field_site_may_trap` admitted the site). A failure therefore
    /// leaves exactly as the site's own guard would. The receiver of an
    /// instance method is never null (the JVM null-checks it at every call
    /// site); the null test is defence, not a speculation. Its layout proof
    /// rides on the site's: a deopting epoch guard that passed after `base`
    /// was defined passed after the receiver (a parameter) was, and the
    /// layout fact is about the epoch, not about one object
    /// ([`Self::emit_field_layout_guard`]).
    ///
    /// The failure is charged to the site's `(pc, SpeculationFailed)`, so a
    /// legacy receiver that keeps arriving withdraws the site's own guard,
    /// and with it this one.
    pub(in crate::ir_lower) fn coestablish_receiver_proof(&mut self, base: NodeId, pc: usize, receiver: u8) {
        if self.pc_is_in_a_spliced_body(pc) {
            return;
        }
        let Some(block) = self.field_proof_block() else {
            return;
        };
        if !self.compact_proven.contains(&(base, block)) {
            return;
        }
        let on_cycle = self
            .field_proof_cycle_blocks()
            .get(block)
            .copied()
            .unwrap_or(true);
        if on_cycle {
            return;
        }
        let Some(recv) = self.receiver_param_node() else {
            return;
        };
        if recv == base || self.graph.nodes.get(recv as usize).map(|n| n.ty) != Some(IrType::Ref) {
            return;
        }
        if self.field_proof_holds(&self.compact_proven, recv, block, true) {
            return;
        }
        if !self.field_site_in_a_dominated_block(recv, block) {
            return;
        }
        self.gp_load_value(RAX, recv);
        // TEST RAX, RAX -- ZF=0 exactly when non-null: continue on JNZ.
        self.buf.emit(&[0x48, 0x85, 0xC0]);
        self.emit_deopt_unless(0x85, pc, DeoptReason::SpeculationFailed);
        // TEST byte [RAX + disp32], imm8 -- ZF=0 exactly when the object is
        // compact: continue on JNZ.
        self.buf.emit(&[0xF6, 0x80]);
        self.buf
            .emit(&(cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32).to_le_bytes());
        self.buf.emit_byte(cratonvm_types::GC_FLAG_COMPACT);
        self.emit_deopt_unless(0x85, pc, DeoptReason::SpeculationFailed);
        self.compact_proven.push((recv, block));
        if self.field_proof_holds(&self.layout_proven, base, block, false) {
            self.layout_proven.push((recv, block));
        }
        self.proven_field_census.coestablished += 1;
        if crate::ir_stage_reporting() {
            eprintln!("[ir] compact-receiver co-established n{recv} beside n{base} at pc {pc} (block {block})");
        }
        if receiver == RAX {
            self.gp_load_value(RAX, base);
        }
    }

    /// W6-E: the compiling method's receiver parameter node, if it has one.
    pub(in crate::ir_lower) fn receiver_param_node(&self) -> Option<NodeId> {
        let recv = self.graph.receiver_param?;
        self.graph
            .nodes
            .iter()
            .position(|n| n.op == Op::Param(recv))
            // Cast: node ids index `graph.nodes`, which the builder bounds.
            .map(|i| i as NodeId)
    }

    /// W6-E: is there a field `Op::Load` / `Op::Store` on `v`, other than the
    /// node being lowered, in a reachable block `block` strictly dominates?
    /// The loaded twin of [`Self::coestablish_receiver_proof`]: right after a
    /// reference `getfield` whose value is read again inside a loop that does
    /// not contain this load (`find-ci`'s `this.next`, whose `Branch.size` is
    /// the `atoms` loop's header read), prove the loaded object once here --
    /// non-null, compact, the layout epoch -- so those reads are `Proven`
    /// instead of re-running their own establish test on every trip.
    ///
    /// Each test deopts at this site's pc, which re-executes the `getfield`
    /// and nothing else (`field_site_may_trap`); the load committed nothing.
    /// A null or legacy value is a failed speculation charged to that pc, so a
    /// site that keeps seeing one is withdrawn as the site's own guard is. The
    /// epoch compare runs after the load, so the object it covers existed
    /// before the epoch read ([`Self::emit_field_layout_guard`]'s argument).
    /// Own code only, as the receiver twin.
    pub(in crate::ir_lower) fn coestablish_loaded_reference(&mut self, pc: usize) {
        let (Some(value), Some(block)) = (self.cur_def, self.field_proof_block()) else {
            return;
        };
        let admitted = self
            .graph
            .proven_compact_withdrawn_pcs
            .as_ref()
            .is_some_and(|w| !w.contains(&pc));
        if !self.proven_coestablish_on
            || !admitted
            || self.pc_is_in_a_spliced_body(pc)
            || !self.field_site_may_trap(pc, true)
            || !self.value_reread_in_a_loop_without(value, block)
        {
            return;
        }
        self.gp_load_value(RAX, value);
        // TEST RAX, RAX -- ZF=0 exactly when non-null: continue on JNZ.
        self.buf.emit(&[0x48, 0x85, 0xC0]);
        self.emit_deopt_unless(0x85, pc, DeoptReason::SpeculationFailed);
        // TEST byte [RAX + disp32], imm8 -- ZF=0 exactly when compact.
        self.buf.emit(&[0xF6, 0x80]);
        self.buf
            .emit(&(cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32).to_le_bytes());
        self.buf.emit_byte(cratonvm_types::GC_FLAG_COMPACT);
        self.emit_deopt_unless(0x85, pc, DeoptReason::SpeculationFailed);
        self.compact_proven.push((value, block));
        // A dominating VM-wide epoch guard (block 0 / a pre-header) already
        // covers every field site; skip the per-definition compare when the
        // value has no other kind of reader that would want a value-keyed proof.
        if self.layout_guard_deopt_on
            && !self.coestablish_epoch_covered(value, block)
            && self.emit_layout_epoch_cmp()
        {
            // ZF=1 exactly when the epoch still matches: continue on JE.
            self.emit_deopt_unless(0x84, pc, DeoptReason::ClassLoading);
            self.layout_proven.push((value, block));
        }
        self.proven_field_census.coestablished += 1;
    }

    /// Is a field of `v` read or written in a block `block` strictly
    /// dominates that lies on a cycle avoiding `block`? Such a site runs more
    /// often than `v` is defined, which is when a test at the definition pays.
    pub(in crate::ir_lower) fn value_reread_in_a_loop_without(&self, v: NodeId, block: usize) -> bool {
        let dom = &self.schedule.dom;
        let blocks = &self.schedule.blocks;
        let n = blocks.len();
        self.graph.nodes.iter().enumerate().any(|(id, node)| {
            // Cast: an index into the node arena is a `NodeId`.
            if !matches!(node.op, Op::Load(_) | Op::Store(_)) || self.cur_def == Some(id as NodeId) {
                return false;
            }
            if node.inputs.get(2).is_none_or(|&b| self.field_proof_canon(b) != v) {
                return false;
            }
            let Some(s) = self.schedule.node_to_block.get(id).copied().filter(|&s| {
                s < n && s != block && dom.is_reachable(s) && dom.dominates(block, s)
            }) else {
                return false;
            };
            let mut seen = vec![false; n];
            let mut work: Vec<usize> = blocks[s].successors.clone();
            while let Some(x) = work.pop() {
                if x == s {
                    return true;
                }
                if x >= n || x == block || seen[x] {
                    continue;
                }
                seen[x] = true;
                work.extend(blocks[x].successors.iter().copied());
            }
            false
        })
    }

    pub(in crate::ir_lower) fn field_site_in_a_dominated_block(&self, v: NodeId, block: usize) -> bool {
        let dom = &self.schedule.dom;
        let nblocks = self.schedule.blocks.len();
        self.graph.nodes.iter().enumerate().any(|(id, n)| {
            matches!(n.op, Op::Load(_) | Op::Store(_))
                && n.inputs.get(2).copied() == Some(v)
                // Cast: an index into the node arena is a `NodeId`.
                && self.cur_def != Some(id as NodeId)
                && self
                    .schedule
                    .node_to_block
                    .get(id)
                    .copied()
                    .is_some_and(|c| {
                        c < nblocks && c != block && dom.is_reachable(c) && dom.dominates(block, c)
                    })
        })
    }

    /// `MOV dst32/dst64, [base + disp32]` for any GPR pair (a SIB byte for an
    /// RSP/R12 base). Used by the inline `ArrayList` prefix.
    pub(in crate::ir_lower) fn emit_load_base_disp32(&mut self, dst: u8, base: u8, disp: i32, wide: bool) {
        if dst == RCX {
            self.note_rcx_written();
        }
        let rex = 0x40u8 | ((wide as u8) << 3) | (((dst >= 8) as u8) << 2) | ((base >= 8) as u8);
        if rex != 0x40 {
            self.buf.emit_byte(rex);
        }
        self.buf.emit_byte(0x8B);
        self.buf.emit_byte(0x80 | ((dst & 7) << 3) | (base & 7));
        if base & 7 == 4 {
            self.buf.emit_byte(0x24); // SIB: base only
        }
        self.buf.emit(&disp.to_le_bytes());
    }

    /// NIO19-2 (nio20 wave 1, lane L2): an `Unsafe` primitive accessor over a
    /// heap PRIMITIVE array, answered inline as a guarded PREFIX of the
    /// `Op::Call`'s unchanged lowering -- one load or store where the site used
    /// to CALL `jit_unsafe_get_int_direct` & co (or the funnel, for `getChar`
    /// and the `*Unaligned` bodies). Sites: `direct_helpers::unsafe_inline_access_site`.
    ///
    /// Same contract with the `Op::Call` arm as
    /// [`Self::try_emit_ir_arraylist_access_prefix`]: scratch RAX, RCX, RDX,
    /// R10 and R11 only; the hit edge writes a get's result exactly as the call
    /// route's `store_rax` would and jumps to [`Self::call_prefix_join`]; no
    /// GC point, no call, no deopt on the hit path.
    ///
    /// # What it answers (exactly the thin helpers' heap arm)
    ///
    /// `vm::jit::helpers::raw_accessor_read_bytes` / `_write_bytes` for a
    /// non-null base: the base's header says ARRAY with a PRIMITIVE element type
    /// (any of the eight -- a mismatched width is a raw little-endian copy, as
    /// on HotSpot), `offset >= ARRAY_DATA_OFFSET` (signed), and
    /// `offset - ARRAY_DATA_OFFSET + width <= length << log2(element size)`. No
    /// alignment is required (x86-64 has none, and the helper asked none). A
    /// big-endian `*Unaligned` form swaps the bytes. Everything else DECLINES to
    /// the call: a null receiver (the call raises the NPE), a null base (an
    /// off-heap address or an `Unsafe` arena handle), a plain object (a field or
    /// static offset), a reference array, an out-of-range offset.
    ///
    /// No GC barrier: the access reads or writes primitive bytes only, through
    /// the same object pointer the optimizing tier's own `ArrayLoad`/`ArrayStore`
    /// address primitive elements with.
    ///
    /// # Declines at compile time
    ///
    /// No VM admission bit for the accessor (`DirectHelperTable::unsafe_inline_admitted`,
    /// filled from the per-call policy the inline code skips),
    /// `CRATONVM_JIT_UNSAFE_ACCESSOR_INLINE=0` or `CRATONVM_JIT_UNSAFE_ACCESSOR_DIRECT=0`,
    /// a shape whose arity or result type does not match, an operand some carry
    /// is holding, and a get result that is carried or has its home dropped.
    pub(in crate::ir_lower) fn try_emit_ir_unsafe_access_prefix(&mut self, id: NodeId, info_ptr: usize) {
        if info_ptr == 0 || self.direct_helpers.unsafe_inline_admitted == 0 {
            return;
        }
        // SAFETY: as every other read of an `Op::Call`'s `info_ptr` in this
        // file -- a live `JitInvokeInfo` owned by `ir_call_infos`.
        let info: &JitInvokeInfo = unsafe { &*(info_ptr as *const JitInvokeInfo) };
        let Some(access) = crate::direct_helpers::unsafe_inline_access_site(
            info.class_name,
            info.method_name,
            info.descriptor,
        ) else {
            // nio21 wave 5 L3: the CAS / getAndAdd / getAndSet rows.
            if let Some(rmw) = crate::direct_helpers::unsafe_inline_rmw_site(
                info.class_name,
                info.method_name,
                info.descriptor,
            ) {
                self.try_emit_ir_unsafe_rmw_prefix(id, info, rmw);
            } else if crate::direct_helpers::unsafe_inline_ref_cas_site(
                info.class_name,
                info.method_name,
                info.descriptor,
            ) {
                // JIT round 20 wave 12, lane qUnsafe (AT-4).
                self.try_emit_ir_unsafe_ref_cas_prefix(id, info);
            } else if let Some(rv) = crate::direct_helpers::unsafe_inline_ref_volatile_site(
                info.class_name,
                info.method_name,
                info.descriptor,
            ) {
                // JIT round 21 wave 1, lane updaters (UPDATERS21-1).
                self.try_emit_ir_unsafe_ref_volatile_prefix(id, info, rv);
            } else if let Some(rmw) = crate::direct_helpers::unsafe_inline_subword_site(
                info.class_name,
                info.method_name,
                info.descriptor,
            ) {
                // JIT round 21, lane atomics (UNSAFE21-2).
                self.try_emit_ir_unsafe_subword_prefix(id, info, rmw);
            }
            return;
        };
        // Instance methods of the final `Unsafe`: virtual (0) or the
        // devirtualised special (1); the receiver is input 2 either way.
        if !matches!(info.invoke_kind, 0 | 1)
            || !access.admitted(self.direct_helpers.unsafe_inline_admitted)
            || !crate::direct_helpers::unsafe_accessor_inline_enabled()
            // nio20 w3 L3 (W2L2-4): this tier's half only.
            || !crate::ir_thin_prefix_enabled()
        {
            return;
        }
        let graph = self.graph;
        let Some(node) = graph.nodes.get(id as usize) else {
            return;
        };
        // [ctrl, mem, receiver, base, offset, (value), (bigEndian)]
        if node.inputs.len() != 3 + access.num_params() {
            return;
        }
        let want_ty = if access.is_put {
            IrType::Void
        } else if access.value_kind == b'F' {
            // JIT round 20 wave 12, lane qUnsafe (L2-2): an FP result is its
            // raw bits in RAX stored to the home word, exactly what the call
            // route's `store_rax(slot)` leaves (it publishes no FP register).
            IrType::Float
        } else if access.value_kind == b'D' {
            IrType::Double
        } else if access.width == 8 {
            IrType::Long
        } else {
            IrType::Int
        };
        if node.ty != want_ty {
            return;
        }
        if node.inputs[2..].iter().any(|&i| self.carry_names(i)) {
            return;
        }
        let recv = node.inputs[2];
        let base = node.inputs[3];
        let offset = node.inputs[4];
        let value = if access.is_put {
            Some(node.inputs[5])
        } else {
            None
        };
        let big_endian = if access.big_endian_operand {
            Some(node.inputs[node.inputs.len() - 1])
        } else {
            None
        };
        // A get's result home, computed without allocating it (see the
        // ArrayList prefix): the call route allocates it after its map.
        let slot = if access.is_put {
            None
        } else {
            let Some(s) = self.prefix_result_slot(id, node.ty) else {
                return;
            };
            s
        };
        // A constant `bigEndian` (the common `ByteOrder` shapes after
        // inlining) decides the swap here; otherwise it is tested at run time.
        let big_endian_const: Option<bool> =
            big_endian.and_then(|b| match graph.nodes.get(b as usize).map(|n| &n.op) {
                Some(Op::Const(v)) => Some(*v as i32 != 0),
                _ => None,
            });
        if crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] ir-unsafe-inline n{id} {}.{}{} width={} put={} be={:?}",
                info.class_name,
                info.method_name,
                info.descriptor,
                access.width,
                access.is_put,
                big_endian.map(|_| big_endian_const),
            );
        }
        let w = access.width;

        let mut decline: Vec<usize> = Vec::new();
        // RCX: the tag byte, then the scale, then a runtime `bigEndian`.
        self.note_rcx_written();
        // R10 = the `Unsafe` receiver; null declines (the call's NPE).
        self.gp_load_value(R10, recv);
        self.buf.emit(&[0x4D, 0x85, 0xD2]); // TEST R10, R10
        decline.push(self.emit_jcc_rel32(0x84)); // JZ
                                                 // RAX = base; null (an off-heap address or arena handle) declines --
                                                 // or, since nio21 wave 4 L3 (`ir_unsafe_offheap_prefix_enabled`), takes
                                                 // the off-heap arm emitted after the heap one.
        self.gp_load_value(RAX, base);
        self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
        let to_offheap = self.emit_jcc_rel32(0x84); // JZ
                                                    // Not for a volatile access (nio21 wave 4 L4's arm): the off-heap
                                                    // arm has neither its alignment test nor its locked store.
                                                    // JIT round 20 wave 12, lane qUnsafe (L2-2): nor for a boolean / float /
                                                    // double row (`UnsafeInlineAccess::heap_only`).
        let offheap = ir_unsafe_offheap_prefix_enabled() && !access.volatile && !access.heap_only();
        if !offheap {
            decline.push(to_offheap);
        }
        // JIT round 20 wave 11, lane pAtom (W7-L3-1 / AT-3): a volatile
        // int/long access to a non-array base takes the instance-field arm
        // (emitted after the hit path) when this VM baked its cell table;
        // without one it declines in the array screen's own kind test.
        let field_arm =
            crate::direct_helpers::unsafe_volatile_field_arm_words(&self.direct_helpers, access);
        // t1 wave 9, lane ATOM2 (ATOMICS21-1, opt-in): and a static-band
        // offset on its class's mirror takes the static arm first.
        let static_arm = unsafe_volatile_static_arm_table(&self.direct_helpers, access);
        let to_field = (field_arm.is_some() || static_arm.is_some()).then(|| {
            let (bytes, edge) = crate::direct_helpers::unsafe_inline_not_array_bytes();
            let at = self.buf.pos();
            self.buf.emit(&bytes);
            at + edge
        });
        self.emit_ir_unsafe_prim_array_screen(offset, w, &mut decline);
        if access.volatile {
            // nio21 wave 4 L4: a volatile access is served only naturally
            // aligned (a misaligned one is the natives' raw, non-atomic arm).
            // The payload starts 8-aligned at +16, so an aligned offset is an
            // aligned address. TEST R11B, w-1 ; JNZ decline.
            self.buf.emit(&[0x41, 0xF6, 0xC3, w - 1]);
            decline.push(self.emit_jcc_rel32(0x85));
        }
        // RAX = the base, R11 = the array offset or (from the field arm) the
        // published compact displacement, which is naturally aligned. The
        // field arm joins here.
        let field_join = self.buf.pos();

        if let Some(value) = value {
            // RDX = the value; its low `w` bytes are stored.
            self.gp_load_value(RDX, value);
            self.emit_unsafe_inline_swap(w, big_endian, big_endian_const, RDX);
            if access.value_kind == b'Z' {
                // JIT r20 lane rev12: `putBoolean` stores `x & 1`, as HotSpot
                // (and `native_unsafe_put_boolean_mb`) do. AND EDX, 1.
                self.buf
                    .emit(&crate::direct_helpers::UNSAFE_BOOLEAN_NORMALIZE_EDX);
            }
            // MOV [RAX + R11], DL / DX / EDX / RDX (SIB: base RAX, index R11);
            // a volatile put is XCHG [RAX + R11], EDX / RDX (an implicitly
            // locked store: the StoreLoad a volatile store owes). JIT round 20
            // wave 12, lane qUnsafe (L2-2): a release / opaque row
            // (`ordered_store`) is the plain `MOV`.
            match (w, access.volatile && !access.ordered_store) {
                (4, true) => self.buf.emit(&[0x42, 0x87, 0x14, 0x18]),
                (8, true) => self.buf.emit(&[0x4A, 0x87, 0x14, 0x18]),
                (1, _) => self.buf.emit(&[0x42, 0x88, 0x14, 0x18]),
                (2, _) => self.buf.emit(&[0x66, 0x42, 0x89, 0x14, 0x18]),
                (4, _) => self.buf.emit(&[0x42, 0x89, 0x14, 0x18]),
                _ => self.buf.emit(&[0x4A, 0x89, 0x14, 0x18]),
            }
        } else {
            // The raw bytes into RAX (SIB: base RAX, index R11).
            match w {
                1 => self.buf.emit(&[0x4A, 0x0F, 0xBE, 0x04, 0x18]), // MOVSX RAX, BYTE
                2 => self.buf.emit(&[0x42, 0x0F, 0xB7, 0x04, 0x18]), // MOVZX EAX, WORD
                4 => self.buf.emit(&[0x42, 0x8B, 0x04, 0x18]),       // MOV EAX, DWORD
                _ => self.buf.emit(&[0x4A, 0x8B, 0x04, 0x18]),       // MOV RAX, QWORD
            }
            self.emit_unsafe_inline_swap(w, big_endian, big_endian_const, RAX);
            // Widen the way the helper's `i64::from(..)` did: `short`/`int`
            // sign-extend, `char` zero-extends. L2-2: a `float` keeps the
            // zero-extended bits of its `MOV EAX`; a `boolean` is normalised
            // to `0`/`1` (`native_unsafe_get_boolean_mb`, HotSpot).
            match w {
                2 if access.signed => self.buf.emit(&[0x48, 0x0F, 0xBF, 0xC0]), // MOVSX RAX, AX
                2 => self.buf.emit(&[0x0F, 0xB7, 0xC0]),                        // MOVZX EAX, AX
                4 if access.is_fp() => {}
                4 => self.buf.emit(&[0x48, 0x63, 0xC0]), // MOVSXD RAX, EAX
                _ => {}
            }
            if access.value_kind == b'Z' {
                self.buf
                    .emit(&crate::direct_helpers::UNSAFE_BOOLEAN_NORMALIZE_RAX);
            }
            if !access.is_put {
                self.emit_prefix_result_store(id, slot);
            }
        }
        let hit = self.emit_jmp_rel32();
        if let Some(to_field) = to_field {
            self.patch_rel32_to_here(to_field);
            self.gp_load_value(R11, offset);
            // t1 wave 9, lane ATOM2 (ATOMICS21-1): a static-band offset on its
            // class's mirror leaves RAX = the statics block and R11 = the
            // static's payload displacement (naturally aligned), and joins the
            // same MOV / XCHG at [RAX + R11]; any other offset falls through
            // to the instance-field screen (or declines without one) with R11
            // still the offset and RAX still the base.
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
                    eprintln!("[cratonvm-jitc] ir-unsafe-inline n{id} volatile static arm");
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
                    eprintln!("[cratonvm-jitc] ir-unsafe-inline n{id} volatile field arm");
                }
            }
        }
        if offheap {
            // nio21 wave 4 L3 (proposal W3L3-2): a null base, the single-pass
            // prefix's off-heap arm (`x64::op_invoke` `emit_sp_offheap_address`)
            // in this tier. The helper's `raw_accessor_{read,write}_bytes` with
            // `obj == 0`: a positive untagged address is a real pointer; a
            // tagged one is an arena handle, served inline only when it
            // resolves to a live slotted block holding the whole access.
            // Everything else declines to the unchanged call.
            self.patch_rel32_to_here(to_offheap);
            self.gp_load_value(R11, offset);
            let table = scoped_arena_jit_table(&self.direct_helpers);
            let (bytes, edges) = unsafe_offheap_address_decline_bytes(table, w);
            let at = self.buf.pos();
            self.buf.emit(&bytes);
            decline.extend(edges.into_iter().map(|e| at + e));
            // RDX = the real address. RAX carries the value or the result
            // (RDX is not free for the value, as it is in the heap arm).
            if let Some(value) = value {
                self.gp_load_value(RAX, value);
                self.emit_unsafe_inline_swap(w, big_endian, big_endian_const, RAX);
                match w {
                    1 => self.buf.emit(&[0x88, 0x02]),       // MOV [RDX], AL
                    2 => self.buf.emit(&[0x66, 0x89, 0x02]), // MOV [RDX], AX
                    4 => self.buf.emit(&[0x89, 0x02]),       // MOV [RDX], EAX
                    _ => self.buf.emit(&[0x48, 0x89, 0x02]), // MOV [RDX], RAX
                }
            } else {
                match w {
                    1 => self.buf.emit(&[0x48, 0x0F, 0xBE, 0x02]), // MOVSX RAX, BYTE [RDX]
                    2 => self.buf.emit(&[0x0F, 0xB7, 0x02]),       // MOVZX EAX, WORD [RDX]
                    4 => self.buf.emit(&[0x8B, 0x02]),             // MOV EAX, [RDX]
                    _ => self.buf.emit(&[0x48, 0x8B, 0x02]),       // MOV RAX, [RDX]
                }
                self.emit_unsafe_inline_swap(w, big_endian, big_endian_const, RAX);
                match w {
                    2 if access.signed => self.buf.emit(&[0x48, 0x0F, 0xBF, 0xC0]), // MOVSX RAX, AX
                    2 => self.buf.emit(&[0x0F, 0xB7, 0xC0]),                        // MOVZX EAX, AX
                    4 => self.buf.emit(&[0x48, 0x63, 0xC0]), // MOVSXD RAX, EAX
                    _ => {}
                }
                if !access.is_put {
                    self.emit_prefix_result_store(id, slot);
                }
            }
            // Join the heap arm's hit `JMP` (one hit edge for the caller).
            self.emit_jmp_back_to(hit - 1);
            if crate::x64::dbg_jitc_enabled() {
                eprintln!(
                    "[cratonvm-jitc] ir-unsafe-inline n{id} off-heap arm (arena table: {})",
                    table.is_some()
                );
            }
        }
        for p in decline {
            self.patch_rel32_to_here(p);
        }
        self.call_prefix_join = Some(hit);
    }

    /// The heap-array screen of [`Self::try_emit_ir_unsafe_access_prefix`] and
    /// [`Self::try_emit_ir_unsafe_rmw_prefix`] (moved out unchanged by nio21
    /// wave 5 L3; the single-pass twin is `x64::op_invoke`'s
    /// `emit_sp_prim_array_screen`): RAX = a non-null base. Loads R11 =
    /// `offset` (a `long`) and leaves RAX = the base, R11 = the offset, or
    /// jumps to a `decline` edge unless the base is a one-dimensional
    /// PRIMITIVE array whose payload holds `[offset - 16, offset - 16 + w)`.
    /// Writes RCX, RDX, R10, R11.
    pub(in crate::ir_lower) fn emit_ir_unsafe_prim_array_screen(
        &mut self,
        offset: NodeId,
        w: u8,
        decline: &mut Vec<usize>,
    ) {
        // `offset` is a JAVA offset: `Unsafe.arrayBaseOffset` (16, the
        // helpers' `ABASE`) plus the byte index, so `base + offset` is the
        // byte's address only while the payload starts there.
        const _: () = assert!(
            cratonvm_types::ARRAY_DATA_OFFSET == 16,
            "the Unsafe inline prefix assumes Unsafe.arrayBaseOffset (16) == ARRAY_DATA_OFFSET"
        );
        const KIND_TAGS: u8 = cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8; // Cast: x86-64 disp8
                                                                           // The checked narrowing: a length offset past 127 fails the build
                                                                           // instead of encoding a negative disp8.
        const LEN: u8 =
            crate::x64::disp::disp8_const(cratonvm_types::ARRAY_LENGTH_OFFSET as i64) as u8;
        const ARRAY_KIND: u8 = cratonvm_types::ObjectKind::Array as u8;
        // The primitive element tags are 4 (`Boolean`) ..= 11 (`Long`), so the
        // whole tag byte of a primitive array is 0x11 ..= 0x2D, and
        // `element_type & 3` is log2 of its element size (pinned by
        // `direct_helpers`' `element_tag_low_bits_are_log2_of_the_element_size`).
        const PRIM_LO: u8 = ARRAY_KIND | ((cratonvm_types::ArrayElementType::Boolean as u8) << 2);
        const PRIM_HI: u8 = ARRAY_KIND | ((cratonvm_types::ArrayElementType::Long as u8) << 2);
        const _: () = assert!(PRIM_LO == 0x11 && PRIM_HI == 0x2D);
        const _: () = assert!(cratonvm_types::KIND_TAGS_BYTE_OFFSET <= 127);
        // RCX: the tag byte, then the scale.
        self.note_rcx_written();
        // ECX = the kind/element tag byte; EDX = its kind bits.
        self.buf.emit(&[0x0F, 0xB6, 0x48, KIND_TAGS]); // MOVZX ECX, BYTE [RAX+KIND_TAGS]
        self.buf.emit(&[0x89, 0xCA]); // MOV EDX, ECX
        self.buf
            .emit(&[0x83, 0xE2, cratonvm_types::KIND_TAG_BYTE_MASK]); // AND EDX, kind mask
        self.buf.emit(&[0x83, 0xFA, ARRAY_KIND]); // CMP EDX, Array
        decline.push(self.emit_jcc_rel32(0x85)); // JNE: not an array
        self.buf.emit(&[0x83, 0xF9, PRIM_LO]); // CMP ECX, PRIM_LO
        decline.push(self.emit_jcc_rel32(0x82)); // JB: reference array
        self.buf.emit(&[0x83, 0xF9, PRIM_HI]); // CMP ECX, PRIM_HI
        decline.push(self.emit_jcc_rel32(0x87)); // JA: no such element type
        self.buf.emit(&[0xC1, 0xE9, 0x02]); // SHR ECX, 2
        self.buf.emit(&[0x83, 0xE1, 0x03]); // AND ECX, 3 (log2 element size)
                                            // RDX = payload bytes = length << log2 (a u32 shifted by <= 3: no wrap).
        self.buf.emit(&[0x8B, 0x50, LEN]); // MOV EDX, [RAX+LEN] (zero-extends)
        self.buf.emit(&[0x48, 0xD3, 0xE2]); // SHL RDX, CL
                                            // R11 = offset (a `long`).
        self.gp_load_value(R11, offset);
        // CMP R11, ARRAY_DATA_OFFSET ; JL decline (signed: below the payload).
        self.buf
            .emit(&[0x49, 0x83, 0xFB, cratonvm_types::ARRAY_DATA_OFFSET as u8]); // Cast: 16
        decline.push(self.emit_jcc_rel32(0x8C));
        // R10 = offset - ARRAY_DATA_OFFSET + width, the access's END in the
        // payload: positive here, and no wrap (offset < 2^63).
        let end_disp = (i32::from(w) - cratonvm_types::ARRAY_DATA_OFFSET as i32) as i8; // Cast: -15..=-8
        self.buf.emit(&[0x4D, 0x8D, 0x53, end_disp as u8]); // LEA R10, [R11 + disp8]
        self.buf.emit(&[0x49, 0x39, 0xD2]); // CMP R10, RDX
        decline.push(self.emit_jcc_rel32(0x87)); // JA (unsigned): past the payload
    }

    /// nio21 wave 5 L3 (`r21-unsafe-volatile-accessors-are-thin-calls-not-inline-and-getandadd-is-not-one-xadd-FIXED-20261012.md`):
    /// an `Unsafe` CAS / getAndAdd / getAndSet on an aligned primitive-array
    /// element as ONE locked instruction (`LOCK CMPXCHG` / `LOCK XADD` /
    /// `XCHG`), a guarded prefix of the `Op::Call`'s unchanged lowering with
    /// the contract of [`Self::try_emit_ir_unsafe_access_prefix`] (scratch
    /// RAX, RCX, RDX, R10, R11; no GC point, no call, no deopt on the hit
    /// path). Sites and the served set: `direct_helpers::unsafe_inline_rmw_site`
    /// and the section comment above it. A null receiver or base, a reference
    /// array, an out-of-range or misaligned offset all decline to the call, so
    /// every element this writes is one the natives also update with a
    /// hardware atomic (`vm_exec::hw_cas_primitive_array_element`,
    /// `primitive_array_atomic_update`).
    ///
    /// nio21 wave 7 L3: a plain-object base takes the instance-field arm
    /// (`direct_helpers::unsafe_inline_field_cell_bytes`, out of line after the
    /// hit path) when this VM baked its cell table: a compact instance whose
    /// `(class id, offset, width)` the VM's hardware field CAS has published
    /// is updated at the published displacement by the same locked
    /// instruction; anything else declines. The `compareAndExchange*` rows
    /// (`UnsafeRmwOp::Cae`) keep CMPXCHG's witness as the result.
    ///
    /// For `getAndAdd` / `getAndSet` the call is the JDK's Java body (a
    /// `getXVolatile` + `weakCompareAndSetX` loop) or, in `--compatible`, the
    /// registered native; one `LOCK XADD` / `XCHG` is what HotSpot's
    /// intrinsic emits for either. No barrier: the bytes are primitive.
    pub(in crate::ir_lower) fn try_emit_ir_unsafe_rmw_prefix(
        &mut self,
        id: NodeId,
        info: &JitInvokeInfo,
        rmw: crate::direct_helpers::UnsafeInlineRmw,
    ) {
        if !matches!(info.invoke_kind, 0 | 1)
            || !rmw.admitted(self.direct_helpers.unsafe_inline_admitted)
            || !crate::direct_helpers::unsafe_accessor_inline_enabled()
            || !crate::ir_thin_prefix_enabled()
        {
            return;
        }
        let graph = self.graph;
        let Some(node) = graph.nodes.get(id as usize) else {
            return;
        };
        // [ctrl, mem, receiver, base, offset, a, (b)]
        if node.inputs.len() != 3 + rmw.num_params() {
            return;
        }
        let want_ty = if rmw.ret_type() == b'J' {
            IrType::Long
        } else {
            IrType::Int
        };
        if node.ty != want_ty || node.inputs[2..].iter().any(|&i| self.carry_names(i)) {
            return;
        }
        // The result's home, as the accessor prefix computes it.
        let Some(slot) = self.prefix_result_slot(id, node.ty) else {
            return;
        };
        let recv = node.inputs[2];
        let base = node.inputs[3];
        let offset = node.inputs[4];
        // CAS / compareAndExchange: (expected, new); getAndAdd: delta;
        // getAndSet: new; getAndBitwise: the mask.
        let (expected, operand) = if rmw.has_expected() {
            (Some(node.inputs[5]), node.inputs[6])
        } else {
            (None, node.inputs[5])
        };
        // nio21 wave 7 L3 (W6-L3-1): the instance-field arm, when this VM
        // published its cell table and no armed ZGC read barrier forbids an
        // inline field access (as for every other inline field arm).
        let field_arm = crate::direct_helpers::unsafe_field_arm_words(&self.direct_helpers);
        // JIT round 21, lane atomics (STATICCAS21-2): the static arm, a
        // non-array base whose offset lies in the static band.
        let static_arm = crate::direct_helpers::unsafe_static_arm_table(&self.direct_helpers);
        if crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] ir-unsafe-rmw-inline n{id} {}.{}{} width={} op={:?} field_arm={} static_arm={}",
                info.class_name,
                info.method_name,
                info.descriptor,
                rmw.width,
                rmw.op,
                field_arm.is_some(),
                static_arm.is_some(),
            );
        }
        let w = rmw.width;
        let mut decline: Vec<usize> = Vec::new();
        self.note_rcx_written();
        // R10 = the `Unsafe` receiver; null declines (the call's NPE).
        self.gp_load_value(R10, recv);
        self.buf.emit(&[0x4D, 0x85, 0xD2]); // TEST R10, R10
        decline.push(self.emit_jcc_rel32(0x84)); // JZ
                                                 // RAX = base; null (an off-heap address, an arena handle, a static
                                                 // base) declines: no off-heap arm for an atomic.
        self.gp_load_value(RAX, base);
        self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
        decline.push(self.emit_jcc_rel32(0x84)); // JZ
                                                 // A non-array base takes the field arm (emitted after the hit path);
                                                 // without one it declines in the array screen's own kind test.
        let to_field = (field_arm.is_some() || static_arm.is_some()).then(|| {
            let (bytes, edge) = crate::direct_helpers::unsafe_inline_not_array_bytes();
            let at = self.buf.pos();
            self.buf.emit(&bytes);
            at + edge
        });
        self.emit_ir_unsafe_prim_array_screen(offset, w, &mut decline);
        // Natural alignment (and, under the natives' old arm order, their
        // synthetic band).
        let (screen, edges) = crate::direct_helpers::unsafe_inline_rmw_screen_bytes(w);
        let at = self.buf.pos();
        self.buf.emit(&screen);
        decline.extend(edges.into_iter().map(|e| at + e));
        // RAX = the base, R11 = the payload displacement: the array arm's
        // offset or the field arm's compact displacement. The field arm joins
        // here.
        let join = self.buf.pos();
        // R10 = expected (a CAS); RDX = the operand. The screen is done with
        // both registers.
        if let Some(expected) = expected {
            self.gp_load_value(R10, expected);
        }
        self.gp_load_value(RDX, operand);
        self.buf
            .emit(&crate::direct_helpers::unsafe_inline_rmw_bytes(rmw));
        // The call route's `store_rax(slot)`: the register, then the home.
        self.emit_prefix_result_store(id, slot);
        let hit = self.emit_jmp_rel32();
        if let Some(to_field) = to_field {
            self.patch_rel32_to_here(to_field);
            self.gp_load_value(R11, offset);
            // STATICCAS21-2 (lane atomics): a static-band offset on its
            // class's mirror leaves RAX = the statics block and R11 = the
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
                // The field arm: R11 = the offset, then the cell screen leaves
                // R11 = the published compact displacement or declines.
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
        self.call_prefix_join = Some(hit);
    }

    /// JIT round 21, lane atomics (UNSAFE21-2): a `compareAndExchangeByte` /
    /// `compareAndExchangeShort` / `compareAndSetShort` / `compareAndSetByte`
    /// on a published COMPACT `boolean` / `byte` / `short` / `char` field as
    /// ONE `LOCK CMPXCHG` of the field's width, a guarded prefix of the
    /// `Op::Call`'s unchanged lowering with the contract of
    /// [`Self::try_emit_ir_unsafe_access_prefix`] (scratch RAX, RCX, RDX, R10,
    /// R11; no GC point, no call, no deopt on the hit path). The section
    /// comment above `direct_helpers::unsafe_subword_rmw_inline_enabled` says
    /// what is served and why; the single-pass twin is `x64::op_invoke`'s
    /// `emit_sp_unsafe_subword_core`, the same pieces in the same order: null
    /// receiver / null base / an array base -> decline; the sub-word cell
    /// screen (`direct_helpers::unsafe_inline_subword_field_cell_bytes`) ->
    /// R11 = the compact displacement or decline; then
    /// `direct_helpers::unsafe_inline_subword_rmw_bytes`. No barrier: the bytes
    /// are primitive.
    pub(in crate::ir_lower) fn try_emit_ir_unsafe_subword_prefix(
        &mut self,
        id: NodeId,
        info: &JitInvokeInfo,
        rmw: crate::direct_helpers::UnsafeSubwordRmw,
    ) {
        if !matches!(info.invoke_kind, 0 | 1) || !crate::ir_thin_prefix_enabled() {
            return;
        }
        let Some((cells, epoch_addr)) =
            crate::direct_helpers::unsafe_subword_field_arm_words(&self.direct_helpers, rmw)
        else {
            return;
        };
        let graph = self.graph;
        let Some(node) = graph.nodes.get(id as usize) else {
            return;
        };
        // [ctrl, mem, receiver, base, offset, expected, new]; every result
        // type (`B`, `S`, `Z`) is an `int` in the graph.
        if node.inputs.len() != 7
            || node.ty != IrType::Int
            || node.inputs[2..].iter().any(|&i| self.carry_names(i))
        {
            return;
        }
        let Some(slot) = self.prefix_result_slot(id, node.ty) else {
            return;
        };
        let (recv, base, offset) = (node.inputs[2], node.inputs[3], node.inputs[4]);
        let (expected, new) = (node.inputs[5], node.inputs[6]);
        if crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] ir-unsafe-subword-inline n{id} {}.{}{} width={} cae={}",
                info.class_name, info.method_name, info.descriptor, rmw.width, rmw.cae,
            );
        }
        let mut decline: Vec<usize> = Vec::new();
        self.note_rcx_written();
        // R10 = the `Unsafe` receiver; null declines (the call's NPE).
        self.gp_load_value(R10, recv);
        self.buf.emit(&[0x4D, 0x85, 0xD2]); // TEST R10, R10
        decline.push(self.emit_jcc_rel32(0x84)); // JZ
                                                 // RAX = base; null (a static base, an off-heap address) declines.
        self.gp_load_value(RAX, base);
        self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
        decline.push(self.emit_jcc_rel32(0x84)); // JZ
                                                 // An array base (a byte lane of an element) declines: the field arm
                                                 // only. The kind test jumps over the decline for a non-array base.
        let (bytes, edge) = crate::direct_helpers::unsafe_inline_not_array_bytes();
        let at = self.buf.pos();
        self.buf.emit(&bytes);
        let to_field = at + edge;
        decline.push(self.emit_jmp_rel32());
        self.patch_rel32_to_here(to_field);
        // R11 = the offset, then the published displacement (writes RCX,
        // RDX, R10, R11).
        self.gp_load_value(R11, offset);
        let (bytes, edges) = crate::direct_helpers::unsafe_inline_subword_field_cell_bytes(
            cells, epoch_addr, rmw.width,
        );
        let at = self.buf.pos();
        self.buf.emit(&bytes);
        decline.extend(edges.into_iter().map(|e| at + e));
        // R10 = expected, RDX = new; the CMPXCHG leaves the result in RAX.
        self.gp_load_value(R10, expected);
        self.gp_load_value(RDX, new);
        self.buf
            .emit(&crate::direct_helpers::unsafe_inline_subword_rmw_bytes(rmw));
        // The call route's `store_rax(slot)`: the register, then the home.
        self.emit_prefix_result_store(id, slot);
        let hit = self.emit_jmp_rel32();
        for p in decline {
            self.patch_rel32_to_here(p);
        }
        self.call_prefix_join = Some(hit);
    }

    /// JIT round 20 wave 12, lane qUnsafe (AT-4 / W7-L3-2): a
    /// `compareAndSetReference` (or `weakCompareAndSetReference*`) on a
    /// published COMPACT reference field as ONE `LOCK CMPXCHG`, a guarded
    /// prefix of the `Op::Call`'s unchanged lowering with the contract of
    /// [`Self::try_emit_ir_unsafe_access_prefix`] (scratch RAX, RCX, RDX,
    /// R10, R11; no GC point, no call, no deopt on the hit path). The section
    /// comment above `direct_helpers::unsafe_qu_ref_cas_field_enabled` says
    /// what is served and why; the single-pass twin is `x64::op_invoke`'s
    /// `emit_sp_unsafe_ref_cas_core`, byte for byte the same pieces:
    ///
    /// null receiver / null base / an array base -> decline; the reference
    /// store gate screen ([`vh_ref_store_gate_screen_bytes`], RAX = the
    /// holder, RDX = the new value) -> decline unless both barriers are
    /// provably no-ops; the reference cell screen
    /// (`direct_helpers::unsafe_inline_ref_field_cell_bytes`) -> R11 = the
    /// compact displacement or decline; then the `Unsafe` RMW prefixes' own
    /// CAS bytes (`unsafe_inline_rmw_bytes`, width 8).
    pub(in crate::ir_lower) fn try_emit_ir_unsafe_ref_cas_prefix(&mut self, id: NodeId, info: &JitInvokeInfo) {
        if !matches!(info.invoke_kind, 0 | 1)
            || !crate::ir_thin_prefix_enabled()
            || crate::x64::zgc_read_barrier_blocks_inline_fields()
        {
            return;
        }
        let field = crate::direct_helpers::unsafe_ref_cas_field_arm_words(&self.direct_helpers);
        // j27 wave 6, lane T1DISP20 (DISP20-8, opt-in): the reference-array
        // element arm (`crate::unsafe_ref_array_arm`).
        let array = crate::unsafe_ref_array_arm::unsafe_ref_array_arm_admitted(
            &self.direct_helpers,
            crate::unsafe_ref_array_arm::UNSAFE_REF_ARRAY_CAS_GATE_BIT,
        );
        if field.is_none() && !array {
            return;
        }
        let Some(gates) = vh_ref_store_gates(self.ref_store_gates, self.ref_store_post_skip_mask)
        else {
            return;
        };
        let graph = self.graph;
        let Some(node) = graph.nodes.get(id as usize) else {
            return;
        };
        // [ctrl, mem, receiver, base, offset, expected, new]
        if node.inputs.len() != 7
            || node.ty != IrType::Int
            || node.inputs[2..].iter().any(|&i| self.carry_names(i))
        {
            return;
        }
        let Some(slot) = self.prefix_result_slot(id, node.ty) else {
            return;
        };
        let (recv, base, offset) = (node.inputs[2], node.inputs[3], node.inputs[4]);
        let (expected, new) = (node.inputs[5], node.inputs[6]);
        if crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] ir-unsafe-ref-cas-inline n{id} {}.{}{}",
                info.class_name, info.method_name, info.descriptor,
            );
        }
        let mut decline: Vec<usize> = Vec::new();
        self.note_rcx_written();
        // R10 = the `Unsafe` receiver; null declines (the call's NPE).
        self.gp_load_value(R10, recv);
        self.buf.emit(&[0x4D, 0x85, 0xD2]); // TEST R10, R10
        decline.push(self.emit_jcc_rel32(0x84)); // JZ
                                                 // RAX = base; null (a static base, an off-heap address) declines.
        self.gp_load_value(RAX, base);
        self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
        decline.push(self.emit_jcc_rel32(0x84)); // JZ
                                                 // An array base (a reference-array element) declines, or takes the
                                                 // array arm; the kind test jumps to the field arm for a non-array base.
        let (bytes, edge) = crate::direct_helpers::unsafe_inline_not_array_bytes();
        let at = self.buf.pos();
        self.buf.emit(&bytes);
        let to_field = at + edge;
        let to_join = if array {
            crate::unsafe_ref_array_arm::note_ref_array_arm("ir cas", info.method_name);
            // The barrier gates, decided before the store (RAX = holder, RDX
            // = the new value; writes RCX, R11).
            self.gp_load_value(RDX, new);
            let (bytes, edges) = vh_ref_store_gate_screen_bytes(gates);
            let at = self.buf.pos();
            self.buf.emit(&bytes);
            decline.extend(edges.into_iter().map(|e| at + e));
            // R11 = the offset, kept: `[RAX + R11]` is the element slot
            // (writes RCX, RDX, R10).
            self.gp_load_value(R11, offset);
            let (bytes, edges) = crate::unsafe_ref_array_arm::unsafe_ref_array_screen_bytes();
            let at = self.buf.pos();
            self.buf.emit(&bytes);
            decline.extend(edges.into_iter().map(|e| at + e));
            Some(self.emit_jmp_rel32())
        } else {
            decline.push(self.emit_jmp_rel32());
            None
        };
        self.patch_rel32_to_here(to_field);
        if let Some((cells, epoch_addr)) = field {
            // The barrier gates, decided before the store (RAX = holder, RDX =
            // the new value; writes RCX, R11).
            self.gp_load_value(RDX, new);
            let (bytes, edges) = vh_ref_store_gate_screen_bytes(gates);
            let at = self.buf.pos();
            self.buf.emit(&bytes);
            decline.extend(edges.into_iter().map(|e| at + e));
            // R11 = the offset, then the published displacement (writes RCX,
            // RDX, R10, R11).
            self.gp_load_value(R11, offset);
            let (bytes, edges) =
                crate::direct_helpers::unsafe_inline_ref_field_cell_bytes(cells, epoch_addr);
            let at = self.buf.pos();
            self.buf.emit(&bytes);
            decline.extend(edges.into_iter().map(|e| at + e));
        } else {
            decline.push(self.emit_jmp_rel32());
        }
        // The array arm joins here (RAX = the base, R11 = the slot's offset).
        if let Some(to_join) = to_join {
            self.patch_rel32_to_here(to_join);
        }
        // R10 = expected, RDX = new; the CAS leaves `0`/`1` in RAX.
        self.gp_load_value(R10, expected);
        self.gp_load_value(RDX, new);
        self.buf
            .emit(&crate::direct_helpers::unsafe_inline_rmw_bytes(
                crate::direct_helpers::UnsafeInlineRmw {
                    width: 8,
                    op: crate::direct_helpers::UnsafeRmwOp::Cas,
                },
            ));
        // The call route's `store_rax(slot)`: the register, then the home.
        self.emit_prefix_result_store(id, slot);
        let hit = self.emit_jmp_rel32();
        for p in decline {
            self.patch_rel32_to_here(p);
        }
        self.call_prefix_join = Some(hit);
    }

    /// JIT round 21 wave 1, lane updaters (UPDATERS21-1,
    /// `CRATONVM_JIT_UNSAFE_REF_VOLATILE_FIELD`, opt-in): a
    /// `getReferenceVolatile` / `putReferenceVolatile` (and their acquire /
    /// opaque / release bodies) on a published COMPACT reference field as one
    /// `MOV` load / one `XCHG` or `MOV` store, a guarded prefix of the
    /// `Op::Call`'s unchanged lowering with the contract of
    /// [`Self::try_emit_ir_unsafe_access_prefix`] (scratch RAX, RCX, RDX, R10,
    /// R11; no GC point, no call, no deopt on the hit path). What is served and
    /// why: the section comment above
    /// `direct_helpers::unsafe_ref_volatile_field_enabled`. The single-pass
    /// twin is `x64::op_invoke`'s `emit_sp_unsafe_ref_volatile_core`.
    ///
    /// null receiver / null base / an array base -> decline; for a put the
    /// reference-store gate screen ([`vh_ref_store_gate_screen_bytes`], RAX =
    /// holder, RDX = value) -> decline unless both barriers are provably
    /// no-ops; the reference cell screen -> R11 = the displacement or decline;
    /// then `direct_helpers::unsafe_ref_volatile_access_bytes`.
    pub(in crate::ir_lower) fn try_emit_ir_unsafe_ref_volatile_prefix(
        &mut self,
        id: NodeId,
        info: &JitInvokeInfo,
        access: crate::direct_helpers::UnsafeRefVolatileAccess,
    ) {
        if !matches!(info.invoke_kind, 0 | 1)
            || !crate::ir_thin_prefix_enabled()
            || crate::x64::zgc_read_barrier_blocks_inline_fields()
        {
            return;
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
            return;
        }
        let gates = if access.is_put {
            match vh_ref_store_gates(self.ref_store_gates, self.ref_store_post_skip_mask) {
                Some(g) => Some(g),
                None => return,
            }
        } else {
            None
        };
        let graph = self.graph;
        let Some(node) = graph.nodes.get(id as usize) else {
            return;
        };
        // [ctrl, mem, receiver, base, offset, (value)]
        // JIT round 23 wave 1, lane L9: a getAndSet (`access.exchange`)
        // stores like a put and returns the previous reference like a get.
        let want_ty = if access.returns_value() {
            IrType::Ref
        } else {
            IrType::Void
        };
        if node.inputs.len() != 3 + access.num_params()
            || node.ty != want_ty
            || node.inputs[2..].iter().any(|&i| self.carry_names(i))
        {
            return;
        }
        let slot = if !access.returns_value() {
            None
        } else {
            let Some(s) = self.prefix_result_slot(id, node.ty) else {
                return;
            };
            s
        };
        let (recv, base, offset) = (node.inputs[2], node.inputs[3], node.inputs[4]);
        let value = if access.is_put {
            Some(node.inputs[5])
        } else {
            None
        };
        if crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] ir-unsafe-ref-volatile-inline n{id} {}.{}{} put={} xchg={}",
                info.class_name, info.method_name, info.descriptor, access.is_put, access.exchange,
            );
        }
        let mut decline: Vec<usize> = Vec::new();
        self.note_rcx_written();
        // R10 = the `Unsafe` receiver; null declines (the call's NPE).
        self.gp_load_value(R10, recv);
        self.buf.emit(&[0x4D, 0x85, 0xD2]); // TEST R10, R10
        decline.push(self.emit_jcc_rel32(0x84)); // JZ
        // RAX = base; null (a static base, an off-heap address) declines.
        self.gp_load_value(RAX, base);
        self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
        decline.push(self.emit_jcc_rel32(0x84)); // JZ
        // An array base declines (or, for a get with the array arm, takes it);
        // a plain object jumps to the field arm.
        let (bytes, edge) = crate::direct_helpers::unsafe_inline_not_array_bytes();
        let at = self.buf.pos();
        self.buf.emit(&bytes);
        let to_field = at + edge;
        let to_join = if array {
            crate::unsafe_ref_array_arm::note_ref_array_arm("ir get", info.method_name);
            // R11 = the offset, kept: `[RAX + R11]` is the element slot
            // (writes RCX, RDX, R10).
            self.gp_load_value(R11, offset);
            let (bytes, edges) = crate::unsafe_ref_array_arm::unsafe_ref_array_screen_bytes();
            let at = self.buf.pos();
            self.buf.emit(&bytes);
            decline.extend(edges.into_iter().map(|e| at + e));
            Some(self.emit_jmp_rel32())
        } else {
            decline.push(self.emit_jmp_rel32());
            None
        };
        self.patch_rel32_to_here(to_field);
        if let Some((cells, epoch_addr)) = cells {
            if let (Some(g), Some(value)) = (gates, value) {
                // The barrier gates, decided before the store (RAX = holder, RDX
                // = the value; writes RCX, R11).
                self.gp_load_value(RDX, value);
                let (bytes, edges) = vh_ref_store_gate_screen_bytes(g);
                let at = self.buf.pos();
                self.buf.emit(&bytes);
                decline.extend(edges.into_iter().map(|e| at + e));
            }
            // R11 = the offset, then the published displacement (writes RCX,
            // RDX, R10, R11).
            self.gp_load_value(R11, offset);
            let (bytes, edges) =
                crate::direct_helpers::unsafe_inline_ref_field_cell_bytes(cells, epoch_addr);
            let at = self.buf.pos();
            self.buf.emit(&bytes);
            decline.extend(edges.into_iter().map(|e| at + e));
        } else {
            decline.push(self.emit_jmp_rel32());
        }
        // The array arm joins here (RAX = the base, R11 = the slot's offset).
        if let Some(to_join) = to_join {
            self.patch_rel32_to_here(to_join);
        }
        if let Some(value) = value {
            self.gp_load_value(RDX, value);
        }
        self.buf
            .emit(&crate::direct_helpers::unsafe_ref_volatile_access_bytes(
                access,
            ));
        if access.exchange {
            // The `XCHG` left the previous reference in RDX: the result.
            self.buf
                .emit(&crate::direct_helpers::UNSAFE_REF_EXCHANGE_RESULT_BYTES);
        }
        if access.returns_value() {
            self.emit_prefix_result_store(id, slot);
        }
        let hit = self.emit_jmp_rel32();
        for p in decline {
            self.patch_rel32_to_here(p);
        }
        self.call_prefix_join = Some(hit);
    }

    /// nio21 wave 11 L5 (W9L2-2, page
    /// `r21-varhandle-constant-handle-field-access-inline-FIXED-20261012.md`): a
    /// `VarHandle` access-mode call whose handle operand IS a `getstatic` of a
    /// `static final` field the VM filed a constant plan for, as the field
    /// access itself -- a guarded PREFIX of the `Op::Call`'s unchanged lowering
    /// with the contract of [`Self::try_emit_ir_unsafe_access_prefix`]
    /// (scratch RAX, RCX, RDX, R10, R11 only; the hit edge writes the result
    /// home and register and jumps to [`Self::call_prefix_join`]; no call, no
    /// GC point, no deopt on the hit path). The shared section above
    /// `vh_const_core_bytes` says what is served; every receiver, value or
    /// barrier shape it does not serve declines to the call, which answers it
    /// exactly as before.
    ///
    /// The `getstatic` is the proof that the handle is the filed one: a
    /// `static final` is written once, by `<clinit>`, before the VM filed it.
    /// The node stays in the graph (it is this call's input), so its class
    /// initialisation obligation is unchanged.
    ///
    /// Declines at compile time under `CRATONVM_JIT_VH_CONST_INLINE=0` (and
    /// `CRATONVM_JIT_VH_CONST_REF_INLINE=0` for a reference field), for a site
    /// shape [`vh_const_site_shape`] refuses, a handle operand that is not a
    /// `LoadStatic`, no filed plan, no compact layout for the slot, an armed
    /// ZGC read barrier, an operand some carry is holding, and a result that is
    /// carried or has its home dropped.
    pub(in crate::ir_lower) fn try_emit_ir_vh_const_prefix(&mut self, id: NodeId, info_ptr: usize) {
        if info_ptr == 0 || !vh_const_inline_enabled() {
            return;
        }
        // SAFETY: as every other read of an `Op::Call`'s `info_ptr` in this
        // file -- a live `JitInvokeInfo` owned by `ir_call_infos`.
        let info: &JitInvokeInfo = unsafe { &*(info_ptr as *const JitInvokeInfo) };
        // j27 wave 2 (lane VHCOMP, `CRATONVM_JIT_VH_CONST_STATIC`): a
        // receiver-less static-field shape; it reads no reference, so ZGC's
        // read barrier does not gate it.
        if info.class_name == "java/lang/invoke/VarHandle"
            && matches!(info.invoke_kind, 0 | 1)
            && self.try_emit_ir_vh_const_static_prefix(id, info)
        {
            return;
        }
        if info.class_name != "java/lang/invoke/VarHandle"
            || !matches!(info.invoke_kind, 0 | 1)
            || crate::x64::zgc_read_barrier_blocks_inline_fields()
        {
            return;
        }
        // JIT round 21, lane cf: every compile-time decline below names itself
        // under `CRATONVM_DBG_JITC=1` (`vh-const-inline (ir) n.. DECLINED`), so
        // a site that keeps its call (`CompletableFuture.completeValue`'s
        // `RESULT.compareAndSet`, say) says why instead of printing nothing.
        // Diagnostic only.
        let dbg = crate::x64::dbg_jitc_enabled();
        let declined = |why: &str| {
            if dbg {
                eprintln!(
                    "[cratonvm-jitc] vh-const-inline (ir) n{id} DECLINED {}{}: {why}",
                    info.method_name, info.descriptor
                );
            }
        };
        let Some(shape) = vh_const_site_shape(info.method_name, info.descriptor) else {
            // JIT r20 wave 18 (lane xVhArr, AT-2): an array-element shape.
            self.try_emit_ir_vh_const_array_prefix(id, info);
            return;
        };
        let graph = self.graph;
        let Some(node) = graph.nodes.get(id as usize) else {
            return;
        };
        // [ctrl, mem, handle, receiver, values...]
        if node.inputs.len() != 4 + shape.values.len() {
            declined("operand count");
            return;
        }
        let want_ty = match shape.ret {
            b'V' => IrType::Void,
            b'J' => IrType::Long,
            b'L' | b'[' => IrType::Ref,
            _ => IrType::Int,
        };
        if node.ty != want_ty {
            declined("node type is not the site's return");
            return;
        }
        if node.inputs[2..].iter().any(|&i| self.carry_names(i)) {
            declined("an operand is held by a carry");
            return;
        }
        let handle = node.inputs[2];
        let Some(Op::LoadStatic {
            class_id,
            field_index,
            ..
        }) = graph.nodes.get(handle as usize).map(|n| &n.op)
        else {
            declined("handle operand is not a getstatic");
            return;
        };
        let (static_class, static_field) = (*class_id, *field_index as usize); // Cast: u32 index
        let Some(plan) = vh_const_plan(&self.direct_helpers, static_class, static_field) else {
            declined(
                format!("no constant plan filed for static ({static_class},{static_field})")
                    .as_str(),
            );
            return;
        };
        let ref_facts = if plan.kind == b'L' {
            match vh_const_ref_facts(&self.direct_helpers, static_class, static_field) {
                Some(facts) => Some(facts),
                None => {
                    declined("no reference row filed");
                    return;
                }
            }
        } else {
            None
        };
        // `class_id_offset_in_obj` is 0 by contract (the ArrayList prefix bakes
        // the same `[RAX + 0]`).
        // JIT round 21, lane g1vh: under G1 (no published gates) a reference
        // write is built against G1's plan (`vh_g1_gates`).
        let g1 = vh_g1_gates(
            self.ref_store_gates,
            self.g1_barrier_addr,
            self.satb_pre_gate,
            self.g1_post_write_barrier,
        );
        let store = match g1 {
            Some(g1) => VhStoreBarriers::G1(g1),
            None => VhStoreBarriers::Gates(self.ref_store_gates, self.ref_store_post_skip_mask),
        };
        // JIT round 21, lane vhcae: a typed reference return (a
        // `compareAndExchange` cast to the field's class) is proven in any
        // class of the coordinate class's loader.
        let ret_caller =
            if matches!(shape.ret, b'L' | b'[') && shape.ret_desc != "Ljava/lang/Object;" {
                vh_const_ret_proof_caller(
                    &self.direct_helpers,
                    info.declaring_class_id,
                    plan.receiver_class_id,
                )
            } else {
                info.declaring_class_id
            };
        if dbg && ret_caller != info.declaring_class_id {
            eprintln!(
                "[cratonvm-jitc] vh-const-inline (ir) n{id} return proven by the coordinate \
                 class's loader (caller {} coordinate {ret_caller})",
                info.declaring_class_id
            );
        }
        let Some(core) = vh_const_core_cast_return(
            &self.direct_helpers,
            &shape,
            plan,
            ref_facts.as_ref(),
            info.declaring_class_id,
            ret_caller,
            store,
            0,
        ) else {
            if dbg {
                let why = if shape.kind == b'L'
                    && shape.op != VhConstOp::Get
                    && self.ref_store_gates.is_none()
                    && g1.is_none()
                {
                    "reference write mode, the heap publishes no store gates and the G1 arm is \
                     unavailable (CRATONVM_JIT_VH_CONST_G1_REF / G1 geometry)"
                        .to_string()
                } else {
                    format!(
                        "core screen (plan kind={} read_only={} disp={:?}; shape kind={} ret={})",
                        char::from(plan.kind),
                        plan.read_only,
                        vh_const_field_disp(plan),
                        char::from(shape.kind),
                        shape.ret_desc,
                    )
                };
                declined(why.as_str());
            }
            return;
        };
        let slot = if want_ty == IrType::Void {
            None
        } else {
            let Some(slot) = self.prefix_result_slot(id, want_ty) else {
                declined("result is carried, routed, or has no destination");
                return;
            };
            slot
        };
        let recv = node.inputs[3];
        let (expected, operand) = if shape.has_expected() {
            (Some(node.inputs[4]), Some(node.inputs[5]))
        } else if shape.values.len() == 1 {
            (None, Some(node.inputs[4]))
        } else {
            (None, None)
        };
        if dbg {
            eprintln!(
                "[cratonvm-jitc] vh-const-inline (ir) n{id} {}{} kind={} class={} slot={} disp={} \
                 static=({static_class},{static_field}) checks={:?}/{:?} gated={}",
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
                    "[cratonvm-jitc] vh-const-inline (ir) n{id} g1 plan post_call={}",
                    g1.post_call
                );
            }
        }
        let mut decline: Vec<usize> = Vec::new();
        self.note_rcx_written();
        // First: the epoch guard's fallback form clobbers R11 and ECX.
        if let Some(p) = self.emit_layout_epoch_guard() {
            decline.push(p);
        }
        self.gp_load_value(RAX, recv);
        if let Some(expected) = expected {
            self.gp_load_value(R10, expected);
        }
        if let Some(operand) = operand {
            self.gp_load_value(RDX, operand);
        }
        // j27 wave 2 (lane VHCOMP, `CRATONVM_JIT_VH_CONST_GEN_CARD_EDGE`): a
        // generational reference write that may owe a card takes the card
        // edge below instead of the call.
        let gen_edge = self.vh_gen_card_edge_admitted(&core);
        let (bytes, edges, post) = if gen_edge {
            vh_const_core_bytes_gen_edge(&core)
        } else {
            vh_const_core_bytes_post(&core)
        };
        let at = self.buf.pos();
        self.buf.emit(&bytes);
        decline.extend(edges.into_iter().map(|e| at + e));
        if want_ty != IrType::Void {
            self.emit_prefix_result_store(id, slot);
        }
        let hit_at = self.buf.pos();
        let hit = self.emit_jmp_rel32();
        // JIT round 21, lane g1vh: G1's cross-region post edge, entered
        // BEFORE the access with RAX / R10 / RDX still the operands (the
        // screens wrote only RCX and R11): the access (whose cast arms may
        // still decline, before any write), the result as above, then G1's
        // own remembered-set barrier (`jit_g1_post_write_barrier(vm_ptr, recv,
        // val)`), exactly the gated G1 `putfield`'s call after its store. No
        // safepoint lies between the region test, the store and the barrier.
        // The call clobbers only what the `Op::Call` this prefixes clobbers
        // anyway (every caller-saved register was given up before the node);
        // the operands are re-read from their homes / registers.
        if let (true, Some(p)) = (gen_edge, post.map(|e| at + e)) {
            match operand {
                Some(value) => {
                    self.patch_rel32_to_here(p);
                    // j27 wave 4 (lane VHCARD): the screen's own access ends
                    // the core bytes (`vh_const_core_bytes_with`).
                    let plain_access = at + bytes.len() - vh_const_access_bytes(&core).0.len();
                    let ops = (recv, expected, value);
                    let access_declines =
                        self.emit_ir_vh_gen_card_edge(&core, id, ops, slot, hit_at, plain_access);
                    decline.extend(access_declines);
                }
                // Not reachable (a write mode has a value): the call.
                None => decline.push(p),
            }
        } else if let Some(p) = post.map(|e| at + e) {
            match operand {
                Some(value) if self.g1_post_write_barrier != 0 => {
                    self.patch_rel32_to_here(p);
                    let (access, access_edges) = vh_const_access_bytes(&core);
                    let access_at = self.buf.pos();
                    self.buf.emit(&access);
                    let access_declines: Vec<usize> =
                        access_edges.into_iter().map(|e| access_at + e).collect();
                    if want_ty != IrType::Void {
                        self.emit_prefix_result_store(id, slot);
                    }
                    // No call when the barrier provably does nothing; every
                    // skip rejoins exactly as the call does. Both filters
                    // write only caller-saved registers (the call below
                    // clobbers them too); the operands are re-read.
                    let mut skips: Vec<usize> = Vec::new();
                    // JIT round 23 wave 3, lane L4's young-holder filter
                    // (`CRATONVM_JIT_G1_INLINE_YOUNG_FILTER`), as the gated
                    // G1 `putfield` takes it: a holder in an Eden / Survivor
                    // region owes no edge and no card. Without it every
                    // young-holder CAS (`ConcurrentLinkedQueue` node links)
                    // would walk the edge filter below to a sure miss.
                    self.gp_load_value(RCX, recv);
                    self.emit_mov_reg_imm64(R11, self.g1_barrier_addr as u64);
                    self.buf.emit(&[0x49, 0x2B, 0x8B]); // SUB RCX, [R11 + disp32]
                    self.buf.emit(&G1B_ARENA_BASE.to_le_bytes());
                    if let Some((bytes, young)) =
                        crate::x64::g1_young_holder_skip_bytes(RCX, None, R11)
                    {
                        let young_at = self.buf.pos();
                        self.buf.emit(&bytes);
                        skips.extend(young.into_iter().map(|s| young_at + s));
                    }
                    // JIT round 21, lane vhcae (VHCAE21-1,
                    // `CRATONVM_JIT_G1_EDGE_FILTER`): the collector's edge
                    // filter (card dirty, block offset covered, the edge
                    // recorded this epoch).
                    let mut filter_misses: Vec<usize> = Vec::new();
                    if let Some(g) =
                        crate::ir_g1_edge_filter::g1_edge_filter_geom(&self.direct_helpers)
                    {
                        if crate::x64::dbg_jitc_enabled() {
                            eprintln!(
                                "[cratonvm-jitc] vh-const-inline (ir) n{id} g1 edge filter inline (filter=0x{:x})",
                                g.filter
                            );
                        }
                        self.gp_load_value(RAX, recv);
                        self.gp_load_value(RDX, value);
                        let (bytes, misses) =
                            crate::ir_g1_edge_filter::g1_edge_filter_hit_bytes(&g);
                        let filter_at = self.buf.pos();
                        self.buf.emit(&bytes);
                        filter_misses.extend(misses.into_iter().map(|m| filter_at + m));
                        skips.push(self.emit_jmp_rel32());
                    }
                    if !skips.is_empty() {
                        // The filters' misses fall to the call; the skips
                        // land here, behind a jump over this rejoin.
                        let over = self.emit_jmp_rel32();
                        for s in skips {
                            self.patch_rel32_to_here(s);
                        }
                        if let Some(slot) = slot {
                            self.load_reg_from_frame(RAX, slot); // RAX = the result again
                        }
                        self.emit_jmp_back_to(hit_at);
                        self.patch_rel32_to_here(over);
                    }
                    for m in filter_misses {
                        self.patch_rel32_to_here(m);
                    }
                    self.load_context(CALL_ARG_REGS[0]);
                    self.gp_load_value(CALL_ARG_REGS[1], recv);
                    self.gp_load_value(CALL_ARG_REGS[2], value);
                    cratonvm_jit_api::assert_helper_call_shape!(
                        "g1_post_write_barrier",
                        int_args = 3,
                        returns_value = false
                    );
                    self.emit_mov_reg_imm64(RAX, self.g1_post_write_barrier as u64);
                    self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
                    if let Some(slot) = slot {
                        self.load_reg_from_frame(RAX, slot); // RAX = the result again
                    }
                    self.emit_jmp_back_to(hit_at);
                    decline.extend(access_declines);
                }
                // Not reachable (a write mode has a value; `vh_g1_gates` needs
                // the barrier's address): the edge then takes the call, before
                // any access.
                _ => decline.push(p),
            }
        }
        for p in decline {
            self.patch_rel32_to_here(p);
        }
        self.call_prefix_join = Some(hit);
        // JIT round 21, lane g1vhsoft (`CRATONVM_JIT_IR_EVIDENCE_G1_VH_WRITE`):
        // a G1 reference write served here is a call the single-pass body
        // keeps (its prefix has no G1 plan), so it is acceptance evidence.
        if core.g1.is_some() && ir_evidence_g1_vh_write_enabled() {
            if dbg {
                eprintln!(
                    "[cratonvm-jitc] vh-const-inline (ir) n{id} g1 reference write: acceptance evidence"
                );
            }
            crate::ir_evidence::note_builder_expansion();
        }
    }

    /// JIT round 22 (lane memory, FFM22-1 step (a),
    /// `CRATONVM_JIT_IR_FFM_CRITICAL_CALL`): the `invokeExact` of a `static
    /// final` bound FFM downcall handle linked `Linker.Option.critical`, made
    /// as the C call itself, as a guarded PREFIX of the `Op::Call`'s unchanged
    /// lowering. The hit edge stores the result and jumps to
    /// [`Self::call_prefix_join`]; every decline edge lands on the call, whose
    /// door (`jit_invoke_virtual_mic`, the FFM direct door, the bound row)
    /// serves it as before.
    ///
    /// The VM answers the row ([`ffm_const_row`]) only for a field of an
    /// initialised class that is `static final` and not volatile, holding the
    /// very handle the bound row's direct stub was filled from (same address in
    /// the same collection epoch), whose call is a PLAIN stub of at most six
    /// integer-class parameters answering one of them or nothing, filled for a
    /// site of this descriptor (so the call cannot be a
    /// `WrongMethodTypeException`). The function address, the widenings and
    /// the return read are the stub's own (`JitDowncallDirect::compiled_call_shape`),
    /// fixed for the handle's life.
    ///
    /// Thread state: none, as the critical row (`ffm_downcall_direct_call`)
    /// makes none -- HotSpot's critical contract: no upcall, no blocking, no
    /// safepoint inside; a stop-the-world waits for this thread's next poll,
    /// and a take-over that signals it in C finds a compiled frame above a
    /// non-JIT pc, the helper-window case it already scans conservatively, as
    /// for the Rust row. Per call, before the C call, the row's guard words
    /// (the capability index's generation as the row's settled access verdict
    /// read it, and the "a `SecurityManager` is installed" mirror at zero: the
    /// two halves of the row's per-call access verdict that can change,
    /// `panama::compiled_call_access_guard`) must hold, else the call
    /// declines. What the row also skips: the native invocation tally (the
    /// VM's fill declares the slot's count a floor) and the context guard (a
    /// critical function may not up-call).
    ///
    /// SysV x86-64 only (the row is refused elsewhere). The call is a
    /// clobber point like the `Op::Call` it prefixes: every caller-saved
    /// register was given up before the node (`invalidate_caller_saved_residency`)
    /// and the frame's alignment is the body's, which every helper call
    /// relies on.
    pub(in crate::ir_lower) fn try_emit_ir_ffm_const_prefix(&mut self, id: NodeId, info_ptr: usize) {
        if info_ptr == 0 || !ir_ffm_critical_call_enabled() {
            return;
        }
        let Some((_, row)) = self.ffm_const_site(id, info_ptr) else {
            return;
        };
        if row.flags & FFM_CONST_CRITICAL == 0 || row.flags & FFM_CONST_HELPER_ONLY != 0 {
            return;
        }
        // SAFETY: as every other read of an `Op::Call`'s `info_ptr` in this
        // file -- a live `JitInvokeInfo` owned by `ir_call_infos`.
        let info: &JitInvokeInfo = unsafe { &*(info_ptr as *const JitInvokeInfo) };
        let graph = self.graph;
        let Some(node) = graph.nodes.get(id as usize) else {
            return;
        };
        let Some(Op::LoadStatic {
            class_id,
            field_index,
            ..
        }) = graph.nodes.get(node.inputs[2] as usize).map(|n| &n.op)
        else {
            return;
        };
        let arity = usize::from(row.arity);
        let args = &node.inputs[3..];
        let want_ty = node.ty;
        if node.inputs[2..].iter().any(|&i| self.carry_names(i)) {
            return;
        }
        let slot = if want_ty == IrType::Void {
            None
        } else {
            match self.prefix_result_slot(id, want_ty) {
                Some(slot) => slot,
                None => return,
            }
        };
        if crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] ffm-const-call (ir) n{id} {}{} fn={:#x} arity={arity} params={:?} \
                 ret={} static=({class_id},{field_index}) critical",
                info.method_name,
                info.descriptor,
                row.fn_addr,
                &row.params[..arity],
                row.ret,
            );
        }
        let mut decline: Vec<usize> = Vec::new();
        self.note_rcx_written();
        decline.extend(self.emit_ffm_const_guards(&row));
        for (i, &arg) in args.iter().enumerate() {
            let reg = FFM_CONST_SYSV_ARGS[i];
            self.gp_load_value(reg, arg);
            self.buf.emit(&ffm_const_widen_bytes(reg, row.params[i]));
        }
        // MOV RAX, fn ; CALL RAX
        self.emit_mov_reg_imm64(RAX, row.fn_addr);
        self.buf.emit(&[0xFF, 0xD0]);
        self.buf.emit(&ffm_const_widen_bytes(RAX, row.ret));
        if want_ty != IrType::Void {
            self.emit_prefix_result_store(id, slot);
        }
        let hit = self.emit_jmp_rel32();
        for p in decline {
            self.patch_rel32_to_here(p);
        }
        self.call_prefix_join = Some(hit);
    }

    /// JIT round 22 (lane memory, FFM22-1): the row of the `Op::Call` `id`
    /// when it is an `invokeExact` of a `static final` (non-volatile) field
    /// whose handle the VM filed a compiled-call row for, for this very
    /// descriptor, with this node's arity and result type; with the row's
    /// address. Shared by step (a)'s prefix and step (b)'s route.
    pub(in crate::ir_lower) fn ffm_const_site(&self, id: NodeId, info_ptr: usize) -> Option<(usize, FfmConstRow)> {
        if info_ptr == 0 {
            return None;
        }
        // SAFETY: as every other read of an `Op::Call`'s `info_ptr` in this
        // file -- a live `JitInvokeInfo` owned by `ir_call_infos`.
        let info: &JitInvokeInfo = unsafe { &*(info_ptr as *const JitInvokeInfo) };
        if info.class_name != "java/lang/invoke/MethodHandle"
            || info.method_name != "invokeExact"
            || info.invoke_kind != 0
        {
            return None;
        }
        let graph = self.graph;
        let node = graph.nodes.get(id as usize)?;
        // [ctrl, mem, handle, args...]
        if node.inputs.len() < 3 {
            return None;
        }
        let Some(Op::LoadStatic {
            class_id,
            field_index,
            is_volatile: false,
            ..
        }) = graph.nodes.get(node.inputs[2] as usize).map(|n| &n.op)
        else {
            return None;
        };
        let (addr, row) = ffm_const_row(&self.direct_helpers, *class_id, *field_index)?;
        if row.descriptor() != Some(info.descriptor) {
            return None;
        }
        let arity = usize::from(row.arity);
        let helper_only = row.flags & FFM_CONST_HELPER_ONLY != 0;
        if node.inputs.len() - 3 != arity || (!helper_only && arity > FFM_CONST_SYSV_ARGS.len())
        {
            return None;
        }
        let want_ty = match row.ret {
            FFM_CONST_VOID => IrType::Void,
            FFM_CONST_W64 => IrType::Long,
            FFM_CONST_S8 | FFM_CONST_S16 | FFM_CONST_S32 | FFM_CONST_U16 => IrType::Int,
            FFM_CONST_REF if helper_only => IrType::Ref,
            _ => return None,
        };
        (node.ty == want_ty).then_some((addr, row))
    }

    /// The row's per-call guards (FFM22-1): for each used guard word, `MOV
    /// RAX, word ; MOV R11, value ; CMP [RAX], R11 ; JNE <decline>`. Clobbers
    /// RAX, R11 and the flags; answers the `JNE` operands to patch.
    pub(in crate::ir_lower) fn emit_ffm_const_guards(&mut self, row: &FfmConstRow) -> Vec<usize> {
        let mut edges = Vec::new();
        for (&word, &value) in row.guard_words.iter().zip(&row.guard_values) {
            if word == 0 {
                continue;
            }
            self.emit_mov_reg_imm64(RAX, word);
            self.emit_mov_reg_imm64(R11, value);
            // CMP qword [RAX], R11
            self.buf.emit(&[0x4C, 0x39, 0x18]);
            edges.push(self.emit_jcc_rel32(0x85));
        }
        edges
    }

    /// JIT round 22 (lane memory, FFM22-1 step (b),
    /// `CRATONVM_JIT_IR_FFM_LAZY_CALL`): the `Op::Call` `id` of a NON-critical
    /// constant bound downcall ([`Self::ffm_const_site`]), lowered as a call of
    /// the row's thin helper (`jit_ffm_const_lazy_call`) instead of the
    /// dispatch helper: the helper makes the lazy in-native transition (the
    /// spill and the band from its own frame, the context guard an up-call
    /// reads, the `NativeRunning` span) around the C call and nothing else --
    /// no door, no site cache, no bind, no handle validation.
    ///
    /// It is an ordinary call of this arm: the map is published before it, the
    /// frame record republished, the shadow stack reloaded and the sentinel
    /// tested after it, so a pause that claims this thread while it is in C
    /// (the lazy state lets it run on) walks a compiled frame whose map
    /// describes this very call, and a collection an up-call makes moves this
    /// frame's references as any call's. Per call, before the helper, the
    /// row's guard words must hold (as step (a)'s); else the site's own dispatch
    /// helper (`jit_invoke_dispatch`, which serves any site) is called on the
    /// same staged arguments. Both calls join at one return check.
    #[allow(clippy::too_many_arguments)]
    pub(in crate::ir_lower) fn emit_ffm_lazy_call(
        &mut self,
        inputs: &[NodeId],
        slot: i32,
        num_args: usize,
        row_addr: usize,
        row: &FfmConstRow,
        info_ptr: usize,
        ty: IrType,
        map: &mut Option<SafepointMapToken>,
    ) {
        if crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] ffm-const-call (ir) lazy fn={:#x} arity={} ret={} row={row_addr:#x}",
                row.fn_addr, row.arity, row.ret,
            );
        }
        for i in 0..num_args {
            let arg = inputs[2 + i];
            self.gp_load_abi_value(RAX, arg);
            self.store_rax(self.args_stage_top_off - (i as i32) * 8);
        }
        let slow = self.emit_ffm_const_guards(row);
        self.load_context(CALL_ARG_REGS[0]);
        self.emit_mov_reg_imm64(CALL_ARG_REGS[1], row_addr as u64);
        self.lea_reg_from_frame(CALL_ARG_REGS[2], self.args_stage_top_off);
        self.emit_mov_reg_imm64(CALL_ARG_REGS[3], info_ptr as u64);
        self.emit_mov_reg_imm64(RAX, row.call_helper);
        self.buf.emit(&[0xFF, 0xD0]);
        self.anchor_safepoint_return(map.as_mut(), self.buf.pos());
        self.note_inline_frame_return_site();
        let done = self.emit_jmp_rel32();
        for p in slow {
            self.patch_rel32_to_here(p);
        }
        self.load_context(CALL_ARG_REGS[0]);
        self.emit_mov_reg_imm64(CALL_ARG_REGS[1], info_ptr as u64);
        self.lea_reg_from_frame(CALL_ARG_REGS[2], self.args_stage_top_off);
        self.emit_mov_reg_imm64(CALL_ARG_REGS[3], num_args as u64);
        self.emit_mov_reg_imm64(RAX, self.invoke_dispatch as u64);
        self.buf.emit(&[0xFF, 0xD0]);
        self.anchor_safepoint_return(map.as_mut(), self.buf.pos());
        self.note_inline_frame_return_site();
        self.patch_rel32_to_here(done);
        self.emit_call_return_check_after_note(slot, ty);
    }

    /// JIT round 22 (lane memory, FFM22-3, `CRATONVM_JIT_IR_FFM_SEGMENT_INLINE`):
    /// the `Op::Call` `id` of a constant bound downcall whose row has one
    /// segment parameter ([`FfmConstRow::seg_param`]), lowered as step (c)'s
    /// route ([`Self::emit_ffm_lazy_call`]) with the segment checked here
    /// first. The arguments are staged as there; then, after the row's guard
    /// words and the layout epoch:
    ///
    /// ```text
    ///   RDX = staged segment ; null, not exactly NativeMemorySegmentImpl, or
    ///   not compact -> step (c)
    ///   extra[0] = [RDX+min] ; extra[1] = [RDX+length] ; RDX = [RDX+scope]
    ///   class(RDX) == GlobalSession  -> extra[2] = extra[3] = 0
    ///   class(RDX) == ConfinedSession, compact, state >= 0 (open), owner ==
    ///     this thread's java.lang.Thread -> extra[2] = RDX, extra[3] = the
    ///     acquireCount offset
    ///   anything else -> step (c)
    ///   CALL seg_call_helper(vm, row, stage, info)
    /// ```
    ///
    /// The confined test is the scoped fold's (`emit_scoped_confined_session_check`
    /// says why owner-and-state is all of `checkValidStateRaw()` and why no
    /// other thread can close the session meanwhile); a global session's
    /// check cannot fail. The helper acquires, calls and releases with no
    /// field read of its own, and hands a call it cannot make (a forwarded
    /// argument, an address the row's screens refuse, a count at its limit)
    /// to step (c)'s helper, which reads everything again. A guard word that
    /// does not hold takes the dispatch helper, as step (c)'s route. Every
    /// route is an ordinary call of this arm; all join at one return check.
    #[allow(clippy::too_many_arguments)]
    pub(in crate::ir_lower) fn emit_ffm_seg_call(
        &mut self,
        inputs: &[NodeId],
        slot: i32,
        num_args: usize,
        row_addr: usize,
        row: &FfmConstRow,
        info_ptr: usize,
        ty: IrType,
        lay: FfmSegInlineLayout,
        map: &mut Option<SafepointMapToken>,
    ) {
        if crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] ffm-const-call (ir) segment-inline fn={:#x} arity={} seg={} ret={} \
                 row={row_addr:#x}",
                row.fn_addr, row.arity, row.seg_param, row.ret,
            );
        }
        let top = self.args_stage_top_off;
        // Cast: the stage's word index, bounded by `args_stage_words`.
        let stage_off = |i: usize| top - (i as i32) * 8;
        for i in 0..num_args {
            let arg = inputs[2 + i];
            self.gp_load_abi_value(RAX, arg);
            self.store_rax(stage_off(i));
        }
        let dispatch = self.emit_ffm_const_guards(row);
        let mut step_c: Vec<usize> = Vec::new();
        // The epoch guard's fallback form clobbers R11 and ECX.
        self.note_rcx_written();
        if let Some(p) = self.emit_layout_epoch_guard() {
            step_c.push(p);
        }
        // MOV RDX, [RBP - stage(1 + seg)]
        self.buf.emit(&[0x48, 0x8B]);
        self.emit_rbp_modrm_disp(RDX, stage_off(1 + usize::from(row.seg_param)));
        self.buf.emit(&[0x48, 0x85, 0xD2]); // TEST RDX, RDX
        step_c.push(self.emit_jcc_rel32(0x84)); // JZ: null
        self.buf.emit(&[0x81, 0x3A]); // CMP DWORD [RDX], imm32 -- the class id
        self.buf.emit(&lay.segment_class.to_le_bytes());
        step_c.push(self.emit_jcc_rel32(0x85));
        let compact_test = [
            0xF6,
            0x42,
            cratonvm_types::GC_FLAGS_BYTE_OFFSET as u8, // Cast: x86-64 disp8
            cratonvm_types::GC_FLAG_COMPACT,
        ];
        self.buf.emit(&compact_test); // TEST BYTE [RDX + GC_FLAGS], COMPACT
        step_c.push(self.emit_jcc_rel32(0x84)); // JZ: legacy
        let extra = |k: usize| stage_off(num_args + k);
        self.buf.emit(&[0x48, 0x8B, 0x82]); // MOV RAX, [RDX + min]
        self.buf.emit(&lay.min.to_le_bytes());
        self.store_rax(extra(0));
        self.buf.emit(&[0x48, 0x8B, 0x82]); // MOV RAX, [RDX + length]
        self.buf.emit(&lay.length.to_le_bytes());
        self.store_rax(extra(1));
        self.buf.emit(&[0x48, 0x8B, 0x92]); // MOV RDX, [RDX + scope]
        self.buf.emit(&lay.scope.to_le_bytes());
        self.buf.emit(&[0x48, 0x85, 0xD2]); // TEST RDX, RDX
        step_c.push(self.emit_jcc_rel32(0x84));
        self.buf.emit(&[0x8B, 0x02]); // MOV EAX, [RDX] -- the session's class id
        self.buf.emit_byte(0x3D); // CMP EAX, imm32
        self.buf.emit(&lay.global_class.to_le_bytes());
        let global = self.emit_jcc_rel32(0x84);
        self.buf.emit_byte(0x3D); // CMP EAX, imm32
        self.buf.emit(&lay.confined_class.to_le_bytes());
        step_c.push(self.emit_jcc_rel32(0x85));
        self.buf.emit(&compact_test);
        step_c.push(self.emit_jcc_rel32(0x84));
        self.buf.emit(&[0x8B, 0x82]); // MOV EAX, [RDX + state]
        self.buf.emit(&lay.state.to_le_bytes());
        self.buf.emit(&[0x85, 0xC0]); // TEST EAX, EAX
        step_c.push(self.emit_jcc_rel32(0x88)); // JS: closed
        self.buf.emit(&[0x4C, 0x8B, 0x92]); // MOV R10, [RDX + owner]
        self.buf.emit(&lay.owner.to_le_bytes());
        let tls = u32::try_from(crate::x64::jit_thread_tls_disp()).unwrap_or(0);
        self.emit_mov_rax_tls_disp32(tls);
        self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
        step_c.push(self.emit_jcc_rel32(0x84));
        self.buf.emit(&[0x48, 0x8B, 0x80]); // MOV RAX, [RAX + thread_off]
        self.buf.emit(&lay.thread_off.to_le_bytes());
        self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
        step_c.push(self.emit_jcc_rel32(0x84));
        self.buf.emit(&[0x4C, 0x39, 0xD0]); // CMP RAX, R10
        step_c.push(self.emit_jcc_rel32(0x85)); // JNE: not this thread's
        self.buf.emit(&[0x48, 0x89, 0xD0]); // MOV RAX, RDX
        self.store_rax(extra(2));
        self.buf.emit_byte(0xB8); // MOV EAX, imm32
        self.buf.emit(&lay.acquire.to_le_bytes());
        self.store_rax(extra(3));
        let have = self.emit_jmp_rel32();
        self.patch_rel32_to_here(global);
        self.buf.emit(&[0x31, 0xC0]); // XOR EAX, EAX
        self.store_rax(extra(2));
        self.store_rax(extra(3));
        self.patch_rel32_to_here(have);
        let mut done: Vec<usize> = Vec::new();
        for (helper, edges) in [(row.seg_call_helper, Vec::new()), (row.call_helper, step_c)] {
            for p in edges {
                self.patch_rel32_to_here(p);
            }
            self.load_context(CALL_ARG_REGS[0]);
            self.emit_mov_reg_imm64(CALL_ARG_REGS[1], row_addr as u64);
            self.lea_reg_from_frame(CALL_ARG_REGS[2], top);
            self.emit_mov_reg_imm64(CALL_ARG_REGS[3], info_ptr as u64);
            self.emit_mov_reg_imm64(RAX, helper);
            self.buf.emit(&[0xFF, 0xD0]);
            self.anchor_safepoint_return(map.as_mut(), self.buf.pos());
            self.note_inline_frame_return_site();
            done.push(self.emit_jmp_rel32());
        }
        for p in dispatch {
            self.patch_rel32_to_here(p);
        }
        self.load_context(CALL_ARG_REGS[0]);
        self.emit_mov_reg_imm64(CALL_ARG_REGS[1], info_ptr as u64);
        self.lea_reg_from_frame(CALL_ARG_REGS[2], top);
        self.emit_mov_reg_imm64(CALL_ARG_REGS[3], num_args as u64);
        self.emit_mov_reg_imm64(RAX, self.invoke_dispatch as u64);
        self.buf.emit(&[0xFF, 0xD0]);
        self.anchor_safepoint_return(map.as_mut(), self.buf.pos());
        self.note_inline_frame_return_site();
        for p in done {
            self.patch_rel32_to_here(p);
        }
        self.emit_call_return_check_after_note(slot, ty);
    }

    /// JIT round 21 wave 16 (lane perf-cf-lean,
    /// `CRATONVM_JIT_IR_CALL_ENTRY_NULL_GUARD`, opt-in; page
    /// `docs/feature-designs/jit-ir-optimizations.md`):
    /// a `void` instance call whose resolved target begins with a pure null
    /// test of one reference field of `this` that returns at once on null,
    /// as a guarded PREFIX of the `Op::Call`'s unchanged lowering: when the
    /// field reads null the call is skipped (the hit edge stores nothing --
    /// the method is `void` -- and jumps to [`Self::call_prefix_join`]); any
    /// other receiver or a non-null field declines to the call. No call, no
    /// GC point and no deopt on the hit path.
    ///
    /// The VM decides admission (`jit_bridge::call_entry_null_guard_answer`):
    /// a receiver-bound target (private, final, or in / reached from a final
    /// class), not static, native, abstract, synchronized or a constructor,
    /// with no registered native shadowing it and no breakpoint in it, whose
    /// entry path -- abstractly executed with that field null -- touches only
    /// locals, `this`, that field and reference compares before `return`.
    /// The VM records the target class as copied bytecode, so a redefinition
    /// of it withdraws this body. Skipping is then exactly the callee's own
    /// execution with no side effect: one read of the field, then `return`.
    /// A racing writer between the read and the skip is the race the callee
    /// has between its read and its return.
    ///
    /// Declines at compile time with the switch off, for a static or
    /// interface site, a constructor, a non-`void` descriptor, narrow oops or
    /// an armed ZGC read barrier, a carried receiver, and no VM answer.
    pub(in crate::ir_lower) fn try_emit_ir_call_entry_null_guard_prefix(&mut self, id: NodeId, info_ptr: usize) {
        if info_ptr == 0 || !ir_call_entry_null_guard_enabled() {
            return;
        }
        // SAFETY: as every other read of an `Op::Call`'s `info_ptr` in this
        // file -- a live `JitInvokeInfo` owned by `ir_call_infos`.
        let info: &JitInvokeInfo = unsafe { &*(info_ptr as *const JitInvokeInfo) };
        if !matches!(info.invoke_kind, 0 | 1)
            || !info.descriptor.ends_with(")V")
            || info.method_name.starts_with('<')
            || info.declaring_class_id == 0
            || crate::x64::narrow_oops_block_inline_fields()
            || crate::x64::zgc_read_barrier_blocks_inline_fields()
        {
            return;
        }
        let graph = self.graph;
        let Some(node) = graph.nodes.get(id as usize) else {
            return;
        };
        // [ctrl, mem, receiver, args...], void.
        if node.ty != IrType::Void || node.inputs.len() < 3 || self.carry_names(node.inputs[2]) {
            return;
        }
        let Some((disp, volatile)) = call_entry_null_guard_ask(&self.direct_helpers, info) else {
            return;
        };
        if crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] call-entry-null-guard (ir) n{id} {}.{}{} disp={disp} volatile={volatile}",
                info.class_name, info.method_name, info.descriptor,
            );
        }
        let mut decline: Vec<usize> = Vec::new();
        // The epoch guard's fallback form clobbers R11 and ECX.
        self.note_rcx_written();
        if let Some(p) = self.emit_layout_epoch_guard() {
            decline.push(p);
        }
        self.gp_load_value(RAX, node.inputs[2]);
        let (bytes, edges) = call_entry_null_guard_bytes(disp);
        let at = self.buf.pos();
        self.buf.emit(&bytes);
        decline.extend(edges.into_iter().map(|e| at + e));
        let hit = self.emit_jmp_rel32();
        for p in decline {
            self.patch_rel32_to_here(p);
        }
        self.call_prefix_join = Some(hit);
    }

    /// JIT r20 wave 18 (lane xVhArr, AT-2 of
    /// `docs/feature-designs/jit-ir-optimizations.md`): the
    /// ARRAY-ELEMENT arm of [`Self::try_emit_ir_vh_const_prefix`] (which has
    /// checked the switch, the `VarHandle` site and the ZGC gate), with the
    /// same prefix contract: a `VarHandle` access whose handle operand IS a
    /// `getstatic` of a `static final` filed as an `int[]` / `long[]` element
    /// handle ([`vh_const_array_plan`]) runs as the element access
    /// ([`vh_const_array_core_bytes`]); a null array, an index out of range,
    /// another array shape decline to the unchanged call. The section above
    /// `vh_const_array_element_enabled` says what is served.
    ///
    /// Declines at compile time under `CRATONVM_JIT_VH_CONST_ARRAY_ELEMENT=0`,
    /// for a shape [`vh_const_array_site_shape`] refuses, a handle operand
    /// that is not a `LoadStatic`, no filed array plan of the site's kind, an
    /// operand some carry is holding, and a result that is carried or has its
    /// home dropped.
    pub(in crate::ir_lower) fn try_emit_ir_vh_const_array_prefix(&mut self, id: NodeId, info: &JitInvokeInfo) {
        let Some(shape) = vh_const_array_site_shape(info.method_name, info.descriptor) else {
            return;
        };
        // JIT round 21 wave 1, lane updaters (UPDATERS21-5).
        if shape.kind == b'L' {
            self.try_emit_ir_vh_const_ref_array_prefix(id, info, &shape);
            return;
        }
        let graph = self.graph;
        let Some(node) = graph.nodes.get(id as usize) else {
            return;
        };
        // [ctrl, mem, handle, array, index, values...]
        if node.inputs.len() != 5 + shape.values.len() {
            return;
        }
        let want_ty = match shape.ret {
            b'V' => IrType::Void,
            b'J' => IrType::Long,
            _ => IrType::Int,
        };
        if node.ty != want_ty || node.inputs[2..].iter().any(|&i| self.carry_names(i)) {
            return;
        }
        let handle = node.inputs[2];
        let Some(Op::LoadStatic {
            class_id,
            field_index,
            ..
        }) = graph.nodes.get(handle as usize).map(|n| &n.op)
        else {
            return;
        };
        let (static_class, static_field) = (*class_id, *field_index as usize); // Cast: u32 index
        let Some(plan_kind) = vh_const_array_plan(&self.direct_helpers, static_class, static_field)
        else {
            return;
        };
        let Some(core) = vh_const_array_core(&shape, plan_kind) else {
            return;
        };
        let slot = if want_ty == IrType::Void {
            None
        } else {
            match self.prefix_result_slot(id, want_ty) {
                Some(slot) => slot,
                None => return,
            }
        };
        let (array, index) = (node.inputs[3], node.inputs[4]);
        let (expected, operand) = if shape.has_expected() {
            (Some(node.inputs[5]), Some(node.inputs[6]))
        } else if shape.values.len() == 1 {
            (None, Some(node.inputs[5]))
        } else {
            (None, None)
        };
        if crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] vh-const-array-inline (ir) n{id} {}{} kind={} \
                 static=({static_class},{static_field})",
                info.method_name,
                info.descriptor,
                char::from(core.kind),
            );
        }
        self.note_rcx_written();
        // RAX = the array, R11 = the index, then R10 / RDX (loads into R10 /
        // RDX leave RAX and R11 alone, as the `Unsafe` RMW prefix relies on).
        self.gp_load_value(RAX, array);
        self.gp_load_value(R11, index);
        if let Some(expected) = expected {
            self.gp_load_value(R10, expected);
        }
        if let Some(operand) = operand {
            self.gp_load_value(RDX, operand);
        }
        let (bytes, edges) = vh_const_array_core_bytes(&core);
        let at = self.buf.pos();
        self.buf.emit(&bytes);
        let decline: Vec<usize> = edges.into_iter().map(|e| at + e).collect();
        // The call route's `store_rax(slot)`, as the other prefixes write it.
        if want_ty != IrType::Void {
            self.emit_prefix_result_store(id, slot);
        }
        let hit = self.emit_jmp_rel32();
        for p in decline {
            self.patch_rel32_to_here(p);
        }
        self.call_prefix_join = Some(hit);
    }

    /// JIT round 21 wave 1, lane updaters (UPDATERS21-5,
    /// `CRATONVM_JIT_VH_CONST_REF_ARRAY_GET`, opt-in): the reference-array
    /// GET arm of [`Self::try_emit_ir_vh_const_array_prefix`], with the same
    /// prefix contract. The section above `vh_const_ref_array_get_enabled`
    /// says what is served and why.
    pub(in crate::ir_lower) fn try_emit_ir_vh_const_ref_array_prefix(
        &mut self,
        id: NodeId,
        info: &JitInvokeInfo,
        shape: &VhConstShape,
    ) {
        // JIT round 21 wave 22, lane vhconst (UPDATERS21-5,
        // `CRATONVM_JIT_VH_CONST_REF_ARRAY_STORE`): the store modes.
        if shape.op != VhConstOp::Get {
            self.try_emit_ir_vh_const_ref_array_store_prefix(id, info, shape);
            return;
        }
        let graph = self.graph;
        let Some(node) = graph.nodes.get(id as usize) else {
            return;
        };
        // [ctrl, mem, handle, array, index]
        if node.inputs.len() != 5
            || node.ty != IrType::Ref
            || node.inputs[2..].iter().any(|&i| self.carry_names(i))
        {
            return;
        }
        let handle = node.inputs[2];
        let Some(Op::LoadStatic {
            class_id,
            field_index,
            ..
        }) = graph.nodes.get(handle as usize).map(|n| &n.op)
        else {
            return;
        };
        let (static_class, static_field) = (*class_id, *field_index as usize); // Cast: u32 index
        let Some(plan_kind) = vh_const_array_plan(&self.direct_helpers, static_class, static_field)
        else {
            return;
        };
        let Some(core) = vh_const_ref_array_core(
            &self.direct_helpers,
            shape,
            plan_kind,
            info.declaring_class_id,
            0,
        ) else {
            return;
        };
        let Some(slot) = self.prefix_result_slot(id, node.ty) else {
            return;
        };
        let (array, index) = (node.inputs[3], node.inputs[4]);
        if crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] vh-const-ref-array-inline (ir) n{id} {}{} \
                 static=({static_class},{static_field}) cast={:?}",
                info.method_name, info.descriptor, core.result_check,
            );
        }
        self.note_rcx_written();
        self.gp_load_value(RAX, array);
        self.gp_load_value(R11, index);
        let (bytes, edges) = vh_const_ref_array_get_bytes(&core);
        let at = self.buf.pos();
        self.buf.emit(&bytes);
        let decline: Vec<usize> = edges.into_iter().map(|e| at + e).collect();
        self.emit_prefix_result_store(id, slot);
        let hit = self.emit_jmp_rel32();
        for p in decline {
            self.patch_rel32_to_here(p);
        }
        self.call_prefix_join = Some(hit);
    }

    /// JIT round 21 wave 22, lane vhconst (UPDATERS21-5,
    /// `CRATONVM_JIT_VH_CONST_REF_ARRAY_STORE`, opt-in): the reference-array
    /// STORE arm (set / CAS / compareAndExchange / getAndSet) of
    /// [`Self::try_emit_ir_vh_const_array_prefix`], with the same prefix
    /// contract. The section above `vh_const_cast_cae_enabled` says what is
    /// served and why. The first half's screens (array, gates, store check)
    /// spend every scratch register, so the index, `expected` and the value
    /// are loaded again for the second half (index screen and the store):
    ///
    /// ```text
    ///   RAX = array ; RDX = value ; R10 = expected
    ///   <vh_const_ref_array_store_screen_bytes>
    ///   R11 = index ; R10 = expected ; RDX = value
    ///   <vh_const_ref_array_store_access_bytes>
    /// ```
    pub(in crate::ir_lower) fn try_emit_ir_vh_const_ref_array_store_prefix(
        &mut self,
        id: NodeId,
        info: &JitInvokeInfo,
        shape: &VhConstShape,
    ) {
        let graph = self.graph;
        let Some(node) = graph.nodes.get(id as usize) else {
            return;
        };
        // [ctrl, mem, handle, array, index, values...]
        if node.inputs.len() != 5 + shape.values.len() || shape.values.is_empty() {
            return;
        }
        let want_ty = match shape.ret {
            b'V' => IrType::Void,
            b'Z' => IrType::Int,
            _ => IrType::Ref,
        };
        if node.ty != want_ty || node.inputs[2..].iter().any(|&i| self.carry_names(i)) {
            return;
        }
        let handle = node.inputs[2];
        let Some(Op::LoadStatic {
            class_id,
            field_index,
            ..
        }) = graph.nodes.get(handle as usize).map(|n| &n.op)
        else {
            return;
        };
        let (static_class, static_field) = (*class_id, *field_index as usize); // Cast: u32 index
        let Some(plan_kind) = vh_const_array_plan(&self.direct_helpers, static_class, static_field)
        else {
            return;
        };
        let Some(core) = vh_const_ref_array_store_core(
            &self.direct_helpers,
            shape,
            plan_kind,
            self.ref_store_gates,
            self.ref_store_post_skip_mask,
            0,
        ) else {
            return;
        };
        let slot = if want_ty == IrType::Void {
            None
        } else {
            match self.prefix_result_slot(id, want_ty) {
                Some(slot) => slot,
                None => return,
            }
        };
        let (array, index) = (node.inputs[3], node.inputs[4]);
        let (expected, operand) = if shape.has_expected() {
            (Some(node.inputs[5]), node.inputs[6])
        } else {
            (None, node.inputs[5])
        };
        if crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] vh-const-ref-array-store-inline (ir) n{id} {}{} \
                 static=({static_class},{static_field}) display={}",
                info.method_name,
                info.descriptor,
                core.display_cell.is_some(),
            );
        }
        self.note_rcx_written();
        self.gp_load_value(RAX, array);
        if let Some(expected) = expected {
            self.gp_load_value(R10, expected);
        }
        self.gp_load_value(RDX, operand);
        let mut decline: Vec<usize> = Vec::new();
        let (bytes, edges) = vh_const_ref_array_store_screen_bytes(&core);
        let at = self.buf.pos();
        self.buf.emit(&bytes);
        decline.extend(edges.into_iter().map(|e| at + e));
        // The screens spent RCX, RDX, R10 and R11; RAX still holds the array.
        // r21 review #10 review point: reload registers must not touch RAX.
        debug_assert_ne!(R11, RAX);
        debug_assert_ne!(R10, RAX);
        debug_assert_ne!(RDX, RAX);
        self.gp_load_value(R11, index);
        if let Some(expected) = expected {
            self.gp_load_value(R10, expected);
        }
        self.gp_load_value(RDX, operand);
        let (bytes, edges) = vh_const_ref_array_store_access_bytes(&core);
        let at = self.buf.pos();
        self.buf.emit(&bytes);
        decline.extend(edges.into_iter().map(|e| at + e));
        // The call route's `store_rax(slot)`, as the other prefixes write it.
        if want_ty != IrType::Void {
            self.emit_prefix_result_store(id, slot);
        }
        let hit = self.emit_jmp_rel32();
        for p in decline {
            self.patch_rel32_to_here(p);
        }
        self.call_prefix_join = Some(hit);
    }

    /// The `bigEndian` byte swap of [`Self::try_emit_ir_unsafe_access_prefix`],
    /// on the low `width` bytes of `reg` (RAX or RDX). Nothing for a form
    /// without the operand or a constant `false`; unconditional for a constant
    /// `true`; otherwise behind `TEST ECX, ECX` on the operand loaded into RCX.
    /// A 2-byte swap leaves the upper bits as they were; the caller widens.
    pub(in crate::ir_lower) fn emit_unsafe_inline_swap(
        &mut self,
        width: u8,
        big_endian: Option<NodeId>,
        big_endian_const: Option<bool>,
        reg: u8,
    ) {
        let Some(be) = big_endian else {
            return;
        };
        if width == 1 || big_endian_const == Some(false) {
            return;
        }
        let skip = if big_endian_const == Some(true) {
            None
        } else {
            self.gp_load_value(RCX, be);
            self.buf.emit(&[0x85, 0xC9]); // TEST ECX, ECX
            Some(self.emit_jcc_rel32(0x84)) // JZ: little-endian
        };
        let r = reg & 7; // RAX = 0, RDX = 2
        match width {
            2 => self.buf.emit(&[0x66, 0xC1, 0xC0 | r, 0x08]), // ROL r16, 8
            4 => self.buf.emit(&[0x0F, 0xC8 | r]),             // BSWAP r32
            _ => self.buf.emit(&[0x48, 0x0F, 0xC8 | r]),       // BSWAP r64
        }
        if let Some(p) = skip {
            self.patch_rel32_to_here(p);
        }
    }

    /// NIO19-9 (nio20 wave 1, lane L2): `Reference.reachabilityFence(Object)`
    /// as NO code, which is what HotSpot emits for it.
    ///
    /// The method's whole contract is that its referent is strongly reachable
    /// up to the call. This tier keeps the `Op::Call` node in the graph (it is
    /// impure, so nothing deletes it, and it stays the memory barrier it always
    /// was), with the referent as its input: liveness is computed from the
    /// GRAPH, so the referent's register or home stays allocated and stays in
    /// every safepoint map up to this node -- exactly the coverage the thin
    /// `jit_reachability_fence_direct` CALL bought, minus the CALL. The one map
    /// it no longer publishes is the one AT the fence, which was only owed
    /// because the CALL was a GC point.
    ///
    /// Under `--real-jdk` the same site may be bound to the compiled empty JDK
    /// body instead; that body is `{}`, so dropping the call is inlining it.
    ///
    /// `CRATONVM_JIT_IR_REACHABILITY_FENCE_KEEPALIVE=0` keeps the call. Declines
    /// (keeps the call) for a shape that is not the static `(Object)V` site and
    /// for a referent a carry is holding (a carry must be consumed by a read).
    pub(in crate::ir_lower) fn try_lower_ir_reachability_fence(&mut self, id: NodeId, info_ptr: usize) -> bool {
        if info_ptr == 0
            || !true
        {
            return false;
        }
        // SAFETY: as every other read of an `Op::Call`'s `info_ptr` in this
        // file -- a live `JitInvokeInfo` owned by `ir_call_infos`.
        let info: &JitInvokeInfo = unsafe { &*(info_ptr as *const JitInvokeInfo) };
        if info.invoke_kind != 3
            || !crate::is_reachability_fence_site(
                info.class_name,
                info.method_name,
                info.descriptor,
            )
        {
            return false;
        }
        let graph = self.graph;
        let Some(node) = graph.nodes.get(id as usize) else {
            return false;
        };
        if node.inputs.len() != 3 || node.ty != IrType::Void || self.carry_names(node.inputs[2]) {
            return false;
        }
        // The slot every `Op::Call` takes, for layout uniformity; nothing
        // writes it (the node is void).
        let _ = self.alloc_slot(id);
        if crate::x64::dbg_jitc_enabled() {
            eprintln!("[cratonvm-jitc] ir-reachability-fence keep-alive n{id} (no code)");
        }
        true
    }

    /// ARRAYLIST_ACCESS in the optimizing tier (round 9 wave 9, irl9): the
    /// inline `ArrayList.get(int)` / `size()` fast path as a guarded PREFIX of
    /// an `Op::Call`'s ordinary lowering. The twin of
    /// `x64/op_invoke.rs::try_emit_arraylist_access_prefix`, which only the
    /// single-pass tier ran: a `[c2]` body's `List.get` stayed a MIC/native
    /// dispatch (`AlProbe list` on w8b: ~1.8 s per 400k x 16 calls from the C2
    /// body, 30 ms from the C1 body that has the prefix).
    ///
    /// Page: `perf-collection-natives-cost-a-generic-native-dispatch-per-call`
    /// (`NOTES-w8-arraylist8.md` cross-lane request 2).
    ///
    /// # Contract with the `Op::Call` arm
    ///
    /// Called first in the arm, before the safepoint map and before a byte of
    /// the call is emitted. Every decline edge lands on the byte after the
    /// prefix, i.e. on the UNCHANGED call lowering (MIC/PIC, direct call or
    /// dispatch), which then runs the registered native exactly as before. The
    /// hit edge writes the node's result exactly as the call route's final
    /// `store_rax` would at run time (home word, plus the resident register
    /// when the value has one) and jumps to [`Self::call_prefix_join`], which
    /// `lower_data_node_tracked` patches to the end of the node's lowering.
    /// Compile-time state is left as the call route leaves it, and that is
    /// sound for the hit path: it writes only RAX, RCX, RDX, R10, R11 (this
    /// lowerer's per-opcode scratch, never an allocated value's register), the
    /// result's home and its own register -- a strict subset of what the call
    /// route's run-time effect is -- and it makes no call, so there is no
    /// collection, no frame-record republish and no shadow reload owed.
    ///
    /// # What the fast path answers
    ///
    /// Exactly the single-pass prefix's answers (see its doc for the sequence
    /// and for why each screen exists): an exact `java/util/ArrayList` plain
    /// object, compact or legacy (both cells present and tagged), `size >= 0`,
    /// a non-null reference-array `elementData`, for `get` an index below
    /// both `size` and the array length (unsigned, so a negative index
    /// declines), and no stale-view marker in the first spare slot. Anything
    /// else declines to the call. Never a deopt, never an exception.
    ///
    /// # Declines at compile time
    ///
    /// No layout on this compile, `CRATONVM_NO_JIT_ARRAYLIST_INTRINSICS=1`, a
    /// site `try_resolve_arraylist_intrinsic` does not name, `invokespecial`,
    /// an armed ZGC read barrier, an argument count or result type that does
    /// not match the resolved member, an operand some carry is holding, and a
    /// result that is itself carried or has its home dropped (the hit edge
    /// writes the home word; those two shapes have no home to write).
    pub(in crate::ir_lower) fn try_emit_ir_arraylist_access_prefix(&mut self, id: NodeId, info_ptr: usize) {
        let Some(layout) = self.array_list_layout else {
            return;
        };
        if info_ptr == 0 || crate::arraylist_intrinsics_disabled() {
            return;
        }
        // SAFETY: as every other read of an `Op::Call`'s `info_ptr` in this
        // file -- the pointer names a live `JitInvokeInfo` owned by
        // `ir_call_infos` for the lifetime of this compile.
        let info: &JitInvokeInfo = unsafe { &*(info_ptr as *const JitInvokeInfo) };
        let Some((entry, num_params, _ret, guard)) = crate::try_resolve_arraylist_intrinsic(
            info.class_name,
            info.method_name,
            info.descriptor,
            Some(layout),
        ) else {
            return;
        };
        // Virtual (0) and interface (2) only, as in the single-pass tier.
        if !matches!(info.invoke_kind, 0 | 2) || guard == 0 {
            return;
        }
        if crate::x64::zgc_read_barrier_blocks_inline_fields() {
            return;
        }
        let is_get = entry == crate::JitIntrinsic::ArrayListGet.as_entry();
        let graph = self.graph;
        let Some(node) = graph.nodes.get(id as usize) else {
            return;
        };
        if node.inputs.len() != num_params + 3 {
            return; // [ctrl, mem, receiver, params...]
        }
        let want_ty = if is_get { IrType::Ref } else { IrType::Int };
        if node.ty != want_ty {
            return;
        }
        let recv = node.inputs[2];
        let index = if is_get { Some(node.inputs[3]) } else { None };
        if self.carry_names(recv) || index.is_some_and(|i| self.carry_names(i)) {
            return;
        }
        let Some(slot) = self.prefix_result_slot(id, want_ty) else {
            return;
        };
        if crate::ir_stage_reporting() {
            eprintln!(
                "[ir] arraylist-intrinsic n{id} {}.{}{} guard_class_id={guard}",
                info.class_name, info.method_name, info.descriptor,
            );
        }

        let mut decline: Vec<usize> = Vec::new();
        // RCX: the epoch guard's fallback form and the index load write it.
        self.note_rcx_written();
        // A layout REPLACEMENT invalidates the baked compact offsets.
        if let Some(p) = self.emit_layout_epoch_guard() {
            decline.push(p);
        }
        // RAX = receiver; null declines (the native reports the NPE).
        self.gp_load_value(RAX, recv);
        self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
        decline.push(self.emit_jcc_rel32(0x84)); // JZ
                                                 // A plain object only: an array header carries its COMPONENT's id.
                                                 // TEST BYTE [RAX+KIND_TAGS], KIND_TAG_BYTE_MASK (F6 /0 ib) -- kind bits only.
        self.buf.emit(&[
            0xF6,
            0x40,
            cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8, // Cast: x86-64 disp8
            cratonvm_types::KIND_TAG_BYTE_MASK,
        ]);
        decline.push(self.emit_jcc_rel32(0x85)); // JNE
                                                 // CMP DWORD [RAX+0], ArrayList -- `class_id` is the `u32` at offset 0.
        self.buf.emit(&[0x81, 0x78, 0x00]);
        self.buf.emit(&guard.to_le_bytes());
        decline.push(self.emit_jcc_rel32(0x85)); // JNE
                                                 // TEST BYTE [RAX+GC_FLAGS], COMPACT (F6 /0 ib) ; JZ legacy.
        self.buf.emit(&[
            0xF6,
            0x40,
            cratonvm_types::GC_FLAGS_BYTE_OFFSET as u8, // Cast: x86-64 disp8
            cratonvm_types::GC_FLAG_COMPACT,
        ]);
        let legacy = self.emit_jcc_rel32(0x84);
        // COMPACT: R10D = size, RDX = elementData.
        self.emit_load_base_disp32(R10, RAX, layout.size_compact_offset, false);
        self.emit_load_base_disp32(RDX, RAX, layout.data_compact_offset, true);
        let have = self.emit_jmp_rel32();
        // LEGACY: both cells present, each carrying the tag it is read as.
        self.patch_rel32_to_here(legacy);
        // CMP DWORD [RAX+NUM_SLOTS], min_num_slots ; JB decline.
        self.buf.emit(&[0x81, 0xB8]);
        self.buf
            .emit(&(cratonvm_types::NUM_SLOTS_OFFSET as i32).to_le_bytes()); // Cast: small header constant
        self.buf.emit(&layout.min_num_slots.to_le_bytes());
        decline.push(self.emit_jcc_rel32(0x82)); // JB
                                                 // CMP DWORD [RAX+size_tag], 0 (`Value::Int`) ; JNE decline.
        self.buf.emit(&[0x81, 0xB8]);
        self.buf.emit(&layout.size_legacy_tag_offset.to_le_bytes());
        self.buf.emit(&0u32.to_le_bytes());
        decline.push(self.emit_jcc_rel32(0x85));
        // CMP DWORD [RAX+data_tag], FIELD_CELL_TAG_OBJECT ; JNE decline.
        self.buf.emit(&[0x81, 0xB8]);
        self.buf.emit(&layout.data_legacy_tag_offset.to_le_bytes());
        self.buf
            .emit(&cratonvm_types::FIELD_CELL_TAG_OBJECT.to_le_bytes());
        decline.push(self.emit_jcc_rel32(0x85));
        self.emit_load_base_disp32(R10, RAX, layout.size_legacy_offset, false);
        self.emit_load_base_disp32(RDX, RAX, layout.data_legacy_offset, true);

        self.patch_rel32_to_here(have);
        self.buf.emit(&[0x45, 0x85, 0xD2]); // TEST R10D, R10D
        decline.push(self.emit_jcc_rel32(0x88)); // JS: size >= 0
        self.buf.emit(&[0x48, 0x85, 0xD2]); // TEST RDX, RDX
        decline.push(self.emit_jcc_rel32(0x84)); // JZ: elementData != null
        let ref_array_tags = (cratonvm_types::ObjectKind::Array as u8)
            | ((cratonvm_types::ArrayElementType::Reference as u8) << 2);
        // CMP BYTE [RDX+KIND_TAGS], Array|Reference (80 /7 ib, rm=RDX).
        self.buf.emit(&[
            0x80,
            0x7A,
            cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8, // Cast: x86-64 disp8
            ref_array_tags,
        ]);
        decline.push(self.emit_jcc_rel32(0x85));
        // R11D = elementData.length (zero-extends into R11).
        self.emit_load_base_disp32(
            R11,
            RDX,
            cratonvm_types::ARRAY_LENGTH_OFFSET as i32, // Cast: small header constant
            false,
        );
        if let Some(idx) = index {
            self.gp_load_value(RCX, idx);
            // MOV ECX, ECX: the element load scales the FULL register.
            self.buf.emit(&[0x89, 0xC9]);
            self.buf.emit(&[0x44, 0x39, 0xD1]); // CMP ECX, R10D (index vs size)
            decline.push(self.emit_jcc_rel32(0x83)); // JAE
            self.buf.emit(&[0x44, 0x39, 0xD9]); // CMP ECX, R11D (index vs length)
            decline.push(self.emit_jcc_rel32(0x83)); // JAE
        }
        // `view_source_in`: with spare capacity, a non-null LAST slot names a
        // source map this list is a stale view of; the native resyncs it.
        self.buf.emit(&[0x45, 0x39, 0xDA]); // CMP R10D, R11D (size vs length)
        let no_marker = self.emit_jcc_rel32(0x83); // JAE: no spare capacity
                                                   // CMP QWORD [RDX + R11*8 + (DATA - 8)], 0 -- REX.W|REX.X, 83 /7 ib,
                                                   // ModRM mod=01 rm=SIB, SIB scale 8 index=R11 base=RDX.
        self.buf.emit(&[
            0x4A,
            0x83,
            0x7C,
            0xDA,
            (cratonvm_types::ARRAY_DATA_OFFSET - cratonvm_types::REF_ELEMENT_SIZE) as u8, // Cast: x86-64 disp8
            0x00,
        ]);
        decline.push(self.emit_jcc_rel32(0x85));
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
            self.buf.emit(&[0x49, 0x63, 0xC2]); // MOVSXD RAX, R10D
        }
        // The call route's `store_rax(slot)` at run time, with no carry and a
        // home that is not dropped (both refused above): the resident
        // register when the value has one and definitions publish, then the
        // home word. No compile-time state is touched -- the call route's
        // own store sets it, and it describes the join for both paths.
        //
        // Round 11 (irlower): whenever the value HAS a register, not only when
        // definitions publish. With `CRATONVM_JIT_IR_PUBLISH_AT_DEF=0` the call
        // route's register is written by the generic publish at the end of
        // `lower_data_node` -- which this hit path jumps PAST (the join is
        // patched after the whole arm) -- while the compile-time state says the
        // register is live on both paths. Writing it here is what makes that
        // true on this one.
        self.emit_prefix_result_store(id, slot);
        let hit = self.emit_jmp_rel32();
        for p in decline {
            self.patch_rel32_to_here(p);
        }
        self.call_prefix_join = Some(hit);
    }

    /// nio20 wave 1, lane L3 (NIO19-5, was NIO17-1 / BUF18-1): may the
    /// `Op::Call` described by `info` take [`Self::try_emit_ir_getclass_prefix`]?
    /// The compile-time half of its screens, shared with
    /// [`Self::call_site_may_take_inline_prefix`] so the dead-home planner and
    /// the emitter cannot disagree about which sites have a prefix.
    pub(in crate::ir_lower) fn getclass_prefix_admits(&self, info: &JitInvokeInfo) -> bool {
        matches!(info.invoke_kind, 0 | 2)
            && crate::is_object_get_class_site(info.method_name, info.descriptor)
            // JIT r20 (lane qMisc, correctness): not a site that resolves to a
            // class's own private / static `getClass()`.
            && !self
                .graph
                .getclass_prefix_refused_infos
                .contains(&(info as *const JitInvokeInfo as usize))
            && self.direct_helpers.getclass_mirror_words != 0
            && crate::getclass_inline_enabled()
    }

    /// `Object.getClass()` in the optimizing tier as two loads (nio20 wave 1,
    /// lane L3, NIO19-5): the receiver's class id from its header, then that
    /// class's word in the VM's compiled-`getClass()` table
    /// (`DirectHelperTable::getclass_mirror_words`; the VM crate's
    /// `ClassRealm::getclass_mirror_words` documents who writes it and why a
    /// non-zero word always equals `class_mirrors[class_id]`). HotSpot's
    /// `getClass` is the same two loads (klass from the header, mirror from
    /// the klass).
    ///
    /// A guarded PREFIX of the call's unchanged lowering, with the contract of
    /// [`Self::try_emit_ir_arraylist_access_prefix`]: scratch RAX and R10
    /// only; the hit edge writes the result home (and register) and jumps to
    /// [`Self::call_prefix_join`]; every decline edge lands on the call, which
    /// is `jit_object_get_class_direct` (or the MIC route to the native) and
    /// answers exactly what it answered before:
    ///
    /// * a null receiver -- the call raises the JEP 358 receiver NPE;
    /// * a non-plain object -- an array's header carries its COMPONENT's id
    ///   (since nio21 wave 1 L5 a one-dimensional PRIMITIVE array is answered
    ///   from the table's primitive-array band, keyed by its tag byte,
    ///   [`getclass_prim_array_lookup_bytes`], and since nio21 wave 2 L5 a
    ///   REFERENCE array from the reference-array band keyed by that component
    ///   id, [`getclass_array_arm_bytes`]; an unpublished word still declines);
    /// * a class id past the table, or a class with no published word (no
    ///   mirror yet, a display verdict not yet memoised as the stamp, or a
    ///   class the native remaps -- those are never published).
    ///
    /// ```text
    ///   MOV  RAX, recv ; TEST RAX,RAX ; JZ call
    ///   TEST BYTE [RAX+KIND_TAGS], KIND_TAG_BYTE_MASK ; JNZ call
    ///   MOV  EAX, [RAX]                    ; class id, zero-extended
    ///   MOV  R10, imm64 table
    ///   CMP  RAX, [R10] ; JAE call         ; word 0 = capacity
    ///   MOV  RAX, [R10 + RAX*8 + 8] ; TEST RAX,RAX ; JZ call
    ///   <store RAX to the result home> ; JMP join
    /// ```
    ///
    /// Declines at compile time under `CRATONVM_JIT_GETCLASS_INLINE=0`, with
    /// no table on this compile (a door that did not publish one, a non-x86-64
    /// VM), under an armed ZGC read barrier (the gate every inline reference
    /// load shares; the words are rewritten only while mutators are parked,
    /// never colored), and for the carry / dropped-home shapes the other
    /// prefixes refuse.
    pub(in crate::ir_lower) fn try_emit_ir_class_mirror_fast_prefix(
        &mut self,
        id: NodeId,
        info_ptr: usize,
    ) {
        if info_ptr == 0
            || self.direct_helpers.mirror_fast_table_ptr == 0
            || self.direct_helpers.mirror_fast_epoch_addr == 0
            || !crate::ir_thin_prefix_enabled()
            || crate::x64::narrow_oops_block_inline_fields()
        {
            return;
        }
        // SAFETY: the planner keeps each nonzero call row alive through lowering.
        let info: &JitInvokeInfo = unsafe { &*(info_ptr as *const JitInvokeInfo) };
        if !matches!(info.invoke_kind, 0 | 1) {
            return;
        }
        let is_super = self.direct_helpers.class_get_superclass_inline_admitted
            && crate::is_class_get_superclass_site(
                info.class_name,
                info.method_name,
                info.descriptor,
            );
        let is_name = !is_super
            && self.direct_helpers.class_get_name_inline_admitted
            && crate::is_class_get_name_site(
                info.class_name,
                info.method_name,
                info.descriptor,
            );
        if !is_super && !is_name {
            return;
        }
        let graph = self.graph;
        let Some(node) = graph.nodes.get(id as usize) else {
            return;
        };
        if node.inputs.len() != 3 || node.ty != IrType::Ref {
            return;
        }
        let recv = node.inputs[2];
        if self.carry_names(recv) {
            return;
        }
        if self.carry_of.get(id as usize).copied().flatten().is_some()
            || self.home_dropped.get(id as usize).copied().unwrap_or(false)
        {
            return;
        }
        let Ok(slot) = self.planned_slot_off(id) else {
            return;
        };
        let mut decline: Vec<usize> = Vec::new();
        self.gp_load_value(RAX, recv);
        self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
        decline.push(self.emit_jcc_rel32(0x84)); // JZ
        if self.direct_helpers.zgc_read_barrier_armed_addr != 0 {
            self.emit_mov_reg_imm64(R10, self.direct_helpers.zgc_read_barrier_armed_addr as u64);
            self.buf.emit(&[0x41, 0x80, 0x3A, 0x00]); // CMP BYTE PTR [R10], 0
            decline.push(self.emit_jcc_rel32(0x85)); // JNE
        }
        // RCX = ((RAX >> 3) ^ (RAX >> 13)) & 4095 << 5
        self.buf.emit(&[0x48, 0x89, 0xC1]); // MOV RCX, RAX
        self.buf.emit(&[0x48, 0xC1, 0xE9, 0x03]); // SHR RCX, 3
        self.buf.emit(&[0x48, 0x89, 0xCA]); // MOV RDX, RCX
        self.buf.emit(&[0x48, 0xC1, 0xEA, 0x0A]); // SHR RDX, 10
        self.buf.emit(&[0x48, 0x31, 0xD1]); // XOR RCX, RDX
        self.buf.emit(&[0x81, 0xE1, 0xFF, 0x0F, 0x00, 0x00]); // AND ECX, 4095
        self.buf.emit(&[0x48, 0xC1, 0xE1, 0x05]); // SHL RCX, 5
        self.emit_mov_reg_imm64(R10, self.direct_helpers.mirror_fast_table_ptr as u64);
        self.buf.emit(&[0x49, 0x01, 0xCA]); // ADD R10, RCX
        self.buf.emit(&[0x41, 0x8B, 0x12]); // MOV EDX, [R10] (s1 = seq)
        self.buf.emit(&[0xF6, 0xC2, 0x01]); // TEST DL, 1
        decline.push(self.emit_jcc_rel32(0x85)); // JNZ
        self.emit_mov_reg_imm64(R11, self.direct_helpers.mirror_fast_epoch_addr as u64);
        self.buf.emit(&[0x45, 0x8B, 0x1B]); // MOV R11D, [R11]
        self.buf.emit(&[0x45, 0x39, 0x5A, 0x04]); // CMP [R10 + 4], R11D
        decline.push(self.emit_jcc_rel32(0x85)); // JNE
        self.buf.emit(&[0x49, 0x39, 0x42, 0x08]); // CMP [R10 + 8], RAX
        decline.push(self.emit_jcc_rel32(0x85)); // JNE
        let val_disp = if is_super { 16u8 } else { 24u8 };
        self.buf.emit(&[0x49, 0x8B, 0x42, val_disp]); // MOV RAX, [R10 + disp]
        self.buf.emit(&[0x41, 0x39, 0x12]); // CMP [R10], EDX (s1 == s2)
        decline.push(self.emit_jcc_rel32(0x85)); // JNE
        if is_super {
            self.buf.emit(&[0x48, 0x83, 0xF8, 0x01]); // CMP RAX, 1
            decline.push(self.emit_jcc_rel32(0x84)); // JE (uncached)
        } else {
            self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
            decline.push(self.emit_jcc_rel32(0x84)); // JZ (uncached)
        }
        if let Some(reg) = self.assigned_gpr(id) {
            self.emit_mov_reg_reg64(reg, RAX);
        }
        let mut bytes = FrameAccess::new();
        enc_frame_store(RAX, slot, &mut bytes);
        self.buf.emit(bytes.as_slice());
        let hit = self.emit_jmp_rel32();
        for p in decline {
            self.patch_rel32_to_here(p);
        }
        self.call_prefix_join = Some(hit);
    }

    pub(in crate::ir_lower) fn try_emit_ir_reference_get0_prefix(&mut self, id: NodeId, info_ptr: usize) {
        if info_ptr == 0
            || !self.direct_helpers.reference_get0_inline_admitted
            || !crate::ir_thin_prefix_enabled()
            || crate::x64::narrow_oops_block_inline_fields()
        {
            return;
        }
        // SAFETY: the planner keeps each nonzero call row alive through lowering.
        let info: &JitInvokeInfo = unsafe { &*(info_ptr as *const JitInvokeInfo) };
        if !matches!(info.invoke_kind, 0 | 1)
            || info.class_name != "java/lang/ref/Reference"
            || info.method_name != "get0"
            || info.descriptor != "()Ljava/lang/Object;"
        {
            return;
        }
        let graph = self.graph;
        let Some(node) = graph.nodes.get(id as usize) else {
            return;
        };
        if node.inputs.len() != 3 || node.ty != IrType::Ref {
            return;
        }
        let recv = node.inputs[2];
        if self.carry_names(recv) {
            return;
        }
        if self.carry_of.get(id as usize).copied().flatten().is_some()
            || self.home_dropped.get(id as usize).copied().unwrap_or(false)
        {
            return;
        }
        let Ok(slot) = self.planned_slot_off(id) else {
            return;
        };
        let mut decline: Vec<usize> = Vec::new();
        self.gp_load_value(RAX, recv);
        self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
        decline.push(self.emit_jcc_rel32(0x84)); // JZ
        self.buf.emit(&[
            0xF6,
            0x40,
            cratonvm_types::GC_FLAGS_BYTE_OFFSET as u8,
            cratonvm_types::GC_FLAG_COMPACT,
        ]);
        decline.push(self.emit_jcc_rel32(0x84)); // JZ (not compact)
        if self.direct_helpers.zgc_read_barrier_armed_addr != 0 {
            self.emit_mov_reg_imm64(R10, self.direct_helpers.zgc_read_barrier_armed_addr as u64);
            self.buf.emit(&[0x41, 0x80, 0x3A, 0x00]); // CMP BYTE PTR [R10], 0
            decline.push(self.emit_jcc_rel32(0x85)); // JNE
        }
        if self.direct_helpers.zgc_referent_gate_state_addr != 0 {
            self.emit_mov_reg_imm64(R10, self.direct_helpers.zgc_referent_gate_state_addr as u64);
            self.buf.emit(&[0x41, 0x80, 0x3A, 0x02]); // CMP BYTE PTR [R10], 2
            decline.push(self.emit_jcc_rel32(0x84)); // JE
        }
        self.buf.emit(&[0x48, 0x8B, 0x40, 0x08]); // MOV RAX, [RAX + 8]
        if let Some(reg) = self.assigned_gpr(id) {
            self.emit_mov_reg_reg64(reg, RAX);
        }
        let mut bytes = FrameAccess::new();
        enc_frame_store(RAX, slot, &mut bytes);
        self.buf.emit(bytes.as_slice());
        let hit = self.emit_jmp_rel32();
        for p in decline {
            self.patch_rel32_to_here(p);
        }
        self.call_prefix_join = Some(hit);
    }

    pub(in crate::ir_lower) fn try_emit_ir_getclass_prefix(&mut self, id: NodeId, info_ptr: usize) {
        if info_ptr == 0 {
            return;
        }
        // SAFETY: as every other read of an `Op::Call`'s `info_ptr` in this
        // file -- a live `JitInvokeInfo` owned by `ir_call_infos`.
        let info: &JitInvokeInfo = unsafe { &*(info_ptr as *const JitInvokeInfo) };
        if !self.getclass_prefix_admits(info) {
            return;
        }
        // nio20 w3 L3 (W2L2-4): this tier's half of the prefix only.
        if crate::x64::zgc_read_barrier_blocks_inline_fields() || !crate::ir_thin_prefix_enabled() {
            return;
        }
        let table = self.direct_helpers.getclass_mirror_words;
        let graph = self.graph;
        let Some(node) = graph.nodes.get(id as usize) else {
            return;
        };
        // [ctrl, mem, receiver], a reference result.
        if node.inputs.len() != 3 || node.ty != IrType::Ref {
            return;
        }
        let recv = node.inputs[2];
        if self.carry_names(recv) {
            return;
        }
        let Some(slot) = self.prefix_result_slot(id, node.ty) else {
            return;
        };
        if crate::ir_stage_reporting() {
            eprintln!(
                "[ir] getclass-prefix n{id} {}.{}{} table={table:#x}",
                info.class_name, info.method_name, info.descriptor,
            );
        }
        // nio20 wave 4, lane L2: the optimizing tier's twin of the single-pass
        // `object-getclass-inline (sp)` line, so one `CRATONVM_DBG_JITC` run
        // shows which tier's body carries the prefix; the VM's
        // `getclass-helper served=.. declined=..` lines count the misses.
        if crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] object-getclass-inline (ir) n{id} pc={:?} table={table:#x}",
                node.bytecode_pc,
            );
        }
        // nio21 wave 1 L5: a one-dimensional primitive array is answered from
        // the table's primitive-array band instead of declining; nio21 wave 2
        // L5: a reference array from the reference-array band.
        let prim_arrays = getclass_prim_array_words_enabled();
        let ref_arrays = getclass_ref_array_words_enabled();
        let mut decline: Vec<usize> = Vec::new();
        // RAX = receiver; null declines (the call raises the NPE at this bci).
        self.gp_load_value(RAX, recv);
        self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
        decline.push(self.emit_jcc_rel32(0x84)); // JZ
                                                 // R10 = the table (loaded before the kind test: both arms read it).
        self.emit_mov_reg_imm64(R10, table as u64); // Cast: the table's address
                                                    // A plain object takes the class-id lookup: an array header carries
                                                    // its COMPONENT's id. TEST BYTE [RAX+KIND_TAGS], KIND_TAG_BYTE_MASK
                                                    // (F6 /0 ib) ; JNE array arm (or decline).
        self.buf.emit(&[
            0xF6,
            0x40,
            cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8, // Cast: x86-64 disp8
            cratonvm_types::KIND_TAG_BYTE_MASK,
        ]);
        let not_plain = self.emit_jcc_rel32(0x85); // JNE
                                                   // MOV EAX, dword [RAX] -- `class_id`, the `u32` at offset 0; the
                                                   // 32-bit load zero-extends into RAX, so the index below is unsigned.
        self.buf.emit(&[0x8B, 0x00]);
        // The class's word (or, nio20 wave 5 L2, a lambda proxy's word in the
        // table's second band); an id with no word or an unpublished word
        // declines (`getclass_table_lookup_bytes`).
        let (lookup, edges) = getclass_table_lookup_bytes();
        let at = self.buf.pos();
        self.buf.emit(&lookup);
        decline.extend(edges.into_iter().map(|e| at + e));
        if prim_arrays || ref_arrays {
            // JMP loaded ; array: <the array arm> ; loaded:
            let to_loaded = self.emit_jmp_rel32();
            self.patch_rel32_to_here(not_plain);
            let (arm, edges) = getclass_array_arm_bytes(ref_arrays, prim_arrays);
            let at = self.buf.pos();
            self.buf.emit(&arm);
            decline.extend(edges.into_iter().map(|e| at + e));
            self.patch_rel32_to_here(to_loaded);
        } else {
            decline.push(not_plain);
        }
        // The call route's `store_rax(slot)` at run time, written as the
        // ArrayList prefix writes it: the resident register when the value
        // has one, then the home word.
        self.emit_prefix_result_store(id, slot);
        let hit = self.emit_jmp_rel32();
        for p in decline {
            self.patch_rel32_to_here(p);
        }
        self.call_prefix_join = Some(hit);
    }

    /// JIT round 20 wave 17, lane pAtom2: may the `Op::Call` described by
    /// `info` take [`Self::try_emit_ir_class_is_instance_prefix`]? The
    /// compile-time half of its screens, shared with
    /// [`Self::call_site_may_take_inline_prefix`]. `Class` is final, so the
    /// site is virtual (0) or the devirtualised special (1); the receiver is
    /// input 2 either way.
    pub(in crate::ir_lower) fn class_is_instance_prefix_admits(&self, info: &JitInvokeInfo) -> bool {
        matches!(info.invoke_kind, 0 | 1)
            && info.class_name == "java/lang/Class"
            && info.method_name == "isInstance"
            && info.descriptor == "(Ljava/lang/Object;)Z"
            && self.direct_helpers.getclass_mirror_words != 0
            && crate::getclass_inline_enabled()
            && crate::class_is_instance_inline_enabled()
    }

    /// `Class.isInstance(obj)` for an `obj` of EXACTLY the receiver's class, in
    /// the optimizing tier (JIT round 20 wave 17, lane pAtom2; the field
    /// updaters' `accessCheck` is `cclass.isInstance(obj)` on every access,
    /// `R20PAtomVolatileField` / `R20PAtomRmwNoSplice`): `obj`'s class id from
    /// its header, then that class's word in the compiled-`getClass()` table
    /// (exactly [`Self::try_emit_ir_getclass_prefix`]'s lookup); a word equal to
    /// the receiver mirror answers `true`. HotSpot's intrinsic for a constant
    /// mirror is the same exact-klass compare first.
    ///
    /// Sound because a non-zero word for class id `c` is `class_mirrors[c]`
    /// (`ClassRealm::getclass_mirror_words`), so the hit says `obj.getClass()`
    /// is the receiver, and `native_class_is_instance` answers `true` for that
    /// pair (its id compare). Every other shape declines to the call, which
    /// answers exactly what it answered before: a null `obj` (`false`), a null
    /// receiver (its NPE), an array (its header carries the component id), a
    /// subclass or interface instance, an id past the table, an unpublished
    /// word. The VM's site arm publishes the word on the call's first exact hit
    /// (`LeafNativeKind::ClassIsInstance`).
    ///
    /// Same contract with the `Op::Call` arm as the getClass prefix: scratch
    /// RAX and R10 only; no call, no GC point; the hit edge writes `1` home
    /// (and to the register) and jumps to [`Self::call_prefix_join`]. Declines
    /// at compile time under `CRATONVM_JIT_CLASS_IS_INSTANCE_INLINE=0`, without
    /// a table, under an armed ZGC read barrier, and for the carry /
    /// dropped-home shapes the other prefixes refuse.
    pub(in crate::ir_lower) fn try_emit_ir_class_is_instance_prefix(&mut self, id: NodeId, info_ptr: usize) {
        if info_ptr == 0 {
            return;
        }
        // SAFETY: as every other read of an `Op::Call`'s `info_ptr` in this
        // file -- a live `JitInvokeInfo` owned by `ir_call_infos`.
        let info: &JitInvokeInfo = unsafe { &*(info_ptr as *const JitInvokeInfo) };
        if !self.class_is_instance_prefix_admits(info) {
            return;
        }
        if crate::x64::zgc_read_barrier_blocks_inline_fields() || !crate::ir_thin_prefix_enabled() {
            return;
        }
        let table = self.direct_helpers.getclass_mirror_words;
        let graph = self.graph;
        let Some(node) = graph.nodes.get(id as usize) else {
            return;
        };
        // [ctrl, mem, mirror, obj], an int result.
        if node.inputs.len() != 4 || node.ty != IrType::Int {
            return;
        }
        let mirror = node.inputs[2];
        let obj = node.inputs[3];
        if self.carry_names(mirror) || self.carry_names(obj) {
            return;
        }
        let Some(slot) = self.prefix_result_slot(id, node.ty) else {
            return;
        };
        if crate::ir_stage_reporting() || crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] class-is-instance-inline (ir) n{id} pc={:?} table={table:#x}",
                node.bytecode_pc,
            );
        }
        let mut decline: Vec<usize> = Vec::new();
        // RAX = obj; null declines (the call answers `false`, or raises the
        // receiver's NPE).
        self.gp_load_value(RAX, obj);
        self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
        decline.push(self.emit_jcc_rel32(0x84)); // JZ
                                                 // A plain object only: an array header carries its COMPONENT's id.
                                                 // TEST BYTE [RAX+KIND_TAGS], KIND_TAG_BYTE_MASK (F6 /0 ib) ; JNE.
        self.buf.emit(&[
            0xF6,
            0x40,
            cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8, // Cast: x86-64 disp8
            cratonvm_types::KIND_TAG_BYTE_MASK,
        ]);
        decline.push(self.emit_jcc_rel32(0x85)); // JNE
                                                 // MOV EAX, dword [RAX] -- the class id, zero-extended.
        self.buf.emit(&[0x8B, 0x00]);
        // JIT round 20 wave 19, lane zAssign: the subtype arm's display cell
        // (`CRATONVM_JIT_CLASS_IS_INSTANCE_SUBTYPE_INLINE`, which reads the
        // hints `CRATONVM_JIT_CLASS_IS_ASSIGNABLE_INLINE` fills), and the
        // class id kept in RCX across the lookup (which writes RAX only).
        let subtype = if crate::class_is_instance_subtype_inline_enabled()
            && crate::class_is_assignable_inline_enabled()
        {
            self.class_assignable_display_cell()
        } else {
            None
        };
        if subtype.is_some() {
            self.note_rcx_written();
            self.buf.emit(&[0x89, 0xC1]); // MOV ECX, EAX
        }
        // R10 = the table; RAX = obj's class word (an id with no word or an
        // unpublished word declines).
        self.emit_mov_reg_imm64(R10, table as u64); // Cast: the table's address
        let (lookup, edges) = getclass_table_lookup_bytes();
        let at = self.buf.pos();
        self.buf.emit(&lookup);
        decline.extend(edges.into_iter().map(|e| at + e));
        // R10 = the receiver mirror (resident registers are callee-saved, never
        // RAX / R10); CMP RAX, R10 (REX.W+R 39 /r) ; JNE decline.
        self.gp_load_value(R10, mirror);
        self.buf.emit(&[0x4C, 0x39, 0xD0]);
        // JIT round 20 wave 19, lane zAssign: with the subtype arm, a word
        // that is not the receiver goes to that arm instead of the call.
        let other = self.emit_jcc_rel32(0x85); // JNE
        if subtype.is_none() {
            decline.push(other);
        }
        // MOV EAX, 1 -- `true`, zero-extended as the call route's int result.
        let one_at = self.buf.pos();
        self.buf.emit(&[0xB8, 0x01, 0x00, 0x00, 0x00]);
        let store_at = self.buf.pos();
        self.emit_prefix_result_store(id, slot);
        let hit = self.emit_jmp_rel32();
        // JIT round 20 wave 19, lane zAssign
        // (`CRATONVM_JIT_CLASS_IS_INSTANCE_SUBTYPE_INLINE`): RCX = obj's class
        // id `s` (saved before the lookup), whose compiled-`getClass()` word
        // is published -- so `obj.getClass()` reports `s` and the native's
        // display-class disjunct is the hierarchy -- but is not the receiver.
        // The receiver's class from its mirror hint, then `s`'s display
        // ([`class_instance_subtype_answer_bytes`]): a hit is the `1` above,
        // a hierarchy-only miss `0`, every other outcome the call.
        if let Some(cell) = subtype {
            self.patch_rel32_to_here(other);
            self.buf.emit(&[0x4C, 0x89, 0xD2]); // MOV RDX, R10 (the receiver)
            let latch = cratonvm_types::duplicate_class_name_seen_addr();
            let (arm, edges, hits, miss) = class_instance_subtype_answer_bytes(table, cell, latch);
            let at = self.buf.pos();
            self.buf.emit(&arm);
            decline.extend(edges.into_iter().map(|e| at + e));
            for h in hits {
                let field = at + h;
                // Cast: offsets inside one compiled method.
                let rel = one_at as i32 - (field as i32 + 4);
                let _ = self.buf.try_patch_i32(field, rel);
            }
            self.patch_rel32_to_here(at + miss);
            self.buf.emit(&[0x31, 0xC0]); // XOR EAX, EAX -- `false`
            self.emit_jmp_back_to(store_at);
        }
        for p in decline {
            self.patch_rel32_to_here(p);
        }
        self.call_prefix_join = Some(hit);
    }

    /// JIT r21 wave 1 (lane typecheck): the supers-display cell the IR
    /// `aastore` store-check arm reads ([`aastore_display_arm_bytes`]), or
    /// `None` under `CRATONVM_JIT_IR_AASTORE_DISPLAY_ARM` off (the default) or
    /// without displays. `CRATONVM_DBG_JITC` names each site.
    pub(in crate::ir_lower) fn aastore_display_cell(&self) -> Option<usize> {
        if !ir_aastore_display_arm_enabled() {
            return None;
        }
        let cell = self.class_assignable_display_cell();
        if crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] ir aastore display arm: {}",
                if cell.is_some() { "emitted" } else { "DECLINED (no display cell)" }
            );
        }
        cell
    }

    /// JIT round 20 wave 19, lane zAssign: the supers-display cell the
    /// `Class.isAssignableFrom` / `Class.isInstance` subtype arms read
    /// ([`class_assignable_core`]), or `None` when this VM keeps no displays
    /// (`CRATONVM_JIT_VH_CONST_INLINE` / `CRATONVM_JIT_VH_CONST_SUBTYPE` off)
    /// or has not published the cell.
    pub(in crate::ir_lower) fn class_assignable_display_cell(&self) -> Option<usize> {
        typecheck_display_cell(&self.direct_helpers)
    }

    /// JIT round 20 wave 19, lane zAssign: may the `Op::Call` described by
    /// `info` take [`Self::try_emit_ir_class_is_assignable_prefix`]? The
    /// compile-time half of its screens, shared with
    /// [`Self::call_site_may_take_inline_prefix`]. `Class` is final: a virtual
    /// (0) or devirtualised special (1) site, the receiver at input 2.
    pub(in crate::ir_lower) fn class_is_assignable_prefix_admits(&self, info: &JitInvokeInfo) -> bool {
        matches!(info.invoke_kind, 0 | 1)
            && info.class_name == "java/lang/Class"
            && info.method_name == "isAssignableFrom"
            && info.descriptor == "(Ljava/lang/Class;)Z"
            && self.direct_helpers.getclass_mirror_words != 0
            && crate::getclass_inline_enabled()
            && crate::class_is_assignable_inline_enabled()
    }

    /// `Class.isAssignableFrom(Class)` in the optimizing tier with no call
    /// (JIT round 20 wave 19, lane zAssign; page
    /// `jr20-class-is-assignable-from-takes-the-class-manager-lock-per-call-20261013.md`,
    /// `R20PNatAssignable` 93-215 ns against HotSpot's 2-16, the leaf-native
    /// call itself): both mirrors' class ids from the compiled-`getClass()`
    /// table's mirror-hint band, each proven by its class word
    /// ([`class_mirror_hint_resolve`]), then the source's supers display
    /// ([`class_assignable_core`]) -- HotSpot's inline `Klass` subtype check.
    /// [`class_assignable_answer_bytes`] states why a hit is the native's
    /// `true` and a screened miss its `false`; every other shape (a null
    /// argument: the NPE; an array or primitive mirror, which never gets a
    /// hint; an unfilled hint; an interface or lambda-proxy source; a
    /// verdict-0 source; a synthetic target with
    /// `CRATONVM_JIT_TYPECHECK_DISPLAY_MISS_ANY_TARGET=0`; a raised duplicate-name latch)
    /// is the unchanged call. The VM's native fills the hints
    /// (`NativeClassAccess::note_class_mirror_id`), so the first calls of a
    /// pair take the call.
    ///
    /// The `Op::Call` prefix contract: no call, no GC point; scratch RAX, RCX,
    /// RDX, R10, R11 (as the constant-`VarHandle` prefix); the answer goes home
    /// (and to the register) and jumps to [`Self::call_prefix_join`]. Declines
    /// at compile time under `CRATONVM_JIT_CLASS_IS_ASSIGNABLE_INLINE=0`,
    /// without a table or display cell, under an armed ZGC read barrier, and
    /// for the carry / dropped-home shapes the other prefixes refuse.
    pub(in crate::ir_lower) fn try_emit_ir_class_is_assignable_prefix(&mut self, id: NodeId, info_ptr: usize) {
        if info_ptr == 0 {
            return;
        }
        // SAFETY: as every other read of an `Op::Call`'s `info_ptr` in this
        // file -- a live `JitInvokeInfo` owned by `ir_call_infos`.
        let info: &JitInvokeInfo = unsafe { &*(info_ptr as *const JitInvokeInfo) };
        if !self.class_is_assignable_prefix_admits(info) {
            return;
        }
        if crate::x64::zgc_read_barrier_blocks_inline_fields() || !crate::ir_thin_prefix_enabled() {
            return;
        }
        let Some(cell) = self.class_assignable_display_cell() else {
            return;
        };
        let table = self.direct_helpers.getclass_mirror_words;
        let graph = self.graph;
        let Some(node) = graph.nodes.get(id as usize) else {
            return;
        };
        // [ctrl, mem, target mirror (the receiver), source mirror], an int.
        if node.inputs.len() != 4 || node.ty != IrType::Int {
            return;
        }
        let target = node.inputs[2];
        let source = node.inputs[3];
        if self.carry_names(target) || self.carry_names(source) {
            return;
        }
        let Some(slot) = self.prefix_result_slot(id, node.ty) else {
            return;
        };
        if crate::ir_stage_reporting() || crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] class-is-assignable-inline (ir) n{id} pc={:?} table={table:#x} \
                 cell={cell:#x}",
                node.bytecode_pc,
            );
        }
        let latch = cratonvm_types::duplicate_class_name_seen_addr();
        let (bytes, edges, hits, miss) = class_assignable_answer_bytes(table, cell, latch);
        self.note_rcx_written();
        self.gp_load_value(RAX, target);
        self.gp_load_value(RDX, source);
        let at = self.buf.pos();
        self.buf.emit(&bytes);
        let decline: Vec<usize> = edges.into_iter().map(|e| at + e).collect();
        // miss: XOR EAX, EAX ; JMP store.
        self.patch_rel32_to_here(at + miss);
        self.buf.emit(&[0x31, 0xC0]);
        let to_store = self.emit_jmp_rel32();
        // hit: MOV EAX, 1.
        for h in hits {
            self.patch_rel32_to_here(at + h);
        }
        self.buf.emit(&[0xB8, 0x01, 0x00, 0x00, 0x00]);
        self.patch_rel32_to_here(to_store);
        self.emit_prefix_result_store(id, slot);
        let join = self.emit_jmp_rel32();
        for p in decline {
            self.patch_rel32_to_here(p);
        }
        self.call_prefix_join = Some(join);
    }

    /// nio20 wave 5, lane L2: the compile-time half of
    /// [`Self::try_emit_ir_identity_hash_prefix`]'s screens, shared with
    /// [`Self::call_site_may_take_inline_prefix`]. `Some(true)` for
    /// `System.identityHashCode`, `Some(false)` for a `Class.hashCode()` site
    /// (see [`ir_identity_hash_site`]).
    pub(in crate::ir_lower) fn identity_hash_prefix_admits(&self, info: &JitInvokeInfo) -> Option<bool> {
        if !ir_identity_hash_inline_enabled() {
            return None;
        }
        ir_identity_hash_site(
            info.class_name,
            info.method_name,
            info.descriptor,
            info.invoke_kind,
        )
    }

    /// Emit [`identity_hash_read_bytes`] with RAX = a non-null object: on the
    /// fall-through RAX is the installed identity hash; every decline edge is
    /// pushed onto `decline`. Writes RCX and RDX.
    pub(in crate::ir_lower) fn emit_ir_identity_hash_read(&mut self, decline: &mut Vec<usize>) {
        self.note_rcx_written();
        let (bytes, edges) = identity_hash_read_bytes();
        let at = self.buf.pos();
        self.buf.emit(&bytes);
        decline.extend(edges.into_iter().map(|e| at + e));
    }

    /// The identity hash in the optimizing tier (nio20 wave 5, lane L2; page
    /// `r20-identity-hash-is-a-native-call-in-compiled-code-FIXED-20261012.md`):
    /// `System.identityHashCode(o)` and `Class.hashCode()` answered from the
    /// object's header when a hash is already installed there, as a guarded
    /// PREFIX of the call's unchanged lowering. HotSpot reads the mark word the
    /// same way (its `hashCode` intrinsic, with the slow path for an unhashed or
    /// locked object).
    ///
    /// Same contract with the `Op::Call` arm as
    /// [`Self::try_emit_ir_getclass_prefix`]: scratch RAX, RCX and RDX only; no
    /// call, no GC point and nothing that traps on the hit path; the hit edge
    /// writes the result home (and register) and jumps to
    /// [`Self::call_prefix_join`]. Every decline lands on the call, which runs
    /// the registered native exactly as before (`System.identityHashCode`'s
    /// `Bridge`, or `Object.hashCode`'s), so the prefix only ever answers what
    /// that native would have read back from the same header:
    ///
    /// * a null argument / receiver -- the call answers `0` / raises the NPE;
    /// * a `Class.hashCode()` receiver whose class id is not exactly
    ///   `java/lang/Class` (cannot happen for a verified `Class`-typed site
    ///   unless the id is unknown; `Class` is final and does not override
    ///   `hashCode`, so the exact id is the whole override test);
    /// * no hash installed yet (the call mints and installs it), a compact
    ///   instance that is thin-locked or inflated (its hash, if any, is in the
    ///   monitor), a forwarded header.
    ///
    /// The values are GC-stable (every mover copies the header verbatim) and an
    /// installed hash is never rewritten, so one racy 32-bit header load is a
    /// consistent snapshot. Nothing is baked but the decode constants and, for
    /// `Class.hashCode`, `Class`'s id.
    ///
    /// Declines at compile time under `CRATONVM_JIT_IR_IDENTITY_HASH_INLINE=0`,
    /// under an armed ZGC read barrier (as the getClass prefix), without the
    /// `Class` id for a `Class.hashCode` site, and for the carry / dropped-home
    /// shapes the other prefixes refuse.
    pub(in crate::ir_lower) fn try_emit_ir_identity_hash_prefix(&mut self, id: NodeId, info_ptr: usize) {
        if info_ptr == 0 {
            return;
        }
        // SAFETY: as every other read of an `Op::Call`'s `info_ptr` in this
        // file -- a live `JitInvokeInfo` owned by `ir_call_infos`.
        let info: &JitInvokeInfo = unsafe { &*(info_ptr as *const JitInvokeInfo) };
        let Some(any_object) = self.identity_hash_prefix_admits(info) else {
            return;
        };
        if crate::x64::zgc_read_barrier_blocks_inline_fields() {
            return;
        }
        let class_guard = if any_object {
            None
        } else {
            match ir_class_class_id(&self.direct_helpers) {
                Some(cid) => Some(cid),
                None => return,
            }
        };
        let graph = self.graph;
        let Some(node) = graph.nodes.get(id as usize) else {
            return;
        };
        // [ctrl, mem, object], an int result.
        if node.inputs.len() != 3 || node.ty != IrType::Int {
            return;
        }
        let obj = node.inputs[2];
        if self.carry_names(obj) {
            return;
        }
        let Some(slot) = self.prefix_result_slot(id, node.ty) else {
            return;
        };
        if crate::ir_stage_reporting() || crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] identity-hash-inline (ir) n{id} pc={:?} {}.{}{} class_guard={class_guard:?}",
                node.bytecode_pc, info.class_name, info.method_name, info.descriptor,
            );
        }
        let mut decline: Vec<usize> = Vec::new();
        // RAX = the object; null for System.identityHashCode answers 0 directly.
        self.gp_load_value(RAX, obj);
        self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
        let null_hit = if any_object {
            Some(self.emit_jcc_rel32(0x84))
        } else {
            decline.push(self.emit_jcc_rel32(0x84)); // JZ
            None
        };
        if let Some(cid) = class_guard {
            // A plain object of exactly `java/lang/Class`:
            // TEST BYTE [RAX+KIND_TAGS], KIND_TAG_BYTE_MASK ; JNE decline ;
            // CMP DWORD [RAX+0], Class ; JNE decline.
            self.buf.emit(&[
                0xF6,
                0x40,
                cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8, // Cast: x86-64 disp8
                cratonvm_types::KIND_TAG_BYTE_MASK,
            ]);
            decline.push(self.emit_jcc_rel32(0x85));
            self.buf.emit(&[0x81, 0x78, 0x00]);
            self.buf.emit(&cid.to_le_bytes());
            decline.push(self.emit_jcc_rel32(0x85));
        }
        self.emit_ir_identity_hash_read(&mut decline);
        if let Some(jz) = null_hit {
            self.patch_rel32_to_here(jz);
        }
        self.emit_prefix_result_store(id, slot);
        let hit = self.emit_jmp_rel32();
        for p in decline {
            self.patch_rel32_to_here(p);
        }
        self.call_prefix_join = Some(hit);
    }

    /// nio21 wave 1, lane L5: the compile-time half of
    /// [`Self::try_emit_ir_current_thread_prefix`]'s screens, shared with
    /// [`Self::call_site_may_take_inline_prefix`]: `Thread.currentThread()`
    /// (static), with the VM's published thread-word offset
    /// (`DirectHelperTable::thread_java_obj_offset`, non-zero only when the
    /// policy gate the thin helper asks can never refuse for this VM's life,
    /// i.e. not `--real-jdk`; see `vm/src/jit/helpers.rs`
    /// `thread_java_obj_offset_for`) and the `JIT_THREAD` TLS mirror.
    ///
    /// nio21 wave 3, lane L4: `Thread.currentCarrierThread()` too, whose
    /// registered native is `currentThread`'s (`native_thread_current_thread`)
    /// and whose thin helper's fast arm reads the same word; its own bind is
    /// behind `CRATONVM_JIT_CARRIER_THREAD_DIRECT`, asked here as well.
    pub(in crate::ir_lower) fn current_thread_prefix_admits(&self, info: &JitInvokeInfo) -> bool {
        info.invoke_kind == 3
            && info.class_name == "java/lang/Thread"
            && (info.method_name == "currentThread"
                || (info.method_name == "currentCarrierThread"
                    && crate::direct_helpers::carrier_thread_direct_enabled()
                    // An unwired cell: this VM's carrier is not the thread
                    // word (JDK-driven virtual threads, `vm::jdk_continuation`).
                    && self.direct_helpers.thread_current_carrier_thread != 0))
            && info.descriptor == "()Ljava/lang/Thread;"
            && self.direct_helpers.thread_java_obj_offset != 0
            && crate::x64::jit_thread_tls_disp() != 0
            && crate::ir_thin_prefix_enabled()
            && ir_current_thread_prefix_enabled()
    }

    /// `Thread.currentThread()` in the optimizing tier (nio21 wave 1, lane L5;
    /// the twin of the single-pass `Compiler::try_emit_sp_current_thread_prefix`,
    /// page `r20-sp-thread-current-thread-prefix-needs-a-per-vm-thread-word-FIXED-20261012.md`):
    /// the `JvmThread*` from the `JIT_THREAD` TLS mirror, then its
    /// `java_thread_obj` word at the per-VM offset the VM published -- exactly
    /// what the thin helper `jit_thread_current_thread_direct` returns on its
    /// fast arm. A guarded PREFIX of the call's unchanged lowering (the
    /// getClass prefix's contract: scratch RAX only, the hit edge writes the
    /// result home and register and jumps to [`Self::call_prefix_join`]); no
    /// JIT thread context or no mirror yet (`None` = 0) declines to the call.
    ///
    /// The word is per VM (`DirectHelperTable`), never a process global, and
    /// is `0` under `--real-jdk`, so the strict mode keeps every call. Declines
    /// at compile time under `CRATONVM_JIT_IR_CURRENT_THREAD_PREFIX=0`,
    /// `CRATONVM_JIT_IR_THIN_PREFIX=0`, an armed ZGC read barrier (the gate
    /// every inline reference load shares), and the carry / dropped-home
    /// shapes the other prefixes refuse.
    pub(in crate::ir_lower) fn try_emit_ir_current_thread_prefix(&mut self, id: NodeId, info_ptr: usize) {
        if info_ptr == 0 {
            return;
        }
        // SAFETY: as every other read of an `Op::Call`'s `info_ptr` in this
        // file -- a live `JitInvokeInfo` owned by `ir_call_infos`.
        let info: &JitInvokeInfo = unsafe { &*(info_ptr as *const JitInvokeInfo) };
        if !self.current_thread_prefix_admits(info)
            || crate::x64::zgc_read_barrier_blocks_inline_fields()
        {
            return;
        }
        let Ok(off) = i32::try_from(self.direct_helpers.thread_java_obj_offset) else {
            return;
        };
        let Ok(tls) = u32::try_from(crate::x64::jit_thread_tls_disp()) else {
            return;
        };
        let graph = self.graph;
        let Some(node) = graph.nodes.get(id as usize) else {
            return;
        };
        // [ctrl, mem], a reference result.
        if node.inputs.len() != 2 || node.ty != IrType::Ref {
            return;
        }
        let Some(slot) = self.prefix_result_slot(id, node.ty) else {
            return;
        };
        if crate::ir_stage_reporting() || crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] current-thread-inline (ir) n{id} pc={:?} off={off}",
                node.bytecode_pc,
            );
        }
        let mut decline: Vec<usize> = Vec::with_capacity(3);
        // RAX = the `JvmThread*` (0 outside a JIT thread context).
        self.emit_mov_rax_tls_disp32(tls);
        self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
        decline.push(self.emit_jcc_rel32(0x84)); // JZ
        if info.method_name == "currentCarrierThread" && self.direct_helpers.vthread_state_offset >= 4 {
            let carrier_off = (self.direct_helpers.vthread_state_offset as i32) - 4;
            // CMP DWORD PTR [RAX + disp32], 0 (`jdk_continuation.carrier() == 0`)
            self.buf.emit(&[0x83, 0xB8]);
            self.buf.emit(&carrier_off.to_le_bytes());
            self.buf.emit(&[0x00]);
            decline.push(self.emit_jcc_rel32(0x85)); // JNE decline
        }
                                                 // MOV RAX, [RAX + disp32] -- `java_thread_obj` (`None` = 0).
        self.buf.emit(&[0x48, 0x8B, 0x80]);
        self.buf.emit(&off.to_le_bytes());
        self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
        decline.push(self.emit_jcc_rel32(0x84)); // JZ
        self.emit_prefix_result_store(id, slot);
        let hit = self.emit_jmp_rel32();
        for p in decline {
            self.patch_rel32_to_here(p);
        }
        self.call_prefix_join = Some(hit);
    }

}
