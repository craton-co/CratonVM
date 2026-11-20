#[allow(unused_imports)]
use super::*;

impl IrBuilder {
    fn attach_sb_virtual_builders(&self, sp: &mut SafepointSnapshot) {
        use crate::sb_chain_fuse::SbChainPart;
        let Some(active) = &self.sb_chain_active else {
            return;
        };
        if !active.has_call || sp.bci <= active.new_pc {
            return;
        }
        let Some(fusion) = self.sb_chain_fusions.get(&active.new_pc) else {
            return;
        };
        let local_slot = fusion
            .chain
            .local
            .as_ref()
            .map(|l| l.slot as usize)
            .filter(|&slot| sp.locals.get(slot) == Some(&active.placeholder));
        let stack_slot = active
            .base_len
            .checked_sub(1)
            .filter(|&slot| sp.stack.get(slot) == Some(&active.placeholder));
        if local_slot.is_none() && stack_slot.is_none() {
            return;
        }
        let mut parts = Vec::new();
        if let Some(lit) = &fusion.chain.constructor_literal {
            if !lit.is_empty() {
                parts.push(SbVirtualPart::Literal { text: lit.clone() });
            }
        }
        let mut args_iter = active.args.iter().copied();
        for part in fusion.chain.parts.iter().take(active.next_part) {
            match part {
                SbChainPart::Literal { text, .. } => {
                    parts.push(SbVirtualPart::Literal { text: text.clone() });
                }
                SbChainPart::Value { kind, .. } => {
                    let Some(val) = args_iter.next() else {
                        return;
                    };
                    parts.push(SbVirtualPart::Value { kind: *kind, val });
                }
            }
        }
        sp.sb_virtual_builders.push(SbVirtualBuilder {
            id: sp.sb_virtual_builders.len() as u32,
            local_slot,
            stack_slot,
            parts,
        });
    }

    fn removed_pruned_edges_for_block_walk(
        &self,
        verified: &cratonvm_reader::VerifiedCode,
        code_len: usize,
    ) -> Vec<(usize, usize)> {
        let mut removed = Vec::new();
        for insn in verified.instructions() {
            let pc = insn.pc as usize;
            if pc >= code_len {
                break;
            }
            let next_pc = insn.next_pc as usize;
            if self.pruned_always_taken.contains(&pc)
                && next_pc < code_len
                && verified
                    .successors(pc)
                    .iter()
                    .any(|&s| s > pc && s != next_pc)
            {
                removed.push((pc, next_pc));
            }
            if self.pruned_never_taken.contains(&pc) {
                for succ in verified.successors(pc) {
                    if succ > next_pc && succ < code_len {
                        removed.push((pc, succ));
                    }
                }
            }
        }
        removed
    }

    // ── Main bytecode→IR conversion ──────────────────────────────────

