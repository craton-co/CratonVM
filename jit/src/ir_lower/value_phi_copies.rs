#[allow(unused_imports)]
use super::*;

impl<'a> Lowerer<'a> {
    /// The value phis' half of [`Self::emit_phi_copies`].
    pub(in crate::ir_lower) fn emit_value_phi_copies(&mut self, pred_block: usize, succ_block: usize) {
        let merge_ctrl = self.schedule.blocks[succ_block].ctrl;
        // Only Merge/Region blocks carry phis tied to incoming edges.
        if !matches!(
            self.graph.nodes[merge_ctrl as usize].op,
            Op::Merge | Op::Region
        ) {
            return;
        }

        // Gather (phi_slot, value_id) pairs first to avoid borrowing `self`
        // immutably while emitting (which borrows `self` mutably).
        let gathered = self.gather_phi_copies(pred_block, succ_block);
        let copies: Vec<(i32, i32)> = gathered.iter().map(|c| (c.dst, c.src_slot)).collect();

        // Phi copies are a PARALLEL assignment, not a sequence.
        //
        // `prealloc_phi_slots` gives every phi its own frame word, so a loop
        // that swaps two locals makes each phi the other's back-edge value and
        // this list becomes a cycle (`a <- b, b <- a`). Emitted in gather order
        // through RAX, the second copy reads the word the first just overwrote
        // and both locals end up holding `b` — a wrong-code bug, not a missed
        // optimisation.
        //
        // `resolve_parallel_copy` orders the copies so that every read still
        // sees the pre-copy value: destinations nothing else reads go first,
        // and when only cycles remain one element is parked in the scratch,
        // which frees its predecessor and unwinds the rest of the cycle. The
        // acyclic majority — every merge that is not a permutation — pays
        // exactly the same one load + one store per copy it always did, and
        // touches the scratch word not at all.
        //
        // An intermediate state of this fix REFUSED a residual cycle
        // (`UnsupportedShape("cyclic phi parallel copy")`) because no scratch
        // location was reserved. That was correct but dropped every swap-shaped
        // loop to a lower tier; `phi_copy_scratch_slot_off` is the reserved word
        // that makes the refusal unnecessary.
        let ops = match phi_copy_sequence(&copies) {
            Ok(ops) => ops,
            Err(bailout) => {
                let Bailout { reason, context } = bailout;
                let edge = format!("phi parallel copy on block {pred_block} -> {succ_block}");
                self.latch_bailout(Bailout::with_context(
                    reason,
                    match context {
                        Some(c) => format!("{edge}: {c}"),
                        None => edge,
                    },
                ));
                return;
            }
        };
        // Which node does each frame word in this edge belong to? Two maps,
        // because one word can be a source here and a destination there, and
        // the reserved scratch is in neither — a `Save`'s destination and a
        // `Restore`'s source therefore find nothing and stay memory, which is
        // what they must be.
        let (src_node_of, phi_of_dst) = if self.phi_copy_regs_on {
            let mut s: HashMap<i32, NodeId> = HashMap::new();
            let mut d: HashMap<i32, NodeId> = HashMap::new();
            for c in &gathered {
                s.insert(c.src_slot, c.src);
                d.insert(c.dst, c.phi);
            }
            (s, d)
        } else {
            (HashMap::new(), HashMap::new())
        };
        // ── The register layer's OWN interference, which the resolver cannot see
        //
        // `resolve_parallel_copy` sequentialises over FRAME WORDS. The register
        // fast path below then reads a source from its assigned GPR and
        // publishes a destination into the phi's assigned GPR — and two
        // DISTINCT frame words can be homed in the SAME register, because the
        // allocator sees a phi's live range as beginning after the merge and a
        // dying source's as ending at it, so they do not interfere by its
        // reckoning. They do interfere across the copy sequence: an earlier
        // copy's publish then overwrites a register a later copy still has to
        // read, and the resolver's word-level order says nothing about it.
        //
        // Measured on
        // `vm/tests/jit_ir_phi_copy_register_alias_fixtures/PhiCopyRegisterAliasProbe.java`
        // (and, in the wild, `java.time.Duration.toNanos()` — 198,356 wrong
        // answers in 200,000 calls): the else-arm of `if (seconds < 0)` emitted
        //   `mov rbx,rax`   ; publish phi_seconds, whose GPR is RBX
        //   `mov rax,rbx`   ; read phi_nanos's SOURCE, whose GPR is also RBX
        // so `nanos` came back as `seconds` and `toNanos()` returned
        // `seconds * 1e9 + seconds`. `CRATONVM_JIT_IR_PHI_COPY_REGS=0` and
        // `CRATONVM_JIT_IR_PHI_RESIDENCY=0` each made it disappear, which is
        // what named this layer as the owner.
        //
        // The fix is per-edge and conservative: a phi whose publish register is
        // ALSO some source's register on this edge does not publish inline.
        // Its home store is kept (so there is a word to read) and the trailing
        // residency loop below publishes it from that word — after every source
        // on the edge has been read. Everything else keeps the reg-to-reg
        // publish it had. The cost is one load per deferred phi per edge; the
        // alternative, ordering the two layers together, would have to model a
        // copy that writes two locations at once.
        let mut defer_publish: Vec<NodeId> = Vec::new();
        if self.phi_copy_regs_on {
            let src_regs: Vec<(NodeId, u8)> = gathered
                .iter()
                .filter_map(|c| self.assigned_gpr(c.src).map(|r| (c.src, r)))
                .collect();
            // Round 16 wave 3 (lane codegen, CG16-6): a phi's OWN source in
            // the phi's register (a coalesced back-edge source) is read by its
            // own copy before that copy writes the register, so it is no
            // reason to defer, unless another copy on this edge reads it too.
            let own_ok = ir_phi_coalesce_back_edge_enabled(self.graph);
            for c in &gathered {
                if let Some(dst_reg) = self.assigned_gpr(c.phi) {
                    let own_only = |src: NodeId| {
                        own_ok
                            && src == c.src
                            && gathered.iter().filter(|o| o.src == src).count() == 1
                    };
                    if src_regs
                        .iter()
                        .any(|(src, r)| *r == dst_reg && *src != c.phi && !own_only(*src))
                    {
                        defer_publish.push(c.phi);
                        self.phi_copy_publish_deferred += 1;
                    }
                }
            }
        }
        let mut published: Vec<NodeId> = Vec::new();
        for op in ops {
            if let Err(bailout) = self.emit_copy_op(
                op,
                &src_node_of,
                &phi_of_dst,
                &defer_publish,
                &mut published,
            ) {
                self.latch_bailout(bailout);
                return;
            }
        }
        // 2026-09-02: a register-resident phi is PUBLISHED here, at every
        // incoming edge, from the home word the copies just wrote. This is the
        // definition site a phi never had, and it is what lets a loop counter
        // or accumulator -- a phi at every loop header -- be read from a
        // register inside the loop (`ir_phi_residency_enabled`). Reads before
        // the first published edge (in emission order) still take the home
        // word; every edge publishes, so the register is valid on entry to the
        // block whichever predecessor ran.
        //
        // 2026-09-04: a GP phi whose copy went through RAX is published *there*,
        // from RAX, and appears in `published`. The reload below is what is
        // left for everything else — the FP half, and any phi whose copy the
        // resolver dropped as a self-copy (nothing was emitted, so nothing
        // could publish, and on the first edge in emission order the register
        // may not be live yet).
        if ir_phi_residency_enabled() {
            // The XMM registers this loop's FP publishes have written so far.
            let mut fp_written: Vec<u8> = Vec::new();
            let fp_from_reg = ir_loop_carried_fp_regs_enabled();
            for c in &gathered {
                if published.contains(&c.phi) {
                    continue;
                }
                // A PHI WHOSE HOME WAS DROPPED HAS NO WORD TO PUBLISH FROM.
                //
                // `emit_copy_op` drops the home store for a non-deferred phi
                // that `phi_home_droppable` cleared, on the grounds that its
                // register is the only location anyone reads. This loop is the
                // one reader that did not get that memo: it reaches a phi that
                // `published` does not contain -- which includes every phi
                // whose copy `resolve_parallel_copy` dropped as a SELF-COPY,
                // where `emit_copy_op` never ran at all and so neither stored
                // the word nor published the register -- and loads the home
                // word regardless.
                //
                // That is a read of uninitialised stack, and it is not
                // theoretical: `org.h2.command.query.Select.processGroupResult`
                // reloaded its `long offset` phi from `[rbp-0B0h]`, a slot the
                // 11,591-byte body reads three times and writes zero times, on
                // the loop back edge. The garbage came back large and positive
                // and the loop's `quickOffset && offset > 0` arm dropped result
                // rows as if the query had an OFFSET clause -- H2 window and
                // GROUP BY queries returning 3 rows of 5. See
                // `internal/fixed-bugs/jit-warm-groupdata-window-row-collapse-20260906-FIXED.md`.
                //
                // A DEFERRED phi is exempt and must stay exempt: `emit_copy_op`
                // keeps its home store precisely so this loop can read it.
                if !defer_publish.contains(&c.phi)
                    && self
                        .home_dropped
                        .get(c.phi as usize)
                        .copied()
                        .unwrap_or(false)
                {
                    if self.resident_gpr(c.phi).is_some() {
                        // The register already holds it -- which is the whole
                        // premise of dropping the home. Nothing to publish.
                        self.phi_home_publish_skipped += 1;
                    } else if let Some(dst) = self.assigned_gpr(c.phi) {
                        // A copy the resolver dropped as a SELF-COPY (the phi's
                        // home slot IS the source's): nothing was emitted on
                        // this edge, and the phi's home is never written, so
                        // publish the phi's register from wherever the SOURCE's
                        // value is. `gp_load_value` is the one reader that knows
                        // every place (its register, a carry -- which only this
                        // read takes out of its slot -- a constant, or a
                        // written home word) and refuses the compile itself
                        // when it is none of them. Refusing here instead threw
                        // away every compile whose slot colouring shared a
                        // phi's home with one of its sources -- bounded
                        // self-recursive inlining's `itemCheck` (a spliced
                        // copy's result phi, fed by a carried `Add`) among them.
                        self.gp_load_value(dst, c.src);
                        self.mark_gp_reg_live(c.phi);
                        self.phi_home_publish_skipped += 1;
                    } else {
                        // Neither location is readable. REFUSE, exactly as
                        // `emit_copy_op` refuses the mirror case, rather than
                        // emit a read of a word nothing wrote.
                        self.phi_home_publish_refused += 1;
                        self.latch_bailout(Bailout::with_context(
                            BailoutReason::UnallocatedValue { node: c.phi },
                            format!(
                                "n{}'s home was dropped and its register is not live at a publish edge",
                                c.phi
                            ),
                        ));
                        return;
                    }
                    continue;
                }
                if self.assigned_gpr(c.phi).is_some() {
                    self.publish_gp_from_slot(c.phi, c.dst);
                } else if let Some(dst_xmm) = self.assigned_xmm(c.phi) {
                    let is_double = self.graph.nodes[c.phi as usize].ty == IrType::Double;
                    // `CRATONVM_JIT_IR_LOOP_CARRIED_FP_REGS`: from the source's
                    // register, when it holds the source and no publish of THIS
                    // loop has written that register yet (the copies above go
                    // through RAX and the frame, so only these publishes write
                    // the XMM file on this edge). The home word the copy wrote
                    // stays the frame's image either way.
                    let src_xmm = fp_from_reg
                        .then(|| self.resident_xmm(c.src))
                        .flatten()
                        .filter(|r| !fp_written.contains(r));
                    match src_xmm {
                        Some(r) => {
                            self.fp_reg_move(dst_xmm, r);
                            self.mark_reg_live(c.phi);
                            self.fp_phi_reg_publishes += 1;
                        }
                        None => {
                            self.publish_fp_from_slot(c.phi, c.dst, is_double);
                        }
                    }
                    fp_written.push(dst_xmm);
                }
            }
        } else {
            // The publish `emit_copy_op` skipped for an aliasing phi still has
            // to happen, whatever the residency switch says: it was skipped
            // because doing it inline would have been WRONG, not because it was
            // unwanted. The loop above covers this when residency is on; this
            // arm is the same duty when it is off (the inline publish is gated
            // on `ir_phi_copy_regs_enabled` alone, so it runs in that
            // configuration too, and so must its deferral).
            for c in &gathered {
                if defer_publish.contains(&c.phi) {
                    self.publish_gp_from_slot(c.phi, c.dst);
                }
            }
        }
    }

    /// Emit one sequentialised [`CopyOp`], routed through RAX (the move
    /// temporary) and, for `Save`/`Restore`, the reserved scratch frame word.
    ///
    /// `Save` and `Restore` are *not* a push/pop pair: `resolve_parallel_copy`
    /// emits at most one live save at a time and always consumes it before
    /// starting another cycle, so a single word suffices no matter how many
    /// disjoint cycles the edge contains.
    pub(in crate::ir_lower) fn emit_copy_op(
        &mut self,
        op: CopyOp,
        src_node_of: &HashMap<i32, NodeId>,
        phi_of_dst: &HashMap<i32, NodeId>,
        defer_publish: &[NodeId],
        published: &mut Vec<NodeId>,
    ) -> CompileResult<()> {
        let scratch = self.phi_copy_scratch_slot_off;
        let (src, dst) = match op {
            CopyOp::Move { from, to } => (frame_word_off(from)?, frame_word_off(to)?),
            CopyOp::Save { from } => (frame_word_off(from)?, scratch),
            CopyOp::Restore { to } => (scratch, frame_word_off(to)?),
        };
        // The scratch is a reserved bookkeeping word placed by `Lowerer::new`;
        // a zero here would encode `[rbp - 0]`, the saved caller RBP.
        if src <= 0 || dst <= 0 {
            return Err(Bailout::with_context(
                BailoutReason::Internal("ir_lower: a phi copy names frame offset 0"),
                format!("[rbp - {dst}] <- [rbp - {src}]"),
            ));
        }
        // `CRATONVM_JIT_IR_LOOP_CARRIED_FP_REGS`: an FP phi's home written
        // straight from its source's register. Through RAX the copy was a
        // 64-bit load of a word the source's definition had just written with
        // a 32-bit `MOVSS`, which store forwarding cannot serve: the load
        // waited for the store to commit, once per back edge (the hot sample
        // of `float-sum-1024` on the Linux host). The register holds the
        // word's pre-copy value (write-through, and nothing on this edge
        // writes the XMM file before the trailing publishes), so reading it
        // is the same read the resolver's order protects. A `Move` only; the
        // phi's own publish stays in `emit_value_phi_copies`.
        if let CopyOp::Move { .. } = op {
            let fp = phi_of_dst
                .get(&dst)
                .copied()
                .filter(|_| self.phi_copy_regs_on && ir_loop_carried_fp_regs_enabled())
                .filter(|&phi| {
                    !self.home_dropped.get(phi as usize).copied().unwrap_or(false)
                        && matches!(
                            self.graph.nodes.get(phi as usize).map(|n| n.ty),
                            Some(IrType::Float | IrType::Double)
                        )
                })
                .and_then(|phi| {
                    let sid = src_node_of.get(&src).copied()?;
                    let r = self.resident_xmm(sid)?;
                    Some((phi, r))
                });
            if let Some((phi, r)) = fp {
                let is_double = self.graph.nodes[phi as usize].ty == IrType::Double;
                self.fp_store(dst, r, is_double);
                self.fp_phi_reg_stores += 1;
                return Ok(());
            }
        }
        // ── Read: from the source's register when it has one ──────────
        //
        // Safe for the same reason the memory schedule is safe, and by the same
        // argument. `resolve_parallel_copy` orders the ops so that every source
        // is READ before anything writes it; a register is updated at exactly
        // the instruction that writes the word (below), so a register and its
        // home word go stale at the same point, and an order that protects one
        // protects the other. A cycle's `Save` copies the pre-value out at the
        // same instant either way.
        // ── Which register STAGES the value ──────────────────────────
        //
        // RAX was the staging register for every phi copy, and the publish
        // below then copied RAX into the phi's own register. When the phi has
        // one, that register is a strictly better temporary: the read lands
        // there directly and the publish disappears, because the value is
        // already where the publish was going to put it.
        //
        // Worth one instruction per phi per edge, i.e. per loop-carried value
        // per iteration. On `probes/FieldLoop.java` `sum` the back edge was
        //
        //     mov rax,r15 / mov r12,rax          ; the `sum` phi
        //     mov rax,[rbp-98h] / mov rbx,rax    ; the `i` phi
        //
        // which is four instructions in a 26-instruction loop body to move two
        // values that are already in a register or a word.
        //
        // # Why it is the same program
        //
        // The write to the phi's register moves EARLIER inside this one
        // `CopyOp` — from after the home store to before it — and nothing in
        // between reads anything: the only instruction it crosses is this
        // copy's own store, whose source it now is. Relative to every OTHER
        // copy on this edge the ordering is unchanged, because the old publish
        // was already inside this op and therefore already ahead of the next
        // op's read. `resolve_parallel_copy`'s invariant (every source is read
        // before anything writes it) is a statement about that cross-op order
        // and is untouched.
        //
        // # What still goes through RAX, and why
        //
        // * a phi with no GP register — an FP phi publishes from its home word
        //   in `emit_phi_copies`'s later loop, and a non-resident phi has
        //   nowhere else to stage;
        // * a DEFERRED publish — the home store is the only thing carrying the
        //   value to the end of the edge, so writing the register here would
        //   publish it at the wrong point;
        // * `CopyOp::Save`, whose destination is the scratch word and has no
        //   phi at all;
        // * **a phi whose home store SURVIVES.** That one is a deliberate
        //   restriction rather than an obstacle, and the reason is in
        //   `a_phi_copy_that_keeps_its_home_is_byte_identical`: staging there
        //   would also have to write the home from the staged register, and no
        //   test in this crate can separate a right store from a wrong one on
        //   that path — nothing reads a resident phi's home word back, so
        //   `store_abi_reg(RAX, dst)` in place of the value produces identical
        //   answers. Requiring the drop keeps every remaining path one this
        //   suite can fail. It costs the home-keeping case one instruction, on
        //   a path a `panic!` proved the whole crate never reaches.
        //
        // JIT r25 W7 increment 6 (`CRATONVM_JIT_IR_REG_ALLOC`): under the
        // allocator an `int` / `long` phi whose home SURVIVES is staged in its
        // register too, and its home is then written from that register
        // ([`r25_w7a_reg_alloc::stages_kept_phi`]): the RAX round trip goes,
        // and a coalesced self-copy is the home store alone.
        let stage = phi_of_dst
            .get(&dst)
            .copied()
            .filter(|_| self.phi_copy_regs_on && ir_phi_copy_direct_enabled())
            .filter(|phi| !defer_publish.contains(phi))
            .filter(|phi| {
                self.home_dropped
                    .get(*phi as usize)
                    .copied()
                    .unwrap_or(false)
                    || self.reg_alloc_stages_kept_phi(*phi)
            })
            .and_then(|phi| self.assigned_gpr(phi).map(|reg| (phi, reg)));
        let stage_reg = stage.map_or(RAX, |(_, reg)| reg);
        let mut from_reg = false;
        if self.phi_copy_regs_on {
            if let Some(&sid) = src_node_of.get(&src) {
                // A source whose home is never written has ONE readable
                // location, and `resident_gpr`'s "has it been published yet"
                // gate is the wrong question for it: there is nothing to fall
                // back to. `assigned_gpr` is the right one, and it is safe for
                // the same reason the publish is — the value is defined before
                // any edge that reads it, and the register is exclusively its
                // own (that is a clause of `phi_home_droppable`).
                let homeless = self
                    .home_dropped
                    .get(sid as usize)
                    .copied()
                    .unwrap_or(false);
                let src_reg = if homeless {
                    self.assigned_gpr(sid)
                } else {
                    self.resident_gpr(sid)
                };
                match src_reg {
                    Some(r) => {
                        // A no-op when the source already sits in the phi's
                        // register, which `emit_mov_reg_reg64` drops rather
                        // than encoding `mov r,r`.
                        self.emit_mov_reg_reg64(stage_reg, r);
                        from_reg = true;
                        self.phi_copy_reg_reads += 1;
                    }
                    // Unreachable by construction: a home is dropped only for a
                    // value the residency file gave an exclusive register. If
                    // it ever happens, refuse the compile rather than emit a
                    // read of a word nothing wrote.
                    // CG16P-4: a dropped constant is read as an immediate
                    // below.
                    None if homeless && self.is_dropped_const(sid) => {}
                    None if homeless => {
                        return Err(Bailout::with_context(
                            BailoutReason::UnallocatedValue { node: sid },
                            format!("n{sid}'s home was dropped but it has no register"),
                        ));
                    }
                    None => {}
                }
            }
        }
        // Round 16 wave 3 (lane codegen, CG16-7): a constant source is an
        // immediate, exactly as `gp_load_value` materialises it, not a home
        // word (`Op::Const` writes its home with the same 64-bit value).
        let dropped_const_src = src_node_of
            .get(&src)
            .is_some_and(|&sid| self.is_dropped_const(sid));
        if !from_reg
            && self.const_imm_on
            && (ir_phi_const_source_imm_enabled() || dropped_const_src)
        {
            let imm = src_node_of
                .get(&src)
                .and_then(|&sid| self.graph.nodes.get(sid as usize))
                .and_then(|node| match node.op {
                    Op::Const(val)
                        if matches!(node.ty, IrType::Int | IrType::Long)
                            || (node.ty == IrType::Ref && val == 0) =>
                    {
                        Some(val)
                    }
                    _ => None,
                });
            if let Some(val) = imm {
                self.emit_mov_reg_imm_smart(stage_reg, val);
                from_reg = true;
                self.phi_copy_const_imms += 1;
            }
        }
        if !from_reg && dropped_const_src {
            return Err(Bailout::with_context(
                BailoutReason::Internal(
                    "ir_lower: a dropped constant home is an edge copy's source",
                ),
                format!("[rbp - {src}]"),
            ));
        }
        if !from_reg {
            self.load_reg_from_frame(stage_reg, src);
        }
        // ── The store, when anything could read it ───────────────────
        //
        // This is what the register image was reserved for. A loop-carried
        // value whose register a deopt frame can name, whose every reader takes
        // that register, needs no frame word at all — and this store was the
        // last frame traffic left in the loop after the two reloads went.
        // A deferred publish (see `emit_phi_copies`) reads this word back at the
        // end of the edge, so the store it would otherwise have dropped is the
        // only thing that carries the value — keep it.
        let drop_home = phi_of_dst.get(&dst).is_some_and(|phi| {
            !defer_publish.contains(phi)
                && self
                    .home_dropped
                    .get(*phi as usize)
                    .copied()
                    .unwrap_or(false)
        });
        if drop_home {
            self.home_stores_dropped += 1;
        } else {
            // Reached with `stage_reg == RAX`, except for increment 6's staged
            // kept-home phi (`stage` above): `stage` otherwise requires the
            // phi's home to be dropped and `drop_home` is that same fact. The
            // `debug_assert` is what stops any other widening of `stage` from
            // silently storing the wrong register here.
            let staged_kept = stage.is_some_and(|(phi, _)| self.reg_alloc_stages_kept_phi(phi));
            debug_assert!(
                stage_reg == RAX || staged_kept,
                "a staged phi copy reached the home store; `stage` and \
                 `drop_home` have come apart",
            );
            // JIT round 25 wave 6 (lane W6-B): the `ir-home-why` census of
            // phi edge stores (nothing without `CRATONVM_DBG_JITC`).
            if let Some(&phi) = phi_of_dst.get(&dst) {
                if !self.home_keep_why.is_empty() {
                    self.phi_edge_home_stores += 1;
                    self.phi_edge_home_weighted += self
                        .schedule
                        .freq
                        .freq
                        .get(self.cur_lower_block)
                        .copied()
                        .filter(|w| w.is_finite())
                        .unwrap_or(0.0);
                }
                self.note_home_keep(phi);
            }
            if staged_kept {
                // The register holds the value (staged above); no twin of
                // `store_rax` may outlive this store either.
                self.rcx_twin = None;
                self.store_abi_reg(stage_reg, dst);
                self.reg_alloc_kept_phi_stages += 1;
            } else {
                self.store_rax(dst);
            }
        }
        // ── Publish: from RAX, which provably holds the value ─────────
        //
        // The reload this replaces was the second half of the store-then-load
        // pair: `store [dst], rax` and then, a couple of instructions later,
        // `mov reg, [dst]` — the same word, written and read back across a
        // store-forwarding stall, once per phi per edge, i.e. once per loop
        // iteration for every loop-carried value.
        if let Some((phi, _)) = stage {
            // Staged straight into the phi's register above: the value is
            // already published, and the bookkeeping is the same either way so
            // that `emit_phi_copies`'s `published` check cannot tell the two
            // routes apart.
            self.mark_gp_reg_live(phi);
            published.push(phi);
            self.phi_copy_reg_publishes += 1;
        } else if self.phi_copy_regs_on {
            if let Some(&phi) = phi_of_dst.get(&dst) {
                if let Some(dst_reg) = self.assigned_gpr(phi) {
                    if !defer_publish.contains(&phi) {
                        self.emit_mov_reg_reg64(dst_reg, RAX);
                        self.mark_gp_reg_live(phi);
                        published.push(phi);
                        self.phi_copy_reg_publishes += 1;
                    }
                }
            }
        }
        // Round 11 (irlower): a cycle's parked value is consumed; clear the
        // scratch. It sits in the reserved bookkeeping tail the prologue zeroes
        // for the band verifier's sake (see `emit_prologue`), and after a swap
        // of two REFERENCES it held a live young pointer that no oop map names
        // and no shadow push publishes -- which refuses every moving cycle
        // that stops this frame, until the next cycle overwrites it. One store,
        // on the rare edge whose copies form a cycle.
        if matches!(op, CopyOp::Restore { .. }) && !prologue_zero_reserved_tail_disabled() {
            self.emit_zero_frame_slot(scratch);
        }
        Ok(())
    }

    // ── Code emission helpers ────────────────────────────────────────

    /// The callee-saved XMM registers this method actually parked a value in,
    /// paired with the frame offset each is saved at.
    ///
    /// Empty on System V (nothing is callee-saved), empty when the linear-scan
    /// path is off, and empty for any method whose allocation promoted nothing
    /// into XMM6/XMM7 — which is the common case even with the flag on. A
    /// method that uses none of them emits no save, no restore and pays only
    /// the reserved frame bytes.
    ///
    /// Reads the residency plan, which `lower_inner_with_array_list` installs
    /// BEFORE `lower()` runs (`set_residency`, "installed before the prologue
    /// and never touched again"). That ordering is what makes a *dynamic* save
    /// set legal against a *static* frame reservation: the set can only shrink
    /// relative to `IR_LOWER_SAVED_XMMS`, never grow past it.
    ///
    /// **A SNAPSHOT (`ls_saved_fp`), not a scan of `reg_of`.** It was the scan
    /// until split residency, and the scan was correct only for as long as
    /// `reg_of` never changed after installation. Split residency changes it,
    /// per position, so a prologue that saved XMM6 could be followed by an exit
    /// that no longer found XMM6 in `reg_of` and returned to its caller with
    /// XMM6 destroyed. The snapshot is taken once, over every register the plan
    /// can ever hand out, so the prologue and every epilogue read one answer.
    pub(in crate::ir_lower) fn saved_xmm_regs(&self) -> impl Iterator<Item = (u8, i32)> + '_ {
        let base = self.spill_cap_off;
        let reserved = self.saved_xmm_bytes;
        // Round 16 wave 17 (lane frames2): a sized band holds the `k`-th saved
        // register in its `k`-th slot (`size_save_bands`).
        let packed = self.save_bands_packed;
        self.saved_xmm_file
            .iter()
            .enumerate()
            .filter(move |_| reserved > 0)
            .filter(move |(_, reg)| self.ls_saved_fp[**reg as usize])
            .enumerate()
            // Cast: the save file has at most six elements.
            .map(move |(k, (i, reg))| {
                let slot = if packed { k } else { i };
                (*reg, base + (slot as i32 + 1) * 16)
            })
    }

    /// `MOVUPS [rbp - off], xmm` (save) or `MOVUPS xmm, [rbp - off]` (restore).
    ///
    /// Unaligned on purpose: `frame_size` is 16-byte aligned but `rbp` itself is
    /// only guaranteed 8-byte aligned at entry (the `push rbp` follows the
    /// caller's `call`), so `MOVAPS` would fault on half the call sites. The
    /// three-byte cost of the unaligned form is paid once per method.
    ///
    /// # REX (R14)
    ///
    /// This is the **fourth** no-REX XMM encoder in this file, and the one
    /// `NOTES-regalloc7.md` §1 did not name. It is fed by
    /// [`Self::saved_xmm_regs`], i.e. by `xmm_roles::IR_PROLOGUE_SAVED` — which
    /// is precisely the constant that same note says must grow in lockstep with
    /// `IR_LINEAR_SCAN` on Windows. Fixing `fp_load`/`fp_store`/`fp_binop` and
    /// then widening the two constants would therefore have moved the silent
    /// truncation from the body into the PROLOGUE, where it corrupts a
    /// caller's register instead of a callee's value. Hence the REX here.
    ///
    /// `MOVUPS` has no mandatory SSE prefix, so REX is the FIRST byte — unlike
    /// `fp_load`/`fp_store`, where it follows the `F2`/`F3`. Only REX.R can be
    /// wanted: the base is RBP (encoding 5).
    pub(in crate::ir_lower) fn emit_xmm_frame_move(&mut self, reg: u8, off: i32, store: bool) {
        if reg >= 8 {
            self.buf.emit_byte(0x44); // REX.R — MOVUPS has no prefix to follow
        }
        self.buf.emit(&[0x0F, if store { 0x11 } else { 0x10 }]);
        // ModRM + displacement for `[rbp - off]`, smallest legal form.
        self.emit_rbp_modrm_disp(reg, off);
    }

    /// The callee-saved GPRs this method actually parked a value in, paired
    /// with the frame offset each is saved at.
    ///
    /// The GPR band sits immediately below the XMM one, so register `i` of
    /// [`IR_LOWER_SAVED_GPRS`] lives at
    /// `spill_cap_off + saved_xmm_bytes + 8*(i+1)` — the same "+1 so the
    /// store's bytes land inside the reservation" shape `saved_xmm_regs` uses.
    ///
    /// Dynamic against a static reservation, for the reason stated there: the
    /// residency plan is installed before `lower()` runs, so this set can only
    /// shrink relative to [`IR_LOWER_SAVED_GPRS`], never grow past it. A method
    /// that promotes nothing emits no save and no restore.
    ///
    /// A SNAPSHOT (`ls_saved_gp`) for the reason `saved_xmm_regs` gives at
    /// length: split residency makes `gp_reg_of` change during the emission,
    /// and a prologue and an epilogue that disagree about which registers this
    /// frame owes its caller corrupt the caller's register state with nothing
    /// to notice.
    pub(in crate::ir_lower) fn saved_gpr_regs(&self) -> impl Iterator<Item = (u8, i32)> + '_ {
        let base = self.spill_cap_off + self.saved_xmm_bytes;
        let reserved = self.saved_gpr_bytes;
        // `ir_gp_file()`, not `IR_LOWER_SAVED_GPRS`: the save area covers the
        // registers this compile could HAND OUT, so with the kill switch off
        // the frame is byte-identical to the pre-widening tree rather than
        // paying 16 bytes for two registers nothing can name.
        // Round 16 wave 17 (lane frames2): a sized band holds the `k`-th saved
        // register in its `k`-th slot (`size_save_bands`).
        let packed = self.save_bands_packed;
        ir_gp_file()
            .iter()
            .enumerate()
            .filter(move |_| reserved > 0)
            .filter(move |(_, reg)| self.ls_saved_gp[**reg as usize])
            .enumerate()
            // Cast: `IR_LOWER_SAVED_GPRS` is a small compile-time constant
            // (five registers, or seven on Win64).
            .map(move |(k, (i, reg))| {
                let slot = if packed { k } else { i };
                (*reg, base + (slot as i32 + 1) * 8)
            })
    }

    /// `MOV [rbp - off], reg` (save) or `MOV reg, [rbp - off]` (restore), for a
    /// callee-saved GPR. Both directions already exist as frame accessors that
    /// pick the smallest displacement form; this names the pair.
    pub(in crate::ir_lower) fn emit_gpr_frame_move(&mut self, reg: u8, off: i32, store: bool) {
        if store {
            self.store_abi_reg(reg, off);
        } else {
            self.load_reg_from_frame(reg, off);
        }
    }

    /// Restore the callee-saved registers. Emitted at every exit, and it must
    /// not disturb RAX — a method's return value and the `i64::MIN`
    /// exception/deopt sentinel both travel there. `MOVUPS` into an XMM
    /// satisfies that for free, and the GPR restores target RBX/R12–R15 (plus
    /// RSI/RDI on Win64), a set RAX could never have been in because every
    /// member of it is callee-saved and RAX is not: see `IR_GP_LINEAR_SCAN`.
    pub(in crate::ir_lower) fn emit_callee_saved_restore(&mut self) {
        let restores: Vec<(u8, i32)> = self.saved_xmm_regs().collect();
        for (reg, off) in restores {
            self.emit_xmm_frame_move(reg, off, false);
        }
        let gpr_restores: Vec<(u8, i32)> = self.saved_gpr_regs().collect();
        for (reg, off) in gpr_restores {
            self.emit_gpr_frame_move(reg, off, false);
        }
    }

    pub(in crate::ir_lower) fn emit_prologue(&mut self) {
        // push rbp
        self.buf.emit_byte(0x55);
        // mov rbp, rsp
        self.buf.emit(&[0x48, 0x89, 0xE5]);
        // sub rsp, frame_size
        self.buf.emit(&[0x48, 0x81, 0xEC]);
        self.buf.emit(&self.frame_size.to_le_bytes());

        // gen r5w4/jit8, under default-on `CRATONVM_JIT_IR_ZERO_FRESH_FRAMES`
        // (item 1 of `docs/feature-designs/jit-ir-optimizations.md`):
        // a FRESH frame starts all-zero. Everything below is written after
        // this, so nothing of this activation is lost.
        self.frame_prezeroed = false;
        self.fresh_clear_skip = Vec::new();
        // JIT round 25 item 2, wave 3: which frame-block words the clear below
        // may leave to their own first writes (plans the block itself first;
        // the second `plan_frame_block` below computes the same block).
        // JIT round 25 wave 6 (W6-D, `CRATONVM_JIT_IR_LEAF_FRAME_ELIDE`): a
        // leaf frame (no GC point, no call, no exit; `plan_leaf_entry`) is
        // never seen by a collector, a stack walk or a deopt, so none of the
        // GC-only stores below runs for it -- this clear included -- and it
        // plans no frame block (published only at a safepoint).
        let leaf_frame = self.leaf_entry.plan.leaf_frame;
        if !leaf_frame {
            self.plan_block_lazy_clear();
        }
        if leaf_frame {
            self.leaf_entry.prologue_parts_skipped += 1;
        }
        if !leaf_frame
            && self.frame_size > 0
            && self.frame_size % 8 == 0
        {
            // Round 16 wave 6 (lane codegen, CG16-10): the same words, zeroed
            // with 16-byte stores (`emit_zero_frame_sse`), and the prologue's
            // later per-word zero stores, which would store zero over zero,
            // skipped (`frame_prezeroed`).
            // Under a MIR mode too: the prologue is no tile, and the level-2
            // byte oracle compares the whole body against the per-opcode one.
            //
            // Round 16 wave 7 (lane frames): through
            // `emit_fresh_frame_clear_sse`, which with its three switches off
            // emits exactly `emit_zero_frame_sse`; a word it leaves out keeps
            // its later per-word zero store (`fresh_clear_skipped`).
            {
                self.fresh_clear_skip = self.emit_fresh_frame_clear_sse(true);
                self.frame_prezeroed = true;
            }
        }

        // Callee-saved XMM save area. FIRST, before the ABI parameter stores:
        // an incoming floating-point argument arrives in XMM0..XMM3 (Win64) and
        // nothing here touches those, but ordering the save ahead of every
        // other prologue step keeps "the caller's registers are preserved" true
        // across the whole body rather than across most of it.
        let saves: Vec<(u8, i32)> = self.saved_xmm_regs().collect();
        for (reg, off) in saves {
            self.emit_xmm_frame_move(reg, off, true);
        }
        // …and the callee-saved GPR file, on the same footing and for the same
        // reason: every register in it belongs to the caller on both ABIs.
        let gpr_saves: Vec<(u8, i32)> = self.saved_gpr_regs().collect();
        for (reg, off) in gpr_saves {
            self.emit_gpr_frame_move(reg, off, true);
        }

        // Store params from ABI registers to local frame slots.
        // Windows: RCX, RDX, R8, R9.  SysV: RDI, RSI, RDX, RCX, R8, R9.
        // One list, shared with `incoming_abi_reg_capacity()` — the local copy
        // this used to keep could (and did) disagree with the bail that is
        // supposed to keep the `break` below unreachable.
        let abi_regs: &[u8] = ENTRY_ABI_REGS;

        // Gap B: a `needs_context` method receives the VM context pointer in
        // ABI[0] (the `try_call_with_context` convention), with the Java params
        // shifted to ABI[1..]. Store the context to its slot, then the params to
        // their local slots.
        //
        // An argument past the register file arrives on the CALLER'S STACK, and
        // this prologue used to drop it: the loop `break`s and the local keeps
        // whatever the frame slot happened to contain. `lower()` therefore
        // refused any method with more incoming slots than registers outright —
        // four on Win64, six on SysV — which made it "the commonest whole-method
        // refusal an ordinary accessor hits": an instance method with three
        // parameters is four slots, and touching one field turns
        // `needs_context` on, which is the fifth.
        //
        // It is loaded now, the way the single-pass prologue has loaded it since
        // the ROUND-12 fix (`x64/frames.rs`), from the same place, because the
        // two backends share the outgoing convention: `stack_arg_block_size`
        // here mirrors `x64::frames::stack_arg_block_size`, and a caller
        // materializes stack arguments at `[rsp + shadow]` immediately before
        // the CALL. After the CALL pushes the return address and this prologue
        // pushes RBP, argument `k` past the register file sits at
        // `[rbp + STACK_ARG_BASE + (k - reg_count) * 8]`.
        let base = if self.needs_context {
            self.store_abi_reg(abi_regs[0], self.context_slot_off);
            1
        } else {
            0
        };
        // Java arguments that fit the register file, then the rest off the
        // caller's frame. `reg_count` is how many of THIS method's arguments a
        // register carries, which is the file size minus the context slot.
        let reg_count = self.num_params.min(abi_regs.len().saturating_sub(base));
        for i in 0..reg_count {
            self.store_abi_reg(abi_regs[base + i], ((i as i32) + 1) * 8); // local_offset(i)
        }
        // Round 14 wave 7 (lane callcost3, CC6-2): the parameters' registers,
        // straight from the ABI registers they arrived in.
        let fills = self.plan_param_register_fills(base, reg_count);
        for (id, dst, src) in fills {
            self.emit_mov_reg_reg64_raw(dst, src);
            self.mark_gp_reg_live(id);
            self.param_register_fills.push((id, dst));
        }
        if !self.param_register_fills.is_empty()
            && false
        {
            eprintln!(
                "[ir-param-fill] filled (node, reg): {:?}",
                self.param_register_fills
            );
        }
        for i in reg_count..self.num_params {
            // `[RBP + disp]`, which `load_reg_from_frame` spells as a NEGATIVE
            // offset (it encodes `[RBP - offset]`). Via RAX because there is no
            // memory-to-memory MOV.
            let disp = STACK_ARG_BASE + ((i - reg_count) as i32) * 8;
            self.load_reg_from_frame(RAX, -disp);
            self.store_abi_reg(RAX, ((i as i32) + 1) * 8); // local_offset(i)
        }
        // ARGWIN21-3 (`CRATONVM_JIT_ZERO_INCOMING_STACK_ARGS`, default on): the
        // incoming stack words, now copied to their local slots, are zeroed.
        // They sit in the CALLER's frame (its outgoing block), which the
        // caller's band scan reads for the whole call, so a reference argument
        // this method stops using stayed rooted there until the call returned
        // (HotSpot's outgoing arguments belong to the callee's
        // liveness-exact map). Nothing reads them again: every reader of a
        // parameter after this point reads its local slot or its node's home,
        // and both C ABIs give the callee its incoming argument area.
        if reg_count < self.num_params && crate::zero_incoming_stack_args_enabled() {
            for i in reg_count..self.num_params {
                // Cast: as the copy loop above.
                let disp = STACK_ARG_BASE + ((i - reg_count) as i32) * 8;
                // MOV qword [rbp + disp32], 0
                self.buf.emit(&[0x48, 0xC7, 0x85]);
                self.buf.emit(&disp.to_le_bytes());
                self.buf.emit(&0i32.to_le_bytes());
            }
        }
        // Zero every local slot this method did NOT receive as a parameter.
        //
        // The JVM's definite-assignment rule means such a slot is never read
        // before it is written, so its entry value is unobservable to Java --
        // but it is very observable to the GC. `alloc`'s `long[] a` is slot 1,
        // and at the `new long[n]` safepoint one instruction later it still
        // holds whatever the previous frame at this address left there. In an
        // allocation-heavy workload that is a stale object pointer, so G1's
        // band verifier finds a movable-resident word that no oop map names
        // (correctly -- the dataflow knows the local is not live yet) and no
        // shadow push published, and refuses to relocate.
        //
        // That was the ENTIRE residual after the reserved-tail zeroing below:
        // 8.5-15.2% of pauses incomplete on `CoverageBench`, every report at
        // this one offset in this one leaf.
        //
        // Safe because this prologue runs only on a normal entry. This tier's
        // OSR entries do not jump past it into an interpreter-filled frame:
        // each enters through its own stub (`emit_osr_entry_stubs`), which
        // builds the frame, saves the callee-saved set into these same slots,
        // zeroes the locals the same way, and only THEN seeds the live ones
        // from the interpreter's snapshot. The single-pass trampoline
        // (`osr_pc_to_native`) is never used for this tier's artifacts.
        // (gcg g3/v: this used to say "the prologue is the sole entry", which
        // stopped being true when the tier gained OSR stubs.)
        //
        // Round 11 wave 16 (lane irback; step 4 of
        // `r11w14-rt-ir-lower-oop-free-frame-patch`): a reference-free body
        // (`lean_prologue`, below) skips this loop and the three GC-only
        // reserved-tail stores. Every one of those stores exists for the
        // collector's band verifier and conservative band scan; such a body is
        // published `frame_holds_no_references` (the artifact fill refuses the
        // compile otherwise), and the collector neither verifies nor rewrites
        // its band. No Java-visible reader exists: a JVM local past the
        // parameters is never read before it is written (definite assignment)
        // and is homed in a coloured slot, not here.
        let lean_prologue = self.lean_ref_free_prologue();
        // Round 16 wave 4 (lane codegen, CG16-8): the zero stores from here to
        // the end of the prologue use the RAX form (`emit_zero_frame_slot`).
        // Under a MIR mode too, for the reason the fresh-frame clear gives.
        self.prologue_zero_rax = ir_prologue_zero_rax_enabled();
        self.zero_rax_end = None;
        self.prologue_zero_stretch = true;
        if leaf_frame && self.num_locals > self.num_params {
            self.leaf_entry.prologue_parts_skipped += 1;
        }
        if !prologue_zero_unset_locals_disabled() && !lean_prologue && !leaf_frame {
            for i in self.num_params..self.num_locals {
                // Cast: a JVM local index times 8 -- `max_locals` is u16.
                self.emit_zero_frame_slot(((i as i32) + 1) * 8);
            }
        }
        // Before anything is zeroed: the frame block changes which reference
        // homes the prologue must clear, and makes the saved-base word its
        // "published" flag.
        // W6-D: a frame block is published at a safepoint; a leaf frame has
        // none, so it plans none (`verify_leaf_entry` re-checks it is empty).
        if leaf_frame {
            self.frame_block.clear();
        } else {
            self.plan_frame_block();
        }
        // JIT round 25 item 2, wave 3: the lazy clear was planned against the
        // block the call above recomputed; a word it left uncleared that the
        // block no longer names would be published per call unwritten.
        if !self
            .block_lazy_clear
            .iter()
            .all(|w| self.frame_block.binary_search(w).is_ok())
        {
            self.latch_bailout(Bailout::new(BailoutReason::Internal(
                "ir_lower: the frame block changed under the lazy clear's plan",
            )));
        }
        // Zero the cached-thread and watermark slots BEFORE the fetch. The
        // fetch is erased (NOP'd) by `finish_lazy_thread_fetch` when the method
        // publishes nothing, and every consumer below is null-guarded on the
        // thread slot — so it must read 0, not uninitialised stack. The
        // single-pass backend zero-initialises for exactly this reason (see the
        // incident writeup on `x64.rs::Compiler::emit_epilogue`).
        // W6-D: a leaf frame emits none of those readers (no deopt stub, no
        // call-exception stub, no shadow push, no savetop restore: verified by
        // `verify_leaf_entry`), so the four bookkeeping words stay unwritten.
        if leaf_frame && self.get_current_thread != 0 && self.shadow_thread_slot_off > 0 {
            self.leaf_entry.prologue_parts_skipped += 1;
        }
        if !leaf_frame && self.get_current_thread != 0 && self.shadow_thread_slot_off > 0 {
            // KEPT for a lean body: the thread slot has null-guarded readers
            // (the deopt stub's retraction, the call-exception stubs) that
            // would dereference a non-zero stale word.
            self.emit_zero_frame_slot(self.shadow_thread_slot_off);
            // Round 11 wave 16 (lane irback): the watermark's only readers are
            // `emit_shadow_savetop_restore` (erased for a body with no roots)
            // and a rethrow pad's copy-back (only with a pending publication,
            // which a reference-free body never has); the fetch's capture, if
            // it survives, writes it first. A lean body skips the store.
            if !lean_prologue {
                self.emit_zero_frame_slot(self.shadow_savetop_slot_off);
            }
            // ... and the OTHER two reserved bookkeeping words, for the same
            // reason and a second one.
            //
            // `shadow_savebase` is written only by a shadow PUSH and
            // `phi_copy_scratch` only by a parallel copy that needs to break a
            // cycle. A method that does neither -- every no-oop leaf, and
            // `finish_lazy_thread_fetch` erases the fetch in exactly those --
            // leaves both holding whatever the previous frame at this address
            // left behind. In allocation-heavy code that is a stale object
            // pointer, and G1's band verifier then finds a movable-resident
            // word in the reserved-locals tail that no oop map names and no
            // shadow push published, and correctly refuses to relocate the
            // whole collection.
            //
            // Measured on `CoverageBench` after the §26 window repair: the
            // ENTIRE residual incomplete rate (~19%, 386 reports over six runs)
            // was `CoverageBench.alloc`, at three offsets -- and two of them are
            // these. Neither word is ever read while it holds that garbage, so
            // zeroing costs two prologue stores and removes the refusal.
            //
            // Kept inside this gate deliberately: it is the same "the reserved
            // tail must read 0, not uninitialised stack" contract the two
            // stores above already establish, and the same block the
            // single-pass backend zero-initialises for.
            // With a frame block the saved-base word is the block's
            // "published" flag, which every safepoint reads — so it is zeroed
            // whatever the reserved-tail switch says.
            //
            // A lean body has no frame block, no shadow push and no pending
            // publication, which are the saved base's only readers, and its
            // phi scratch is written before every read (`emit_phi_copies`,
            // the `newarray` length staging); both stores exist for the band
            // verifier alone, which does not verify such a frame.
            if (!prologue_zero_reserved_tail_disabled() && !lean_prologue)
                || !self.frame_block.is_empty()
            {
                if self.shadow_savebase_slot_off > 0 {
                    self.emit_zero_frame_slot(self.shadow_savebase_slot_off);
                }
            }
            if !prologue_zero_reserved_tail_disabled() && !lean_prologue {
                if self.phi_copy_scratch_slot_off > 0 {
                    self.emit_zero_frame_slot(self.phi_copy_scratch_slot_off);
                }
            }
        }
        // And the safepoint-id slot, for the SAME reason and on its own gate:
        // `active_safepoint_id` reads `[rbp - sp_id_slot_off]` on any live
        // frame, including one stopped BEFORE its first safepoint, and until
        // that first `emit_safepoint_map` store the word is whatever the
        // previous frame at this stack depth left behind.
        //
        // Ids start at 1 precisely so 0 can mean "no safepoint reached" (see
        // the slot's allocation above), but nothing was writing the 0. The
        // sentinel was a convention the prologue never established.
        //
        // MEASURED, `TestCachedQueryResults` (H2), 13 frames the band verifier
        // reported as `no-map-for-id`: 10 read `0` and 3 read a HEAP POINTER at
        // `sp_id_off`. That was diagnosed as two defects -- "has not reached a
        // safepoint" and "something stored an oop into the reserved slot". It
        // is one: uninitialised stack, reading as zero where the region happened
        // to be clean and as a stale oop where it did not.
        //
        // Refusing the cycle is the benign outcome. The hazard this closes is
        // that safepoint ids are small consecutive integers, so a stale word
        // can equal a VALID id for this method -- and then
        // `find_oop_map_for_safepoint_id` matches the map for a DIFFERENT
        // program point and relocation rewrites against it. That is a silent
        // wrong answer, not a refusal.
        //
        // `CRATONVM_JIT_ZERO_SPID=0` restores the uninitialised read: the
        // one-binary A/B, and the kill switch.
        // W6-D: the safepoint-id word, the frame record (the innermost-RBP and
        // compile-id mirrors), the thread fetch and the reference-home zeroing
        // are read only at a GC point, a call or an exit of this frame; a leaf
        // frame has none, and the mirrors keep naming its caller, a live frame.
        if leaf_frame {
            self.leaf_entry.prologue_parts_skipped += 1;
            self.mark_ref_phi_slots_defined();
        } else {
            if self.sp_id_slot_off > 0 && Self::zero_sp_id_slot_enabled() {
                self.emit_zero_frame_slot(self.sp_id_slot_off);
            }
            self.emit_frame_record();
            self.fetch_current_thread();
            self.zero_ref_phi_slots();
            // The entry-block refinement holds only for per-call publication; a
            // frame block publishes every home at the first safepoint, including
            // one the entry block has not written yet (`plan_frame_block`).
            self.zero_ref_value_slots(/* entry_runs */ self.frame_block.is_empty());
        }
        self.prologue_zero_rax = false;
        self.zero_rax_end = None;
        self.prologue_zero_stretch = false;
    }

    /// Round 14 wave 7 (lane callcost3, proposal CC6-2 of
    /// `jit-r14-callcost2-proposals-RETIRED-20260930.md`; F8-4 / LC-5 of
    /// `r12w8-callcost-fib-call-anatomy-FIXED-20261001.md`): the `int`/`long`
    /// parameters whose register [`Self::emit_prologue`] fills with `MOV dst,
    /// abi` right after storing the ABI register to the prologue word, as
    /// `(Op::Param node, dst, abi)`. The prologue marks each one live in `dst`
    /// (`mark_gp_reg_live`), and its `Op::Param` arm then skips the reload
    /// `publish_gp_from_slot` would make from that word (the store-forwarded
    /// `MOV dst, [rbp - 8]` of every activation that reaches the prologue) --
    /// but only while [`Self::param_register_filled`] still says `dst` holds
    /// it.
    ///
    /// Why that answer is exact: the value is the whole incoming register, the
    /// same 64 bits the prologue word holds and the reload would read. Every
    /// write of a GP-file register by the lowering is a publish, which goes
    /// through `mark_gp_reg_live` / `ls_publish` / `ls_drop_residency` and
    /// takes the previous owner's bit away, so a value the linear scan hands
    /// `dst` before the parameter's interval starts clears the fill and the
    /// arm reloads as before. The admission keeps that bookkeeping trivially
    /// complete on the only code between the fill and the arm: the parameter
    /// is in block 0 and only `Op::Param` / `Op::Const` nodes precede it there
    /// (straight-line, no merge, no call), and what else runs in between --
    /// the rest of the prologue, the entry poll and its outlined slow path --
    /// writes RAX/R10/R11, memory, or calls helpers that preserve the
    /// callee-saved file the GP file is made of (C ABI). `dst` is saved by the
    /// prologue before the fill (`ls_saved_gp` covers every register the scan
    /// handed out) and is never an incoming ABI register.
    ///
    /// Off under a MIR mode (its tiles publish on their own), the `int`
    /// poisoning stress mode (the arm writes the poisoned high half into the
    /// word and the register must read it back), and with
    /// `CRATONVM_JIT_IR_PARAM_HOME_ALIAS=0` (the arm then copies the word
    /// through RAX). Kill switch: `CRATONVM_JIT_IR_PARAM_REGISTER_FILL=0`,
    /// read per compile.
    pub(in crate::ir_lower) fn plan_param_register_fills(&self, base: usize, reg_count: usize) -> Vec<(NodeId, u8, u8)> {
        let mut fills: Vec<(NodeId, u8, u8)> = Vec::new();
        if !ir_param_register_fill_enabled()
            || self.mir_mode != MirMode::Off
            || self.poison_int_high_on
        {
            return fills;
        }
        let Some(entry) = self.schedule.blocks.first() else {
            return fills;
        };
        for &n in &entry.nodes {
            let Some(node) = self.graph.nodes.get(n as usize) else {
                break;
            };
            let idx = match node.op {
                Op::Const(_) => continue,
                Op::Param(idx) => usize::from(idx),
                _ => break,
            };
            if !matches!(node.ty, IrType::Int | IrType::Long) || idx >= reg_count {
                continue;
            }
            let (Some(dst), Some(&src)) = (self.assigned_gpr(n), ENTRY_ABI_REGS.get(base + idx))
            else {
                continue;
            };
            if dst == src
                || ENTRY_ABI_REGS.contains(&dst)
                || !ir_gp_file().contains(&dst)
                || self.saved_gpr_bytes <= 0
                || !self.ls_saved_gp.get(dst as usize).copied().unwrap_or(false)
                || fills.iter().any(|&(m, d, _)| m == n || d == dst)
            {
                continue;
            }
            fills.push((n, dst, src));
        }
        fills
    }

    /// Did [`Self::emit_prologue`] fill `id`'s register, and does it still
    /// hold `id` (no publish into it since)? Round 14 wave 7 (lane callcost3,
    /// CC6-2; [`Self::plan_param_register_fills`]).
    pub(in crate::ir_lower) fn param_register_filled(&self, id: NodeId) -> bool {
        self.param_register_fills.iter().any(|&(n, reg)| {
            n == id
                && self.resident_gpr(id) == Some(reg)
                && self.gp_reg_owner.get(reg as usize).copied().flatten() == Some(id)
        })
    }

    /// Does [`Self::emit_prologue`] skip its GC-only zeroing for this body?
    /// Round 11 wave 16 (lane irback; step 4 of
    /// `r11w14-rt-ir-lower-oop-free-frame-patch`).
    ///
    /// True exactly for a body [`Self::body_publishes_no_roots`] predicts
    /// reference-free, spelled without `frame_block` so it can be asked before
    /// `plan_frame_block` (the block is planned from these same two lists, so
    /// it is empty whenever they are, and the answer is identical). The
    /// artifact fill refuses a compile whose body this predicts reference-free
    /// but which is not then published `frame_holds_no_references`, so a
    /// skipped store is never left for a collector that would verify it.
    ///
    /// Kept: the thread-slot zeroing (null-guarded readers) and the
    /// safepoint-id zeroing (the innermost frame reads its raw slot for a line
    /// number before its first call). Kill switch:
    /// `CRATONVM_JIT_IR_REF_FREE_LEAN_PROLOGUE=0`, read per compile.
    pub(in crate::ir_lower) fn lean_ref_free_prologue(&self) -> bool {
        self.ref_node_ids.is_empty()
            && self.ref_param_homes.is_empty()
            && true
    }

    /// Publish this frame's exact RBP into the precise-maps innermost-RBP
    /// mirror, exactly as the single-pass prologue does.
    ///
    /// Without it the collector cannot locate the frame at all:
    /// `moving_young_frame_coverage_complete` needs an address to resolve
    /// `sp_id_slot_off` and the map's slot offsets against, and
    /// `PreciseFrameInfo::exact_rbp` is only ever a snapshot of this mirror. An
    /// IR frame that never wrote it left the mirror holding `0`, and the
    /// verifier refused the whole cycle with `missing-exact-rbp` — before it
    /// ever looked at an oop map.
    ///
    /// That is why the relocation contract, though implemented and sound,
    /// changed nothing observable: with `CRATONVM_JIT_FORCE_C2=1` on the
    /// `IrEscapeProbe` the collector reported `cycles=0 coverage_fallbacks=14,
    /// missing-exact-rbp=14` — every young collection diverted for want of a
    /// nine-byte store, with the maps and the shadow publication both present
    /// and correct. Emitted after the ABI parameter stores for the same reason
    /// the single-pass backend does it there.
    pub(in crate::ir_lower) fn emit_frame_record(&mut self) {
        if !crate::x64::precise_jit_maps_enabled() || self.frame_record == 0 {
            return;
        }
        let disp = crate::x64::inline_rbp_tls_disp();
        if disp != 0 {
            self.emit_mov_tls_disp32_rbp(disp as u32);
            // …and the identity of the frame that RBP names. Without it the GC
            // must decode the call that created this frame, which is impossible
            // when the caller reached us indirectly (`CALL R11` — every inline
            // cache hit).
            self.emit_frame_record_identity();
        } else {
            // No usable TLS displacement on this target: fall back to the
            // helper. Params are already in frame slots, so its caller-saved
            // clobbers cost nothing here.
            self.emit_mov_arg0_rbp();
            self.emit_mov_reg_imm64(RAX, self.frame_record as u64);
            self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
        }
    }

    /// Restore this frame's RBP into the mirror after a call returned.
    ///
    /// A compiled callee publishes its OWN rbp on entry, so after it returns
    /// the mirror names the wrong (now-dead) frame. The single-pass backend
    /// republishes at every call site (`emit_post_call_rbp_republish`); the
    /// shared allocation stub does the same
    /// (`runtime_lowering::emit_post_call_frame_republish`). This is the IR
    /// tier's equivalent, and it must not disturb RAX — the callee's return
    /// value is still in it — which the inline TLS store satisfies for free.
    pub(in crate::ir_lower) fn emit_post_call_frame_record(&mut self) {
        if !crate::x64::precise_jit_maps_enabled() || self.frame_record == 0 {
            return;
        }
        let disp = crate::x64::inline_rbp_tls_disp();
        if disp != 0 {
            self.emit_mov_tls_disp32_rbp(disp as u32);
            // The callee published its own identity on entry; restoring only
            // the RBP would leave the pair naming two different frames.
            self.emit_frame_record_identity();
            return;
        }
        self.buf.emit_byte(0x50); // PUSH RAX (preserve the Java return value)
        #[cfg(target_os = "windows")]
        const RESERVE: u8 = 40; // shadow space + alignment after the PUSH
        #[cfg(not(target_os = "windows"))]
        const RESERVE: u8 = 8; // restore 16-byte call-site alignment
        self.buf.emit(&[0x48, 0x83, 0xEC, RESERVE]); // SUB RSP, reserve
        self.emit_mov_arg0_rbp();
        self.emit_mov_reg_imm64(RAX, self.frame_record as u64);
        self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
        self.buf.emit(&[0x48, 0x83, 0xC4, RESERVE]); // ADD RSP, reserve
        self.buf.emit_byte(0x58); // POP RAX
    }

    /// JIT r21 wave 1 (lane typecheck, TYPECHECK21-3): the array-target plan
    /// of a site whose resolved target `cid` is the array class `site_name`
    /// ([`typecheck_array_plan`]), or `None`. `CRATONVM_DBG_JITC` names each
    /// `[L...;` site.
    pub(in crate::ir_lower) fn typecheck_array_plan_for(
        &self,
        site_name: Option<&str>,
        cid: u32,
        op: &str,
    ) -> Option<TypecheckArrayPlan> {
        let plan = typecheck_array_plan(&self.direct_helpers, site_name, cid);
        if crate::x64::dbg_jitc_enabled() && site_name.is_some_and(|n| n.starts_with("[L")) {
            match plan {
                Some(p) => eprintln!(
                    "[cratonvm-jitc] ir {op} array arm: target={cid} component={} object={} display={}",
                    p.component,
                    p.object_component,
                    p.display.is_some()
                ),
                None => eprintln!("[cratonvm-jitc] ir {op} array arm DECLINED: target={cid}"),
            }
        }
        plan
    }

    /// The array-target arm itself (TYPECHECK21-3), with RAX = a non-null
    /// receiver: a PLAIN object skips it (`JE` past it, to the caller's
    /// plain-object arms); an array is answered by
    /// [`typecheck_array_hit_bytes`], its hits appended to `hits` and its
    /// declines to `slow`. Writes RCX, R11 and the flags; RAX is preserved.
    ///
    /// t1 w8 DISP21 (DISPATCH21-5, the rows form; `CRATONVM_JIT_TYPECHECK_ROWS`,
    /// admitted as [`Self::emit_typecheck_row_arm`] admits a plain receiver --
    /// an interface component under `instanceof`, every component with
    /// `_ROWS_ALL`): a reference array's header holds its immediate
    /// component's id, so the component's type-check row answers the HIT in
    /// one compare (JVMS 6.5: `S[]` is a `T[]` iff `S` is assignable to `T`;
    /// a nested receiver's component is an array class, whose row -- if it
    /// had one -- lists only `Object` and its own interfaces, which is JVMS
    /// too). Hits only: anything else falls into the display arm below,
    /// which reloads RCX.
    ///
    /// j27 wave 2 (lane TCARRAY, `CRATONVM_JIT_TYPECHECK_ARRAY_ARM` +
    /// `_ARRAY_MISS`): with `zero` (an `instanceof`), a receiver that is not a
    /// reference array is answered `0` here
    /// ([`crate::typecheck_rows::typecheck_array_miss_guard_bytes`]: a plain
    /// object or a primitive array is never a `T[]`, JVMS 6.5) instead of
    /// passing to the caller's plain-object arms, which can only call the
    /// helper for an array target; those arms are then reached by nothing.
    pub(in crate::ir_lower) fn emit_typecheck_array_arm(
        &mut self,
        plan: TypecheckArrayPlan,
        op: &str,
        hits: &mut Vec<usize>,
        slow: &mut Vec<usize>,
        zero: Option<&mut Vec<usize>>,
    ) {
        let mut zero = zero.filter(|_| crate::typecheck_rows::typecheck_array_miss_guard_enabled());
        let plain = match zero.as_deref_mut() {
            Some(zero) => {
                self.emit_typecheck_array_miss_guard(op, plan, slow, zero);
                None
            }
            None => {
                // TEST BYTE [RAX+KIND_TAGS_BYTE_OFFSET], KIND_TAG_BYTE_MASK ; JE plain.
                self.buf.emit(&[
                    0xF6,
                    0x40,
                    cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8, // Cast: x86-64 disp8
                    cratonvm_types::KIND_TAG_BYTE_MASK,
                ]);
                Some(self.emit_jcc_rel32(0x84))
            }
        };
        self.note_rcx_written();
        if let (false, Some(display)) = (plan.object_component, plan.display) {
            // Only where the row arm will be emitted, so a declined site keeps
            // dev's bytes exactly.
            if typecheck_row_arm_admitted(display, op)
                && plan.component != 0
                && (display.interface
                    || (1..=crate::typecheck_rows::PRIMARY_MAX_DEPTH).contains(&display.depth))
                && self.typecheck_rows_base().is_some()
            {
                // CMP BYTE [RAX+KIND_TAGS], REF_ARRAY ; JNE past the row arm
                // (a primitive array's component word is not a class id).
                self.buf.emit(&[
                    0x80,
                    0x78,
                    cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8, // Cast: x86-64 disp8
                    REF_ARRAY_KIND_TAGS,
                ]);
                let not_ref = self.emit_jcc_rel32(0x85);
                let mut no_miss: Vec<usize> = Vec::new();
                self.emit_typecheck_row_arm(display, plan.component, false, op, None, hits, &mut no_miss);
                debug_assert!(no_miss.is_empty());
                self.patch_rel32_to_here(not_ref);
            }
        }
        let (bytes, edges, arm_hits) = typecheck_array_hit_bytes(plan);
        let at = self.buf.pos();
        self.buf.emit(&bytes);
        hits.extend(arm_hits.into_iter().map(|h| at + h));
        // tca21 (TCARRAY21-1, second half): an `instanceof` whose miss guard
        // ran (so the receiver is exactly a reference array) answers the
        // hit arm's declines from the component's display too -- a component
        // the display proves hierarchy-only NOT a subtype of `T` is a `0`,
        // everything else still asks the helper. `CRATONVM_JIT_TYPECHECK_DISPLAY_MISS`
        // off keeps dev's hit-only arm.
        match (zero, plan.display) {
            (Some(zero), Some(display))
                if !plan.object_component && typecheck_display_miss_enabled() =>
            {
                for e in edges {
                    self.patch_rel32_to_here(at + e);
                }
                self.emit_typecheck_array_component_miss(op, plan, display, hits, slow, zero);
            }
            _ => slow.extend(edges.into_iter().map(|e| at + e)),
        }
        if let Some(plain) = plain {
            self.patch_rel32_to_here(plain);
        }
    }

    /// tca21 (TCARRAY21-1, second half): the component miss of an `instanceof
    /// T[]`, emitted where [`typecheck_array_hit_bytes`]'s declines land with
    /// RAX = a reference array (the miss guard has run). The array's header
    /// class id is its COMPONENT's, so the plain-object display answer
    /// ([`typecheck_array_component_miss_bytes`]) applies to it unchanged: a
    /// hit joins `hits`, a hierarchy-only non-subtype `zero`, every other
    /// outcome (no display, a nested array or lambda-proxy component, a
    /// verdict-0 closure, the duplicate-name latch) `slow`. Writes RCX, R11
    /// and the flags; RAX is preserved. Nothing falls through.
    pub(in crate::ir_lower) fn emit_typecheck_array_component_miss(
        &mut self,
        op: &str,
        plan: TypecheckArrayPlan,
        display: VhSubtypeDisplay,
        hits: &mut Vec<usize>,
        slow: &mut Vec<usize>,
        zero: &mut Vec<usize>,
    ) {
        self.note_rcx_written();
        let latch = cratonvm_types::duplicate_class_name_seen_addr();
        let (bytes, edges, hit, miss) =
            typecheck_array_component_miss_bytes(display, plan.component, latch);
        let at = self.buf.pos();
        self.buf.emit(&bytes);
        slow.extend(edges.into_iter().map(|e| at + e));
        hits.push(at + hit);
        zero.push(at + miss);
        if crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] ir {op} array arm: component={} component-miss",
                plan.component
            );
        }
    }

    /// j27 wave 2 (lane TCARRAY): the miss guard of an `instanceof T[]`
    /// ([`crate::typecheck_rows::typecheck_array_miss_guard_bytes`]), with RAX
    /// = a non-null receiver: a reference array falls through to the hit
    /// arms; its misses join `zero`, its screens `slow`. Writes RCX, R11 and
    /// the flags. `CRATONVM_DBG_JITC` names the site.
    pub(in crate::ir_lower) fn emit_typecheck_array_miss_guard(
        &mut self,
        op: &str,
        plan: TypecheckArrayPlan,
        slow: &mut Vec<usize>,
        zero: &mut Vec<usize>,
    ) {
        let latch = cratonvm_types::duplicate_class_name_seen_addr();
        let guard = crate::typecheck_rows::typecheck_array_miss_guard_bytes(latch);
        let at = self.buf.pos();
        self.buf.emit(&guard.bytes);
        slow.extend(guard.slow.into_iter().map(|e| at + e));
        zero.extend(guard.zero.into_iter().map(|e| at + e));
        if crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] ir {op} array arm: component={} not-array-miss",
                plan.component
            );
        }
    }

    /// JIT r20 wave 11 (lane pAlloc; book `jit-r21-nio-proposals-w14-L4.md`
    /// W14L4-3; `CRATONVM_JIT_TYPECHECK_DISPLAY_ARM`, default ON, `=0` keeps
    /// the exact-class / seen-way arms alone): the supers display of the
    /// typecheck target `cid` (`vh_const_display`: a class with a published
    /// display, or any interface), or `None` -- then the site emits no
    /// display arm and is byte-identical to before. `CRATONVM_DBG_JITC` names
    /// each site.
    pub(in crate::ir_lower) fn typecheck_display_for(&self, cid: u32, op: &str) -> Option<VhSubtypeDisplay> {
        if !typecheck_display_arm_enabled() {
            return None;
        }
        let display = vh_const_display(&self.direct_helpers, cid);
        if crate::x64::dbg_jitc_enabled() {
            match display {
                Some(d) => eprintln!(
                    "[cratonvm-jitc] ir {op} display arm: target={cid} interface={} depth={}",
                    d.interface, d.depth
                ),
                None => eprintln!(
                    "[cratonvm-jitc] ir {op} display arm DECLINED: target={cid} has no display"
                ),
            }
        }
        display
    }

    /// The display arm itself, with RAX = a non-null PLAIN object (the caller
    /// has screened null and arrays): falls through when the object's class
    /// is `cid` or a subtype of it per its published display, else jumps to
    /// one of the rel32 edges appended to `decline`. Writes RCX, R11 and the
    /// flags; RAX is preserved.
    ///
    /// Only a HIT is answered here. A miss always declines to the helper,
    /// because the helper's `true` is wider than the hierarchy (an instance-
    /// admitted proxy or `Unmodifiable*` stamp, a non-hierarchy id at or past
    /// `0x8000_0000`, the duplicate-class-name fallback), while its `true` for
    /// a class that the display proves a subtype is exactly JVMS 6.5. An id
    /// past the table's capacity (any of those synthetic ids) declines.
    ///
    /// Why the display can be trusted from compiled code without a lock, for
    /// every thread and across redefinition (the reasoning on
    /// `VhSubtypeDisplay` and `jit_realm::SupersDisplays`): a display is
    /// published once, Release-stored after its words, and never changed or
    /// freed for the VM's life; a table is never freed, and a reader racing a
    /// growth sees an older table that merely lacks the newest classes -- a
    /// decline; x86-64 loads are acquire, so a non-zero entry is a complete
    /// display. Class ids are never reused within a VM, and a redefinition
    /// cannot change a class's superclass or interfaces, so a display, once
    /// published, stays the class's hierarchy. An object whose class has no
    /// display yet (used before its `<clinit>` completed) declines.
    pub(in crate::ir_lower) fn emit_typecheck_display_hit(
        &mut self,
        display: VhSubtypeDisplay,
        cid: u32,
        decline: &mut Vec<usize>,
    ) {
        self.note_rcx_written();
        // MOV ECX, DWORD [RAX] -- `ObjectHeader.class_id`, the `u32` at
        // offset 0 (zero-extends RCX).
        self.buf.emit(&[0x8B, 0x08]);
        let (bytes, edges) = typecheck_display_hit_bytes(display, cid);
        let at = self.buf.pos();
        self.buf.emit(&bytes);
        decline.extend(edges.into_iter().map(|e| at + e));
    }

    /// JIT r20 wave 17 (lane pAlloc2; `CRATONVM_JIT_TYPECHECK_DISPLAY_MISS`):
    /// [`Self::emit_typecheck_display_hit`] with the miss answered too
    /// ([`typecheck_display_answer_bytes`]), for an `instanceof` whose target
    /// the caller screened ([`Self::typecheck_display_miss_for`]). RAX = a
    /// non-null PLAIN object; appends the rel32 edges to the `1` (`one`), the
    /// `0` (`zero`) and the helper (`decline`). Writes RCX, R11 and the flags;
    /// RAX is preserved.
    /// JIT r23 w2c close (`CRATONVM_JIT_TYPECHECK_MISS_MEMO`): the memo probe
    /// of an `instanceof` whose receiver (non-null, a plain object) is in
    /// RAX -- `MOV R11, memo; MOV ECX, [RAX]; CMP ECX, [R11]; JE zero; CMP
    /// ECX, [R11+4]; JE zero` ([`crate::typecheck_miss_memo_cell`] says what a
    /// way proves). The caller has screened the duplicate-class-name latch.
    pub(in crate::ir_lower) fn emit_typecheck_miss_memo_probe(&mut self, memo: usize, zero: &mut Vec<usize>) {
        self.note_rcx_written();
        self.emit_mov_reg_imm64(R11, memo as u64);
        self.buf.emit(&[0x8B, 0x08]); // MOV ECX, [RAX]
        self.buf.emit(&[0x41, 0x3B, 0x0B]); // CMP ECX, [R11]
        zero.push(self.emit_jcc_rel32(0x84));
        self.buf.emit(&[0x41, 0x3B, 0x4B, 0x04]); // CMP ECX, [R11+4]
        zero.push(self.emit_jcc_rel32(0x84));
    }

    /// The memo fill on an inline PROVEN miss: way 0 while it still holds the
    /// target's own id, else way 1 (so a site that misses on more than two
    /// classes rewrites only way 1). Ends falling through to the caller's
    /// `0` jump, or jumping to `0` itself after the way-0 write.
    pub(in crate::ir_lower) fn emit_typecheck_miss_memo_fill(&mut self, memo: usize, target: u32, zero: &mut Vec<usize>) {
        self.note_rcx_written();
        self.buf.emit(&[0x8B, 0x08]); // MOV ECX, [RAX]
        self.emit_mov_reg_imm64(R11, memo as u64);
        self.buf.emit(&[0x41, 0x81, 0x3B]); // CMP DWORD [R11], imm32
        self.buf.emit(&target.to_le_bytes());
        let way1 = self.emit_jcc_rel32(0x85);
        self.buf.emit(&[0x41, 0x89, 0x0B]); // MOV [R11], ECX
        zero.push(self.emit_jmp_rel32());
        self.patch_rel32_to_here(way1);
        self.buf.emit(&[0x41, 0x89, 0x4B, 0x04]); // MOV [R11+4], ECX
    }

    /// Round rx27 (lane compile): the lambda-proxy HIT-way fill after a
    /// helper `1` of an `instanceof`; RAX holds the cell, R11D the proxy id
    /// ([`proxy_hit_way_fill_bytes`]).
    pub(in crate::ir_lower) fn emit_proxy_hit_way_fill(&mut self) {
        self.buf
            .emit(&proxy_hit_way_fill_bytes(false, ir_proxy_two_hit_ways_enabled()));
    }

    pub(in crate::ir_lower) fn emit_typecheck_display_answer(
        &mut self,
        display: VhSubtypeDisplay,
        cid: u32,
        site_name: Option<&str>,
        decline: &mut Vec<usize>,
        one: &mut Vec<usize>,
        zero: &mut Vec<usize>,
    ) {
        self.note_rcx_written();
        // MOV ECX, DWORD [RAX] -- `ObjectHeader.class_id` (zero-extends RCX).
        self.buf.emit(&[0x8B, 0x08]);
        let latch = if typecheck_site_is_boot_package(site_name) {
            0
        } else {
            cratonvm_types::duplicate_class_name_seen_addr()
        };
        let (bytes, edges, hit, miss) = typecheck_display_answer_bytes(display, cid, latch);
        let at = self.buf.pos();
        self.buf.emit(&bytes);
        decline.extend(edges.into_iter().map(|e| at + e));
        one.push(at + hit);
        zero.push(at + miss);
    }

    /// JIT r20 wave 17 (lane pAlloc2): may this `instanceof` site answer a
    /// display miss inline? The target-side half of the proof
    /// ([`typecheck_display_answer_bytes`] checks the receiver side at run
    /// time): the switch is on, and the target is not one the
    /// `synthetic_implements` name rules can admit
    /// (`cratonvm_types::synthetic_targets`, proved exhaustive over those
    /// rules by the VM's own test). Every other non-hierarchy `true` of
    /// `jit_typecheck_resolve` is receiver-side (a lambda proxy id has no
    /// display; the instance-admitted shapes carry verdict `0`) or the
    /// duplicate-name rule the latch screens. `CRATONVM_DBG_JITC` names the
    /// site.
    pub(in crate::ir_lower) fn typecheck_display_miss_for(&self, cid: u32, site_name: Option<&str>) -> bool {
        // JIT round 21 (lane typecheck, `CRATONVM_JIT_TYPECHECK_DISPLAY_MISS_ANY_TARGET`):
        // the target screen is moot once `synthetic_implements` declines every
        // receiver whose supertype closure is hierarchy-only -- the same
        // predicate, over the same closure, as the display's verdict word the
        // emitted miss already requires (`receiver_closure_is_hierarchy_only`
        // in the VM's `typecheck.rs`).
        let ok = typecheck_display_miss_admits_target(site_name);
        if crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] ir instanceof display miss arm: target={cid} {}",
                if ok { "answered" } else { "DECLINED" }
            );
        }
        ok
    }

    /// JIT round 21, lane dispatch (`CRATONVM_JIT_TYPECHECK_ROWS`): the base of
    /// this VM's type-check rows (`crate::typecheck_rows`), or `None` (switch
    /// off, no displays, the VM could not allocate them).
    pub(in crate::ir_lower) fn typecheck_rows_base(&self) -> Option<usize> {
        typecheck_rows_base_for(&self.direct_helpers)
    }

    /// JIT round 21, lane dispatch (`CRATONVM_JIT_TYPECHECK_ROWS`): the
    /// type-check row arm ([`crate::typecheck_rows::typecheck_row_bytes`]) in
    /// front of a site's display arm, with RAX = a non-null PLAIN object.
    /// Appends its hit edges to `one` and, when `miss_ok` (the site's display
    /// miss screen passed), its miss edges to `zero`; every other outcome falls
    /// through to the code the caller emits next, which is the site's code
    /// before this arm. Writes RCX, R11 and the flags; RAX is preserved.
    /// Returns whether an arm was emitted. `CRATONVM_DBG_JITC` names the site.
    pub(in crate::ir_lower) fn emit_typecheck_row_arm(
        &mut self,
        display: VhSubtypeDisplay,
        cid: u32,
        miss_ok: bool,
        op: &str,
        site_name: Option<&str>,
        one: &mut Vec<usize>,
        zero: &mut Vec<usize>,
    ) -> bool {
        use crate::typecheck_rows::{typecheck_row_bytes, RowTarget};
        let kind = if display.interface {
            RowTarget::Interface
        } else {
            RowTarget::Class {
                depth: display.depth,
            }
        };
        // The w1g host A/B (JIT r21): in front of the seen ways and on every
        // target the arm lost on the common hit (`R21DispatchTypecheck`
        // instanceof-subclass 378 vs 261 ns) and won only on receivers with
        // seven to nine interfaces (instanceof-wide-iface 1080 vs 1755). It
        // now runs after the seen ways, for `instanceof` against an interface
        // only, unless `CRATONVM_JIT_TYPECHECK_ROWS_ALL` admits class targets
        // and `checkcast` too.
        let admitted = (display.interface && op == "instanceof")
            || crate::typecheck_rows::typecheck_rows_all_enabled();
        let base = if admitted {
            self.typecheck_rows_base()
        } else {
            None
        };
        let arm = base.and_then(|rows| {
            let latch = miss_ok.then(|| {
                if typecheck_site_is_boot_package(site_name) {
                    0
                } else {
                    cratonvm_types::duplicate_class_name_seen_addr()
                }
            });
            typecheck_row_bytes(rows, cid, kind, latch)
        });
        if crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] ir {op} row arm: target={cid} interface={} depth={} {}",
                display.interface,
                display.depth,
                match (&arm, miss_ok) {
                    (Some(_), true) => "emitted hit+miss",
                    (Some(_), false) => "emitted hit",
                    (None, _) => "DECLINED",
                }
            );
        }
        let Some(arm) = arm else {
            return false;
        };
        self.note_rcx_written();
        let at = self.buf.pos();
        self.buf.emit(&arm.bytes);
        one.extend(arm.hit.into_iter().map(|e| at + e));
        zero.extend(arm.miss.into_iter().map(|e| at + e));
        true
    }

    /// JIT round 21, lane tcperf (`CRATONVM_JIT_TYPECHECK_ROW_FIRST`, class
    /// targets; `CRATONVM_JIT_TYPECHECK_ROW_FIRST_IFACE`, interfaces): the
    /// status-first row arm ([`crate::typecheck_rows::typecheck_row_first_bytes`])
    /// as the FIRST test of a site, with RAX = a non-null PLAIN object, ahead
    /// of the exact-id compare, the seen ways and the display arm (all of which
    /// still follow, for what the arm does not answer). Same edge contract as
    /// [`Self::emit_typecheck_row_arm`]; returns whether it was emitted, and the
    /// caller then skips that later arm. `CRATONVM_DBG_JITC` names the site.
    pub(in crate::ir_lower) fn emit_typecheck_row_first_arm(
        &mut self,
        display: VhSubtypeDisplay,
        cid: u32,
        miss_ok: bool,
        op: &str,
        site_name: Option<&str>,
        one: &mut Vec<usize>,
        zero: &mut Vec<usize>,
    ) -> bool {
        use crate::typecheck_rows::{typecheck_row_first_bytes, RowTarget};
        let (kind, enabled) = if display.interface {
            (
                RowTarget::Interface,
                crate::typecheck_rows::typecheck_row_first_iface_enabled(),
            )
        } else {
            (
                RowTarget::Class {
                    depth: display.depth,
                },
                crate::typecheck_rows::typecheck_row_first_enabled(),
            )
        };
        if !enabled {
            return false;
        }
        let arm = self.typecheck_rows_base().and_then(|rows| {
            let latch = miss_ok.then(|| {
                if typecheck_site_is_boot_package(site_name) {
                    0
                } else {
                    cratonvm_types::duplicate_class_name_seen_addr()
                }
            });
            typecheck_row_first_bytes(rows, cid, kind, latch)
        });
        if crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] ir {op} row-first arm: target={cid} interface={} depth={} {}",
                display.interface,
                display.depth,
                match (&arm, miss_ok) {
                    (Some(_), true) => "emitted hit+miss",
                    (Some(_), false) => "emitted hit",
                    (None, _) => "DECLINED",
                }
            );
        }
        let Some(arm) = arm else {
            return false;
        };
        self.note_rcx_written();
        let at = self.buf.pos();
        self.buf.emit(&arm.bytes);
        one.extend(arm.hit.into_iter().map(|e| at + e));
        zero.extend(arm.miss.into_iter().map(|e| at + e));
        true
    }

    /// JIT round 21, lane tcfuse (TCPERF21-3,
    /// `CRATONVM_JIT_TYPECHECK_SHARED_CLASS_WORD`): the status-first row arm
    /// started from the check's SHARED class word (its fourth input, an
    /// `Op::ClassWordOf` of its own operand `obj` that
    /// `ir_optimize::typecheck_class_word` planted) instead of from the
    /// receiver's header, emitted BEFORE the site's null and kind screens:
    ///
    /// ```text
    ///   MOV RCX, word ; TEST ECX, ECX ; JZ fall      ; null or array: 0
    ///   (typecheck_row_first_bytes without its MOV ECX, [RAX])
    /// fall: the site's screens and arms, unchanged
    /// ```
    ///
    /// A non-zero word is exactly the arm's precondition (RAX non-null and a
    /// plain object: `class_word_of_bytes` answers `0` for a null or an
    /// array, and both are fixed for the object's life); RAX must already
    /// hold `obj` (the caller's hits may read it). Same edge contract as
    /// [`Self::emit_typecheck_row_first_arm`], whose switches it also needs;
    /// returns whether the arm was emitted (the caller then emits no other
    /// row arm). `CRATONVM_DBG_JITC` names the site.
    #[allow(clippy::too_many_arguments)]
    pub(in crate::ir_lower) fn emit_typecheck_row_first_arm_from_word(
        &mut self,
        id: NodeId,
        obj: NodeId,
        display: VhSubtypeDisplay,
        cid: u32,
        miss_ok: bool,
        op: &str,
        site_name: Option<&str>,
        one: &mut Vec<usize>,
        zero: &mut Vec<usize>,
    ) -> bool {
        use crate::typecheck_rows::{typecheck_row_first_from_word_bytes, RowTarget};
        if !crate::typecheck_rows::typecheck_shared_class_word_enabled() {
            return false;
        }
        let graph = self.graph;
        let Some(&word) = graph.nodes.get(id as usize).and_then(|n| n.inputs.get(3)) else {
            return false;
        };
        let shared = graph.nodes.get(word as usize).is_some_and(|w| {
            matches!(w.op, Op::ClassWordOf) && w.inputs.first().copied() == Some(obj)
        });
        if !shared {
            return false;
        }
        let (kind, enabled) = if display.interface {
            (
                RowTarget::Interface,
                crate::typecheck_rows::typecheck_row_first_iface_enabled(),
            )
        } else {
            (
                RowTarget::Class {
                    depth: display.depth,
                },
                crate::typecheck_rows::typecheck_row_first_enabled(),
            )
        };
        if !enabled {
            return false;
        }
        let arm = self.typecheck_rows_base().and_then(|rows| {
            let latch = miss_ok.then(|| {
                if typecheck_site_is_boot_package(site_name) {
                    0
                } else {
                    cratonvm_types::duplicate_class_name_seen_addr()
                }
            });
            // The header arm without its `MOV ECX, [RAX]`: the word replaces it.
            typecheck_row_first_from_word_bytes(rows, cid, kind, latch)
        });
        if crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] ir {op} row-first arm from shared class word n{word}: target={cid} {}",
                match (&arm, miss_ok) {
                    (Some(_), true) => "emitted hit+miss",
                    (Some(_), false) => "emitted hit",
                    (None, _) => "DECLINED",
                }
            );
        }
        let Some(arm) = arm else {
            return false;
        };
        self.note_rcx_written();
        self.gp_load_value(RCX, word);
        self.buf.emit(&[0x85, 0xC9]); // TEST ECX, ECX
        let not_plain = self.emit_jcc_rel32(0x84); // JZ -> the site's screens
        let at = self.buf.pos();
        self.buf.emit(&arm.bytes);
        one.extend(arm.hit.into_iter().map(|e| at + e));
        zero.extend(arm.miss.into_iter().map(|e| at + e));
        self.patch_rel32_to_here(not_plain);
        true
    }

    /// JIT round 21, lane tcfuse (`CRATONVM_JIT_TYPECHECK_BRANCH_FUSION`,
    /// TCPERF21-2 = DISPATCH21-3): may the `Op::InstanceOf` `id` send its
    /// inline `1` / `0` answers straight to the edges of the `If` that ends
    /// its block? `Some((cond, one_is_true))` names that `If`'s compare and
    /// whether a `1` takes its true edge.
    ///
    /// The jumps skip, on the inline paths only, everything emitted between
    /// the arm's answer and the `If`'s branch: the arm's own materialisation
    /// and home store, the lowering of every node scheduled after it in the
    /// block, the terminator's back-edge poll, and the fused compare. So
    /// each of those must be nothing the successors need:
    ///
    /// * the instanceof's value has ONE value use (the compare; memory-token
    ///   edges to the next memory op read no answer) and no frame state
    ///   names it -- or (2026-10-26, under `CRATONVM_JIT_IR_ELIDE_UNNAMED_HOMES`)
    ///   only frame states no reachable deopt reads, in no local or monitor
    ///   and not refused by an earlier attempt: round 25 item 1a's rule for a
    ///   fused compare, and the answer is then marked `fused_cmp_unnamed`
    ///   (the third field of the answer), so describing it anywhere fails the
    ///   attempt closed and it is lowered again with every home kept. The
    ///   answer sits on the operand stack in the `ifeq`'s own frame state,
    ///   which cannot trap; with the old rule no `if (o instanceof T)` was
    ///   ever fused. The compare is FUSED into the `If` (so its own arm emits
    ///   nothing), has one use, is `Cmp(Eq|Ne)` against `Const(0)`, and is
    ///   the ONLY node scheduled after the instanceof in the block, with no
    ///   planned dead-home clear;
    /// * no successor is a back edge (the poll before the branch runs only
    ///   when one is; a loop whose latch is this `If` keeps the old shape);
    /// * exactly two distinct successors (the arms that record the edge
    ///   entries);
    /// * no state that models registers across emission order: no split
    ///   residency plan (`ls_at` transitions at the compare's position), no
    ///   caller-saved residency (its reloads after the instanceof would be
    ///   skipped; t1 w9 TCFUSE), no MIR tiles, no reference residency cache
    ///   (default off), and no owed volatile-store fence (t1 w9 TCFUSE: a
    ///   fence settled at the compare would be skipped). Every other model
    ///   the `If`'s edges read (phi copies, the edge trampolines) is
    ///   re-entered at the edge's first byte, as the branch enters it. Also
    ///   (t1 w9 TCFUSE, conservative): no planned dead-home clear at the
    ///   instanceof itself and no lazy-reference hook at the compare.
    ///
    /// The helper path is unchanged: it stores the answer and falls into the
    /// `If`, which tests it as before. `CRATONVM_DBG_JITC` names the site.
    pub(in crate::ir_lower) fn typecheck_branch_fusion_for(
        &self,
        id: NodeId,
    ) -> Result<(NodeId, bool, bool), &'static str> {
        if !crate::typecheck_rows::typecheck_branch_fusion_enabled()
            || !self.fused_branch_on
            || self.mir_mode != MirMode::Off
            || self.ref_residency_on
            || self.ls_caller_saved_on
            || self.fence_pending
            || !self.ls_block_start.is_empty()
            || self.typecheck_branch_pending.is_some()
        {
            return Err("off, or a MIR mode / residency / split plan / a pending fusion");
        }
        if self.dead_home_clears.contains_key(&id) {
            return Err("the instanceof has a planned dead-home clear");
        }
        let i = id as usize;
        // VALUE uses: an `Op::InstanceOf` is also on the memory chain (the next
        // check's token input, the loop's memory phi), and a token edge reads no
        // answer. `use_count` counts both, so with it no site ever fused.
        let value_uses = self
            .graph
            .nodes
            .iter()
            .map(|n| {
                n.inputs
                    .iter()
                    .enumerate()
                    .filter(|&(slot, &x)| x == id && !crate::ir::is_memory_token_slot(n, slot))
                    .count()
            })
            .sum::<usize>();
        if value_uses != 1 {
            return Err("the answer has another use");
        }
        // Round 25 item 1a's rule (the compare fusion's `unreached`); the
        // back-edge half of it is the forward-edge test below.
        let unnamed = self.deopt_named.get(i).copied().unwrap_or(true);
        if unnamed
            && !(self.elide_unnamed_homes_on
                && !self.elide_reach.get(i).copied().unwrap_or(true)
                && !self.elide_held.get(i).copied().unwrap_or(true)
                && !self.elide_excluded.get(i).copied().unwrap_or(true))
        {
            return Err("a frame state a reachable deopt reads names the answer");
        }
        const UNSCHEDULED: &str = "not in the schedule";
        let block_idx = *self.schedule.node_to_block.get(id as usize).ok_or(UNSCHEDULED)?;
        let block = self.schedule.blocks.get(block_idx).ok_or(UNSCHEDULED)?;
        let pos = block.nodes.iter().position(|&n| n == id).ok_or(UNSCHEDULED)?;
        if block.nodes.len() != pos + 2 {
            return Err("the compare is not the only node after it in the block");
        }
        let cond = block.nodes[pos + 1];
        let term = block.terminator.ok_or("the block has no terminator")?;
        let if_node = self.graph.nodes.get(term as usize).ok_or(UNSCHEDULED)?;
        if !matches!(if_node.op, Op::If) || if_node.inputs.get(1).copied() != Some(cond) {
            return Err("the terminator is not an If on the compare");
        }
        if !self.fused_cmp.get(cond as usize).copied().unwrap_or(false)
            || self.use_count.get(cond as usize).copied() != Some(1)
            || self.dead_home_clears.contains_key(&cond)
            || self.lazy_ref_sites.contains_key(&cond)
        {
            return Err("the compare is not fused, has another use, a clear or a lazy hook");
        }
        const NOT_A_TEST: &str = "the compare is not Eq / Ne against 0";
        let cmp = self.graph.nodes.get(cond as usize).ok_or(UNSCHEDULED)?;
        let one_is_true = match cmp.op {
            Op::Cmp(CmpOp::Ne) => true,
            Op::Cmp(CmpOp::Eq) => false,
            _ => return Err(NOT_A_TEST),
        };
        let (a, b) = (
            *cmp.inputs.first().ok_or(NOT_A_TEST)?,
            *cmp.inputs.get(1).ok_or(NOT_A_TEST)?,
        );
        let is_zero = |n: NodeId| {
            matches!(
                self.graph.nodes.get(n as usize).map(|x| &x.op),
                Some(Op::Const(0))
            )
        };
        if !((a == id && is_zero(b)) || (b == id && is_zero(a))) {
            return Err(NOT_A_TEST);
        }
        // Two reasons, told apart for the tcf21 census of TCFUSE21-2 (a loop
        // latch's `If`): `CRATONVM_DBG_JITC=1 ... 2>&1 | grep 'branch fusion'
        // | sed 's/.*declined -- //' | sort | uniq -c`.
        if block.successors.len() != 2 || block.successors[0] == block.successors[1] {
            return Err("the If has not two distinct successors");
        }
        if block.successors.iter().any(|&s| s <= block_idx) {
            return Err("a successor is a back edge (the loop latch's poll precedes the branch)");
        }
        Ok((cond, one_is_true, unnamed))
    }

    /// JIT round 21, lane tcmiss (`CRATONVM_JIT_AASTORE_SUBTYPE_ARM`,
    /// TCARRAY21-2): the `aastore` element check's subclass arm
    /// ([`crate::typecheck_rows::aastore_subtype_bytes`]), emitted after the
    /// exact-component compare with RAX = the non-null PLAIN value, EDX = its
    /// class id and R11 = the array (whose component id is neither `0` nor
    /// EDX). Appends its pass edges to `pass` (the store); everything else
    /// falls through to the helper. Writes RAX, RCX, RDX, R11 and the flags.
    /// `CRATONVM_DBG_JITC` names the site.
    pub(in crate::ir_lower) fn emit_aastore_subtype_arm(&mut self, pass: &mut Vec<usize>) {
        if !crate::typecheck_rows::aastore_subtype_arm_enabled() {
            return;
        }
        let arm = self
            .typecheck_rows_base()
            .and_then(crate::typecheck_rows::aastore_subtype_bytes);
        if crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] ir aastore subtype arm: bci={} {}",
                self.cur_bci,
                if arm.is_some() {
                    "emitted"
                } else {
                    "DECLINED (no type-check rows)"
                }
            );
        }
        let Some((bytes, edges)) = arm else {
            return;
        };
        self.note_rcx_written();
        let at = self.buf.pos();
        self.buf.emit(&bytes);
        pass.extend(edges.into_iter().map(|e| at + e));
    }

    /// The inline thin-lock fast path of an `Op::MonitorEnter` (`enter`) /
    /// `Op::MonitorExit` whose receiver's home is `obj_slot`: the single-pass
    /// tier's sequence, through the shared emitter
    /// `crate::runtime_lowering::emit_inline_thin_lock`. Returns the `JMP done`
    /// site the caller patches after its helper sequence, or `None` having
    /// emitted nothing.
    ///
    /// The thread comes from the `JIT_THREAD` mirror (one segment load), NOT
    /// from `shadow_thread_slot_off`, whose prologue fetch is erased in a
    /// method that publishes nothing; the frame slot is the fallback only on a
    /// target with no mirror, where a zero (erased) slot takes the helper.
    ///
    /// Census: `CRATONVM_DBG_JITC` prints one `[cratonvm-jitc] ir thin-lock`
    /// line per site, emitted or declined with the reason, so a run can tell
    /// "the inline path is in the code" from "it never compiled". Under the
    /// same flag an emitted site also bumps the thread's inline-hit / helper-fall
    /// counters (`[monitor-inline census]`), as the single-pass copy does.
    pub(in crate::ir_lower) fn emit_monitor_fast_path(&mut self, enter: bool, obj_slot: i32) -> Option<usize> {
        use crate::runtime_lowering::ThinLockThread;
        let op = if enter { "monitorenter" } else { "monitorexit" };
        let outcome = if !crate::runtime_lowering::inline_thin_lock_enabled() {
            Err("CRATONVM_MONITOR_FASTPATH=0")
        } else {
            let tls = crate::x64::jit_thread_tls_disp();
            let thread = if tls != 0 {
                ThinLockThread::Tls(u32::try_from(tls).unwrap_or(0))
            } else if self.get_current_thread != 0 && self.shadow_thread_slot_off > 0 {
                ThinLockThread::Frame(self.shadow_thread_slot_off)
            } else {
                ThinLockThread::Tls(0)
            };
            crate::runtime_lowering::emit_inline_thin_lock(
                &mut self.buf,
                enter,
                obj_slot,
                self.monitor_block_off,
                thread,
                crate::x64::dbg_jitc_enabled(),
            )
        };
        if crate::x64::dbg_jitc_enabled() {
            match outcome {
                Ok(_) => eprintln!(
                    "[cratonvm-jitc] ir thin-lock {op} bci={} emitted",
                    self.cur_bci
                ),
                Err(why) => eprintln!(
                    "[cratonvm-jitc] ir thin-lock {op} bci={} declined: {why}",
                    self.cur_bci
                ),
            }
        }
        outcome.ok()
    }

    /// Put THIS frame's identity back after a shared `runtime_lowering` stub.
    ///
    /// Round 11 (irlower). `runtime_lowering::emit_post_call_frame_republish`
    /// -- the tail of the monitor, allocation and constant-pool stubs -- is
    /// shared code that does not know which method it was emitted into, so on
    /// the inline-TLS shape it restores RBP and writes `0` as the compile id.
    /// This tier then left that `0` standing until its next own republish, and
    /// every GC point in between (the next allocation's helper, a back-edge
    /// poll, another monitor) found no published identity: the collector fell
    /// back to decoding the call that entered this frame, which fails for every
    /// frame entered through `CALL R11` (each inline-cache hit), and the moving
    /// young collection was refused. The identity store writes no register and
    /// no flag. Same condition as the stub's own zeroing: a frame record and an
    /// inline RBP displacement (`emit_frame_record_identity` adds the id half).
    pub(in crate::ir_lower) fn emit_identity_after_shared_stub(&mut self) {
        if self.frame_record != 0 && crate::x64::inline_rbp_tls_disp() != 0 {
            self.emit_frame_record_identity();
        }
    }

    /// [`Self::emit_post_call_frame_record`] for a SELF-RECURSIVE direct call
    /// (`emit_self_recursive_call`): on the inline-TLS shape, the RBP store
    /// alone. Answers whether it elided the identity half.
    ///
    /// Round 9 wave 3 (invoke2 X1), the IR twin of the single-pass N1 edit in
    /// `x64/op_invoke.rs`. The callee is this buffer's own entry
    /// (`patch_self_calls`), whose prologue publishes THIS compile's id, and
    /// every writer inside it restores that same id before a normal return
    /// (its own post-call republishes, the Rust dispatch brackets). One
    /// segment-relative `MOV dword gs:[disp32], imm32` per recursive call.
    /// The helper-call shape (no inline displacement) is unchanged: the helper
    /// writes both halves in one call.
    pub(in crate::ir_lower) fn emit_post_self_call_frame_record(&mut self) -> bool {
        if !crate::x64::precise_jit_maps_enabled() || self.frame_record == 0 {
            return false;
        }
        let disp = crate::x64::inline_rbp_tls_disp();
        if disp == 0 || self.inline_cm_tls_disp == 0 {
            self.emit_post_call_frame_record();
            return false;
        }
        self.emit_mov_tls_disp32_rbp(disp as u32);
        true
    }

    /// `MOV <arg0>, RBP` — for the helper form of the frame record, which is
    /// reached only when no inline TLS displacement could be probed.
    pub(in crate::ir_lower) fn emit_mov_arg0_rbp(&mut self) {
        const RBP_REG: u8 = 5;
        let dest = CALL_ARG_REGS[0];
        // REX.W (+ REX.B when the destination is R8..R15), MOV r/m64, r64.
        self.buf
            .emit_byte(0x48 | if dest >= 8 { 0x01 } else { 0x00 });
        self.buf.emit_byte(0x89);
        self.buf.emit_byte(0xC0 | (RBP_REG << 3) | (dest & 7));
    }

    /// `MOV <seg>:[disp32], RBP` — the 9-byte inline frame-record store.
    ///
    /// Byte-identical to the single-pass backend's `emit_mov_tls_disp32_rbp`;
    /// both must write the SAME slot, since `inline_rbp_tls_disp()` is the one
    /// source of truth the VM-side mirror accessor reads back.
    /// `MOV RAX, gs:[disp32]` (`fs:` on Linux): the one-instruction thread
    /// fetch through the `JIT_THREAD` mirror (`x64::jit_thread_tls_disp`).
    pub(in crate::ir_lower) fn emit_mov_rax_tls_disp32(&mut self, disp32: u32) {
        self.buf
            .emit_byte(crate::x64::inline_rbp_tls_segment_prefix());
        self.buf.emit_byte(0x48); // REX.W
        self.buf.emit_byte(0x8B); // MOV r64, r/m64
        self.buf.emit_byte(0x04); // ModRM: reg=RAX, r/m=SIB
        self.buf.emit_byte(0x25); // SIB: [disp32] absolute
        self.buf.emit(&disp32.to_le_bytes());
    }
    pub(in crate::ir_lower) fn emit_mov_tls_disp32_rbp(&mut self, disp32: u32) {
        self.buf
            .emit_byte(crate::x64::inline_rbp_tls_segment_prefix());
        self.buf.emit_byte(0x48); // REX.W
        self.buf.emit_byte(0x89); // MOV r/m64, r64
        self.buf.emit_byte(0x2C); // ModRM: reg=RBP, r/m=SIB
        self.buf.emit_byte(0x25); // SIB: [disp32] absolute
        self.buf.emit(&disp32.to_le_bytes());
    }

    /// `MOV dword <seg>:[disp32], imm32` — the identity half of the frame
    /// record, emitted immediately after every RBP store so the two always
    /// describe the same frame.
    ///
    /// 32-bit and immediate, so it needs no scratch register: the post-call
    /// republish path runs with the callee's return value live in RAX. Byte
    /// layout matches the single-pass `emit_mov_tls_disp32_imm32`.
    pub(in crate::ir_lower) fn emit_frame_record_identity(&mut self) {
        if self.inline_cm_tls_disp == 0 {
            return;
        }
        self.buf
            .emit_byte(crate::x64::inline_rbp_tls_segment_prefix());
        self.buf.emit_byte(0xC7); // MOV r/m32, imm32
        self.buf.emit_byte(0x04); // ModRM: /0, r/m=SIB
        self.buf.emit_byte(0x25); // SIB: [disp32] absolute
        self.buf
            .emit(&(self.inline_cm_tls_disp as u32).to_le_bytes());
        self.buf.emit(&self.compile_id.id().to_le_bytes());
    }

    /// JIT r20 wave 11 (lane pAlloc; book `jit-r19-concat5-proposals.md`,
    /// "Review notes"; `CRATONVM_JIT_IR_INLINE_TLAB_TLS_THREAD`, default ON,
    /// `=0` restores the frame slot): the `thread_tls_disp` of an inline TLAB
    /// plan. A body that publishes no root has its prologue thread fetch erased
    /// ([`Self::finish_lazy_thread_fetch`], or never emitted,
    /// [`Self::fetch_current_thread`]), so `shadow_thread_slot_off` keeps the
    /// prologue's zero and every bump that reads it takes its stub -- the
    /// inline path never ran in exactly the lean allocating bodies it was built
    /// for. The `JIT_THREAD` mirror is what the fetch itself reads and what the
    /// IR thin lock already reads ([`Self::emit_monitor_fast_path`]), so the
    /// thread is the same pointer; only its availability changes. Gated on
    /// the same condition the frame path needs (a thread helper and a slot),
    /// so a configuration with neither still declines the same way.
    pub(in crate::ir_lower) fn ir_tlab_thread_tls_disp(&mut self) -> u32 {
        let tls = if self.get_current_thread != 0
            && (self.shadow_thread_slot_off > 0 || crate::x64::jit_thread_tls_disp() != 0)
        {
            u32::try_from(crate::x64::jit_thread_tls_disp()).unwrap_or(0)
        } else {
            0
        };
        if tls != 0 {
            self.tlab_tls_thread_plans = self.tlab_tls_thread_plans.saturating_add(1);
        } else {
            self.tlab_frame_thread_plans = self.tlab_frame_thread_plans.saturating_add(1);
        }
        tls
    }

    /// The compile's `Class.method(desc)` for a `CRATONVM_DBG_JITC` line
    /// (`Graph::dbg_label`, set by the IR door only while that flag is on),
    /// or `<unnamed>` for a graph nobody labelled (unit tests).
    pub(in crate::ir_lower) fn dbg_method(&self) -> &str {
        if self.graph.dbg_label.is_empty() {
            "<unnamed>"
        } else {
            &self.graph.dbg_label
        }
    }

    /// Round rx27 wave 1 (lane alloc): the `Op::NewArray` twin of the
    /// `ir new-site` census line, one per compiled array site under
    /// `CRATONVM_DBG_JITC=1`, naming the route, why the site calls Rust per
    /// allocation (`call=`, [`newarray_site_call_reason`]) and the compile.
    /// The Generational / G1 sunk route prints its own `ir newarray map sink`
    /// line instead. Diagnostic only.
    ///
    /// `emitted`: the bump was emitted (`route` is then `zgc-split` or
    /// `inline`, else `stub-only`).
    #[allow(clippy::too_many_arguments)]
    pub(in crate::ir_lower) fn note_newarray_site_census(
        &self,
        id: NodeId,
        element_type: u8,
        component_class_id: u32,
        offered: bool,
        registration: bool,
        emitted: bool,
        route: &str,
    ) {
        if !crate::x64::dbg_jitc_enabled() {
            return;
        }
        let call = newarray_site_call_reason(NewArraySiteFacts {
            offered,
            reference: element_type == 0,
            registration,
            anewarray_any_gc: crate::runtime_lowering::ir_inline_anewarray_any_gc_enabled(),
            bump_emitted: emitted,
            inline_start_bit: crate::x64::ir_zgc_inline_announce_enabled(),
        });
        eprintln!(
            "[cratonvm-jitc] ir newarray-site n{id} atype={element_type} \
             component={component_class_id} route={route} call={call} method={}",
            self.dbg_method(),
        );
    }

    /// The engagement control of [`Self::ir_tlab_thread_tls_disp`], called
    /// for a body whose thread fetch is erased (or was never emitted): under
    /// `CRATONVM_DBG_JITC` one `[cratonvm-jitc] ir lean body` line naming how
    /// many of its inline TLAB plans read the mirror (`thread=tls`, the bump
    /// can engage) and how many read the zeroed frame slot (`thread=frame`,
    /// the stub on every execution). `=0` of the switch prints `tls 0`.
    pub(in crate::ir_lower) fn note_lean_body_tlab_thread_census(&self) {
        if (self.tlab_tls_thread_plans != 0 || self.tlab_frame_thread_plans != 0)
            && crate::x64::dbg_jitc_enabled()
        {
            eprintln!(
                "[cratonvm-jitc] ir lean body: thread fetch erased; inline-TLAB plans thread=tls {} thread=frame {} (frame = stub on every execution)",
                self.tlab_tls_thread_plans, self.tlab_frame_thread_plans
            );
        }
    }

    /// Cache `*mut JvmThread` in its reserved slot.
    ///
    /// Emitted AFTER the ABI parameter stores: the helper clobbers the
    /// caller-saved registers the parameters arrive in, and by this point they
    /// are already in frame slots. A zero helper address (the JIT unit tests'
    /// stub table) leaves the slot untouched, and every shadow site is gated on
    /// the same condition, so such a method is consistently untracked rather
    /// than dereferencing a pointer that was never fetched.
    pub(in crate::ir_lower) fn fetch_current_thread(&mut self) {
        if self.get_current_thread == 0 || self.shadow_thread_slot_off <= 0 || !self.reloc_emit_on {
            return;
        }
        // Round 11 wave 13 (lane fib): a body that can never publish a root
        // does not emit the span `finish_lazy_thread_fetch` would jump over.
        if self.body_publishes_no_roots() {
            return;
        }
        let start = self.buf.pos();
        // 2026-09-02: one `mov rax, gs:[disp]` through the `JIT_THREAD`
        // mirror where the VM publishes it; the helper call otherwise. Both
        // stay inside the erasable span.
        let tls_disp = crate::x64::jit_thread_tls_disp();
        if tls_disp != 0 {
            self.emit_mov_rax_tls_disp32(tls_disp as u32);
        } else {
            self.emit_mov_reg_imm64(RAX, self.get_current_thread as u64);
            self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
        }
        self.store_rax(self.shadow_thread_slot_off);
        // Capture the entry watermark, inside the erasable span: a method that
        // publishes nothing has no push to unwind, so the capture goes away
        // with the fetch and the (zeroed) thread slot makes every exit's
        // restore skip through its null guard.
        if self.shadow_savetop_slot_off > 0 {
            let ss_top = self.shadow_off_in_thread;
            // Round 16 wave 17 (lane frames2, FR16P-3,
            // `CRATONVM_JIT_IR_PROLOGUE_THREAD_IN_RAX`, default OFF): RAX
            // still holds the word `store_rax` just wrote -- the TLS load or
            // the helper's return value -- since no current definition is
            // published from the prologue (`publish_def_at_store` and the
            // carry only act on `cur_def`), so copy it rather than reload it.
            if ir_prologue_thread_in_rax_enabled() && self.cur_def.is_none() {
                self.buf.emit(&[0x49, 0x89, 0xC2]); // MOV R10, RAX
            } else {
                // MOV R10, [rbp - thread_slot]
                self.buf.emit(&[0x4C, 0x8B, 0x95]);
                self.buf.emit(&(-self.shadow_thread_slot_off).to_le_bytes());
            }
            // TEST R10, R10 ; JE skip
            self.buf.emit(&[0x4D, 0x85, 0xD2]);
            self.buf.emit(&[0x0F, 0x84]);
            let skip = self.buf.pos();
            self.buf.emit(&[0, 0, 0, 0]);
            // MOV R11, [R10 + ss_top] ; MOV [rbp - savetop], R11
            self.buf.emit(&[0x4D, 0x8B, 0x9A]);
            self.buf.emit(&ss_top.to_le_bytes());
            self.buf.emit(&[0x4C, 0x89, 0x9D]);
            self.buf
                .emit(&(-self.shadow_savetop_slot_off).to_le_bytes());
            self.patch_rel32_to_here(skip);
        }
        self.thread_fetch_span = Some((start, self.buf.pos()));
    }

    /// Lazy prologue: erase the thread fetch when nothing published.
    ///
    /// The fetch is a `CALL` on the ENTRY path of every IR method, and most
    /// methods publish nothing — they hold no live reference across a call, or
    /// make no call at all. Paying a helper call per invocation for a slot that
    /// is then never read is what made the first publication attempt regress
    /// `ZonedDateTimeTest` 302 s -> >1200 s while leaving shallow-stack
    /// `ASTParserLoadingTest` untouched: the cost scales with how often methods
    /// are ENTERED, not with stack depth.
    ///
    /// The single-pass backend solves this the same way (`shadow_pushed_any`).
    /// Erasing in place rather than shifting the body keeps every
    /// already-recorded offset — branch patches, deopt points, oop-map native
    /// pcs — valid, which is why this is an erase and not a removal.
    ///
    /// It erases with a JUMP over the span, not with a run of `0x90`. This
    /// half was missing here while the single-pass backend had it, and the
    /// asymmetry cost real time: the span is ~46 bytes, so every IR method
    /// that published nothing retired 46 NOPs on entry, on every invocation.
    /// `CratonBench fib` — a two-line static method entered 2.27e9 times —
    /// paid it 2.27e9 times and ran ~2x slower on the IR tier than on the
    /// single-pass body it displaced. Both backends now share
    /// [`ExecutableBuffer::erase_range_with_jump_over`] so they cannot drift
    /// apart again.
    pub(in crate::ir_lower) fn finish_lazy_thread_fetch(&mut self) {
        debug_assert!(
            !(self.shadow_pushed_any && self.body_publishes_no_roots()),
            "a body predicted to publish no root pushed one; its prologue has no thread fetch"
        );
        if self.shadow_pushed_any {
            return;
        }
        self.note_lean_body_tlab_thread_census();
        if let Some((start, end)) = self.thread_fetch_span.take() {
            // Jumping over the span is only safe while nothing branches INTO
            // its interior: a site patched at `start + 1` would overwrite the
            // rel8 displacement, and one that TARGETS `start + 1` would decode
            // the displacement as an opcode. Under the old NOP fill neither
            // was fatal, so the invariant was never stated — it holds because
            // the span is emitted at the top of the prologue, before any block
            // is lowered, so every recorded patch site is past `end`.
            //
            // This runs before `patch_branches` / `patch_self_calls`, so both
            // lists are still intact here and the invariant is checkable
            // rather than merely true.
            debug_assert!(
                self.branch_patches
                    .iter()
                    .all(|&(pos, _)| pos < start || pos >= end)
                    && self
                        .self_call_patches
                        .iter()
                        .all(|&pos| pos < start || pos >= end),
                "a patch site landed inside the erased thread-fetch span {start}..{end}; \
                 jumping over it would corrupt that patch (or its target)"
            );
            self.buf.erase_range_with_jump_over(start, end);
        }
        // Round 11 wave 11 (lane `calls`, `CratonBench fib`): every exit's
        // shadow-`top` restore, on the same condition. With the fetch erased
        // the thread slot keeps the prologue's zero for the whole activation --
        // the fetch was its only writer -- so each restore's `TEST R10, R10;
        // JE` always skipped, after a load: three instructions on every return
        // of every method that publishes nothing (a leaf, or `fib`, which has
        // no reference at all). One taken `JMP` now, the path those
        // instructions always took. Same interior-branch invariant as the fetch
        // span: the restore's only branch is its own `JE`, already patched to
        // the span's end, and no recorded patch site lies inside it.
        let restores = std::mem::take(&mut self.savetop_restore_spans);
        for (start, end) in restores {
            debug_assert!(
                self.branch_patches
                    .iter()
                    .all(|&(pos, _)| pos < start || pos >= end)
                    && self
                        .self_call_patches
                        .iter()
                        .all(|&pos| pos < start || pos >= end),
                "a patch site landed inside an erased shadow-top restore {start}..{end}"
            );
            self.buf.erase_range_with_jump_over(start, end);
        }
    }

    /// Publish every live oop of this safepoint onto the thread's shadow stack,
    /// so a relocating collector can rewrite each one in place.
    ///
    /// This is the half that `moving_young_coverage_complete` actually asserts.
    /// The oop map names the slots; only this makes them *rewritable*, because
    /// `scan_oop_slots` yields `ObjectRef` values (marking roots) while
    /// `published_shadow_values` yields the homes the collector writes back to.
    ///
    /// Registers: R10 (thread), R11 (running top), RCX (value temp). All
    /// caller-saved and free here — this runs at the top of the `Op::Call` arm,
    /// before any argument staging or ABI register loading.
    ///
    /// Returns whether the push was actually emitted.
    ///
    /// gci i9/ng (i8/nt review item 6): gated on
    /// [`crate::x64::shadow_stack_maps_enabled`], like the frame block
    /// (`plan_frame_block`) and the single-pass backend (whose shadow slots are
    /// not even reserved without it). With shadow emission off
    /// (`CRATONVM_JIT_MY_SHADOW_EMISSION=0`) the VM never allocates the
    /// thread's shadow stack (`top == end == 0`), so every push this tier
    /// emitted took the overflow bail: it stored nothing, but it bumped
    /// `SHADOW_OVERFLOW_COUNT` and so read as `shadow-stack-push-bailed` (47 of
    /// 358 pauses on the vi5g-g1r SH0 rows) and fed the shadow-bail refusal. Not
    /// emitting it makes the map's claim honest instead
    /// (`moving_young_coverage_complete` needs `published`, so the frame now
    /// refuses as `MAP_NOT_COMPLETE`, where the bail refused before). The
    /// default (shadow emission on) is byte-identical.
    pub(in crate::ir_lower) fn emit_shadow_push(&mut self, offsets: &[i16]) -> bool {
        if !Self::shadow_push_emits(
            self.get_current_thread,
            self.shadow_thread_slot_off,
            self.shadow_savebase_slot_off,
            offsets.len(),
            crate::x64::shadow_stack_maps_enabled,
        ) {
            return false;
        }
        let ss_top = self.shadow_off_in_thread;
        // MOV R10, [rbp - thread_slot]
        self.buf.emit(&[0x4C, 0x8B, 0x95]);
        self.buf.emit(&(-self.shadow_thread_slot_off).to_le_bytes());
        // TEST R10, R10 ; JE skip  — a null thread means "untracked", exactly
        // as in the single-pass backend; the reload's guard is symmetric.
        self.buf.emit(&[0x4D, 0x85, 0xD2]);
        self.buf.emit(&[0x0F, 0x84]);
        let skip = self.buf.pos();
        self.buf.emit(&[0, 0, 0, 0]);
        // MOV R11, [R10 + ss_top]      (current top)
        self.buf.emit(&[0x4D, 0x8B, 0x9A]);
        self.buf.emit(&ss_top.to_le_bytes());
        // OVERFLOW GUARD (`ShadowStack::END_OFFSET`) — see the matching comment
        // in the single-pass backend. Without it a push that runs past the end
        // of the 2 MiB buffer keeps storing into the allocator arena behind it.
        // LEA leaves flags alone, so the bump is undone between the CMP and the
        // branch and R11 stays the only scratch. `top == end` is the legal
        // exactly-full state, so the test is strictly-above.
        let need = (offsets.len() as i32) * 8; // Cast: x86-64 disp32
        let overflow = if crate::shadow_end_guard_enabled() {
            // LEA R11,[R11+need] ; CMP R11,[R10+end] ; LEA R11,[R11-need]
            self.buf.emit(&[0x4D, 0x8D, 0x9B]);
            self.buf.emit(&need.to_le_bytes());
            self.buf.emit(&[0x4D, 0x3B, 0x9A]);
            self.buf.emit(&(ss_top + 8).to_le_bytes());
            self.buf.emit(&[0x4D, 0x8D, 0x9B]);
            self.buf.emit(&(-need).to_le_bytes());
            // JA overflow
            self.buf.emit(&[0x0F, 0x87]);
            let p = self.buf.pos();
            self.buf.emit(&[0, 0, 0, 0]);
            Some(p)
        } else {
            None
        };
        // MOV [rbp - savebase], R11    (base of THIS push)
        self.buf.emit(&[0x4C, 0x89, 0x9D]);
        self.buf
            .emit(&(-self.shadow_savebase_slot_off).to_le_bytes());
        for &off in offsets {
            // MOV RCX, [rbp - off] ; MOV [R11], RCX ; LEA R11, [R11 + 8]
            self.buf.emit(&[0x48, 0x8B, 0x8D]);
            self.buf.emit(&(-(off as i32)).to_le_bytes());
            self.buf.emit(&[0x49, 0x89, 0x0B]);
            self.buf.emit(&[0x4D, 0x8D, 0x5B, 0x08]);
        }
        // MOV [R10 + ss_top], R11      (publish the new top)
        self.buf.emit(&[0x4D, 0x89, 0x9A]);
        self.buf.emit(&ss_top.to_le_bytes());
        if let Some(overflow) = overflow {
            // JMP done
            self.buf.emit_byte(0xE9);
            let done = self.buf.pos();
            self.buf.emit(&[0, 0, 0, 0]);
            self.patch_rel32_to_here(overflow);
            // Overflow bail: nothing was stored and `top` is untouched. R11
            // still holds the pre-push `top`; tag bit 0 (slot addresses are
            // 8-aligned) so the paired reload skips its value-restore and only
            // puts `top` back.
            self.buf.emit(&[0x49, 0x83, 0xCB, 0x01]); // OR R11, 1
            self.buf.emit(&[0x4C, 0x89, 0x9D]); // MOV [rbp - savebase], R11
            self.buf
                .emit(&(-self.shadow_savebase_slot_off).to_le_bytes());
            self.emit_shadow_overflow_note();
            self.patch_rel32_to_here(done);
        }
        self.patch_rel32_to_here(skip);
        self.shadow_pushes += 1;
        true
    }

    /// gci i9/ng: does [`Self::emit_shadow_push`] emit anything? The wiring
    /// (thread helper, the two reserved slots), something to publish, and the
    /// shadow-stack codegen on (`shadow_maps_on`, read last and only when the
    /// rest holds). Pure, so the gate is testable without the process-wide
    /// cache.
    pub(in crate::ir_lower) fn shadow_push_emits(
        get_current_thread: usize,
        thread_slot_off: i32,
        savebase_slot_off: i32,
        n_offsets: usize,
        shadow_maps_on: impl FnOnce() -> bool,
    ) -> bool {
        get_current_thread != 0
            && thread_slot_off > 0
            && savebase_slot_off > 0
            && n_offsets > 0
            && shadow_maps_on()
    }

    /// `CRATONVM_JIT_IR_SHADOW_FRAME_BLOCK=0` — publish per call (push every
    /// home's value before the call, copy it back after) instead of once per
    /// activation through the frame block. Default on; the switch is the
    /// one-binary A/B and the kill switch.
    pub(in crate::ir_lower) fn frame_block_enabled() -> bool {
        #[cfg(test)]
        if let Some(on) = FRAME_BLOCK_FORCE.with(|c| c.get()) {
            return on;
        }
        true
    }

    /// JIT round 25 item 2, wave 3 (`CRATONVM_JIT_IR_SHADOW_FRAME_BLOCK_LAZY_CLEAR`;
    /// the section above [`ir_shadow_frame_block_lazy_clear_enabled`]): fill
    /// [`Self::block_lazy_clear`] and [`Self::block_lazy_writers`], or leave
    /// both empty. Runs at the top of the prologue, before the fresh-frame
    /// clear, and plans the frame block itself to know its words.
    pub(in crate::ir_lower) fn plan_block_lazy_clear(&mut self) {
        self.block_lazy_clear.clear();
        self.block_lazy_writers.clear();
        self.own_home_written.clear();
        if !self.block_lazy_clear_allowed {
            return;
        }
        self.plan_frame_block();
        if self.frame_block.is_empty() {
            return;
        }
        let graph = self.graph;
        let schedule = self.schedule;
        let plan = self.slot_plan;
        let nb = schedule.blocks.len();
        // The dataflow starts every path at block 0, so block 0 must be the
        // entry and entered from nowhere else.
        if nb == 0
            || scheduled_entry_block(graph, schedule) != Some(0)
            || !schedule.blocks[0].predecessors.is_empty()
        {
            return;
        }
        let n = graph.nodes.len();
        let mut used = vec![false; n];
        for node in &graph.nodes {
            for &i in &node.inputs {
                if let Some(cell) = used.get_mut(i as usize) {
                    *cell = true;
                }
            }
        }
        let home_of = |colour: u32| -> Option<i16> {
            let off = i32::try_from(u64::from(colour).saturating_mul(8))
                .ok()
                .and_then(|d| self.first_spill.checked_add(d))?;
            if off <= 0 || off > self.spill_cap_off {
                return None;
            }
            i16::try_from(off).ok()
        };
        // Candidate colours: a frame-block word no phi lives in, with at
        // least one occupant that writes its home through its own arm.
        let mut cand_of_colour: HashMap<u32, usize> = HashMap::new();
        let mut cand_home: Vec<i16> = Vec::new();
        let mut phi_colour: std::collections::HashSet<u32> = std::collections::HashSet::new();
        let mut writer_cand: Vec<Option<usize>> = vec![None; n];
        let mut writers: Vec<(NodeId, i32)> = Vec::new();
        for (id, node) in graph.nodes.iter().enumerate() {
            let Some(colour) = plan.node_color.get(id).copied().flatten() else {
                continue;
            };
            if matches!(node.op, Op::Phi) {
                phi_colour.insert(colour);
            }
        }
        for (id, node) in graph.nodes.iter().enumerate() {
            let Some(colour) = plan.node_color.get(id).copied().flatten() else {
                continue;
            };
            if node.ty != IrType::Ref
                || matches!(node.op, Op::Phi | Op::Dead)
                || phi_colour.contains(&colour)
                || !used.get(id).copied().unwrap_or(false)
                || self.home_dropped.get(id).copied().unwrap_or(false)
                || schedule.node_to_block.get(id).copied().is_none_or(|b| b >= nb)
            {
                continue;
            }
            let Some(home) = home_of(colour) else {
                continue;
            };
            if self.frame_block.binary_search(&home).is_err() {
                continue;
            }
            let ci = *cand_of_colour.entry(colour).or_insert_with(|| {
                cand_home.push(home);
                cand_home.len() - 1
            });
            if let Some(cell) = writer_cand.get_mut(id) {
                *cell = Some(ci);
            }
            if let Ok(nid) = NodeId::try_from(id) {
                writers.push((nid, i32::from(home)));
            }
        }
        if cand_home.is_empty() {
            return;
        }
        // The blocks as the dataflow sees them: GC points (every scheduled
        // node `op_cannot_deopt` does not clear, before its own write), the
        // writes, and a GC point at the end for a back-edge poll or a
        // terminator that may call.
        let blocks: Vec<LazyClearBlock> = schedule
            .blocks
            .iter()
            .enumerate()
            .map(|(b, block)| {
                let steps = block
                    .nodes
                    .iter()
                    .flat_map(|&nid| {
                        let node = graph.nodes.get(nid as usize);
                        let gc = node.is_none_or(|n| !op_cannot_deopt(&n.op));
                        let write = writer_cand.get(nid as usize).copied().flatten();
                        [gc.then_some(LazyClearStep::Gc), write.map(LazyClearStep::Write)]
                    })
                    .flatten()
                    .collect();
                let term_gc = block
                    .terminator
                    .is_some_and(|t| graph.nodes.get(t as usize).is_none_or(|n| !op_cannot_deopt(&n.op)));
                let back_edge = block.successors.iter().any(|&s| s <= b);
                LazyClearBlock {
                    preds: block.predecessors.clone(),
                    steps,
                    gc_at_end: term_gc || back_edge,
                }
            })
            .collect();
        let Some(alive) = block_lazy_clear_survivors(&blocks, cand_home.len(), 4 * nb + 16) else {
            return;
        };
        let mut lazy: Vec<i16> = cand_home
            .iter()
            .enumerate()
            .filter(|&(ci, _)| alive.get(ci).copied().unwrap_or(false))
            .map(|(_, &h)| h)
            .collect();
        lazy.sort_unstable();
        lazy.dedup();
        if lazy.is_empty() {
            return;
        }
        writers.retain(|&(_, home)| {
            i16::try_from(home).is_ok_and(|h| lazy.binary_search(&h).is_ok())
        });
        if crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[ir-block-lazy-clear] method={} block_words={} candidates={} lazy={} offsets={:?}",
                graph.scope_method_key,
                self.frame_block.len(),
                cand_home.len(),
                lazy.len(),
                lazy,
            );
        }
        self.own_home_written = vec![0; n];
        self.block_lazy_clear = lazy;
        self.block_lazy_writers = writers;
    }

    /// JIT round 25 item 2, wave 3: every definition [`Self::plan_block_lazy_clear`]
    /// counted as a write of a lazy word must have had its own home write
    /// emitted, to that word. Otherwise the plan rested on a store that is not
    /// there; latch a refusal, and the ladder lowers once more without it.
    pub(in crate::ir_lower) fn verify_block_lazy_clear_writes(&mut self) {
        if self.block_lazy_clear.is_empty() {
            return;
        }
        let missing = self.block_lazy_writers.iter().copied().find(|&(id, home)| {
            self.own_home_written.get(id as usize).copied() != Some(home)
                || self.home_dropped.get(id as usize).copied().unwrap_or(false)
        });
        if let Some((id, home)) = missing {
            self.latch_bailout(Bailout::with_context(
                BailoutReason::Internal("ir_lower: a lazy-cleared frame-block word's write was not emitted"),
                format!("n{id} did not write its home [rbp - {home}]"),
            ));
        }
    }

    /// JIT round 25 item 2, wave 4 (`CRATONVM_JIT_IR_LAZY_REF_HOMES`; the
    /// section above [`ir_lazy_ref_homes_enabled`]): choose the reference
    /// values whose definition stops storing their home, mark them
    /// home-dropped, and record, per hooked node in their ranges, which ones
    /// its helper calls spill and reload. Runs once, before the prologue.
    pub(in crate::ir_lower) fn plan_lazy_ref_homes(&mut self) {
        self.lazy_ref_home.clear();
        self.lazy_ref_sites.clear();
        self.lazy_hook_regions.clear();
        self.lazy_ref_planned = 0;
        self.lazy_ref_blocked_by_calls = 0;
        self.lazy_ref_blocked_other = 0;
        if !(self.ref_residency_on && self.publish_at_def_on && self.phi_copy_regs_on) {
            return;
        }
        let graph = self.graph;
        let schedule = self.schedule;
        let plan = self.slot_plan;
        let n = graph.nodes.len();
        let Some(edges) = block_edge_positions(schedule, &plan.pos_of) else {
            return;
        };
        // Every position a collection may run at that is no hookable node:
        // non-allowlisted nodes and terminators, and back-edge polls (at the
        // terminator, which they precede, or the block's edge).
        let mut blocked_pos: Vec<usize> = Vec::new();
        // Wave 5: the blocking positions that are NOT a real call, for the
        // census of what a call-crossing extension would add.
        let mut other_pos: Vec<usize> = Vec::new();
        let mut hookable_at: Vec<(usize, NodeId)> = Vec::new();
        for (b, block) in schedule.blocks.iter().enumerate() {
            for &id in &block.nodes {
                let Some(node) = graph.nodes.get(id as usize) else {
                    continue;
                };
                let Some(p) = plan.pos_of.get(id as usize).copied().flatten() else {
                    continue;
                };
                match lazy_ref_node_class(&node.op) {
                    LazyRefNodeClass::Transparent => {}
                    LazyRefNodeClass::Hookable if self.lazy_ref_site_ok(id) => {
                        hookable_at.push((p, id));
                    }
                    _ => {
                        blocked_pos.push(p);
                        if !matches!(node.op, Op::Call { .. }) {
                            other_pos.push(p);
                        }
                    }
                }
            }
            let term_pos = block
                .terminator
                .and_then(|t| plan.pos_of.get(t as usize).copied().flatten());
            if let Some(t) = block.terminator {
                let calls = graph.nodes.get(t as usize).is_none_or(|n| !op_cannot_deopt(&n.op));
                if calls {
                    let p = term_pos.unwrap_or(edges.get(b).copied().unwrap_or(0));
                    blocked_pos.push(p);
                    other_pos.push(p);
                }
            }
            if block.successors.iter().any(|&s| s <= b) {
                if let Some(p) = term_pos.or_else(|| edges.get(b).copied()) {
                    blocked_pos.push(p);
                    other_pos.push(p);
                }
            }
        }
        blocked_pos.sort_unstable();
        other_pos.sort_unstable();
        let excluded: std::collections::HashSet<NodeId> = elide_excluded_ids().into_iter().collect();
        let mut chosen: Vec<NodeId> = Vec::new();
        for id in 0..n {
            let Some(node) = graph.nodes.get(id) else {
                continue;
            };
            let Ok(nid) = NodeId::try_from(id) else {
                continue;
            };
            if node.ty != IrType::Ref
                || !matches!(node.op, Op::Load(_))
                || excluded.contains(&nid)
                || self.home_dropped.get(id).copied().unwrap_or(false)
                || self.ls_managed.get(id).copied().unwrap_or(false)
                || self.phi_coalesced_src.get(id).copied().unwrap_or(false)
                || self.deopt_named_reachable.get(id).copied().unwrap_or(true)
            {
                continue;
            }
            let Some(reg) = self.gp_reg_of.get(id).copied().flatten() else {
                continue;
            };
            if !IR_LOWER_LS_GPRS.contains(&reg)
                || IR_LS_CALLER_SAVED_GPRS.contains(&reg)
                || reg == DEOPT_ARG0
            {
                continue;
            }
            let Some(range) = plan.range.get(id).copied().flatten() else {
                continue;
            };
            if range.hi <= range.lo {
                continue;
            }
            // No position in `(lo, hi]` a collection may run at, but the
            // hooked nodes'.
            let first_blocked = blocked_pos.partition_point(|&p| p <= range.lo);
            if blocked_pos.get(first_blocked).is_some_and(|&p| p <= range.hi) {
                let first_other = other_pos.partition_point(|&p| p <= range.lo);
                if other_pos.get(first_other).is_some_and(|&p| p <= range.hi) {
                    self.lazy_ref_blocked_other += 1;
                } else {
                    self.lazy_ref_blocked_by_calls += 1;
                }
                continue;
            }
            chosen.push(nid);
        }
        if chosen.is_empty() {
            return;
        }
        if self.home_dropped.len() < n {
            self.home_dropped.resize(n, false);
        }
        self.lazy_ref_home = vec![false; n];
        for &v in &chosen {
            let i = v as usize;
            self.home_dropped[i] = true;
            self.lazy_ref_home[i] = true;
            let Some(range) = plan.range.get(i).copied().flatten() else {
                continue;
            };
            for &(p, site) in &hookable_at {
                if p > range.lo && p <= range.hi {
                    self.lazy_ref_sites.entry(site).or_default().push(v);
                }
            }
        }
        self.lazy_ref_planned = chosen.len();
    }

    /// May the hookable node `id` carry lazy references across it? Not where
    /// an exception it raises can be caught in this frame: a bci a handler of
    /// this method covers, asked of the node's own pc and of the invoke it
    /// resolves to out of a splice (the implicit-throw pads, planned later,
    /// enter only such handlers). The handler would run past the reload.
    pub(in crate::ir_lower) fn lazy_ref_site_ok(&self, id: NodeId) -> bool {
        let Some(node) = self.graph.nodes.get(id as usize) else {
            return false;
        };
        let Some(pc) = node.bytecode_pc else {
            return false;
        };
        !self.implicit_throw_plans.contains_key(&id)
            && !self.bci_is_protected(pc)
            && !self.bci_is_protected(self.resume_bci(pc))
    }

    /// Wave 4: the lazy references live across node `id` that are in their
    /// registers right now -- the ones [`Self::lazy_ref_after_node`] keeps. A
    /// planned one that is NOT resident refuses the attempt naming it (its
    /// cached copy was dropped somewhere the plan did not foresee).
    pub(in crate::ir_lower) fn lazy_ref_keep_before(&mut self, id: NodeId) -> Vec<NodeId> {
        let Some(list) = self.lazy_ref_sites.get(&id).cloned() else {
            return Vec::new();
        };
        let mut keep = Vec::with_capacity(list.len());
        for v in list {
            if self.lazy_resident(v).is_some() {
                keep.push(v);
            } else {
                self.latch_bailout(Bailout::with_context(
                    BailoutReason::UnallocatedValue { node: v },
                    format!("n{v}'s lazy home: not in its register at hooked node n{id}"),
                ));
            }
        }
        keep
    }

    /// Wave 4: `v`'s register when its cached copy is live and the register
    /// still belongs to it -- [`Self::resident_gpr`]'s question without the
    /// token-read audit (this is no operand read).
    pub(in crate::ir_lower) fn lazy_resident(&self, v: NodeId) -> Option<u8> {
        if !self.gp_reg_live.get(v as usize).copied().unwrap_or(false) {
            return None;
        }
        let reg = self.gp_reg_of.get(v as usize).copied().flatten()?;
        (self.gp_reg_owner.get(reg as usize).copied().flatten() == Some(v)).then_some(reg)
    }

    /// Wave 4: the `(register, home)` pairs the helper call of the node being
    /// lowered (`cur_def`) spills and reloads. Emits nothing.
    pub(in crate::ir_lower) fn lazy_ref_pairs_here(&mut self) -> Vec<(u8, i32)> {
        let Some(d) = self.cur_def else {
            return Vec::new();
        };
        let Some(list) = self.lazy_ref_sites.get(&d).cloned() else {
            return Vec::new();
        };
        let mut pairs = Vec::with_capacity(list.len());
        for v in list {
            match (self.lazy_resident(v), self.home_offset_of(v)) {
                (Some(reg), Some(home)) if home > 0 => pairs.push((reg, home)),
                _ => self.latch_bailout(Bailout::with_context(
                    BailoutReason::UnallocatedValue { node: v },
                    format!("n{v}'s lazy home: no register or home at the helper call of n{d}"),
                )),
            }
        }
        pairs
    }

    /// Wave 4: `MOV [rbp - home], reg` for each pair -- before a helper call
    /// that may collect, so the frame (its block, its band) holds the value.
    pub(in crate::ir_lower) fn emit_lazy_ref_spill_stores(&mut self, pairs: &[(u8, i32)]) {
        for &(reg, home) in pairs {
            self.store_abi_reg_raw(reg, home);
            self.lazy_ref_spill_stores += 1;
        }
    }

    /// Wave 4: `MOV reg, [rbp - home]` for each pair -- after the helper call,
    /// the value a moving collection may have rewritten in the home. Raw:
    /// never a home-reload-forward candidate (RAX holds the helper's answer).
    pub(in crate::ir_lower) fn emit_lazy_ref_reloads(&mut self, pairs: &[(u8, i32)]) {
        for &(reg, home) in pairs {
            let mut prefix = 0x48u8;
            if reg >= 8 {
                prefix |= 0x04;
            }
            self.buf.emit_byte(prefix);
            self.buf.emit_byte(0x8B);
            self.emit_rbp_modrm_disp(reg, home);
        }
    }

    /// Wave 4: open a hooked helper call -- spill this node's lazy references
    /// and remember where the hooked span starts. `None`: nothing to do.
    pub(in crate::ir_lower) fn lazy_ref_hook_enter(&mut self) -> Option<(Vec<(u8, i32)>, usize)> {
        if self.lazy_ref_sites.is_empty() {
            return None;
        }
        let pairs = self.lazy_ref_pairs_here();
        if pairs.is_empty() {
            return None;
        }
        let start = self.buf.pos();
        self.emit_lazy_ref_spill_stores(&pairs);
        Some((pairs, start))
    }

    /// Wave 4: close a hooked helper call -- reload, and record the span as
    /// one whose calls are accounted for.
    pub(in crate::ir_lower) fn lazy_ref_hook_exit(&mut self, hook: Option<(Vec<(u8, i32)>, usize)>) {
        let Some((pairs, start)) = hook else {
            return;
        };
        self.emit_lazy_ref_reloads(&pairs);
        self.lazy_hook_regions.push((start, self.buf.pos()));
    }

    /// Wave 4: after hooked node `keep`'s values were spilled around every
    /// helper call it emitted, re-mark them resident (the node-end
    /// invalidation dropped them; on the fast path the register was never
    /// touched, on the slow path it was reloaded). Refuses the attempt
    /// instead when the node's code `[start, here)` holds a call outside a
    /// hooked span ([`code_has_unhooked_call`], conservative: a byte pattern
    /// that merely looks like a call refuses too).
    pub(in crate::ir_lower) fn lazy_ref_after_node(&mut self, node: NodeId, start: usize, keep: &[NodeId]) {
        let end = self.buf.pos();
        let unhooked = self
            .buf
            .as_slice()
            .get(start..end)
            .is_none_or(|code| code_has_unhooked_call(code, start, &self.lazy_hook_regions))
            // An implicit-throw pad (planned after the lazy plan) enters a
            // handler of this frame past the reload.
            || self.implicit_throw_plans.contains_key(&node);
        if unhooked {
            if let Some(&v) = keep.first() {
                self.latch_bailout(Bailout::with_context(
                    BailoutReason::UnallocatedValue { node: v },
                    format!("n{v}'s lazy home: a call outside a hooked span at code offset {start}"),
                ));
            }
            return;
        }
        for &v in keep {
            let Some(reg) = self.gp_reg_of.get(v as usize).copied().flatten() else {
                continue;
            };
            if self.gp_reg_owner.get(reg as usize).copied().flatten() == Some(v) {
                if let Some(cell) = self.gp_reg_live.get_mut(v as usize) {
                    *cell = true;
                }
            }
        }
    }

    /// Wave 4: the `[ir-lazy-ref]` census line under `CRATONVM_DBG_JITC`:
    /// reference definitions whose home store was emitted vs removed by the
    /// plan, the hooked nodes and the spill stores the slow paths carry.
    pub(in crate::ir_lower) fn report_lazy_ref_homes(&self) {
        if !crate::x64::dbg_jitc_enabled() {
            return;
        }
        if self.lazy_ref_planned == 0
            && self.ref_def_home_stores == 0
            && self.lazy_ref_blocked_by_calls == 0
        {
            return;
        }
        eprintln!(
            "[ir-lazy-ref] method={} ref_def_stores_emitted={} ref_def_stores_removed={} \
             hooked_nodes={} spill_stores={} blocked_by_calls_only={} blocked_other={}",
            self.graph.scope_method_key,
            self.ref_def_home_stores,
            self.lazy_ref_planned,
            self.lazy_ref_sites.len(),
            self.lazy_ref_spill_stores,
            self.lazy_ref_blocked_by_calls,
            self.lazy_ref_blocked_other,
        );
    }

    /// JIT round 25 item 2, proposal 3: may a body whose safepoints make
    /// precise-liveness claims publish through a frame block? Only under the
    /// switch, and only when every claim it can make is backed by a zero: no
    /// claim at all without the converged dead-home plan (`converged` false:
    /// the block is then the pre-claim block exactly), and with it the dead
    /// homes are zeroed at each armed site either by the complete planned
    /// first-byte clears or by the dense per-site stores. Pure.
    pub(in crate::ir_lower) fn frame_block_composes_with_liveness(
        switch_on: bool,
        converged: bool,
        clears_complete: bool,
        dense_stores: bool,
    ) -> bool {
        switch_on && (!converged || clears_complete || dense_stores)
    }

    /// Largest frame block worth publishing. The block costs one store per
    /// home the first time an activation reaches a safepoint, where the
    /// per-call form costs a push and a copy-back per LIVE home per call; past
    /// this many homes a method that reaches one safepoint per activation
    /// would pay more for the block than it saves.
    pub(in crate::ir_lower) const FRAME_BLOCK_MAX_HOMES: usize = 32;

    /// Ceiling for [`Self::frame_block_max_homes`]. A block of this many homes
    /// is 8 KiB of shadow stack, far inside the per-push bound the VM's bail
    /// verdict assumes (`SHADOW_BAIL_MAX_PUSH_BYTES`), and the frame itself is
    /// capped at `DEFAULT_MAX_FRAME_BYTES` (4096 words) anyway.
    pub(in crate::ir_lower) const FRAME_BLOCK_MAX_HOMES_CEILING: usize = 1024;

    /// `CRATONVM_JIT_IR_FRAME_BLOCK_MAX_HOMES` (round 14 wave 1, lane chain;
    /// `r14w1-chain-frame-block-home-cap-patch-FIXED-20260929.md`): the cap
    /// [`Self::plan_frame_block`] applies, default [`Self::FRAME_BLOCK_MAX_HOMES`]
    /// (byte-identical), clamped to [`Self::FRAME_BLOCK_MAX_HOMES_CEILING`]; zero
    /// or junk keeps the default. Past the cap a body publishes its roots at
    /// EVERY GC point (push and copy back every defined home), which costs a
    /// one-activation kernel with a call in its loop 2N memory operations per
    /// call; a larger block costs ~14 bytes of code per home per GC point.
    /// Read per compile.
    pub(in crate::ir_lower) fn frame_block_max_homes() -> usize {
        cratonvm_types::flags::resolve_capped_usize(
            "CRATONVM_JIT_IR_FRAME_BLOCK_MAX_HOMES",
            "IR frame block home cap",
            Self::FRAME_BLOCK_MAX_HOMES,
            Self::FRAME_BLOCK_MAX_HOMES_CEILING,
        )
    }

    /// Decide whether this body publishes through a frame block, and if so
    /// which words the block names (`frame_block`). Must run before the
    /// prologue zeroes the reference homes, because the block changes which
    /// ones it has to zero.
    ///
    /// The block names every word a safepoint map of this body can name: the
    /// reference-parameter homes and the planned slot of every `Ref` node
    /// (phis included). Publishing a word from the first safepoint to the
    /// return is sound only because each of those words holds a reference or
    /// null for that whole span:
    ///
    ///   * the colouring never lets a primitive into a reference colour
    ///     (`SlotPlan::prim_colors`), so a word the block names cannot hold
    ///     an `int` the collector would rewrite;
    ///   * the prologue zeroes every one of them that it does not write itself
    ///     — `zero_ref_value_slots` WITHOUT its entry-block refinement, whose
    ///     argument ("a safepoint before the definition does not publish it")
    ///     does not hold for a block published whole;
    ///   * every exit restores the entry watermark, retracting the block.
    ///
    /// And it is used only where the collector will rewrite through it: the
    /// VM remaps the shadow stack exactly when `shadow_stack_maps_enabled`
    /// agrees, which is also when its frame walk leaves a published block's
    /// homes to it (`conservative_roots::remap_one_jit_frame`).
    pub(in crate::ir_lower) fn plan_frame_block(&mut self) {
        self.frame_block.clear();
        // gen r5w3/live7: a block publishes EVERY reference home for the whole
        // activation, so a home the precise-liveness claim proves dead at a
        // call would still be a precise root through it. Under
        // `CRATONVM_JIT_PRECISE_FRAME_LIVENESS` the body publishes per call
        // instead -- the `CRATONVM_JIT_IR_SHADOW_FRAME_BLOCK=0` form, which
        // pushes only the homes the map names (`emit_safepoint_map`).
        //
        // JIT round 25 item 2, proposal 3 (`CRATONVM_JIT_IR_SHADOW_FRAME_BLOCK_PRECISE`,
        // default ON since R25-FLIP; the section above `ir_shadow_frame_block_precise_enabled`):
        // the block composes with the claim, because every home the claim
        // proves dead at a GC point is zero there (`precise_liveness_claim`
        // makes no claim without the converged dead-home plan), and a block
        // entry naming a zero word roots nothing.
        if self.frame_liveness_on
            && !Self::frame_block_composes_with_liveness(
                true,
                self.dead_home_plan_converged,
                self.dead_home_clears_complete,
                self.dead_ref_clear_stores,
            )
        {
            return;
        }
        if !Self::frame_block_enabled()
            || !crate::x64::shadow_stack_maps_enabled()
            || !self.reloc_emit_on
            || self.sp_id_slot_off <= 0
            || self.get_current_thread == 0
            || self.shadow_thread_slot_off <= 0
            || self.shadow_savebase_slot_off <= 0
            || self.shadow_savetop_slot_off <= 0
        {
            return;
        }
        let mut offs: Vec<i16> = self.ref_param_homes.clone();
        for (id, n) in self.graph.nodes.iter().enumerate() {
            if n.ty != IrType::Ref || matches!(n.op, Op::Dead) {
                continue;
            }
            let Some(color) = self.slot_plan.node_color.get(id).copied().flatten() else {
                continue;
            };
            // `planned_slot_off`'s arithmetic, as `zero_ref_value_slots` does it.
            let Some(off) = i32::try_from(u64::from(color).saturating_mul(8))
                .ok()
                .and_then(|d| self.first_spill.checked_add(d))
            else {
                return;
            };
            if off <= 0 || off > self.spill_cap_off {
                continue;
            }
            let Ok(off) = i16::try_from(off) else {
                return;
            };
            offs.push(off);
        }
        offs.sort_unstable();
        offs.dedup();
        if offs.is_empty() {
            return;
        }
        // Round 15 wave 1 (lane chain): a GC point inside a loop runs the
        // per-GC-point fallback once per ITERATION (a push and a copy-back per
        // defined home), where the block costs one store per home per
        // ACTIVATION. The chain arm's kernels (`CratonBench hashmap`, the
        // reference homes of the spliced `putVal`/`getNode`) are exactly this
        // shape: -10 % with the cap raised to 128 (round 14 w1b bench).
        let base_cap = Self::frame_block_max_homes();
        let cap = Self::frame_block_cap_for(self.graph, self.schedule, offs.len(), base_cap);
        if offs.len() > cap {
            // Round 14 wave 1 (lane chain): say so, or the per-GC-point
            // fallback shows only as code size.
            if self.dbg_ir_reloc_on {
                eprintln!(
                    "[ir-reloc] frame block refused: {} reference homes (cap {cap}); \
                     publishing per GC point",
                    offs.len()
                );
            }
            return;
        }
        if offs.len() > base_cap && self.dbg_ir_reloc_on {
            eprintln!(
                "[ir-reloc] frame block admitted by the loop cap: {} reference homes \
                 (cap {base_cap}, loop cap {cap})",
                offs.len()
            );
        }
        self.frame_block_shared = true;
        self.frame_block = offs;
    }

    /// Round 15 wave 1 (lane chain): the cap a body with a GC point inside a
    /// loop gets under [`Self::frame_block_loop_cap_enabled`] (or the
    /// `CRATONVM_JIT_IR_FRAME_BLOCK_MAX_HOMES` knob, when that is larger).
    pub(in crate::ir_lower) const FRAME_BLOCK_LOOP_MAX_HOMES: usize = 128;

    /// `CRATONVM_JIT_IR_FRAME_BLOCK_LOOP_CAP` — default ON; `0` restores the
    /// flat cap. Read per compile.


    /// `CRATONVM_JIT_IR_FRAME_BLOCK_SHARED_PUBLISH` — default ON; `0` emits
    /// the whole publication inline at every safepoint again. See
    /// [`Self::emit_frame_block_ensure`]. Read per compile.


    /// The home cap [`Self::plan_frame_block`] applies to a body with `homes`
    /// reference homes, and whether a block of that many is taken at all.
    /// The one rule both the plan and the buffer reservation read.
    /// `cap` is [`Self::frame_block_max_homes`], read once by the caller.
    pub(in crate::ir_lower) fn frame_block_cap_for(graph: &Graph, schedule: &Schedule, homes: usize, cap: usize) -> usize {
        if homes > cap
            
            && ir_gc_point_in_loop(graph, schedule)
        {
            cap.max(Self::FRAME_BLOCK_LOOP_MAX_HOMES)
        } else {
            cap
        }
    }

    /// Publish the frame block if this activation has not yet done so. The
    /// frame-block counterpart of [`Self::emit_shadow_push`], emitted at every
    /// safepoint and executed once per activation.
    ///
    /// The saved-base word is the "already published" flag: the prologue zeroes
    /// it, this sets it to the block's base, and an overflow sets it to the
    /// pre-push `top` with bit 0 set — non-zero, so no later safepoint retries,
    /// and tagged, so the collector's frame walk knows the homes are NOT on the
    /// shadow stack and keeps rewriting them itself.
    ///
    /// Registers: R10 (thread), R11 (block base), RCX (entry temp) — the same
    /// three, free at the same point, as the per-call push.
    ///
    /// Round 15 wave 1 (lane chain): under
    /// `CRATONVM_JIT_IR_FRAME_BLOCK_SHARED_PUBLISH` (default ON) only the
    /// once-per-activation test stays at the safepoint —
    /// `CMP [rbp - savebase], 0 ; JNE +5 ; CALL publish` (15 bytes) — and the
    /// publication itself (~50 bytes plus 14 per home) is one subroutine per
    /// body, [`Self::emit_frame_block_publish_stub`]. The inline form repeated
    /// it at every GC point, which is GC points x homes of code (the chain
    /// arm's `HashMap.putVal`: tens of KB). The subroutine touches only R10,
    /// R11, RCX, the frame through `rbp`, and the shadow stack; it calls
    /// nothing (the overflow note's `PUSH RAX`/`POP RAX` is balanced, as it
    /// already was inline) and cannot fault, so the return address it has on
    /// the machine stack is never seen by a stack walk.
    pub(in crate::ir_lower) fn emit_frame_block_ensure(&mut self) -> bool {
        if self.frame_block.is_empty() {
            return false;
        }
        let savebase = -self.shadow_savebase_slot_off;
        // CMP qword [rbp - savebase], 0
        self.buf.emit(&[0x48, 0x83, 0xBD]);
        self.buf.emit(&savebase.to_le_bytes());
        self.buf.emit_byte(0);
        if self.frame_block_shared {
            // JNE +5 (over the CALL) ; CALL publish
            self.buf.emit(&[0x75, 0x05]);
            self.buf.emit_byte(0xE8);
            let rel_at = self.buf.pos();
            match self.frame_block_stub_at {
                Some(stub) => {
                    // A site lowered after the subroutine: a backward call.
                    let rel = i32::try_from(stub as i64 - (rel_at as i64 + 4));
                    if rel.is_err() {
                        self.latch_bailout(Bailout::new(BailoutReason::Internal(
                            "ir_lower: a frame-block publication call is out of rel32 range",
                        )));
                    }
                    self.buf.emit(&rel.unwrap_or(0).to_le_bytes());
                }
                None => {
                    self.frame_block_call_patches.push(rel_at);
                    self.buf.emit(&[0, 0, 0, 0]);
                }
            }
            return true;
        }
        // JNE done
        self.buf.emit(&[0x0F, 0x85]);
        let done_published = self.buf.pos();
        self.buf.emit(&[0, 0, 0, 0]);
        let dones = self.emit_frame_block_publication();
        for p in dones {
            self.patch_rel32_to_here(p);
        }
        self.patch_rel32_to_here(done_published);
        true
    }

    /// Round 15 wave 1 (lane chain): the shared publication subroutine of a
    /// `frame_block_shared` body, after the body, and every `CALL` lowered
    /// before it patched to it. Emits nothing for any other body. The
    /// sequence is the inline ensure's, verbatim, ending in `RET` where the
    /// inline form fell through.
    pub(in crate::ir_lower) fn emit_frame_block_publish_stub(&mut self) {
        if self.frame_block_stub_at.is_some() || self.frame_block_call_patches.is_empty() {
            // Already emitted, or no safepoint published: nothing calls it.
            return;
        }
        if !self.frame_block_shared || self.frame_block.is_empty() {
            // A `CALL` with nothing to call: fail closed rather than leave a
            // zero `rel32` (a call to the next instruction).
            self.latch_bailout(Bailout::new(BailoutReason::Internal(
                "ir_lower: a frame-block publication call has no subroutine",
            )));
            return;
        }
        let stub = self.buf.pos();
        self.frame_block_stub_at = Some(stub);
        let dones = self.emit_frame_block_publication();
        for p in dones {
            self.patch_rel32_to_here(p);
        }
        self.buf.emit_byte(0xC3); // RET
        for site in std::mem::take(&mut self.frame_block_call_patches) {
            // A buffer that overflowed fails the patch; the overflow itself
            // refuses (and retries) the lowering, as for every other patch.
            match i32::try_from(stub as i64 - (site as i64 + 4)) {
                Ok(rel) => {
                    let _ = self.buf.try_patch_i32(site, rel);
                }
                Err(_) => self.latch_bailout(Bailout::new(BailoutReason::Internal(
                    "ir_lower: a frame-block publication call is out of rel32 range",
                ))),
            }
        }
    }

    /// The publication half of the frame-block ensure: from the thread load to
    /// the published `top`. Returns the `rel32` positions the caller patches to
    /// "done", its end (the untracked-thread skip; the overflow tail's own jump
    /// already lands there).
    pub(in crate::ir_lower) fn emit_frame_block_publication(&mut self) -> Vec<usize> {
        let ss_top = self.shadow_off_in_thread;
        let savebase = -self.shadow_savebase_slot_off;
        // MOV R10, [rbp - thread_slot] ; TEST R10, R10 ; JE done — a null
        // thread is "untracked", exactly as the per-call push treats it.
        self.buf.emit(&[0x4C, 0x8B, 0x95]);
        self.buf.emit(&(-self.shadow_thread_slot_off).to_le_bytes());
        self.buf.emit(&[0x4D, 0x85, 0xD2]);
        self.buf.emit(&[0x0F, 0x84]);
        let done_untracked = self.buf.pos();
        self.buf.emit(&[0, 0, 0, 0]);
        // MOV R11, [R10 + ss_top]
        self.buf.emit(&[0x4D, 0x8B, 0x9A]);
        self.buf.emit(&ss_top.to_le_bytes());
        // rx27 (`CRATONVM_JIT_IR_FRAME_BLOCK_DESCRIPTOR`): the block as ONE
        // descriptor record (`cratonvm_gc::shadow_stack`, "Frame descriptor
        // records") when every home packs; otherwise the indirect entries.
        let record = {
            frame_block_descriptor_words(&self.frame_block)
        };
        // The record is constant per body: publish its interned address
        // (`frame_descriptor::intern_descriptor`) instead of copying it.
        let interned = record.as_deref().and_then(cratonvm_types::frame_descriptor::intern_descriptor);
        let record = if interned.is_some() { None } else { record };
        // Cast: at most FRAME_BLOCK_MAX_HOMES_CEILING entries (`frame_block_max_homes`),
        // or 1 + 1 + 1024 / 4 record words.
        let need = match (&record, interned) {
            (_, Some(_)) => 16,
            (Some(words), None) => (words.len() as i32 + 1) * 8,
            (None, None) => (self.frame_block.len() as i32) * 8,
        };
        // The per-call push's overflow guard, verbatim.
        let overflow = if crate::shadow_end_guard_enabled() {
            self.buf.emit(&[0x4D, 0x8D, 0x9B]);
            self.buf.emit(&need.to_le_bytes());
            self.buf.emit(&[0x4D, 0x3B, 0x9A]);
            self.buf.emit(&(ss_top + 8).to_le_bytes());
            self.buf.emit(&[0x4D, 0x8D, 0x9B]);
            self.buf.emit(&(-need).to_le_bytes());
            self.buf.emit(&[0x0F, 0x87]);
            let p = self.buf.pos();
            self.buf.emit(&[0, 0, 0, 0]);
            Some(p)
        } else {
            None
        };
        // MOV [rbp - savebase], R11 — the block is published from here on.
        self.buf.emit(&[0x4C, 0x89, 0x9D]);
        self.buf.emit(&savebase.to_le_bytes());
        let block = if record.is_some() || interned.is_some() {
            Vec::new()
        } else {
            self.frame_block.clone()
        };
        if let Some(addr) = interned {
            // LEA RCX, [rbp + DESCRIPTOR_TAG] ; MOV [R11], RCX ;
            // MOV RCX, imm64 ; MOV [R11 + 8], RCX -- `[rbp | 2, address]`.
            self.buf.emit(&[0x48, 0x8D, 0x8D]);
            self.buf.emit(&FRAME_BLOCK_DESCRIPTOR_TAG.to_le_bytes());
            self.buf.emit(&[0x49, 0x89, 0x0B]);
            self.buf.emit(&[0x48, 0xB9]);
            self.buf.emit(&(addr as u64).to_le_bytes());
            self.buf.emit(&[0x49, 0x89, 0x8B]);
            self.buf.emit(&8i32.to_le_bytes());
        }
        if let Some(words) = &record {
            // LEA RCX, [rbp + DESCRIPTOR_TAG] ; MOV [R11], RCX -- word 0.
            self.buf.emit(&[0x48, 0x8D, 0x8D]);
            self.buf.emit(&FRAME_BLOCK_DESCRIPTOR_TAG.to_le_bytes());
            self.buf.emit(&[0x49, 0x89, 0x0B]);
            // MOV RCX, imm64 ; MOV [R11 + 8 + 8k], RCX -- the header and the
            // packed home words, all stored before `top` moves over them.
            for (k, &w) in words.iter().enumerate() {
                self.buf.emit(&[0x48, 0xB9]);
                self.buf.emit(&w.to_le_bytes());
                self.buf.emit(&[0x49, 0x89, 0x8B]);
                // Cast: k < 1 + 1024 / 4.
                self.buf.emit(&(((k as i32) + 1) * 8).to_le_bytes());
            }
        }
        for (i, &off) in block.iter().enumerate() {
            // LEA RCX, [rbp - off + 1] ; MOV [R11 + 8i], RCX — the slot's
            // address with `ShadowStack::INDIRECT_TAG` set.
            let tagged = 1 - i32::from(off);
            self.buf.emit(&[0x48, 0x8D, 0x8D]);
            self.buf.emit(&tagged.to_le_bytes());
            // Cast: i < FRAME_BLOCK_MAX_HOMES_CEILING.
            self.buf.emit(&[0x49, 0x89, 0x8B]);
            self.buf.emit(&((i as i32) * 8).to_le_bytes());
        }
        // LEA R11, [R11 + need] ; MOV [R10 + ss_top], R11
        self.buf.emit(&[0x4D, 0x8D, 0x9B]);
        self.buf.emit(&need.to_le_bytes());
        self.buf.emit(&[0x4D, 0x89, 0x9A]);
        self.buf.emit(&ss_top.to_le_bytes());
        if let Some(overflow) = overflow {
            // JMP done
            self.buf.emit_byte(0xE9);
            let done = self.buf.pos();
            self.buf.emit(&[0, 0, 0, 0]);
            self.patch_rel32_to_here(overflow);
            // Nothing stored, `top` untouched: tag the saved base so this
            // activation neither retries nor claims the block (see above).
            self.buf.emit(&[0x49, 0x83, 0xCB, 0x01]); // OR R11, 1
            self.buf.emit(&[0x4C, 0x89, 0x9D]); // MOV [rbp - savebase], R11
            self.buf.emit(&savebase.to_le_bytes());
            self.emit_shadow_overflow_note();
            self.patch_rel32_to_here(done);
        }
        vec![done_untracked]
    }

    /// Bump the process-wide shadow-overflow bail counter
    /// (`cratonvm_jit::SHADOW_OVERFLOW_COUNT`). RAX is pushed/popped around it
    /// so the site stays transparent to whatever the call is staging.
    pub(in crate::ir_lower) fn emit_shadow_overflow_note(&mut self) {
        // Cast through a raw pointer before converting the static's address.
        let counter =
            (&crate::SHADOW_OVERFLOW_COUNT as *const std::sync::atomic::AtomicUsize) as usize;
        self.buf.emit_byte(0x50); // push rax
        self.emit_mov_reg_imm64(RAX, counter as u64);
        self.buf.emit(&[0xF0, 0x48, 0xFF, 0x00]); // lock inc qword [rax]
        self.buf.emit_byte(0x58); // pop rax
    }

    /// cov-01 — direct (helper-free) read of a static field, the IR tier's
    /// mirror of `x64::Compiler::try_emit_inline_getstatic`. Returns `false`
    /// when the site is not eligible, leaving the caller's helper lowering in
    /// place.
    ///
    /// What is baked is the address of the class's base-POINTER cell, not of
    /// the statics block: one extra dependent load buys immunity to every
    /// republication path, because a `StaticsBlock` is a leaked, never-freed
    /// allocation per class whose *cell* address is stable while the block
    /// pointer inside it is not.
    ///
    /// `resolve_static_base` returning `None` is the whole eligibility test,
    /// and it is the VM's judgement rather than this file's: it declines a
    /// class that is not yet initialized (an inline load runs no `<clinit>`),
    /// `java/lang/System` (the `out`/`err`/`in` bootstrap intercept), anything
    /// not yet published, and a second VM in this process. Reusing that one
    /// predicate is what keeps the two tiers from developing different opinions
    /// about which statics may be read directly.
    ///
    /// Only the two type tags the builder admits are emitted — reference
    /// (64-bit payload) and int-category (`MOVSXD` of the 32-bit payload) —
    /// because `IrBuilder`'s `0xb2` arm refuses `J`/`D`/`F` outright. A tag
    /// that reached here anyway would be a builder bug, so it takes the helper
    /// rather than a plausible-looking wrong width.
    /// r23 wave 2d, lane L8: will [`Self::emit_inline_getstatic`] serve this
    /// `getstatic` (the direct read, no call)? Its own admission, asked ahead
    /// of emission: the inline route on, a tag it has a width for, a resolved
    /// static base and a cell offset. Reads only. A resolver that declines
    /// later only takes the helper road UNARMED, which is the conservative map
    /// every unarmed GC point has.
    pub(in crate::ir_lower) fn load_static_reads_directly(&self, class_id: u32, field_index: u32, type_tag: u8) -> bool {
        crate::x64::inline_getstatic_enabled()
            && matches!(
                type_tag,
                b'L' | b'[' | b'J' | b'D' | b'F' | b'I' | b'Z' | b'B' | b'C' | b'S'
            )
            && static_cell_offsets(field_index).is_some()
            && self
                .direct_helpers
                .resolve_static_base(class_id, field_index as usize)
                .is_some()
    }

    pub(in crate::ir_lower) fn emit_inline_getstatic(
        &mut self,
        id: NodeId,
        class_id: u32,
        field_index: u32,
        type_tag: u8,
        is_volatile: bool,
    ) -> bool {
        if !crate::x64::inline_getstatic_enabled() {
            return false;
        }
        // Three widths, and they are the single-pass arm's three, in the same
        // order and with the same constants:
        //
        //   `J`/`D`/`L`/`[`  64-bit MOV of the 64-bit payload
        //   `F`              32-bit MOV of the 32-bit payload — a float's bit
        //                    pattern, so ZERO-extended. `MOVSXD` here would
        //                    sign-extend any float whose bit 31 is set (i.e.
        //                    every negative one) into garbage in the high half,
        //                    and the home word is what `publish_fp_from_slot`
        //                    and every deopt frame read.
        //   int-category     `MOVSXD` of the 32-bit payload
        //
        // A tag outside those is a builder bug — its 0xb2 arm admits exactly
        // these — so take the helper rather than emit a plausible-looking width.
        let wide = matches!(type_tag, b'L' | b'[' | b'J' | b'D');
        let is_float = type_tag == b'F';
        if !wide && !is_float && !matches!(type_tag, b'I' | b'Z' | b'B' | b'C' | b'S') {
            return false;
        }
        let Some(base_cell) = self
            .direct_helpers
            .resolve_static_base(class_id, field_index as usize)
        else {
            return false;
        };
        // Cell byte offset within the class's statics block, plus the payload
        // half of the 16-byte cell — the same arithmetic the single-pass arm
        // uses, and the same `FIELD_CELL_PAYLOAD*_OFFSET` constants
        // (`static_cell_offsets`, shared with the inline `putstatic`).
        let Some((_, payload32, payload64)) = static_cell_offsets(field_index) else {
            return false;
        };
        let disp = if wide { payload64 } else { payload32 };
        let slot = self.alloc_slot(id);
        // MOV RAX, imm64(&base_cell) ; MOV RAX, [RAX]
        self.emit_mov_reg_imm64(RAX, base_cell as u64);
        self.buf.emit(&[0x48, 0x8B, 0x80]); // MOV RAX, [RAX + disp32]
        self.buf.emit(&0i32.to_le_bytes());
        if wide {
            self.buf.emit(&[0x48, 0x8B, 0x80]); // MOV RAX, [RAX + disp32]
        } else if is_float {
            self.buf.emit(&[0x8B, 0x80]); // MOV EAX, [RAX + disp32] (zero-extends)
        } else {
            self.buf.emit(&[0x48, 0x63, 0x80]); // MOVSXD RAX, [RAX + disp32]
        }
        self.buf.emit(&disp.to_le_bytes());
        // No fence after a volatile LOAD on x86-TSO: the load is already an
        // acquire, and the StoreLoad the JMM needs is the volatile store's
        // trailing fence (`LOCK ADD DWORD [RSP], 0` in this tier since round 11
        // wave 2). `CRATONVM_JIT_VOLATILE_LOAD_FENCE=1` restores the
        // old one — see `runtime_lowering::volatile_load_fence_enabled`.
        if is_volatile && self.volatile_load_fence_on {
            self.buf.emit(&[0x0F, 0xAE, 0xF0]); // MFENCE
        }
        self.store_rax(slot);
        // An FP result was computed in RAX and is now in its home word — the
        // `Op::ConstF` shape exactly, so it publishes the same way. A no-op when
        // the allocator gave this value no register.
        if matches!(type_tag, b'F' | b'D') {
            self.publish_fp_from_slot(id, slot, type_tag == b'D');
        }
        true
    }

    /// JIT round 21 wave 1 (lane indy, INDY21-2, [`ir_clinit_barrier_enabled`]):
    /// HotSpot's class-initialization barrier for a `getstatic` whose class was
    /// NOT initialized at compile time (so [`Self::emit_inline_getstatic`]
    /// declined), emitted AHEAD of the helper route, which stays as its decline
    /// edge:
    ///
    /// ```text
    ///   rax = TLS JvmThread*           ; 0 -> helper
    ///   rax = [rax + thread_id] + 1
    ///   cmp rax, [owner cell]          ; != -> helper (not this thread's claim)
    ///   rax = [base cell]              ; the statics block, 0 -> helper
    ///   rax = [rax + payload]          ; the width of emit_inline_getstatic
    ///   store ; jmp done
    /// helper: <map, CALL jit_getstatic, ...>
    /// done:
    /// ```
    ///
    /// The owner cell (VM: `jit_bridge::clinit_owner_cell_for`) holds `1 +` the
    /// thread id of the class's init claimant, set under the claim and cleared
    /// when `finalize_class_init` ends the claim (success or failure). So the
    /// inline read happens exactly when the helper would answer the JVMS 5.5
    /// step 3 recursive request (the claimant reading its own class's statics,
    /// no `<clinit>` to run), and every other thread -- and the claimant after
    /// a failed `<clinit>` -- takes the helper, which waits, runs `<clinit>`,
    /// or throws `NoClassDefFoundError`. No call, no safepoint and no deopt
    /// point on the fast path. RAX and R11 only (R11 is this tier's scratch,
    /// as the poll uses it).
    ///
    /// Returns the `JMP done` operand to patch after the helper route, or
    /// `None` (nothing emitted): switch off, no TLS thread word, a width the
    /// inline read does not take, a field index past 16 bits, or a VM that
    /// answered no cell / no base / no offset.
    pub(in crate::ir_lower) fn emit_clinit_barrier_getstatic(
        &mut self,
        id: NodeId,
        class_id: u32,
        field_index: u32,
        type_tag: u8,
        is_volatile: bool,
    ) -> Option<usize> {
        if !ir_clinit_barrier_enabled() || class_id == 0 {
            return None;
        }
        let wide = matches!(type_tag, b'L' | b'[' | b'J' | b'D');
        let is_float = type_tag == b'F';
        if !wide && !is_float && !matches!(type_tag, b'I' | b'Z' | b'B' | b'C' | b'S') {
            return None;
        }
        let tls = u32::try_from(crate::x64::jit_thread_tls_disp())
            .ok()
            .filter(|&d| d != 0)?;
        let field16 = u16::try_from(field_index).ok()?;
        let cell = vh_const_ask(
            &self.direct_helpers,
            vh_const_query(WELL_KNOWN_CLINIT_OWNER_CELL_TAG, class_id, 0),
        );
        let base_cell = vh_const_ask(
            &self.direct_helpers,
            vh_const_query(WELL_KNOWN_CLINIT_BASE_CELL_TAG, class_id, field16),
        );
        let tid_off_plus_one = vh_const_ask(&self.direct_helpers, WELL_KNOWN_JVM_THREAD_ID_OFFSET);
        if cell <= 0 || base_cell <= 0 || tid_off_plus_one <= 0 {
            return None;
        }
        let tid_off = i32::try_from(tid_off_plus_one - 1).ok()?;
        let (_, payload32, payload64) = static_cell_offsets(field_index)?;
        let disp = if wide { payload64 } else { payload32 };
        if crate::ir_stage_reporting() || crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] ir clinit barrier getstatic n{id} class_id={class_id} \
                 field={field_index} cell={cell:#x}"
            );
        }
        let slot = self.alloc_slot(id);
        let mut helper: Vec<usize> = Vec::with_capacity(3);
        // RAX = the `JvmThread*` (0 outside a JIT thread context).
        self.emit_mov_rax_tls_disp32(tls);
        self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
        helper.push(self.emit_jcc_rel32(0x84)); // JZ helper
        self.buf.emit(&[0x48, 0x8B, 0x80]); // MOV RAX, [RAX + disp32]: thread_id
        self.buf.emit(&tid_off.to_le_bytes());
        self.buf.emit(&[0x48, 0xFF, 0xC0]); // INC RAX
        // Cast: a VM-lived cell address as the imm64 operand.
        self.emit_mov_reg_imm64(R11, cell as u64);
        self.buf.emit(&[0x49, 0x3B, 0x03]); // CMP RAX, [R11]
        helper.push(self.emit_jcc_rel32(0x85)); // JNE helper
        // Cast: as above.
        self.emit_mov_reg_imm64(RAX, base_cell as u64);
        self.buf.emit(&[0x48, 0x8B, 0x80]); // MOV RAX, [RAX + 0]: the statics block
        self.buf.emit(&0i32.to_le_bytes());
        self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
        helper.push(self.emit_jcc_rel32(0x84)); // JZ helper
        if wide {
            self.buf.emit(&[0x48, 0x8B, 0x80]); // MOV RAX, [RAX + disp32]
        } else if is_float {
            self.buf.emit(&[0x8B, 0x80]); // MOV EAX, [RAX + disp32] (zero-extends)
        } else {
            self.buf.emit(&[0x48, 0x63, 0x80]); // MOVSXD RAX, [RAX + disp32]
        }
        self.buf.emit(&disp.to_le_bytes());
        if is_volatile && self.volatile_load_fence_on {
            self.buf.emit(&[0x0F, 0xAE, 0xF0]); // MFENCE
        }
        self.store_rax(slot);
        if matches!(type_tag, b'F' | b'D') {
            self.publish_fp_from_slot(id, slot, type_tag == b'D');
        }
        let done = self.emit_jmp_rel32();
        for p in helper {
            self.patch_rel32_to_here(p);
        }
        Some(done)
    }

    /// Round 16 (lane fieldguard): a PRIMITIVE `putstatic` as a direct store
    /// into the class's statics cell, with no helper `CALL`
    /// (`CRATONVM_JIT_INLINE_PUTSTATIC`, default ON since round 16;
    /// [`inline_putstatic_enabled`]).
    ///
    /// The store twin of [`Self::emit_inline_getstatic`], on the same
    /// eligibility test: `resolve_static_base` answers only for a class whose
    /// `<clinit>` has completed (monotone), whose statics block is published
    /// and covers `field_index`, and which is not `java/lang/System`. So the
    /// helper has nothing left to decide for a primitive write:
    ///
    /// * **class initialization** -- `jit_putstatic_class_init_guard` returns
    ///   at its first line for an initialized class;
    /// * **the SATB pre-barrier** -- `set_static_shared` logs only an old
    ///   `Value::Object`, and a primitive static's cell never holds one;
    /// * **block creation / growth** -- only on its locked fallback, which a
    ///   published block covering the index never reaches;
    /// * **the write itself** -- the helper writes the whole 16-byte `Value`
    ///   cell (`cell.write_volatile`). This writes the same two words,
    ///   word 0 = `tag | payload32 << 32` and word 1 = 0 for an int-category or
    ///   `float` value, word 0 = `tag` and word 1 = the payload for `long` /
    ///   `double`: the words `write_value_atomic` leaves and the inline
    ///   primitive `putfield`'s legacy arm writes, so each store changes one
    ///   aligned 8-byte word and a racing reader sees the old or the new value.
    ///
    /// The base pointer is re-read from the baked cell on every execution, so
    /// a republished block is followed exactly as the inline `getstatic`
    /// follows it. A store racing a GROW of the same block can land in the
    /// abandoned copy -- the helper's documented behaviour too
    /// (`set_static_shared`, "Exclusion from republication: not provided").
    /// The value is already narrowed (the builder's `narrow_to_field_type`).
    /// A volatile write keeps its trailing StoreLoad fence in the caller.
    ///
    /// Reference statics never come here: their write owes the SATB
    /// pre-barrier, which is the helper's.
    ///
    /// Clobbers RAX, RCX and RDX only (this lowerer's per-opcode scratch).
    /// Returns `false`, having emitted nothing, when the site is not admitted.
    pub(in crate::ir_lower) fn emit_inline_putstatic(
        &mut self,
        class_id: u32,
        field_index: u32,
        type_tag: u8,
        value: NodeId,
    ) -> bool {
        if !inline_putstatic_enabled() {
            return false;
        }
        let wide = matches!(type_tag, b'J' | b'D');
        if !wide && !matches!(type_tag, b'I' | b'Z' | b'B' | b'C' | b'S' | b'F') {
            return false;
        }
        let Some(base_cell) = self
            .direct_helpers
            .resolve_static_base(class_id, field_index as usize)
        else {
            return false;
        };
        // The cell and its 64-bit payload word (`static_cell_offsets`, shared
        // with the inline `getstatic`).
        let Some((cell_off, _, hi_off)) = static_cell_offsets(field_index) else {
            return false;
        };
        // `Value`'s discriminants: Int 0, Long 1, Float 2, Double 3 (the
        // single-pass `legacy_cell_tag_for`; sub-int statics are `Value::Int`).
        let tag: u32 = match type_tag {
            b'J' => 1,
            b'F' => 2,
            b'D' => 3,
            _ => 0,
        };
        // The value first, so nothing `gp_load_value` emits can disturb the
        // address RAX is about to hold.
        self.gp_load_value(RDX, value);
        // MOV RAX, imm64(&base_cell) ; MOV RAX, [RAX]
        self.emit_mov_reg_imm64(RAX, base_cell as u64);
        self.buf.emit(&[0x48, 0x8B, 0x80]); // MOV RAX, [RAX + disp32]
        self.buf.emit(&0i32.to_le_bytes());
        if wide {
            // word 0 = tag (bytes 4..8 zero); word 1 = the 64-bit payload.
            self.buf.emit(&[0x48, 0xC7, 0x80]); // MOV qword [RAX + disp32], imm32
            self.buf.emit(&cell_off.to_le_bytes());
            self.buf.emit(&tag.to_le_bytes());
            self.buf.emit(&[0x48, 0x89, 0x90]); // MOV qword [RAX + disp32], RDX
            self.buf.emit(&hi_off.to_le_bytes());
        } else {
            // word 0 = tag | payload32 << 32; word 1 = 0.
            self.note_rcx_written();
            self.buf.emit(&[0x89, 0xD1]); // MOV ECX, EDX (zero-extends)
            self.buf.emit(&[0x48, 0xC1, 0xE1, 0x20]); // SHL RCX, 32
            if tag != 0 {
                // Cast: `tag` is 0..=3, a positive imm8.
                self.buf.emit(&[0x48, 0x83, 0xC9, tag as u8]); // OR RCX, imm8
            }
            self.buf.emit(&[0x48, 0x89, 0x88]); // MOV qword [RAX + disp32], RCX
            self.buf.emit(&cell_off.to_le_bytes());
            self.buf.emit(&[0x31, 0xC9]); // XOR ECX, ECX
            self.buf.emit(&[0x48, 0x89, 0x88]); // MOV qword [RAX + disp32], RCX
            self.buf.emit(&hi_off.to_le_bytes());
        }
        true
    }

    /// JIT r20 wave 17 (lane pAlloc2; `CRATONVM_JIT_IR_INLINE_PUTSTATIC_REF`,
    /// default ON, `=0` = the helper call alone): a REFERENCE `putstatic` of
    /// an initialized class as a direct store into its statics cell while the
    /// SATB pre-gate byte is clear. `R20PAllocTlabTls`'s kernels pay one
    /// `sink = obj` per allocation, and every one was a `jit_putstatic_object`
    /// call with its safepoint map, post-call record and shadow reload.
    ///
    /// What the helper does for a site `resolve_static_base` admits (the
    /// eligibility test [`Self::emit_inline_putstatic`] already relies on: a
    /// completed `<clinit>`, a published block covering the index, not
    /// `java/lang/System`), and why the fast path may skip each step:
    ///
    /// * **class initialization** -- the guard returns at once for an
    ///   initialized class (monotone), so the value pin around it is moot;
    /// * **the SATB pre-barrier** -- `set_static_shared` logs the OLD
    ///   reference through `heap.satb_barrier`, a no-op exactly while the
    ///   collector's pre-gate byte (`helpers.ref_store_pre_gate`, the
    ///   `Lowerer::satb_pre_gate` the G1 gated `putfield` reads) is clear. The
    ///   byte is armed only inside a stop-the-world step, and nothing between
    ///   the test and the store can reach a safepoint, so a clear byte proves
    ///   the barrier owes nothing; an armed byte takes the helper. A
    ///   cross-thread take-over can freeze a peer between the two (no poll is
    ///   needed for that); for a static it is still sound, because the old
    ///   value is in a static, which the arming pause scans as a root
    ///   (`docs/feature-designs/jit-barrier-lowering.md`
    ///   covers the field and array stores). An unwired
    ///   gate (`0`) declines the fast path at compile time;
    /// * **no post barrier** -- statics live outside the heap and are scanned
    ///   as roots; `set_static_shared` calls none, on either path;
    /// * **the write** -- the helper writes the whole 16-byte `Value` cell.
    ///   This writes word 1 = the pointer (`Object(None)` is the zero pointer,
    ///   the niche) and then word 0 = discriminant `4`, each one aligned 8-byte
    ///   store, so a racing reader sees the old or the new reference.
    ///
    /// Returns the fast path's `JMP` patch (the caller lands it past its
    /// helper route), having routed an armed gate to the code that follows;
    /// `None`, having emitted nothing, when the site is not admitted. Clobbers
    /// RAX, RDX and R11 (this lowerer's per-opcode scratch).
    pub(in crate::ir_lower) fn emit_inline_putstatic_ref(
        &mut self,
        class_id: u32,
        field_index: u32,
        type_tag: u8,
        value: NodeId,
    ) -> Option<usize> {
        if !matches!(type_tag, b'L' | b'[')
            || !inline_putstatic_enabled()
            || !ir_inline_putstatic_ref_enabled()
            || self.satb_pre_gate == 0
        {
            return None;
        }
        let base_cell = self
            .direct_helpers
            .resolve_static_base(class_id, field_index as usize)?;
        let (cell_off, _, hi_off) = static_cell_offsets(field_index)?;
        if crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] ir inline putstatic ref: class={class_id} field={field_index}"
            );
        }
        // MOV R11, imm64(&gate) ; CMP BYTE [R11], 0 ; JNE -> the helper route.
        // zs s5/bt2: on ZGC's age-floor plan (the fused gate's condition, so
        // G1 and the generational mask plan keep these bytes) the same byte
        // compare is one RIP-relative instruction when the gate is in reach:
        // `CMP BYTE [rip+disp32], 0` (7 bytes, `80 3D d32 00`). Only the pre
        // byte: a static is a root, so no post gate is owed.
        let here = self.buf.as_ptr() as usize + self.buf.pos(); // Cast: buffer address
        let rip = (self.zgc_fused_gate.is_some() && crate::x64::zgc_fused_gate_all_sites_enabled())
            .then(|| zgc_pre_byte_cmp_rip_bytes(here, self.satb_pre_gate))
            .flatten();
        if let Some(bytes) = rip {
            self.buf.emit(&bytes);
        } else {
            self.emit_mov_reg_imm64(R11, self.satb_pre_gate as u64);
            self.buf.emit(&[0x41, 0x80, 0x3B, 0x00]);
        }
        let armed = self.emit_jcc_rel32(0x85);
        // The value first, so nothing `gp_load_value` emits can disturb RAX.
        self.gp_load_value(RDX, value);
        // MOV RAX, imm64(&base_cell) ; MOV RAX, [RAX]
        self.emit_mov_reg_imm64(RAX, base_cell as u64);
        self.buf.emit(&[0x48, 0x8B, 0x80]); // MOV RAX, [RAX + disp32]
        self.buf.emit(&0i32.to_le_bytes());
        self.buf.emit(&[0x48, 0x89, 0x90]); // MOV qword [RAX + disp32], RDX
        self.buf.emit(&hi_off.to_le_bytes());
        self.buf.emit(&[0x48, 0xC7, 0x80]); // MOV qword [RAX + disp32], imm32
        self.buf.emit(&cell_off.to_le_bytes());
        self.buf.emit(&4u32.to_le_bytes()); // `Value::Object`'s discriminant
        let join = self.emit_jmp_rel32();
        self.patch_rel32_to_here(armed);
        Some(join)
    }

    /// Guard a baked COMPACT CELL OFFSET against a layout replacement, and
    /// return the patch site the caller routes to its helper.
    ///
    /// The optimizing tier's twin of `x64::objects::emit_layout_epoch_guard`,
    /// and it exists for the same reason: an emitter that bakes
    /// `HEADER_SIZE + packed_body_offset` as an immediate is making a
    /// compile-time claim about a layout the class manager can replace at run
    /// time. The two ALLOCATION emitters have guarded that since
    /// perf/halfgap-20260717 and call an unguarded baked layout "confirmed heap
    /// corruption"; the field-access emitters had no guard at all.
    ///
    /// Four instructions against the process-wide replacement epoch, which only
    /// a REPLACEMENT bumps — never a new class registration — so a workload
    /// that never swaps a layout keeps every inline arm.
    pub(in crate::ir_lower) fn emit_layout_epoch_guard(&mut self) -> Option<usize> {
        if !self.emit_layout_epoch_cmp() {
            return None;
        }
        Some(self.emit_jcc_rel32(0x85)) // JNE -> the caller's slow path
    }

    /// The compare half of [`Self::emit_layout_epoch_guard`]: ZF=1 exactly
    /// when the replacement epoch still matches the one this compile baked.
    /// Emits nothing and answers `false` when there is no epoch cell.
    ///
    /// Split out (round 11 wave 2, lane lower) for a site with no helper slow
    /// path to branch to: `Op::Unbox` deopts on a mismatch instead. Clobbers
    /// R11 and ECX on the non-RIP fallback form, like the guard.
    pub(in crate::ir_lower) fn emit_layout_epoch_cmp(&mut self) -> bool {
        let (addr, expected) = cratonvm_types::layout_replace_epoch_guard();
        if addr.is_null() {
            return false;
        }
        // Round 16 (lane fieldguard, FG-b3): the epoch as of the compile's
        // start, before its compact rows were resolved, not as of now.
        let expected = crate::compile_baked_layout_epoch(expected);
        if !self.emit_cmp_layout_epoch_rip(addr as usize, expected) {
            // Out of ±2GB RIP reach, or the encoding switched off: materialize
            // the address and read through it. This is the shape the guard had
            // before 2026-09-10, kept verbatim as the fallback.
            self.emit_mov_reg_imm64(R11, addr as u64);
            // CMP dword [R11], imm32 (REX.B + 81 /7).
            self.buf.emit(&[0x41, 0x81, 0x3B]);
            self.buf.emit(&(expected as i32).to_le_bytes());
        }
        true
    }

    /// Round 16 (lane fieldguard, FG16-1): the layout-epoch guard of an inline
    /// field site on receiver `base` at bytecode `pc`, in one of three forms.
    /// Returns the patch the caller routes to its helper, as
    /// [`Self::emit_layout_epoch_guard`] does, or `None` when nothing that
    /// rejoins was emitted.
    ///
    /// With `CRATONVM_JIT_IR_LAYOUT_GUARD_DEOPT` off (the default) it IS
    /// `emit_layout_epoch_guard`: a compare whose failure takes this site's
    /// helper and then REJOINS. That rejoin is why no later site could trust
    /// it: the code after the site runs whether the guard passed or not.
    ///
    /// With the switch on:
    ///
    /// * **Elided** -- no compare at all -- when an earlier DEOPTING guard
    ///   proved `base` and that guard's execution is on every path here with
    ///   no later redefinition of `base` in between: either it was emitted
    ///   earlier in THIS block (a block runs straight through, and `base`, an
    ///   input of the earlier site, was defined before it), or the body is
    ///   LOOP-FREE (`schedule_block_cfg_has_cycle` is false) and the guard's
    ///   block dominates this one. Loop-free matters twice: a value defined
    ///   inside a loop is a different object each iteration under one node
    ///   id, and an OSR entry stub enters a loop header past the blocks that
    ///   dominate it (the precedent is the dominated-sample elision, which is
    ///   planned for loop-free bodies only, for the same reason).
    /// * **Deopting** -- `CMP epoch; JE continue; <deopt at pc>` -- otherwise,
    ///   when the site may deopt here: `may_deopt` (the caller's statement
    ///   that [`node_cannot_deopt`] already predicts a deopt at this node, so
    ///   its snapshot is consultable and every value it names keeps a home),
    ///   the site is in the compiling method's own code (a guard inside a
    ///   spliced body would resume at the `invoke` and could replay a
    ///   committed store), no OTHER effectful node carries the same bytecode
    ///   pc (an intrinsic expansion that stores and then reads at one
    ///   `invoke` pc would replay its store on re-execution;
    ///   [`layout_guard_lone_pcs`]), and the body is not self-locking (whose deopt
    ///   exits the hand-over audits; lane monitor's). The deopt re-executes
    ///   the `getfield`/`putfield` in the interpreter, which nothing before
    ///   the guard has started: the guard precedes every load, store and
    ///   receiver dereference of the site. Records `(base, block)` and the
    ///   same epoch witness for entry reference parameters. Compactness and
    ///   nullness remain separate per-object checks.
    /// * **Rejoining**, as with the switch off, in every other case; records
    ///   nothing.
    ///
    /// # Why an elided guard is sound
    ///
    /// The guard protects a BAKED compact displacement against a layout
    /// REPLACEMENT. An object's layout is fixed when it is allocated: its
    /// header's compact bit and its class's layout version at that moment
    /// decide its body, and no collector re-lays an object (they copy it).
    /// `register_class_layout` bumps `LAYOUT_REPLACE_EPOCH` (Release) under
    /// the registry's write lock BEFORE it publishes the replacement, and an
    /// allocation reads the layout through that lock, so any object laid out
    /// by a replacement was allocated by a thread that had already seen the
    /// bump, and any thread that obtains a reference to it (x86-TSO is
    /// causally consistent) sees the bump too. A deopting guard that PASSED
    /// after `base` was defined therefore proves that the object `base`
    /// names has the baked layout or a legacy one (which the per-object
    /// compact test still routes), for good: across later calls, safepoints
    /// and relocations. The epoch only grows, so a failed guard never needs
    /// re-asking; it left for the interpreter.
    ///
    /// The per-object compact TEST is not elided: it is a property of the
    /// object, not of the epoch, and choosing an arm without it needs a
    /// speculation or a versioned copy of the code (FG16-2).
    pub(in crate::ir_lower) fn emit_field_layout_guard(
        &mut self,
        base: NodeId,
        pc: usize,
        may_deopt: bool,
    ) -> Option<usize> {
        if !self.layout_guard_deopt_on {
            return self.emit_layout_epoch_guard();
        }
        let Some(block) = self.field_proof_block() else {
            return self.emit_layout_epoch_guard();
        };
        if self.epoch_proof_holds(block) {
            if crate::ir_stage_reporting() {
                eprintln!("[ir] layout-guard elided n{base} at pc {pc} (block {block})");
            }
            return None;
        }
        if self.field_site_may_trap(pc, may_deopt) {
            if !self.emit_layout_epoch_cmp() {
                // No epoch cell: the rejoining form emits nothing either.
                return None;
            }
            // ZF=1 exactly when the epoch still matches: continue on JE.
            self.emit_deopt_unless(0x84, pc, DeoptReason::ClassLoading);
            self.layout_proven.push((base, block));
            // Entry references existed before this epoch read; references loaded later may not.
            // Reuse the existing per-value scope/OSR rules, without asserting compactness.
            for (id, node) in self.graph.nodes.iter().enumerate() {
                if node.ty == IrType::Ref && matches!(node.op, Op::Param(_)) {
                    let proof = (id as NodeId, block);
                    if !self.layout_proven.contains(&proof) {
                        self.layout_proven.push(proof);
                    }
                }
            }
            if crate::ir_stage_reporting() {
                eprintln!("[ir] layout-guard deopting n{base} at pc {pc} (block {block})");
            }
            return None;
        }
        self.emit_layout_epoch_guard()
    }

    /// The block of the node being lowered, which the field-guard proofs
    /// ([`Self::layout_proven`], [`Self::compact_proven`]) are keyed by.
    pub(in crate::ir_lower) fn field_proof_block(&self) -> Option<usize> {
        self.cur_def
            .and_then(|d| self.schedule.node_to_block.get(d as usize).copied())
    }

    /// Does a proof in `proofs` -- a deopting guard on `base`, recorded with
    /// its block -- cover a site in `block`? (Round 16, FG16-1; factored out
    /// in JIT round 25 for the compactness proofs, item 4.)
    ///
    /// Same block: the guard was emitted earlier in it, and a block runs
    /// straight through. Another block: only in a LOOP-FREE body
    /// ([`Self::layout_guard_cross_block`]) whose guard block dominates this
    /// one -- see [`Self::emit_field_layout_guard`] for why loop-free.
    ///
    /// JIT round 25 adds one condition to the cross-block arm: the body has
    /// no compiled exception entry (no call-site exception pad target,
    /// `call_throw_entry_blocks`, and no compiled handler arm,
    /// `Graph::catch_arm_sites`). An exception edge leaves its block from the
    /// middle -- possibly from a node BEFORE the guard -- so a handler block
    /// the guard's block dominates on the CFG can be reached without the
    /// guard having run. Whether the schedule's dominator tree counts those
    /// edges at all is not something this rule should depend on, so the
    /// cross-block arm is simply refused when any exist.
    ///
    /// `compact` names which proof list `proofs` is (the compactness proofs,
    /// else the layout-epoch ones): what an OSR entry that jumps past the
    /// guard must re-check (item 4 residual 2, [`Self::field_proof_reaches`]).
    pub(in crate::ir_lower) fn field_proof_holds(
        &self,
        proofs: &[(NodeId, usize)],
        base: NodeId,
        block: usize,
        compact: bool,
    ) -> bool {
        // Item 4 wave 2: the dominance rule, in every body.
        if self.proven_scope_on {
            // Residual 2: modulo a redundant φ-web (`field_proof_canon`), so
            // a loop's invariant header φ of the receiver reads the proof put
            // down on the receiver before the loop.
            let base = self.field_proof_canon(base);
            return proofs.iter().any(|&(r, b)| {
                self.field_proof_canon(r) == base
                    && (b == block || self.field_proof_reaches(base, b, block, compact))
            });
        }
        let cross_block = self.layout_guard_cross_block
            && self.call_throw_entry_blocks.is_empty()
            && self.graph.catch_arm_sites.is_empty();
        proofs.iter().any(|&(r, b)| {
            r == base
                && (b == block
                    || (cross_block
                        && self.schedule.dom.is_reachable(b)
                        && self.schedule.dom.dominates(b, block)))
        })
    }

    /// May the inline field site at bytecode `pc` leave for the interpreter
    /// here, re-executing its `getfield` / `putfield`? `may_deopt` is the
    /// caller's statement that the deopt prediction already counts this node.
    /// The rule of [`Self::emit_field_layout_guard`]'s deopting form: the pc
    /// is lone (no other effectful node shares it), in the compiling method's
    /// own code, and the body is not self-locking.
    pub(in crate::ir_lower) fn field_site_may_trap(&self, pc: usize, may_deopt: bool) -> bool {
        if !may_deopt {
            return false;
        }
        // JIT round 17 wave 3 (lane irlower): under the opt-in per-frame rule,
        // the owner count is per `(pc, own frame)`, so a copy that names its
        // own frame is lone in its copy. See `layout_guard_lone_sites`.
        let (lone, own) = match self.layout_guard_lone_sites.as_ref() {
            Some(sites) => {
                let graph = self.graph;
                let own = self.cur_node_frame.filter(|&si| {
                    graph
                        .safepoints
                        .get(si as usize)
                        .is_some_and(|sp| sp.bci == pc)
                });
                (sites.contains(&(pc, own)), own)
            }
            None => (self.layout_guard_lone_pcs.contains(&pc), None),
        };
        // Item 4 wave 2: a spliced site may trap when its guard publishes a
        // chain that resumes the callee at the field's OWN bci, which
        // re-executes nothing the splice committed.
        let site_ok = if self.pc_is_in_a_spliced_body(pc) {
            self.proven_spliced_on && self.field_site_chain_trap_ok(pc)
        } else {
            self.flat_site_frame_located(pc)
        };
        if !site_ok || self.graph.method_self_lock.is_some() {
            return false;
        }
        lone || (!self.pc_is_in_a_spliced_body(pc)
            && self.field_site_no_preceding_side_effects(pc, own))
    }

    /// JIT r21 LOOPS21-3 close: would a deopt at the compiling method's own
    /// bci `pc`, emitted while lowering the current node, find a location for
    /// every value of the frame it publishes ([`Self::resolve_frame_state_for_site`]'s
    /// choice)? Asked before a field guard takes its DEOPTING form
    /// ([`Self::field_site_may_trap`]); a `false` keeps the rejoining forms,
    /// which consult no frame and are always sound.
    ///
    /// The case it exists for: LICM moved `this.k` of `a.v += this.k` to the
    /// loop's pre-header with the flat snapshot of its in-loop bci, which
    /// names the iteration's `a.v` (an operand-stack temporary the pre-header
    /// has not computed). Since `CRATONVM_JIT_IR_PROVEN_RECEIVER_FIELDS` (JIT
    /// round 25, default ON) made the compactness test and the layout-epoch
    /// compare deopt, describing that frame latched `UnallocatedValue` and
    /// refused the whole compile (`R20PLoopStaticBarrier.run` stayed on its C1
    /// body, 8.9 us against 3.9 us). The guard of a site that runs once per
    /// loop entry costs nothing in its rejoining form.
    pub(in crate::ir_lower) fn flat_site_frame_located(&self, pc: usize) -> bool {
        let graph = self.graph;
        let si = match self
            .cur_node_frame
            .filter(|&si| graph.safepoints.get(si as usize).is_some_and(|sp| sp.bci == pc))
        {
            Some(si) => si as usize,
            None => match self.safepoint_first_by_bci.get(&pc) {
                Some(&si) => si,
                // No frame at all: the deopt's own refusal path answers.
                None => return true,
            },
        };
        let Some(sp) = graph.safepoints.get(si) else {
            return true;
        };
        sp.locals
            .iter()
            .chain(sp.stack.iter())
            .chain(sp.monitors.iter())
            .all(|&v| self.frame_value_located(v) && self.frame_value_defined_at_site(v))
    }

    /// [`Self::frame_value_for`]'s answer to "is there a location", asked
    /// without describing anything or latching a bailout: `false` exactly
    /// where that function would answer `Unsupported` (or latch).
    pub(in crate::ir_lower) fn frame_value_located(&self, node_id: NodeId) -> bool {
        if node_id == NO_NODE {
            return true;
        }
        let Some(node) = self.graph.nodes.get(node_id as usize) else {
            return false;
        };
        let flag = |v: &[bool]| v.get(node_id as usize).copied().unwrap_or(false);
        if flag(&self.fused_lcmp)
            || flag(&self.fused_fcmp)
            || flag(&self.fused_cmp_unnamed)
            || flag(&self.fused_test_and)
        {
            return false;
        }
        let param_not_yet_published =
            matches!(node.op, Op::Param(_)) && !flag(&self.defined_nodes);
        if !matches!(node.op, Op::Const(_) | Op::ConstF(_)) && !param_not_yet_published {
            if self.register_frame_value(node_id, node.ty).is_some() {
                return true;
            }
            if flag(&self.site_named) || flag(&self.home_dropped) {
                return false;
            }
        }
        match node.op {
            Op::Const(_) | Op::ConstF(_) | Op::Param(_) | Op::Dead => true,
            _ if !self.snapshot_ranged_home_ok(node_id) => false,
            _ => {
                self.node_slot
                    .get(node_id as usize)
                    .copied()
                    .flatten()
                    .is_some()
                    || self.dominating_unemitted_slot(node_id).is_some()
            }
        }
    }

    /// Item 4 wave 2 (`CRATONVM_JIT_IR_PROVEN_RECEIVER_FIELDS_SPLICED`): would a
    /// guard at the spliced raw pc `pc` publish a chain snapshot -- the
    /// callee's frame at `pc` with its callers parked mid-`invoke` -- rather
    /// than the flat frame at the outermost `invoke`? The flat frame re-runs
    /// the whole splice from its call, replaying any store the splice already
    /// committed (the FG16-1 objection); the chain re-executes only the
    /// `getfield` / `putfield`, exactly as the splice's own null and bounds
    /// guards do. Asked with the same three readers
    /// [`Self::splice_chain_for_site`] consults, before anything is emitted:
    /// no synchronized-window flat site, a chain snapshot at `pc`, and one
    /// that can be published (every value it names has a location).
    pub(in crate::ir_lower) fn field_site_chain_trap_ok(&self, pc: usize) -> bool {
        if self.sync_window_site_publishes_no_chain() {
            return false;
        }
        match self.splice_chain_snapshot_for_site(pc) {
            Some((si, _)) => {
                self.snapshot_defined_at_site(si) && self.splice_chain_frame_state(si).is_ok()
            }
            None => false,
        }
    }

    /// Item 4 wave 2 (`CRATONVM_JIT_IR_PROVEN_RECEIVER_FIELDS_DOMINANCE`): may
    /// a field proof on `base` recorded in block `b` be read in another block
    /// `block`? Sound when every way control can arrive in `block` with this
    /// activation's value of `base` has run the guard first:
    ///
    /// 1. `b` dominates `block` (both reachable) -- every path from the
    ///    method entry passes `b`, and the guard runs with `b`;
    /// 2. no other ENTRY reaches `block` without entering `b`
    ///    ([`Self::field_proof_entry_blocks`]: OSR entry candidates and
    ///    exception pad targets, all of which enter a block past its
    ///    dominators, the pads from the middle of a block that may sit before
    ///    the guard). An entry AT `b` runs `b` whole, guard included;
    /// 3. `base` is defined at most once per activation: its block lies on no
    ///    cycle and no exception entry leads back to it
    ///    ([`Self::field_proof_base_runs_once`]). A value redefined each trip
    ///    is a different object under one node id; for such a value a proof is
    ///    read only where its definition's block dominates `b` (the guard then
    ///    ran on this trip, after the latest definition);
    /// 4. no compiled handler arm (`Graph::catch_arm_sites`): its arm blocks
    ///    are only known at probe time.
    ///
    /// Neither proof is ever invalidated once it holds (see
    /// [`Self::proven_receiver_mode`]), so these are the whole conditions.
    ///
    /// # Item 4 residual 2: proofs from before a loop
    ///
    /// Every loop header is an ORDINARY OSR entry candidate, so rule 2 alone
    /// kept a proof put down before a loop from ever covering its body. Such
    /// an entry is admitted when its stub can do what the skipped guard did:
    /// the header snapshot names `base` in a local (the interpreter's value
    /// of it there is the one object `base` names in this activation, rule
    /// 3, and the seeding makes it `base`'s home), so the stub re-reads that
    /// local and re-checks the receiver's compactness -- or, for a layout
    /// proof, the replacement epoch -- and leaves for the interpreter at the
    /// header when it fails, before anything of the body runs
    /// ([`Self::osr_entry_field_proof_checks`]). The obligation is recorded
    /// here ([`Self::field_proof_osr_obligations`]); an entry whose stub
    /// cannot meet it is refused, which is always sound. Opaque OSR arms and
    /// exception pads still refuse the cross-block read.
    ///
    /// # Item 4 residual 4: compiled handler arms
    ///
    /// Rule 4 is lifted under `CRATONVM_JIT_IR_PROVEN_RECEIVER_FIELDS_CATCH_ARMS`.
    /// An arm block is the `Proj(0)` of its call block's `If(CatchEntryFlag)`
    /// and has that block as its ONLY predecessor (`catch_arm_leg`); the probe
    /// enters it from the call's exceptional exit, and the call is the last
    /// node of its block when the arm is wired (`TailAfterCall` refuses any
    /// other). So every guard of the call's block, and of each block that
    /// dominates it, ran before the arm is entered, exactly as on the CFG
    /// edge the dominator tree already counts; a ladder's link blocks the
    /// probe jumps past hold only their flags, no guard. An arm that is not
    /// wired is never entered by compiled code at all.
    pub(in crate::ir_lower) fn field_proof_reaches(&self, base: NodeId, b: usize, block: usize, compact: bool) -> bool {
        if !self.graph.catch_arm_sites.is_empty() && !self.proven_catch_arms_on {
            return false;
        }
        let dom = &self.schedule.dom;
        if !(dom.is_reachable(b) && dom.is_reachable(block) && dom.dominates(b, block)) {
            return false;
        }
        // A `New` is laid out by this body's own baked allocation on every
        // trip (its lowering records both proofs), so the once-per-activation
        // rule guarding a TESTED object does not apply to it.
        let allocated = self
            .graph
            .nodes
            .get(base as usize)
            .is_some_and(|n| matches!(n.op, Op::New { .. }));
        // A value redefined each trip is covered too when its definition's
        // block dominates `b`: a path from its LATEST definition to `block`
        // that skipped `b`, prefixed by an entry path to that definition's
        // first visit (which holds no `b`), would skip `b` from the method
        // entry. Paths that come in through an entry instead are rule 2's.
        let def_dominates_guard = || {
            let Some(n) = self.graph.nodes.get(base as usize) else {
                return false;
            };
            let def = if n.op == Op::Phi {
                n.inputs.first().and_then(|&m| self.block_of_ctrl(m))
            } else {
                self.schedule.node_to_block.get(base as usize).copied()
            };
            def.is_some_and(|d| dom.is_reachable(d) && dom.dominates(d, b))
        };
        if !allocated && !self.field_proof_base_runs_once(base) && !def_dominates_guard() {
            return false;
        }
        let cached = self.field_proof_scope_cache.borrow().get(&(b, block)).cloned();
        let bypass = match cached {
            Some(v) => v,
            None => {
                let mut bypass: Option<Vec<usize>> = Some(Vec::new());
                for &e in self.field_proof_entry_blocks() {
                    if e == b
                        || !Self::osr_entry_can_reach(self.schedule, e, Some(b), block, true)
                    {
                        continue;
                    }
                    if !self.field_proof_entry_is_rechecked_osr_header(e) {
                        bypass = None;
                        break;
                    }
                    if let Some(list) = bypass.as_mut() {
                        list.push(e);
                    }
                }
                self.field_proof_scope_cache
                    .borrow_mut()
                    .insert((b, block), bypass.clone());
                bypass
            }
        };
        let Some(bypass) = bypass else {
            return false;
        };
        if bypass.is_empty() {
            return true;
        }
        if !self.proven_preheader_on
            || !bypass
                .iter()
                .all(|&e| self.osr_entry_local_naming(e, base).is_some())
        {
            return false;
        }
        let mut obligations = self.field_proof_osr_obligations.borrow_mut();
        for e in bypass {
            if !obligations.contains(&(e, base, compact)) {
                obligations.push((e, base, compact));
            }
        }
        true
    }

    /// Item 4 residual 2: `v` with every REDUNDANT φ-web looked through: when
    /// the φs reachable from `v` through φ inputs (bounded) have exactly one
    /// input that is not one of them, every φ of the web holds that value on
    /// every execution (each is assigned only it or another of the web), so
    /// a proof about it is a proof about them. This is the loop-invariant
    /// header φ the builder gives every local (`φ(this, itself)`, and through
    /// nested loops `φ(φ_outer, itself)` with `φ_outer = φ(this, φ_inner)`).
    /// Any other `v` answers itself. `OsrLocal` and every non-φ input are
    /// leaves, so an opaque OSR merge's `φ(param, OsrLocal)` stays itself.
    pub(in crate::ir_lower) fn field_proof_canon(&self, v: NodeId) -> NodeId {
        if let Some(&c) = self.field_proof_canon_cache.borrow().get(&v) {
            return c;
        }
        let canon = (|| {
            let is_phi = |x: NodeId| {
                self.graph
                    .nodes
                    .get(x as usize)
                    .is_some_and(|n| n.op == Op::Phi)
            };
            if !is_phi(v) {
                return v;
            }
            let mut web: Vec<NodeId> = vec![v];
            let mut leaf: Option<NodeId> = None;
            let mut i = 0;
            while i < web.len() {
                let Some(n) = self.graph.nodes.get(web[i] as usize) else {
                    return v;
                };
                for &x in n.inputs.iter().skip(1) {
                    if x == NO_NODE {
                        return v;
                    }
                    if web.contains(&x) {
                        continue;
                    }
                    if is_phi(x) {
                        if web.len() >= FIELD_PROOF_CANON_MAX_WEB {
                            return v;
                        }
                        web.push(x);
                        continue;
                    }
                    match leaf {
                        None => leaf = Some(x),
                        Some(l) if l == x => {}
                        Some(_) => return v,
                    }
                }
                i += 1;
            }
            leaf.unwrap_or(v)
        })();
        self.field_proof_canon_cache.borrow_mut().insert(v, canon);
        canon
    }

}