    /// Convert bytecode to IR graph.  Returns `None` if an unsupported
    /// opcode is encountered.
    pub fn build(mut self, code: &[u8], code_len: usize) -> Option<Graph> {
        // Per-build, per-thread: `lib.rs` reads it back the moment this
        // returns, to install the compact-field rows the String-access
        // expansion's two loads need. See `string_access_site_pcs`.
        reset_string_access_sites();
        reset_site_traps_this_build();
        set_splice_resume_map_this_build(None);
        // Bound the frame-state memory before any of it is allocated (round 11
        // wave 3); see `IR_MAX_FRAME_SNAPSHOT_WORDS`. `code.len()` rather than
        // `code_len`: the walk also covers every spliced body in the buffer.
        if self.locals.len().saturating_mul(code.len()) > IR_MAX_FRAME_SNAPSHOT_WORDS {
            return ir_build_bail(line!(), 0);
        }
        // Consume the verifier's canonical decode/CFG contract instead of
        // maintaining a second opcode-length scanner in the compiler.
        let verified = cratonvm_reader::verified_code(code.get(..code_len)?).ok()?;
        sparse_switch::prepare_build_inline_sites(&mut self, code, code_len, &verified);
        if self.vthread_guard.is_some() {
            self.graph.vthread_boundaries = Some(
                verified
                    .instructions()
                    .iter()
                    .filter(|insn| matches!(code[insn.pc as usize], 0xb6..=0xba | 0xc2))
                    .map(|insn| insn.pc)
                    .collect(),
            );
        }
        // Round 14 wave 5 (lane sync5, SS8-3): see the field.
        if self.graph.method_monitor_param == Some(0) && !body_writes_local_zero(code, code_len) {
            self.method_monitor_slot0_loops_below = Some(code_len);
        }
        // Round 12 wave 4 (lane iropt3): the whole-method half of
        // `exact_receiver_proof`, over the CALLER's code only (a spliced body's
        // locals are its own frame's). Computed only when a site needs it.
        if !self.exact_receiver_sites.is_empty() {
            self.exact_receiver_single_store_slots =
                single_astore_local_slots(code, code_len, self.param_slot_count);
        }
        // `CRATONVM_JIT_IR_OSR_PROFILED_SLOT_CHECK`: written once in the whole
        // method, by an `astore`, and not a parameter -- the bytecode half.
        if !self.osr_profiled_slot_classes.is_empty() {
            let single = single_astore_local_slots(code, code_len, self.param_slot_count);
            self.osr_profiled_slot_classes
                .retain(|k, _| single.contains(k));
        }
        // al16: the loop half of the flow proof (`exact_receiver_flow`).
        if !self.exact_receiver_sites.is_empty() && ir_exact_receiver_flow_enabled() {
            self.loop_written_slots = loop_written_local_slots(code, code_len);
        }
        // Round 13 wave 10 (lane iropt8, HS-2): the per-body half of the
        // nested exact-receiver proof (`exact_receiver_node_proof`).
        if !self.exact_receiver_sites.is_empty() && crate::ir_splice_nested_exact_receiver_enabled()
        {
            self.splice_bases_keeping_this = self
                .inline_sites
                .values()
                .filter(|s| {
                    // A static body's argument 0 too (`arg0` of its frame).
                    (s.receiver_is_arg0
                        || (s.num_args > 0 && ir_static_final_exact_class_enabled()))
                        && s.base
                            .checked_add(s.code_len)
                            .is_some_and(|end| end <= code.len())
                        && !body_writes_local_zero(code.get(s.base..).unwrap_or(&[]), s.code_len)
                })
                .map(|s| s.base)
                .collect();
            self.splice_bases_written_locals = self
                .inline_sites
                .values()
                .filter_map(|s| {
                    let end = s.base.checked_add(s.code_len)?;
                    (end <= code.len()).then(|| {
                        (
                            s.base,
                            body_written_locals_mask(code.get(s.base..).unwrap_or(&[]), s.code_len),
                        )
                    })
                })
                .collect();
        }
        // `CRATONVM_FROZEN_PAGES`, stage 2: see the field.
        if self.frozen_facts.is_some() {
            let mut keeps: HashSet<usize> = self
                .inline_sites
                .values()
                .filter(|s| {
                    s.receiver_is_arg0
                        && s.base
                            .checked_add(s.code_len)
                            .is_some_and(|end| end <= code.len())
                        && !body_writes_local_zero(code.get(s.base..).unwrap_or(&[]), s.code_len)
                })
                .map(|s| s.base)
                .collect();
            if !body_writes_local_zero(code, code_len) {
                keeps.insert(0);
            }
            self.frozen_keeps_this = keeps;
        }
        // Round 12 wave 5 (lane iropt4): the call-fence relaxations, read once
        // per build and only for a build that splices (`SpliceFenceRelax`).
        if !self.inline_sites.is_empty() {
            self.splice_fence_relax = SpliceFenceRelax::from_flags();
            self.splice_store_fence_forward_branch = self.splice_fence_relax.forward_branch
                && ir_splice_store_fence_forward_branch_enabled();
            // Round 13 wave 3 (lane framestate): chain snapshots inside the
            // spliced bodies. Pointless without the compiling method's key,
            // which names the chain's outermost scope.
            self.splice_frame_states =
                ir_splice_frame_states_enabled() && !self.graph.scope_method_key.is_empty();
            self.splice_chain_fences = self.splice_frame_states && ir_splice_chain_fences_enabled();
            // JIT round 19 (lane splice19, CH16-4).
            self.chain_snapshots_lazy =
                self.splice_frame_states && ir_chain_snapshots_lazy_enabled();
            // Round 16 wave 7 (lane arraylist, AL16-1): the cold throw arms the
            // resolver masked in the spliced bodies become traps.
            self.inline_cold_athrow = ir_inline_cold_athrow_enabled();
            // Interpreter round i1 wave 47, lane L3: the post-call chains,
            // which need the same outermost identity.
            self.splice_call_states =
                ir_post_call_splice_exits_enabled() && !self.graph.scope_method_key.is_empty();
        }

        // STUB-S8 fix: the bytecode of a `catch`/`finally` handler is reachable
        // ONLY through an exception edge, and a JIT frame never takes one — an
        // exception makes the compiled body return the `i64::MIN` sentinel and
        // the runtime re-runs (or precisely resumes) the method in the
        // interpreter, which is the only thing that consults the exception
        // table (see `route_implicit_exception_through_callee` /
        // `callee_has_exception_table` in vm/src/jit/helpers.rs). So handler
        // bodies are dead code in the compiled body and must be SKIPPED, not
        // walked.
        //
        // Walking them was the whole reason the optimizing tier refused every
        // method with an exception table: the linear pc walk fell into the
        // handler carrying whatever `ctrl`/`locals`/`stack` the *textually*
        // preceding code left behind, and a handler's leading `astore` of the
        // (never-modelled) exception object popped an empty stack, so the
        // builder emitted orphan nodes referencing `NO_NODE` that the
        // scheduler/lowerer still visited — panicking in `ir_lower::slot_of`
        // on a `u32::MAX` slot index.
        //
        // Skipping is safe for the code AFTER a handler too: any instruction
        // that follows unreachable code and is itself reachable can only be
        // entered by a branch (fall-through from unreachable code is not a real
        // predecessor), so it is necessarily a merge target and its state is
        // restored by `activate_merge` rather than inherited. The `ctrl ==
        // NO_NODE` bail inside the loop enforces exactly that invariant.
        //
        // Round nio21 wave 1 (lane L2): except the handler bodies a compiled
        // local throw edge may enter (`plan_local_throw_reachability`; only
        // with `set_local_throw_rows` called, else this is the set above).
        let reachable = {
            let base = normally_reachable_pcs(&verified, code_len);
            self.plan_local_throw_reachability(&verified, base, code, code_len)
        };

        // Only reachable targets get a merge node. A `merge_targets` entry that
        // lies inside a skipped handler (or that only handler code branches to)
        // would otherwise leave an `Op::Merge` with zero control inputs in the
        // graph — never activated, because the walk never arrives there — and
        // an input-less control node is exactly the kind of orphan the
        // scheduler/lowerer is not prepared for.
        for &target in verified.merge_targets() {
            let target = target as usize;
            if reachable.contains(&target) {
                self.ensure_merge(target);
            }
        }
        self.loop_headers = verified
            .loop_headers()
            .iter()
            .map(|target| *target as usize)
            .filter(|target| reachable.contains(target))
            .collect();

        // ── Bottom-tested (rotated) loops ────────────────────────────
        //
        // ECJ, Kotlin, Scala and several bytecode generators rotate a loop:
        // `goto test; body: …; test: if<cond> body`. In pc order the body, a
        // loop header by the verifier's "target of a backward branch"
        // definition, comes before every one of its predecessors. The pc walk
        // below reaches it with no control token, `activate_loop_header` has
        // no entry edge to seed its phis from, and the safety net refused the
        // whole method.
        //
        // Such a method is walked in reverse post-order of its block CFG
        // instead (see `BlockWalk`). A block is then visited after every
        // forward predecessor, the test block before the body it dominates,
        // and the loop headers are the targets of the edges that are
        // retreating in THAT order. Every method the pc walk already handles
        // keeps the pc walk, node for node.
        // An IRREDUCIBLE cycle (entered at two points) is built like any other
        // since round 11 wave 5. Rounds 11 wave 3-4 refused it here because the
        // loop passes after the builder classified back edges by forward
        // reachability, which reads a two-entry cycle as a natural loop with
        // one pre-header; `ir_optimize::loop_headers` and `unroll` now also
        // require the header to dominate every back-edge source
        // (`back_edges_dominated_by`) and decline such a cycle. Both walks
        // build valid SSA on any CFG: a retreating edge's target is a
        // registered header whose φs are seeded from every predecessor walked
        // before it and back-patched from the rest (see `BlockWalk` and
        // `patch_loop_backedge`). Kotlin `invokeSuspend` state machines with a
        // suspension point inside a loop are the population this returns to
        // the optimizing tier
        // (`r11w3-ircore-irreducible-cfgs-reached-natural-loop-passes`).
        let pruned_bw_state: Option<(HashSet<usize>, HashSet<usize>)> =
            if self.pruned_always_taken.is_empty() && self.pruned_never_taken.is_empty() {
                None
            } else {
                let removed = self.removed_pruned_edges_for_block_walk(&verified, code_len);
                if removed.is_empty() {
                    None
                } else {
                    let base = reachable_pcs_without_edges(&verified, code_len, &removed);
                    let r = self.plan_local_throw_reachability(&verified, base, code, code_len);
                    let lh = self
                        .loop_headers
                        .iter()
                        .copied()
                        .filter(|h| r.contains(h))
                        .collect();
                    Some((r, lh))
                }
            };
        let (bw_reachable, bw_loop_headers) = match &pruned_bw_state {
            Some((r, lh)) => (r, lh),
            None => (&reachable, &self.loop_headers),
        };
        let block_walk: Option<BlockWalk> =
            if has_rotated_loop_header(&verified, bw_reachable, bw_loop_headers, code_len) {
                match BlockWalk::reverse_postorder(&verified, &reachable, code_len) {
                    Some(walk) => Some(walk),
                    None => return ir_build_bail(line!(), 0),
                }
            } else {
                None
            };

        // ── The same pre-scan, for every SPLICED body ────────────────
        //
        // `verified_code` above analysed `code[..code_len]` — the caller alone.
        // A relocated callee body lives past `code_len` in the combined buffer
        // and has its own branches, its own merge targets and its own loop
        // headers, none of which that analysis can see. That gap, and only that
        // gap, is why the splice scanner refused every callee containing an
        // `if` or a `goto` and called itself "straight-line only, v1".
        //
        // Closing it needs no new analysis, because a callee body IS a valid
        // method body: run the SAME verifier over it and rebase what comes out
        // by the body's `base`. Branch offsets need no rebasing at all — they
        // are relative, the body is copied contiguously, and a branch inside it
        // therefore lands inside it in combined coordinates by construction.
        //
        // Reachability needs no merging either: the walk's skip is already
        // scoped to `self.splice.is_empty()` (see the `!reachable.contains`
        // test below) precisely because a relocated body is unreachable from
        // pc 0. What a body's own reachability is used for here is the same
        // thing it is used for above — refusing to create an `Op::Merge` for a
        // target only handler code can reach, which would leave an input-less
        // control node in the graph.
        //
        // A `return` anywhere but the last instruction is still refused by the
        // scanner (`ir-splice-not-single-trailing-return`); several returns
        // that all funnel to a trailing one are admitted under
        // `ir_splice_multi_return_enabled`, or as a bounded self-recursive
        // copy (`ir_recursive_inline_enabled`), and the loop below is where
        // the walk learns which bodies those are.
        if ir_splice_branch_enabled() {
            let bodies: Vec<(usize, usize)> = self
                .inline_sites
                .values()
                .map(|s| (s.base, s.code_len))
                .collect();
            for (base, body_len) in bodies {
                let Some(body) = code.get(base..base.saturating_add(body_len)) else {
                    return ir_build_bail(line!(), base);
                };
                let Ok(body_verified) = cratonvm_reader::verified_code(body) else {
                    // The body did not verify on its own. Refuse the METHOD
                    // rather than walk a region whose control flow nothing has
                    // analysed — the failure mode that produces is orphan nodes
                    // referencing `NO_NODE`, which is what STUB-S8 was.
                    return ir_build_bail(line!(), base);
                };
                let body_always: HashSet<usize> = self
                    .splice_always_taken
                    .iter()
                    .filter_map(|&p| p.checked_sub(base).filter(|&lp| lp < body_len))
                    .collect();
                let body_never: HashSet<usize> = self
                    .splice_never_taken
                    .iter()
                    .filter_map(|&p| p.checked_sub(base).filter(|&lp| lp < body_len))
                    .collect();
                let body_reachable = if body_always.is_empty() && body_never.is_empty() {
                    normally_reachable_pcs(&body_verified, body_len)
                } else {
                    reachable_pcs_without_pruned_branches(body, body_len, &body_always, &body_never)
                        .unwrap_or_else(|| normally_reachable_pcs(&body_verified, body_len))
                };
                // nio21 wave 4 (lane L2): a body admitted with its handlers
                // (`ir_splice_callee_handlers_enabled`). Each handler gets a
                // merge the call-site edges enter, the masked handler code is
                // stepped over with dead control, and the body's covered
                // invokes are planned exactly as the compiling method's own
                // (`call_site_throw_plan`), in combined pcs.
                if let Some(rows) = self.splice_handler_rows.get(&base).cloned() {
                    if !self.splice_handler_edges {
                        return ir_build_bail(line!(), base);
                    }
                    let mut local_rows: Vec<IrLocalThrowRow> = Vec::with_capacity(rows.len());
                    for r in &rows {
                        let (Some(s), Some(e), Some(h)) = (
                            r.start_pc.checked_sub(base),
                            r.end_pc.checked_sub(base),
                            r.handler_pc.checked_sub(base),
                        ) else {
                            return ir_build_bail(line!(), base);
                        };
                        if h >= body_len || body_reachable.contains(&h) {
                            return ir_build_bail(line!(), base + h);
                        }
                        local_rows.push(IrLocalThrowRow {
                            start_pc: s,
                            end_pc: e,
                            handler_pc: h,
                            catch: IrCatchClass::Unknown,
                        });
                    }
                    for insn in body_verified.instructions() {
                        let p = insn.pc as usize;
                        if p < body_len && !body_reachable.contains(&p) {
                            self.splice_handler_dead.insert(base + p);
                        }
                    }
                    for r in &local_rows {
                        self.ensure_merge(base + r.handler_pc);
                        self.splice_handler_entries.insert(base + r.handler_pc);
                    }
                    let plan = call_site_throw_plan(
                        &body_verified,
                        &body_reachable,
                        body,
                        body_len,
                        &local_rows,
                    );
                    for (p, handlers) in plan {
                        self.call_throw_planned
                            .insert(base + p, handlers.into_iter().map(|h| base + h).collect());
                    }
                }
                for &target in body_verified.merge_targets() {
                    let target = target as usize;
                    if body_reachable.contains(&target) {
                        self.ensure_merge(base + target);
                    }
                }
                let body_headers: HashSet<usize> = body_verified
                    .loop_headers()
                    .iter()
                    .map(|&h| h as usize)
                    .filter(|h| body_reachable.contains(h))
                    .collect();
                // A body with a rotated loop gets its own block walk, for the
                // same reason the caller does: walked in pc order, its loop
                // body has no forward entry when the walk reaches it, the
                // safety net fires, and the CALLER's whole build is refused.
                // Its loop headers are then the walk's (the test blocks).
                if has_rotated_loop_header(&body_verified, &body_reachable, &body_headers, body_len)
                {
                    let Some(walk) =
                        BlockWalk::reverse_postorder(&body_verified, &body_reachable, body_len)
                    else {
                        return ir_build_bail(line!(), base);
                    };
                    self.loop_headers
                        .extend(walk.headers.iter().map(|&h| base + h));
                    self.splice_block_walks.insert(
                        base,
                        walk.ranges
                            .iter()
                            .map(|&(s, e)| (base + s, base + e))
                            .collect(),
                    );
                } else {
                    self.loop_headers
                        .extend(body_headers.iter().map(|&h| base + h));
                }

                // A body with several `return`s: the walk must NOT leave the
                // splice at the first one. Decide it here, from the same
                // verified decode, because the walk meets the returns one at a
                // time and cannot tell the first from the last.
                //
                // Only reachable returns count. An unreachable one is code the
                // walk never enters, and counting it would put the body on the
                // continuation path with one exit edge that never arrives —
                // which `finish_multi_return_splice` would refuse, losing a
                // body the straight-through path handles fine.
                //
                // Unconditional since round 11 wave 11 (lane `calls`): it used
                // to be gated on `ir_splice_multi_return_enabled`, the switch
                // the scanner admits such bodies under, so the two halves had
                // to agree. Whatever admitted the body, walking a multi-return
                // body straight through leaves the splice at its FIRST return
                // (STUB-S8's orphan nodes), so the builder recognises one from
                // its own decode. A single-return body counts one and is
                // unaffected.
                {
                    let mut reachable_returns = 0usize;
                    let mut last_return: Option<usize> = None;
                    let mut last_op: Option<u8> = None;
                    let mut p = 0usize;
                    while p < body_len {
                        let Some(decoded) = body_verified.instruction_at(p) else {
                            return ir_build_bail(line!(), base + p);
                        };
                        if body_reachable.contains(&p) && matches!(body.get(p), Some(0xac..=0xb1)) {
                            reachable_returns += 1;
                            last_return = Some(p);
                        }
                        last_op = body.get(p).copied();
                        p = decoded.next_pc as usize;
                    }
                    // j27 wave 5 (lane DECLOOP): one return followed by a
                    // masked handler run to the end (`ByteBuffer.wrap`): walked
                    // as a multi-return body so the walk reaches the handler's
                    // trap and closes at the body's end.
                    let tail_handler = reachable_returns == 1
                        && last_return.is_some_and(|r| {
                            self.splice_handler_rows.get(&base).is_some_and(|rows| {
                                splice_tail_handler_follows_return(
                                    body,
                                    body_len,
                                    r,
                                    rows.iter().map(|row| row.handler_pc.saturating_sub(base)),
                                )
                            })
                        });
                    // nio21 wave 4 (lane L2): a body that ENDS in a backward
                    // `goto` (admitted with its handlers) closes only as a
                    // multi-return body, at its end, after its last edge.
                    let loop_tail =
                        reachable_returns > 0 && splice_ends_in_backward_goto(body, body_len);
                    if self.splice_handler_rows.contains_key(&base)
                        && !matches!(last_op, Some(0xac..=0xb1))
                        && reachable_returns < 2
                        && !tail_handler
                        && !loop_tail
                    {
                        return ir_build_bail(line!(), base);
                    }
                    if reachable_returns > 1 || tail_handler || loop_tail {
                        self.splice_multi_return.insert(base);
                    }
                }
            }
        }

        // A block walk's loop headers replace the caller's pc-order ones: a
        // rotated body is an ordinary forward merge in reverse post-order, and
        // its test block is the header. A spliced body's headers (at or past
        // `code_len`) are kept, because a relocated body is still walked in pc
        // order.
        let mut range_end = code_len;
        let mut next_range = 0usize;
        if let Some(walk) = block_walk.as_ref() {
            self.loop_headers.retain(|&h| h >= code_len);
            self.loop_headers.extend(walk.headers.iter().copied());
            self.block_walk = true;
            // `BlockWalk::reverse_postorder` returns at least one range, and
            // the first starts at pc 0.
            range_end = walk.ranges[0].1;
            next_range = 1;
        }

        // Round 15 wave 1 (lane sync, SS8-2): a self-locking body's enter, on
        // the entry edge before the first snapshot (`begin_method_self_lock`).
        if !self.begin_method_self_lock(code, code_len) {
            return ir_build_bail(line!(), 0);
        }
        // JIT round 19 wave 5 (lane handler5, SPLICE4-1): which calls get a
        // compiled handler entry, decided against the reachable set and the
        // merges above. Empty unless `set_catch_arm_table` supplied rows.
        self.plan_catch_arms(code, code_len, &verified, &reachable);
        self.plan_pure_splice_sites(code);
        self.root_code_len = code_len;
        self.plan_convert_member_splices(code);
        let mut pc = 0;
        // Where the walk stops, and where it goes next.
        //
        // The pc walk stops at `code_len` with no splice open. That is the
        // loop condition this used to be, `pc < code_len ||
        // !self.splice.is_empty()`: inside a splice `pc` addresses a relocated
        // callee body appended AFTER `code_len`, and the walk must keep going
        // until that body returns.
        //
        // The block walk stops when its ranges run out. At the end of each
        // range, `pc` is the instruction after the range's last one. If control
        // is still live, that instruction falls through into `pc`. The walk
        // visits `pc` somewhere else in its order, so the edge is recorded
        // exactly as the merge activation below records a fall-through: as a
        // predecessor, or as a back edge when `pc` was already visited.
        // Control and the abstract frame are then dropped before the jump, the
        // same as after a `goto`. The next range starts at a merge, whose
        // activation restores them from its predecessors.
        //
        // ── The step budget ──────────────────────────────────────────
        //
        // The walk visits each instruction of the caller and of every spliced
        // body once — `pc` advances monotonically within a range, ranges are
        // disjoint, and each splice frame is entered once — so the number of
        // iterations is bounded by the combined buffer's length. It had no
        // *enforced* bound until 2026-09-17, and the profiled always-taken
        // back-edge prune (see `prune_always_taken_branch`) turned that into a
        // compiler thread that never returned, allocating graph nodes on every
        // pass: `IR_MAX_GRAPH_NODES` is checked only after `build` returns, so
        // nothing downstream could stop it either.
        //
        // The budget below costs one increment and one compare per opcode and
        // converts every future control-flow bookkeeping bug of that shape from
        // "the VM hangs" into "this method took the single-pass backend". The
        // factor is deliberately loose — this is a backstop, not a policy. The
        // walk performs at most one iteration per INSTRUCTION and the shortest
        // instruction is one byte, so `4 * len` is four times the loosest legal
        // walk and no real method can approach it.
        let step_budget = code
            .len()
            .max(code_len)
            .saturating_mul(IR_BUILD_STEPS_PER_CODE_BYTE)
            .saturating_add(64);
        let mut steps = 0usize;
        loop {
            steps += 1;
            // Round 12 wave 6 (lane iropt5, W5-2): a refusal from here to the
            // next instruction belongs to this pc's splice, if it has one.
            self.note_splice_refusal_pc(Some(pc), code_len);
            if steps > step_budget {
                return ir_build_bail(line!(), pc);
            }
            // A helper latched a refusal it had no way to return (a dropped
            // control edge, a second activation of a merge, a φ re-typing that
            // did not converge). Stop at the next instruction boundary, which
            // is the last point the graph is still describable.
            if let Some((site, at)) = self.build_refused {
                self.note_splice_refusal_pc(Some(at), code_len);
                return ir_build_bail(site, at);
            }
            // nio21 wave 4 (lane L2): the instruction just built inside a
            // spliced callee's covered `try` range left the frame only through
            // planted call-site edges or guards.
            if let Some((at, mark)) = self.splice_handler_mark.take() {
                if !self.splice_handler_nodes_armed(mark, at, code) {
                    self.note_splice_refusal_pc(Some(at), code_len);
                    return ir_build_bail(line!(), at);
                }
                // j27 wave 7 (lane T1L1): the proofs, for the lowering.
                self.note_splice_handler_proven_exits(mark);
            }
            if self.splice.is_empty() {
                match block_walk.as_ref() {
                    None => {
                        if pc >= code_len {
                            break;
                        }
                    }
                    Some(walk) => {
                        if pc >= range_end {
                            // An arm moved `pc` past the range's end rather than
                            // onto it. The ranges and the walk disagree about
                            // where instructions end; refuse.
                            if pc != range_end {
                                return ir_build_bail(line!(), pc);
                            }
                            if self.ctrl_opt().is_some() {
                                // Only a merge target can be reached by a
                                // fall-through that the order does not follow:
                                // every other block is placed right after the
                                // block that falls into it.
                                if !self.merges.contains_key(&pc) {
                                    return ir_build_bail(line!(), pc);
                                }
                                self.add_merge_predecessor(pc);
                            }
                            let Some(&(start, end)) = walk.ranges.get(next_range) else {
                                break;
                            };
                            next_range += 1;
                            self.ctrl = NO_NODE;
                            // A local no entry edge initialises gets no loop
                            // phi, so `activate_loop_header` leaves its slot
                            // as it finds it. After a jump that would be a
                            // value from whichever block the order visited
                            // last, which need not dominate this one. Say
                            // "uninitialised" instead.
                            self.locals.fill(NO_NODE);
                            self.stack.clear();
                            pc = start;
                            range_end = end;
                        }
                    }
                }
            }
            // Bounds on the relocated region. A callee body that falls off its
            // own end never executed a return, which means the resolver admitted
            // a shape the walk does not agree with — refuse rather than walk
            // into whatever `lib.rs` appended next.
            // A spliced body with a rotated loop is walked range by range, the
            // same as the caller's block walk above. At a range's end a live
            // fall-through is a merge predecessor. When the ranges run out,
            // every `return` has recorded its exit edge, so the body closes the
            // way a multi-return body does.
            if let Some(frame) = self.splice.last() {
                if !frame.ranges.is_empty() && pc >= frame.range_end {
                    let (range_end, end) = (frame.range_end, frame.end);
                    if pc != range_end {
                        return ir_build_bail(line!(), pc);
                    }
                    if self.ctrl_opt().is_some() {
                        // Live control at the body's end never executed a
                        // `return`; anywhere else it must enter a merge.
                        if pc >= end || !self.merges.contains_key(&pc) {
                            return ir_build_bail(line!(), pc);
                        }
                        self.add_merge_predecessor(pc);
                    }
                    let Some(frame) = self.splice.last_mut() else {
                        return ir_build_bail(line!(), pc);
                    };
                    if let Some((start, next_end)) = frame.ranges.get(frame.next_range).copied() {
                        frame.next_range += 1;
                        frame.range_end = next_end;
                        // As at a caller range jump: the next range starts at a
                        // merge whose activation restores the callee's frame.
                        self.ctrl = NO_NODE;
                        self.locals.fill(NO_NODE);
                        self.stack.clear();
                        pc = start;
                        continue;
                    }
                    if frame.exits.is_empty() {
                        return ir_build_bail(line!(), pc);
                    }
                    match self.finish_multi_return_splice() {
                        Some(next) => {
                            pc = next;
                            continue;
                        }
                        None => return ir_build_bail(line!(), pc),
                    }
                }
            }
            if let Some(frame) = self.splice.last() {
                if pc >= frame.end {
                    // A multi-return body ENDS here rather than falling off:
                    // its last instruction is a `return` (the scanner admits no
                    // other shape) and that return recorded an edge instead of
                    // exiting, so `pc` lands exactly on `end` with every edge
                    // in and `ctrl` dead. Join them and resume the caller.
                    if frame.multi_return && pc == frame.end && !frame.exits.is_empty() {
                        match self.finish_multi_return_splice() {
                            Some(next) => {
                                pc = next;
                                continue;
                            }
                            None => return ir_build_bail(line!(), pc),
                        }
                    }
                    return ir_build_bail(line!(), pc);
                }
            }
            // A relocated body is unreachable from pc 0 by construction, so the
            // REACHABILITY skip — computed over the caller's code alone — must
            // stay scoped to code outside a splice. Merge bookkeeping is a
            // different matter: since 2026-09-09 the pre-scan above registers
            // each spliced body's own merge targets and loop headers, rebased,
            // so a merge inside a splice both exists and must be activated. The
            // gate below is deliberately NOT `self.splice.is_empty()` any more.
            // SPLICE4-1: a handler region an arm opened is walked like any
            // other code; every other handler stays skipped.
            if self.splice.is_empty()
                && !reachable.contains(&pc)
                && !self.catch_arm_walkable.contains(&pc)
            {
                // Handler-only (or otherwise unreachable) bytecode: emit no IR
                // for it at all. `next_pc` comes from the verifier's canonical
                // decode, so this steps over multi-byte operands correctly
                // (a hand-rolled `pc += 1` would resync onto operand bytes).
                pc = match verified.instruction_at(pc) {
                    Some(decoded) => decoded.next_pc as usize,
                    // Not an instruction boundary — the verifier guarantees the
                    // walk only ever lands on one, so this is unreachable; bail
                    // to single-pass rather than guess a length.
                    None => return ir_build_bail(line!(), pc),
                };
                continue;
            }

            // Round 14 wave 2 (lane guard, CH5-5): a miss-edge trap's receiver
            // fact holds only where the trap dominates -- in the compiling
            // method's own code, up to the next join or jump. A spliced body's
            // own merges are all behind the `invoke` that opened it.
            if self.splice.is_empty()
                && !self.trap_exact_facts.is_empty()
                && (self.merges.contains_key(&pc) || self.ctrl_opt().is_none())
            {
                self.trap_exact_facts.clear();
            }
            // Round 15 wave 1 (lane chain, G14-1): the field-load memo has the
            // same scope (see `field_load_memo_forward`).
            if self.splice.is_empty()
                && (self.merges.contains_key(&pc) || self.ctrl_opt().is_none())
            {
                // JIT round 23 wave 2e (W4L1-2): a forward join keeps what
                // every incoming edge carries; anything else clears.
                self.field_load_memo_join(pc);
                // Round 16 wave 14 (lane osrir, MON3-2): the static-load memo
                // has the same scope.
                self.static_load_memo_clear_at_join();
            } else if !self.splice.is_empty()
                && self.field_memo_in_splices
                && (self.merges.contains_key(&pc) || self.ctrl_opt().is_none())
            {
                // Round nio20 wave 4 (lane L1, W3L4-2): a join or dead point
                // INSIDE a spliced body. See the method.
                let loop_header =
                    self.loop_headers.contains(&pc) || self.loop_phis.contains_key(&pc);
                self.field_load_memo_clear_at_splice_join(loop_header);
            }
            // If this PC is a merge target, activate the merge.
            //
            // Inside a splice too. Every predecessor of a merge that the
            // spliced-body pre-scan registered is itself inside that same body,
            // so the locals and operand stack this snapshots are the callee's
            // throughout — the same invariant the caller's own merges rely on.
            if self.merges.contains_key(&pc) {
                // al16: the flow facts of `exact_receiver_flow` survive a join
                // into a loop the walk falls into as its sole forward
                // predecessor, for the slots that loop never writes, and a
                // forward join for the slots every predecessor agrees on.
                if self.splice.is_empty() && !self.exact_receiver_flow.is_empty() {
                    let sole_fall_through = !self.block_walk
                        && self.ctrl_opt().is_some()
                        && self.loop_headers.contains(&pc)
                        && self
                            .merges
                            .get(&pc)
                            .is_some_and(|m| m.ctrl_inputs.is_empty());
                    match self.loop_written_slots.get(&pc) {
                        Some(written) if sole_fall_through => {
                            self.exact_receiver_flow.retain(|k, _| !written.contains(k));
                        }
                        // A forward join (the continuation a spliced body's
                        // `ireturn`s meet at, say) keeps a fact for a slot
                        // that holds the SAME node on every predecessor: one
                        // value, so one class. Every forward predecessor of a
                        // pc the linear walk reaches is already recorded.
                        _ if !self.block_walk
                            && self.ctrl_opt().is_some()
                            && !self.loop_headers.contains(&pc) =>
                        {
                            let locals = &self.locals;
                            let preds = self.merges.get(&pc).map(|m| &m.local_snapshots);
                            self.exact_receiver_flow.retain(|&k, _| {
                                locals.get(k).is_some_and(|&v| {
                                    preds.is_some_and(|snaps| {
                                        snaps.iter().all(|s| s.get(k) == Some(&v))
                                    })
                                })
                            });
                        }
                        _ => self.exact_receiver_flow.clear(),
                    }
                }
                // Add current state as predecessor (fall-through). On a loop
                // header this is the forward-entry predecessor; the back-edge
                // arrives later and is back-patched (see add_merge_predecessor).
                if self.ctrl_opt().is_some() {
                    self.add_merge_predecessor(pc);
                }
                // Loop headers get eager loop-carried phis; forward joins use
                // the difference-based merge (all predecessors already known).
                if self.loop_headers.contains(&pc) {
                    self.activate_loop_header(pc);
                } else {
                    self.activate_merge(pc);
                }
                // Round 12 wave 5 (lane iropt4): a join ends every null fact;
                // an `ifnonnull` edge that is this merge's only input starts
                // one. See `SpliceFenceRelax`, relaxation 2.
                self.note_splice_merge_null_facts(pc);
            }

            // Safety net for the skip above: reachable code must always have a
            // live control token — either inherited from the fall-through or
            // restored by the merge activation. If it does not, the reachable
            // set and the merge bookkeeping disagree, and continuing would
            // build the very orphan nodes the skip exists to prevent. Bail to
            // the single-pass backend instead of emitting them.
            //
            // One exception: below `pruned_dead_until` dead control is the cold
            // arm of a pruned branch (see `prune_always_taken_branch`), walked
            // as dead code. Step over the instruction exactly as the
            // unreachable-code skip above does. A merge in that arm with a live
            // predecessor was activated just above and does not get here; one
            // whose only predecessors are in the cold arm stays unvisited and
            // input-less, and `audit_unactivated_merges` retires it. A back
            // edge into a skipped header from live code records a predecessor
            // on a merge the walk has passed, which the same audit refuses.
            // nio21 wave 17 (lane L4): code that only a pruned NEVER-taken
            // branch's taken edge reached (`prune_never_taken_branch`, never in
            // the block walk) -- proven unreachable without that edge, see
            // `never_taken_pc_is_dead` -- is stepped over the same way.
            if self.ctrl_opt().is_none()
                && self.splice.is_empty()
                && (pc < self.pruned_dead_until
                    || (!self.block_walk && self.never_taken_pc_is_dead(&verified, code_len, pc)))
            {
                pc = match verified.instruction_at(pc) {
                    Some(decoded) => decoded.next_pc as usize,
                    None => return ir_build_bail(line!(), pc),
                };
                continue;
            }
            // Round nio21 wave 1 (lane L2): handler code the local throw edges
            // made walkable, reached with dead control because no `athrow` of
            // this build took an edge into it. Normal flow never reaches such a
            // pc (`plan_local_throw_reachability`), so dead control here is the
            // truth, not a bookkeeping disagreement: step over it as the
            // unreachable-code skip does. A merge inside it has no predecessor
            // and `audit_unactivated_merges` retires it.
            if self.ctrl_opt().is_none()
                && self.splice.is_empty()
                && self.local_throw_optional.contains(&pc)
            {
                pc = match verified.instruction_at(pc) {
                    Some(decoded) => decoded.next_pc as usize,
                    None => return ir_build_bail(line!(), pc),
                };
                continue;
            }
            // Round 16 wave 7 (lane arraylist, AL16-1): the same for a spliced
            // body's cold throw arm, which `prune_cold_throw_arm` turned into a
            // trap at its branch. The arm has no merge inside it (that is what
            // makes it an arm), and its end is the branch target, a merge the
            // live edge entered.
            if self.ctrl_opt().is_none()
                && !self.splice.is_empty()
                && pc >= self.cold_throw_dead.0
                && pc < self.cold_throw_dead.1
            {
                pc += crate::bytecode_analysis::step(code, pc).max(1);
                continue;
            }
            // nio21 wave 4 (lane L2): a spliced body's masked handler code
            // (`splice_callee_handler_plan`), which normal flow never reaches;
            // an entered handler deopted at its first instruction.
            // JIT round 23 wave 4 (lane L1, CF23-2): a spliced body's code
            // that only a pruned never-taken edge reached
            // (`prune_spliced_never_taken_branch`). Normal flow cannot reach it
            // once the edge is a guard, so dead control here is the truth; a
            // merge inside it has no predecessor and is retired by
            // `audit_unactivated_merges`. With the branch built instead, the
            // code is reached with live control and walked as before.
            if self.ctrl_opt().is_none()
                && !self.splice.is_empty()
                && (self.splice_never_taken_dead.contains(&pc)
                    || self.splice_always_taken_dead.contains(&pc))
            {
                pc += crate::bytecode_analysis::step(code, pc).max(1);
                continue;
            }
            if self.ctrl_opt().is_none()
                && !self.splice.is_empty()
                && self.splice_handler_dead.contains(&pc)
            {
                pc += crate::bytecode_analysis::step(code, pc).max(1);
                continue;
            }
            // JIT round 23 wave 8e (lane W8-L1,
            // `CRATONVM_JIT_IR_COLD_THROW_TAIL_ARM_DEAD_SKIP`): a spliced
            // MULTI-RETURN body's tail cold throw arm (`COLD_THROW_TAIL_ARM`:
            // nothing falls into it, every way in is a side entry that
            // `prune_cold_throw_side_entry` made a guard). A single-return body
            // leaves the splice at its `return` and never walks the arm, but a
            // multi-return one walks on past each `return` to `end`, and so
            // reaches the arm with dead control: none of the skips above names
            // it (`cold_throw_dead` is set only by an arm's OWN branch, which a
            // tail arm does not have). Dead control at the arm is the truth: a
            // side entry left as a branch recorded a predecessor at the arm's
            // start, whose merge activation above gave it live control. The arm
            // is straight-line by shape, so it holds no merge.
            // `sun/invoke/util/Wrapper.forPrimitiveType` (spliced into
            // `ConstantUtils.classDesc`) is the shape.
            if self.ctrl_opt().is_none()
                && !self.splice.is_empty()
                && self.pc_in_dead_tail_cold_throw_arm(code, pc)
            {
                pc += crate::bytecode_analysis::step(code, pc).max(1);
                continue;
            }
            if self.ctrl_opt().is_some() {
                self.const_folded_dead_skip = false;
            }
            if self.ctrl_opt().is_none() && self.const_folded_dead_targets.contains(&pc) {
                self.const_folded_dead_skip = true;
            }
            if self.ctrl_opt().is_none() && self.const_folded_dead_skip {
                pc += crate::bytecode_analysis::step(code, pc).max(1);
                continue;
            }
            if self.ctrl_opt().is_none() {
                // JIT round 23 wave 8e (lane W8-L1, diagnostic): name the
                // spliced body (and its own bci) a dead-control refusal lands
                // in, so the census can tell which callee shape the dead-code
                // skips above miss.
                if ir_bail_reporting() {
                    if let Some(f) = self.splice.last() {
                        eprintln!(
                            "[ir] dead control in splice: callee {} bci {} (combined pc {pc}, \
                             invoke pc {}, depth {}, merge here: {}, loop header: {}) in {}",
                            f.method_key,
                            pc.saturating_sub(f.base),
                            f.invoke_pc,
                            self.splice.len(),
                            self.merges.contains_key(&pc),
                            self.loop_headers.contains(&pc),
                            self.method_label.as_deref().unwrap_or("<unknown>"),
                        );
                    }
                }
                return ir_build_bail(line!(), pc);
            }
            // Two edges into a join held different monitors: there is no one
            // monitor state to record for this bci. See `unstructured_monitors`.
            if self.unstructured_monitors {
                return ir_build_bail(line!(), pc);
            }

            // Step 1 of real-frame-deopt: record the abstract interpreter
            // state at this bytecode boundary so a precise deopt frame can be
            // rebuilt if execution must resume here. We snapshot AFTER any
            // merge activation above, so the recorded locals/stack reflect the
            // merged (phi-resolved) state the interpreter would see on entry to
            // this bci. Dead code after an unconditional transfer (ctrl ==
            // NO_NODE, not itself a merge target) has no reachable frame state,
            // so it is skipped. These snapshots are emit-and-discard until the
            // lowerer resolves them — they do not affect codegen on their own.
            //
            // Never inside a splice: a snapshot there would name a
            // combined-buffer pc, and `build_deopt_points` turns EVERY snapshot
            // into a `DeoptimizationPoint` whose `bci` the VM parks the
            // interpreter at. The spliced region is covered instead by the
            // caller's snapshot at the `invoke` pc, which every node in the
            // region is re-stamped with — see [`IrInlineSite`].
            //
            // Round 13 wave 3 (lane framestate, W5-3 stage 1): under
            // `CRATONVM_JIT_IR_SPLICE_FRAME_STATES` a spliced instruction gets
            // a snapshot too, but a CHAIN one ([`SpliceScopeChain`]): the
            // callee's own frame at its own bci over every caller parked
            // mid-`invoke`. Every by-bci consumer (including
            // `build_deopt_points`) skips it; `ir_lower` builds a
            // `FrameState::caller` chain from it for a guard in the body.
            // JIT round 21 wave 20 (lane sbfuse): none INSIDE a fused
            // `StringBuilder` chain, whose builder slot holds a placeholder
            // ([`Self::sb_chain_fuse_step`]). Trapping field/array reads in
            // operands are re-stamped with the chain's pre-`new` snapshot;
            // other nodes in the chain have no deopt exits.
            if self.splice.is_empty()
                && self.ctrl_opt().is_some()
                && (!self.sb_chain_active.as_ref().is_some_and(|a| pc > a.new_pc)
                    || self.sb_chain_active.as_ref().is_some_and(|a| a.has_call))
            {
                let mut sp = SafepointSnapshot::new(
                    pc,
                    self.locals.clone(),
                    self.stack.clone(),
                    self.monitors.clone(),
                );
                self.attach_sb_virtual_builders(&mut sp);
                self.graph.push_safepoint(sp);
            } else if self.splice_frame_states && self.ctrl_opt().is_some() {
                // JIT round 19 (lane splice19, CH16-4): under
                // `CRATONVM_JIT_IR_CHAIN_SNAPSHOTS_LAZY`, only where a reader
                // can exist.
                if self.chain_snapshots_lazy && !self.splice_pc_may_read_chain(code, pc) {
                    self.graph.chain_snapshot_census.skipped += 1;
                } else {
                    self.push_splice_state(pc);
                }
            }
            // Interpreter round i1 wave 47, lane L3: the frames a spliced call
            // built by the previous instruction returns into (see
            // `push_post_call_splice_state`). Taken here whatever `pc` is, so a
            // pending call never outlives its successor.
            if let Some((call, invoke_pc)) = self.pending_post_call.take() {
                let len = crate::bytecode_analysis::step(code, invoke_pc);
                let next_pc = (len > 0).then(|| invoke_pc.saturating_add(len));
                self.push_post_call_splice_state(pc, call, invoke_pc, next_pc);
            }

            // A spliced store into a caller-visible object was committed: from
            // here to the end of the outermost splice nothing may trap, or the
            // re-executed `invoke` would commit it twice. See
            // `admits_after_committed_splice_store`. A surviving CALL inside the
            // splice has committed whatever it did, and is fenced the same way
            // (round 11 wave 16, lane `replay`; see `splice_call_committed`).
            if !self.splice_committed_bases.is_empty() || self.splice_call_committed {
                if self.splice.is_empty() {
                    self.splice_committed_bases.clear();
                    self.splice_call_committed = false;
                } else if !self.sync_splice_open()
                    // Round 14 wave 2 (lane syncsplice): a synchronized body
                    // may not trap anywhere (checked whole at its exit), so
                    // there is no replay to fence.
                    && !self.admits_after_committed_splice_store(code, pc)
                    // Round 13 wave 3 (W5-3 stage 3): a trap here resumes the
                    // callee at its own bci through the chain snapshot just
                    // recorded, so nothing committed is re-run.
                    && !self.relax_fence_by_chain(pc)
                {
                    return ir_build_bail(line!(), pc);
                }
            }

            // nio21 wave 4 (lane L2): a spliced callee's handler, entered by a
            // call-site edge, resumes in the interpreter at its first
            // instruction (`plant_splice_handler_trap`); its masked code is
            // dead from here.
            if !self.splice.is_empty() && self.splice_handler_entries.contains(&pc) {
                if !self.plant_splice_handler_trap(pc) {
                    return ir_build_bail(line!(), pc);
                }
                pc += crate::bytecode_analysis::step(code, pc).max(1);
                continue;
            }
            // An instruction inside such a callee's covered `try` range: what
            // it builds is checked at the next boundary.
            // j27 wave 7 (lane T1L1, CF23-1): or one of a body spliced, at any
            // depth, at a covered `invoke` (`enter_covered_nested_instruction`).
            if self.pc_in_splice_handler_cover(pc) || self.enter_covered_nested_instruction() {
                self.splice_handler_mark = Some((pc, self.graph.nodes.len()));
            }
            // JIT round 21 wave 20 (lane sbfuse, STRINTR21-4): a fused
            // `StringBuilder` chain's own instructions.
            if self.splice.is_empty() && !self.sb_chain_fusions.is_empty() {
                match self.sb_chain_fuse_step(pc) {
                    Some(Some(next)) => {
                        pc = next;
                        continue;
                    }
                    Some(None) => return ir_build_bail(line!(), pc),
                    None => {}
                }
            }
            // JIT round 16 wave 10 (lane indyir, IN16-8): a constant call site
            // planned as a plain call is built by its invoke's arm.
            let op = match code[pc] {
                0xba if !self.indy_direct_sites.is_empty() => self.indy_direct_opcode(pc),
                // JIT round 16 wave 17 (lane mh7): a constant handle's call
                // planned as its member's `invokestatic`.
                0xb6 if self.mh_constant_sites.contains_key(&pc) => {
                    match self.mh_constant_opcode(pc) {
                        // A convert member's splice opened in place of the call.
                        Some(_) if self.pending_splice_jump.is_some() => {
                            pc = self.pending_splice_jump.take().unwrap_or(pc);
                            continue;
                        }
                        Some(op) => op,
                        None => return ir_build_bail(line!(), pc),
                    }
                }
                // JIT round 21 (lane lambda2): a SAM call on a lambda this
                // method built, planned as its impl's `invokestatic`.
                0xb9 if self.lambda_impl_sites.contains_key(&pc) => {
                    match self.lambda_impl_opcode(pc) {
                        Some(op) => op,
                        None => return ir_build_bail(line!(), pc),
                    }
                }
                other => other,
            };
            if let Some((offset, mask)) = self.vthread_guard {
                if self.splice.is_empty() && matches!(code[pc], 0xb6..=0xba | 0xc2) {
                    let ctrl = self.ctrl_opt()?;
                    let snapshot = self.graph.safepoints.len().checked_sub(1)? as u32;
                    let platform = self.graph.add(
                        Op::VthreadBoundaryTest { offset, mask },
                        IrType::Int,
                        vec![ctrl],
                        Some(pc),
                    );
                    let guard = self.graph.add(
                        Op::Guard { bci: pc },
                        IrType::Void,
                        vec![ctrl, platform],
                        Some(pc),
                    );
                    if !self.graph.set_node_frame_snapshot(guard, snapshot) {
                        return ir_build_bail(line!(), pc);
                    }
                }
            }
            // A method-exit `return` while this frame still holds a monitor is
            // unstructured locking (JVMS §2.11.10): the interpreter would throw
            // `IllegalMonitorStateException` there, and no frame state this
            // builder records could describe the lock being dropped. Refuse.
            // Inside a splice a return hands a value back to the caller, whose
            // monitors are legitimately still held.
            //
            // Round 15 wave 1 (lane sync, SS8-2): except the self-locking
            // method's own monitor, released right here before the return.
            if self.splice.is_empty()
                && matches!(op, 0xac..=0xb1)
                && !self.monitors.is_empty()
                && !self.release_method_self_lock_at_return(pc)
            {
                return ir_build_bail(line!(), pc);
            }
            match op {
                // nop — do nothing, advance one byte.
                //
                // Free coverage that was missing until 2026-09-17, and a real
                // divergence: `jit_scan` accepts `nop` (`x64/bytecode_compat.rs`
                // admits 0x00), the single-pass tier implements it
                // (`x64/op_local_stack.rs`), and `ir_compatible` says nothing
                // about it — so a `nop`-bearing method was admitted to this
                // tier, built all the way to the `nop`, and then refused at the
                // `_ =>` catch-all after a full partial build. That is exactly
                // the pattern `ir_compatible`'s own comment condemns: "A method
                // the builder will refuse must be refused HERE, cheaply and
                // with a reason, not after a full graph build."
                //
                // It is not a rare opcode in practice: ASM-based rewriters emit
                // `nop` as padding when they shorten an instruction, and
                // several agent/instrumentation frameworks and obfuscators
                // leave them behind. This arm ADMITS those methods to the
                // optimizing tier for the first time.
                0x00 => {
                    pc += 1;
                }
                // aconst_null — push the null reference.
                //
                // Absent until 2026-07-31, and it cost more than its size
                // suggests: `BinTreesClassic.bottomUpTree` was refused at its
                // first `aconst_null`, and a null literal is unavoidable in any
                // method that builds or terminates a reference structure. See
                // `Self::aconst_null` for why the node is `Ref`-typed.
                0x01 => {
                    let c = self.aconst_null();
                    self.push(c);
                    pc += 1;
                }
                // iconst_m1..iconst_5
                0x02..=0x08 => {
                    let val = (op as i64) - 3;
                    let c = self.iconst(val);
                    self.push(c);
                    pc += 1;
                }
                // bipush
                0x10 => {
                    let val = code[pc + 1] as i8 as i64;
                    let c = self.iconst(val);
                    self.push(c);
                    pc += 2;
                }
                // sipush
                0x11 => {
                    let val = i16::from_be_bytes([code[pc + 1], code[pc + 2]]) as i64;
                    let c = self.iconst(val);
                    self.push(c);
                    pc += 3;
                }
                // iload
                0x15 => {
                    let idx = code[pc + 1] as usize;
                    if !self.push_local(idx) {
                        return ir_build_bail(line!(), pc);
                    }
                    pc += 2;
                }
                // iload_0..3
                0x1a..=0x1d => {
                    let idx = (op - 0x1a) as usize;
                    if !self.push_local(idx) {
                        return ir_build_bail(line!(), pc);
                    }
                    pc += 1;
                }
                // lload_0..3
                0x1e..=0x21 => {
                    let idx = (op - 0x1e) as usize;
                    if !self.push_local(idx) {
                        return ir_build_bail(line!(), pc);
                    }
                    pc += 1;
                }
                // lload (wide index) — inc 26. A long is one NodeId slot; the
                // value lives at `locals[idx]` (the high half slot `idx+1` is
                // never read by valid bytecode). Long-only, so inert for the int
                // path.
                0x16 => {
                    let idx = code[pc + 1] as usize;
                    if !self.push_local(idx) {
                        return ir_build_bail(line!(), pc);
                    }
                    pc += 2;
                }
                // aload — push an object reference local (same node-graph
                // mechanics as iload: a reference is just a NodeId on the
                // abstract stack; its only consumer in the IR slice we lower is
                // a `getfield` base).
                0x19 => {
                    let idx = code[pc + 1] as usize;
                    if !self.push_local(idx) {
                        return ir_build_bail(line!(), pc);
                    }
                    pc += 2;
                }
                // aload_0..3
                0x2a..=0x2d => {
                    let idx = (op - 0x2a) as usize;
                    if !self.push_local(idx) {
                        return ir_build_bail(line!(), pc);
                    }
                    pc += 1;
                }
                // astore — store an object reference into a local. Same
                // node-graph mechanics as istore: a reference is just a NodeId
                // slot in the abstract locals array (mirrors the `aload`
                // comment above). Real javac stores a `new` object into a local
                // (`new; dup; invokespecial; astore_N`), so without this the IR
                // builder bails on every allocation method and scalar
                // replacement never fires on production bytecode.
                0x3a => {
                    let idx = code[pc + 1] as usize;
                    let val = self.pop();
                    self.store_local(idx, val);
                    pc += 2;
                }
                // astore_0..3
                0x4b..=0x4e => {
                    let idx = (op - 0x4b) as usize;
                    let val = self.pop();
                    self.store_local(idx, val);
                    pc += 1;
                }
                // istore
                0x36 => {
                    let idx = code[pc + 1] as usize;
                    let val = self.pop();
                    self.store_local(idx, val);
                    pc += 2;
                }
                // istore_0..3
                0x3b..=0x3e => {
                    let idx = (op - 0x3b) as usize;
                    let val = self.pop();
                    self.store_local(idx, val);
                    pc += 1;
                }
                // lstore_0..3
                0x3f..=0x42 => {
                    let idx = (op - 0x3f) as usize;
                    let val = self.pop();
                    self.store_local(idx, val);
                    pc += 1;
                }
                // lstore (wide index) — inc 26. Stores the long NodeId at
                // `locals[idx]`; `store_local` marks the high half slot `idx+1`
                // uninitialised (JVMS §2.6.1). Long-only.
                0x37 => {
                    let idx = code[pc + 1] as usize;
                    let val = self.pop();
                    self.store_local(idx, val);
                    pc += 2;
                }
                // iadd
                0x60 => {
                    let b = self.pop();
                    let a = self.pop();
                    let r = self.add_data(Op::Add, IrType::Int, vec![a, b], pc);
                    self.push(r);
                    pc += 1;
                }
                // ladd
                0x61 => {
                    let b = self.pop();
                    let a = self.pop();
                    let r = self.add_data(Op::Add, IrType::Long, vec![a, b], pc);
                    self.push(r);
                    pc += 1;
                }
                // isub
                0x64 => {
                    let b = self.pop();
                    let a = self.pop();
                    let r = self.add_data(Op::Sub, IrType::Int, vec![a, b], pc);
                    self.push(r);
                    pc += 1;
                }
                // lsub
                0x65 => {
                    let b = self.pop();
                    let a = self.pop();
                    let r = self.add_data(Op::Sub, IrType::Long, vec![a, b], pc);
                    self.push(r);
                    pc += 1;
                }
                // imul
                0x68 => {
                    let b = self.pop();
                    let a = self.pop();
                    let r = self.add_data(Op::Mul, IrType::Int, vec![a, b], pc);
                    self.push(r);
                    pc += 1;
                }
                // lmul
                0x69 => {
                    let b = self.pop();
                    let a = self.pop();
                    let r = self.add_data(Op::Mul, IrType::Long, vec![a, b], pc);
                    self.push(r);
                    pc += 1;
                }
                // idiv
                0x6c => {
                    let b = self.pop();
                    let a = self.pop();
                    let g = self.add_div_zero_guard(b, IrType::Int, pc);
                    self.plant_implicit_throw_edges(pc, g, self.mem, code_len);
                    let r =
                        self.add_data(Op::Div, IrType::Int, with_guard_token(vec![a, b], g), pc);
                    self.push(r);
                    pc += 1;
                }
                // ldiv — the lowerer already emits CQO + 64-bit `IDIV RCX` for a
                // `Long` Op::Div (incl. the div-by-zero + MIN/-1 overflow guards),
                // so the builder just needs to produce the typed node. The long
                // div-by-zero guard is the cat-2 deopt trigger (real-frame-deopt).
                0x6d => {
                    let b = self.pop();
                    let a = self.pop();
                    let g = self.add_div_zero_guard(b, IrType::Long, pc);
                    self.plant_implicit_throw_edges(pc, g, self.mem, code_len);
                    let r =
                        self.add_data(Op::Div, IrType::Long, with_guard_token(vec![a, b], g), pc);
                    self.push(r);
                    pc += 1;
                }
                // irem
                0x70 => {
                    let b = self.pop();
                    let a = self.pop();
                    let g = self.add_div_zero_guard(b, IrType::Int, pc);
                    self.plant_implicit_throw_edges(pc, g, self.mem, code_len);
                    let r =
                        self.add_data(Op::Rem, IrType::Int, with_guard_token(vec![a, b], g), pc);
                    self.push(r);
                    pc += 1;
                }
                // lrem — see `ldiv`: the lowerer handles the 64-bit `Long` path.
                0x71 => {
                    let b = self.pop();
                    let a = self.pop();
                    let g = self.add_div_zero_guard(b, IrType::Long, pc);
                    self.plant_implicit_throw_edges(pc, g, self.mem, code_len);
                    let r =
                        self.add_data(Op::Rem, IrType::Long, with_guard_token(vec![a, b], g), pc);
                    self.push(r);
                    pc += 1;
                }
                // ineg
                0x74 => {
                    let a = self.pop();
                    let r = self.add_data(Op::Neg, IrType::Int, vec![a], pc);
                    self.push(r);
                    pc += 1;
                }
                // lneg
                0x75 => {
                    let a = self.pop();
                    let r = self.add_data(Op::Neg, IrType::Long, vec![a], pc);
                    self.push(r);
                    pc += 1;
                }
                // ishl
                0x78 => {
                    let b = self.pop();
                    let a = self.pop();
                    let r = self.add_data(Op::Shl, IrType::Int, vec![a, b], pc);
                    self.push(r);
                    pc += 1;
                }
                // ishr
                0x7a => {
                    let b = self.pop();
                    let a = self.pop();
                    let r = self.add_data(Op::Shr, IrType::Int, vec![a, b], pc);
                    self.push(r);
                    pc += 1;
                }
                // iushr
                0x7c => {
                    let b = self.pop();
                    let a = self.pop();
                    let r = self.add_data(Op::UShr, IrType::Int, vec![a, b], pc);
                    self.push(r);
                    pc += 1;
                }
                // iand
                0x7e => {
                    let b = self.pop();
                    let a = self.pop();
                    let r = self.add_data(Op::And, IrType::Int, vec![a, b], pc);
                    self.push(r);
                    pc += 1;
                }
                // ior
                0x80 => {
                    let b = self.pop();
                    let a = self.pop();
                    let r = self.add_data(Op::Or, IrType::Int, vec![a, b], pc);
                    self.push(r);
                    pc += 1;
                }
                // ixor
                0x82 => {
                    let b = self.pop();
                    let a = self.pop();
                    let r = self.add_data(Op::Xor, IrType::Int, vec![a, b], pc);
                    self.push(r);
                    pc += 1;
                }
                // inc 27: long shifts + bitwise. The shift count is an `int` (one
                // slot) on top of the `long` value; the lowerer's `Op::Shl/Shr/
                // UShr` are width-aware (64-bit shift masks the count to 6 bits,
                // matching `lshl`'s `count & 0x3f`), and `Op::And/Or/Xor` are
                // already 64-bit (correct for both Int and Long). These are
                // 1-byte opcodes (the length walkers' default arm sizes them
                // correctly). Long-only → inert for the int path.
                // lshl
                0x79 => {
                    let b = self.pop();
                    let a = self.pop();
                    let r = self.add_data(Op::Shl, IrType::Long, vec![a, b], pc);
                    self.push(r);
                    pc += 1;
                }
                // lshr
                0x7b => {
                    let b = self.pop();
                    let a = self.pop();
                    let r = self.add_data(Op::Shr, IrType::Long, vec![a, b], pc);
                    self.push(r);
                    pc += 1;
                }
                // lushr
                0x7d => {
                    let b = self.pop();
                    let a = self.pop();
                    let r = self.add_data(Op::UShr, IrType::Long, vec![a, b], pc);
                    self.push(r);
                    pc += 1;
                }
                // land
                0x7f => {
                    let b = self.pop();
                    let a = self.pop();
                    let r = self.add_data(Op::And, IrType::Long, vec![a, b], pc);
                    self.push(r);
                    pc += 1;
                }
                // lor
                0x81 => {
                    let b = self.pop();
                    let a = self.pop();
                    let r = self.add_data(Op::Or, IrType::Long, vec![a, b], pc);
                    self.push(r);
                    pc += 1;
                }
                // lxor
                0x83 => {
                    let b = self.pop();
                    let a = self.pop();
                    let r = self.add_data(Op::Xor, IrType::Long, vec![a, b], pc);
                    self.push(r);
                    pc += 1;
                }
                // lcmp — inc 27. 3-way signed compare of two longs → int
                // {-1,0,1}; usually feeds an `if<cond>` against 0 (the existing
                // 0x99..0x9e arm), so `lcmp; iflt` becomes `left < right`.
                0x94 => {
                    let b = self.pop();
                    let a = self.pop();
                    let r = self.add_data(Op::LCmp, IrType::Int, vec![a, b], pc);
                    self.push(r);
                    pc += 1;
                }
                // fcmpl / fcmpg / dcmpl / dcmpg — FP 3-way compare → int
                // {-1,0,1}, like `lcmp` but with the JVMS NaN-unordered rule:
                // a NaN operand yields -1 for the `l` variants (fcmpl/dcmpl) and
                // +1 for the `g` variants (fcmpg/dcmpg). The result feeds the
                // same `if<cond>`-against-0 arm as `lcmp`. Only reachable under
                // the FP gate (`method_uses_fp` ⇒ `ir_emit_fp`); the lowerer
                // emits `ucomiss`/`ucomisd` (see `Op::FCmp`).
                0x95 | 0x96 | 0x97 | 0x98 => {
                    let b = self.pop();
                    let a = self.pop();
                    let double = op == 0x97 || op == 0x98; // dcmpl / dcmpg
                    let nan_greater = op == 0x96 || op == 0x98; // fcmpg / dcmpg
                    let r = self.add_data(
                        Op::FCmp {
                            double,
                            nan_greater,
                        },
                        IrType::Int,
                        vec![a, b],
                        pc,
                    );
                    self.push(r);
                    pc += 1;
                }
                // wide — the 16-bit-index forms of the local loads, local
                // stores and `iinc`. Same node-graph mechanics as the narrow
                // arms (a load pushes the local's node, a store replaces it,
                // `iinc` adds an `Int` constant); only the operand width
                // differs. `jit_scan` accepted `wide`, so `ir_compatible`
                // admitted its methods and the build refused them at the
                // catch-all. `wide ret` stays refused, like `ret`.
                0xc4 => {
                    if pc + 3 >= code.len() {
                        return ir_build_bail(line!(), pc);
                    }
                    let sub = code[pc + 1];
                    let idx = u16::from_be_bytes([code[pc + 2], code[pc + 3]]) as usize;
                    if idx >= self.locals.len() {
                        return ir_build_bail(line!(), pc);
                    }
                    match sub {
                        // iload lload fload dload aload
                        0x15..=0x19 => {
                            if !self.push_local(idx) {
                                return ir_build_bail(line!(), pc);
                            }
                            pc += 4;
                        }
                        // istore lstore fstore dstore astore
                        0x36..=0x3a => {
                            let val = self.pop();
                            self.store_local(idx, val);
                            pc += 4;
                        }
                        // iinc with a 16-bit signed increment
                        0x84 => {
                            if pc + 5 >= code.len() {
                                return ir_build_bail(line!(), pc);
                            }
                            let inc = i64::from(i16::from_be_bytes([code[pc + 4], code[pc + 5]]));
                            let old = self.locals[idx];
                            let c = self.iconst(inc);
                            let r = self.add_data(Op::Add, IrType::Int, vec![old, c], pc);
                            self.store_local(idx, r);
                            pc += 6;
                        }
                        _ => return ir_build_bail(line!(), pc),
                    }
                }
                // iinc
                0x84 => {
                    let idx = code[pc + 1] as usize;
                    let inc = code[pc + 2] as i8 as i64;
                    let Some(old) = self.locals.get(idx).copied() else {
                        return ir_build_bail(line!(), pc);
                    };
                    let c = self.iconst(inc);
                    let r = self.add_data(Op::Add, IrType::Int, vec![old, c], pc);
                    self.store_local(idx, r);
                    pc += 3;
                }
                // i2l
                0x85 => {
                    let a = self.pop();
                    let r = self.add_data(Op::I2L, IrType::Long, vec![a], pc);
                    self.push(r);
                    pc += 1;
                }
                // l2i
                0x88 => {
                    let a = self.pop();
                    let r = self.add_data(Op::L2I, IrType::Int, vec![a], pc);
                    self.push(r);
                    pc += 1;
                }
                // i2b — sign-extend the low byte. Decomposed to `(x << 24) >> 24`
                // (32-bit `SHL`/`SAR EAX`, so the arithmetic shift sign-extends
                // the byte) rather than a dedicated truncation node — no new Op
                // or lowering, and the existing GVN/fold passes handle it.
                0x91 => {
                    let a = self.pop();
                    let c = self.iconst(24);
                    let shl = self.add_data(Op::Shl, IrType::Int, vec![a, c], pc);
                    let r = self.add_data(Op::Shr, IrType::Int, vec![shl, c], pc);
                    self.push(r);
                    pc += 1;
                }
                // i2c — zero-extend the low 16 bits: `x & 0xFFFF` (char is an
                // unsigned 16-bit value).
                0x92 => {
                    let a = self.pop();
                    let mask = self.iconst(0xFFFF);
                    let r = self.add_data(Op::And, IrType::Int, vec![a, mask], pc);
                    self.push(r);
                    pc += 1;
                }
                // i2s — sign-extend the low 16 bits: `(x << 16) >> 16`.
                0x93 => {
                    let a = self.pop();
                    let c = self.iconst(16);
                    let shl = self.add_data(Op::Shl, IrType::Int, vec![a, c], pc);
                    let r = self.add_data(Op::Shr, IrType::Int, vec![shl, c], pc);
                    self.push(r);
                    pc += 1;
                }

                // ── FP value tier (inc 30) ───────────────────────────────────
                //
                // The whole block below is reached ONLY when the method was
                // admitted via the `ir_emit_fp` gate clause in `lib.rs`
                // (`method_uses_fp` ⇒ FP-gate-only). With the gate off, no
                // FP-opcode method enters the builder, so these arms are inert
                // and gate-off codegen is byte-identical. A float is one NodeId
                // (cat-1); a double is one NodeId occupying two JVM local slots
                // (cat-2, exactly like `long` — `store_local` marks the high-half
                // slot uninitialised).
                //
                // The lowerer (`ir_lower.rs`) marshals every `IrType::Float`/
                // `Double` value through XMM. The optimizer passes are FP-safe by
                // construction: `const_value`/`is_const_val` match only
                // `Op::Const`, never `Op::ConstF`, and `int_width` returns `None`
                // for FP, so fold/identity/reassociation never rewrite FP nodes.

                // fconst_0 / fconst_1 / fconst_2
                0x0b => {
                    let c = self.fconst(0.0);
                    self.push(c);
                    pc += 1;
                }
                0x0c => {
                    let c = self.fconst(1.0);
                    self.push(c);
                    pc += 1;
                }
                0x0d => {
                    let c = self.fconst(2.0);
                    self.push(c);
                    pc += 1;
                }
                // dconst_0 / dconst_1
                0x0e => {
                    let c = self.dconst(0.0);
                    self.push(c);
                    pc += 1;
                }
                0x0f => {
                    let c = self.dconst(1.0);
                    self.push(c);
                    pc += 1;
                }

                // fload / dload — read a FP local. Identical node-graph mechanics
                // to `iload`/`lload`: the value is a single NodeId held at
                // `locals[idx]` (a double's high-half slot `idx+1` is never read).
                // fload (wide index)
                0x17 => {
                    let idx = code[pc + 1] as usize;
                    if !self.push_local(idx) {
                        return ir_build_bail(line!(), pc);
                    }
                    pc += 2;
                }
                // dload (wide index)
                0x18 => {
                    let idx = code[pc + 1] as usize;
                    if !self.push_local(idx) {
                        return ir_build_bail(line!(), pc);
                    }
                    pc += 2;
                }
                // fload_0..3
                0x22..=0x25 => {
                    let idx = (op - 0x22) as usize;
                    if !self.push_local(idx) {
                        return ir_build_bail(line!(), pc);
                    }
                    pc += 1;
                }
                // dload_0..3
                0x26..=0x29 => {
                    let idx = (op - 0x26) as usize;
                    if !self.push_local(idx) {
                        return ir_build_bail(line!(), pc);
                    }
                    pc += 1;
                }
                // fstore / dstore — write a FP local (same mechanics as
                // `istore`/`lstore`; `store_local` marks a double's high-half
                // slot `idx+1` uninitialised).
                // fstore (wide index)
                0x38 => {
                    let idx = code[pc + 1] as usize;
                    let val = self.pop();
                    self.store_local(idx, val);
                    pc += 2;
                }
                // dstore (wide index)
                0x39 => {
                    let idx = code[pc + 1] as usize;
                    let val = self.pop();
                    self.store_local(idx, val);
                    pc += 2;
                }
                // fstore_0..3
                0x43..=0x46 => {
                    let idx = (op - 0x43) as usize;
                    let val = self.pop();
                    self.store_local(idx, val);
                    pc += 1;
                }
                // dstore_0..3
                0x47..=0x4a => {
                    let idx = (op - 0x47) as usize;
                    let val = self.pop();
                    self.store_local(idx, val);
                    pc += 1;
                }

                // FP arithmetic — `Op::Add/Sub/Mul/Div/Neg` typed `Float`/
                // `Double`. The lowerer dispatches on the node type to the XMM
                // scalar form (`addss`/`addsd`/…). FP `Div` has NO div-zero /
                // overflow guard (IEEE division by zero is `±inf`/`NaN`, never an
                // exception), so unlike int `Div` it never deopts. `frem`/`drem`
                // (0x72/0x73) — JVM FP remainder is `fmod`-style with no single
                // SSE instruction, so the lowerer emits a `CALL` to the
                // `jit_frem`/`jit_drem` runtime helper (Slice A); the builder
                // just produces `Op::Rem` typed `Float`/`Double`.
                // fadd / dadd
                0x62 => {
                    let b = self.pop();
                    let a = self.pop();
                    let r = self.add_data(Op::Add, IrType::Float, vec![a, b], pc);
                    self.push(r);
                    pc += 1;
                }
                0x63 => {
                    let b = self.pop();
                    let a = self.pop();
                    let r = self.add_data(Op::Add, IrType::Double, vec![a, b], pc);
                    self.push(r);
                    pc += 1;
                }
                // fsub / dsub
                0x66 => {
                    let b = self.pop();
                    let a = self.pop();
                    let r = self.add_data(Op::Sub, IrType::Float, vec![a, b], pc);
                    self.push(r);
                    pc += 1;
                }
                0x67 => {
                    let b = self.pop();
                    let a = self.pop();
                    let r = self.add_data(Op::Sub, IrType::Double, vec![a, b], pc);
                    self.push(r);
                    pc += 1;
                }
                // fmul / dmul
                0x6a => {
                    let b = self.pop();
                    let a = self.pop();
                    let r = self.add_data(Op::Mul, IrType::Float, vec![a, b], pc);
                    self.push(r);
                    pc += 1;
                }
                0x6b => {
                    let b = self.pop();
                    let a = self.pop();
                    let r = self.add_data(Op::Mul, IrType::Double, vec![a, b], pc);
                    self.push(r);
                    pc += 1;
                }
                // fdiv / ddiv
                0x6e => {
                    let b = self.pop();
                    let a = self.pop();
                    let r = self.add_data(Op::Div, IrType::Float, vec![a, b], pc);
                    self.push(r);
                    pc += 1;
                }
                0x6f => {
                    let b = self.pop();
                    let a = self.pop();
                    let r = self.add_data(Op::Div, IrType::Double, vec![a, b], pc);
                    self.push(r);
                    pc += 1;
                }
                // frem / drem — `fmod`-style remainder lowered via the
                // jit_frem/jit_drem helper call (no single SSE instruction).
                0x72 => {
                    let b = self.pop();
                    let a = self.pop();
                    let r = self.add_data(Op::Rem, IrType::Float, vec![a, b], pc);
                    self.push(r);
                    pc += 1;
                }
                0x73 => {
                    let b = self.pop();
                    let a = self.pop();
                    let r = self.add_data(Op::Rem, IrType::Double, vec![a, b], pc);
                    self.push(r);
                    pc += 1;
                }
                // fneg / dneg
                0x76 => {
                    let a = self.pop();
                    let r = self.add_data(Op::Neg, IrType::Float, vec![a], pc);
                    self.push(r);
                    pc += 1;
                }
                0x77 => {
                    let a = self.pop();
                    let r = self.add_data(Op::Neg, IrType::Double, vec![a], pc);
                    self.push(r);
                    pc += 1;
                }

                // FP ⇄ integer / FP ⇄ FP conversions. All are pure single-input
                // `Op` variants the lowerer maps to a `cvt*` XMM instruction.
                // i2f / i2d
                0x86 => {
                    let a = self.pop();
                    let r = self.add_data(Op::I2F, IrType::Float, vec![a], pc);
                    self.push(r);
                    pc += 1;
                }
                0x87 => {
                    let a = self.pop();
                    let r = self.add_data(Op::I2D, IrType::Double, vec![a], pc);
                    self.push(r);
                    pc += 1;
                }
                // l2f / l2d
                0x89 => {
                    let a = self.pop();
                    let r = self.add_data(Op::L2F, IrType::Float, vec![a], pc);
                    self.push(r);
                    pc += 1;
                }
                0x8a => {
                    let a = self.pop();
                    let r = self.add_data(Op::L2D, IrType::Double, vec![a], pc);
                    self.push(r);
                    pc += 1;
                }
                // f2i / f2l / f2d
                0x8b => {
                    let a = self.pop();
                    let r = self.add_data(Op::F2I, IrType::Int, vec![a], pc);
                    self.push(r);
                    pc += 1;
                }
                0x8c => {
                    let a = self.pop();
                    let r = self.add_data(Op::F2L, IrType::Long, vec![a], pc);
                    self.push(r);
                    pc += 1;
                }
                0x8d => {
                    let a = self.pop();
                    let r = self.add_data(Op::F2D, IrType::Double, vec![a], pc);
                    self.push(r);
                    pc += 1;
                }
                // d2i / d2l / d2f
                0x8e => {
                    let a = self.pop();
                    let r = self.add_data(Op::D2I, IrType::Int, vec![a], pc);
                    self.push(r);
                    pc += 1;
                }
                0x8f => {
                    let a = self.pop();
                    let r = self.add_data(Op::D2L, IrType::Long, vec![a], pc);
                    self.push(r);
                    pc += 1;
                }
                0x90 => {
                    let a = self.pop();
                    let r = self.add_data(Op::D2F, IrType::Float, vec![a], pc);
                    self.push(r);
                    pc += 1;
                }

                // faload / daload — FP array element load (Slice B). The index
                // is a runtime value, so the lowerer emits the JVMS null +
                // bounds checks (deopt-on-fault) before the element access. The
                // load advances the memory token (a later store to a possibly-
                // aliasing element must be ordered after this read).
                0x30 | 0x31 => {
                    let (kind, ty) = if op == 0x30 {
                        (MemKind::Float, IrType::Float)
                    } else {
                        (MemKind::Double, IrType::Double)
                    };
                    let index = self.pop();
                    let array = self.pop();
                    if let Some(folded) = self
                        .fold_frozen_array_load(code_len, pc, array, index, ty)
                        .and_then(|val| self.frozen_value_to_const(ty, val))
                    {
                        self.push(folded);
                        pc += 1;
                        continue;
                    }
                    let load = self.graph.add(
                        Op::ArrayLoad(kind),
                        ty,
                        vec![self.ctrl, self.mem, array, index],
                        Some(pc),
                    );
                    self.plant_implicit_throw_edges(pc, load, self.mem, code_len);
                    self.mem = load;
                    self.push(load);
                    pc += 1;
                }
                // fastore / dastore — FP array element store (Slice B). Consumes
                // the memory token and produces a new one, so the scheduler
                // serialises it against neighbouring memory ops.
                //
                // Inside a splice both are held to the rule every other store
                // arm keeps — `splice_store_is_local` — and until 2026-09-18
                // these two were the exception. The splice scanner in
                // `jit_bridge.rs` admits `0x4f..=0x56` in IR mode, fastore and
                // dastore included, so a spliced `a[i] = x` into a CALLER'S
                // `float[]`/`double[]` was built, and a deopt later in the same
                // spliced region re-executed the whole call and committed the
                // write twice (`a[i] += 1.0f` would add two).
                0x51 => {
                    let value = self.pop();
                    let index = self.pop();
                    let array = self.pop();
                    if !self.splice.is_empty() && !self.splice_store_is_local(array) {
                        return ir_build_bail(line!(), pc);
                    }
                    let store = self.graph.add(
                        Op::ArrayStore(MemKind::Float),
                        IrType::Memory,
                        vec![self.ctrl, self.mem, array, index, value],
                        Some(pc),
                    );
                    self.plant_implicit_throw_edges(pc, store, self.mem, code_len);
                    self.mem = store;
                    pc += 1;
                }
                0x52 => {
                    let value = self.pop();
                    let index = self.pop();
                    let array = self.pop();
                    if !self.splice.is_empty() && !self.splice_store_is_local(array) {
                        return ir_build_bail(line!(), pc);
                    }
                    let store = self.graph.add(
                        Op::ArrayStore(MemKind::Double),
                        IrType::Memory,
                        vec![self.ctrl, self.mem, array, index, value],
                        Some(pc),
                    );
                    self.plant_implicit_throw_edges(pc, store, self.mem, code_len);
                    self.mem = store;
                    pc += 1;
                }

                // ── COV-02: the integral and reference array element access ──
                //
                // `faload`/`daload`/`fastore`/`dastore` above are the arms an FP
                // benchmark kernel needed. Nobody decided that a `float[]`
                // element should be lowerable and an `int[]` element should
                // not; the survey in `docs/known-issues/c2/` measured 77 events
                // on the missing arms, second-largest opcode bucket, and the
                // *shape* is the argument rather than the number. These arms
                // are the same node with a different [`MemKind`]: one element
                // width, one sign/zero-extension rule, the same JVMS null +
                // bounds guards, the same memory token.
                //
                // iaload / laload / aaload / baload / caload / saload.
                // (`baload` covers `boolean[]` too — one byte either way.)
                //
                // `aaload` is a REFERENCE load: the node is typed
                // `IrType::Ref`, so `ir_lower::emit_safepoint_map`'s scan
                // publishes its spill slot as a rewritable root at every later
                // safepoint. Typing it `Int` would compile — and lose the
                // element across the first relocating collection.
                //
                // `aastore` is deliberately NOT here (see the store arm below).
                0x2e | 0x2f | 0x32 | 0x33 | 0x34 | 0x35 => {
                    let (kind, ty) = match op {
                        0x2e => (MemKind::Int, IrType::Int),
                        0x2f => (MemKind::Long, IrType::Long),
                        0x32 => (MemKind::Ref, IrType::Ref),
                        0x33 => (MemKind::Byte, IrType::Int),
                        0x34 => (MemKind::Char, IrType::Int),
                        // 0x35 saload
                        _ => (MemKind::Short, IrType::Int),
                    };
                    let index = self.pop();
                    let array = self.pop();
                    if self.frozen_facts.is_some() {
                        if let Some(folded) = self
                            .fold_frozen_array_load(code_len, pc, array, index, ty)
                            .and_then(|val| self.materialize_frozen_value(pc, ty, val))
                        {
                            self.push(folded);
                            pc += 1;
                            continue;
                        }
                    }
                    let load = self.graph.add(
                        Op::ArrayLoad(kind),
                        ty,
                        vec![self.ctrl, self.mem, array, index],
                        Some(pc),
                    );
                    self.note_splice_local_load(load, array);
                    self.plant_implicit_throw_edges(pc, load, self.mem, code_len);
                    self.mem = load;
                    self.push(load);
                    pc += 1;
                }
                // iastore / lastore / bastore / castore / sastore.
                //
                // `aastore` (0x53) is IN scope as of 2026-09-06, and the
                // paragraph that used to stand here is worth keeping because
                // its reasoning is still correct -- it just argued for the
                // wrong conclusion. It said a reference element store needs the
                // SATB pre-write barrier and the card mark the single-pass
                // backend emits around `emit_ref_astore_regs`, that a missing
                // barrier is invisible until a concurrent collection drops the
                // only path to an overwritten-but-live target, and that
                // refusing the method is the cheap answer.
                //
                // All true. What it missed is that emitting the store INLINE is
                // not the only alternative to refusing: `jit_aastore` owns the
                // covariance check and both barriers, and the single-pass
                // backend already calls it as its own escape hatch under the
                // ZGC barrier gate. So this arm builds the node and
                // `ir_lower` calls that helper -- no barrier contract moves
                // into this tier, which is the property the old paragraph was
                // really protecting.
                //
                // Measured: **35 methods refused on the H2 JDBC workload**, the
                // second-largest build refusal there after the invoke plan.
                0x4f | 0x50 | 0x53 | 0x54 | 0x55 | 0x56 => {
                    let kind = match op {
                        0x4f => MemKind::Int,
                        0x50 => MemKind::Long,
                        0x53 => MemKind::Ref,
                        0x54 => MemKind::Byte,
                        0x55 => MemKind::Char,
                        // 0x56 sastore
                        _ => MemKind::Short,
                    };
                    let value = self.pop();
                    let index = self.pop();
                    let array = self.pop();
                    // See `splice_store_is_local`: a write inside a spliced body
                    // may be re-executed, so it must target an array the splice
                    // itself made. Round 13 wave 3 (W5-3 stage 3): unless a trap
                    // after it resumes through a caller chain; the store is then
                    // COMMITTED like a `putfield` into a caller object, and the
                    // walk-top fence asks the same of everything after it.
                    if !self.splice.is_empty() && !self.splice_store_is_local(array) {
                        if !self.relax_fence_by_chain(pc) {
                            return ir_build_bail(line!(), pc);
                        }
                        self.splice_committed_bases.insert(array);
                    }
                    // An `aastore` of a caller-visible reference into a local
                    // array taints it exactly as `putfield` does (round 11
                    // wave 15, lane `ir`). Without this, `a = new P[1]; a[0] =
                    // callerP; a[0].x = 1` read `a[0]` back as local and
                    // admitted a store into the caller's object.
                    if matches!(kind, MemKind::Ref) {
                        self.note_splice_store_value(array, value);
                    }
                    let store = self.graph.add(
                        Op::ArrayStore(kind),
                        IrType::Memory,
                        vec![self.ctrl, self.mem, array, index, value],
                        Some(pc),
                    );
                    self.plant_implicit_throw_edges(pc, store, self.mem, code_len);
                    self.mem = store;
                    pc += 1;
                }
                // arraylength — the array's immutable length word.
                //
                // The cheapest thing in this lane: one 32-bit load at a fixed
                // header offset behind a null check. No element type, no bounds
                // check, no barrier.
                //
                // It does NOT advance the memory token. Nothing in the JVM
                // writes an array's length, so `AliasClass::ArrayLength` can
                // never conflict with a write (`Graph::may_alias`), and putting
                // a pure read in the token chain would serialise every store
                // around it for nothing. The token is still an *input*, which is
                // what pins the node behind the writes that produced the array.
                0xbe => {
                    let array = self.pop();
                    if let Some(folded) = self.fold_frozen_array_length(code_len, pc, array) {
                        self.push(folded);
                        pc += 1;
                        continue;
                    }
                    let guard = if self.splice_handler_mark.is_some()
                        && ir_splice_covered_nested_guarded_enabled()
                    {
                        // The snapshot at this bytecode still has the array on
                        // its stack. A null resumes here, with all enclosing
                        // catches and completed effects intact.
                        if !self.relax_fence_by_chain(pc) {
                            return ir_build_bail(line!(), pc);
                        }
                        let null = self.aconst_null();
                        let non_null =
                            self.add_data(Op::Cmp(CmpOp::Ne), IrType::Int, vec![array, null], pc);
                        Some(self.graph.add(
                            Op::Guard { bci: pc },
                            IrType::Void,
                            vec![self.ctrl, non_null],
                            Some(pc),
                        ))
                    } else {
                        None
                    };
                    let mut inputs = vec![self.ctrl, self.mem, array];
                    if let Some(guard) = guard {
                        inputs.push(guard);
                    }
                    let len = self
                        .graph
                        .add(Op::ArrayLength, IrType::Int, inputs, Some(pc));
                    self.push(len);
                    pc += 1;
                }
                // athrow — cov-07. Pop the exception ref and terminate the
                // block: this never falls through and produces no value,
                // exactly like `ireturn`/`areturn` above — a later bytecode pc
                // is reachable only via a real branch target (goto/if), never
                // by falling out of this arm.
                //
                // Lowered as a call to `helpers.throw_exception` with THIS
                // instruction's own bci baked in as an immediate — the SAME
                // `(exc_ptr, bci)` call the single-pass backend's `0xbf` arm
                // makes (`x64/bytecode_walk.rs`), so handler resolution reuses
                // the exact machinery a callee-thrown or `checkcast`-thrown
                // exception already goes through:
                // `route_jit_exception_through_method`
                // (`vm/src/runtime/interpreter/exception_dispatch.rs`), run in
                // the INTERPRETER after the compiled frame exits via the
                // shared `i64::MIN` sentinel. See `Op::Throw`'s doc comment
                // for why this is not a second, divergent answer to where an
                // exception goes.
                0xbf => {
                    // Symmetry with the `return` refusal at the top of this
                    // loop, which `athrow` lacked until 2026-09-17.
                    //
                    // `Op::Throw` lowers to `helpers.throw_exception` plus the
                    // shared `i64::MIN` sentinel: the compiled frame *returns*,
                    // and the interpreter resolves the handler afterwards.
                    // Nothing on that path releases a monitor this frame took,
                    // so a `synchronized` region exited by an exception would
                    // leak the lock. Unlike `return`, the splice case is not an
                    // exception: a `return` inside a spliced callee hands a
                    // value back to the caller, whose monitors are legitimately
                    // still held, whereas an `athrow` leaves the WHOLE compiled
                    // frame no matter how deeply spliced it is — so this checks
                    // `self.monitors` unconditionally.
                    //
                    // The method was already excluded upstream, but only by
                    // accident: `precise_exception_frames` refuses it because a
                    // `synchronized` block always carries a synthetic
                    // `astore exc; aload mon; monitorexit; aload exc; athrow`
                    // handler that reads non-parameter locals. That is an
                    // accidental protection, not a stated one, and it
                    // evaporates the moment that gate widens. Stating it costs
                    // one comparison.
                    //
                    // Round 15 wave 1 (lane sync, SS8-2): except a self-locking
                    // body's method monitor, which `ir_lower` releases on that
                    // shared exception exit (`emit_call_exc_stub`): an admitted
                    // method has no exception table, so the throw always leaves
                    // it (JVMS 2.11.10's release case).
                    if !self.monitors.is_empty() && !self.holds_only_method_self_lock() {
                        // nio21 wave 18 (lane L3,
                        // `r21-chm-putval-has-no-ir-body-CLOSED-20261012.md`): the
                        // throw becomes a deopt to the interpreter AT this
                        // `athrow`, which runs it with the lock still held and
                        // reaches the block's release handler itself
                        // (`trap_athrow_in_monitor`). `ConcurrentHashMap.putVal`'s
                        // "Recursive update" arm (pc 377) refused the whole
                        // method here.
                        //
                        // The `Op::Throw` is still built behind the guard, as
                        // the terminator every path of the graph needs; it is
                        // unreachable at run time (the guard always fires),
                        // so its lock-leaking exit never runs.
                        if self.trap_athrow_in_monitor(pc) {
                            let exc = self.pop();
                            let throw = self.graph.add(
                                Op::Throw,
                                IrType::Void,
                                vec![self.ctrl, self.mem, exc],
                                Some(pc),
                            );
                            self.graph.exit = throw;
                            self.ctrl = NO_NODE;
                            pc += 1;
                            continue;
                        }
                        return ir_build_bail(line!(), pc);
                    }
                    // Round 16 wave 7 (lane arraylist, AL16-1): under
                    // `CRATONVM_JIT_IR_INLINE_COLD_ATHROW` the resolver admits a
                    // spliced body with `athrow`s only after masking each arm
                    // to `aconst_null; nop...; athrow`, for
                    // `prune_cold_throw_arm` to replace with a trap at its
                    // branch. Reaching one here means the arm was NOT replaced
                    // (no chain to resume through, a rotated or synchronized
                    // body): building it would throw `null` at the caller's
                    // bci. Refuse; the refusal names this splice, so the
                    // planner's retry keeps its call.
                    // `CRATONVM_JIT_IR_SPLICED_THROW_EDGES`: a spliced throw
                    // the compiling method's own handler catches is a guarded
                    // edge into that handler.
                    if !self.splice.is_empty() {
                        if let Some(&exc) = self.stack.last() {
                            if let Some(handler_pc) = self.spliced_throw_edge(pc, exc) {
                                let Some(next) = self.finish_spliced_throw(handler_pc) else {
                                    return ir_build_bail(line!(), pc);
                                };
                                self.ctrl = NO_NODE;
                                pc = next;
                                continue;
                            }
                        }
                    }
                    // JIT round 23 wave 5 (lane L1,
                    // `CRATONVM_JIT_IR_SPLICE_ATHROW_EXIT`): an UNMASKED spliced
                    // `athrow` (its exception is a real value, not the masked
                    // arm's `aconst_null`) becomes an always-failing guard
                    // through the chain snapshot at its own pc, instead of
                    // refusing the whole build. See `plant_spliced_athrow_exit`.
                    if !self.splice.is_empty() && self.plant_spliced_athrow_exit(pc) {
                        let exc = self.pop();
                        let throw = self.graph.add(
                            Op::Throw,
                            IrType::Void,
                            vec![self.ctrl, self.mem, exc],
                            Some(pc),
                        );
                        // The preceding precise guard always fails. This
                        // terminator is unreachable, including inside a covered
                        // nested span; record that proof for both exit checks.
                        if self.splice_handler_mark.is_some()
                            && ir_splice_handler_lower_proven_enabled()
                        {
                            self.graph.splice_handler_proven_exits.push(throw);
                            self.graph.splice_handler_proven_pcs.push(pc);
                        }
                        self.graph.exit = throw;
                        self.ctrl = NO_NODE;
                        pc += 1;
                        continue;
                    }
                    if self.inline_cold_athrow && !self.splice.is_empty() {
                        return ir_build_bail(line!(), pc);
                    }
                    let exc = self.pop();
                    // Round nio21 wave 1 (lane L2): a throw this method's own
                    // handler provably catches is a branch to that handler
                    // (`local_throw_target`; `CRATONVM_JIT_IR_LOCAL_THROW_EDGES`).
                    // JVMS 2.10 / 6.5 `athrow`: the operand stack is cleared,
                    // the exception pushed, the locals kept -- which is the
                    // frame this edge records at the handler's merge.
                    if let Some(handler_pc) = self.local_throw_target(pc, code_len, exc) {
                        self.guard_local_throw_events(pc, exc);
                        self.stack.clear();
                        self.push(exc);
                        self.add_merge_predecessor(handler_pc);
                        self.local_throw_edges += 1;
                    } else if self.local_throw_dynamic_edge(pc, code_len, exc).is_some() {
                        // The helper installed both the matching handler edge
                        // and the interpreter replay side exit for a type miss.
                    } else {
                        let throw = self.graph.add(
                            Op::Throw,
                            IrType::Void,
                            vec![self.ctrl, self.mem, exc],
                            Some(pc),
                        );
                        self.graph.exit = throw;
                    }
                    self.ctrl = NO_NODE;
                    pc += 1;
                }
                // checkcast / instanceof — cov-05. Both admitted ONLY for a
                // site whose target class is already resolved and loaded at
                // compile time: `checkcast_info`/`instanceof_info` are
                // populated in `lib.rs`'s `try_compile_inner` (reusing
                // `cp_new_resolver`, the same CONSTANT_Class resolver
                // `new`/`anewarray` already use, to answer "is it loaded" —
                // see that call site). A pc absent from the map — not yet
                // loaded, an unresolvable CP entry, or no resolver supplied at
                // all — bails this site (and so this whole method, same as
                // every other per-pc bail in this builder) to single-pass,
                // which resolves lazily via `jit_typecheck_resolve`'s
                // not-yet-loaded slow path — a path that can run a user
                // classloader's `loadClass`/`findClass`, arbitrary Java this
                // tier does not host inside a helper call.
                0xc0 => {
                    let (name_ptr, name_len) = match self.checkcast_info.get(&pc) {
                        Some(&info) => info,
                        // JIT round 23 wave 5 (`CRATONVM_JIT_IR_NULL_TYPECHECK_NO_RESOLVE`):
                        // a `checkcast` of the null constant to a class that is
                        // not loaded resolves nothing and passes (JVMS 6.5);
                        // the null stays on the stack. `ClassLoader.<init>`'s
                        // `(Certificate[]) null` is the shape.
                        None if self.top_is_null_const_operand() => {
                            pc += 3;
                            continue;
                        }
                        // Round nio20 wave 1, lane L1 (NIO19-3): a spliced
                        // body's type check whose class is not loaded, on an
                        // arm the callee's profile never ran -- a precise
                        // exit into the callee's frame at this bci.
                        None if self.unloaded_static_call_pcs.contains(&pc) => {
                            if !self.plant_unloaded_typecheck_exit(pc) {
                                return ir_build_bail(line!(), pc);
                            }
                            let _obj = self.pop();
                            let null = self.aconst_null();
                            self.push(null);
                            pc += 3;
                            continue;
                        }
                        // The named class is not loaded, so no path that has
                        // ever executed reached this `checkcast` -- trap here
                        // and compile the rest. `plant_uncommon_trap` carries
                        // the argument; the operand is replaced by a null so
                        // the abstract stack keeps the depth the verifier
                        // proved, and the code that reads it is unreachable.
                        None => {
                            if !self.plant_uncommon_trap(
                                code,
                                code_len,
                                pc,
                                TrapCause::UnresolvedTypeCheck,
                            ) {
                                return ir_build_bail(line!(), pc);
                            }
                            let _obj = self.pop();
                            // A `Ref`-typed null, not `iconst(0)`: the value
                            // this arm replaces is a reference, and a
                            // mistyped stack entry would be joined against a
                            // real reference at the next merge. Wave 8b: a
                            // fresh one after an in-place trap
                            // (`trap_placeholder_ref`).
                            let null = self.trap_placeholder_ref();
                            self.push(null);
                            pc += 3;
                            continue;
                        }
                    };
                    let obj = self.pop();
                    // A null passes every `checkcast` unchanged (JVMS 6.5), and
                    // the target is already resolved (`checkcast_info` holds
                    // only compile-time-resolved names), so a constant null
                    // needs no node: javac's `(String) null` argument, which a
                    // spliced `assertEquals(a, b)` forwards on every call.
                    if is_null_const(&self.graph.nodes[obj as usize])
                        || self.box_call_passes_checkcast(obj, name_ptr, name_len)
                    {
                        self.push(obj);
                        pc += 3;
                        continue;
                    }
                    if self.splice_handler_mark.is_some() && self.relax_fence_by_chain(pc) {
                        let header_only = crate::ir_instanceof_header_read_enabled();
                        let is_inst = self.graph.add(
                            Op::InstanceOf {
                                name_ptr,
                                name_len,
                                seen_cell: 0,
                                header_only,
                            },
                            IrType::Int,
                            vec![self.ctrl, self.mem, obj],
                            Some(pc),
                        );
                        if !(header_only && crate::ir_instanceof_off_chain_enabled()) {
                            self.mem = is_inst;
                        }
                        let null = self.aconst_null();
                        let is_null =
                            self.add_data(Op::Cmp(CmpOp::Eq), IrType::Int, vec![obj, null], pc);
                        let ok = self.add_data(Op::Or, IrType::Int, vec![is_null, is_inst], pc);
                        self.graph.add(
                            Op::Guard { bci: pc },
                            IrType::Void,
                            vec![self.ctrl, ok],
                            Some(pc),
                        );
                        self.push(obj);
                        pc += 3;
                        continue;
                    }
                    let result = self.graph.add(
                        Op::CheckCast { name_ptr, name_len },
                        IrType::Ref,
                        vec![self.ctrl, self.mem, obj],
                        Some(pc),
                    );
                    // Opaque: same reasoning as `instanceof` below, plus a
                    // definitive refusal stashes a pending ClassCastException
                    // and returns the deopt sentinel — see `Op::CheckCast`.
                    self.mem = result;
                    self.push(result);
                    pc += 3;
                }
                0xc1 => {
                    let (name_ptr, name_len) = match self.instanceof_info.get(&pc) {
                        Some(&info) => info,
                        // JIT round 23 wave 5 (`CRATONVM_JIT_IR_NULL_TYPECHECK_NO_RESOLVE`):
                        // `null instanceof C` is 0 with no resolution (JVMS 6.5).
                        None if self.top_is_null_const_operand() => {
                            let _obj = self.pop();
                            let zero = self.iconst(0);
                            self.push(zero);
                            pc += 3;
                            continue;
                        }
                        // Round nio20 wave 1, lane L1 (NIO19-3): as the
                        // `checkcast` arm above.
                        None if self.unloaded_static_call_pcs.contains(&pc) => {
                            if !self.plant_unloaded_typecheck_exit(pc) {
                                return ir_build_bail(line!(), pc);
                            }
                            let _obj = self.pop();
                            let zero = self.iconst(0);
                            self.push(zero);
                            pc += 3;
                            continue;
                        }
                        // Same argument as the `checkcast` arm above: an
                        // unloaded class has been reached by nothing.
                        None => {
                            if !self.plant_uncommon_trap(
                                code,
                                code_len,
                                pc,
                                TrapCause::UnresolvedTypeCheck,
                            ) {
                                return ir_build_bail(line!(), pc);
                            }
                            let _obj = self.pop();
                            let zero = self.iconst(0);
                            self.push(zero);
                            pc += 3;
                            continue;
                        }
                    };
                    let obj = self.pop();
                    // A null is never an instance (JVMS 6.5); the target is
                    // resolved, as in the `checkcast` arm above.
                    if is_null_const(&self.graph.nodes[obj as usize]) {
                        let zero = self.iconst(0);
                        self.push(zero);
                        pc += 3;
                        continue;
                    }
                    // JIT r21 wave 2 (lane typecheck, TYPECHECK21-1 part A):
                    // under `CRATONVM_JIT_IR_INSTANCEOF_HEADER_READ` the test
                    // is a read of the operand's immutable header in the
                    // memory model (`MemAccess::HeaderRead`), and under
                    // `CRATONVM_JIT_IR_INSTANCEOF_OFF_CHAIN` as well it also
                    // produces no memory token, exactly like `arraylength`
                    // (it consumes `self.mem`, so it still runs after the
                    // writes before it; nothing later is ordered behind it).
                    // Sound for this site because the tier admits only a
                    // compile-time-resolved target: `jit_instanceof` then runs
                    // no Java code, raises nothing (no sentinel check is
                    // lowered) and writes no Java-visible memory.
                    let header_only = crate::ir_instanceof_header_read_enabled();
                    let result = self.graph.add(
                        Op::InstanceOf {
                            name_ptr,
                            name_len,
                            seen_cell: 0,
                            header_only,
                        },
                        IrType::Int,
                        vec![self.ctrl, self.mem, obj],
                        Some(pc),
                    );
                    // Opaque unless header_only && off_chain (the helper can still
                    // allocate a `java/lang/Class` mirror on first touch even
                    // though the target class is already loaded — see
                    // `Op::InstanceOf`'s doc comment).
                    if !(header_only && crate::ir_instanceof_off_chain_enabled()) {
                        self.mem = result;
                    }
                    self.push(result);
                    pc += 3;
                }
                // dup_x1 — insert a copy of the top value below the second.
                //
                // A stack shuffle rather than an access; it is in this lane only
                // because it appears in the same method bodies (six events).
                // The abstract stack holds one entry per VALUE, so the shuffle
                // is three pushes — but only if BOTH operands are category 1,
                // which is what JVMS §dup_x1 requires. A category-2 operand
                // would mean this builder's one-entry-per-value stack and the
                // verifier's two-slot stack disagree about what "the value
                // below" names, so refuse rather than shuffle the wrong entry.
                0x5a => {
                    let (Some(v1), Some(v2)) = (self.pop_opt(), self.pop_opt()) else {
                        return ir_build_bail(line!(), pc);
                    };
                    let is_cat2 = |ty: IrType| matches!(ty, IrType::Long | IrType::Double);
                    if is_cat2(self.graph.nodes[v1 as usize].ty)
                        || is_cat2(self.graph.nodes[v2 as usize].ty)
                    {
                        return ir_build_bail(line!(), pc);
                    }
                    self.push(v1);
                    self.push(v2);
                    self.push(v1);
                    pc += 1;
                }
                // getfield — read an instance field as an `Op::Load`.
                //
                // Slice 1 (read-only) of the field/call IR frontier. An
                // int-category field (`I`/`Z`/`B`/`C`/`S`) reads the 32-bit
                // `Value::Int` payload sign-extended — the exact ABI the
                // single-pass backend's inline getfield emits (`MOVSXD` from
                // `HEADER_SIZE + field_index*SLOT_SIZE +
                // FIELD_CELL_PAYLOAD32_OFFSET`). A reference field reads the raw
                // pointer, and (COV-03) a `J`/`F`/`D` field the long payload or
                // the FP bit pattern, both through the checked helper. The
                // cell-offset operand is a `Const(field_index)`; the lowerer
                // derives the byte displacement. A pc without resolved layout
                // bails (`None` → single-pass).
                0xb4 => {
                    // Compact reference-field layout packs field offsets (refs
                    // to 8 bytes), so the `HEADER_SIZE + field_index*SLOT_SIZE`
                    // displacement is wrong for a compact object. That is a
                    // property of ONE of `Op::Load`'s two lowerings, not of the
                    // opcode, and this used to be a blanket bail here.
                    //
                    // The blanket bail was the single largest exclusion in the
                    // optimizing tier. `compact_ref_fields_enabled()` defaults
                    // to TRUE, so it refused every method containing a
                    // `getfield` — which is most object-oriented Java — and it
                    // did so at stage one of the pipeline, invisibly. That is
                    // why the relocation-map contract could never be exercised:
                    // every probe written for it read a field
                    // (jit-ir-relocation-map-contract.md).
                    //
                    // The constraint belongs where the displacement is emitted.
                    // `ir_lower` lowers `Op::Load` through the checked
                    // `jit_getfield` helper whenever the helper address is
                    // present, and that helper IS compact-aware
                    // (`jit_compact_field_slot`); only the inline
                    // displacement fallback, taken when the helper is absent
                    // (the JIT unit tests' stub table), is layout-naive. So
                    // `ir_lower::lower_inner` refuses the graph in exactly that
                    // combination and the builder no longer refuses the opcode.
                    //
                    // A VOLATILE field is refused unless admitted: a hoisted or
                    // merged read of a volatile flag is a spin loop that never
                    // ends. Admitted, the load is marked and every pass orders
                    // it. See `set_volatile_field_pcs`.
                    let is_volatile = self.volatile_field_pcs.contains(&pc);
                    if is_volatile && !self.volatile_loads_admitted {
                        return ir_build_bail(line!(), pc);
                    }
                    // JIT round 20 wave 20, lane rev16: a spliced
                    // `Reference.referent` read (`Graph::referent_load_pcs`,
                    // xLib) is ordered like a volatile one: the GC clears the
                    // field under the loop, so it must be re-read per
                    // iteration and never forwarded or hoisted
                    // (`while (r.get() != null)` ends once the referent is
                    // cleared; HotSpot's `inline_reference_get` fences the
                    // load with a `MemBarCPUOrder` for the same reason). It
                    // is a plain read on x86-64, so admission is not asked.
                    let is_volatile = is_volatile || self.graph.referent_load_pcs.contains(&pc);
                    // Copied out: the trap arm below needs `&mut self`.
                    let row = self.field_info.get(&pc).copied();
                    let (field_index, type_tag) = match row {
                        Some(fi) => fi,
                        // Wave 94, lane L2: an admitted unloaded-owner site
                        // traps (`plant_field_site_trap`).
                        None if self.plant_field_site_trap(code, code_len, pc, 0xb4) => {
                            pc += 3;
                            continue;
                        }
                        None => return self.rowless_field_site_bail(line!(), pc, 0xb4),
                    };
                    // Reference fields (`L…;` / `[…`) read through the SAME
                    // `jit_getfield` helper: it already returns the raw pointer
                    // for a compact reference slot (0 for null, and an
                    // implausible word degraded to null), and `i64::MIN` stays
                    // unambiguous because no plausible heap pointer can equal
                    // it. Only the node type differs — `Ref`, so every
                    // safepoint map publishes the result slot as a rewritable
                    // root. `ir_lower::lower_inner` refuses the graph when the
                    // helper is absent (the unit-test stub table), because the
                    // inline displacement fallback decodes an int cell.
                    //
                    // This was the single largest builder refusal measured on a
                    // real workload (66 of 141 on a Hibernate class): a
                    // reference field read is most of what object-oriented Java
                    // does.
                    // COV-03 (wide fields): a `J`/`F`/`D` field reads through
                    // the SAME helper — it already returns the long payload or
                    // the float/double BITS — so the only thing that differs is
                    // the node type and, for `J`/`D`, the `i64::MIN`-sentinel
                    // collision the lowerer must disambiguate through
                    // `jit_dispatch_threw` (the identical problem, and the
                    // identical answer, as a `J`/`D` `Op::Call` return). The
                    // inline compact fast path refuses these tags on its own, so
                    // a wide field simply takes the helper arm.
                    let Some(ty) = self.field_access_type(type_tag) else {
                        return ir_build_bail(line!(), pc);
                    };
                    let base = self.pop();
                    if !is_volatile {
                        if let Some(v) = self.box_call_payload(base, pc, type_tag) {
                            self.push(v);
                            pc += 3;
                            continue;
                        }
                        if let Some(obj_slot) = self.const_ref_slot_of(base) {
                            if let Some(fold) = self
                                .const_folds
                                .const_field_folds
                                .get(&(obj_slot, field_index as u32))
                                .cloned()
                            {
                                let v = match fold {
                                    ConstFieldFold::Ref {
                                        slot_addr,
                                        exact_class_id,
                                    } => {
                                        let node = self.graph.add(
                                            Op::IndyRootRef { slot_addr },
                                            IrType::Ref,
                                            vec![self.ctrl],
                                            None,
                                        );
                                        self.const_ref_slots.insert(node, slot_addr);
                                        self.record_folded_root_ref_meta(slot_addr, exact_class_id);
                                        if exact_class_id != 0 {
                                            self.const_ref_exact_classes
                                                .insert(node, exact_class_id);
                                        }
                                        node
                                    }
                                    ConstFieldFold::I32(val) => self.iconst(val as i64),
                                    ConstFieldFold::I64(val) => self.lconst(val),
                                };
                                self.push(v);
                                pc += 3;
                                continue;
                            }
                        }
                    }
                    // Round 15 wave 1 (lane chain, G14-1): an earlier read of
                    // the same `(base, field)` nothing can have overwritten
                    // since. See `field_load_memo_forward`. nio21 wave 2
                    // (lane L3): a `final` field forwards by default.
                    let is_final = self.final_field_pcs.contains(&pc);
                    if let Some(prev) =
                        self.field_load_memo_forward(base, field_index, ty, is_volatile, is_final)
                    {
                        // Round nio20 wave 4 (lane L1, W3L4-2): inside a
                        // splice, the read still happened for the replay rules.
                        self.note_splice_forwarded_read(base);
                        self.note_byte_array_field_read(pc, prev);
                        self.push(prev);
                        pc += 3;
                        continue;
                    }
                    // `CRATONVM_FROZEN_PAGES`: fold CHA-effectively-final or
                    // `@Stable` fields directly to constants / `IndyRootRef`.
                    if !is_volatile && self.frozen_facts.is_some() {
                        if let Some(folded) = self
                            .fold_frozen_field(code_len, pc, base, field_index, ty)
                            .and_then(|val| self.materialize_frozen_value(pc, ty, val))
                        {
                            self.note_byte_array_field_read(pc, folded);
                            self.push(folded);
                            pc += 3;
                            continue;
                        }
                    }
                    let offset = self.iconst(field_index as i64);
                    let mem_kind = match ty {
                        IrType::Ref => MemKind::Ref,
                        IrType::Long => MemKind::Long,
                        IrType::Float => MemKind::Float,
                        IrType::Double => MemKind::Double,
                        _ => MemKind::Int,
                    };
                    let load = self.graph.add(
                        Op::Load(mem_kind),
                        ty,
                        vec![self.ctrl, self.mem, base, offset],
                        Some(pc),
                    );
                    if is_volatile {
                        self.graph.set_volatile_access(load);
                    } else if is_final {
                        self.graph.final_java_field_reads.insert(load);
                    }
                    // j27 wave 1 (lane TYPECHK,
                    // `CRATONVM_JIT_IR_REFERENT_SPLICE_COMPACT_ROW`): a spliced
                    // `Reference.get` referent read carries the referent's
                    // compact row (the VM's; Gen / G1 only), so a compact
                    // receiver reads inline behind the SATB pre-gate.
                    if ty == IrType::Ref
                        && field_index == 0
                        && self.graph.referent_load_pcs.contains(&pc)
                    {
                        self.note_spliced_referent_row(load);
                    }
                    // nio21 wave 6 L2 (L2W5-1): a null receiver's NPE into a
                    // handler of this body (`plant_implicit_throw_edges`).
                    if !self.receiver_cannot_be_null(base) {
                        self.plant_implicit_throw_edges(pc, load, self.mem, code_len);
                    }
                    self.note_splice_local_load(load, base);
                    self.note_splice_field_read_alias(base);
                    // The load advances the memory token: a subsequent store to
                    // a possibly-aliasing location must be ordered AFTER this
                    // read (WAR), and the scheduler enforces ordering only via
                    // the input-edge dependency this creates. (For a getfield-
                    // only method this merely serialises reads — harmless.)
                    self.mem = load;
                    self.push(load);
                    // Round 15 wave 3 (lane chain2, CHN15-2): the field's
                    // declaring class, before the memo classifies the load.
                    self.note_field_decl_class(pc, load);
                    self.note_byte_array_field_read(pc, load);
                    self.field_load_memo_record(base, field_index, load, is_volatile);
                    pc += 3;
                }
                // putfield — write an instance field as an `Op::Store`.
                //
                // Slice 2 (read/write) of the field/call IR frontier. The store
                // consumes the current memory token and produces a new one (the
                // store node itself), so the scheduler serialises it after every
                // prior memory op and before every later one (RAW/WAR/WAW all
                // preserved by the input-edge topological sort). Any pc without
                // resolved layout bails (`None` → single-pass).
                0xb5 => {
                    // See the getfield (0xb4) note. This arm accepts exactly the
                    // tags that arm accepts — one classifier, two call sites —
                    // and the DIFFERENCE between a read and a write is carried
                    // entirely by `MemKind`, which selects the lowering:
                    //
                    //   * `Int`  → the inline store, or `jit_putfield_int` when
                    //     compact layout is on (an int cell has no barrier).
                    //   * `Ref`  → ALWAYS `jit_putfield_object`, which is the
                    //     single-pass backend's own full-barrier route: SATB
                    //     pre-barrier on the OLD reference, then the collector's
                    //     post-write barrier (G1's remembered-set edge included).
                    //     This — not the compact layout — is the reason the
                    //     reference case was missing here while `getfield` had
                    //     it: a reference LOAD needs no barrier, so the fix that
                    //     added it there had nothing to say about this arm.
                    //     `jit_putfield_object` is compact-aware in its own
                    //     right, so there is no separate compact refusal to make.
                    //   * `Long`/`Float`/`Double` → the matching
                    //     `jit_putfield_{long,float,double}` helper, gated on the
                    //     same long/FP flags the read arm uses.
                    //
                    // `ir_lower::lower_inner` refuses the graph when the helper a
                    // given kind needs is absent (the unit-test stub table).
                    //
                    // A VOLATILE field is refused unless admitted, which needs
                    // the lowerer's StoreLoad fence as well as the flag
                    // (`IR_LOWER_FENCES_VOLATILE_STORES`). See
                    // `set_volatile_field_pcs`.
                    let is_volatile = self.volatile_field_pcs.contains(&pc);
                    if is_volatile && !self.volatile_stores_admitted {
                        return ir_build_bail(line!(), pc);
                    }
                    let row = self.field_info.get(&pc).copied();
                    let (field_index, type_tag) = match row {
                        Some(fi) => fi,
                        // Wave 94, lane L2: see the getfield arm.
                        None if self.plant_field_site_trap(code, code_len, pc, 0xb5) => {
                            pc += 3;
                            continue;
                        }
                        None => return self.rowless_field_site_bail(line!(), pc, 0xb5),
                    };
                    let Some(ty) = self.field_access_type(type_tag) else {
                        return ir_build_bail(line!(), pc);
                    };
                    let mem_kind = match ty {
                        IrType::Ref => MemKind::Ref,
                        IrType::Long => MemKind::Long,
                        IrType::Float => MemKind::Float,
                        IrType::Double => MemKind::Double,
                        _ => MemKind::Int,
                    };
                    let value = self.pop();
                    let base = self.pop();
                    // A `byte`/`short`/`char`/`boolean` field holds the value
                    // NARROWED to its type (JVMS §6.5 putfield; the
                    // interpreter's `narrow_int_to_field_type`). Every such
                    // field is `MemKind::Int` here, so without this the store
                    // wrote the whole int and IR scalar replacement forwarded
                    // it to the next `getfield` unchanged: `sipush 300;
                    // putfield B; getfield B` read 300 compiled and 44
                    // interpreted (round 9 wave 2,
                    // `sub-int-field-stores-are-not-narrowed-by-compiled-code`).
                    // Narrowing the VALUE covers the store, the forwarded load
                    // and any snapshot that names it, with no new `MemKind`.
                    let value = self.narrow_to_field_type(value, type_tag, pc);
                    // JIT r23 wave 11b (lane L5,
                    // `CRATONVM_JIT_IR_SPLICE_VOLATILE_STORE_FRESH`): a spliced
                    // volatile store the fresh rule alone admitted must go into
                    // an unpublished allocation of this compile.
                    if is_volatile
                        && !self.splice.is_empty()
                        && ir_splice_volatile_store_needs_fresh_receiver()
                    {
                        if let Some(why) =
                            spliced_volatile_store_fresh_refusal(&self.graph, self.mem, base)
                        {
                            if ir_bail_reporting() || crate::x64::dbg_jitc_enabled() {
                                eprintln!(
                                    "[ir] spliced volatile store REFUSED at combined pc {pc} in {}: {why}",
                                    self.method_label.as_deref().unwrap_or("<unknown>")
                                );
                            }
                            return ir_build_bail(line!(), pc);
                        }
                    }
                    // See `splice_store_is_local`. A store into an object the
                    // splice did not allocate is committed instead: the walk
                    // then refuses anything that could trap before the splice
                    // ends (`admits_after_committed_splice_store`), so no
                    // re-executed `invoke` can repeat it.
                    //
                    // `CRATONVM_JIT_IR_SPLICE_COMMITTED_STORE=0` (token
                    // `ir-splice-committed-store`, default ON since round 11
                    // wave 11) restores the old refusal of any such store.
                    if !self.splice.is_empty() {
                        // A constructor receiver this method allocated itself
                        // (`splice_store_is_ctor_receiver`, dev 2026-09-24)
                        // is replay-safe like a splice-local object.
                        if self.splice_store_is_local(base)
                            || self.splice_store_is_ctor_receiver(base)
                        {
                            self.note_splice_store_value(base, value);
                        } else {
                            self.splice_committed_bases.insert(base);
                        }
                    }
                    let offset = self.iconst(field_index as i64);
                    let store = self.graph.add(
                        Op::Store(mem_kind),
                        IrType::Memory,
                        vec![self.ctrl, self.mem, base, offset, value],
                        Some(pc),
                    );
                    if is_volatile {
                        self.graph.set_volatile_access(store);
                    }
                    // nio21 wave 6 L2 (L2W5-1): as the `getfield` arm.
                    if !self.receiver_cannot_be_null(base) {
                        self.plant_implicit_throw_edges(pc, store, self.mem, code_len);
                    }
                    // Round 15 wave 3 (lane chain2, CHN15-2).
                    self.note_field_decl_class(pc, store);
                    self.mem = store;
                    // Round 16 (lane fieldguard, CHN15-5 (a)): the cell now
                    // holds `value` (already narrowed to the field's type).
                    // See `field_load_memo_record_store`.
                    self.field_load_memo_record_store(
                        base,
                        field_index,
                        store,
                        value,
                        mem_kind,
                        is_volatile,
                    );
                    pc += 3;
                }
                // monitorenter / monitorexit — emitted so escape analysis can
                // see the lock and offer an elision or coarsening plan. Both
                // advance the memory token: they are ordering barriers with JMM
                // acquire/release semantics and both are safepoints, so nothing
                // may be reordered across them.
                //
                // `ir_lower` refuses the graph when the helper table carries no
                // monitor entry, so a backend that cannot lower these declines
                // the method rather than silently emitting nothing.
                //
                // Both also maintain the abstract monitor stack every snapshot
                // copies. `monitorenter` pushes the locked reference;
                // `monitorexit` must release the innermost one, and the build is
                // refused when it names anything else (unstructured locking, or
                // a shape the frame state could not describe). A splice never
                // takes a monitor: the resolver admits no callee body with
                // `monitorenter`, and a spliced region's frame is the caller's
                // snapshot at the `invoke`, which could not describe one.
                0xc2 | 0xc3 => {
                    let obj = self.pop();
                    // Round 15 wave 1 (lane sync, SS8-2): a self-locking body
                    // takes no bytecode monitor (its admission refuses one; a
                    // bytecode region would sit on top of the method monitor
                    // every frame names, a shape the hand-over does not have).
                    if obj == NO_NODE
                        || !self.splice.is_empty()
                        || self.graph.method_self_lock.is_some()
                    {
                        return ir_build_bail(line!(), pc);
                    }
                    if code[pc] == 0xc2 {
                        self.monitors.push(obj);
                    } else {
                        let matches_innermost = self
                            .monitors
                            .last()
                            .is_some_and(|&held| self.same_monitor_object(held, obj));
                        if !matches_innermost {
                            return ir_build_bail(line!(), pc);
                        }
                        self.monitors.pop();
                    }
                    let op = if code[pc] == 0xc2 {
                        Op::MonitorEnter
                    } else {
                        Op::MonitorExit
                    };
                    let mon = self.graph.add(
                        op,
                        IrType::Memory,
                        vec![self.ctrl, self.mem, obj],
                        Some(pc),
                    );
                    self.mem = mon;
                    pc += 1;
                }
                // new — allocate an object as an `Op::New`. Emitted so escape
                // analysis can scalar-replace it when it does not escape (no
                // heap allocation, fields become SSA values). An escaping
                // allocation survives optimization and is emitted through the
                // shared compact-layout/TLAB-aware runtime lowering stub.
                0xbb => {
                    // JIT round 19 wave 3, lane eager3 (EAGER2-3,
                    // `CRATONVM_JIT_UNINIT_NEW_GUARD`): a LOADED class that
                    // owes its initialization, admitted by the VM to a real
                    // class-initialized guard. Its row is used only behind the
                    // guard; a refused plant takes the row-less path below
                    // (the EAGER19-1 trap, else the bail), as if the row were
                    // absent. Never inside a splice (outer-method pcs only).
                    let guard_refused = match self.uninit_new_guard_sites.get(&pc).copied() {
                        Some((state_addr, initialized)) => {
                            !(self.splice.is_empty()
                                && self.plant_class_init_guard(
                                    code,
                                    code_len,
                                    pc,
                                    state_addr,
                                    initialized,
                                ))
                        }
                        None => false,
                    };
                    let row = if guard_refused {
                        None
                    } else {
                        self.new_info.get(&pc).copied()
                    };
                    let (class_id, num_fields) = match row {
                        Some(ci) => ci,
                        // JIT round 20 wave 19 (lane ySplice, SPLICE20-2): a
                        // spliced `new` whose class is not loaded, admitted by
                        // the resolver on an arm the callee's profile never
                        // ran -- a precise exit into the callee's frame at
                        // this bci (`plant_unloaded_new_exit`). The `null`
                        // keeps the verified stack depth; the code that reads
                        // it is unreachable.
                        None if !self.splice.is_empty()
                            && self.unloaded_static_call_pcs.contains(&pc) =>
                        {
                            if !self.plant_unloaded_new_exit(pc) {
                                return ir_build_bail(line!(), pc);
                            }
                            let null = self.aconst_null();
                            self.push(null);
                            pc += 3;
                            continue;
                        }
                        // `JitNewSite::Deferred`: the class is not loaded, so
                        // nothing has ever run this `new`. Trap and keep the
                        // method. Note the `<init>` that follows names the same
                        // unloaded class and so has no `invoke_info` either --
                        // it takes the unplanned-invoke trap for the same
                        // reason, and the two together are what actually saves
                        // the method. Item 7's look budget is the other half of
                        // this story: it stops the retry sweep re-resolving a
                        // site that traps perfectly well.
                        None => {
                            // JIT round 19 wave 2, lane eager2 (EAGER19-1): a
                            // LOADED class that owes its initialization, which
                            // the VM admitted to the trap (one shot per bci;
                            // the trap asks the recompile that sees the class
                            // initialized). Never inside a splice: the set is
                            // keyed by this method's own pcs.
                            let uninit_trap = self.splice.is_empty()
                                && self.uninit_new_trap_pcs.contains(&pc)
                                && self.plant_uncommon_trap(
                                    code,
                                    code_len,
                                    pc,
                                    TrapCause::UninitNew,
                                );
                            // nio21 wave 15 (lane L3): the site the single-pass
                            // tier already compiles -- a call of the CP-indexed
                            // `new_object_cp` helper, which resolves and
                            // initialises the class at run time exactly as the
                            // interpreter's `new` does ([`Op::NewCp`]). Tried
                            // before the trap: it is correct on every path, the
                            // trap only on a path that never runs. Outside a
                            // splice only (the row names this method's pool).
                            // A pc the uninitialized-class trap took
                            // (EAGER19-1, above) is not built again.
                            if !uninit_trap && self.splice.is_empty() && pc < code_len {
                                if let Some(&(holder_class_id, cp_idx)) =
                                    self.new_deferred_sites.get(&pc)
                                {
                                    let newobj = self.graph.add(
                                        Op::NewCp {
                                            holder_class_id,
                                            cp_idx,
                                        },
                                        IrType::Ref,
                                        vec![self.ctrl, self.mem],
                                        Some(pc),
                                    );
                                    self.mem = newobj;
                                    self.push(newobj);
                                    if ir_bail_reporting() {
                                        eprintln!(
                                            "[ir] deferred new at bytecode pc {pc} in {} -- built \
                                             as Op::NewCp (holder {holder_class_id}, cp {cp_idx}): \
                                             the class resolves at run time through new_object_cp",
                                            self.method_label.as_deref().unwrap_or("<unknown>"),
                                        );
                                    }
                                    pc += 3;
                                    continue;
                                }
                            }
                            if !uninit_trap
                                && !self.plant_uncommon_trap(
                                    code,
                                    code_len,
                                    pc,
                                    TrapCause::UnresolvedNew,
                                )
                            {
                                if ir_bail_reporting() {
                                    eprintln!(
                                        "[ir] deferred new REFUSED at bytecode pc {pc} in {}: the \
                                         class is not loaded and {}",
                                        self.method_label.as_deref().unwrap_or("<unknown>"),
                                        if !self.splice.is_empty() || pc >= code_len {
                                            "the site is inside a spliced body (no CP-indexed row)"
                                        } else if !ir_deferred_new_enabled() {
                                            "CRATONVM_JIT_IR_DEFERRED_NEW=0"
                                        } else {
                                            "the door supplied no deferred row (no resolver or \
                                             new_object_cp unwired)"
                                        },
                                    );
                                }
                                return ir_build_bail(line!(), pc);
                            }
                            let null = self.aconst_null();
                            self.push(null);
                            pc += 3;
                            continue;
                        }
                    };
                    let newobj = self.graph.add(
                        Op::New {
                            class_id,
                            num_fields,
                        },
                        IrType::Ref,
                        vec![self.ctrl, self.mem],
                        Some(pc),
                    );
                    self.note_splice_local_alloc(newobj);
                    self.uninitialized_news.insert(newobj);
                    self.push(newobj);
                    pc += 3;
                }
                // newarray — allocate a PRIMITIVE array (cov-06, first
                // increment). No class resolution, no deferred path: the
                // atype immediate names the element kind directly, so this
                // proves the `Op::NewArray` shape with nothing else attached.
                // Escape analysis may scalar-replace a small constant-length
                // primitive array (`escape_analysis::field_in_range`,
                // `ea_ir_bridge`'s constant-index mapping); one that survives
                // goes to `ir_lower`'s shared allocation stub, which calls the
                // same `jit_newarray` helper the single-pass backend's 0xbc
                // arm does.
                0xbc => {
                    let atype = code[pc + 1];
                    let len = self.pop();
                    let arr = self.graph.add(
                        Op::NewArray {
                            element_type: atype,
                            component_class_id: 0,
                        },
                        IrType::Ref,
                        vec![self.ctrl, self.mem, len],
                        Some(pc),
                    );
                    self.note_splice_local_alloc(arr);
                    self.push(arr);
                    pc += 2;
                }
                // anewarray — allocate a REFERENCE array of an
                // already-loaded component class (cov-06, second increment).
                // `element_type: 0` is not a valid JVM atype (they start at
                // 4), so it doubles as the "this is a reference array"
                // discriminant `ir_lower` and `jit_anewarray_object` share.
                //
                // A `Deferred` site (component class not loaded at compile
                // time) has no entry in `anewarray_info` — `lib.rs` never
                // inserts one — so it bails this method to single-pass here,
                // exactly like an unresolved `new`. `ir_compatible` no longer
                // refuses every `anewarray`-bearing method outright; only the
                // per-site resolution gates it, the same model `new` already
                // uses.
                0xbd => {
                    let component_class_id = match self.anewarray_info.get(&pc) {
                        Some(&id) => id,
                        None => return ir_build_bail(line!(), pc),
                    };
                    let len = self.pop();
                    let arr = self.graph.add(
                        Op::NewArray {
                            element_type: 0,
                            component_class_id,
                        },
                        IrType::Ref,
                        vec![self.ctrl, self.mem, len],
                        Some(pc),
                    );
                    self.note_splice_local_alloc(arr);
                    self.push(arr);
                    pc += 3;
                }
                // multianewarray (round 16, lane matrix;
                // `CRATONVM_JIT_IR_MULTIANEWARRAY`): one `Op::MultiNewArray`
                // calling the single-pass tier's own `multianewarray_n`
                // helper. The dimensions are popped innermost-first and put
                // back in source order, so the node's inputs are `[ctrl, mem,
                // dim0, …]` -- outermost first, the order the helper reads its
                // buffer in. The node is the new memory token: resolving a
                // level's class can run a user class loader.
                //
                // Only a site the door resolved (`multianewarray_sites`), and
                // never inside a splice: the site table is keyed by the
                // compiling method's own pcs. The snapshot this bci got above
                // is the frame the call's exception edge resolves.
                0xc5 => {
                    let ndims = usize::from(code[pc + 3]);
                    let site = match self.multianewarray_sites.get(&pc) {
                        Some(&site) if ndims >= 1 && self.splice.is_empty() => site,
                        _ => return ir_build_bail(line!(), pc),
                    };
                    let mut dims = Vec::with_capacity(ndims);
                    for _ in 0..ndims {
                        dims.push(self.pop());
                    }
                    dims.reverse();
                    let mut inputs = Vec::with_capacity(2 + ndims);
                    inputs.push(self.ctrl);
                    inputs.push(self.mem);
                    inputs.extend(dims);
                    let arr =
                        self.graph
                            .add(Op::MultiNewArray { site }, IrType::Ref, inputs, Some(pc));
                    self.mem = arr;
                    self.push(arr);
                    pc += 4;
                }
                // invokespecial — two lowerings, tried in this order:
                //
                //  1. ELISION of a trivial `<init>()V` on a fresh object: pop
                //     the `dup`'d receiver and emit nothing. Sound only because
                //     the caller admits the pc to `trivial_init_pcs` exclusively
                //     when the object is a `new` whose class has no primitive
                //     field initialisers (its fields are zero-initialised and
                //     set by the visible `putfield`s — the constructor adds
                //     nothing the scalar-replaced slots don't already model),
                //     AND because the receiver is a fresh `Op::New` this pass
                //     emitted. Preferred when it applies: it is the whole point
                //     of scalar replacement, and a call here would pin the
                //     allocation (`Op::Call` arg-escapes its reference inputs).
                //
                //  2. an ordinary statically-bound CALL — a `super.m()`, a
                //     private method, or (cov-04) a real constructor: a
                //     `super(...)`/`this(...)` chain call, or the `<init>` of a
                //     `new` the elision analysis declined.
                //
                // cov-04 measured this arm on the three Spring Boot workloads
                // that carry 1,947 of the survey's 1,954 compile requests, at
                // `48fba3a31` — **every** refusal here was an `<init>`, and
                // **none** was a non-`<init>` `invokespecial` the arm had no
                // path for. 29 of 52 were a `super(...)`/`this(...)` chain call
                // in a compiled constructor (receiver `this`, no `new` in the
                // method at all) and 23 were a `new X(args)` site. So the
                // ordering above is not a preference between two common cases —
                // case 1 is rare (`is_elidable_construction` admits only a
                // 5-byte `aload_0; invokespecial Object.<init>()V; return`
                // body) and case 2 is the arm's real work.
                //
                // The COUNTS are baseline-relative and should not be quoted
                // without their tree: the same measurement re-run after `cov-01`
                // and `cov-02` landed reads 106, not 52, because a method
                // blocked on `ldc` never reached its `invokespecial`. The
                // *shape* — all of them `<init>` — held at every baseline.
                //
                // Calling an `<init>` runs every side effect the elision path
                // was allowed to skip, which is the safe direction: the hazard
                // named at the elision site is the other one.
                0xb7 => {
                    // Defence in depth on the elision: only elide when the
                    // receiver (top of stack for a no-arg `<init>`) is a fresh
                    // `Op::New` we emitted. Eliding a `<init>` whose receiver is
                    // `this` or a parameter would skip a real superclass
                    // constructor (and hide any escape it performs) — and that
                    // is exactly what a `super()` chain call looks like, which
                    // the census found 29 of. Such a site now falls through to
                    // the call path below instead of bailing the method.
                    // A site the compile door found unresolved is always a
                    // call (its first-execution guard rides on it; interpreter
                    // round i1 wave 50, lane L1).
                    let first_call = self.first_call_cell_at(pc).is_some();
                    let elide = !first_call
                        && ((self.trivial_init_pcs.contains(&pc)
                            && matches!(
                                self.peek_opt().and_then(|r| self.graph.node_opt(r)),
                                Some(Node {
                                    op: Op::New { .. },
                                    ..
                                })
                            ))
                            // `java/lang/Object.<init>()V` needs no receiver check:
                            // its body is a bare `return` and the native that
                            // shadows it is `native_noop_with_this`, so eliding it
                            // is sound on `this` as much as on a fresh `Op::New`.
                            // That is the ONLY shape exempt from the check above,
                            // and it is the terminal of every constructor chain —
                            // see `object_init_pcs` for what it was costing.
                            || self.object_init_pcs.contains(&pc));
                    if elide {
                        let recv = self.pop();
                        self.uninitialized_news.remove(&recv);
                        pc += 3;
                    }
                    // inc 24 (Gap B) + cov-04: a resolved `invokespecial`
                    // lowered to `Op::Call` — identical to the `invokestatic`
                    // arm except the receiver is arg0 (the caller's
                    // `invoke_info` entry already counts it in `num_args`, and
                    // the leaked `JitInvokeInfo` carries `invoke_kind == 1` so
                    // `invoke_dispatch` does the non-virtual dispatch to the
                    // statically-resolved target — the same route the
                    // single-pass backend gives every non-elided `<init>`).
                    // GC-safe by the same conservative IR-frame scan that roots
                    // reference args (inc 22). Only populated when the
                    // special-call gate is on; otherwise `invoke_info` has no
                    // entry for this pc and the method bails to single-pass.
                    // IR-tier inlining: splice the callee's body in place of the
                    // call. Checked after the elision (eliding costs nothing and
                    // a site that qualifies for it has no body worth splicing)
                    // and before the dispatch lowering. A synchronized body
                    // whose monitor pair cannot be built here keeps the call
                    // (round 14 wave 2, lane syncsplice).
                    else if !first_call
                        && self.inline_sites.contains_key(&pc)
                        && self.sync_splice_site_admits(pc)
                    {
                        match self.begin_splice(pc, 3) {
                            Some(next) => pc = next,
                            None => return ir_build_bail(line!(), pc),
                        }
                    } else if let Some((_, num_args, ret_type)) =
                        self.invoke_info.get(&pc).copied().filter(|_| {
                            !first_call
                                && !self.splice.is_empty()
                                && self.unloaded_static_call_pcs.contains(&pc)
                                && ir_splice_never_run_private_exit_enabled()
                        })
                    {
                        // JIT round 21 wave 1, lane updaters (UPDATERS21-3,
                        // opt-in): a never-run private `invokespecial` inside a
                        // splice the resolver listed is the precise exit the
                        // `0xb6` / `0xb8` arm plants for its rows: an
                        // always-failing guard resumed through the chain
                        // snapshot at this call, no call effect replayed.
                        if !ir_site_trap_enabled()
                            || self.sync_splice_open()
                            || self.stack.len() < num_args
                            || !self.relax_fence_by_chain(pc)
                        {
                            return self.bail_invoke(line!(), pc);
                        }
                        let zero = self.iconst(0);
                        self.graph.add(
                            Op::Guard { bci: pc },
                            IrType::Void,
                            vec![self.ctrl, zero],
                            Some(pc),
                        );
                        self.graph.unloaded_static_call_traps = true;
                        self.note_class_loading_trap(pc);
                        for _ in 0..num_args {
                            self.pop();
                        }
                        let placeholder = match ret_type {
                            b'V' => None,
                            b'L' | b'[' => Some(self.aconst_null()),
                            b'J' => Some(self.lconst(0)),
                            b'F' => Some(self.fconst(0.0)),
                            b'D' => Some(self.dconst(0.0)),
                            _ => Some(self.iconst(0)),
                        };
                        if let Some(value) = placeholder {
                            self.push(value);
                        }
                        pc += 3;
                    } else if !first_call
                        && code[pc] == 0xb7
                        && self.invoke_info.get(&pc).copied().is_some_and(
                            |(info_ptr, num_args, ret_type)| {
                                // JIT round 21 (lane tl3): a private
                                // `refersTo0` in a spliced `refersToImpl`.
                                self.try_reference_refers_to_intrinsic(
                                    pc, info_ptr, num_args, ret_type, code_len,
                                )
                            },
                        )
                    {
                        crate::ir_evidence::note_builder_expansion();
                        pc += 3;
                    } else if let Some(&(info_ptr, num_args, ret_type)) = self.invoke_info.get(&pc)
                    {
                        self.note_first_call(pc, info_ptr);
                        let mut args = Vec::with_capacity(num_args);
                        for _ in 0..num_args {
                            args.push(self.pop());
                        }
                        args.reverse();
                        if let Some(&recv) = args.first() {
                            self.uninitialized_news.remove(&recv);
                        }
                        // Round 11 wave 11 (lane `calls`): a caller-held
                        // INSTANCE synchronized direct CALL -- `super.m()` or a
                        // private `m()` whose target is `synchronized`. Same
                        // shape and contract as the `0xb6` arm's twin; see
                        // `enter_receiver_monitor`.
                        let sync_monitor = if self.sync_direct_calls.contains_key(&pc) {
                            let receiver = args.first().copied().unwrap_or(NO_NODE);
                            // Statically selected: no class guard.
                            match self.enter_receiver_monitor(pc, receiver, 0) {
                                Some(m) => Some(m),
                                None => return ir_build_bail(line!(), pc),
                            }
                        } else {
                            None
                        };
                        let mut inputs = Vec::with_capacity(2 + num_args);
                        inputs.push(self.ctrl);
                        inputs.push(self.mem);
                        inputs.extend(args);
                        let returns_value = ret_type != b'V';
                        let ty = match ret_type {
                            b'V' => IrType::Void,
                            b'L' | b'[' => IrType::Ref,
                            // A `J` (long) return is a 64-bit value node so
                            // downstream category-2 ops (lstore/lreturn/ladd/…)
                            // type-check; `static_call_shape` only admits `J`
                            // returns once the i64::MIN-sentinel collision is
                            // disambiguated at the call site (see `Op::Call`
                            // lowering). `D`/`F` returns stay rejected by the
                            // shape gate until the XMM value tier exists.
                            b'J' => IrType::Long,
                            // A `D` (double) return is a 64-bit value node (inc
                            // 32); admitted by `static_call_shape`, the call-site
                            // sentinel check already disambiguates it.
                            b'D' => IrType::Double,
                            // A `F` (float) return is a 32-bit value node (inc 33).
                            b'F' => IrType::Float,
                            _ => IrType::Int,
                        };
                        let call = self.graph.add(Op::Call { info_ptr }, ty, inputs, Some(pc));
                        self.note_splice_call(call);
                        self.mem = call;
                        // The normal edge's release; the exceptional edge's is
                        // the lowerer's (`emit_sync_exit_then_throw`).
                        if let Some(m) = sync_monitor {
                            let exit = self.graph.add(
                                Op::MonitorExit,
                                IrType::Memory,
                                vec![self.ctrl, self.mem, m],
                                Some(pc),
                            );
                            self.mem = exit;
                            // j27 wave 4 (SY16-3).
                            self.graph.note_sync_call_pair(call, exit);
                        }
                        // nio21 wave 3 L2 / JIT round 19 wave 5 (SPLICE4-1): the call's
                        // in-frame handler edges.
                        if sync_monitor.is_none() {
                            self.plant_call_handler_entry(pc, call, code_len);
                        }
                        if returns_value {
                            self.push(call);
                        }
                        pc += 3;
                    } else {
                        // Neither lowering applies: no `invoke_info` entry for
                        // this pc (the special-call gate is off, or one
                        // non-emittable invoke elsewhere in the method
                        // discarded the whole map) and the site is not an
                        // elidable `<init>` on a fresh `Op::New`. Refuse the
                        // method — never guess a callee identity.
                        return self.bail_invoke(line!(), pc);
                    }
                }
                // invokestatic (0xb8) and invokevirtual (0xb6) — lower a resolved
                // call to `Op::Call` (Gap B). `invokestatic` has no receiver;
                // `invokevirtual` marshals the receiver as arg0 (the caller's
                // `invoke_info` entry already counts it in `num_args`, and the
                // leaked `JitInvokeInfo` carries `invoke_kind == 0` so
                // `invoke_dispatch` resolves the actual target on the receiver's
                // runtime class — full virtual dispatch through the generic helper,
                // no inline cache in the emitted code). GC-safe by the same
                // conservative IR-frame scan that roots reference args (inc 22).
                // Both are 3-byte instructions. A pc not in `invoke_info` (gate
                // off, or a non-emittable invoke present) bails to single-pass.
                0xb6 | 0xb8 => {
                    // JIT round 16 wave 10 (lane indyir, IN16-8): an `invokedynamic`
                    // planned as a plain call reaches this arm too
                    // (`indy_direct_opcode`); it is five bytes, not three.
                    // JIT round 21 (lane lambda2): so is an `invokeinterface`
                    // planned as a lambda impl's `invokestatic`.
                    let ilen = if matches!(code[pc], 0xb9 | 0xba) {
                        5
                    } else {
                        3
                    };
                    // A site the compile door found unresolved is always a
                    // call (interpreter round i1 wave 50, lane L1).
                    let first_call = self.first_call_cell_at(pc).is_some();
                    // Scalarize the fixed erased adapter in Dist's private
                    // numeric kernels before it creates Integer/Double boxes.
                    // The run is matched on its MEMBERS, not only its opcode
                    // bytes (round 11 wave 2): see `tdigest_adapter_run_matches`.
                    // 2026-10-07 (item 2 of
                    // `interpreter-L5-getinitiatedclasses-residual-recording-differences-FIXED-20261007`):
                    // the adapter run's other two calls too, which the fused
                    // node would run with no first-execution guard.
                    let run_first_call = first_call
                        || self.first_call_cell_at(pc + 3).is_some()
                        || self.first_call_cell_at(pc + 11).is_some();
                    if op == 0xb8
                        && !run_first_call
                        && self.tdigest_scalar_kernel
                        && self.tdigest_adapter_run_matches(code, code_len, pc)
                    {
                        if self.stack.len() >= 2 {
                            let index = self.pop();
                            let lambda = self.pop();
                            // A null `Function` deopts HERE, at the
                            // `invokestatic`, whose snapshot still holds
                            // `(lambda, index)`; the interpreter re-runs the
                            // run and raises the NPE at `pc + 3`, the
                            // `invokeinterface`, with that bci's handler,
                            // line and JEP 358 message. The helper's own
                            // null arm (kept as a backstop) can only name
                            // `pc` — the node's one bci — and so described
                            // `Integer.valueOf` as the call that failed.
                            // Round 11 wave 4 (lane ircore).
                            let null = self.aconst_null();
                            let lambda_nonnull = self.add_data(
                                Op::Cmp(CmpOp::Ne),
                                IrType::Int,
                                vec![lambda, null],
                                pc,
                            );
                            self.graph.add(
                                Op::Guard { bci: pc },
                                IrType::Void,
                                vec![self.ctrl, lambda_nonnull],
                                Some(pc),
                            );
                            let inputs = vec![self.ctrl, self.mem, lambda, index];
                            let call = self.graph.add(
                                Op::LambdaIntToDouble,
                                IrType::Double,
                                inputs,
                                Some(pc),
                            );
                            self.mem = call;
                            self.push(call);
                            pc += 14;
                            continue;
                        }
                    }
                    // JIT round 21 (lane g1vhsoft,
                    // `CRATONVM_JIT_IR_REFERENCE_GET_INLINE`): an exactly
                    // `WeakReference` / `SoftReference` `get()` as the JDK
                    // body inline (`try_reference_get_intrinsic`), ahead of
                    // the splice below: the expansion carries the referent's
                    // keep-alive gate and a compact row, and needs no
                    // `super.get()` splice of its own. Its refusal leaves the
                    // stack untouched and the site as before.
                    if op == 0xb6
                        && code[pc] == 0xb6
                        && !first_call
                        && self.reference_get_layout.is_some()
                        && self.invoke_info.get(&pc).copied().is_some_and(
                            |(info_ptr, num_args, ret_type)| {
                                self.try_reference_get_intrinsic(
                                    pc, info_ptr, num_args, ret_type, code_len,
                                )
                            },
                        )
                    {
                        crate::ir_evidence::note_builder_expansion();
                        pc += ilen;
                        continue;
                    }
                    // j27 wave 4 (lane LATIN1,
                    // `CRATONVM_JIT_IR_LATIN1_CHAR_CLASS_EXPAND`): a
                    // `Character.isX` over a Latin-1 code point is the JDK
                    // table answer, the call otherwise (`ir::latin1_char_class`);
                    // ahead of the splice, like `Reference.get`.
                    if op == 0xb8
                        && code[pc] == 0xb8
                        && !first_call
                        && self.invoke_info.get(&pc).copied().is_some_and(
                            |(info_ptr, num_args, ret_type)| {
                                self.try_emit_latin1_char_class(
                                    pc, info_ptr, num_args, ret_type, code_len,
                                )
                            },
                        )
                    {
                        crate::ir_evidence::note_builder_expansion();
                        pc += ilen;
                        continue;
                    }
                    // IR-tier inlining: splice the callee's body in place of the
                    // call. `invokevirtual` reaches here only for a site the
                    // resolver bound to ONE body (a `final`/`private`/
                    // effectively-monomorphic target); a polymorphic site never
                    // gets an entry. An exact-receiver site (round 12 wave 4,
                    // `exact_receiver_sites`) whose receiver is not proven
                    // falls through to the ordinary call below. So does a
                    // synchronized body whose monitor pair cannot be built
                    // here (round 14 wave 2, lane syncsplice).
                    if op == 0xb6 && !first_call && self.try_pre_splice_collection_at(pc) {
                        pc += ilen;
                        continue;
                    }
                    if !first_call
                        && self.inline_sites.contains_key(&pc)
                        && !self.signature_polymorphic_site(pc)
                        && self.exact_receiver_splice_admits(pc)
                        && self.sync_splice_site_admits(pc)
                    {
                        match self.begin_splice(pc, ilen) {
                            Some(next) => {
                                pc = next;
                                continue;
                            }
                            None => return ir_build_bail(line!(), pc),
                        }
                    }
                    // JIT r20 wave 12 (lane qMisc, NIO20-L3-1):
                    // `x.getClass() == C.class` as a null guard and an exact
                    // header class test, with no call and no `ldc`.
                    if op == 0xb6 && ilen == 3 && !first_call {
                        if let Some(next) = self.try_fold_getclass_exact_compare(code, pc) {
                            pc = next;
                            continue;
                        }
                        // JIT r20 wave 19 (lane zLdc): the same pair inside a
                        // splice, on a fresh allocation, answered at build time.
                        if let Some(next) = self.try_fold_spliced_getclass_of_fresh(code, pc) {
                            pc = next;
                            continue;
                        }
                    }
                    // A scalar-intrinsic family: emit the arithmetic and skip
                    // the call entirely. Checked BEFORE `invoke_info`, because
                    // the planner deliberately builds no row for such a site --
                    // see `IrBuilder::scalar_intrinsics`.
                    // A first-call site may still be arithmetic: its guard is
                    // emitted in front of the node (`scalar_first_call_cells`).
                    // j27 wave 2 (lane INTRINS): a pure expansion likewise.
                    if (!first_call || scalar_intrinsic_first_call_enabled())
                        && (self.try_emit_scalar_intrinsic(pc) || self.try_emit_pure_expansion(pc))
                    {
                        pc += ilen;
                        continue;
                    }
                    // An unboxing accessor: emit the guarded field load inline
                    // and skip the call. Same contract as the scalar families
                    // above -- the planner builds no `invoke_info` row for a
                    // site it registered here.
                    if !first_call && self.try_emit_unbox_intrinsic(pc) {
                        pc += ilen;
                        continue;
                    }
                    // JIT r23 wave 4 (lane L8): a `MemorySegment` element site
                    // the planner asked to expand inline. A decline keeps the
                    // call row below, or refuses the method when no other
                    // route serves the site in this compile.
                    if !first_call {
                        // JIT r23 wave 11 (lane L4): a never-reached element site.
                        match self.try_emit_ffm_unreached_trap(pc) {
                            Some(true) => {
                                pc += ilen;
                                continue;
                            }
                            Some(false) => return self.bail_invoke(line!(), pc),
                            None => {}
                        }
                        match self.try_emit_ffm_element_inline(pc) {
                            Some(true) => {
                                pc += ilen;
                                continue;
                            }
                            Some(false)
                                if self
                                    .ffm_inline_sites
                                    .get(&pc)
                                    .is_some_and(|s| s.bail_on_decline) =>
                            {
                                return self.bail_invoke(line!(), pc);
                            }
                            _ => {}
                        }
                    }
                    let (mut info_ptr, mut num_args, mut ret_type) = match self.invoke_info.get(&pc)
                    {
                        Some(&t) => t,
                        None => return self.bail_invoke(line!(), pc),
                    };
                    if op != 0xb8 {
                        self.note_frozen_receiver_call(pc, num_args);
                    }
                    let mut mh_folded = false;
                    if op == 0xb6 && info_ptr != 0 && num_args > 0 && self.stack.len() >= num_args {
                        let recv = self.stack[self.stack.len() - num_args];
                        let constant = self.const_ref_slot_of(recv);
                        let guessed = constant
                            .is_none()
                            .then(|| self.static_guess_slot_of(recv, 0))
                            .flatten();
                        if let Some(mh_slot) = constant.or(guessed) {
                            // SAFETY: the planner owns the nonzero invoke row throughout this build.
                            let info: &JitInvokeInfo =
                                unsafe { &*(info_ptr as *const JitInvokeInfo) };
                            if info.class_name == "java/lang/invoke/MethodHandle"
                                && (info.method_name == "invokeExact"
                                    || info.method_name == "invoke")
                            {
                                let fold_opt = self
                                    .const_folds
                                    .const_mh_invoke_infos
                                    .get(&(mh_slot, info.descriptor.to_string()))
                                    .cloned()
                                    .filter(|f| {
                                        f.info_ptr != 0
                                            && (guessed.is_none()
                                                || self.plant_static_guess_guard(pc, recv, mh_slot))
                                    });
                                if let Some(fold) = fold_opt {
                                    if fold.info_ptr != 0 {
                                        let can_guard = if self.splice.is_empty() {
                                            self.has_frame_state_at(pc)
                                        } else {
                                            ir_site_trap_enabled()
                                                && !self.sync_splice_open()
                                                && self.relax_fence_by_chain(pc)
                                        };
                                        // SAFETY: the nonzero folded row is owned by the compile's invoke arena.
                                        let folded_info: &JitInvokeInfo =
                                            unsafe { &*(fold.info_ptr as *const JitInvokeInfo) };
                                        let at = self.stack.len() - num_args;
                                        if let Some(row) = fold.convert {
                                            let arity = usize::from(row.arity);
                                            let box_ok =
                                                row.box_return == 0 || fold.box_info_ptr != 0;
                                            let mut args_ok = arity <= 8
                                                && num_args == 1 + arity
                                                && box_ok
                                                && can_guard
                                                && self.ctrl_opt().is_some();
                                            if args_ok {
                                                for i in 0..arity {
                                                    if node_id_opt(self.stack[at + 1 + i]).is_none()
                                                        || (row.cast_class_ids[i] != 0
                                                            && row.cast_class_names[i].is_empty())
                                                        || !matches!(row.unbox_op[i], 0..=2)
                                                        || (row.unbox_op[i] != 0
                                                            && row.unbox_class_ids[i] == 0)
                                                    {
                                                        args_ok = false;
                                                        break;
                                                    }
                                                }
                                            }
                                            // An interface member's receiver is guarded
                                            // below (`plant_member_class_guard`).
                                            let iface_ok = row.main_kind != 2
                                                || (arity > 0
                                                    && folded_info.declaring_class_id != 0
                                                    && !folded_info.class_name.is_empty());
                                            if args_ok
                                                && iface_ok
                                                && matches!(row.main_kind, 0 | 2 | 3)
                                            {
                                                for i in 0..arity {
                                                    let Some(arg) =
                                                        node_id_opt(self.stack[at + 1 + i])
                                                    else {
                                                        break;
                                                    };
                                                    if row.cast_class_ids[i] != 0 {
                                                        let target_id = row.cast_class_ids[i];
                                                        let target_name = &row.cast_class_names[i];
                                                        if let Some(ctrl) = self.ctrl_opt() {
                                                            let (name_ptr, name_len) =
                                                                crate::intern_typecheck_target(
                                                                    target_name,
                                                                    Some(target_id),
                                                                );
                                                            let is_inst = self.graph.add(
                                                                Op::InstanceOf {
                                                                    name_ptr: name_ptr as usize,
                                                                    name_len,
                                                                    seen_cell: 0,
                                                                    header_only: false,
                                                                },
                                                                IrType::Int,
                                                                vec![ctrl, self.mem, arg],
                                                                Some(pc),
                                                            );
                                                            self.mem = is_inst;
                                                            let null = self.aconst_null();
                                                            let is_null = self.add_data(
                                                                Op::Cmp(CmpOp::Eq),
                                                                IrType::Int,
                                                                vec![arg, null],
                                                                pc,
                                                            );
                                                            let ok = self.add_data(
                                                                Op::Or,
                                                                IrType::Int,
                                                                vec![is_null, is_inst],
                                                                pc,
                                                            );
                                                            if let Some(ctrl) = self.ctrl_opt() {
                                                                self.graph.add(
                                                                    Op::Guard { bci: pc },
                                                                    IrType::Void,
                                                                    vec![ctrl, ok],
                                                                    Some(pc),
                                                                );
                                                            }
                                                        }
                                                    }
                                                    if row.unbox_op[i] != 0 {
                                                        let (uop, ty) = match row.unbox_op[i] {
                                                            1 => (UnboxOp::IntValue, IrType::Int),
                                                            2 => (UnboxOp::LongValue, IrType::Long),
                                                            _ => unreachable!(),
                                                        };
                                                        if let Some(ctrl) = self.ctrl_opt() {
                                                            let unboxed = self.graph.add(
                                                                Op::Unbox {
                                                                    op: uop,
                                                                    class_id: row.unbox_class_ids
                                                                        [i],
                                                                },
                                                                ty,
                                                                vec![ctrl, self.mem, arg],
                                                                Some(pc),
                                                            );
                                                            self.mem = unboxed;
                                                            self.stack[at + 1 + i] = unboxed;
                                                        }
                                                    }
                                                }
                                                if row.main_kind == 2 {
                                                    if let Some(target_recv) =
                                                        node_id_opt(self.stack[at + 1])
                                                    {
                                                        self.plant_member_class_guard(
                                                            pc,
                                                            target_recv,
                                                            folded_info,
                                                            0,
                                                        );
                                                    }
                                                }
                                                if row.main_kind == 0 && arity > 0 {
                                                    if let Some(target_recv) =
                                                        node_id_opt(self.stack[at + 1])
                                                    {
                                                        let proven = self.graph.nodes.get(target_recv as usize).is_some_and(|n| {
                                                            crate::ir_check_elim::definitely_non_null(&n.op)
                                                        });
                                                        if !proven {
                                                            if let Some(ctrl) = self.ctrl_opt() {
                                                                let null = self.aconst_null();
                                                                let cond = self.add_data(
                                                                    Op::Cmp(CmpOp::Ne),
                                                                    IrType::Int,
                                                                    vec![target_recv, null],
                                                                    pc,
                                                                );
                                                                self.graph.add(
                                                                    Op::Guard { bci: pc },
                                                                    IrType::Void,
                                                                    vec![ctrl, cond],
                                                                    Some(pc),
                                                                );
                                                            }
                                                        }
                                                    }
                                                }
                                                self.stack.remove(at);
                                                self.graph.mh_constant_call_pcs.push(pc);
                                                // The member's body, planned at this call
                                                // (`folded_handle_member_row`), spliced when it
                                                // cannot raise; its result is boxed after it.
                                                if row.box_return != 0
                                                    && fold.box_info_ptr != 0
                                                    && self.folded_member_splice_admitted(code, pc, fold.info_ptr).is_some()
                                                {
                                                    self.convert_box_after.insert(pc, (fold.box_info_ptr, pc));
                                                    match self.begin_splice(pc, ilen) {
                                                        Some(next) => {
                                                            pc = next;
                                                            continue;
                                                        }
                                                        None => return ir_build_bail(line!(), pc),
                                                    }
                                                }
                                                if row.box_return != 0 {
                                                    let mut call_args = Vec::with_capacity(arity);
                                                    for _ in 0..arity {
                                                        call_args.push(self.pop());
                                                    }
                                                    call_args.reverse();
                                                    let member_ret = if row.box_return == 2 {
                                                        b'J'
                                                    } else {
                                                        b'I'
                                                    };
                                                    let call_val = self
                                                        .mh_constant_filter_pre_call(
                                                            pc,
                                                            fold.info_ptr,
                                                            member_ret,
                                                            &call_args,
                                                        );
                                                    self.plant_call_throw_edges(
                                                        pc,
                                                        call_val,
                                                        code.len(),
                                                    );
                                                    let boxed = self.mh_constant_filter_pre_call(
                                                        pc,
                                                        fold.box_info_ptr,
                                                        b'L',
                                                        &[call_val],
                                                    );
                                                    self.plant_call_throw_edges(
                                                        pc,
                                                        boxed,
                                                        code.len(),
                                                    );
                                                    self.push(boxed);
                                                    pc += ilen;
                                                    continue;
                                                } else {
                                                    info_ptr = fold.info_ptr;
                                                    num_args = arity;
                                                    ret_type = fold.ret_type;
                                                    mh_folded = true;
                                                }
                                            }
                                        } else if (matches!(folded_info.invoke_kind, 0 | 3)
                                            || (folded_info.invoke_kind == 2
                                                && num_args > 1
                                                && folded_info.declaring_class_id != 0
                                                && !folded_info.class_name.is_empty()))
                                            && can_guard
                                        {
                                            let arity = num_args - 1;
                                            if folded_info.invoke_kind == 2 {
                                                if let Some(target_recv) =
                                                    node_id_opt(self.stack[at + 1])
                                                {
                                                    self.plant_member_class_guard(
                                                        pc,
                                                        target_recv,
                                                        folded_info,
                                                        0,
                                                    );
                                                }
                                            }
                                            if folded_info.invoke_kind == 0 && arity > 0 {
                                                if let Some(target_recv) =
                                                    node_id_opt(self.stack[at + 1])
                                                {
                                                    let proven = self.graph.nodes.get(target_recv as usize).is_some_and(|n| {
                                                        crate::ir_check_elim::definitely_non_null(&n.op)
                                                    });
                                                    if !proven {
                                                        if let Some(ctrl) = self.ctrl_opt() {
                                                            let null = self.aconst_null();
                                                            let cond = self.add_data(
                                                                Op::Cmp(CmpOp::Ne),
                                                                IrType::Int,
                                                                vec![target_recv, null],
                                                                pc,
                                                            );
                                                            self.graph.add(
                                                                Op::Guard { bci: pc },
                                                                IrType::Void,
                                                                vec![ctrl, cond],
                                                                Some(pc),
                                                            );
                                                        }
                                                    }
                                                }
                                            }
                                            self.stack.remove(at);
                                            self.graph.mh_constant_call_pcs.push(pc);
                                            info_ptr = fold.info_ptr;
                                            num_args = arity;
                                            ret_type = fold.ret_type;
                                            mh_folded = true;
                                        }
                                    }
                                }
                            }
                        }
                    }
                    // `CRATONVM_JIT_IR_SPLICE_UNRECORDED_CALL_EXIT`: a spliced
                    // virtual / interface call the resolver listed (its owner
                    // is not recorded yet) takes the same exit.
                    if (op == 0xb8
                        || (matches!(op, 0xb6 | 0xb9)
                            && !self.splice.is_empty()
                            && ir_splice_unrecorded_call_exit_enabled()))
                        && !mh_folded
                        && self.unloaded_static_call_pcs.contains(&pc)
                    {
                        if !ir_site_trap_enabled()
                            || self.sync_splice_open()
                            || self.stack.len() < num_args
                            || !self.relax_fence_by_chain(pc)
                        {
                            return self.bail_invoke(line!(), pc);
                        }
                        // The top-of-invoke snapshot retains every operand and
                        // scope. Resume the original bytecode at this call;
                        // resolution, class initialization and exceptions all
                        // belong to the interpreter. No call effect is replayed.
                        let zero = self.iconst(0);
                        self.graph.add(
                            Op::Guard { bci: pc },
                            IrType::Void,
                            vec![self.ctrl, zero],
                            Some(pc),
                        );
                        self.graph.unloaded_static_call_traps = true;
                        self.note_class_loading_trap(pc);
                        if ir_bail_reporting() {
                            eprintln!("[ir] unloaded-static-call trap at combined pc {pc} in {} with precise scope chain",
                                self.method_label.as_deref().unwrap_or("<unknown>"));
                        }
                        for _ in 0..num_args {
                            self.pop();
                        }
                        let placeholder = match ret_type {
                            b'V' => None,
                            b'L' | b'[' => Some(self.aconst_null()),
                            b'J' => Some(self.lconst(0)),
                            b'F' => Some(self.fconst(0.0)),
                            b'D' => Some(self.dconst(0.0)),
                            _ => Some(self.iconst(0)),
                        };
                        if let Some(value) = placeholder {
                            self.push(value);
                        }
                        pc += ilen;
                        continue;
                    }
                    // Round 14 wave 4 (lane sync4, SS-7): `Thread.holdsLock`
                    // of a monitor this frame provably holds is `true`.
                    if op == 0xb8 && !first_call && self.try_fold_holds_lock(info_ptr, num_args) {
                        pc += ilen;
                        continue;
                    }
                    if op == 0xb8
                        && !first_call
                        && num_args == 0
                        && ret_type == b'L'
                        && !self.sync_direct_calls.contains_key(&pc)
                        && self.try_fold_reflection_get_caller_class(info_ptr)
                    {
                        pc += ilen;
                        continue;
                    }
                    if op == 0xb8
                        && !first_call
                        && num_args == 1
                        && ret_type == b'I'
                        && !self.sync_direct_calls.contains_key(&pc)
                        && self.try_fold_reflection_get_class_access_flags(info_ptr)
                    {
                        pc += ilen;
                        continue;
                    }
                    if op == 0xb6
                        && !first_call
                        && num_args == 2
                        && !self.sync_direct_calls.contains_key(&pc)
                        && self.try_fold_class_is_assignable_from_get_class(pc, info_ptr)
                    {
                        pc += ilen;
                        continue;
                    }
                    // nio20 wave 3, lane L2 (NIO19-9): `Reference.reachabilityFence(ref)`
                    // as an `Op::KeepAlive` -- a use of `ref` at this program point
                    // and nothing else (see the op). The call it replaces was a
                    // memory barrier and a safepoint in every loop that fenced.
                    if op == 0xb8
                        && !first_call
                        && num_args == 1
                        && ret_type == b'V'
                        && is_reachability_fence_info_row(info_ptr)
                        && !self.sync_direct_calls.contains_key(&pc)
                        && ir_reachability_fence_keepalive_enabled()
                    {
                        let referent_is_ref = self
                            .peek_opt()
                            .and_then(|r| self.graph.nodes.get(r as usize))
                            .is_some_and(|n| n.ty == IrType::Ref);
                        if referent_is_ref {
                            let referent = self.pop();
                            self.graph.add(
                                Op::KeepAlive,
                                IrType::Void,
                                vec![self.ctrl, referent],
                                Some(pc),
                            );
                            pc += ilen;
                            continue;
                        }
                    }
                    // String access intrinsics in this tier — see the section
                    // above `IrBuilder`. Offered only to `invokevirtual`:
                    // `invokestatic` has no receiver, so no String accessor
                    // can reach this arm through it. A refusal leaves the
                    // abstract stack untouched and falls through to the
                    // `Op::Call` below, which is what the site does today.
                    if op == 0xb6
                        && !first_call
                        && self.try_string_access_intrinsic(pc, info_ptr, num_args)
                    {
                        pc += ilen;
                        continue;
                    }
                    // JIT round 23 wave 9 (lane W9-L2, CF-W9-3): a
                    // constant-`VarHandle` get / set on a fresh, unpublished
                    // receiver of the plan's exact class is the field access
                    // itself, with no call (`try_vh_const_fresh_field_access`).
                    if op == 0xb6
                        && code[pc] == 0xb6
                        && !first_call
                        && self.try_vh_const_fresh_field_access(pc, info_ptr, num_args)
                    {
                        pc += ilen;
                        continue;
                    }
                    // nio21 wave 19 (lane L3): `ThreadLocal.get()`'s hit path
                    // inline, the call on every miss
                    // (`try_thread_local_get_intrinsic`).
                    if op == 0xb6
                        && code[pc] == 0xb6
                        && !first_call
                        && self.try_thread_local_get_intrinsic(
                            pc, info_ptr, num_args, ret_type, code_len,
                        )
                    {
                        // JIT round 21 (lane accept): acceptance evidence
                        // (`ir_evidence::note_builder_expansion`).
                        crate::ir_evidence::note_builder_expansion();
                        pc += ilen;
                        continue;
                    }
                    if op == 0xb8 && code[pc] == 0xb8 && !first_call {
                        if let Some(next_pc) = self.try_nio_charset_decode_to_string_return(
                            code, pc, info_ptr, num_args, ret_type, code_len,
                        ) {
                            crate::ir_evidence::note_builder_expansion();
                            pc = next_pc;
                            continue;
                        }
                    }
                    if (op == 0xb6 || op == 0xb8)
                        && code[pc] == op
                        && !first_call
                        && self.try_nio_charset_ir_intrinsic(
                            pc, op, info_ptr, num_args, ret_type, code_len,
                        )
                    {
                        crate::ir_evidence::note_builder_expansion();
                        pc += ilen;
                        continue;
                    }
                    // JIT round 21 (lane tl3): `Reference.refersTo` and its
                    // spliced `refersToImpl` / `refersTo0` as an identity
                    // compare of the referent (`try_reference_refers_to_intrinsic`;
                    // the `invokespecial` twin is in the `0xb7` arm).
                    if op == 0xb6
                        && code[pc] == 0xb6
                        && !first_call
                        && self.try_reference_refers_to_intrinsic(
                            pc, info_ptr, num_args, ret_type, code_len,
                        )
                    {
                        crate::ir_evidence::note_builder_expansion();
                        pc += ilen;
                        continue;
                    }
                    self.note_first_call(pc, info_ptr);
                    // Round nio20 wave 3, lane L4 (L3W2-3): a NESTED
                    // nest-exact `invokevirtual` whose receiver is not proven
                    // splices behind a trap guard, like the interface rows of
                    // the `0xb9` arm; `None` falls through.
                    if op == 0xb6 && code[pc] == 0xb6 && !first_call && !mh_folded {
                        if let Some((receiver, class_id)) =
                            self.nested_trap_guard_plan(pc, 0xb6, num_args, ret_type)
                        {
                            match self.begin_trap_guarded_receiver_splice(pc, 3, receiver, class_id)
                            {
                                Some(next) => {
                                    pc = next;
                                    continue;
                                }
                                None => return ir_build_bail(line!(), pc),
                            }
                        }
                    }
                    // Round 12 wave 7 (lane iropt6, W6-1): an exact-receiver
                    // site whose receiver is not proven exact splices behind a
                    // class test, with this call as the miss edge. A `None`
                    // plan builds the call below exactly as before.
                    if op == 0xb6 && !first_call && !mh_folded {
                        if let Some((receiver, class_id)) =
                            self.guarded_receiver_splice_plan(pc, num_args, ret_type)
                        {
                            match self.begin_guarded_receiver_splice(
                                pc, ilen, info_ptr, num_args, ret_type, receiver, class_id,
                            ) {
                                Some(next) => {
                                    pc = next;
                                    continue;
                                }
                                None => return ir_build_bail(line!(), pc),
                            }
                        }
                    }
                    // Pop args (deepest-first on the abstract stack) and restore
                    // source order so inputs are [ctrl, mem, arg0, arg1, …].
                    let mut args = Vec::with_capacity(num_args);
                    for _ in 0..num_args {
                        args.push(self.pop());
                    }
                    args.reverse();
                    // Round 11 wave 10 (lane `synccall`, page
                    // `r11w9-sync-caller-held-monitor-direct-call-patch`): a
                    // `static synchronized` callee CALLed directly, with THIS
                    // method holding the class monitor around the call:
                    //
                    //   m = ConstClass(Methodref.class_index); MonitorEnter m;
                    //   Call; MonitorExit m
                    //
                    // and `ir_lower` releases `m` on the call's exceptional
                    // edge. `m` is deliberately NOT pushed on `self.monitors`:
                    // no frame state may name it, because none can be taken
                    // while it is held (the callee body can neither trap nor
                    // call, and `ir_lower` refuses the compile if anything
                    // that can deopt is scheduled between the enter and the
                    // exit), and a frame state that named it would make the
                    // interpreter release a monitor this code then releases
                    // again. The three new nodes carry the invoke's pc, so a
                    // deopt at the `ConstClass` or a throw from the enter
                    // re-executes or raises at the whole invoke with the
                    // monitor not yet held. Never inside a splice: a spliced
                    // body's pcs are not the caller's, and the resolver
                    // splices no synchronized callee anyway.
                    // Copied out, so no arm below holds a borrow of `self`.
                    let sync_row = self.sync_direct_calls.get(&pc).copied();
                    let sync_monitor = match sync_row {
                        Some((holder_class_id, cp_idx, slot_addr))
                            if op == 0xb8 && self.splice.is_empty() =>
                        {
                            let m = self.graph.add(
                                Op::ConstClass {
                                    holder_class_id,
                                    cp_idx,
                                    slot_addr,
                                },
                                IrType::Ref,
                                vec![self.ctrl, self.mem],
                                Some(pc),
                            );
                            self.mem = m;
                            let enter = self.graph.add(
                                Op::MonitorEnter,
                                IrType::Memory,
                                vec![self.ctrl, self.mem, m],
                                Some(pc),
                            );
                            self.mem = enter;
                            Some(m)
                        }
                        // Round 11 wave 11 (lane `calls`): the instance twin, an
                        // `invokevirtual` pinned to a private/final `synchronized`
                        // target. The monitor is the receiver, argument 0. See
                        // `enter_receiver_monitor`; a shape it cannot build
                        // refuses the method, because the lowerer serves the
                        // site's tagged row only with the pair in place.
                        //
                        // Round 11 wave 13 (lane `cha`): the row's first field
                        // is the exact receiver class of a CHA-bound genuinely
                        // virtual site (`0` for a pinned one), guarded before
                        // the enter.
                        Some((exact_class_id, _, _)) if op == 0xb6 && self.splice.is_empty() => {
                            let receiver = args.first().copied().unwrap_or(NO_NODE);
                            match self.enter_receiver_monitor(pc, receiver, exact_class_id) {
                                Some(m) => Some(m),
                                None => return ir_build_bail(line!(), pc),
                            }
                        }
                        _ => None,
                    };
                    let mut inputs = Vec::with_capacity(2 + num_args);
                    inputs.push(self.ctrl);
                    inputs.push(self.mem);
                    inputs.extend(args);
                    // A call is a hard memory barrier: it consumes the current
                    // memory token and BECOMES the new one (serialising every
                    // prior memory op before it and every later one after). The
                    // same node also carries the return value (like `Op::Load`).
                    // Result type from the descriptor: `V` → no value; `L`/`[` →
                    // a reference result (`IrType::Ref`); else int-category.
                    let returns_value = ret_type != b'V';
                    let ty = match ret_type {
                        b'V' => IrType::Void,
                        b'L' | b'[' => IrType::Ref,
                        // A `J` (long) return is a 64-bit value node so
                        // downstream category-2 ops type-check; `static_call_shape`
                        // only admits `J` returns once the i64::MIN-sentinel
                        // collision is disambiguated at the call site.
                        b'J' => IrType::Long,
                        // A `D` (double) return is a 64-bit value node (inc 32).
                        b'D' => IrType::Double,
                        // A `F` (float) return is a 32-bit value node (inc 33).
                        b'F' => IrType::Float,
                        _ => IrType::Int,
                    };
                    let call = self.graph.add(Op::Call { info_ptr }, ty, inputs, Some(pc));
                    self.note_splice_call(call);
                    self.mem = call;
                    // The normal edge's release (see `sync_monitor` above).
                    if let Some(m) = sync_monitor {
                        let exit = self.graph.add(
                            Op::MonitorExit,
                            IrType::Memory,
                            vec![self.ctrl, self.mem, m],
                            Some(pc),
                        );
                        self.mem = exit;
                        // j27 wave 4 (SY16-3).
                        self.graph.note_sync_call_pair(call, exit);
                    }
                    // nio21 wave 3 L2 / JIT round 19 wave 5 (SPLICE4-1): the call's
                    // in-frame handler edges.
                    if sync_monitor.is_none() {
                        self.plant_call_handler_entry(pc, call, code_len);
                    }
                    if returns_value {
                        self.push(call);
                    }
                    pc += ilen;
                }
                // invokeinterface (0xb9) — identical to the invokevirtual path
                // (receiver arg0, `invoke_kind == 2`, dynamic dispatch via the
                // helper) except it is a FIVE-byte instruction (opcode, cp_hi,
                // cp_lo, count, 0). The trailing count/0 bytes are not consumed by
                // the builder (the descriptor was resolved from the cp index by
                // the caller); only the pc advance differs.
                // invokedynamic — replaced by an uncommon trap, not compiled.
                //
                // This tier has no lowering for a bootstrap-method call site
                // and is not going to grow one soon. Before this arm existed,
                // `ir_compatible` refused any method containing one: **53
                // methods on the H2 JDBC workload**, most of them lambda or
                // string-concatenation sites on paths that run once.
                //
                // The single-pass backend made exactly this trade first -- an
                // indy site there lowers to an uncommon-trap deopt stub and the
                // rest of the method still compiles -- so this is matching a
                // decision already taken and soaked, not making a new one.
                //
                // The stack effect must still be exact. A pc with no recorded
                // shape means the descriptor resolver declined, and there the
                // method IS refused: guessing how many entries an indy consumes
                // would silently corrupt every bytecode after it.
                0xba => {
                    let Some(&(arg_entries, ret_tag)) = self.indy_trap_sites.get(&pc) else {
                        return ir_build_bail(line!(), pc);
                    };
                    // JIT round 21 wave 17 (lane records, IN16-6): a record's
                    // generated `hashCode` / `equals` site, as the field-wise
                    // fold `ObjectMethods` computes (acceptance evidence: it
                    // replaces the bridge call).
                    if self.try_indy_record_fold(pc, arg_entries, ret_tag) {
                        crate::ir_evidence::note_builder_expansion();
                        pc += 5;
                        continue;
                    }
                    // JIT round 20 wave 11 (lane pIndy, SPLICE19-1): a mutable
                    // call site's planned target, as a guarded plain call with
                    // the bridge as its miss edge.
                    // JIT round 21 (lane accept): each of the three expansions
                    // below replaces the bridge call the single-pass body
                    // keeps, and is acceptance evidence
                    // (`ir_evidence::note_builder_expansion`).
                    // JIT round 21 lane mcs (`CRATONVM_JIT_INDY_MUTABLE_GUARD_SPLICE`):
                    // a static target's body spliced on the guard's hit edge.
                    match self.try_indy_mutable_guarded_splice(pc, arg_entries, ret_tag) {
                        Some(Some(next)) => {
                            crate::ir_evidence::note_builder_expansion();
                            pc = next;
                            continue;
                        }
                        Some(None) => return ir_build_bail(line!(), pc),
                        None => {}
                    }
                    if self.try_indy_mutable_guarded_call(pc, arg_entries, ret_tag) {
                        crate::ir_evidence::note_builder_expansion();
                        pc += 5;
                        continue;
                    }
                    // JIT round 20 wave 11 (lane pIndy, W10L3-3 / W11L3-3): a
                    // published lambda singleton, read, with the bridge cold.
                    if self.try_indy_lambda_word(pc, arg_entries, ret_tag) {
                        crate::ir_evidence::note_builder_expansion();
                        pc += 5;
                        continue;
                    }
                    // JIT round 20 wave 12 (lane qLamNew, W10L3-2 / W11L3-2):
                    // a linked capturing lambda site, as its proxy's `new`
                    // plus one store per capture.
                    if self.try_indy_capture_new(pc, arg_entries, ret_tag) {
                        crate::ir_evidence::note_builder_expansion();
                        pc += 5;
                        continue;
                    }
                    // Round 16 (lane indy, SY4-1): a BRIDGED site is a call,
                    // not a trap -- what the single-pass `0xba` arm has done
                    // for rounds. See `indy_bridge_site_at`.
                    // JIT round 22 (lane throws, churn-loop J1): under the
                    // refusal-only plan the bridge is taken only where the
                    // default arm would not plant its trap (it would bail).
                    if let Some(site) = self.indy_bridge_site_at(pc, ret_tag) {
                        // t1 wave 8, lane LAMBDATRAP: a published-word lambda
                        // site keeps its bridge under the refusal-only plan
                        // too (its trap would cost the method its optimizing
                        // body, the loop included).
                        let published_word_kept = self.indy_bridge_refusal_only
                            && self.splice.is_empty()
                            && self.indy_published_word_pcs.contains(&pc)
                            && ir_indy_published_word_bridge_over_refusal_only_enabled();
                        if !(self.indy_bridge_refusal_only
                            && self.splice.is_empty()
                            && !published_word_kept
                            && self.indy_default_trap_would_plant(code, code_len, pc))
                        {
                            if self.indy_bridge_refusal_only
                                && self.splice.is_empty()
                                && !published_word_kept
                            {
                                if ir_bail_reporting() {
                                    eprintln!(
                                        "[ir] indy at pc {pc} in {} BRIDGED on trap refusal \
                                         (CRATONVM_JIT_IR_INDY_BRIDGE_ON_TRAP_REFUSAL)",
                                        self.method_label.as_deref().unwrap_or("<unknown>"),
                                    );
                                }
                            }
                            self.push_indy_bridge_call(pc, site, arg_entries, ret_tag);
                            pc += 5;
                            continue;
                        }
                    }
                    // nio21 wave 11, lane L3: a spliced `invokedynamic` is
                    // never a trap (it would deopt to re-run the invoke on
                    // every call); one this build cannot bridge refuses it.
                    if !self.splice.is_empty() {
                        return ir_build_bail(line!(), pc);
                    }
                    if !self.plant_uncommon_trap(code, code_len, pc, TrapCause::Indy) {
                        return ir_build_bail(line!(), pc);
                    }
                    for _ in 0..arg_entries {
                        self.pop();
                    }
                    if ret_tag != b'V' {
                        let placeholder = match ret_tag {
                            b'J' => self.lconst(0),
                            b'F' => self.fconst(0.0),
                            b'D' => self.dconst(0.0),
                            b'L' | b'[' => self.aconst_null(),
                            _ => self.iconst(0),
                        };
                        self.push(placeholder);
                    }
                    // invokedynamic is five bytes: opcode, two CP index bytes,
                    // and two zero bytes JVMS §invokedynamic reserves.
                    pc += 5;
                }
                0xb9 => {
                    // IR-tier inlining — five bytes, not three. Round 13 wave
                    // 12 (lane guardsplice, W7-1): an exact-receiver site (a
                    // profile-selected class, the only kind the planner
                    // registers at an `invokeinterface`) splices unguarded only
                    // when its receiver is proven, as at `invokevirtual`, and
                    // otherwise takes the guarded plan below or the call.
                    // A site the compile door found unresolved is always a
                    // call (interpreter round i1 wave 50, lane L1).
                    let first_call = self.first_call_cell_at(pc).is_some();
                    // JIT r23 wave 6 (lane L8): a `MemorySegment` element site
                    // the planner asked to expand inline. `MemorySegment` is an
                    // interface, so every such site is an `invokeinterface`; the
                    // `0xb6` arm's contract, with the five-byte advance.
                    if !first_call {
                        // JIT r23 wave 11 (lane L4): a never-reached element site.
                        match self.try_emit_ffm_unreached_trap(pc) {
                            Some(true) => {
                                pc += 5;
                                continue;
                            }
                            Some(false) => return self.bail_invoke(line!(), pc),
                            None => {}
                        }
                        match self.try_emit_ffm_element_inline(pc) {
                            Some(true) => {
                                pc += 5;
                                continue;
                            }
                            Some(false)
                                if self
                                    .ffm_inline_sites
                                    .get(&pc)
                                    .is_some_and(|s| s.bail_on_decline) =>
                            {
                                return self.bail_invoke(line!(), pc);
                            }
                            _ => {}
                        }
                        if self.try_pre_splice_collection_at(pc) {
                            pc += 5;
                            continue;
                        }
                        if self.inline_sites.contains_key(&pc)
                            && self.exact_receiver_splice_admits(pc)
                            && self.sync_splice_site_admits(pc)
                        {
                            match self.begin_splice(pc, 5) {
                                Some(next) => {
                                    pc = next;
                                    continue;
                                }
                                None => return ir_build_bail(line!(), pc),
                            }
                        }
                    }
                    let (info_ptr, num_args, ret_type) = match self.invoke_info.get(&pc) {
                        Some(&t) => t,
                        None => return self.bail_invoke(line!(), pc),
                    };
                    self.note_frozen_receiver_call(pc, num_args);
                    // A `java/lang/CharSequence.charAt`/`length`/`isEmpty`
                    // site arrives here rather than at 0xb6. The expander
                    // refuses every guarded site today, so this call exists
                    // to COUNT that population (`guarded_site_refused`)
                    // rather than to emit anything — a number this lane will
                    // need before anyone decides whether the header class-id
                    // compare is worth building. Five bytes, not three.
                    // (nio20 wave 3, lane L2: built -- not here but in the
                    // lowering of the `Op::Call` this site becomes,
                    // `ir_lower::Lowerer::try_emit_ir_string_search_prefix`,
                    // which makes the compare in machine code and declines to
                    // the interface call on a miss.)
                    if !first_call && self.try_string_access_intrinsic(pc, info_ptr, num_args) {
                        pc += 5;
                        continue;
                    }
                    self.note_first_call(pc, info_ptr);
                    // Round nio20 wave 2, lane L3: a NESTED one-class
                    // interface site (`StringCharBuffer.get`'s `charAt`)
                    // splices behind a trap guard; `None` falls through.
                    if !first_call {
                        if let Some((receiver, class_id)) =
                            self.nested_trap_guard_plan(pc, 0xb9, num_args, ret_type)
                        {
                            match self.begin_trap_guarded_receiver_splice(pc, 5, receiver, class_id)
                            {
                                Some(next) => {
                                    pc = next;
                                    continue;
                                }
                                None => return ir_build_bail(line!(), pc),
                            }
                        }
                    }
                    // Round 13 wave 12 (lane guardsplice, W7-1): the guarded
                    // splice of the `invokevirtual` arm, five bytes long. The
                    // miss edge is this interface call, with its own row and
                    // inline cache. A `None` plan builds the call below.
                    if let Some((receiver, class_id)) = (!first_call)
                        .then(|| self.guarded_receiver_splice_plan(pc, num_args, ret_type))
                        .flatten()
                    {
                        match self.begin_guarded_receiver_splice(
                            pc, 5, info_ptr, num_args, ret_type, receiver, class_id,
                        ) {
                            Some(next) => {
                                pc = next;
                                continue;
                            }
                            None => return ir_build_bail(line!(), pc),
                        }
                    }
                    let mut args = Vec::with_capacity(num_args);
                    for _ in 0..num_args {
                        args.push(self.pop());
                    }
                    args.reverse();
                    let mut inputs = Vec::with_capacity(2 + num_args);
                    inputs.push(self.ctrl);
                    inputs.push(self.mem);
                    inputs.extend(args);
                    let returns_value = ret_type != b'V';
                    let ty = match ret_type {
                        b'V' => IrType::Void,
                        b'L' | b'[' => IrType::Ref,
                        // A `J` (long) return is a 64-bit value node so
                        // downstream category-2 ops type-check; `static_call_shape`
                        // only admits `J` returns once the i64::MIN-sentinel
                        // collision is disambiguated at the call site.
                        b'J' => IrType::Long,
                        // A `D` (double) return is a 64-bit value node (inc 32).
                        b'D' => IrType::Double,
                        // A `F` (float) return is a 32-bit value node (inc 33).
                        b'F' => IrType::Float,
                        _ => IrType::Int,
                    };
                    let call = self.graph.add(Op::Call { info_ptr }, ty, inputs, Some(pc));
                    self.note_splice_call(call);
                    self.mem = call;
                    // nio21 wave 3 L2 / JIT round 19 wave 5 (SPLICE4-1): the call's
                    // in-frame handler edges.
                    self.plant_call_handler_entry(pc, call, code_len);
                    if returns_value {
                        self.push(call);
                    }
                    pc += 5;
                }
                // dup
                0x59 => {
                    let top = self.peek();
                    self.push(top);
                    pc += 1;
                }
                // pop
                0x57 => {
                    self.pop();
                    pc += 1;
                }

                // dup2 — JVMS has two forms, and this builder's abstract stack
                // makes the second one trivial.
                //
                // Form 2 is a single CATEGORY-2 value duplicated. The stack here
                // holds one entry per VALUE rather than per slot, so a `long` or
                // `double` is one entry and duplicating it is exactly `dup`.
                //
                // Form 1 is the top TWO category-1 values duplicated as a pair:
                // `.., v2, v1` becomes `.., v2, v1, v2, v1`.
                //
                // A category-1 top over a category-2 second is not a legal
                // `dup2` shape in either form — the verifier does not produce
                // it — so it is refused rather than shuffled, the same call
                // `dup_x1` above makes for the same reason: guessing which
                // entry "the value below" names is how the builder's stack and
                // the verifier's stack quietly disagree.
                //
                // Measured on `org.h2.test.db.TestAlter`: one of the six
                // `unsupported_opcode` build refusals.
                0x5c => {
                    let Some(v1) = self.pop_opt() else {
                        return ir_build_bail(line!(), pc);
                    };
                    let v1_cat2 = matches!(
                        self.graph.nodes[v1 as usize].ty,
                        IrType::Long | IrType::Double
                    );
                    if v1_cat2 {
                        self.push(v1);
                        self.push(v1);
                    } else {
                        let Some(v2) = self.pop_opt() else {
                            return ir_build_bail(line!(), pc);
                        };
                        if matches!(
                            self.graph.nodes[v2 as usize].ty,
                            IrType::Long | IrType::Double
                        ) {
                            return ir_build_bail(line!(), pc);
                        }
                        self.push(v2);
                        self.push(v1);
                        self.push(v2);
                        self.push(v1);
                    }
                    pc += 1;
                }

                // pop2, swap, dup_x2, dup2_x1, dup2_x2 — the rest of the JVMS
                // stack shuffles, on the same one-entry-per-VALUE model as
                // `dup_x1` and `dup2` above: a category-2 value is one entry,
                // its category is read from the node's type, and a shape the
                // verifier cannot produce (a category-2 value where the form
                // needs category 1) is refused, never guessed.
                //
                // `jit_scan` accepted all five, so `ir_compatible` admitted
                // their methods and the build then refused them at the
                // catch-all after doing the partial work.
                0x58 | 0x5f | 0x5b | 0x5d | 0x5e => {
                    macro_rules! is_cat2 {
                        ($v:expr) => {
                            matches!(
                                self.graph.nodes[$v as usize].ty,
                                IrType::Long | IrType::Double
                            )
                        };
                    }
                    macro_rules! pop_value {
                        () => {
                            match self.pop_opt() {
                                Some(v) => v,
                                None => return ir_build_bail(line!(), pc),
                            }
                        };
                    }
                    macro_rules! pop_cat1 {
                        () => {{
                            let v = pop_value!();
                            if is_cat2!(v) {
                                return ir_build_bail(line!(), pc);
                            }
                            v
                        }};
                    }
                    match op {
                        // pop2: one category-2 value, or two category-1 values.
                        0x58 => {
                            let v1 = pop_value!();
                            if !is_cat2!(v1) {
                                let _ = pop_cat1!();
                            }
                        }
                        // swap: `.., v2, v1` -> `.., v1, v2`, both category 1.
                        0x5f => {
                            let v1 = pop_cat1!();
                            let v2 = pop_cat1!();
                            self.push(v1);
                            self.push(v2);
                        }
                        // dup_x2: v1 is category 1.
                        //   form 1: `.., v3, v2, v1` -> `.., v1, v3, v2, v1`
                        //   form 2: `.., v2(cat2), v1` -> `.., v1, v2, v1`
                        0x5b => {
                            let v1 = pop_cat1!();
                            let v2 = pop_value!();
                            if is_cat2!(v2) {
                                self.push(v1);
                                self.push(v2);
                                self.push(v1);
                            } else {
                                let v3 = pop_cat1!();
                                self.push(v1);
                                self.push(v3);
                                self.push(v2);
                                self.push(v1);
                            }
                        }
                        // dup2_x1:
                        //   form 1: `.., v3, v2, v1` -> `.., v2, v1, v3, v2, v1`
                        //   form 2: `.., v2, v1(cat2)` -> `.., v1, v2, v1`
                        0x5d => {
                            let v1 = pop_value!();
                            if is_cat2!(v1) {
                                let v2 = pop_cat1!();
                                self.push(v1);
                                self.push(v2);
                                self.push(v1);
                            } else {
                                let v2 = pop_cat1!();
                                let v3 = pop_cat1!();
                                self.push(v2);
                                self.push(v1);
                                self.push(v3);
                                self.push(v2);
                                self.push(v1);
                            }
                        }
                        // dup2_x2:
                        //   form 1: `.., v4, v3, v2, v1` -> `.., v2, v1, v4, v3, v2, v1`
                        //   form 2: `.., v3, v2, v1(cat2)` -> `.., v1, v3, v2, v1`
                        //   form 3: `.., v3(cat2), v2, v1` -> `.., v2, v1, v3, v2, v1`
                        //   form 4: `.., v2(cat2), v1(cat2)` -> `.., v1, v2, v1`
                        _ => {
                            let v1 = pop_value!();
                            if is_cat2!(v1) {
                                let v2 = pop_value!();
                                if is_cat2!(v2) {
                                    self.push(v1);
                                    self.push(v2);
                                    self.push(v1);
                                } else {
                                    let v3 = pop_cat1!();
                                    self.push(v1);
                                    self.push(v3);
                                    self.push(v2);
                                    self.push(v1);
                                }
                            } else {
                                let v2 = pop_cat1!();
                                let v3 = pop_value!();
                                if is_cat2!(v3) {
                                    self.push(v2);
                                    self.push(v1);
                                    self.push(v3);
                                    self.push(v2);
                                    self.push(v1);
                                } else {
                                    let v4 = pop_cat1!();
                                    self.push(v2);
                                    self.push(v1);
                                    self.push(v4);
                                    self.push(v3);
                                    self.push(v2);
                                    self.push(v1);
                                }
                            }
                        }
                    }
                    pc += 1;
                }

                // ifeq..ifle (0x99..0x9e) — compare int against zero
                0x99..=0x9e => {
                    let offset = i16::from_be_bytes([code[pc + 1], code[pc + 2]]) as i32;
                    let target_pc = (pc as i32 + offset) as usize;
                    let val = self.pop();
                    let zero = self.iconst(0);
                    let cc = match op {
                        0x99 => CmpOp::Eq,
                        0x9a => CmpOp::Ne,
                        0x9b => CmpOp::Lt,
                        0x9c => CmpOp::Ge,
                        0x9d => CmpOp::Gt,
                        0x9e => CmpOp::Le,
                        _ => unreachable!(),
                    };
                    let cmp = self.add_data(Op::Cmp(cc), IrType::Int, vec![val, zero], pc);
                    if self.prune_always_taken_branch(pc, cmp, target_pc) {
                        pc += 3; // into the cold arm, as dead code
                        continue;
                    }
                    // nio21 wave 17 (lane L4): the mirror, for a never-taken
                    // edge: a guard, and the walk continues on the fall-through.
                    if self.prune_never_taken_branch(pc, cmp, target_pc) {
                        pc += 3;
                        continue;
                    }
                    if self.prune_cold_throw_arm(code, pc, cmp, target_pc) {
                        pc += 3; // into the cold throw arm, as dead code
                        continue;
                    }
                    if self.prune_cold_throw_side_entry(code, pc, cmp, target_pc) {
                        pc += 3; // the fall-through; the jump into the arm is a guard
                        continue;
                    }
                    let if_node =
                        self.graph
                            .add(Op::If, IrType::Control, vec![self.ctrl, cmp], Some(pc));
                    let true_ctrl =
                        self.graph
                            .add(Op::Proj(0), IrType::Control, vec![if_node], Some(pc));
                    let false_ctrl =
                        self.graph
                            .add(Op::Proj(1), IrType::Control, vec![if_node], Some(pc));

                    // True edge → target. `add_merge_predecessor` snapshots
                    // the frame and writes none of `locals`, `stack` or `mem`
                    // (nor do the `patch_loop_backedge` / `retype_phi` it may
                    // call), so the fall-through edge carries the same state
                    // with no save/restore. Round 11 wave 2 removed the pair of
                    // whole-frame clones per branch that used to "restore" it.
                    self.ctrl = true_ctrl;
                    self.add_merge_predecessor(target_pc);

                    // False edge → fall-through
                    self.ctrl = false_ctrl;

                    pc += 3;
                }

                // if_icmpeq..if_icmple (0x9f..0xa4)
                0x9f..=0xa4 => {
                    let offset = i16::from_be_bytes([code[pc + 1], code[pc + 2]]) as i32;
                    let target_pc = (pc as i32 + offset) as usize;
                    let b = self.pop();
                    let a = self.pop();
                    let cc = match op {
                        0x9f => CmpOp::Eq,
                        0xa0 => CmpOp::Ne,
                        0xa1 => CmpOp::Lt,
                        0xa2 => CmpOp::Ge,
                        0xa3 => CmpOp::Gt,
                        0xa4 => CmpOp::Le,
                        _ => unreachable!(),
                    };
                    let cmp = self.add_data(Op::Cmp(cc), IrType::Int, vec![a, b], pc);
                    if self.prune_always_taken_branch(pc, cmp, target_pc) {
                        pc += 3; // into the cold arm, as dead code
                        continue;
                    }
                    // nio21 wave 17 (lane L4): the mirror, for a never-taken
                    // edge: a guard, and the walk continues on the fall-through.
                    if self.prune_never_taken_branch(pc, cmp, target_pc) {
                        pc += 3;
                        continue;
                    }
                    if self.prune_cold_throw_arm(code, pc, cmp, target_pc) {
                        pc += 3; // into the cold throw arm, as dead code
                        continue;
                    }
                    if self.prune_cold_throw_side_entry(code, pc, cmp, target_pc) {
                        pc += 3; // the fall-through; the jump into the arm is a guard
                        continue;
                    }
                    let if_node =
                        self.graph
                            .add(Op::If, IrType::Control, vec![self.ctrl, cmp], Some(pc));
                    let true_ctrl =
                        self.graph
                            .add(Op::Proj(0), IrType::Control, vec![if_node], Some(pc));
                    let false_ctrl =
                        self.graph
                            .add(Op::Proj(1), IrType::Control, vec![if_node], Some(pc));

                    // No save/restore: see the 0x99..=0x9e arm.
                    self.ctrl = true_ctrl;
                    self.add_merge_predecessor(target_pc);

                    self.ctrl = false_ctrl;

                    pc += 3;
                }

                // ifnull (0xc6) / ifnonnull (0xc7) — compare a reference
                // against null; if_acmpeq (0xa5) / if_acmpne (0xa6) — compare
                // two references. Structurally identical to the int forms
                // above; the operands are `Ref`-typed, and `ir_lower`'s
                // `Op::Cmp` selects a 64-bit CMP when either operand is, so a
                // pointer whose low 32 bits happen to be zero is not mistaken
                // for null.
                0xa5 | 0xa6 | 0xc6 | 0xc7 => {
                    let offset = i16::from_be_bytes([code[pc + 1], code[pc + 2]]) as i32;
                    let target_pc = (pc as i32 + offset) as usize;
                    let (a, b) = if op == 0xc6 || op == 0xc7 {
                        let val = self.pop();
                        if self.const_ref_slot_of(val).is_some() {
                            if op == 0xc7 {
                                self.const_folded_dead_targets.insert(pc + 3);
                                self.add_merge_predecessor(target_pc);
                                self.ctrl = NO_NODE;
                                self.const_folded_dead_skip = true;
                            } else {
                                self.const_folded_dead_targets.insert(target_pc);
                            }
                            pc += 3;
                            continue;
                        }
                        (val, self.aconst_null())
                    } else {
                        let b = self.pop();
                        let a = self.pop();
                        (a, b)
                    };
                    let cc = if op == 0xc6 || op == 0xa5 {
                        CmpOp::Eq
                    } else {
                        CmpOp::Ne
                    };
                    let cmp = self.add_data(Op::Cmp(cc), IrType::Int, vec![a, b], pc);
                    if self.prune_always_taken_branch(pc, cmp, target_pc) {
                        pc += 3; // into the cold arm, as dead code
                        continue;
                    }
                    // nio21 wave 17 (lane L4): the mirror, for a never-taken
                    // edge: a guard, and the walk continues on the fall-through.
                    if self.prune_never_taken_branch(pc, cmp, target_pc) {
                        pc += 3;
                        continue;
                    }
                    if self.prune_cold_throw_arm(code, pc, cmp, target_pc) {
                        pc += 3; // into the cold throw arm, as dead code
                        continue;
                    }
                    if self.prune_cold_throw_side_entry(code, pc, cmp, target_pc) {
                        pc += 3; // the fall-through; the jump into the arm is a guard
                        continue;
                    }
                    let if_node =
                        self.graph
                            .add(Op::If, IrType::Control, vec![self.ctrl, cmp], Some(pc));
                    let true_ctrl =
                        self.graph
                            .add(Op::Proj(0), IrType::Control, vec![if_node], Some(pc));
                    let false_ctrl =
                        self.graph
                            .add(Op::Proj(1), IrType::Control, vec![if_node], Some(pc));

                    // True edge → target (no save/restore: see the
                    // 0x99..=0x9e arm).
                    self.ctrl = true_ctrl;
                    self.add_merge_predecessor(target_pc);

                    // False edge → fall-through
                    self.ctrl = false_ctrl;

                    // Round 12 wave 5 (lane iropt4): inside a splice, the edge
                    // on which the tested reference is non-null carries that
                    // fact (`SpliceFenceRelax`, relaxation 2): the fall-through
                    // of `ifnull` from here, the taken edge of `ifnonnull` from
                    // its target's merge.
                    if !self.splice.is_empty() && self.splice_fence_relax.null_facts {
                        if op == 0xc6 {
                            self.splice_null_facts.insert(a);
                            // The fall-through pc can itself be a merge target
                            // (a block leader whose only input is this edge),
                            // and activating it clears every fact; keyed by the
                            // edge, `note_splice_merge_null_facts` restores it
                            // exactly when this edge is the merge's sole input.
                            self.splice_edge_null_facts.insert(false_ctrl, a);
                        } else if op == 0xc7 {
                            self.splice_edge_null_facts.insert(true_ctrl, a);
                        }
                    }

                    pc += 3;
                }

                // goto
                0xa7 => {
                    let offset = i16::from_be_bytes([code[pc + 1], code[pc + 2]]) as i32;
                    let target_pc = (pc as i32 + offset) as usize;
                    self.add_merge_predecessor(target_pc);
                    self.ctrl = NO_NODE; // dead after unconditional jump
                    pc += 3;
                }

                // goto_w — `goto` with a four-byte signed offset. The same arm;
                // only the operand width differs. javac emits it only past a
                // 32 KiB method (beyond `IR_MAX_BYTECODE_SIZE`), but ASM-based
                // rewriters and instrumentation agents widen jumps freely, and
                // the verifier, `bytecode_analysis` and the merge pre-scan all
                // already treat it as a branch. It was the one control-flow
                // opcode with no arm, so such a method built all the way to it
                // and then died at the `_ =>` catch-all.
                0xc8 => {
                    if pc + 4 >= code.len() {
                        return ir_build_bail(line!(), pc);
                    }
                    let offset = i32::from_be_bytes([
                        code[pc + 1],
                        code[pc + 2],
                        code[pc + 3],
                        code[pc + 4],
                    ]);
                    let Some(target_pc) = pc.checked_add_signed(offset as isize) else {
                        return ir_build_bail(line!(), pc);
                    };
                    self.add_merge_predecessor(target_pc);
                    self.ctrl = NO_NODE; // dead after unconditional jump
                    pc += 5;
                }

                // ireturn
                //
                // A method may have several `return` statements, so this builds
                // one `Op::Return` terminator per `ireturn`. `graph.exit` records
                // only the LAST one (each `ireturn` overwrites it) — it is an
                // "an exit" marker, NOT the sole exit. Consumers that need every
                // exit must enumerate all `Op::Return` nodes: the scheduler does
                // (it blocks every control node), and `eliminate_dead_nodes`
                // roots from ALL returns (rooting only from `graph.exit` would
                // delete the other return paths — see its comment).
                // areturn (0xb0) shares this arm: `Op::Return` loads the value's
                // frame slot into RAX and tears the frame down, which is the
                // same ABI for a reference as for an int — the single-pass
                // backend returns a raw pointer in RAX too. It was simply never
                // wired, and it is the second-largest builder refusal on a real
                // workload (25 of 141 on a Hibernate class): any method that
                // returns an object was refused at its return.
                0xac | 0xb0 => {
                    let val = self.pop();
                    // JVMS §6.5 `ireturn` narrowing for a `Z`/`B`/`C`/`S`
                    // return — never `areturn`. Inside a splice the tag is the
                    // spliced callee's own, so it applies at every return edge
                    // of that body, single- or multi-return alike.
                    let val = if code[pc] == 0xac {
                        let tag = match self.splice.last() {
                            Some(frame) => frame.return_narrow,
                            None => self.return_narrow,
                        };
                        self.narrow_int_return(val, tag, pc)
                    } else {
                        val
                    };
                    // Inside a splice this is not a method exit: it hands the
                    // value back to the caller's operand stack and the walk
                    // resumes after the `invoke`. No `Op::Return`, and `ctrl`
                    // stays live.
                    if !self.splice.is_empty() {
                        match self.splice_return(Some(val), pc) {
                            Some(next) => {
                                pc = next;
                                continue;
                            }
                            None => return ir_build_bail(line!(), pc),
                        }
                    }
                    let ret =
                        self.graph
                            .add(Op::Return, IrType::Void, vec![self.ctrl, val], Some(pc));
                    self.graph.exit = ret;
                    self.ctrl = NO_NODE;
                    pc += 1;
                }

                // lreturn
                0xad => {
                    let val = self.pop();
                    if !self.splice.is_empty() {
                        match self.splice_return(Some(val), pc) {
                            Some(next) => {
                                pc = next;
                                continue;
                            }
                            None => return ir_build_bail(line!(), pc),
                        }
                    }
                    let ret =
                        self.graph
                            .add(Op::Return, IrType::Void, vec![self.ctrl, val], Some(pc));
                    self.graph.exit = ret;
                    self.ctrl = NO_NODE;
                    pc += 1;
                }

                // freturn (inc 33) / dreturn (inc 32) — an FP return rides RAX as
                // bits (the JIT i64 return ABI): the interpreter's post-JIT path
                // reads `result as u64`→`f64::from_bits` (`D`) or `result as u32`
                // →`f32::from_bits` (`F`), and `lower_terminator`'s `Op::Return`
                // does `load_to_rax` from the value's slot, so no XMM return
                // marshalling is needed. Identical to `lreturn`/`ireturn`.
                //
                // `float` (inc 33): the slot holds 32 bits; `load_to_rax` reads 64,
                // so RAX's UPPER 32 are stale — harmless because every consumer
                // reads only the low 32 (`result as u32`; downstream `MOVSS`). The
                // one hazard is a `+0.0f` whose stale upper bits make RAX ==
                // `i64::MIN`: on a `F` CALL result the call-site `dispatch_threw`
                // peek (extended to `IrType::Float`) disambiguates it, and the
                // restore (RAX=`i64::MIN`, low-32=0) preserves `+0.0f` — `i64::MIN`
                // has low-63 = 0, so only `+0.0f` can ever collide.
                //
                // Reachable only under the FP gate; the `-0.0`/`Long.MIN_VALUE` ↔
                // `i64::MIN` collision on a `D`/`F` call result is handled by the
                // item-1 `dispatch_threw` peek.
                0xae | 0xaf => {
                    let val = self.pop();
                    if !self.splice.is_empty() {
                        match self.splice_return(Some(val), pc) {
                            Some(next) => {
                                pc = next;
                                continue;
                            }
                            None => return ir_build_bail(line!(), pc),
                        }
                    }
                    let ret =
                        self.graph
                            .add(Op::Return, IrType::Void, vec![self.ctrl, val], Some(pc));
                    self.graph.exit = ret;
                    self.ctrl = NO_NODE;
                    pc += 1;
                }

                // return (void)
                0xb1 => {
                    if !self.splice.is_empty() {
                        match self.splice_return(None, pc) {
                            Some(next) => {
                                pc = next;
                                continue;
                            }
                            None => return ir_build_bail(line!(), pc),
                        }
                    }
                    let ret = self
                        .graph
                        .add(Op::Return, IrType::Void, vec![self.ctrl], Some(pc));
                    self.graph.exit = ret;
                    self.ctrl = NO_NODE;
                    pc += 1;
                }

                // lconst_0, lconst_1
                0x09 => {
                    let c = self.lconst(0);
                    self.push(c);
                    pc += 1;
                }
                0x0a => {
                    let c = self.lconst(1);
                    self.push(c);
                    pc += 1;
                }
                // ldc2_w (long OR double constant from the constant pool) — inc 26
                // (long) / inc 35 (double). The resolved `(bits, is_double)` comes
                // from `set_ldc2w_info`; an absent pc bails to single-pass. A
                // `double` constant lowers to `dconst` (`Op::ConstF`/`Double`), a
                // `long` to `lconst` (`Op::Const`/`Long`). (`ldc2_w` is 3 bytes:
                // opcode + 2-byte CP index.)
                0x14 => {
                    let (val, is_double) = match self.ldc2w_info.get(&pc) {
                        Some(&v) => v,
                        None => return ir_build_bail(line!(), pc),
                    };
                    let c = if is_double {
                        self.dconst(f64::from_bits(val as u64))
                    } else {
                        self.lconst(val)
                    };
                    self.push(c);
                    pc += 3;
                }
                // ldc (0x12, 1-byte CP index) / ldc_w (0x13, 2-byte CP index) —
                // cov-01 increment 1, the IMMEDIATE case.
                //
                // `ldc` + `ldc_w` + `getstatic` was 189 of the 273 opcode-gap
                // events measured on 2026-08-03 — 69% of every opcode
                // `IrBuilder::build` had no arm for at all
                // (`ir-coverage-survey-20260803.md`).
                //
                // A resolved `int` / `float` constant is a constant node and
                // nothing else — no memory edge, no safepoint, no GC
                // interaction — so it is typed and pushed exactly the way the
                // `ldc2_w` arm above types its `long` / `double`. `is_float`
                // comes from the resolver rather than from the opcode because
                // `ldc` is polymorphic and `is_float_opcode` does NOT list it:
                // a method whose only FP is `ldc 1.5f` is admitted through the
                // int clause, so the builder cannot infer the width from
                // admission either. See `JitLdcConstant::Immediate`.
                //
                // Increments 2 and 3 add the two REFERENCE constants — a String
                // literal and a Class mirror. Neither is an immediate: each
                // names a constant-pool site whose value is materialised on
                // every execution by the same runtime helper the single-pass
                // backend calls, because an `ObjectRef` baked at compile time is
                // stale the moment a relocating collector moves it. Both are
                // therefore `[ctrl, mem]` safepoint nodes, not constants — see
                // `Op::ConstString` / `Op::ConstClass`.
                //
                // Everything still absent from all three tables — `MethodHandle`,
                // `MethodType`, condy, a site whose helper is unwired, a float
                // with the FP gate off — bails the method to single-pass, which
                // DOES compile all of them. That is the fail-closed half of this
                // arm and it is the point: a constant materialised without its
                // resolution side effects (interning, class loading, `<clinit>`)
                // is a wrong-code bug, not a missing optimisation.
                0x12 | 0x13 => {
                    let width = if op == 0x12 { 2 } else { 3 };
                    // Round 14 wave 7 (lane sync7, SS8-5): `holdsLock(C.class)`
                    // inside a `static synchronized` splice of `C`, built as
                    // `1` with the `invokestatic` after it skipped.
                    if self.try_fold_self_class_holds_lock(code, pc, width) {
                        pc += width + 3;
                    } else if let Some(&(bits, is_float)) = self.ldc_info.get(&pc) {
                        let c = if is_float {
                            self.fconst(f32::from_bits(bits as u32))
                        } else {
                            self.iconst(bits)
                        };
                        self.push(c);
                        pc += width;
                    } else if let Some(&(holder_class_id, cp_idx)) = self.ldc_string_info.get(&pc) {
                        let slot_addr = self
                            .ldc_slot_info
                            .get(&(holder_class_id, cp_idx))
                            .copied()
                            .unwrap_or(0);
                        let s = self.graph.add(
                            Op::ConstString {
                                holder_class_id,
                                cp_idx,
                                slot_addr,
                            },
                            IrType::Ref,
                            vec![self.ctrl, self.mem],
                            Some(pc),
                        );
                        // The node IS the new memory token, exactly as
                        // `Op::Call` is: interning can allocate, and a
                        // collection may run inside the helper, so nothing may
                        // be reordered across it.
                        self.mem = s;
                        self.push(s);
                        pc += width;
                    } else if let Some(&(holder_class_id, cp_idx)) = self.ldc_class_info.get(&pc) {
                        let slot_addr = self
                            .ldc_slot_info
                            .get(&(holder_class_id, cp_idx))
                            .copied()
                            .unwrap_or(0);
                        let c = self.graph.add(
                            Op::ConstClass {
                                holder_class_id,
                                cp_idx,
                                slot_addr,
                            },
                            IrType::Ref,
                            vec![self.ctrl, self.mem],
                            Some(pc),
                        );
                        self.mem = c;
                        self.push(c);
                        pc += width;
                    } else if let Some(slot_addr) = self
                        .splice
                        .is_empty()
                        .then(|| self.ldc_condy_slot_info.get(&pc).copied())
                        .flatten()
                    {
                        // JIT r23 wave 4b, lane L2
                        // (`CRATONVM_JIT_IR_LDC_CONDY_SLOT`): a resolved
                        // reference condy. JVMS 5.4.3.6 resolves the entry
                        // once, so the read of its never-cleared GC-maintained
                        // slot is the whole instruction: no call, no
                        // safepoint, no memory effect (`Op::IndyRootRef`).
                        let v = self.graph.add(
                            Op::IndyRootRef { slot_addr },
                            IrType::Ref,
                            vec![self.ctrl],
                            Some(pc),
                        );
                        self.push(v);
                        pc += width;
                    } else if !self.splice.is_empty()
                        && self.unloaded_static_call_pcs.contains(&pc)
                        && self.plant_unresolved_ldc_exit(pc)
                    {
                        // JIT r23 wave 9, lane L6: a spliced `ldc` of a String
                        // not resolved yet, on an arm the callee never ran. The
                        // exit always fires; the null keeps the stack depth.
                        let null = self.aconst_null();
                        self.push(null);
                        pc += width;
                    } else {
                        return ir_build_bail(line!(), pc);
                    }
                }
                // getstatic — cov-01 increment 4, and the single largest opcode
                // in the survey (92 of 273 events).
                //
                // A load from a statics base plus the ref/non-ref split, exactly
                // as the lane brief frames it. `static_field_info` names the
                // offset and the type tag; this arm's job is to match the
                // single-pass `0xb2` arm's behaviour, not to invent one, so the
                // class-initialisation question is answered where that arm
                // answers it — in the lowering, which picks between the direct
                // load (only for a class the resolver reports already
                // initialised) and `helpers.getstatic` (which runs `<clinit>`
                // and can throw).
                //
                // Every JVM field type lowers. `J`/`D`/`F` were refused when this
                // arm landed, on the grounds that admitting one would put a
                // `Long`/`Double`/`Float` node in a graph whose admission clause
                // may have been the int one. That is a real hazard and the
                // refusal was in the wrong PLACE: `getstatic` is polymorphic and
                // is listed by neither `is_category2_opcode` nor
                // `is_float_opcode`, so the builder cannot see the width — but
                // the CALLER can, because it resolved the type tag in order to
                // build this table at all. So the gate moved to the feed in
                // `try_compile`, which admits a `J` entry only under
                // `ir_emit_long` and a `D`/`F` entry only under `ir_emit_fp`,
                // exactly as it already does for a float `ldc` and as the
                // `ldc2_w` feed does for its `(bits, is_double)`.
                //
                // A tag absent from this map is therefore either unresolvable or
                // gated off, and both bail — the same fail-closed answer, now
                // decided by the one party that has the information.
                0xb2 => {
                    let (class_id, field_index, type_tag, is_volatile) =
                        match self.static_field_info.get(&pc).copied() {
                            Some(si) => si,
                            // Wave 94, lane L2: an admitted unloaded-owner
                            // site traps (`plant_field_site_trap`).
                            None if self.plant_field_site_trap(code, code_len, pc, 0xb2) => {
                                pc += 3;
                                continue;
                            }
                            None => return self.rowless_field_site_bail(line!(), pc, 0xb2),
                        };
                    let ty = match type_tag {
                        b'I' | b'Z' | b'B' | b'C' | b'S' => IrType::Int,
                        b'L' | b'[' => IrType::Ref,
                        b'J' => IrType::Long,
                        b'D' => IrType::Double,
                        b'F' => IrType::Float,
                        _ => return ir_build_bail(line!(), pc),
                    };
                    // A spliced `getstatic` whose owner is not loaded (the
                    // resolver's class-id-0 row, on an arm the callee never
                    // ran): the unloaded `new`'s precise exit. The typed zero
                    // keeps the verified stack; the code reading it is
                    // unreachable.
                    if class_id == crate::UNLOADED_SPLICE_CLASS_ID
                        && !self.splice.is_empty()
                        && self.unloaded_static_call_pcs.contains(&pc)
                    {
                        if !self.plant_unloaded_new_exit(pc) {
                            return ir_build_bail(line!(), pc);
                        }
                        let zero = match ty {
                            IrType::Ref => self.aconst_null(),
                            IrType::Long => self.lconst(0),
                            IrType::Float => self.fconst(0.0),
                            IrType::Double => self.dconst(0.0),
                            _ => self.iconst(0),
                        };
                        self.push(zero);
                        pc += 3;
                        continue;
                    }
                    // A `field_index` past `u32` cannot be encoded in the baked
                    // displacement; refuse rather than truncate.
                    let Ok(field_index) = u32::try_from(field_index) else {
                        return ir_build_bail(line!(), pc);
                    };
                    // `CRATONVM_JIT_IR_STATIC_FINAL_EXACT_CLASS`: a trusted
                    // `static final` the VM filed as null at its class's
                    // `<clinit>` end (class `0`): the constant null. Its class
                    // is initialized, so the read has no effect to keep.
                    if matches!(type_tag, b'L' | b'[')
                        && !is_volatile
                        && self
                            .static_final_exact_classes
                            .get(&(class_id, field_index))
                            == Some(&0)
                    {
                        let null = self.aconst_null();
                        self.push(null);
                        pc += 3;
                        continue;
                    }
                    if !is_volatile {
                        if let Some(v) =
                            self.try_emit_static_final_fold(class_id, field_index, type_tag)
                        {
                            self.push(v);
                            pc += 3;
                            continue;
                        }
                    }
                    // Round 16 wave 14 (lane osrir, MON3-2): an earlier plain
                    // read of the same static nothing can have written since.
                    // See `static_load_memo_forward`.
                    if let Some(prev) =
                        self.static_load_memo_forward(class_id, field_index, ty, is_volatile)
                    {
                        self.push(prev);
                        pc += 3;
                        continue;
                    }
                    let load = self.graph.add(
                        Op::LoadStatic {
                            class_id,
                            field_index,
                            type_tag,
                            is_volatile,
                        },
                        ty,
                        vec![self.ctrl, self.mem],
                        Some(pc),
                    );
                    // Both lowerings can reach the runtime (the helper one runs
                    // `<clinit>`), and a volatile read is a JMM acquire, so the
                    // node advances the memory token like every other
                    // `MemAccess::Opaque` node.
                    // `CRATONVM_JIT_IR_STATIC_FINAL_EXACT_CLASS`: except a
                    // trusted `static final` the VM filed non-null, whose class
                    // is initialized and whose value never changes: a pure read,
                    // off the chain, which dead-code elimination may drop.
                    let trusted = matches!(type_tag, b'L' | b'[')
                        && !is_volatile
                        && self
                            .graph
                            .static_final_non_null
                            .contains(&(class_id, field_index));
                    if !trusted {
                        self.mem = load;
                    }
                    self.push(load);
                    self.static_load_memo_record(class_id, field_index, load, is_volatile);
                    pc += 3;
                }
                // putstatic — the helper call the single-pass `0xb3` arm makes
                // (see `Op::StoreStatic`). The row is the one
                // `set_static_field_info` already carries for this pc: the
                // caller feeds every `static_field_ops` entry, reads and writes
                // alike, with the value-tier gate applied there, so a `J`/`D`/`F`
                // site the long/FP tier is off for is absent and bails here.
                //
                // Inside a splice: the resolver admits a callee body with a
                // `putstatic` only under `CRATONVM_JIT_IR_SPLICE_PUTSTATIC`
                // (JIT round 21, FG16-3; otherwise `ir-splice-putstatic`) or
                // as a `static synchronized` own-class write, and the write is
                // committed like a caller-visible `putfield` (below).
                0xb3 => {
                    let (class_id, field_index, type_tag, is_volatile) =
                        match self.static_field_info.get(&pc).copied() {
                            Some(si) => si,
                            // Wave 94, lane L2: see the getstatic arm.
                            None if self.plant_field_site_trap(code, code_len, pc, 0xb3) => {
                                pc += 3;
                                continue;
                            }
                            None => return self.rowless_field_site_bail(line!(), pc, 0xb3),
                        };
                    // Round 14 wave 2 (lane syncsplice): except the own-class
                    // write of a `static synchronized` splice, whose body
                    // cannot trap, so nothing re-runs the write.
                    //
                    // JIT round 21 (lane staticfwd, FG16-3): and, under
                    // `CRATONVM_JIT_IR_SPLICE_PUTSTATIC`, a plain write the
                    // resolver admitted (its class initialised, so the helper
                    // runs no `<clinit>` and cannot trap), COMMITTED like a
                    // `putfield` into a caller-visible object: the store node
                    // joins `splice_committed_bases` (it is never on the
                    // operand stack, so it proves no base non-null), which
                    // raises the replay fence for the rest of the outermost
                    // splice (`admits_after_committed_splice_store`).
                    let mut splice_commit = false;
                    if !self.splice.is_empty()
                        && !self.sync_splice_owns_static(class_id, is_volatile)
                    {
                        if is_volatile || class_id == 0 || !ir_splice_putstatic_enabled() {
                            return ir_build_bail(line!(), pc);
                        }
                        // j27 wave 7 (lane T1STATICFWD, STATICFWD21-1): a
                        // write a replay of the outermost body re-runs
                        // harmlessly commits nothing.
                        splice_commit = !self.splice_static_store_replays_idempotently(code);
                    }
                    if !matches!(
                        type_tag,
                        b'I' | b'Z' | b'B' | b'C' | b'S' | b'L' | b'[' | b'J' | b'D' | b'F'
                    ) {
                        return ir_build_bail(line!(), pc);
                    }
                    let Ok(field_index) = u32::try_from(field_index) else {
                        return ir_build_bail(line!(), pc);
                    };
                    let value = self.pop();
                    if value == NO_NODE {
                        return ir_build_bail(line!(), pc);
                    }
                    // JVMS §6.5 `putstatic`: a `boolean`/`byte`/`char`/`short`
                    // static holds the value narrowed to its type, exactly as
                    // the `0xb5` arm narrows an instance field.
                    let value = self.narrow_to_field_type(value, type_tag, pc);
                    let store = self.graph.add(
                        Op::StoreStatic {
                            class_id,
                            field_index,
                            type_tag,
                            is_volatile,
                        },
                        IrType::Memory,
                        vec![self.ctrl, self.mem, value],
                        Some(pc),
                    );
                    self.mem = store;
                    if splice_commit {
                        self.splice_committed_bases.insert(store);
                    }
                    // JIT round 21 (lane staticfwd, OI-4): the cell now holds
                    // `value`. See `static_load_memo_record_store`.
                    self.static_load_memo_record_store(
                        class_id,
                        field_index,
                        type_tag,
                        value,
                        is_volatile,
                    );
                    pc += 3;
                }

                // tableswitch / lookupswitch — an `Op::Switch` multi-way
                // terminator when `ir::switch_is_dense` admits the key range,
                // and the historical CMP-equality chain otherwise.
                //
                // # The two shapes, and why both exist
                //
                // The chain is one `Op::Const`, one `Op::Cmp`, one `Op::If` and
                // two `Op::Proj` **per case** — five nodes each — and up to one
                // sequential compare per case at run time. It is correct, it is
                // what this arm has always emitted, and for two or three cases
                // it is also the right answer: a jump table costs an indirect
                // branch the predictor mispredicts, and four compares do not.
                // `ir_lower` keeps a chain lowering for exactly that reason, so
                // the floor is never lost.
                //
                // `Op::Switch` is one node plus one `Op::Proj` per span slot,
                // and `ir_lower` turns it into a jump table or a binary search.
                // The span, not the case count, is what it costs — which is
                // why `switch_is_representable` is asked HERE rather than left
                // for the lowerer to regret, and why the span it is asked
                // about is trimmed to the keys that actually go somewhere.
                //
                // # What the edge set has to preserve
                //
                // JVMS §6.5 gives every switch a default edge and one edge per
                // listed key, and says nothing about them being distinct
                // targets. Both shapes below preserve all of them: the dense
                // form gives every slot in `[low, high]` its own projection,
                // pointing at that key's target if the payload listed one and
                // at the default target if it did not, so two cases naming the
                // same pc stay two edges into the same merge, and a hole in a
                // `tableswitch` stays an edge to default rather than becoming
                // an absent edge.
                0xaa | 0xab => {
                    // A relocated body retains the padding of its ORIGINAL
                    // code array. Decode within that slice, not the combined
                    // buffer, and rebase every verified target. The enclosing
                    // body's bound also prevents a malformed switch from reading
                    // the next appended callee.
                    let switch = if pc < code_len {
                        crate::bytecode_analysis::switch_table(code, code_len, pc)
                    } else {
                        self.inline_sites
                            .values()
                            .find(|site| {
                                site.base <= pc && pc < site.base.saturating_add(site.code_len)
                            })
                            .and_then(|site| {
                                let body =
                                    code.get(site.base..site.base.checked_add(site.code_len)?)?;
                                let mut table = crate::bytecode_analysis::switch_table(
                                    body,
                                    site.code_len,
                                    pc.checked_sub(site.base)?,
                                )?;
                                table.default = site.base.checked_add(table.default)?;
                                for (_, target) in &mut table.cases {
                                    *target = site.base.checked_add(*target)?;
                                }
                                Some(table)
                            })
                    };
                    let Some((len, default_target, cases)) =
                        switch.map(|t| (t.len, t.default, t.cases))
                    else {
                        return ir_build_bail(line!(), pc);
                    };
                    let key = self.pop();
                    if let Op::Const(k) = self.graph.nodes[key as usize].op {
                        let k = k as i32;
                        let chosen = cases
                            .iter()
                            .find(|(ck, _)| *ck == k)
                            .map(|(_, t)| *t)
                            .unwrap_or(default_target);
                        for &(_, t) in &cases {
                            if t != chosen {
                                self.const_folded_dead_targets.insert(t);
                            }
                        }
                        if default_target != chosen {
                            self.const_folded_dead_targets.insert(default_target);
                        }
                        self.add_merge_predecessor(chosen);
                        self.ctrl = NO_NODE;
                        self.const_folded_dead_skip = true;
                        pc += len;
                        continue;
                    }
                    let can_prune_switch = !self.block_walk
                        && !self.sync_splice_open()
                        && self.splice_pruned_switch_edges.contains_key(&pc)
                        && self.splice.last().is_some_and(|frame| {
                            frame.ranges.is_empty() && pc >= frame.base && pc < frame.end
                        })
                        && self.chain_state_at(pc)
                        && {
                            let needs_chain = !self.spliced_bodies_pure
                                || !self.splice_committed_bases.is_empty()
                                || self.splice_call_committed;
                            !needs_chain || self.relax_fence_by_chain(pc)
                        };
                    if can_prune_switch {
                        let pruned_targets =
                            self.splice_pruned_switch_edges.get(&pc).unwrap().clone();
                        let mut unpruned_cases: Vec<(i32, usize)> = Vec::new();
                        let mut pruned_cases: Vec<i32> = Vec::new();
                        for &(match_val, target) in &cases {
                            if target == default_target {
                                continue;
                            }
                            if pruned_targets.contains(&target) {
                                pruned_cases.push(match_val);
                            } else {
                                unpruned_cases.push((match_val, target));
                            }
                        }
                        if !pruned_cases.is_empty() && unpruned_cases.len() <= 4 {
                            for (match_val, target) in unpruned_cases {
                                let cval = self.iconst(match_val as i64);
                                let cmp = self.add_data(
                                    Op::Cmp(CmpOp::Eq),
                                    IrType::Int,
                                    vec![key, cval],
                                    pc,
                                );
                                let if_node = self.graph.add(
                                    Op::If,
                                    IrType::Control,
                                    vec![self.ctrl, cmp],
                                    Some(pc),
                                );
                                let true_ctrl = self.graph.add(
                                    Op::Proj(0),
                                    IrType::Control,
                                    vec![if_node],
                                    Some(pc),
                                );
                                let false_ctrl = self.graph.add(
                                    Op::Proj(1),
                                    IrType::Control,
                                    vec![if_node],
                                    Some(pc),
                                );
                                self.ctrl = true_ctrl;
                                self.add_merge_predecessor(target);
                                self.ctrl = false_ctrl;
                            }
                            for match_val in pruned_cases {
                                let cval = self.iconst(match_val as i64);
                                let ne = self.add_data(
                                    Op::Cmp(CmpOp::Ne),
                                    IrType::Int,
                                    vec![key, cval],
                                    pc,
                                );
                                let guard = self.graph.add(
                                    Op::Guard { bci: pc },
                                    IrType::Void,
                                    vec![self.ctrl, ne],
                                    Some(pc),
                                );
                                self.graph
                                    .chain_predicate_guards
                                    .push((guard, crate::deopt::PredicateKind::UnstableIf as u8));
                            }
                            self.never_taken_pruned += 1;
                            note_branch_pruned();
                            self.add_merge_predecessor(default_target);
                            self.ctrl = NO_NODE;
                            pc += len;
                            continue;
                        }
                    }
                    if let Some((low, high)) = dense_switch_bounds(&cases, default_target) {
                        // ── The multi-way terminator ───────────────────────
                        //
                        // One `Op::Switch` and one `Op::Proj` per span slot,
                        // plus the default's. `dense_switch_bounds` has already
                        // established that the keys are strictly ascending and
                        // that [`switch_is_representable`] admits the span, so
                        // the projection count below is bounded by
                        // `SWITCH_MAX_PROJECTIONS` and the `as u8` casts cannot
                        // truncate.
                        //
                        // `switch_is_representable`, NOT `switch_is_dense`,
                        // which this comment used to name. The two are not
                        // interchangeable: `switch_is_dense` is the LOWERER's
                        // question (jump table, or binary search) and is
                        // strictly stronger, so naming it here overstated what
                        // the builder had actually checked. The weaker
                        // predicate is the correct gate, and it is the one
                        // that bounds the span at `SWITCH_MAX_SPAN` — which is
                        // the clause the casts below rest on.
                        let span = switch_projection_count(low, high) - 1;
                        // Key → target for the slots the payload actually
                        // listed. Built once; the loop below is a lookup per
                        // slot rather than a scan per slot, so a 255-slot
                        // switch stays linear.
                        let listed: HashMap<i32, usize> = cases.iter().copied().collect();
                        let switch_node = self.graph.add(
                            Op::Switch { low, high },
                            IrType::Control,
                            vec![self.ctrl, key],
                            Some(pc),
                        );
                        for k in 0..span {
                            // `low + k` cannot overflow: `k < span` and
                            // `low + span - 1 == high`, both checked in `i64`
                            // by `switch_is_dense`.
                            let match_val = low.wrapping_add(k as i32);
                            let target = listed.get(&match_val).copied().unwrap_or(default_target);
                            let edge = self.graph.add(
                                Op::Proj(k as u16),
                                IrType::Control,
                                vec![switch_node],
                                Some(pc),
                            );
                            // The abstract state is invariant across the edges
                            // (the switch consumed only `key`), so every
                            // predecessor snapshot taken here is correct — the
                            // same argument the chain below rests on.
                            self.ctrl = edge;
                            self.add_merge_predecessor(target);
                        }
                        // The default edge is the LAST projection, and it is
                        // mandatory: `Proj(span)` is the index
                        // `ir_verify::projection_arity` reserves for it and the
                        // one `ir_lower` emits the out-of-range branch to.
                        let default_edge = self.graph.add(
                            Op::Proj(span as u16),
                            IrType::Control,
                            vec![switch_node],
                            Some(pc),
                        );
                        self.ctrl = default_edge;
                        self.add_merge_predecessor(default_target);
                    } else if let Some(live) = switch_search_tree_cases(&cases, default_target) {
                        // Sparse but sorted and large: a balanced tree of
                        // `key < pivot` tests over the live keys instead of a
                        // linear chain over every listed one. See
                        // `switch_search_tree_cases`.
                        self.build_switch_search_tree(key, &live, default_target, pc);
                    } else {
                        for (match_val, target) in &cases {
                            let cval = self.iconst(*match_val as i64);
                            let cmp =
                                self.add_data(Op::Cmp(CmpOp::Eq), IrType::Int, vec![key, cval], pc);
                            let if_node = self.graph.add(
                                Op::If,
                                IrType::Control,
                                vec![self.ctrl, cmp],
                                Some(pc),
                            );
                            let true_ctrl = self.graph.add(
                                Op::Proj(0),
                                IrType::Control,
                                vec![if_node],
                                Some(pc),
                            );
                            let false_ctrl = self.graph.add(
                                Op::Proj(1),
                                IrType::Control,
                                vec![if_node],
                                Some(pc),
                            );
                            // Matched edge → the case target. The abstract state is
                            // invariant across the chain (the switch consumed only
                            // `key`), so each predecessor snapshot is correct.
                            self.ctrl = true_ctrl;
                            self.add_merge_predecessor(*target);
                            // Unmatched edge → next comparison.
                            self.ctrl = false_ctrl;
                        }
                        // All comparisons failed → default target.
                        self.add_merge_predecessor(default_target);
                    }
                    self.ctrl = NO_NODE;
                    pc += len;
                }

                // Unsupported opcode — bail out
                _ => return ir_build_bail_opcode(op, pc),
            }
        }

        // The walk is over: from here a refusal names its own pc or nothing
        // (round 12 wave 6, W5-2; see `set_splice_refusal_sink`).
        self.note_splice_refusal_pc(None, code_len);
        // A splice still open here means the walk left a relocated body without
        // passing its return — only reachable if the loop condition and the
        // splice bookkeeping disagree. The graph would be missing the caller
        // frame it saved, so refuse rather than emit it.
        if !self.splice.is_empty() {
            return ir_build_bail(line!(), code_len);
        }
        // See `splice_guard_seen`.
        if self.splice_guard_seen {
            self.note_splice_refusal_pc(self.splice_guard_pc, code_len);
            return ir_build_bail(line!(), self.splice_guard_pc.unwrap_or(code_len));
        }
        // A join the walk activated at the very end of the method (no
        // instruction after it to trip the per-instruction check).
        if self.unstructured_monitors {
            return ir_build_bail(line!(), code_len);
        }
        // OSR round (2026-10-06): arm tests and hoisted local-throw event guards.
        self.finish_osr_arm_checks();
        self.finish_local_throw_event_guards();
        // JIT round 21 wave 19 (lane lambda3fix, LAMBDA321-4): the loop-phi
        // receivers `lambda_impl_opcode` admitted before their back edges were
        // patched, asked again with every input in place.
        for (pc, receiver, proxy) in std::mem::take(&mut self.lambda_impl_phi_checks) {
            if !self.lambda_receiver_resolves_to(receiver, proxy) {
                if ir_bail_reporting() || crate::x64::dbg_jitc_enabled() {
                    eprintln!(
                        "[ir] lambda-impl-loop-slot refused at bytecode pc {pc} in {} -- a back \
                         edge feeds the receiver's loop phi another value \
                         (CRATONVM_JIT_IR_LAMBDA_IMPL_LOOP_SLOT)",
                        self.method_label.as_deref().unwrap_or("<unknown>"),
                    );
                }
                self.refuse(line!(), pc);
            }
        }
        // Same reason, for the refusals a bookkeeping helper latches: the last
        // `patch_loop_backedge` of a method is very often performed by the very
        // last instruction (the back edge of the outermost loop), so the
        // per-instruction check above never runs again. The site and pc are the
        // helper's own, not this line's, so the census names the real refusal.
        if let Some((site, at)) = self.build_refused {
            self.note_splice_refusal_pc(Some(at), code_len);
            return ir_build_bail(site, at);
        }
        // Every merge the pre-scan registered was either activated by the walk
        // or never reached; see `audit_unactivated_merges`.
        if let Some(lost_at) = self.audit_unactivated_merges() {
            self.note_splice_refusal_pc(Some(lost_at), code_len);
            return ir_build_bail(line!(), lost_at);
        }
        self.retire_untypeable_phis();
        self.emit_build_censuses();

        Some(self.graph)
    }
}
